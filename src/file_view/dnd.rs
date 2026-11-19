//! Drag & drop (SPEC §6): the ghost chip, drop targets, spring-open.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, DragMoveEvent, ExternalDragIcon, FontWeight, Hsla, Modifiers, Pixels,
    Point, Rgba, SharedString, Window, div, prelude::*, px,
};
use resvg::{tiny_skia, usvg};

use crate::{
    app::FmApp,
    assets::Assets,
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
    /// The dragged row's image, if it gets a thumbnail (grid rules: an image of sane size).
    pub image: Option<PathBuf>,
    /// That thumbnail, decoded in the background once the drag starts (see [`load_thumbnail`]);
    /// `None` inside means decoding failed.
    pub thumb: Arc<OnceLock<Option<Thumb>>>,
}

/// A square, cover-cropped thumbnail: `side`×`side` premultiplied RGBA pixels.
pub struct Thumb {
    side: u32,
    rgba: Vec<u8>,
}

/// Side of the drag tile (logical px): a type icon, or the image thumbnail.
const ICON_TILE: f32 = 40.;
const THUMB_TILE: f32 = 64.;

/// Start decoding the dragged image's thumbnail, sized for `scale_factor`. It is only needed
/// once the drag leaves the window, which gives a slow decode a head start; if it is still not
/// ready then, the type icon is used.
pub fn load_thumbnail(payload: &DragPayload, scale_factor: f32, cx: &App) {
    let Some(path) = payload.image.clone() else { return };
    if payload.thumb.get().is_some() {
        return;
    }
    let slot = payload.thumb.clone();
    let side = (THUMB_TILE * scale_factor.ceil().max(1.)) as u32;
    cx.background_executor()
        .spawn(async move {
            let _ = slot.set(decode_thumbnail(&path, side));
        })
        .detach();
}

fn decode_thumbnail(path: &Path, side: u32) -> Option<Thumb> {
    let image = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    // Cheap downscale first so the filtered crop-resize below works on a small image.
    let image = image.thumbnail(side * 2, side * 2).resize_to_fill(side, side, image::imageops::FilterType::Triangle);
    let mut rgba = image.into_rgba8().into_raw();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u16;
        for c in &mut px[..3] {
            *c = ((*c as u16 * a + 127) / 255) as u8;
        }
    }
    Some(Thumb { side, rgba })
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
    let payload = ev.drag(cx).clone();
    let icon = external_drag_icon(&payload, theme(cx), window.scale_factor());
    if !window.start_external_drag(&payload.paths, icon) {
        return; // not on Wayland, or no data device: the drag simply stays in the window
    }
    // From here the compositor owns the gesture; the button release goes to the drop target.
    cx.stop_active_drag(window);
    app.files.drop.target = None;
    app.files.drop.generation += 1;
    app.files.marquee.press_on_handle = false;
    cx.notify();
}

/// The picture the compositor shows under the pointer once a drag has left the window: the
/// dragged file's type icon (or image thumbnail) on a small tile; for several files a second
/// tile peeks out behind it and an accent badge carries the count. Drawn here because the
/// window no longer paints anything for this drag.
fn external_drag_icon(payload: &DragPayload, t: &Theme, scale_factor: f32) -> Option<ExternalDragIcon> {
    const GAP: f32 = 6.; // between the pointer and the tile
    const GLYPH: f32 = 22.;
    const STACK: f32 = 5.; // offset of the tile behind, for several files

    let count = payload.paths.len();
    let many = count > 1;
    let scale = scale_factor.ceil().max(1.) as u32;
    let s = scale as f32;
    let thumb = payload.thumb.get().and_then(Option::as_ref);
    let thumb = thumb.and_then(|thumb| {
        let pixmap = tiny_skia::IntSize::from_wh(thumb.side, thumb.side)
            .and_then(|size| tiny_skia::Pixmap::from_vec(thumb.rgba.clone(), size))?;
        Some(pixmap)
    });
    let tile_size = if thumb.is_some() { THUMB_TILE } else { ICON_TILE };
    let tile_px = tile_size * s;
    let badge = many.then(|| CountBadge::new(count));
    // The badge sits on the top-right corner of the stack, half outside it.
    let stack_right = GAP + tile_size + if many { STACK } else { 0. };
    let badge_x = badge.as_ref().map(|b| stack_right + b.overhang - b.width);
    let width = badge_x.zip(badge.as_ref()).map_or(stack_right, |(x, b)| (x + b.width).max(stack_right)) + 1.;
    let height = GAP + tile_size + if many { STACK } else { 0. } + 1.;
    // Whole logical pixels, so the buffer is a multiple of its scale.
    let (width, height) = (width.ceil() as u32 * scale, height.ceil() as u32 * scale);
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;

    let (fill, border) = (solid(t.surface)?, solid(t.line)?);
    let mut tile = |x: f32, y: f32, picture: Option<&tiny_skia::Pixmap>| -> Option<()> {
        let rect = tiny_skia::Rect::from_xywh(x * s, y * s, tile_px, tile_px)?;
        pixmap.fill_rect(rect, &fill, tiny_skia::Transform::identity(), None);
        if let Some(picture) = picture {
            let k = tile_px / picture.width() as f32;
            let transform = tiny_skia::Transform::from_scale(k, k).post_translate(x * s, y * s);
            let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
            pixmap.draw_pixmap(0, 0, picture.as_ref(), &paint, transform, None);
        }
        let outline = tiny_skia::PathBuilder::from_rect(rect.inset(s / 2., s / 2.)?);
        let stroke = tiny_skia::Stroke { width: s, ..Default::default() };
        pixmap.stroke_path(&outline, &border, &stroke, tiny_skia::Transform::identity(), None);
        Some(())
    };
    let (x, y) = (GAP, GAP + if many { STACK } else { 0. });
    if many {
        tile(GAP + STACK, GAP, None)?;
    }
    tile(x, y, thumb.as_ref())?;

    if thumb.is_none() {
        // The icon SVGs are black strokes that gpui tints; recolour them by hand here.
        let svg = Assets::get(payload.icon.path())?;
        let svg = String::from_utf8_lossy(&svg.data).replace("stroke=\"#000\"", &format!("stroke=\"{}\"", hex(t.fg)));
        let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
        let k = GLYPH * s / tree.size().width().max(tree.size().height());
        let inset = (tile_size - GLYPH) / 2.;
        let transform = tiny_skia::Transform::from_scale(k, k).post_translate((x + inset) * s, (y + inset) * s);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
    }
    if let (Some(badge), Some(bx)) = (badge, badge_x) {
        badge.draw(&mut pixmap, bx, 0., s, t)?;
    }
    Some(to_drag_icon(pixmap, scale))
}

fn solid(color: Hsla) -> Option<tiny_skia::Paint<'static>> {
    let c = Rgba::from(color);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(tiny_skia::Color::from_rgba(c.r, c.g, c.b, c.a)?);
    paint.anti_alias = true;
    Some(paint)
}

fn hex(color: Hsla) -> String {
    let c = Rgba::from(color);
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

/// The number of dragged files on an accent chip (like the in-window ghost's MOVE badge).
/// resvg is built without text support, so the digits are drawn as stroked paths.
struct CountBadge {
    text: String,
    /// Logical size.
    width: f32,
    /// How far the badge reaches past the stack's right edge.
    overhang: f32,
}

impl CountBadge {
    const HEIGHT: f32 = 16.;
    const PAD: f32 = 5.;
    /// Glyph box in logical px (the paths below use a 6×10 grid).
    const GLYPH_H: f32 = 8.;
    const GLYPH_W: f32 = 6. * Self::GLYPH_H / 10.;
    const TRACK: f32 = 1.6;

    fn new(count: usize) -> Self {
        let text = if count > 999 { "999+".to_string() } else { count.to_string() };
        let n = text.chars().count() as f32;
        let width = (Self::PAD * 2. + n * Self::GLYPH_W + (n - 1.) * Self::TRACK).max(Self::HEIGHT);
        CountBadge { text, width, overhang: 6. }
    }

    fn draw(&self, pixmap: &mut tiny_skia::Pixmap, x: f32, y: f32, s: f32, t: &Theme) -> Option<()> {
        let rect = tiny_skia::Rect::from_xywh(x * s, y * s, self.width * s, Self::HEIGHT * s)?;
        pixmap.fill_rect(rect, &solid(t.accent)?, tiny_skia::Transform::identity(), None);

        let k = Self::GLYPH_H / 10.;
        let n = self.text.chars().count() as f32;
        let run = n * Self::GLYPH_W + (n - 1.) * Self::TRACK;
        let (mut gx, gy) = (x + (self.width - run) / 2., y + (Self::HEIGHT - Self::GLYPH_H) / 2.);
        let mut paths = String::new();
        for ch in self.text.chars() {
            paths.push_str(&format!(r#"<path transform="translate({gx} {gy}) scale({k})" d="{}"/>"#, digit_path(ch)));
            gx += Self::GLYPH_W + Self::TRACK;
        }
        let (w, h) = (pixmap.width() as f32 / s, pixmap.height() as f32 / s);
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}"><g fill="none" stroke="{}" stroke-width="{}" stroke-linecap="round" stroke-linejoin="round">{paths}</g></svg>"#,
            hex(t.on_accent),
            1.5 / k,
        );
        let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
        resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pixmap.as_mut());
        Some(())
    }
}

/// A stroke drawing of `ch` (a digit or `+`) on a 6×10 grid, y down.
fn digit_path(ch: char) -> &'static str {
    match ch {
        '0' => "M3 0Q0 0 0 5Q0 10 3 10Q6 10 6 5Q6 0 3 0Z",
        '1' => "M1 2L3.5 0V10M1 10H6",
        '2' => "M0.2 2.4Q0.8 0 3 0Q6 0 6 2.8Q6 4.6 0 10H6",
        '3' => "M0.4 0H5.6L2.6 4Q6 4 6 7Q6 10 3 10Q0.8 10 0 8.4",
        '4' => "M4.4 10V0L0 7H6",
        '5' => "M5.6 0H0.8L0.4 4.6Q1.4 4 3 4Q6 4 6 7Q6 10 3 10Q0.8 10 0 8.4",
        '6' => "M5.2 0.6Q4.4 0 3 0Q0 0 0 6Q0 10 3 10Q6 10 6 7Q6 4.4 3 4.4Q0.6 4.4 0 6.4",
        '7' => "M0 0H6L2 10",
        '8' => "M3 4.4Q0.6 4.4 0.6 2.2Q0.6 0 3 0Q5.4 0 5.4 2.2Q5.4 4.4 3 4.4Q0 4.4 0 7.2Q0 10 3 10Q6 10 6 7.2Q6 4.4 3 4.4Z",
        '9' => "M0.8 9.4Q1.6 10 3 10Q6 10 6 4Q6 0 3 0Q0 0 0 3Q0 5.6 3 5.6Q5.4 5.6 6 3.6",
        _ => "M3 2V8M0 5H6",
    }
}

fn to_drag_icon(pixmap: tiny_skia::Pixmap, scale: u32) -> ExternalDragIcon {
    let (width, height) = (pixmap.width(), pixmap.height());
    // tiny-skia is premultiplied RGBA; wl_shm ARGB8888 is premultiplied BGRA in memory.
    let mut bgra = pixmap.take();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    ExternalDragIcon { width, height, scale, bgra }
}

/// The payload was released over the folder `dest`.
pub fn drop_into(app: &mut FmApp, payload: &DragPayload, dest: PathBuf, window: &mut Window, cx: &mut Context<FmApp>) {
    app.files.drop.target = None;
    app.files.drop.generation += 1;
    cx.notify();
    if crate::fs_model::tag_of(&dest).is_some() || !can_drop(&payload.paths, &dest) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;

    fn payload(n: usize, image: bool) -> DragPayload {
        let paths = (0..n).map(|i| PathBuf::from(format!("/tmp/f{i}.txt"))).collect();
        let thumb = Arc::new(OnceLock::new());
        if image {
            let _ = thumb.set(Some(Thumb { side: 64, rgba: vec![255; 64 * 64 * 4] }));
        }
        DragPayload { paths: Arc::new(paths), icon: Icon::File, image: None, thumb }
    }

    #[test]
    fn drag_icon_for_one_and_many() {
        let t = Theme::get(ThemeName::Ash);
        for (n, image) in [(1, false), (1, true), (3, false), (3, true), (12, false), (1500, true)] {
            for scale in [1., 1.25, 2.] {
                let icon = external_drag_icon(&payload(n, image), &t, scale);
                let icon = icon.unwrap_or_else(|| panic!("no icon for {n} files, image {image}, scale {scale}"));
                assert_eq!(icon.width % icon.scale, 0);
                assert_eq!(icon.height % icon.scale, 0);
                assert_eq!(icon.bgra.len(), (icon.width * icon.height * 4) as usize);
            }
        }
    }
}
