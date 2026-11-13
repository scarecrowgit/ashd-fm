//! Toolbar 48 (SPEC §4.2): back/forward, path bar (crumbs ⇄ edit + autocomplete), filter,
//! view toggle, Quick Look button, sort/hidden menu.
//!
//! OWNER: toolbar agent. STUB — renders the crumb text only.

use gpui::{AnyElement, Context, Window, div, prelude::*, px};

use crate::{
    actions::{EditPath, Filter, RecursiveSearch, SortMenu},
    app::{FmApp, cwd_label},
    theme::{self, theme},
    ui::mono,
};

pub struct ToolbarState {}

impl ToolbarState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        ToolbarState {}
    }
}

/// The active tab now shows another directory (navigation, tab switch) or its filter was
/// cleared by the shell: sync the path field and the filter field from `app.tab()`.
pub fn on_directory_changed(_app: &mut FmApp, _cx: &mut Context<FmApp>) {}

/// `esc` cascade: close path edit / autocomplete / sort menu, or blur the filter.
/// Return true if something was closed.
pub fn cancel(_app: &mut FmApp, _window: &mut Window, _cx: &mut Context<FmApp>) -> bool {
    false
}

/// `/` — focus the filter field.
pub fn act_filter(_app: &mut FmApp, _: &Filter, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `ctrl+/` — recursive search.
pub fn act_recursive_search(_app: &mut FmApp, _: &RecursiveSearch, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `ctrl+l` — path bar edit mode.
pub fn act_edit_path(_app: &mut FmApp, _: &EditPath, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `s` — open the sort / hidden menu.
pub fn act_sort_menu(_app: &mut FmApp, _: &SortMenu, _window: &mut Window, _cx: &mut Context<FmApp>) {}

pub fn render(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx);
    div()
        .h(px(theme::TOOLBAR_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .border_b_1()
        .border_color(t.line)
        .child(mono(cwd_label(app), cx))
        .into_any_element()
}
