//! Sidebar 230 (SPEC §4.1): places, devices, bookmarks, tags. Collapses to a 44px icon
//! rail below `BREAK_SIDEBAR` with hover flyout labels.
//!
//! OWNER: sidebar agent. STUB — renders an empty column.

use gpui::{AnyElement, Context, Window, div, prelude::*, px};

use crate::{
    app::FmApp,
    theme::{self, theme},
};

pub struct SidebarState {}

impl SidebarState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        SidebarState {}
    }
}

/// Called by the shell after the active tab's directory has (re)loaded.
pub fn on_directory_loaded(_app: &mut FmApp, _cx: &mut Context<FmApp>) {}

/// Footer text for the device holding the cwd, e.g. `nvme0n1p2 64G free`.
pub fn free_space_label(_app: &FmApp) -> Option<String> {
    None
}

pub fn render(_app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx);
    let rail = window.viewport_size().width < px(theme::BREAK_SIDEBAR);
    div()
        .w(px(if rail { theme::SIDEBAR_RAIL_W } else { theme::SIDEBAR_W }))
        .flex_none()
        .h_full()
        .p(px(6.))
        .bg(t.surface)
        .border_r_1()
        .border_color(t.line)
        .into_any_element()
}
