//! The main column below the toolbar: list view (§4.3), grid view (§4.4), the states of §6
//! (empty / no results / permission denied / device unavailable / loading skeleton),
//! inline rename, the floating selection bar and drag & drop.
//!
//! OWNER: file-view agent.
//!
//! - `states.rs` — skeleton and the centred empty / error blocks
//! - `rename.rs` — inline rename
//! - `dnd.rs`    — drag payload, ghost chip, drop targets, spring-open

mod dnd;
mod marquee;
mod rename;
mod states;

use std::{cell::Cell, ops::Range, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, DragMoveEvent, Entity, Focusable, HighlightStyle, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, Point, ScrollStrategy, Stateful, StyledText, Subscription,
    UniformListScrollHandle, Window, canvas, div, img, prelude::*, px, transparent_black, uniform_list,
};

use crate::{
    actions::{self, Rename},
    app::{FmApp, LoadState, ViewMode},
    fs_model::{self, Entry, FileType, SortKey},
    icons::{Icon, icon},
    overlays::{self, MenuTarget},
    text_input::TextInput,
    theme::{self, Theme, theme},
    ui::{self, ButtonKind, alpha},
};

use dnd::{DragGhost, DragPayload, DropState};
use rename::RenameState;

// ── grid metrics (SPEC §4.4) ──────────────────────────────────────────────────
const GRID_MIN_TILE: f32 = 160.;
pub(super) const GRID_GAP: f32 = 10.;
pub(super) const GRID_PAD: f32 = 14.;
const THUMB_H: f32 = 74.;
const TILE_NAME_H: f32 = 16.;
const TILE_SIZE_H: f32 = 14.;
/// border + padding + thumbnail + gap + name + gap + size + padding + border
pub(super) const TILE_H: f32 = 1. + 8. + THUMB_H + 8. + TILE_NAME_H + 8. + TILE_SIZE_H + 8. + 1.;
/// Images above this size get an icon instead of a decoded thumbnail.
const THUMB_MAX_BYTES: u64 = 20 * 1024 * 1024;

pub struct FileViewState {
    /// Number of columns the grid view laid out last frame. The shell uses it for ↑/↓ in
    /// grid mode — keep it up to date when rendering the grid.
    pub grid_columns: usize,
    grid_scroll: UniformListScrollHandle,
    /// (tab, folder, cursor, columns) the grid was last scrolled for.
    grid_seen: Option<(u64, PathBuf, usize, usize)>,
    /// Width of the file area as measured during the last paint (0 before the first one).
    width: Rc<Cell<f32>>,
    rename_input: Entity<TextInput>,
    rename: Option<RenameState>,
    drop: DropState,
    marquee: marquee::MarqueeState,
    _subscriptions: Vec<Subscription>,
}

impl FileViewState {
    pub fn new(window: &mut Window, cx: &mut Context<FmApp>) -> Self {
        let rename_input = cx.new(|cx| TextInput::new("", cx));
        let subscriptions = vec![
            cx.subscribe_in(&rename_input, window, rename::on_input_event),
            cx.on_blur(&rename_input.focus_handle(cx), window, rename::on_blur),
        ];
        FileViewState {
            grid_columns: 5,
            grid_scroll: UniformListScrollHandle::new(),
            grid_seen: None,
            width: Rc::new(Cell::new(0.)),
            rename_input,
            rename: None,
            drop: DropState::default(),
            marquee: marquee::MarqueeState::default(),
            _subscriptions: subscriptions,
        }
    }
}

/// Start an inline rename of `path` in the active tab (used after "New file"/"New folder").
/// No `Window` here on purpose: it is called from an async completion; take focus on the
/// next render.
pub fn request_rename(app: &mut FmApp, path: PathBuf, cx: &mut Context<FmApp>) {
    rename::request(app, path, cx);
}

/// `r` / `F2` — rename the cursor row.
pub fn act_rename(app: &mut FmApp, _: &Rename, window: &mut Window, cx: &mut Context<FmApp>) {
    rename::start_on_cursor(app, window, cx);
}

/// `esc` cascade: cancel an inline rename or a drag. Return true if something was cancelled.
pub fn cancel(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> bool {
    if marquee::cancel(app, cx) {
        return true;
    }
    if cx.has_active_drag() {
        cx.stop_active_drag(window);
        app.files.drop = DropState { target: None, generation: app.files.drop.generation + 1 };
        cx.notify();
        return true;
    }
    if rename::discard(app, cx) {
        app.focus_list(window);
        return true;
    }
    false
}

// ── per-frame context ─────────────────────────────────────────────────────────

/// Everything rows and tiles need that is the same for the whole frame.
struct Frame {
    t: Theme,
    /// The multi-selection as a drag payload (shared by every selected row).
    selection: Option<Arc<Vec<PathBuf>>>,
    /// Folder under an active drag.
    drop_target: Option<PathBuf>,
    drop_generation: usize,
    renaming: Option<PathBuf>,
}

impl Frame {
    fn new(app: &FmApp, cx: &App) -> Self {
        let tab = app.tab();
        Frame {
            t: theme(cx).clone(),
            selection: (!tab.selected.is_empty()).then(|| Arc::new(tab.targets())),
            drop_target: app.files.drop.target.clone().filter(|_| cx.has_active_drag()),
            drop_generation: app.files.drop.generation,
            renaming: app.files.rename.as_ref().map(|r| r.path.clone()),
        }
    }
}

/// How one entry is drawn this frame.
struct Look {
    cursor: bool,
    selected: bool,
    cut: bool,
    renaming: bool,
    drop_target: bool,
    opacity: f32,
}

impl Look {
    fn of(app: &FmApp, row: usize, entry: &Entry, f: &Frame) -> Self {
        let tab = app.tab();
        let cut = app.ops.is_cut(&entry.path);
        let renaming = f.renaming.as_deref() == Some(entry.path.as_path());
        Look {
            cursor: row == tab.cursor,
            selected: tab.is_selected(&entry.path),
            cut,
            renaming,
            drop_target: f.drop_target.as_deref() == Some(entry.path.as_path()),
            opacity: match (renaming, cut, entry.hidden) {
                (true, ..) => 1.,
                (_, true, _) => 0.5,
                (_, _, true) => 0.55,
                _ => 1.,
            },
        }
    }
}

fn entry_icon(entry: &Entry) -> Icon {
    if entry.is_symlink { Icon::Link } else { entry.file_type.icon() }
}

// ── mouse ─────────────────────────────────────────────────────────────────────

/// The row `path` is on now (rows can move between the render and the event).
fn current_row(app: &FmApp, row: usize, path: &PathBuf) -> Option<usize> {
    let tab = app.tab();
    if tab.entry_at(row).is_some_and(|e| e.path == *path) { Some(row) } else { tab.row_of(path) }
}

fn is_renaming(app: &FmApp, path: &PathBuf) -> bool {
    app.files.rename.as_ref().is_some_and(|r| r.path == *path)
}

fn on_entry_click(app: &mut FmApp, row: usize, path: &PathBuf, ev: &ClickEvent, window: &mut Window, cx: &mut Context<FmApp>) {
    cx.stop_propagation();
    if is_renaming(app, path) {
        return; // the click belongs to the text field
    }
    app.focus_list(window);
    if std::mem::take(&mut app.files.marquee.suppress_click) {
        return; // this mouse-up ended a rubber band
    }
    let Some(row) = current_row(app, row, path) else { return };
    let modifiers = ev.modifiers();
    if modifiers.shift {
        let anchor = app.tab().anchor.unwrap_or(app.tab().cursor);
        app.select_range(anchor, row, cx);
        app.set_cursor(row, cx);
        app.tab_mut().anchor = Some(anchor);
    } else if modifiers.control || modifiers.platform {
        app.toggle_selected(row, cx);
        app.set_cursor(row, cx);
        app.tab_mut().anchor = Some(row);
    } else {
        app.clear_selection(cx);
        app.set_cursor(row, cx);
        if ev.click_count() >= 2 {
            app.open_cursor(cx);
        }
    }
}

fn on_entry_context(app: &mut FmApp, row: usize, path: &PathBuf, position: Point<Pixels>, window: &mut Window, cx: &mut Context<FmApp>) {
    cx.stop_propagation();
    if is_renaming(app, path) {
        return;
    }
    app.focus_list(window);
    let Some(row) = current_row(app, row, path) else { return };
    if app.tab().is_selected(path) {
        overlays::open_context_menu(app, MenuTarget::Selection, position, window, cx);
    } else {
        app.clear_selection(cx);
        app.set_cursor(row, cx);
        overlays::open_context_menu(app, MenuTarget::Entry(path.clone()), position, window, cx);
    }
}

/// Click, context menu and (for folders) drop target — shared by rows and tiles.
fn wire(el: Stateful<Div>, row: usize, entry: &Entry, cx: &mut Context<FmApp>) -> Stateful<Div> {
    let path = entry.path.clone();
    let el = el
        .on_click(cx.listener({
            let path = path.clone();
            move |this, ev: &ClickEvent, window, cx| on_entry_click(this, row, &path, ev, window, cx)
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener({
                let path = path.clone();
                move |this, ev: &MouseDownEvent, window, cx| on_entry_context(this, row, &path, ev.position, window, cx)
            }),
        );
    if !entry.is_dir {
        return el;
    }
    el.on_drag_move(cx.listener({
        let path = path.clone();
        move |this, ev: &DragMoveEvent<DragPayload>, _, cx| {
            let inside = ev.bounds.contains(&ev.event.position);
            let droppable = inside && dnd::can_drop(&ev.drag(cx).paths, &path);
            dnd::hover(this, &path, inside, droppable, cx);
        }
    }))
    .on_drop(cx.listener(move |this, payload: &DragPayload, window, cx| {
        dnd::drop_into(this, payload, path.clone(), window, cx);
    }))
}

/// The part of a row/tile that starts a file drag (list: icon + name, grid: whole tile).
/// Presses elsewhere start a rubber-band selection instead.
fn drag_handle(el: Stateful<Div>, entry: &Entry, look: &Look, f: &Frame, cx: &mut Context<FmApp>) -> Stateful<Div> {
    if look.renaming {
        return el; // dragging inside the field selects text
    }
    let payload = DragPayload {
        paths: match &f.selection {
            Some(selection) if look.selected => selection.clone(),
            _ => Arc::new(vec![entry.path.clone()]),
        },
        icon: entry_icon(entry),
    };
    el.on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.files.marquee.press_on_handle = true))
        .on_drag(payload, |payload: &DragPayload, offset, window, cx| {
            let modifiers = window.modifiers();
            cx.new(|_| DragGhost::new(payload, offset, modifiers))
        })
}

/// Make any element outside the file area a drop target for dragged files: dropping moves
/// (ctrl: copies, alt: links) them into `path`; hovering 600 ms springs the folder open.
/// Used by the toolbar crumbs (drop into the parent or any ancestor) and sidebar places.
pub fn as_drop_target(app: &FmApp, el: Stateful<Div>, path: PathBuf, cx: &mut Context<FmApp>) -> Stateful<Div> {
    let t = theme(cx).clone();
    let is_target = app.files.drop.target.as_deref() == Some(path.as_path());
    let generation = app.files.drop.generation;
    el.relative()
        .when(is_target, |d| d.child(dnd::outline(&t, generation, 0., 0.)))
        .on_drag_move(cx.listener({
            let path = path.clone();
            move |this, ev: &DragMoveEvent<DragPayload>, _, cx| {
                let inside = ev.bounds.contains(&ev.event.position);
                let droppable = inside && dnd::can_drop(&ev.drag(cx).paths, &path);
                dnd::hover(this, &path, inside, droppable, cx);
            }
        }))
        .on_drop(cx.listener(move |this, payload: &DragPayload, window, cx| {
            dnd::drop_into(this, payload, path.clone(), window, cx);
        }))
}

// ── list view (SPEC §4.3) ─────────────────────────────────────────────────────

fn sort_label(app: &FmApp, text: &'static str, key: SortKey, cx: &mut Context<FmApp>) -> Stateful<Div> {
    let t = theme(cx);
    let active = app.sort.key == key;
    let chevron = if app.sort.ascending { Icon::ChevronUp } else { Icon::ChevronDown };
    div()
        .id(text)
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .child(ui::label(text, cx).when(active, |l| l.text_color(t.fg)))
        .when(active, |d| d.child(icon(chevron, 10.).text_color(t.accent)))
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.focus_list(window);
            this.sort_by(key, cx);
        }))
}

/// Header and rows share this column layout: `22 | 1fr | 70 | 120 | 96 | 24`, gap 12.
/// The header's left padding includes the rows' 2px cursor border.
fn list_header(app: &FmApp, cx: &mut Context<FmApp>) -> Div {
    let line = theme(cx).line;
    div()
        .h(px(theme::LIST_HEADER_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(12.))
        .pl(px(16.))
        .pr(px(14.))
        .border_b_1()
        .border_color(line)
        .child(div().w(px(22.)).flex_none())
        .child(div().flex_1().min_w_0().flex().child(sort_label(app, "name", SortKey::Name, cx)))
        .child(div().w(px(70.)).flex_none().flex().justify_end().child(sort_label(app, "size", SortKey::Size, cx)))
        .child(div().w(px(120.)).flex_none().flex().child(sort_label(app, "modified", SortKey::Modified, cx)))
        .child(div().w(px(96.)).flex_none().child(ui::label("perms", cx)))
        .child(div().w(px(24.)).flex_none().child(ui::label("git", cx)))
}

/// `name/`, or `name → target` for symlinks with the arrow and target muted.
fn name_text(entry: &Entry, t: &Theme) -> AnyElement {
    let name = entry.display_name();
    match &entry.symlink_target {
        Some(target) if entry.is_symlink => {
            let text = format!("{name} → {}", target.display());
            let muted = HighlightStyle { color: Some(t.fg_muted), ..Default::default() };
            StyledText::new(text.clone()).with_highlights([(name.len()..text.len(), muted)]).into_any_element()
        }
        _ => name.into_any_element(),
    }
}

fn list_row(app: &FmApp, row: usize, entry: &Entry, f: &Frame, cx: &mut Context<FmApp>) -> AnyElement {
    let t = &f.t;
    let look = Look::of(app, row, entry, f);
    let highlighted = look.cursor || look.selected;
    let hover_bg = alpha(t.surface_2, 0.6);

    let name_cell = div().relative().flex_1().min_w_0().flex().items_center().gap(px(7.));
    let name_cell = if look.renaming {
        name_cell.child(rename::field(app, 12., cx)).children(rename::message(app, 29., cx))
    } else {
        name_cell.when(look.selected, |d| d.child(div().w(px(3.)).h(px(14.)).flex_none().bg(t.accent))).child(
            // Shrinks to the name, so the rest of the row is free for rubber-band selection.
            drag_handle(
                div().id(("name", row)).min_w_0().flex_shrink().truncate().when(look.cut, |d| d.italic()),
                entry,
                &look,
                f,
                cx,
            )
            .child(name_text(entry, t)),
        )
    };
    let meta = |width: f32| div().w(px(width)).flex_none().overflow_hidden().whitespace_nowrap().text_color(t.fg_muted);
    let git = app.git.status_for(&entry.path);

    let el = div()
        .id(("row", row))
        .relative()
        .h(px(theme::ROW_H))
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .border_l_2()
        .border_color(if look.cursor { t.accent } else { transparent_black() })
        .font_family(t.font_mono.clone())
        .text_size(px(12.))
        .text_color(t.fg)
        .map(|d| if highlighted { d.bg(t.surface_2) } else { d.hover(move |s| s.bg(hover_bg)) })
        .when(look.opacity < 1., |d| d.opacity(look.opacity))
        .child(
            div()
                .w(px(22.))
                .flex_none()
                .flex()
                .child(icon(entry_icon(entry), 14.).text_color(if look.cursor { t.accent } else { t.fg_muted })),
        )
        .child(name_cell)
        .child(meta(70.).flex().justify_end().child(entry.size_label()))
        .child(meta(120.).child(fs_model::format_mtime(entry.modified)))
        .child(meta(96.).child(fs_model::format_perms(entry.mode)))
        .child(div().w(px(24.)).flex_none().when_some(git, |d, status| d.text_color(status.color(t)).child(status.letter())))
        .when(look.drop_target, |d| d.child(dnd::outline(t, f.drop_generation, 2., 0.)));
    wire(el, row, entry, cx).into_any_element()
}

fn list_body(app: &FmApp, cx: &mut Context<FmApp>) -> AnyElement {
    uniform_list(
        "file-list",
        app.tab().visible.len(),
        cx.processor(|this, range: Range<usize>, _window, cx| {
            let frame = Frame::new(this, cx);
            range
                .filter_map(|row| this.tab().entry_at(row).map(|entry| list_row(this, row, entry, &frame, cx)))
                .collect::<Vec<_>>()
        }),
    )
    .track_scroll(app.tab().scroll.clone())
    .size_full()
    .into_any_element()
}

// ── grid view (SPEC §4.4) ─────────────────────────────────────────────────────

/// Auto-fill with a 160px minimum: as many columns as fit in `width`.
fn grid_columns_for(width: f32) -> usize {
    (((width - 2. * GRID_PAD + GRID_GAP) / (GRID_MIN_TILE + GRID_GAP)).floor() as usize).max(1)
}

fn thumbnail(row: usize, entry: &Entry, look: &Look, t: &Theme) -> Div {
    let (ic, color, line) = (entry_icon(entry), if look.cursor { t.accent } else { t.fg_muted }, t.line);
    let frame = div().h(px(THUMB_H)).w_full().flex_none().flex().items_center().justify_center().overflow_hidden().bg(t.bg);
    let is_image = entry.file_type == FileType::Image && !entry.is_dir && entry.size > 0 && entry.size <= THUMB_MAX_BYTES;
    if !is_image {
        return frame.child(icon(ic, 26.).text_color(color));
    }
    frame.child(
        img(entry.path.clone())
            .id(("thumb", row))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .object_fit(ObjectFit::Cover)
            .with_loading(move || states::pulse(div().size_full().bg(line), "thumb-loading", 0).into_any_element())
            .with_fallback(move || icon(ic, 26.).text_color(color).into_any_element()),
    )
}

fn grid_tile(app: &FmApp, row: usize, entry: &Entry, f: &Frame, cx: &mut Context<FmApp>) -> AnyElement {
    let t = &f.t;
    let look = Look::of(app, row, entry, f);
    let highlighted = look.cursor || look.selected;

    let el = div()
        .id(("tile", row))
        .relative()
        .flex_1()
        .min_w_0()
        .h(px(TILE_H))
        .flex()
        .flex_col()
        .gap(px(8.))
        .p(px(8.))
        .border_1()
        .border_color(if highlighted { t.accent } else { t.line })
        .when(highlighted, |d| d.bg(t.surface_2))
        .when(look.opacity < 1., |d| d.opacity(look.opacity))
        .font_family(t.font_mono.clone())
        .child(thumbnail(row, entry, &look, t));
    let el = if look.renaming {
        el.child(
            div()
                .relative()
                .h(px(TILE_NAME_H + 8. + TILE_SIZE_H))
                .flex()
                .items_center()
                .child(rename::field(app, 11., cx))
                .children(rename::message(app, TILE_NAME_H + 8. + TILE_SIZE_H + 10., cx)),
        )
    } else {
        el.child(
            div()
                .h(px(TILE_NAME_H))
                .line_height(px(TILE_NAME_H))
                .text_size(px(11.))
                .text_color(t.fg)
                .truncate()
                .when(look.cut, |d| d.italic())
                .child(entry.display_name()),
        )
        .child(
            div()
                .h(px(TILE_SIZE_H))
                .line_height(px(TILE_SIZE_H))
                .text_size(px(10.))
                .text_color(t.fg_muted)
                .truncate()
                .child(entry.size_label()),
        )
    };
    let el = el.when(look.drop_target, |d| d.child(dnd::outline(t, f.drop_generation, 1., 1.)));
    let el = drag_handle(el, entry, &look, f, cx);
    wire(el, row, entry, cx).into_any_element()
}

fn grid_body(app: &mut FmApp, cx: &mut Context<FmApp>) -> AnyElement {
    let columns = app.files.grid_columns.max(1);
    let tab = app.tab();
    let count = tab.visible.len();
    let seen = (tab.id, tab.path.clone(), tab.cursor, columns);
    if app.files.grid_seen.as_ref() != Some(&seen) {
        app.files.grid_scroll.scroll_to_item(seen.2 / columns, ScrollStrategy::Top);
        app.files.grid_seen = Some(seen);
    }
    uniform_list(
        "file-grid",
        count.div_ceil(columns),
        cx.processor(move |this, range: Range<usize>, _window, cx| {
            let frame = Frame::new(this, cx);
            range
                .map(|grid_row| {
                    let first = grid_row * columns;
                    div().h(px(TILE_H + GRID_GAP)).w_full().px(px(GRID_PAD)).flex().items_start().gap(px(GRID_GAP)).children(
                        (first..first + columns).map(|row| match this.tab().entry_at(row) {
                            Some(entry) => grid_tile(this, row, entry, &frame, cx),
                            // Same padding + border as a tile, or flex would hand the real tiles the difference.
                            None => div().flex_1().min_w_0().p(px(8.)).border_1().border_color(gpui::transparent_black()).into_any_element(),
                        }),
                    )
                })
                .collect::<Vec<_>>()
        }),
    )
    .track_scroll(app.files.grid_scroll.clone())
    .size_full()
    .pt(px(GRID_PAD))
    .pb(px(GRID_PAD - GRID_GAP))
    .into_any_element()
}

// ── selection bar (SPEC §6) ───────────────────────────────────────────────────

fn selection_bar(app: &FmApp, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let tab = app.tab();
    let action_button = |id: &'static str, ic: Icon, action: fn() -> Box<dyn gpui::Action>, cx: &mut Context<FmApp>| {
        ui::icon_button_sized(id, ic, 32., 14., cx).on_click(cx.listener(move |this, _, window, cx| {
            this.focus_list(window);
            window.dispatch_action(action(), cx);
        }))
    };
    div()
        .id("selection-bar")
        .absolute()
        .left(px(12.))
        .right(px(12.))
        .bottom(px(12.))
        .h(px(40.))
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(12.))
        .bg(t.surface)
        .border_1()
        .border_color(t.accent)
        .rounded(t.radius_control)
        .shadow(t.shadow_overlay())
        .occlude()
        .child(ui::mono(format!("{} selected", tab.selected.len()), cx).flex_none().text_color(t.accent))
        .child(ui::mono(fs_model::format_size(tab.selected_size()), cx).flex_none().text_color(t.fg_muted))
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .items_center()
                .child(action_button("selection-copy", Icon::Copy, || Box::new(actions::Copy), cx))
                .child(action_button("selection-move", Icon::Move, || Box::new(actions::Cut), cx))
                .child(action_button("selection-trash", Icon::Trash, || Box::new(actions::Trash), cx)),
        )
        .child(ui::button("selection-invert", "Invert", ButtonKind::Ghost, cx).on_click(cx.listener(
            |this, _, window, cx| {
                this.focus_list(window);
                window.dispatch_action(Box::new(actions::InvertSelection), cx);
            },
        )))
        .child(ui::icon_button_sized("selection-clear", Icon::Close, 32., 14., cx).on_click(cx.listener(
            |this, _, window, cx| {
                this.focus_list(window);
                this.clear_selection(cx);
            },
        )))
        .into_any_element()
}

// ── root ──────────────────────────────────────────────────────────────────────

enum Body {
    Skeleton,
    Error,
    Empty,
    NoMatches,
    List,
    Grid,
}

/// Records the width of the file area while painting; re-renders when it changed so the
/// grid can recompute its column count.
fn measure_width(app: &FmApp, cx: &mut Context<FmApp>) -> impl IntoElement {
    let (width, this) = (app.files.width.clone(), cx.weak_entity());
    canvas(
        move |bounds, window, _| {
            let measured = f32::from(bounds.size.width);
            if (measured - width.get()).abs() > 0.5 {
                width.set(measured);
                window.on_next_frame(move |_, cx| {
                    this.update(cx, |_, cx| cx.notify()).ok();
                });
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

pub fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    rename::sync(app, window, cx);

    // Columns come from last frame's measured width; before the first paint, estimate.
    let width = match app.files.width.get() {
        w if w > 0. => w,
        _ => {
            let viewport = f32::from(window.viewport_size().width);
            let preview = if app.show_preview && viewport >= theme::BREAK_PREVIEW { theme::PREVIEW_W } else { 0. };
            viewport - 2. - theme::SIDEBAR_W - preview
        }
    };
    app.files.grid_columns = grid_columns_for(width);

    let tab = app.tab();
    let body = match &tab.load {
        LoadState::Error(_) => Body::Error,
        LoadState::Loading if tab.entries.is_empty() => Body::Skeleton,
        _ if !tab.visible.is_empty() => match app.view_mode {
            ViewMode::List => Body::List,
            ViewMode::Grid => Body::Grid,
        },
        _ if !tab.filter.is_empty() && !tab.entries.is_empty() => Body::NoMatches,
        _ => Body::Empty,
    };
    if !matches!(body, Body::Grid) {
        app.files.grid_seen = None;
    }
    let show_header = app.view_mode == ViewMode::List && matches!(body, Body::List | Body::Skeleton);
    let browsable = !matches!(body, Body::Error | Body::Skeleton);
    let show_selection_bar = !app.tab().selected.is_empty() && matches!(body, Body::List | Body::Grid);

    let body = match body {
        Body::Skeleton => states::skeleton(cx),
        Body::Error => match app.tab().load.clone() {
            LoadState::Error(err) => states::error(app, &err, cx),
            _ => states::empty(app, cx),
        },
        Body::Empty => states::empty(app, cx),
        Body::NoMatches => states::no_matches(app, cx),
        Body::List => list_body(app, cx),
        Body::Grid => grid_body(app, cx),
    };

    div()
        .id("file-area")
        .relative()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(measure_width(app, cx))
        .child(marquee::tracker(app, cx))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                this.focus_list(window);
                let extend = ev.modifiers.control || ev.modifiers.shift || ev.modifiers.platform;
                marquee::on_mouse_down(this, ev.position, extend);
                cx.notify();
            }),
        )
        // Empty space: click clears the selection, right-click opens the folder menu,
        // a drop lands in the current folder (after a spring-open).
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus_list(window);
            if !std::mem::take(&mut this.files.marquee.suppress_click) {
                this.clear_selection(cx);
            }
        }))
        .when(browsable, |d| {
            d.on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.focus_list(window);
                    overlays::open_context_menu(this, MenuTarget::Empty, ev.position, window, cx);
                }),
            )
            .on_drop(cx.listener(|this, payload: &DragPayload, window, cx| {
                let dest = this.cwd().to_path_buf();
                dnd::drop_into(this, payload, dest, window, cx);
            }))
        })
        .when(show_header, |d| d.child(list_header(app, cx)))
        .child(div().flex_1().min_h_0().child(body))
        .children(marquee::overlay(app, window, cx))
        .when(show_selection_bar, |d| d.child(selection_bar(app, cx)))
        .into_any_element()
}
