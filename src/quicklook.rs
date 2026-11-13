//! Quick Look (SPEC §5): scrim + panel over the whole window, body by file type, side
//! prev/next, footer, full-screen toggle. Content comes from `app.preview.data`.
//!
//! OWNER: quicklook agent.
//!
//! Focus never moves here: the list keeps it, the shell routes the arrow keys to the cursor
//! and `on_cursor_changed` / the preview loader re-render us with the new body.
//!
//! Animations are time-based (state holds `Instant`s, `render` asks for another frame while
//! something is moving) instead of `with_animation`, because enter / exit / full screen
//! overlap and all of them act on the same insets. GPUI divs have no transform, so the
//! `.96 → 1` enter scale is emulated by growing every inset by 2% of the panel size at t=0.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Div, FontWeight, HighlightStyle, Hsla, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, ScrollHandle, ScrollStrategy, SharedString, StyledText, Task,
    UniformListScrollHandle, Window, canvas, div, fill, img, point, prelude::*, px, relative, size,
    uniform_list,
};

use crate::{
    actions::{ToggleFullscreen, ToggleQuickLook},
    app::{FmApp, LoadState},
    fs_model::{self, Entry, FileType},
    icons::{Icon, icon},
    ops,
    preview_content::{ArchiveEntry, Line, MdBlock, PreviewBody, PreviewData, Syn},
    theme::{self, Theme, theme},
    ui::{self, ButtonKind},
};

// ── geometry & motion (SPEC §5, §9) ──────────────────────────────────────────
const INSET_TOP: f32 = 72.;
const INSET_SIDE: f32 = 230.;
const INSET_BOTTOM: f32 = 80.;
const INSET_FULL: f32 = 16.;
const MIN_PANEL_W: f32 = 480.;
const MIN_PANEL_H: f32 = 320.;
const HEADER_H: f32 = 52.;
const FOOTER_H: f32 = 38.;
const NAV_SIZE: f32 = 44.;
/// Gap between a side button and the panel: 230 − 168 − 44.
const NAV_GAP: f32 = 18.;
const NAV_EDGE_FULL: f32 = 28.;

const ENTER_MS: u64 = theme::DUR_FLYOUT;
const EXIT_MS: u64 = 100;
const SCRIM_MS: u64 = theme::DUR_FAST;
const FULL_MS: u64 = theme::DUR_BASE;
/// A body that has been stale for longer than this is dimmed (shorter loads never flicker).
const STALE_DIM_MS: u64 = 120;

const CODE_LINE_H: f32 = 23.;
/// Longest prefix of one source line we shape (minified files have megabyte lines).
const MAX_LINE_BYTES: usize = 4096;
const FOLDER_COLS: usize = 6;
const TILE_H: f32 = 138.;
const TILE_GAP: f32 = 10.;
const ARCHIVE_ROW_H: f32 = 26.;
const CHECKER: f32 = 20.;

pub struct QuickLookState {
    pub open: bool,
    pub fullscreen: bool,
    opened_at: Instant,
    /// Set while the 100 ms exit fade runs (`open` is already false).
    closing_at: Option<Instant>,
    close_task: Option<Task<()>>,
    /// Full-screen amount (0 = normal insets, 1 = 16 px) at `full_at`; it animates towards `fullscreen`.
    full_from: f32,
    full_at: Instant,
    /// What the body currently shows. Kept while the preview of a newer cursor entry loads.
    shown: Option<Arc<PreviewData>>,
    stale_since: Option<Instant>,
    scroll: ScrollHandle,
    list_scroll: UniformListScrollHandle,
}

impl QuickLookState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        let now = Instant::now();
        QuickLookState {
            open: false,
            fullscreen: false,
            opened_at: now,
            closing_at: None,
            close_task: None,
            full_from: 0.,
            full_at: now,
            shown: None,
            stale_since: None,
            scroll: ScrollHandle::new(),
            list_scroll: UniformListScrollHandle::new(),
        }
    }

    /// The shell adds the `QuickLook` key context and routes ←/→ to the cursor while this is true.
    /// False as soon as the exit fade starts.
    pub fn is_open(&self) -> bool {
        self.open
    }

    fn full_amount(&self, now: Instant) -> f32 {
        let target = if self.fullscreen { 1. } else { 0. };
        self.full_from + (target - self.full_from) * theme::ease_standard(progress(self.full_at, now, FULL_MS))
    }

    fn reset_scroll(&self) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.list_scroll.0.borrow().base_handle.set_offset(point(px(0.), px(0.)));
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
}

fn progress(since: Instant, now: Instant, ms: u64) -> f32 {
    (now.saturating_duration_since(since).as_secs_f32() * 1000. / ms as f32).clamp(0., 1.)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ── actions ──────────────────────────────────────────────────────────────────

/// `space` / eye button.
pub fn act_toggle(app: &mut FmApp, _: &ToggleQuickLook, _window: &mut Window, cx: &mut Context<FmApp>) {
    if app.quicklook.open {
        close(app, cx);
    } else if app.cursor_entry().is_some() {
        let now = Instant::now();
        let ql = &mut app.quicklook;
        ql.open = true;
        ql.fullscreen = false;
        ql.opened_at = now;
        ql.closing_at = None;
        ql.close_task = None;
        ql.full_from = 0.;
        ql.full_at = now;
        ql.shown = None;
        ql.stale_since = None;
        ql.reset_scroll();
        cx.notify();
    }
}

/// `f` while open.
pub fn act_fullscreen(app: &mut FmApp, _: &ToggleFullscreen, _window: &mut Window, cx: &mut Context<FmApp>) {
    toggle_fullscreen(app, cx);
}

fn toggle_fullscreen(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let ql = &mut app.quicklook;
    if !ql.open {
        return;
    }
    let now = Instant::now();
    ql.full_from = ql.full_amount(now);
    ql.full_at = now;
    ql.fullscreen = !ql.fullscreen;
    cx.notify();
}

/// Start the exit fade; the layer is removed when it ends.
fn close(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let ql = &mut app.quicklook;
    if !ql.open {
        return;
    }
    ql.open = false;
    ql.closing_at = Some(Instant::now());
    ql.close_task = Some(cx.spawn(async move |this, cx| {
        cx.background_executor().timer(Duration::from_millis(EXIT_MS)).await;
        this.update(cx, |this, cx| {
            let ql = &mut this.quicklook;
            ql.closing_at = None;
            ql.fullscreen = false;
            ql.full_from = 0.;
            ql.shown = None;
            ql.stale_since = None;
            cx.notify();
        })
        .ok();
    }));
    cx.notify();
}

/// `↵` while open: run the primary action and close. Return false when Quick Look is closed
/// (the shell then does the normal open).
pub fn open_primary(app: &mut FmApp, cx: &mut Context<FmApp>) -> bool {
    if !app.quicklook.is_open() {
        return false;
    }
    let entry = app.cursor_entry().cloned();
    let kind = entry.as_ref().map(|e| primary_kind(e, app.preview.data.as_deref()));
    close(app, cx);
    let (Some(entry), Some(kind)) = (entry, kind) else { return true };
    match kind {
        Primary::Folder => app.navigate(entry.path, cx),
        Primary::Editor => spawn_or_open(editor_commands(&entry.path), entry.path, cx),
        Primary::Image => {
            let mut imv = Command::new("imv");
            imv.arg(&entry.path);
            spawn_or_open(vec![imv], entry.path, cx)
        }
        Primary::Archive => extract_archive(app, entry.path, cx),
        Primary::Other => ops::open_external(&entry.path, cx),
    }
    true
}

/// `esc` cascade. Return true if Quick Look was open and is now closing.
pub fn cancel(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> bool {
    if app.quicklook.open {
        close(app, cx);
        true
    } else {
        false
    }
}

/// The cursor moved: the panel keeps its size, only the body swaps (no re-enter animation).
pub fn on_cursor_changed(app: &mut FmApp, cx: &mut Context<FmApp>) {
    if app.quicklook.open {
        cx.notify();
    }
}

// ── primary action ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Primary {
    Folder,
    Editor,
    Image,
    Archive,
    Other,
}

impl Primary {
    fn label(self) -> &'static str {
        match self {
            Primary::Folder | Primary::Other => "Open",
            Primary::Editor => "Open in nvim",
            Primary::Image => "Open in imv",
            Primary::Archive => "Extract here",
        }
    }
}

/// What the primary button does for `entry`; the loaded preview (if it is this entry's)
/// refines the guess made from the file name.
fn primary_kind(entry: &Entry, data: Option<&PreviewData>) -> Primary {
    if entry.is_dir {
        return Primary::Folder;
    }
    let by_type = match entry.file_type {
        FileType::Folder => Primary::Folder,
        FileType::Text | FileType::Code | FileType::Markdown => Primary::Editor,
        FileType::Image => Primary::Image,
        FileType::Archive => Primary::Archive,
        _ => Primary::Other,
    };
    match data.filter(|d| d.entry.path == entry.path).map(|d| &d.body) {
        Some(PreviewBody::Text { .. } | PreviewBody::Markdown { .. }) => Primary::Editor,
        Some(PreviewBody::Image { .. }) => Primary::Image,
        Some(PreviewBody::Archive { .. }) => Primary::Archive,
        Some(PreviewBody::Other { .. }) => Primary::Other,
        _ => by_type,
    }
}

/// `$TERMINAL -e nvim <path>`, then the usual suspects.
fn editor_commands(path: &Path) -> Vec<Command> {
    let mut terminals: Vec<Vec<String>> = Vec::new();
    if let Ok(term) = std::env::var("TERMINAL") {
        let words: Vec<String> = term.split_whitespace().map(str::to_owned).collect();
        if !words.is_empty() {
            terminals.push(words);
        }
    }
    terminals.extend(["foot", "kitty", "alacritty", "wezterm"].map(|t| vec![t.to_owned()]));
    terminals
        .into_iter()
        .map(|words| {
            let mut cmd = Command::new(&words[0]);
            cmd.args(&words[1..]);
            if words[0].rsplit('/').next() == Some("wezterm") {
                cmd.args(["start", "--"]);
            } else {
                cmd.arg("-e");
            }
            cmd.arg("nvim").arg(path);
            cmd
        })
        .collect()
}

fn in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Start the first command that can be spawned (detached, null stdio) off the main thread;
/// if none can, open the file with the default application.
fn spawn_or_open(commands: Vec<Command>, path: PathBuf, cx: &mut Context<FmApp>) {
    let task = cx.background_spawn(async move {
        // Every editor command runs nvim inside a terminal: without nvim that terminal
        // would just flash, so go straight to the fallback.
        let needs_nvim = commands.iter().any(|c| c.get_args().any(|a| a == "nvim"));
        if needs_nvim && !in_path("nvim") {
            return false;
        }
        for mut cmd in commands {
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            if let Ok(mut child) = cmd.spawn() {
                // Reap it when it exits so it does not linger as a zombie.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return true;
            }
        }
        false
    });
    cx.spawn(async move |this, cx| {
        if !task.await {
            this.update(cx, |_, cx| ops::open_external(&path, cx)).ok();
        }
    })
    .detach();
}

fn extract_archive(_app: &mut FmApp, path: PathBuf, cx: &mut Context<FmApp>) {
    // TODO(integration): call preview::extract_here(_app, path, cx)
    ops::open_external(&path, cx);
}

// ── render ───────────────────────────────────────────────────────────────────

/// `None` when closed; otherwise an absolutely-positioned layer covering the window.
pub fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    if !app.quicklook.open && app.quicklook.closing_at.is_none() {
        return None;
    }
    let t = theme(cx).clone();
    let now = Instant::now();
    let open = app.quicklook.open;

    // ── which data the body shows ──
    let cursor = app.cursor_entry().cloned();
    let fresh = match (&app.preview.data, &cursor) {
        (Some(data), Some(entry)) if data.entry.path == entry.path => Some(data.clone()),
        _ => None,
    };
    let empty_dir = cursor.is_none() && app.tab().load == LoadState::Ready;
    let mut animating = false;
    let mut dim = false;
    {
        let ql = &mut app.quicklook;
        if let Some(data) = fresh {
            if ql.shown.as_ref().is_none_or(|s| s.entry.path != data.entry.path) {
                ql.reset_scroll();
            }
            ql.shown = Some(data);
            ql.stale_since = None;
        } else if open && !empty_dir && ql.shown.is_some() {
            let since = *ql.stale_since.get_or_insert(now);
            dim = now.saturating_duration_since(since) >= Duration::from_millis(STALE_DIM_MS);
            animating |= !dim;
        }
    }
    let ql = &app.quicklook;
    let shown = if empty_dir && open { None } else { ql.shown.clone() };

    // ── animation state ──
    let enter_p = progress(ql.opened_at, now, ENTER_MS);
    let scrim_p = progress(ql.opened_at, now, SCRIM_MS);
    let exit_p = ql.closing_at.map(|at| progress(at, now, EXIT_MS));
    let full_p = progress(ql.full_at, now, FULL_MS);
    let enter = theme::ease_enter(enter_p);
    let fade = 1. - exit_p.map(theme::ease_exit).unwrap_or(0.);
    let full = ql.full_amount(now);
    animating |= enter_p < 1. || scrim_p < 1. || full_p < 1. || exit_p.is_some_and(|p| p < 1.);
    if animating {
        window.request_animation_frame();
    }

    // ── geometry ──
    let viewport = window.viewport_size();
    let (w, h) = (f32::from(viewport.width), f32::from(viewport.height));
    let side = INSET_SIDE.min(((w - MIN_PANEL_W) / 2.).max(INSET_FULL));
    let (top, bottom) = if h - INSET_TOP - INSET_BOTTOM >= MIN_PANEL_H {
        (INSET_TOP, INSET_BOTTOM)
    } else {
        let v = ((h - MIN_PANEL_H) / 2.).clamp(INSET_FULL, INSET_TOP);
        (v, v)
    };
    let nav_edge = lerp((side - NAV_GAP - NAV_SIZE).max(8.), NAV_EDGE_FULL, full);
    let (mut left, mut right) = (lerp(side, INSET_FULL, full), lerp(side, INSET_FULL, full));
    let (mut top, mut bottom) = (lerp(top, INSET_FULL, full), lerp(bottom, INSET_FULL, full));
    let (panel_w, panel_h) = ((w - left - right).max(0.), (h - top - bottom).max(0.));
    // Enter "scale .96 → 1 from the centre": shrink the panel by 2% of its size on every side.
    let shrink = (1. - lerp(0.96, 1., enter)) / 2.;
    left += panel_w * shrink;
    right += panel_w * shrink;
    top += panel_h * shrink;
    bottom += panel_h * shrink;
    let body_size = size(px((panel_w - 2.).max(0.)), px((panel_h - 2. - HEADER_H - FOOTER_H).max(0.)));
    let opacity = (enter * fade).clamp(0., 1.);

    // ── header ──
    let name: SharedString = match (&cursor, &shown) {
        (Some(entry), _) => entry.name.clone(),
        (None, Some(data)) if !open => data.entry.name.clone(),
        _ => app.tab().title().into(),
    };
    let meta: SharedString = shown.as_ref().map(|d| d.meta.clone()).unwrap_or_default();
    let kind = cursor.as_ref().map(|e| primary_kind(e, app.preview.data.as_deref())).unwrap_or(Primary::Other);
    // The kit's hover fill is surface-2, which is the panel's own background: use surface here.
    let bordered = |id: &'static str, ic: Icon| {
        let hover_bg = t.surface;
        ui::icon_button_sized(id, ic, 30., 14., cx).border_1().border_color(t.line).hover(move |s| s.bg(hover_bg))
    };
    let header = div()
        .h(px(HEADER_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(12.))
        .pl(px(18.))
        .pr(px(12.))
        .border_b_1()
        .border_color(t.line)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .font_family(t.font_mono.clone())
                        .text_size(px(13.5))
                        .line_height(px(18.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.fg)
                        .truncate()
                        .child(name),
                )
                .child(ui::label(meta, cx).text_size(px(10.)).line_height(px(14.)).truncate()),
        )
        .child(
            ui::button("ql-primary", kind.label(), ButtonKind::Primary, cx).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    open_primary(this, cx);
                },
            )),
        )
        .child(
            bordered("ql-copy-path", Icon::Clipboard)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_path_to_clipboard(cx))),
        )
        .child(
            bordered("ql-fullscreen", Icon::Fullscreen)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| toggle_fullscreen(this, cx))),
        )
        .child(bordered("ql-close", Icon::Close).on_click(cx.listener(|this, _: &ClickEvent, _, cx| close(this, cx))));

    // ── body ──
    let content = match &shown {
        Some(data) => body(data, ql, body_size, &t),
        None if empty_dir => centered_note("Empty folder", t.fg_muted, &t),
        None => centered_note("loading", t.fg_muted, &t),
    };
    let body = div()
        .flex_1()
        .min_h_0()
        .relative()
        .overflow_hidden()
        .when(dim, |d| d.opacity(0.5))
        .child(content);

    // ── footer ──
    let tab = app.tab();
    let total = tab.visible.len();
    let row = if total == 0 { 0 } else { tab.cursor.min(total - 1) + 1 };
    let hint = |keys: &[&'static str], text: &'static str| {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .children(keys.iter().map(|k| ui::keycap(*k, cx)))
            .child(div().font_family(t.font_sans.clone()).text_size(px(11.5)).text_color(t.fg_muted).child(text))
    };
    let radius_inner = (t.radius_panel - px(1.)).max(px(0.));
    let footer = div()
        .h(px(FOOTER_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(18.))
        .px(px(14.))
        .overflow_hidden()
        .bg(t.surface)
        .border_t_1()
        .border_color(t.line)
        .rounded_b(radius_inner)
        .child(
            div()
                .flex_none()
                .font_family(t.font_mono.clone())
                .text_size(px(12.))
                .text_color(t.accent)
                .child(format!("{row} / {total}")),
        )
        .child(hint(&["↑", "↓"], "previous · next"))
        .child(hint(&["space"], "close"))
        .child(hint(&["f"], "full screen"))
        .child(hint(&["↵"], "open"))
        .child(div().flex_1())
        .child(ui::label("quick look", cx));

    // ── panel ──
    let panel = div()
        .id("ql-panel")
        .absolute()
        .top(px(top))
        .left(px(left))
        .right(px(right))
        .bottom(px(bottom))
        .occlude()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(t.surface_2)
        .border_1()
        .border_color(t.accent)
        .rounded(t.radius_panel)
        .shadow(t.shadow_quicklook())
        .opacity(opacity)
        .child(header)
        .child(body)
        .child(footer);

    // ── side navigation ──
    let nav = |id: &'static str, ic: Icon, delta: isize| {
        let (line, accent) = (t.line, t.accent);
        div()
            .id(id)
            .absolute()
            .top(px(((h - NAV_SIZE) / 2.).max(0.)))
            .size(px(NAV_SIZE))
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.surface)
            .border_1()
            .border_color(line)
            .rounded(t.radius_control)
            .cursor_pointer()
            .opacity(opacity)
            .hover(move |s| s.border_color(accent))
            .child(icon(ic, 18.).text_color(t.fg))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if this.quicklook.open {
                    this.move_cursor(delta, cx)
                }
            }))
    };
    let prev = (row > 1).then(|| nav("ql-prev", Icon::ChevronLeft, -1).left(px(nav_edge)));
    let next = (row < total).then(|| nav("ql-next", Icon::ChevronRight, 1).right(px(nav_edge)));

    let scrim = t.scrim();
    Some(
        div()
            .id("ql-layer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(ui::alpha(scrim, scrim.a * scrim_p * fade))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, _, cx| close(this, cx)))
            .child(panel)
            .children(prev)
            .children(next)
            .into_any_element(),
    )
}

// ── bodies ───────────────────────────────────────────────────────────────────

fn body(data: &Arc<PreviewData>, ql: &QuickLookState, body_size: gpui::Size<Pixels>, t: &Theme) -> AnyElement {
    match &data.body {
        PreviewBody::Folder { items, .. } if items.is_empty() => centered_note("Empty folder", t.fg_muted, t),
        PreviewBody::Folder { items, .. } => folder_body(data, items.len(), ql, t),
        PreviewBody::Text { lines, truncated, .. } => text_body(data, lines.len(), *truncated, ql, t),
        PreviewBody::Markdown { blocks, .. } => markdown_body(blocks, ql, t),
        PreviewBody::Image { path, width, height, format, .. } => {
            image_body(path, *width, *height, format, body_size, t)
        }
        PreviewBody::Archive { summary, packed, unpacked, files, entries, .. } => {
            archive_body(data, summary, *packed, *unpacked, *files, entries.len(), ql, t)
        }
        PreviewBody::Other { meta, .. } => other_body(data, meta, ql, t),
        PreviewBody::Error(message) => centered_note(message.clone(), t.err, t),
    }
}

fn centered_note(text: impl Into<SharedString>, color: Hsla, t: &Theme) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.))
        .font_family(t.font_sans.clone())
        .text_size(px(13.))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

fn mono(t: &Theme, size_px: f32, color: Hsla) -> Div {
    div().font_family(t.font_mono.clone()).text_size(px(size_px)).text_color(color)
}

// folder ─────────────────────────────────────────────────────────────────────

fn folder_body(data: &Arc<PreviewData>, count: usize, ql: &QuickLookState, t: &Theme) -> AnyElement {
    let rows = count.div_ceil(FOLDER_COLS);
    let data = data.clone();
    let t = t.clone();
    uniform_list("ql-folder", rows, move |range, _window, _cx| {
        let PreviewBody::Folder { items, .. } = &data.body else { return Vec::new() };
        range
            .map(|row| {
                let start = row * FOLDER_COLS;
                div().h(px(TILE_H + TILE_GAP)).pb(px(TILE_GAP)).flex().gap(px(TILE_GAP)).children(
                    (start..start + FOLDER_COLS).map(|ix| match items.get(ix) {
                        Some(entry) => folder_tile(entry, &t),
                        None => div().flex_1().min_w_0(),
                    }),
                )
            })
            .collect()
    })
    .track_scroll(ql.list_scroll.clone())
    .size_full()
    .pt(px(18.))
    .px(px(18.))
    .pb(px(18. - TILE_GAP))
    .into_any_element()
}

/// Grid tile (SPEC §4.4): padding 8, gap 8, 1px line; 74 thumbnail well, name, size.
fn folder_tile(entry: &Entry, t: &Theme) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(8.))
        .p(px(8.))
        .border_1()
        .border_color(t.line)
        .child(
            div()
                .h(px(74.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.bg)
                .child(icon(entry.file_type.icon(), 26.).text_color(t.fg_muted)),
        )
        .child(mono(t, 11., t.fg).line_height(px(16.)).truncate().child(entry.display_name()))
        .child(mono(t, 10., t.fg_muted).line_height(px(14.)).truncate().child(entry.size_label()))
}

// code / text ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct SynColors {
    keyword: Hsla,
    string: Hsla,
    number: Hsla,
    comment: Hsla,
}

impl SynColors {
    fn new(t: &Theme) -> Self {
        SynColors { keyword: t.syn_keyword, string: t.syn_string, number: t.syn_number, comment: t.syn_comment }
    }

    /// `None` = inherit `fg`.
    fn get(self, syn: Syn) -> Option<Hsla> {
        match syn {
            Syn::Plain => None,
            Syn::Keyword => Some(self.keyword),
            Syn::String => Some(self.string),
            Syn::Number => Some(self.number),
            Syn::Comment => Some(self.comment),
        }
    }
}

/// One source line as a single text element with highlight ranges.
fn styled_line(line: &Line, colors: SynColors) -> StyledText {
    if let [span] = line.as_slice()
        && span.text.len() <= MAX_LINE_BYTES
    {
        let len = span.text.len();
        let text = StyledText::new(span.text.clone());
        return match colors.get(span.syn) {
            Some(color) if len > 0 => {
                text.with_highlights([(0..len, HighlightStyle { color: Some(color), ..Default::default() })])
            }
            _ => text,
        };
    }
    let mut text = String::new();
    let mut highlights = Vec::new();
    for span in line {
        let room = MAX_LINE_BYTES.saturating_sub(text.len());
        if room == 0 {
            break;
        }
        let mut end = span.text.len().min(room);
        while !span.text.is_char_boundary(end) {
            end -= 1;
        }
        let start = text.len();
        text.push_str(&span.text[..end]);
        if let Some(color) = colors.get(span.syn)
            && text.len() > start
        {
            highlights.push((start..text.len(), HighlightStyle { color: Some(color), ..Default::default() }));
        }
    }
    StyledText::new(text).with_highlights(highlights)
}

fn text_body(data: &Arc<PreviewData>, count: usize, truncated: bool, ql: &QuickLookState, t: &Theme) -> AnyElement {
    let digits = count.max(1).to_string().len();
    // Mono 13 advances ≈ 7.8 px per digit; the prototype's gutter is 26 wide for two digits.
    let gutter = (digits as f32 * 8. + 2.).max(26.);
    let colors = SynColors::new(t);
    let (font, fg, muted) = (t.font_mono.clone(), t.fg, t.fg_muted);
    let data = data.clone();
    uniform_list("ql-text", count + truncated as usize, move |range, _window, _cx| {
        let PreviewBody::Text { lines, .. } = &data.body else { return Vec::new() };
        range
            .map(|ix| {
                let row = div()
                    .h(px(CODE_LINE_H))
                    .w_full()
                    .flex()
                    .gap(px(18.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family(font.clone())
                    .text_size(px(13.))
                    .line_height(px(CODE_LINE_H))
                    .text_color(fg);
                match lines.get(ix) {
                    Some(line) => row
                        .child(
                            div()
                                .w(px(gutter))
                                .flex_none()
                                .flex()
                                .justify_end()
                                .text_color(colors.comment)
                                .child(SharedString::from((ix + 1).to_string())),
                        )
                        .child(div().flex_1().min_w_0().overflow_hidden().child(styled_line(line, colors))),
                    None => row
                        .child(div().w(px(gutter)).flex_none())
                        .child(div().text_color(muted).child("… truncated")),
                }
            })
            .collect()
    })
    .track_scroll(ql.list_scroll.clone())
    .size_full()
    .py(px(20.))
    .px(px(8.))
    .into_any_element()
}

// markdown ───────────────────────────────────────────────────────────────────

fn markdown_body(blocks: &[MdBlock], ql: &QuickLookState, t: &Theme) -> AnyElement {
    let colors = SynColors::new(t);
    let sans = |size_px: f32, weight: FontWeight| {
        div().font_family(t.font_sans.clone()).text_size(px(size_px)).font_weight(weight).text_color(t.fg)
    };
    let mut column = div().w_full().max_w(px(680.)).flex().flex_col().py(px(28.));
    let mut prev_was_item = false;
    for (ix, block) in blocks.iter().enumerate() {
        let first = ix == 0;
        let is_item = matches!(block, MdBlock::ListItem { .. });
        let gap = |normal: f32| px(if first { 0. } else { normal });
        let el = match block {
            MdBlock::Heading { level: 1, text, .. } => {
                sans(30., FontWeight::SEMIBOLD).line_height(relative(1.25)).mt(gap(24.)).child(text.clone())
            }
            MdBlock::Heading { level: 2, text, .. } => sans(17., FontWeight::SEMIBOLD)
                .line_height(relative(1.4))
                .mt(gap(26.))
                .pb(px(6.))
                .border_b_1()
                .border_color(t.line)
                .child(text.clone()),
            MdBlock::Heading { text, .. } => {
                sans(14., FontWeight::SEMIBOLD).line_height(relative(1.5)).mt(gap(20.)).child(text.clone())
            }
            MdBlock::Paragraph { text, .. } => {
                sans(14., FontWeight::NORMAL).line_height(relative(1.6)).mt(gap(14.)).child(text.clone())
            }
            MdBlock::ListItem { depth, ordered, text, .. } => {
                let marker: SharedString = match ordered {
                    Some(n) => format!("{n}.").into(),
                    None => "•".into(),
                };
                sans(14., FontWeight::NORMAL)
                    .line_height(relative(1.6))
                    .mt(gap(if prev_was_item { 4. } else { 14. }))
                    .pl(px(18. * *depth as f32))
                    .flex()
                    .items_start()
                    .child(div().min_w(px(18.)).pr(px(6.)).flex_none().text_color(t.fg_muted).child(marker))
                    .child(div().flex_1().min_w_0().child(text.clone()))
            }
            MdBlock::Code { lines, .. } => div()
                .mt(gap(14.))
                .p(px(12.))
                .flex()
                .flex_col()
                .overflow_hidden()
                .whitespace_nowrap()
                .bg(t.bg)
                .border_1()
                .border_color(t.line)
                .rounded(t.radius_control)
                .font_family(t.font_mono.clone())
                .text_size(px(12.5))
                .line_height(px(20.))
                .text_color(t.fg)
                .children(lines.iter().map(|line| div().h(px(20.)).child(styled_line(line, colors)))),
            MdBlock::Quote { text, .. } => div()
                .mt(gap(14.))
                .flex()
                .child(div().w(px(2.)).flex_none().bg(t.line))
                .child(
                    sans(14., FontWeight::NORMAL)
                        .line_height(relative(1.6))
                        .text_color(t.fg_muted)
                        .flex_1()
                        .min_w_0()
                        .pl(px(12.))
                        .child(text.clone()),
                ),
            MdBlock::Rule => div().mt(gap(20.)).mb(px(6.)).h(px(1.)).flex_none().bg(t.line),
        };
        prev_was_item = is_item;
        column = column.child(el);
    }
    div()
        .id("ql-markdown")
        .size_full()
        .overflow_y_scroll()
        .track_scroll(&ql.scroll)
        .child(div().w_full().px(px(24.)).flex().justify_center().child(column))
        .into_any_element()
}

// image ──────────────────────────────────────────────────────────────────────

fn image_body(path: &Path, width: u32, height: u32, format: &SharedString, body_size: gpui::Size<Pixels>, t: &Theme) -> AnyElement {
    const PAD: f32 = 24.;
    let (dark, light) = (t.bg, t.surface);
    let checkerboard = canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window, _| {
            window.paint_quad(fill(bounds, dark));
            // Keep the quad count bounded on huge windows by doubling the square size.
            let mut cell = CHECKER;
            let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            while (bw / cell) * (bh / cell) > 12_000. {
                cell *= 2.;
            }
            let (cols, rows) = ((bw / cell).ceil() as usize, (bh / cell).ceil() as usize);
            for row in 0..rows {
                for col in (row % 2..cols).step_by(2) {
                    let origin = bounds.origin + point(px(col as f32 * cell), px(row as f32 * cell));
                    let cell_bounds = Bounds::new(origin, size(px(cell), px(cell))).intersect(&bounds);
                    window.paint_quad(fill(cell_bounds, light));
                }
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full();

    let known = width > 0 && height > 0;
    let chip: SharedString = if known {
        let avail_w = (f32::from(body_size.width) - 2. * PAD).max(1.);
        let avail_h = (f32::from(body_size.height) - 2. * PAD).max(1.);
        let zoom = (avail_w / width as f32).min(avail_h / height as f32).min(1.);
        let zoom = (zoom * 100.).round().max(1.) as u32;
        if format.is_empty() {
            format!("{width} × {height} · {zoom}%").into()
        } else {
            format!("{width} × {height} · {format} · {zoom}%").into()
        }
    } else if format.is_empty() {
        "image".into()
    } else {
        format.clone()
    };
    let (font, muted) = (t.font_sans.clone(), t.fg_muted);

    div()
        .size_full()
        .relative()
        .child(checkerboard)
        .child(
            div().absolute().inset_0().p(px(PAD)).flex().items_center().justify_center().child(
                // ScaleDown = contain, but never above 100%.
                img(path.to_path_buf()).size_full().object_fit(ObjectFit::ScaleDown).with_fallback(move || {
                    div()
                        .font_family(font.clone())
                        .text_size(px(13.))
                        .text_color(muted)
                        .child("Cannot display this image")
                        .into_any_element()
                }),
            ),
        )
        .child(
            div().absolute().left_0().right_0().bottom(px(16.)).flex().justify_center().child(
                mono(t, 11., t.fg)
                    .py(px(4.))
                    .px(px(10.))
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.line)
                    .rounded(t.radius_control)
                    .child(chip),
            ),
        )
        .into_any_element()
}

// archive ────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn archive_body(
    data: &Arc<PreviewData>,
    summary: &SharedString,
    packed: u64,
    unpacked: u64,
    files: usize,
    count: usize,
    ql: &QuickLookState,
    t: &Theme,
) -> AnyElement {
    let ratio = if unpacked == 0 { 0. } else { (packed as f32 / unpacked as f32).clamp(0., 1.) };
    let head = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(12.))
        .pt(px(20.))
        .px(px(24.))
        .pb(px(14.))
        .child(mono(t, 12., t.accent).truncate().child(summary.clone()))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .child(mono(t, 12., t.fg).flex_none().child(format!(
                    "{} → {}",
                    fs_model::format_size(packed),
                    fs_model::format_size(unpacked)
                )))
                .child(
                    div()
                        .w(px(160.))
                        .h(px(4.))
                        .flex_none()
                        .bg(t.line)
                        .child(div().h_full().w(relative(ratio)).bg(t.accent)),
                )
                .child(mono(t, 11.5, t.fg_muted).flex_none().child(format!(
                    "{}% · {files} {}",
                    (ratio * 100.).round() as u32,
                    if files == 1 { "file" } else { "files" }
                ))),
        );

    let tree = {
        let data = data.clone();
        let t = t.clone();
        uniform_list("ql-archive", count, move |range, _window, _cx| {
            let PreviewBody::Archive { entries, .. } = &data.body else { return Vec::new() };
            range.filter_map(|ix| entries.get(ix)).map(|entry| archive_row(entry, &t)).collect()
        })
        .track_scroll(ql.list_scroll.clone())
        .flex_1()
        .min_h_0()
        .w_full()
        .pb(px(12.))
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        .child(head)
        .child(div().mx(px(24.)).h(px(1.)).flex_none().bg(t.line))
        .child(div().flex_1().min_h_0().px(px(24.)).flex().flex_col().child(tree))
        .into_any_element()
}

fn archive_row(entry: &ArchiveEntry, t: &Theme) -> Div {
    let name: SharedString = if entry.is_dir && !entry.name.ends_with('/') {
        format!("{}/", entry.name).into()
    } else {
        entry.name.clone()
    };
    let size_label = if entry.is_dir && entry.size == 0 { String::new() } else { fs_model::format_size(entry.size) };
    div()
        .h(px(ARCHIVE_ROW_H))
        .w_full()
        .flex()
        .items_center()
        .gap(px(10.))
        .pl(px(12. + 22. * entry.depth.min(40) as f32))
        .pr(px(12.))
        .child(icon(if entry.is_dir { Icon::Folder } else { Icon::File }, 14.).text_color(t.fg_muted))
        .child(mono(t, 12., t.fg).flex_1().min_w_0().truncate().child(name))
        .child(mono(t, 12., t.fg_muted).flex_none().child(size_label))
}

// other ──────────────────────────────────────────────────────────────────────

fn other_body(data: &PreviewData, meta: &[(SharedString, SharedString)], ql: &QuickLookState, t: &Theme) -> AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(14.))
        .max_w_full()
        .child(icon(data.entry.file_type.icon(), 48.).text_color(t.fg_muted))
        .child(
            mono(t, 13.5, t.fg)
                .font_weight(FontWeight::MEDIUM)
                .max_w_full()
                .truncate()
                .child(data.entry.name.clone()),
        );
    if !meta.is_empty() {
        column = column.child(div().mt(px(6.)).flex().flex_col().gap(px(6.)).children(meta.iter().map(|(key, value)| {
            div()
                .flex()
                .gap(px(16.))
                .child(mono(t, 12., t.fg_muted).w(px(140.)).flex_none().flex().justify_end().child(key.clone()))
                .child(mono(t, 12., t.fg).w(px(240.)).flex_none().truncate().child(value.clone()))
        })));
    }
    div()
        .id("ql-other")
        .size_full()
        .overflow_y_scroll()
        .track_scroll(&ql.scroll)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .p(px(24.))
        .child(column)
        .into_any_element()
}
