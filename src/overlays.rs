//! Everything that floats above the window except Quick Look (SPEC §6, §7): context menus,
//! jobs panel, conflict dialog, delete confirmation, toasts, the `?` key-hint grid.
//!
//! OWNER: ops agent. STUB — nothing renders.

use std::path::PathBuf;

use gpui::{AnyElement, Context, Pixels, Point, Window};

use crate::{actions::KeyHints, app::FmApp};

/// What was right-clicked.
#[derive(Clone, Debug)]
pub enum MenuTarget {
    /// A single file or folder.
    Entry(PathBuf),
    /// Empty space in the file area.
    Empty,
    /// A row that is part of a multi-selection.
    Selection,
}

pub struct OverlaysState {}

impl OverlaysState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        OverlaysState {}
    }
}

/// Open a context menu at a window position (called by file_view.rs on right-click).
pub fn open_context_menu(
    _app: &mut FmApp,
    _target: MenuTarget,
    _position: Point<Pixels>,
    _window: &mut Window,
    _cx: &mut Context<FmApp>,
) {
}

/// `?` — toggle the which-key grid.
pub fn act_key_hints(_app: &mut FmApp, _: &KeyHints, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `esc` cascade: close the topmost overlay. Return true if one was open.
pub fn cancel(_app: &mut FmApp, _window: &mut Window, _cx: &mut Context<FmApp>) -> bool {
    false
}

/// Zero or more absolutely-positioned layers, bottom to top.
pub fn render(_app: &mut FmApp, _window: &mut Window, _cx: &mut Context<FmApp>) -> Vec<AnyElement> {
    Vec::new()
}
