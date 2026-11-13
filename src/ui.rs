//! Small shared building blocks. Everything reads the theme global; nothing owns state.

use gpui::{App, Div, ElementId, FontWeight, Hsla, SharedString, Stateful, div, prelude::*, px};

use crate::{
    icons::{Icon, icon},
    theme::theme,
};

/// Label style (SPEC §1.4): mono 10.5, uppercase, `fg-muted`.
/// (GPUI has no letter-spacing; the +0.08em tracking is dropped.)
pub fn label(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    let text: SharedString = text.into();
    div()
        .font_family(t.font_mono.clone())
        .text_size(px(10.5))
        .text_color(t.fg_muted)
        .flex_none()
        .child(SharedString::from(text.to_uppercase()))
}

/// Mono text at the row size (12).
pub fn mono(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div().font_family(t.font_mono.clone()).text_size(px(12.)).child(text.into())
}

/// Keycap (SPEC §4.6): 20 high, mono 10.5, 1px line border with a 2px bottom border.
pub fn keycap(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .h(px(20.))
        .min_w(px(20.))
        .px(px(5.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .border_1()
        .border_b_2()
        .border_color(t.line)
        .rounded(t.radius_control)
        .font_family(t.font_mono.clone())
        .text_size(px(10.5))
        .text_color(t.fg_muted)
        .child(text.into())
}

/// 30×30 ghost icon button. Add `.on_click(..)` yourself.
pub fn icon_button(id: impl Into<ElementId>, ic: Icon, cx: &App) -> Stateful<Div> {
    icon_button_sized(id, ic, 30., 15., cx)
}

pub fn icon_button_sized(id: impl Into<ElementId>, ic: Icon, box_size: f32, icon_size: f32, cx: &App) -> Stateful<Div> {
    let t = theme(cx);
    let (hover_bg, fg) = (t.surface_2, t.fg);
    div()
        .id(id)
        .size(px(box_size))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(t.radius_control)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .child(icon(ic, icon_size).text_color(fg))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Accent fill, `on-accent` text.
    Primary,
    /// 1px line border.
    Secondary,
    /// No border.
    Ghost,
}

/// Text button, 30 high, sans 12.5/500. Add `.on_click(..)` yourself.
pub fn button(id: impl Into<ElementId>, text: impl Into<SharedString>, kind: ButtonKind, cx: &App) -> Stateful<Div> {
    let t = theme(cx);
    let base = div()
        .id(id)
        .h(px(30.))
        .px(px(12.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .rounded(t.radius_control)
        .font_family(t.font_sans.clone())
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(text.into());
    let (accent, on_accent, line, fg, s2) = (t.accent, t.on_accent, t.line, t.fg, t.surface_2);
    match kind {
        ButtonKind::Primary => base.bg(accent).text_color(on_accent).hover(|s| s.opacity(0.9)),
        ButtonKind::Secondary => base.border_1().border_color(line).text_color(fg).hover(move |s| s.bg(s2)),
        ButtonKind::Ghost => base.text_color(fg).hover(move |s| s.bg(s2)),
    }
}

/// 1px horizontal divider.
pub fn hline(cx: &App) -> Div {
    div().h(px(1.)).w_full().flex_none().bg(theme(cx).line)
}

/// `color` at `alpha` opacity.
pub fn alpha(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}
