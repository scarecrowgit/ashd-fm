//! Path bar: crumbs ⇄ edit mode with autocomplete (SPEC §4.2).

use std::{
    cell::Cell,
    fs,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, CursorStyle, Entity, Focusable, MouseButton, Pixels,
    SharedString, Window, canvas, deferred, div, font, prelude::*, px,
};

use super::keep_focus;
use crate::{
    app::FmApp,
    fs_model::{self, natural_cmp},
    icons::{Icon, icon},
    text_input::{TextInput, TextInputEvent},
    theme::{self, Theme, theme},
    ui::keycap,
};

/// Rows in the autocomplete dropdown.
const MAX_CANDIDATES: usize = 8;
/// Entries read from one directory for autocomplete.
const MAX_LISTING: usize = 5000;
/// Popups sit this far below the top of their 32px field.
pub(super) const POPUP_TOP: f32 = 36.;

#[derive(Clone)]
struct DirItem {
    name: String,
    is_dir: bool,
}

pub(super) struct PathBarState {
    pub(super) input: Entity<TextInput>,
    pub(super) editing: bool,
    /// "No such directory: …" — shown until the text changes.
    error: Option<SharedString>,
    candidates: Vec<DirItem>,
    highlight: Option<usize>,
    /// The typed text up to and including its last `/`, and what follows it.
    typed_dir: String,
    typed_segment: String,
    /// Cached listing of the directory being completed in.
    listing: Option<(PathBuf, Arc<Vec<DirItem>>)>,
    /// Bumped on every text change; stale background results are dropped.
    generation: u64,
    /// Animation key: bumped whenever the dropdown (re)appears.
    epoch: usize,
    /// Width of the bar on the previous frame (for crumb overflow).
    width: Rc<Cell<Pixels>>,
}

impl PathBarState {
    pub(super) fn new(input: Entity<TextInput>) -> Self {
        PathBarState {
            input,
            editing: false,
            error: None,
            candidates: Vec::new(),
            highlight: None,
            typed_dir: String::new(),
            typed_segment: String::new(),
            listing: None,
            generation: 0,
            epoch: 0,
            width: Rc::new(Cell::new(px(0.))),
        }
    }

    fn popup_visible(&self) -> bool {
        self.error.is_some() || !self.candidates.is_empty()
    }

    fn candidate_text(&self, item: &DirItem) -> String {
        format!("{}{}{}", self.typed_dir, item.name, if item.is_dir { "/" } else { "" })
    }

    /// Re-filter the cached listing by the typed segment.
    fn apply_listing(&mut self) {
        let was_visible = self.popup_visible();
        let needle = self.typed_segment.to_lowercase();
        let hidden_too = self.typed_segment.starts_with('.');
        self.candidates = match &self.listing {
            Some((_, items)) => items
                .iter()
                .filter(|it| (hidden_too || !it.name.starts_with('.')) && it.name.to_lowercase().starts_with(&needle))
                .take(MAX_CANDIDATES)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        self.highlight = None;
        if !was_visible && self.popup_visible() {
            self.epoch += 1;
        }
    }
}

// ── behaviour ────────────────────────────────────────────────────────────────

pub(super) fn start_edit(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    // A tag view has no path to edit: start from home.
    let mut text = fs_model::display_path(app.real_cwd().unwrap_or(&fs_model::home_dir()));
    if !text.ends_with('/') {
        text.push('/');
    }
    let st = &mut app.toolbar.path;
    st.editing = true;
    st.error = None;
    st.candidates.clear();
    st.highlight = None;
    st.listing = None;
    st.generation += 1;
    st.input.update(cx, |input, cx| {
        input.set_text(text, cx);
        input.select_all_text(cx);
    });
    st.input.focus_handle(cx).focus(window);
    cx.notify();
}

/// Back to crumbs without navigating. Does not move focus (callers do, when they should).
pub(super) fn leave_edit(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let st = &mut app.toolbar.path;
    if !st.editing {
        return;
    }
    st.editing = false;
    st.error = None;
    st.candidates.clear();
    st.highlight = None;
    st.listing = None;
    st.generation += 1;
    cx.notify();
}

pub(super) fn on_input_event(
    app: &mut FmApp,
    _: &Entity<TextInput>,
    event: &TextInputEvent,
    window: &mut Window,
    cx: &mut Context<FmApp>,
) {
    if !app.toolbar.path.editing {
        return;
    }
    match event {
        TextInputEvent::Changed => {
            app.toolbar.path.error = None;
            refresh_candidates(app, cx);
        }
        TextInputEvent::Up => move_highlight(app, -1, cx),
        TextInputEvent::Down => move_highlight(app, 1, cx),
        TextInputEvent::Tab => complete(app, cx),
        TextInputEvent::Submit => submit(app, window, cx),
        TextInputEvent::Cancel => {
            leave_edit(app, cx);
            app.focus_list(window);
        }
        TextInputEvent::Blur => leave_edit(app, cx),
    }
}

fn move_highlight(app: &mut FmApp, delta: isize, cx: &mut Context<FmApp>) {
    let st = &mut app.toolbar.path;
    let n = st.candidates.len() as isize;
    if n == 0 || st.error.is_some() {
        return;
    }
    st.highlight = Some(match st.highlight {
        None if delta > 0 => 0,
        None => (n - 1) as usize,
        Some(i) => (i as isize + delta).rem_euclid(n) as usize,
    });
    cx.notify();
}

/// Recompute the candidate list for the current text; reads the directory off the main thread
/// unless it is the one already cached.
fn refresh_candidates(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let text = app.toolbar.path.input.read(cx).text().to_string();
    let cwd = app.cwd().to_path_buf();
    let st = &mut app.toolbar.path;
    st.generation += 1;
    let (typed_dir, typed_segment) = match text.rfind('/') {
        Some(ix) => (text[..=ix].to_string(), text[ix + 1..].to_string()),
        None => (String::new(), text.clone()),
    };
    // A bare `~`, `~user` or an empty field has nothing to complete in.
    let nothing = text.trim().is_empty() || (typed_dir.is_empty() && typed_segment.starts_with('~'));
    st.typed_dir = typed_dir;
    st.typed_segment = typed_segment;
    if nothing {
        st.candidates.clear();
        st.highlight = None;
        cx.notify();
        return;
    }
    let dir = if st.typed_dir.is_empty() { cwd } else { resolve(&st.typed_dir, &cwd) };
    if st.listing.as_ref().is_some_and(|(cached, _)| *cached == dir) {
        st.apply_listing();
        cx.notify();
        return;
    }
    // Another directory: the old candidates no longer match what is typed.
    st.candidates.clear();
    st.highlight = None;
    st.listing = None;
    let generation = st.generation;
    let task = cx.background_spawn({
        let dir = dir.clone();
        async move { list_dir(&dir) }
    });
    cx.spawn(async move |this, cx| {
        let items = task.await;
        this.update(cx, |app, cx| {
            let st = &mut app.toolbar.path;
            if st.editing && st.generation == generation {
                st.listing = Some((dir, Arc::new(items)));
                st.apply_listing();
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
    cx.notify();
}

/// `tab`: complete to the highlighted candidate, the only candidate, the common prefix of all
/// candidates, or else the first one.
fn complete(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let st = &mut app.toolbar.path;
    if st.candidates.is_empty() || st.error.is_some() {
        return;
    }
    let pick = st.highlight.or((st.candidates.len() == 1).then_some(0));
    let text = match pick {
        Some(ix) => st.candidate_text(&st.candidates[ix]),
        None => {
            let common = common_prefix(st.candidates.iter().map(|c| c.name.as_str()));
            if common.chars().count() > st.typed_segment.chars().count() {
                format!("{}{}", st.typed_dir, common)
            } else {
                st.candidate_text(&st.candidates[0])
            }
        }
    };
    st.input.update(cx, |input, cx| input.set_text(text, cx));
    refresh_candidates(app, cx);
}

/// `enter`.
fn submit(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let st = &app.toolbar.path;
    if st.error.is_none()
        && let Some(ix) = st.highlight
        && ix < st.candidates.len()
    {
        return accept(app, ix, window, cx);
    }
    let text = st.input.read(cx).text().trim().to_string();
    if text.is_empty() {
        leave_edit(app, cx);
        return app.focus_list(window);
    }
    let target = resolve(&text, app.cwd());
    let generation = st.generation;
    let task = cx.background_spawn({
        let target = target.clone();
        async move { fs::metadata(&target).ok().map(|m| m.is_dir()) }
    });
    cx.spawn_in(window, async move |this, cx| {
        let is_dir = task.await;
        this.update_in(cx, |app, window, cx| {
            let st = &mut app.toolbar.path;
            if !st.editing || st.generation != generation {
                return;
            }
            match is_dir {
                Some(true) => go_to_dir(app, target, window, cx),
                Some(false) => go_to_file(app, target, window, cx),
                None => {
                    if !st.popup_visible() {
                        st.epoch += 1;
                    }
                    st.error = Some(format!("No such directory: {text}").into());
                    st.highlight = None;
                    cx.notify();
                }
            }
        })
        .ok();
    })
    .detach();
}

/// Accept a dropdown row: folders are entered, files get the cursor in their folder.
fn accept(app: &mut FmApp, ix: usize, window: &mut Window, cx: &mut Context<FmApp>) {
    let st = &app.toolbar.path;
    let Some(item) = st.candidates.get(ix) else { return };
    let target = resolve(&format!("{}{}", st.typed_dir, item.name), app.cwd());
    if item.is_dir {
        go_to_dir(app, target, window, cx);
    } else {
        go_to_file(app, target, window, cx);
    }
}

fn go_to_dir(app: &mut FmApp, dir: PathBuf, window: &mut Window, cx: &mut Context<FmApp>) {
    leave_edit(app, cx);
    app.focus_list(window);
    app.navigate(dir, cx);
}

fn go_to_file(app: &mut FmApp, file: PathBuf, window: &mut Window, cx: &mut Context<FmApp>) {
    leave_edit(app, cx);
    app.focus_list(window);
    reveal(app, file, cx);
}

/// Show `path`'s folder with the cursor on `path`.
pub(super) fn reveal(app: &mut FmApp, path: PathBuf, cx: &mut Context<FmApp>) {
    match path.parent() {
        Some(parent) if parent != app.cwd() => {
            app.navigate(parent.to_path_buf(), cx);
            // The load started by `navigate` is still in flight; it consumes this.
            app.tab_mut().pending_cursor = Some(path);
        }
        Some(_) => app.reload_and_focus(path, false, cx),
        None => app.navigate(path, cx),
    }
}

// ── pure helpers ─────────────────────────────────────────────────────────────

/// Typed text → absolute, lexically normalised path (`~` expanded, relative to `cwd`).
fn resolve(text: &str, cwd: &Path) -> PathBuf {
    let path = fs_model::expand_path(text);
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

/// Blocking: names in `dir`, folders first. Symlinks to folders count as folders.
fn list_dir(dir: &Path) -> Vec<DirItem> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut items: Vec<DirItem> = read
        .filter_map(Result::ok)
        .take(MAX_LISTING)
        .map(|entry| {
            let is_dir = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false),
                Ok(ft) => ft.is_dir(),
                Err(_) => false,
            };
            DirItem { name: entry.file_name().to_string_lossy().into_owned(), is_dir }
        })
        .collect();
    items.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| natural_cmp(&a.name, &b.name)));
    items
}

/// Longest case-insensitive common prefix, in the first name's casing.
fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = names.next() else { return String::new() };
    let mut prefix: Vec<char> = first.chars().collect();
    for name in names {
        let same = prefix
            .iter()
            .zip(name.chars())
            .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            .count();
        prefix.truncate(same);
    }
    prefix.into_iter().collect()
}

/// Crumbs for `cwd`: label + the ancestor it navigates to. Home collapses to `~`.
fn crumbs(cwd: &Path) -> Vec<(String, PathBuf)> {
    // A tag view is one crumb, `Tags › red`, that leads nowhere else.
    if fs_model::tag_of(cwd).is_some() {
        return vec![(fs_model::display_path(cwd), cwd.to_path_buf())];
    }
    let home = fs_model::home_dir();
    let (label, mut acc, rest) = match cwd.strip_prefix(&home) {
        Ok(rest) => ("~", home.clone(), rest),
        Err(_) => ("/", PathBuf::from("/"), cwd.strip_prefix("/").unwrap_or(cwd)),
    };
    let mut out = vec![(label.to_string(), acc.clone())];
    for component in rest.components() {
        if let Component::Normal(name) = component {
            acc.push(name);
            out.push((name.to_string_lossy().into_owned(), acc.clone()));
        }
    }
    out
}

// ── rendering ────────────────────────────────────────────────────────────────

pub(super) fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let bar = if app.toolbar.path.editing { edit_bar(app, &t, cx) } else { crumb_bar(app, &t, window, cx) };

    // Remember the bar's width; when it changes, draw once more so the crumbs can re-fit.
    let width = app.toolbar.path.width.clone();
    let measure = canvas(
        move |bounds, window, _| {
            if width.get() != bounds.size.width {
                width.set(bounds.size.width);
                window.on_next_frame(|window, _| window.refresh());
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();

    div()
        .relative()
        .flex_1()
        .min_w_0()
        .h(px(32.))
        .child(bar)
        .child(measure)
        .when_some(popup(app, &t, cx), |d, popup| d.child(deferred(popup)))
        .into_any_element()
}

fn crumb_bar(app: &FmApp, t: &Theme, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let crumbs = crumbs(app.cwd());
    let last = crumbs.len() - 1;

    // Keep the tail visible: drop leading segments (shown as `…`) until the rest fits.
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&font(t.font_mono.clone()));
    let advance = text_system.em_advance(font_id, px(12.)).map(f32::from).unwrap_or(7.2);
    let bar_width = f32::from(app.toolbar.path.width.get());
    const GAP: f32 = 4.;
    let crumb_width = |label: &str| label.chars().count() as f32 * advance + 12.;
    let separator = advance + GAP;
    // border + padding, the `ctrl+l` keycap (6 glyphs at 10.5 + padding + border) and its gap.
    let available = bar_width - 22. - (6. * advance * 10.5 / 12. + 12.) - GAP - 4.;
    let tail_width = |first: usize| {
        let body: f32 = crumbs[first..]
            .iter()
            .map(|(label, _)| crumb_width(label) + GAP + if label.as_str() == "/" { 0. } else { separator })
            .sum();
        body + if first > 0 { 2. * separator } else { 0. }
    };
    let mut first = 0;
    if bar_width > 0. {
        while first < last && tail_width(first) > available {
            first += 1;
        }
    }

    let slash = || div().flex_none().child("/");
    let mut row = div().flex().flex_1().min_w_0().items_center().gap(px(GAP)).overflow_hidden();
    if first > 0 {
        row = row.child(div().flex_none().child("…")).child(slash());
    }
    for (ix, (label, target)) in crumbs.into_iter().enumerate().skip(first) {
        let is_root = label == "/";
        let (hover_bg, hover_fg) = (t.surface_2, t.fg);
        let crumb = div().id(("crumb", ix));
        // Every ancestor is a drop target; the current folder itself is not.
        let crumb =
            if ix == last { crumb } else { crate::file_view::as_drop_target(app, crumb, target.clone(), cx) };
        row = row.child(
            crumb
                .flex_none()
                .px(px(6.))
                .py(px(2.))
                .rounded(t.radius_control)
                .text_color(if ix == last { t.fg } else { t.fg_muted })
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
                .child(SharedString::from(label))
                .on_click(cx.listener(move |app, _, _, cx| {
                    cx.stop_propagation();
                    app.navigate(target.clone(), cx);
                })),
        );
        if !is_root {
            row = row.child(slash());
        }
    }

    div()
        .id("path-bar")
        .h(px(32.))
        .w_full()
        .flex()
        .items_center()
        .gap(px(GAP))
        .px(px(10.))
        .bg(t.bg)
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_control)
        .overflow_hidden()
        .font_family(t.font_mono.clone())
        .text_size(px(12.))
        .text_color(t.fg_muted)
        .cursor(CursorStyle::IBeam)
        .on_click(cx.listener(|app, _, window, cx| start_edit(app, window, cx)))
        .child(row)
        .child(keycap("ctrl+l", cx))
        .into_any_element()
}

fn edit_bar(app: &FmApp, t: &Theme, cx: &mut Context<FmApp>) -> AnyElement {
    let st = &app.toolbar.path;
    let hover_fg = t.fg;
    let esc = keycap("esc", cx)
        .id("path-esc")
        .cursor_pointer()
        .hover(move |s| s.text_color(hover_fg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|app, _, window, cx| {
                leave_edit(app, cx);
                app.focus_list(window);
            }),
        );
    keep_focus(
        div()
            .id("path-edit")
            .h(px(32.))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .bg(t.bg)
            .border_1()
            .border_color(if st.error.is_some() { t.err } else { t.accent })
            .rounded(t.radius_control)
            .font_family(t.font_mono.clone())
            .text_size(px(12.))
            .text_color(t.fg),
    )
    .child(st.input.clone())
    .child(esc)
    .into_any_element()
}

/// The invalid-path message or the autocomplete rows, directly under the bar.
fn popup(app: &FmApp, t: &Theme, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let st = &app.toolbar.path;
    if !st.editing || !st.popup_visible() {
        return None;
    }
    let mut panel = keep_focus(
        div()
            .absolute()
            .top(px(POPUP_TOP))
            .left_0()
            .right_0()
            .p(px(4.))
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .rounded(t.radius_control)
            .shadow(t.shadow_overlay())
            .occlude()
            .font_family(t.font_mono.clone())
            .text_size(px(12.))
            .text_color(t.fg),
    );

    if let Some(error) = &st.error {
        panel = panel.child(
            div()
                .h(px(28.))
                .px(px(10.))
                .flex()
                .items_center()
                .text_size(px(11.))
                .text_color(t.err)
                .child(div().min_w_0().truncate().child(error.clone())),
        );
    } else {
        let typed_chars = st.typed_segment.chars().count();
        for (ix, item) in st.candidates.iter().enumerate() {
            // The typed part (whatever its case) in accent, the completion in fg.
            let split = item.name.char_indices().nth(typed_chars).map(|(i, _)| i).unwrap_or(item.name.len());
            let typed = format!("{}{}", st.typed_dir, &item.name[..split]);
            let rest = format!("{}{}", &item.name[split..], if item.is_dir { "/" } else { "" });
            let hover_bg = t.surface_2;
            panel = panel.child(
                div()
                    .id(("path-candidate", ix))
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(t.radius_control)
                    .cursor_pointer()
                    .when(st.highlight == Some(ix), |d| d.bg(t.surface_2))
                    .hover(move |s| s.bg(hover_bg))
                    .child(icon(if item.is_dir { Icon::Folder } else { Icon::File }, 13.).text_color(t.fg_muted))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(div().flex_none().text_color(t.accent).child(typed))
                            .child(
                                div().flex_none().text_color(if item.is_dir { t.fg } else { t.fg_muted }).child(rest),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |app, _, window, cx| {
                            window.prevent_default();
                            accept(app, ix, window, cx);
                        }),
                    ),
            );
        }
    }

    Some(
        panel
            .with_animation(
                ("path-popup", st.epoch),
                Animation::new(Duration::from_millis(theme::DUR_FAST)).with_easing(theme::ease_standard),
                |el, delta| el.opacity(delta).top(px(POPUP_TOP - 4. + 4. * delta)),
            )
            .into_any_element(),
    )
}
