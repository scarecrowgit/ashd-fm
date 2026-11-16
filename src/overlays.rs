//! Everything that floats above the window except Quick Look (SPEC §6, §7): context menus,
//! jobs panel, conflict dialog, delete confirmation, properties, toasts, the `?` key-hint grid.
//!
//! OWNER: ops agent. Context menus live in overlays/menu.rs, the modal dialogs in
//! overlays/dialogs.rs.
//!
//! Keyboard: every modal layer is rendered with `modal_layer`, which gives the topmost one
//! the shared `OverlaysState::focus` handle and a key context of its own. That context also
//! carries the `TextInput` identifier on purpose: the list's single-letter bindings are scoped
//! to `Fm && !TextInput` (actions.rs), so they are inert while a menu or dialog has focus.

mod dialogs;
mod menu;

use std::{path::PathBuf, time::Duration};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, Div, ElementId, Entity, FocusHandle, FontWeight,
    KeyBinding, KeyContext, MouseButton, Pixels, Point, SharedString, Subscription, Window, actions, div,
    prelude::*, px, relative,
};

use crate::{
    actions::KeyHints,
    app::FmApp,
    fs_model::{display_path, format_size},
    icons::{Icon, icon},
    ops::{self, Job, JobState, TransferKind, UndoOp},
    text_input::TextInput,
    theme::{self, ease_enter, theme},
    ui::{ButtonKind, button, icon_button_sized, keycap, label},
};

actions!(
    fm_overlays,
    [
        MenuUp,
        MenuDown,
        MenuLeft,
        MenuRight,
        MenuActivate,
        ConflictSkip,
        ConflictKeepBoth,
        ConflictRename,
        ConflictReplace,
        DialogAccept,
    ]
);

const MENU: Option<&str> = Some("FmMenu");
const CONFLICT: Option<&str> = Some("FmConflict");
const DIALOG: Option<&str> = Some("FmDialog");

/// Keys of the overlays' own contexts (called from `actions::bind_keys`).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", MenuUp, MENU),
        KeyBinding::new("k", MenuUp, MENU),
        KeyBinding::new("down", MenuDown, MENU),
        KeyBinding::new("j", MenuDown, MENU),
        KeyBinding::new("left", MenuLeft, MENU),
        KeyBinding::new("h", MenuLeft, MENU),
        KeyBinding::new("right", MenuRight, MENU),
        KeyBinding::new("l", MenuRight, MENU),
        KeyBinding::new("enter", MenuActivate, MENU),
        KeyBinding::new("s", ConflictSkip, CONFLICT),
        KeyBinding::new("k", ConflictKeepBoth, CONFLICT),
        KeyBinding::new("r", ConflictRename, CONFLICT),
        KeyBinding::new("enter", ConflictReplace, CONFLICT),
        KeyBinding::new("enter", DialogAccept, DIALOG),
    ]);
}

/// A job must run this long before the jobs panel shows it (small pastes never flash it).
pub const JOB_PANEL_DELAY: Duration = Duration::from_millis(300);

/// Distance of the toast from the window bottom: status footer + hint bar + 12.
const TOAST_BOTTOM: f32 = theme::FOOTER_H + theme::HINTBAR_H + 12.;

/// What was right-clicked.
#[derive(Clone, Debug)]
pub enum MenuTarget {
    /// A single file or folder.
    Entry(PathBuf),
    /// Empty space in the file area.
    Empty,
    /// A row that is part of a multi-selection.
    Selection,
    /// A pinned folder in the sidebar.
    Pinned(PathBuf),
}

/// A message above the status bar.
pub struct Toast {
    pub text: SharedString,
    /// Failure: info icon in `err` instead of the check in `ok`.
    pub error: bool,
    /// **Open**: go to this folder and put the cursor on this item.
    pub open: Option<(PathBuf, Option<PathBuf>)>,
    /// **Undo**.
    pub undo: Option<UndoOp>,
}

/// Modal layers, bottom to top.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layer {
    Conflict,
    Confirm,
    Properties,
    KeyHints,
    Menu,
}

pub struct OverlaysState {
    /// Keyboard focus of the topmost modal layer.
    focus: FocusHandle,
    menu: Option<menu::Menu>,
    caps: menu::Caps,
    key_hints: bool,
    confirm: Option<Vec<PathBuf>>,
    properties: Option<dialogs::Properties>,
    conflict: dialogs::ConflictUi,
    rename_input: Entity<TextInput>,
    jobs_minimised: bool,
    toast: Option<(u64, Toast)>,
    /// Bumped whenever something opens: animation ids and timer tokens.
    seq: u64,
    _subscriptions: Vec<Subscription>,
}

impl OverlaysState {
    pub fn new(window: &mut Window, cx: &mut Context<FmApp>) -> Self {
        let rename_input = cx.new(|cx| TextInput::new("new name", cx));
        let subscription = cx.subscribe_in(&rename_input, window, dialogs::on_rename_input_event);
        menu::detect_caps(cx);
        OverlaysState {
            focus: cx.focus_handle(),
            menu: None,
            caps: menu::Caps::default(),
            key_hints: false,
            confirm: None,
            properties: None,
            conflict: dialogs::ConflictUi::default(),
            rename_input,
            jobs_minimised: false,
            toast: None,
            seq: 0,
            _subscriptions: vec![subscription],
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

// ── layers & focus ───────────────────────────────────────────────────────────

fn top_layer(app: &FmApp) -> Option<Layer> {
    let o = &app.overlays;
    if o.menu.is_some() {
        Some(Layer::Menu)
    } else if o.key_hints {
        Some(Layer::KeyHints)
    } else if o.properties.is_some() {
        Some(Layer::Properties)
    } else if o.confirm.is_some() {
        Some(Layer::Confirm)
    } else if dialogs::current_conflict(app).is_some() {
        Some(Layer::Conflict)
    } else {
        None
    }
}

/// Full-window layer that swallows the mouse. The topmost one also owns the keyboard.
fn modal_layer(app: &FmApp, layer: Layer, context: &'static str, scrim: bool, cx: &App) -> Div {
    let mut el = div().absolute().inset_0().occlude().flex().items_center().justify_center();
    if scrim {
        el = el.bg(theme(cx).scrim());
    }
    if top_layer(app) == Some(layer) {
        let mut key_context = KeyContext::default();
        key_context.add(context);
        // Disables the list's letter keys (`Fm && !TextInput`) — see the module doc.
        key_context.add("TextInput");
        el = el.key_context(key_context).track_focus(&app.overlays.focus);
    }
    el
}

/// Put keyboard focus where it belongs: the topmost modal layer, or else the file list.
fn refocus(app: &FmApp, window: &mut Window) {
    if top_layer(app).is_some() {
        app.overlays.focus.focus(window);
    } else {
        app.focus_list(window);
    }
}

/// The box shared by the confirm / properties / key-hint / conflict modals.
fn dialog_surface(width: f32, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .w(px(width))
        .max_w(relative(0.94))
        .p(px(20.))
        .flex()
        .flex_col()
        .gap(px(14.))
        .bg(t.surface_2)
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_panel)
        .shadow(t.shadow_overlay())
        .occlude()
}

/// `dialog_surface` whose clicks do not reach the scrim's "click outside" handler.
fn dialog_box(width: f32, cx: &App) -> Div {
    dialog_surface(width, cx).on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Fade a freshly opened layer in.
fn fade_in(el: Div, id: impl Into<ElementId>) -> AnyElement {
    el.with_animation(id, Animation::new(Duration::from_millis(theme::DUR_FAST)), |el, delta| el.opacity(delta))
        .into_any_element()
}

/// Dialog button: the shared kit button at the prototype's 32 height.
fn dialog_button(id: &'static str, text: &'static str, kind: ButtonKind, cx: &App) -> gpui::Stateful<Div> {
    button(id, text, kind, cx).h(px(32.))
}

// ── public hooks ─────────────────────────────────────────────────────────────

/// Open a context menu at a window position (called by file_view.rs on right-click).
pub fn open_context_menu(
    app: &mut FmApp,
    target: MenuTarget,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<FmApp>,
) {
    menu::open(app, target, position, window, cx);
}

/// Whether a context menu is open. Hover tooltips underneath it are suppressed while true.
pub fn menu_open(app: &FmApp) -> bool {
    app.overlays.menu.is_some()
}

/// `?` — toggle the which-key grid.
pub fn act_key_hints(app: &mut FmApp, _: &KeyHints, window: &mut Window, cx: &mut Context<FmApp>) {
    app.overlays.key_hints = !app.overlays.key_hints;
    app.overlays.next_seq();
    refocus(app, window);
    cx.notify();
}

/// Ask before deleting `paths` for good (`shift+d`).
pub fn confirm_delete(app: &mut FmApp, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<FmApp>) {
    app.overlays.confirm = Some(paths);
    app.overlays.next_seq();
    refocus(app, window);
    cx.notify();
}

/// Show / hide the jobs panel — the tab badge is the way back to a minimised panel.
pub fn toggle_jobs_panel(app: &mut FmApp, cx: &mut Context<FmApp>) {
    app.overlays.jobs_minimised = !app.overlays.jobs_minimised;
    cx.notify();
}

/// A new job brings the panel back.
pub(crate) fn on_job_started(app: &mut FmApp, cx: &mut Context<FmApp>) {
    app.overlays.jobs_minimised = false;
    cx.notify();
}

/// `esc` cascade: close the topmost overlay. Return true if one was open.
pub fn cancel(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> bool {
    match top_layer(app) {
        Some(Layer::Menu) => app.overlays.menu = None,
        Some(Layer::KeyHints) => app.overlays.key_hints = false,
        Some(Layer::Properties) => app.overlays.properties = None,
        Some(Layer::Confirm) => app.overlays.confirm = None,
        Some(Layer::Conflict) => {
            // Cancelling the question cancels the whole job.
            app.overlays.conflict.renaming = false;
            if let Some(job_id) = dialogs::current_conflict(app).map(|(job, _)| job.id) {
                ops::cancel_job(app, job_id, cx);
            }
        }
        None if jobs_panel_visible(app) => app.overlays.jobs_minimised = true,
        None => return false,
    }
    refocus(app, window);
    cx.notify();
    true
}

/// Zero or more absolutely-positioned layers, bottom to top.
pub fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> Vec<AnyElement> {
    dialogs::sync_conflict_ui(app);

    // Dialogs can appear and disappear without a window at hand (a worker asks, a job
    // ends): reconcile keyboard focus here.
    let ours = app.overlays.focus.is_focused(window) || app.overlays.rename_input.read(cx).is_focused(window);
    match top_layer(app) {
        Some(_) if !ours => app.overlays.focus.focus(window),
        None if ours => app.focus_list(window),
        _ => {}
    }

    let mut layers = Vec::new();
    layers.extend(render_toast(app, cx));
    layers.extend(render_jobs(app, cx));
    layers.extend(dialogs::render_conflict(app, window, cx));
    layers.extend(dialogs::render_confirm(app, cx));
    layers.extend(dialogs::render_properties(app, cx));
    layers.extend(render_key_hints(app, cx));
    layers.extend(menu::render(app, window, cx));
    layers
}

// ── toasts ───────────────────────────────────────────────────────────────────

/// Show a toast (replaces the current one). Auto-dismisses after 5 s, 8 s with actions.
pub fn show_toast(app: &mut FmApp, toast: Toast, cx: &mut Context<FmApp>) {
    let seq = app.overlays.next_seq();
    let secs = if toast.open.is_some() || toast.undo.is_some() { 8 } else { 5 };
    app.overlays.toast = Some((seq, toast));
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(Duration::from_secs(secs)).await;
        this.update(cx, |app, cx| {
            if app.overlays.toast.as_ref().is_some_and(|(s, _)| *s == seq) {
                app.overlays.toast = None;
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
    cx.notify();
}

/// Plain confirmation toast: "3 files copied to clipboard".
/// Start the user's terminal (`$TERMINAL`, else the first one installed) in `dir`.
pub fn open_terminal(app: &mut FmApp, dir: &std::path::Path, cx: &mut Context<FmApp>) {
    menu::open_terminal(app, dir, cx);
}

pub fn toast(app: &mut FmApp, text: impl Into<SharedString>, cx: &mut Context<FmApp>) {
    show_toast(app, Toast { text: text.into(), error: false, open: None, undo: None }, cx);
}

/// Failure toast.
pub fn toast_error(app: &mut FmApp, text: impl Into<SharedString>, cx: &mut Context<FmApp>) {
    show_toast(app, Toast { text: text.into(), error: true, open: None, undo: None }, cx);
}

fn render_toast(app: &FmApp, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let (seq, toast) = app.overlays.toast.as_ref()?;
    let t = theme(cx).clone();
    let (mark, mark_color) = if toast.error { (Icon::Info, t.err) } else { (Icon::Check, t.ok) };

    let mut bar = div()
        .occlude()
        .h(px(40.))
        .max_w(relative(0.9))
        .px(px(14.))
        .flex()
        .items_center()
        .gap(px(12.))
        .bg(t.surface)
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_panel)
        .shadow(t.shadow_overlay())
        .child(icon(mark, 14.).text_color(mark_color))
        .child(div().min_w_0().truncate().text_size(px(12.5)).text_color(t.fg).child(toast.text.clone()));
    if toast.open.is_some() {
        bar = bar.child(button("toast-open", "Open", ButtonKind::Ghost, cx).h(px(28.)).on_click(cx.listener(
            |app, _, _, cx| {
                if let Some((_, Toast { open: Some((dir, item)), .. })) = app.overlays.toast.take() {
                    if app.cwd() != dir {
                        app.navigate(dir, cx);
                    }
                    if let Some(item) = item {
                        app.reload_and_focus(item, false, cx);
                    }
                }
                cx.notify();
            },
        )));
    }
    if toast.undo.is_some() {
        bar = bar.child(button("toast-undo", "Undo", ButtonKind::Ghost, cx).h(px(28.)).on_click(cx.listener(
            |app, _, _, cx| {
                if let Some((_, Toast { undo: Some(op), .. })) = app.overlays.toast.take() {
                    ops::undo(app, op, cx);
                }
                cx.notify();
            },
        )));
    }
    bar = bar.child(icon_button_sized("toast-close", Icon::Close, 24., 13., cx).on_click(cx.listener(|app, _, _, cx| {
        app.overlays.toast = None;
        cx.notify();
    })));

    Some(
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(TOAST_BOTTOM))
            .flex()
            .justify_center()
            .child(bar)
            .with_animation(
                ("toast", *seq),
                Animation::new(Duration::from_millis(theme::DUR_FLYOUT)).with_easing(ease_enter),
                |el, delta| el.opacity(delta).bottom(px(TOAST_BOTTOM - 8. * (1. - delta))),
            )
            .into_any_element(),
    )
}

// ── jobs panel ───────────────────────────────────────────────────────────────

fn panel_jobs(app: &FmApp) -> impl Iterator<Item = &Job> {
    app.ops.jobs.iter().filter(|j| !j.is_finished() && j.started.elapsed() >= JOB_PANEL_DELAY)
}

fn jobs_panel_visible(app: &FmApp) -> bool {
    !app.overlays.jobs_minimised && panel_jobs(app).next().is_some()
}

/// `abcdefghij` → `abc…hij` when longer than `max` characters.
fn truncate_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max < 3 {
        return text.to_string();
    }
    let head = (max - 1) / 2;
    let tail = max - 1 - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// `12 s left`, `1 min 40 s left`, `2 h 5 min left`.
fn format_eta(eta: Duration) -> String {
    let secs = eta.as_secs().max(1);
    if secs < 60 {
        format!("{secs} s left")
    } else if secs < 3600 {
        format!("{} min {} s left", secs / 60, secs % 60)
    } else {
        format!("{} h {} min left", secs / 3600, secs % 3600 / 60)
    }
}

fn render_jobs(app: &FmApp, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    if !jobs_panel_visible(app) {
        return None;
    }
    let t = theme(cx).clone();
    let jobs: Vec<&Job> = panel_jobs(app).collect();
    let count = jobs.len();

    let mut list = div().id("jobs-list").flex().flex_col().max_h(px(420.)).overflow_y_scroll();
    for (ix, job) in jobs.into_iter().enumerate() {
        let id = job.id;
        let paused = job.state == JobState::Paused;
        let kind_icon = match job.kind {
            TransferKind::Copy => Icon::Copy,
            TransferKind::Move => Icon::Move,
            TransferKind::Link => Icon::Link,
        };
        let route = format!(
            "{} → {}",
            truncate_middle(&display_path(&job.from), 33),
            truncate_middle(&display_path(&job.to), 33)
        );
        let left = match job.state {
            JobState::Running if job.total_bytes > 0 => format!(
                "{}/s · {} / {}",
                format_size(job.speed as u64),
                format_size(job.done_bytes),
                format_size(job.total_bytes)
            ),
            JobState::Running | JobState::Scanning => String::new(),
            _ if job.total_bytes > 0 => format!("{} / {}", format_size(job.done_bytes), format_size(job.total_bytes)),
            _ => String::new(),
        };
        let right = match job.state {
            JobState::Scanning => "scanning…".to_string(),
            JobState::Paused => "paused".to_string(),
            JobState::WaitingConflict => "waiting for an answer".to_string(),
            _ => job.eta.map(format_eta).unwrap_or_default(),
        };
        let (from, to) = (job.bar_from, job.bar_to);
        let fill = if paused { t.fg_muted } else { t.accent };
        let small = |text: String| {
            div().font_family(t.font_mono.clone()).text_size(px(11.)).text_color(t.fg_muted).child(text)
        };

        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .py(px(14.))
                .when(ix + 1 < count, |d| d.border_b_1().border_color(t.line))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(icon(kind_icon, 14.).text_color(t.accent))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(job.title.clone()),
                        )
                        .child(
                            icon_button_sized(("job-pause", id), if paused { Icon::Play } else { Icon::Pause }, 32., 14., cx)
                                .on_click(cx.listener(move |app, _, _, cx| ops::toggle_pause(app, id, cx))),
                        )
                        .child(
                            icon_button_sized(("job-minimise", id), Icon::Minus, 32., 14., cx)
                                .on_click(cx.listener(|app, _, _, cx| toggle_jobs_panel(app, cx))),
                        )
                        .child(
                            icon_button_sized(("job-cancel", id), Icon::Close, 32., 14., cx)
                                .on_click(cx.listener(move |app, _, _, cx| ops::cancel_job(app, id, cx))),
                        ),
                )
                .child(small(route).truncate())
                .child(
                    div().h(px(4.)).w_full().bg(t.line).child(div().h_full().bg(fill).with_animation(
                        ElementId::Name(format!("job-bar-{id}-{}", job.bar_gen).into()),
                        Animation::new(Duration::from_millis(150)),
                        move |el, delta| el.w(relative(from + (to - from) * delta)),
                    )),
                )
                .child(div().flex().justify_between().child(small(left)).child(small(right))),
        );
    }

    Some(
        div()
            .absolute()
            .top(px(theme::TABS_H + 6.))
            .right(px(12.))
            .w(px(520.))
            .max_w(relative(0.94))
            .occlude()
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .rounded(t.radius_panel)
            .shadow(t.shadow_overlay())
            .child(
                div()
                    .px(px(18.))
                    .pt(px(16.))
                    .pb(px(2.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(12.))
                            .pb(px(10.))
                            .border_b_1()
                            .border_color(t.line)
                            .child(
                                div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Operations"),
                            )
                            .child(label(ops::plural(count as u64, "job"), cx)),
                    )
                    .child(list),
            )
            .into_any_element(),
    )
}

// ── key hints ────────────────────────────────────────────────────────────────

type HintColumn = (&'static str, &'static [(&'static [&'static str], &'static str)]);

/// Every binding of actions.rs (SPEC §8), in three columns.
const KEY_HINTS: [HintColumn; 3] = [
    (
        "go",
        &[
            (&["↵", "l"], "open"),
            (&["↑", "↓", "j", "k"], "move cursor"),
            (&["h", "-", "⌫"], "parent folder"),
            (&["←", "→"], "parent / enter · move in grid"),
            (&["home", "end"], "first / last"),
            (&["pgup", "pgdn"], "page up / down"),
            (&["alt ←", "alt →"], "back / forward"),
            (&["g h"], "home"),
            (&["g d"], "dots"),
            (&["g t"], "trash"),
            (&["ctrl l"], "edit path"),
            (&["/"], "filter"),
            (&["ctrl /"], "recursive search"),
            (&["ctrl t", "ctrl w"], "new / close tab"),
            (&["ctrl tab"], "next tab · shift for previous"),
            (&["t", "F4"], "terminal in this folder"),
            (&["ctrl d"], "pin / unpin folder in sidebar"),
        ],
    ),
    (
        "edit",
        &[
            (&["r", "F2"], "rename"),
            (&["y"], "copy"),
            (&["x"], "cut"),
            (&["p"], "paste"),
            (&["e"], "extract archive here"),
            (&["d", "del"], "move to trash"),
            (&["shift d"], "delete permanently"),
            (&["shift ↑", "shift ↓"], "extend selection"),
            (&["ins"], "toggle selection"),
            (&["ctrl a"], "select all"),
            (&["esc"], "close overlay / cancel"),
        ],
    ),
    (
        "view",
        &[
            (&["space"], "quick look"),
            (&["f"], "full screen (in quick look)"),
            (&["ctrl c"], "copy path (in quick look)"),
            (&["v"], "toggle grid"),
            (&["."], "toggle hidden"),
            (&["s"], "sort menu"),
            (&["z"], "toggle preview pane"),
            (&["F5", "ctrl r"], "refresh"),
            (&["ctrl shift t"], "next theme"),
            (&["?"], "key hints"),
            (&["ctrl q"], "quit"),
        ],
    ),
];

fn render_key_hints(app: &FmApp, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    if !app.overlays.key_hints {
        return None;
    }
    let t = theme(cx).clone();
    let close = |app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>| {
        app.overlays.key_hints = false;
        refocus(app, window);
        cx.notify();
    };

    let mut grid = div().flex().items_start().gap(px(28.));
    for (title, rows) in KEY_HINTS {
        let mut column = div().flex_1().min_w_0().flex().flex_col().gap(px(8.)).child(label(title, cx));
        for (keys, text) in rows {
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex().flex_none().gap(px(4.)).children(keys.iter().map(|k| keycap(*k, cx))))
                    .child(div().min_w_0().truncate().text_size(px(12.)).text_color(t.fg_muted).child(*text)),
            );
        }
        grid = grid.child(column);
    }

    let layer = modal_layer(app, Layer::KeyHints, "FmKeyHints", true, cx)
        // Any key or click closes it.
        .on_key_down(cx.listener(move |app, _, window, cx| {
            cx.stop_propagation();
            close(app, window, cx)
        }))
        .on_any_mouse_down(cx.listener(move |app, _, window, cx| close(app, window, cx)))
        .child(dialog_surface(860., cx).child(grid));
    Some(fade_in(layer, ("key-hints", app.overlays.seq)))
}

/// Remove a tag from an item.
fn remove_tag(app: &mut FmApp, path: PathBuf, name: &'static str, cx: &mut Context<FmApp>) {
    crate::sidebar::remove_tag(app, &path, name, cx);
    toast(app, format!("Untagged {} · {name}", ops::name_of(&path)), cx);
}

/// Tag an item. `name` is one of red / yellow / green / blue / accent.
fn add_tag(app: &mut FmApp, path: PathBuf, name: &'static str, cx: &mut Context<FmApp>) {
    crate::sidebar::add_tag(app, &path, name, cx);
    toast(app, format!("Tagged {} · {name}", ops::name_of(&path)), cx);
}

