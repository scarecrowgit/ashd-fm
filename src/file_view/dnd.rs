//! Drag & drop (SPEC §6): the ghost chip, drop targets, spring-open.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, DragMoveEvent, FontWeight, Modifiers, Pixels, Point, SharedString,
    Window, div, prelude::*, px,
};

use crate::{
    app::FmApp,
    icons::{Icon, icon},
    ops::{self, TransferKind},
    theme::{Theme, theme},
    ui::alpha,
};

/// A drag hovering a folder this long navigates into it.
const SPRING_OPEN_MS: u64 = 600;

/// What is being dragged: the multi-selection, or the single dragged row.
#[derive(Clone)]
pub struct DragPayload {
    pub paths: Arc<Vec<PathBuf>>,
    pub icon: Icon,
}

#[derive(Default)]
pub struct DropState {
    /// Folder under the drag (drawn with the dashed outline).
    pub target: Option<PathBuf>,
    /// Bumped whenever `target` changes; identifies the spring-open timer and its animation.
    pub generation: usize,
}

/// ctrl → copy, alt → link, otherwise move.
pub fn kind_for(modifiers: Modifiers) -> TransferKind {
    if modifiers.control {
        TransferKind::Copy
    } else if modifiers.alt {
        TransferKind::Link
    } else {
        TransferKind::Move
    }
}

/// A path can never be dropped onto itself or into its own descendant.
pub fn can_drop(sources: &[PathBuf], dest: &Path) -> bool {
    !sources.is_empty() && sources.iter().all(|src| !dest.starts_with(src))
}

/// The chip that follows the pointer.
pub struct DragGhost {
    label: SharedString,
    icon: Icon,
    badge: &'static str,
    /// Pointer position inside the dragged row; the chip is drawn next to the pointer.
    offset: Point<Pixels>,
}

impl DragGhost {
    pub fn new(payload: &DragPayload, offset: Point<Pixels>, modifiers: Modifiers) -> Self {
        let label = match payload.paths.as_slice() {
            [one] => one.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| one.display().to_string()),
            many => format!("{} files", many.len()),
        };
        let badge = match kind_for(modifiers) {
            TransferKind::Copy => "COPY",
            TransferKind::Link => "LINK",
            TransferKind::Move => "MOVE",
        };
        DragGhost { label: label.into(), icon: payload.icon, badge, offset }
    }
}

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        div().pl(self.offset.x + px(14.)).pt(self.offset.y + px(8.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(6.))
                .px(px(10.))
                .bg(t.surface)
                .border_1()
                .border_color(t.line)
                .shadow(t.shadow_overlay())
                .font_family(t.font_mono.clone())
                .child(icon(self.icon, 14.).text_color(t.fg))
                .child(div().max_w(px(240.)).truncate().text_size(px(11.5)).text_color(t.fg).child(self.label.clone()))
                .child(
                    div()
                        .h(px(18.))
                        .px(px(6.))
                        .flex()
                        .items_center()
                        .bg(t.accent)
                        .text_color(t.on_accent)
                        .text_size(px(10.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(self.badge),
                )
                .child(div().text_size(px(9.5)).text_color(t.fg_muted).child("CTRL COPY · ALT LINK")),
        )
    }
}

/// A drag moved; `inside` says whether the pointer is over the folder `path`.
pub fn hover(app: &mut FmApp, path: &Path, inside: bool, droppable: bool, cx: &mut Context<FmApp>) {
    let is_target = app.files.drop.target.as_deref() == Some(path);
    if inside && droppable {
        if is_target {
            return;
        }
        app.files.drop.target = Some(path.to_path_buf());
        app.files.drop.generation += 1;
        let (generation, path) = (app.files.drop.generation, path.to_path_buf());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(SPRING_OPEN_MS)).await;
            this.update(cx, |app, cx| {
                let still_hovering =
                    app.files.drop.generation == generation && app.files.drop.target.as_deref() == Some(path.as_path());
                if still_hovering && cx.has_active_drag() {
                    app.files.drop.target = None;
                    app.navigate(path, cx);
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    } else if is_target {
        app.files.drop.target = None;
        app.files.drop.generation += 1;
        cx.notify();
    }
}

/// A drag that reaches the window edge leaves the app: it is handed to the compositor as a
/// file drag (`text/uri-list`), so it can be dropped into a browser, chat, editor, terminal….
/// The edge itself counts, for windows that sit against a screen edge.
pub fn drag_move_window(app: &mut FmApp, ev: &DragMoveEvent<DragPayload>, window: &mut Window, cx: &mut Context<FmApp>) {
    let pos = ev.event.position;
    let size = window.viewport_size();
    let at_edge =
        pos.x <= px(0.) || pos.y <= px(0.) || pos.x >= size.width - px(1.) || pos.y >= size.height - px(1.);
    if !at_edge {
        return;
    }
    let paths = ev.drag(cx).paths.clone();
    if !window.start_external_drag(&paths) {
        return; // not on Wayland, or no data device: the drag simply stays in the window
    }
    // From here the compositor owns the gesture; the button release goes to the drop target.
    cx.stop_active_drag(window);
    app.files.drop.target = None;
    app.files.drop.generation += 1;
    app.files.marquee.press_on_handle = false;
    cx.notify();
}

/// The payload was released over the folder `dest`.
pub fn drop_into(app: &mut FmApp, payload: &DragPayload, dest: PathBuf, window: &mut Window, cx: &mut Context<FmApp>) {
    app.files.drop.target = None;
    app.files.drop.generation += 1;
    cx.notify();
    if !can_drop(&payload.paths, &dest) {
        return;
    }
    // Things that already live in `dest` stay where they are.
    let sources: Vec<PathBuf> = payload.paths.iter().filter(|src| src.parent() != Some(dest.as_path())).cloned().collect();
    if sources.is_empty() {
        return;
    }
    ops::start_transfer(app, sources, dest, kind_for(window.modifiers()), cx);
}

/// Outline of the drop target: 1px dashed accent, plus the 18% accent tint that swells in
/// just before the folder springs open. `inset` is the parent's border width on each side
/// (left, top/right/bottom) so the outline sits on the parent's outer edge.
pub fn outline(t: &Theme, generation: usize, left_inset: f32, inset: f32) -> AnyElement {
    let accent = t.accent;
    div()
        .absolute()
        .left(px(-left_inset))
        .top(px(-inset))
        .right(px(-inset))
        .bottom(px(-inset))
        .border_1()
        .border_dashed()
        .border_color(accent)
        .with_animation(
            ("spring-open", generation),
            Animation::new(Duration::from_millis(SPRING_OPEN_MS)),
            move |el, delta| {
                let swell = ((delta - 0.5) / 0.4).clamp(0., 1.);
                el.bg(alpha(accent, 0.18 * swell * swell * (3. - 2. * swell)))
            },
        )
        .into_any_element()
}
