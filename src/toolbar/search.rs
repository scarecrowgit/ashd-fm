//! Filter field (SPEC §4.2) and its recursive-search mode (`ctrl+/`).

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Entity, Focusable, MouseButton, ScrollHandle, SharedString,
    Window, deferred, div, point, prelude::*, px,
};

use super::{keep_focus, path};
use crate::{
    app::FmApp,
    fs_model::FileType,
    icons::{Icon, icon},
    text_input::{TextInput, TextInputEvent},
    theme::{self, Theme, theme},
    ui::label,
};

pub(super) const FILTER_PLACEHOLDER: &str = "filter  /";
const SEARCH_PLACEHOLDER: &str = "search recursively";
/// Directory entries looked at before a search gives up.
const MAX_VISITED: usize = 20_000;
const MAX_RESULTS: usize = 500;
const DEBOUNCE_MS: u64 = 250;
const RESULTS_W: f32 = 420.;
const VISIBLE_ROWS: f32 = 12.;
const ROW_H: f32 = 28.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Nothing asked yet (empty query): no dropdown.
    Idle,
    Running,
    Done { truncated: bool },
}

struct Hit {
    path: PathBuf,
    name: String,
    /// Byte range of the match inside `name`.
    matched: (usize, usize),
    /// Parent folder relative to the search root, with a trailing `/`.
    parent: SharedString,
    icon: Icon,
}

pub(super) struct FilterState {
    pub(super) input: Entity<TextInput>,
    pub(super) search_mode: bool,
    /// The query `results` belong to.
    query: String,
    status: Status,
    results: Vec<Hit>,
    highlight: usize,
    generation: u64,
    /// Stops the walk of a superseded search.
    cancel: Arc<AtomicBool>,
    scroll: ScrollHandle,
    epoch: usize,
}

impl FilterState {
    pub(super) fn new(input: Entity<TextInput>) -> Self {
        FilterState {
            input,
            search_mode: false,
            query: String::new(),
            status: Status::Idle,
            results: Vec::new(),
            highlight: 0,
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            scroll: ScrollHandle::new(),
            epoch: 0,
        }
    }

    fn reset_results(&mut self) {
        self.generation += 1;
        self.cancel.store(true, Ordering::Relaxed);
        self.results.clear();
        self.query.clear();
        self.status = Status::Idle;
        self.highlight = 0;
    }
}

// ── behaviour ────────────────────────────────────────────────────────────────

/// `/`: focus the field and select what is in it.
pub(super) fn focus_filter(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let input = app.toolbar.filter.input.clone();
    input.update(cx, |input, cx| input.select_all_text(cx));
    input.focus_handle(cx).focus(window);
    cx.notify();
}

/// Make the field show the active tab's filter.
pub(super) fn sync_from_tab(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let filter = app.tab().filter.clone();
    app.toolbar.filter.input.update(cx, |input, cx| {
        if input.text() != filter {
            input.set_text(filter, cx);
        }
    });
}

/// `ctrl+/`: turn the field into the recursive-search field. Whatever was typed as a filter
/// becomes the first query.
pub(super) fn enter_search(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    if !app.toolbar.filter.search_mode {
        app.toolbar.filter.search_mode = true;
        app.toolbar.filter.reset_results();
        app.toolbar.filter.input.update(cx, |input, cx| input.set_placeholder(SEARCH_PLACEHOLDER, cx));
        if !app.tab().filter.is_empty() {
            // The text stays in the field as the query; the list itself is no longer filtered.
            app.set_filter("", cx);
        }
        start_search(app, false, cx);
    }
    focus_filter(app, window, cx);
}

/// Back to the plain live filter. Does not move focus.
pub(super) fn exit_search(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let st = &mut app.toolbar.filter;
    if !st.search_mode {
        return;
    }
    st.search_mode = false;
    st.reset_results();
    st.input.update(cx, |input, cx| input.set_placeholder(FILTER_PLACEHOLDER, cx));
    sync_from_tab(app, cx);
    cx.notify();
}

pub(super) fn on_input_event(
    app: &mut FmApp,
    input: &Entity<TextInput>,
    event: &TextInputEvent,
    window: &mut Window,
    cx: &mut Context<FmApp>,
) {
    let searching = app.toolbar.filter.search_mode;
    match event {
        TextInputEvent::Changed if searching => start_search(app, true, cx),
        TextInputEvent::Changed => {
            let text = input.read(cx).text().to_string();
            app.set_filter(text, cx);
        }
        TextInputEvent::Submit if searching => {
            let st = &app.toolbar.filter;
            let current = st.query == input.read(cx).text().trim();
            let highlight = st.highlight;
            if current && matches!(st.status, Status::Done { .. }) && !st.results.is_empty() {
                open_result(app, highlight, window, cx);
            } else if !(current && st.status == Status::Running) {
                start_search(app, false, cx);
            }
        }
        // Keep the filter, give the keys back to the list.
        TextInputEvent::Submit => app.focus_list(window),
        TextInputEvent::Cancel if searching => {
            exit_search(app, cx);
            app.focus_list(window);
        }
        TextInputEvent::Cancel => {
            input.update(cx, |input, cx| input.set_text("", cx));
            if !app.tab().filter.is_empty() {
                app.set_filter("", cx);
            }
            app.focus_list(window);
        }
        TextInputEvent::Up if searching => move_highlight(app, -1, cx),
        TextInputEvent::Down if searching => move_highlight(app, 1, cx),
        // While filtering, the arrows still walk the (filtered) list.
        TextInputEvent::Up => app.move_cursor(-1, cx),
        TextInputEvent::Down => app.move_cursor(1, cx),
        TextInputEvent::Tab => {}
        TextInputEvent::Blur => exit_search(app, cx),
    }
}

fn move_highlight(app: &mut FmApp, delta: isize, cx: &mut Context<FmApp>) {
    let st = &mut app.toolbar.filter;
    let n = st.results.len() as isize;
    if n == 0 {
        return;
    }
    st.highlight = (st.highlight as isize + delta).rem_euclid(n) as usize;
    st.scroll.scroll_to_item(st.highlight);
    cx.notify();
}

/// Go to the result's folder and put the cursor on it.
fn open_result(app: &mut FmApp, ix: usize, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some(target) = app.toolbar.filter.results.get(ix).map(|hit| hit.path.clone()) else { return };
    exit_search(app, cx);
    app.focus_list(window);
    path::reveal(app, target, cx);
}

/// Search for what is in the field, now or after the debounce. Supersedes any running search.
fn start_search(app: &mut FmApp, debounce: bool, cx: &mut Context<FmApp>) {
    let query = app.toolbar.filter.input.read(cx).text().trim().to_string();
    let (root, show_hidden) = (app.cwd().to_path_buf(), app.show_hidden);
    let st = &mut app.toolbar.filter;
    st.generation += 1;
    st.cancel.store(true, Ordering::Relaxed);
    if query.is_empty() {
        st.reset_results();
        cx.notify();
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    st.cancel = cancel.clone();
    let generation = st.generation;
    cx.spawn(async move |this, cx| {
        if debounce {
            cx.background_executor().timer(Duration::from_millis(DEBOUNCE_MS)).await;
        }
        let current = this.update(cx, |app, cx| {
            let st = &mut app.toolbar.filter;
            if st.generation != generation {
                return false;
            }
            if st.status == Status::Idle {
                st.epoch += 1;
            }
            st.status = Status::Running;
            cx.notify();
            true
        });
        if !matches!(current, Ok(true)) {
            return;
        }
        let needle = query.to_lowercase();
        let found =
            cx.background_executor().spawn(async move { walk(&root, &needle, show_hidden, &cancel) }).await;
        this.update(cx, |app, cx| {
            let st = &mut app.toolbar.filter;
            if st.generation != generation {
                return;
            }
            st.results = found.hits;
            st.status = Status::Done { truncated: found.truncated };
            st.query = query;
            st.highlight = 0;
            st.scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        })
        .ok();
    })
    .detach();
    cx.notify();
}

struct Found {
    hits: Vec<Hit>,
    truncated: bool,
}

/// Blocking breadth-first walk: shallow matches come first. Symlinks are reported but never
/// followed; dotfiles are skipped (and not descended into) unless `show_hidden`.
fn walk(root: &Path, needle: &str, show_hidden: bool, cancel: &AtomicBool) -> Found {
    let mut found = Found { hits: Vec::new(), truncated: false };
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut visited = 0usize;
    while let Some(dir) = queue.pop_front() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        let mut entries = Vec::new();
        for entry in read.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return found;
            }
            visited += 1;
            if visited > MAX_VISITED {
                found.truncated = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else { continue };
            entries.push((name, file_type.is_dir(), file_type.is_symlink()));
        }
        entries.sort();
        let parent: SharedString = match dir.strip_prefix(root) {
            Ok(rel) if rel.as_os_str().is_empty() => "./".into(),
            Ok(rel) => format!("{}/", rel.display()).into(),
            Err(_) => format!("{}/", dir.display()).into(),
        };
        for (name, is_dir, is_symlink) in entries {
            let path = dir.join(&name);
            if let Some(matched) = find_ignore_case(&name, needle) {
                let folder = is_dir || (is_symlink && path.is_dir());
                found.hits.push(Hit {
                    icon: FileType::from_name(&name, folder).icon(),
                    path: path.clone(),
                    name,
                    matched,
                    parent: parent.clone(),
                });
                if found.hits.len() >= MAX_RESULTS {
                    found.truncated = true;
                    return found;
                }
            }
            if is_dir {
                queue.push_back(path);
            }
        }
        if found.truncated {
            break;
        }
    }
    found
}

/// Byte range of the first case-insensitive occurrence of `needle` (already lower-case).
fn find_ignore_case(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        return haystack.to_ascii_lowercase().find(needle).map(|start| (start, start + needle.len()));
    }
    // Lower-casing can change byte lengths, so match char by char on the original string.
    haystack.char_indices().find_map(|(start, _)| {
        let mut rest = needle;
        for (offset, ch) in haystack[start..].char_indices() {
            for lower in ch.to_lowercase() {
                rest = rest.strip_prefix(lower)?;
            }
            if rest.is_empty() {
                return Some((start, start + offset + ch.len_utf8()));
            }
        }
        None
    })
}

// ── rendering ────────────────────────────────────────────────────────────────

pub(super) fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let st = &app.toolbar.filter;
    let active = st.search_mode || st.input.read(cx).is_focused(window);

    let field = div()
        .id("filter-field")
        .size_full()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(10.))
        .bg(t.bg)
        .border_1()
        .border_color(if active { t.accent } else { t.line })
        .rounded(t.radius_control)
        .font_family(t.font_mono.clone())
        .text_size(px(12.))
        .text_color(t.fg)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app, _, window, cx| {
                app.toolbar.filter.input.focus_handle(cx).focus(window);
                window.prevent_default();
            }),
        )
        .child(icon(Icon::Search, 14.).text_color(if st.search_mode { t.accent } else { t.fg_muted }))
        .child(st.input.clone());

    div()
        .relative()
        .flex_none()
        .w(px(200.))
        .h(px(32.))
        .child(field)
        .when_some(results_popup(app, &t, cx), |d, popup| d.child(deferred(popup)))
        .into_any_element()
}

fn results_popup(app: &FmApp, t: &Theme, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let st = &app.toolbar.filter;
    if !st.search_mode || st.status == Status::Idle {
        return None;
    }

    let mut rows = div()
        .id("search-results")
        .max_h(px(VISIBLE_ROWS * ROW_H))
        .overflow_y_scroll()
        .track_scroll(&st.scroll);
    for (ix, hit) in st.results.iter().enumerate() {
        let (start, end) = hit.matched;
        let hover_bg = t.surface_2;
        rows = rows.child(
            div()
                .id(("search-result", ix))
                .h(px(ROW_H))
                .flex_none()
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(t.radius_control)
                .cursor_pointer()
                .when(st.highlight == ix, |d| d.bg(t.surface_2))
                .hover(move |s| s.bg(hover_bg))
                .child(icon(hit.icon, 13.).text_color(t.fg_muted))
                .child(
                    div()
                        .flex()
                        .flex_shrink()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(div().flex_none().child(hit.name[..start].to_string()))
                        .child(div().flex_none().text_color(t.accent).child(hit.name[start..end].to_string()))
                        .child(div().flex_none().child(hit.name[end..].to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(60.))
                        .truncate()
                        .text_size(px(11.))
                        .text_color(t.fg_muted)
                        .child(hit.parent.clone()),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |app, _, window, cx| {
                        window.prevent_default();
                        open_result(app, ix, window, cx);
                    }),
                ),
        );
    }

    let status = match st.status {
        Status::Running => "searching…".to_string(),
        Status::Done { .. } if st.results.is_empty() => format!("no matches for “{}”", st.query),
        Status::Done { truncated: true } => format!("first {} matches · refine the query", st.results.len()),
        Status::Done { truncated: false } if st.results.len() == 1 => "1 match".to_string(),
        _ => format!("{} matches", st.results.len()),
    };

    let panel = keep_focus(
        div()
            .absolute()
            .top(px(path::POPUP_TOP))
            .right_0()
            .w(px(RESULTS_W))
            .p(px(4.))
            .flex()
            .flex_col()
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .rounded(t.radius_control)
            .shadow(t.shadow_overlay())
            .occlude()
            .font_family(t.font_mono.clone())
            .text_size(px(12.))
            .text_color(t.fg),
    )
    .when(!st.results.is_empty(), |d| d.child(rows))
    .child(div().h(px(ROW_H)).flex_none().px(px(10.)).flex().items_center().child(label(status, cx).truncate()));

    Some(
        panel
            .with_animation(
                ("search-popup", st.epoch),
                Animation::new(Duration::from_millis(theme::DUR_FAST)).with_easing(theme::ease_standard),
                |el, delta| el.opacity(delta).top(px(path::POPUP_TOP - 4. + 4. * delta)),
            )
            .into_any_element(),
    )
}
