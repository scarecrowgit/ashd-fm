//! Quick Look (SPEC §5): scrim + panel over the whole window, body by file type, side
//! prev/next, footer, full-screen toggle. Content comes from `app.preview.data`.
//!
//! OWNER: quicklook agent. STUB — never opens.

use gpui::{AnyElement, Context, Window};

use crate::{
    actions::{ToggleFullscreen, ToggleQuickLook},
    app::FmApp,
};

pub struct QuickLookState {
    pub open: bool,
    pub fullscreen: bool,
}

impl QuickLookState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        QuickLookState { open: false, fullscreen: false }
    }

    /// The shell adds the `QuickLook` key context and routes ←/→ to the cursor while this is true.
    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// `space` / eye button.
pub fn act_toggle(_app: &mut FmApp, _: &ToggleQuickLook, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `f` while open.
pub fn act_fullscreen(_app: &mut FmApp, _: &ToggleFullscreen, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `↵` while open: run the primary action and close. Return false when Quick Look is closed
/// (the shell then does the normal open).
pub fn open_primary(_app: &mut FmApp, _cx: &mut Context<FmApp>) -> bool {
    false
}

/// `esc` cascade. Return true if Quick Look was open and is now closing.
pub fn cancel(_app: &mut FmApp, _window: &mut Window, _cx: &mut Context<FmApp>) -> bool {
    false
}

/// The cursor moved: the panel keeps its size, only the body swaps (no re-enter animation).
pub fn on_cursor_changed(_app: &mut FmApp, _cx: &mut Context<FmApp>) {}

/// `None` when closed; otherwise an absolutely-positioned layer covering the window.
pub fn render(_app: &mut FmApp, _window: &mut Window, _cx: &mut Context<FmApp>) -> Option<AnyElement> {
    None
}
