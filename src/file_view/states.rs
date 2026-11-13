//! The states of SPEC §6 that replace the list / grid body: loading skeleton, empty folder,
//! no filter matches, permission denied, folder / device unavailable.

use std::{path::Path, time::Duration};

use gpui::{
    Animation, AnimationElement, AnimationExt, AnyElement, Context, ElementId, FontWeight, SharedString,
    Styled, div, prelude::*, px,
};

use crate::{
    actions,
    app::FmApp,
    fs_model::{self, DirError},
    icons::{Icon, icon},
    theme::{self, cubic_bezier, theme},
    ui::{ButtonKind, button, keycap},
};

/// Bar widths of the skeleton rows (prototype/states.html), repeated.
const SKELETON_WIDTHS: [f32; 7] = [140., 90., 180., 120., 160., 100., 130.];
const SKELETON_ROWS: usize = 14;

/// Skeleton pulse (SPEC §6/§9): opacity .35 ↔ .8 over 1.4 s ease-in-out, `index` × 80 ms stagger.
pub fn pulse<E>(el: E, id: impl Into<ElementId>, index: usize) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    let phase = index as f32 * 0.08 / 1.4;
    el.with_animation(id, Animation::new(Duration::from_millis(1400)).repeat(), move |el, delta| {
        let p = (delta - phase).rem_euclid(1.);
        let tri = if p < 0.5 { p * 2. } else { 2. - p * 2. };
        el.opacity(0.35 + 0.45 * cubic_bezier(0.42, 0., 0.58, 1., tri))
    })
}

/// Loading: rows of a 13px square + a bar, aligned with the list's icon and name columns.
pub fn skeleton(cx: &mut Context<FmApp>) -> AnyElement {
    let line = theme(cx).line;
    div()
        .size_full()
        .overflow_hidden()
        .children((0..SKELETON_ROWS).map(|ix| {
            let row = div()
                .h(px(theme::ROW_H))
                .pl(px(16.))
                .flex()
                .items_center()
                .gap(px(12.))
                .child(div().w(px(22.)).flex_none().child(div().size(px(13.)).bg(line)))
                .child(div().h(px(7.)).w(px(SKELETON_WIDTHS[ix % SKELETON_WIDTHS.len()])).bg(line));
            pulse(row, ("skeleton", ix), ix)
        }))
        .into_any_element()
}

/// Centred icon · title · body · buttons block shared by every empty / error state.
fn block(ic: Icon, title: impl Into<SharedString>, body: AnyElement, buttons: Vec<AnyElement>, cx: &Context<FmApp>) -> AnyElement {
    let t = theme(cx);
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.))
        .p(px(24.))
        .overflow_hidden()
        .child(icon(ic, 22.).text_color(t.fg_muted))
        .child(
            div()
                .max_w(px(440.))
                .font_family(t.font_sans.clone())
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.fg)
                .text_center()
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(280.))
                .font_family(t.font_sans.clone())
                .text_size(px(12.5))
                .line_height(px(19.))
                .text_color(t.fg_muted)
                .text_center()
                .child(body),
        )
        .child(div().flex().gap(px(6.)).children(buttons))
        .into_any_element()
}

fn text(s: impl Into<SharedString>) -> AnyElement {
    div().child(s.into()).into_any_element()
}

/// Ready, nothing to show and no filter.
pub fn empty(app: &FmApp, cx: &mut Context<FmApp>) -> AnyElement {
    let hidden = app.tab().entries.len();
    let body = if hidden > 0 {
        let noun = if hidden == 1 { "item" } else { "items" };
        format!("{hidden} hidden {noun} here. Press . to show hidden files.")
    } else {
        "Drop files here or create a new one.".to_string()
    };
    let buttons = vec![
        button("state-new-file", "New file", ButtonKind::Secondary, cx)
            .on_click(cx.listener(|this, _, window, cx| {
                this.focus_list(window);
                window.dispatch_action(Box::new(actions::NewFile), cx);
            }))
            .into_any_element(),
        button("state-new-folder", "New folder", ButtonKind::Ghost, cx)
            .on_click(cx.listener(|this, _, window, cx| {
                this.focus_list(window);
                window.dispatch_action(Box::new(actions::NewFolder), cx);
            }))
            .into_any_element(),
    ];
    block(Icon::Folder, "Nothing here", text(body), buttons, cx)
}

/// Ready, entries exist but the filter matches none of them.
pub fn no_matches(app: &FmApp, cx: &mut Context<FmApp>) -> AnyElement {
    let title = format!("No matches for “{}”", app.tab().filter);
    let body = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.))
        .child("The filter searches this folder.")
        .child(div().flex().items_center().gap(px(6.)).child("Recursive search:").child(keycap("ctrl+/", cx)))
        .into_any_element();
    let buttons = vec![
        button("state-search-recursive", "Search recursively", ButtonKind::Secondary, cx)
            .on_click(cx.listener(|this, _, window, cx| {
                this.focus_list(window);
                window.dispatch_action(Box::new(actions::RecursiveSearch), cx);
            }))
            .into_any_element(),
    ];
    block(Icon::Search, title, body, buttons, cx)
}

fn back_button(kind: ButtonKind, cx: &mut Context<FmApp>) -> AnyElement {
    button("state-back", "Back", kind, cx)
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus_list(window);
            if this.can_go_back() { this.go_back(cx) } else { this.go_parent(cx) }
        }))
        .into_any_element()
}

/// Name of the removable volume a path lives on (`/run/media/<user>/<NAME>/…`, `/media/<NAME>/…`).
fn device_name(path: &Path) -> Option<String> {
    let (rest, skip) = if let Ok(rest) = path.strip_prefix("/run/media") {
        (rest, 1)
    } else if let Ok(rest) = path.strip_prefix("/media") {
        (rest, 0)
    } else {
        return None;
    };
    let mut parts = rest.components().map(|c| c.as_os_str().to_string_lossy().to_string());
    let first = parts.next()?;
    if skip == 0 { Some(first) } else { Some(parts.next().unwrap_or(first)) }
}

/// The directory could not be read.
pub fn error(app: &FmApp, err: &DirError, cx: &mut Context<FmApp>) -> AnyElement {
    let path = app.cwd().to_path_buf();
    if *err == DirError::PermissionDenied {
        let title = format!("No access to {}", fs_model::display_path(&path));
        // Elevation (polkit) is out of scope: the primary action is shown, but disabled.
        let buttons = vec![
            button("state-open-root", "Open as root", ButtonKind::Primary, cx)
                .opacity(0.4)
                .cursor_default()
                .into_any_element(),
            back_button(ButtonKind::Secondary, cx),
        ];
        return block(Icon::Lock, title, text("This folder belongs to another user. Open as administrator?"), buttons, cx);
    }

    let retry = button("state-retry", "Retry", ButtonKind::Secondary, cx)
        .on_click(cx.listener(|this, _, window, cx| {
            this.focus_list(window);
            this.reload(cx);
        }))
        .into_any_element();
    let buttons = vec![retry, back_button(ButtonKind::Ghost, cx)];

    if let Some(device) = device_name(&path) {
        return block(
            Icon::Disk,
            format!("{device} disconnected"),
            text("The drive was removed mid-read. The pending operation was saved."),
            buttons,
            cx,
        );
    }
    let body = match err {
        DirError::Other(message) => message.clone(),
        _ => "This folder was moved, deleted or is not reachable right now.".to_string(),
    };
    block(Icon::Folder, format!("{} is unavailable", fs_model::short_name(&path)), text(body), buttons, cx)
}
