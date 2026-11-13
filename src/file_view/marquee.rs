//! Rubber-band selection: press on empty space (or, in the list, anywhere on a row except its
//! name) and drag. Plain drag replaces the selection; ctrl/shift adds to it. Dragging past the
//! top or bottom edge scrolls. `esc` cancels and restores the previous selection.
//!
//! Rows stay drag sources for drag & drop: in the list only the icon + name start a file drag,
//! in the grid the whole tile does (see `file_view::drag_handle`).

use std::{
    cell::Cell,
    collections::BTreeSet,
    path::PathBuf,
    rc::Rc,
};

use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, Window,
    canvas, div, point, prelude::*, px,
};

use super::{GRID_GAP, GRID_PAD, TILE_H};
use crate::{
    app::{FmApp, ViewMode},
    theme::{self, theme},
    ui::alpha,
};

/// Pointer travel before a press becomes a rubber band.
const THRESHOLD: f32 = 4.;
/// Largest scroll step per mouse move when the pointer is past an edge.
const EDGE_SCROLL_MAX: f32 = 28.;

#[derive(Default)]
pub struct MarqueeState {
    band: Option<Band>,
    /// Bounds of the file area in window coordinates, from the last paint.
    pub area: Rc<Cell<Bounds<Pixels>>>,
    /// The press began on a row's drag handle: leave it to drag & drop.
    pub press_on_handle: bool,
    /// A band just ended; swallow the click that the same mouse-up produces.
    pub suppress_click: bool,
}

struct Band {
    /// Press position in content coordinates (scroll-independent).
    start: Point<Pixels>,
    start_window: Point<Pixels>,
    current: Point<Pixels>,
    /// Selection to add to (ctrl/shift) — empty for a plain band.
    base: BTreeSet<PathBuf>,
    /// Selection before the band, restored by `esc`.
    before: BTreeSet<PathBuf>,
    before_cursor: usize,
    active: bool,
}

/// Bounds and scroll offset of whatever is showing the entries.
fn viewport(app: &FmApp) -> (Bounds<Pixels>, Point<Pixels>) {
    let state = match app.view_mode {
        ViewMode::List => app.tab().scroll.0.borrow(),
        ViewMode::Grid => app.files.grid_scroll.0.borrow(),
    };
    (state.base_handle.bounds(), state.base_handle.offset())
}

fn to_content(p: Point<Pixels>, bounds: Bounds<Pixels>, offset: Point<Pixels>) -> Point<Pixels> {
    point(p.x - bounds.left() - offset.x, p.y - bounds.top() - offset.y)
}

/// Left mouse down anywhere in the file area (bubbles up from rows).
pub fn on_mouse_down(app: &mut FmApp, position: Point<Pixels>, ctrl_or_shift: bool) {
    let m = &mut app.files.marquee;
    m.suppress_click = false;
    if std::mem::take(&mut m.press_on_handle) || app.files.rename.is_some() {
        return;
    }
    let (bounds, offset) = viewport(app);
    if !bounds.contains(&position) {
        return; // header, selection bar edge, etc.
    }
    let tab = app.tab();
    let before = tab.selected.clone();
    app.files.marquee.band = Some(Band {
        start: to_content(position, bounds, offset),
        start_window: position,
        current: position,
        base: if ctrl_or_shift { before.clone() } else { BTreeSet::new() },
        before,
        before_cursor: tab.cursor,
        active: false,
    });
}

pub fn is_active(app: &FmApp) -> bool {
    app.files.marquee.band.as_ref().is_some_and(|b| b.active)
}

/// `esc`: drop the band and put the selection back. True if a band was active.
pub fn cancel(app: &mut FmApp, cx: &mut Context<FmApp>) -> bool {
    let Some(band) = app.files.marquee.band.take() else { return false };
    if !band.active {
        return false;
    }
    let tab = app.tab_mut();
    tab.selected = band.before;
    tab.cursor = band.before_cursor;
    app.selection_changed(cx);
    cx.notify();
    true
}

fn on_move(app: &mut FmApp, ev: &MouseMoveEvent, cx: &mut Context<FmApp>) {
    if ev.pressed_button != Some(MouseButton::Left) || cx.has_active_drag() {
        // Released outside the window, or a file drag took over.
        if app.files.marquee.band.take().is_some_and(|b| b.active) {
            app.files.marquee.suppress_click = true;
        }
        return cx.notify();
    }
    let Some(band) = app.files.marquee.band.as_mut() else { return };
    band.current = ev.position;
    if !band.active {
        let d = band.current - band.start_window;
        if f32::from(d.x).abs() < THRESHOLD && f32::from(d.y).abs() < THRESHOLD {
            return;
        }
        band.active = true;
    }
    edge_scroll(app);
    apply(app, cx);
}

fn on_up(app: &mut FmApp, cx: &mut Context<FmApp>) {
    if let Some(band) = app.files.marquee.band.take() {
        if band.active {
            app.files.marquee.suppress_click = true;
        }
        cx.notify();
    }
}

/// Scroll while the pointer is above or below the viewport.
fn edge_scroll(app: &FmApp) {
    let Some(band) = &app.files.marquee.band else { return };
    let state = match app.view_mode {
        ViewMode::List => app.tab().scroll.0.borrow(),
        ViewMode::Grid => app.files.grid_scroll.0.borrow(),
    };
    let handle = &state.base_handle;
    let (bounds, offset, max) = (handle.bounds(), handle.offset(), handle.max_offset());
    let y = band.current.y;
    let step = if y < bounds.top() {
        (f32::from(bounds.top() - y)).min(EDGE_SCROLL_MAX)
    } else if y > bounds.bottom() {
        -(f32::from(y - bounds.bottom())).min(EDGE_SCROLL_MAX)
    } else {
        return;
    };
    let new_y = (f32::from(offset.y) + step).clamp(-f32::from(max.height), 0.);
    handle.set_offset(point(offset.x, px(new_y)));
}

/// Rows (indices into `tab.visible`) whose box intersects the band.
fn hits(app: &FmApp, band: &Band) -> Vec<usize> {
    let (bounds, offset) = viewport(app);
    let now = to_content(band.current, bounds, offset);
    let (x0, x1) = (f32::from(band.start.x.min(now.x)), f32::from(band.start.x.max(now.x)));
    let (y0, y1) = (f32::from(band.start.y.min(now.y)), f32::from(band.start.y.max(now.y)));
    let count = app.tab().visible.len();
    if count == 0 {
        return Vec::new();
    }
    match app.view_mode {
        ViewMode::List => {
            let first = (y0.max(0.) / theme::ROW_H).floor() as usize;
            let last = (y1.max(0.) / theme::ROW_H).floor() as usize;
            if y1 < 0. || first >= count {
                return Vec::new();
            }
            (first..=last.min(count - 1)).collect()
        }
        ViewMode::Grid => {
            let columns = app.files.grid_columns.max(1);
            let width = f32::from(bounds.size.width);
            let tile_w = (width - 2. * GRID_PAD - (columns as f32 - 1.) * GRID_GAP) / columns as f32;
            let pitch = TILE_H + GRID_GAP;
            let rows = count.div_ceil(columns);
            let mut out = Vec::new();
            for grid_row in 0..rows {
                let top = GRID_PAD + grid_row as f32 * pitch;
                if top + TILE_H < y0 || top > y1 {
                    continue;
                }
                for col in 0..columns {
                    let left = GRID_PAD + col as f32 * (tile_w + GRID_GAP);
                    let ix = grid_row * columns + col;
                    if ix < count && left + tile_w >= x0 && left <= x1 {
                        out.push(ix);
                    }
                }
            }
            out
        }
    }
}

fn apply(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let Some(band) = app.files.marquee.band.as_ref() else { return };
    let rows = hits(app, band);
    let base = band.base.clone();
    let tab = app.tab_mut();
    let mut selected = base;
    selected.extend(rows.iter().filter_map(|r| tab.entry_at(*r).map(|e| e.path.clone())));
    let cursor_moved = rows.last().is_some_and(|r| *r != tab.cursor);
    if let Some(last) = rows.last() {
        tab.cursor = *last;
    }
    if selected != tab.selected || cursor_moved {
        tab.selected = selected;
        app.selection_changed(cx);
        cx.notify();
    }
}

/// Tracks the pointer for the whole window while a press is pending or a band is active
/// (rows and the area itself only see moves while hovered), and records the area bounds.
pub fn tracker(app: &FmApp, cx: &mut Context<FmApp>) -> impl IntoElement {
    let pending = app.files.marquee.band.is_some();
    let (area, this) = (app.files.marquee.area.clone(), cx.weak_entity());
    canvas(
        move |bounds, _, _| area.set(bounds),
        move |_, _, window, _| {
            if !pending {
                return;
            }
            let this_move = this.clone();
            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    this_move.update(cx, |app, cx| on_move(app, ev, cx)).ok();
                }
            });
            let this_up = this.clone();
            window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
                    this_up.update(cx, on_up).ok();
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// The band itself, positioned inside the file area.
pub fn overlay(app: &FmApp, _window: &Window, cx: &Context<FmApp>) -> Option<AnyElement> {
    let band = app.files.marquee.band.as_ref().filter(|b| b.active)?;
    let t = theme(cx);
    let (bounds, offset) = viewport(app);
    let area = app.files.marquee.area.get();
    // Start back in window coordinates with the current scroll.
    let start = point(band.start.x + bounds.left() + offset.x, band.start.y + bounds.top() + offset.y);
    let (a, b) = (start, band.current);
    let left = a.x.min(b.x) - area.left();
    let top = a.y.min(b.y) - area.top();
    let w = (a.x - b.x).abs();
    let h = (a.y - b.y).abs();
    Some(
        div()
            .absolute()
            .left(left)
            .top(top)
            .w(w)
            .h(h)
            .bg(alpha(t.accent, 0.12))
            .border_1()
            .border_color(alpha(t.accent, 0.7))
            .into_any_element(),
    )
}
