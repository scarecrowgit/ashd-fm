//! Toolbar 48 (SPEC §4.2): back/forward, path bar (crumbs ⇄ edit + autocomplete), filter
//! (+ recursive search), view toggle, Quick Look button, sort/hidden menu.
//!
//! OWNER: toolbar agent.
//!
//! - `toolbar/path.rs` — path bar: crumbs, edit mode, autocomplete, invalid-path message.
//! - `toolbar/search.rs` — filter field and its recursive-search mode.
//! - `toolbar/sort.rs` — sort / hidden menu.
//!
//! Popups (autocomplete, search results, sort menu) are absolutely positioned children of
//! their anchor wrapped in `deferred(..)`, so they paint above the file list, and `.occlude()`
//! so the list underneath does not see the mouse.

mod path;
mod search;
mod sort;

use gpui::{AnyElement, Context, Focusable, MouseButton, Stateful, Subscription, Window, div, prelude::*, px};

use crate::{
    actions::{self, EditPath, Filter, RecursiveSearch, SortMenu},
    app::{FmApp, ViewMode},
    icons::{Icon, icon},
    text_input::TextInput,
    theme::{self, theme},
    ui::{alpha, keycap},
};

pub struct ToolbarState {
    path: path::PathBarState,
    filter: search::FilterState,
    sort: sort::SortMenuState,
    _subscriptions: Vec<Subscription>,
}

impl ToolbarState {
    pub fn new(window: &mut Window, cx: &mut Context<FmApp>) -> Self {
        let path_input = cx.new(|cx| TextInput::new("", cx));
        let filter_input = cx.new(|cx| TextInput::new(search::FILTER_PLACEHOLDER, cx));
        let sort_focus = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&path_input, window, path::on_input_event),
            cx.subscribe_in(&filter_input, window, search::on_input_event),
            // `TextInput` does not report focus loss itself; watch the handles.
            cx.on_blur(&path_input.focus_handle(cx), window, |app, _, cx| path::leave_edit(app, cx)),
            cx.on_blur(&filter_input.focus_handle(cx), window, |app, _, cx| search::exit_search(app, cx)),
            cx.on_blur(&sort_focus, window, |app, _, cx| sort::close(app, cx)),
        ];
        ToolbarState {
            path: path::PathBarState::new(path_input),
            filter: search::FilterState::new(filter_input),
            sort: sort::SortMenuState::new(sort_focus),
            _subscriptions: subscriptions,
        }
    }
}

/// The active tab now shows another directory (navigation, tab switch) or its filter was
/// cleared by the shell: sync the path field and the filter field from `app.tab()`.
pub fn on_directory_changed(app: &mut FmApp, cx: &mut Context<FmApp>) {
    path::leave_edit(app, cx);
    search::exit_search(app, cx);
    search::sync_from_tab(app, cx);
}

/// `esc` cascade: close path edit / autocomplete / sort menu, or blur the filter.
/// Return true if something was closed.
pub fn cancel(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> bool {
    let closed = if app.toolbar.sort.open {
        sort::close(app, cx);
        true
    } else if app.toolbar.filter.search_mode {
        search::exit_search(app, cx);
        true
    } else if app.toolbar.path.editing {
        path::leave_edit(app, cx);
        true
    } else {
        app.toolbar.filter.input.read(cx).is_focused(window)
    };
    if closed {
        app.focus_list(window);
        cx.notify();
    }
    closed
}

/// `/` — focus the filter field.
pub fn act_filter(app: &mut FmApp, _: &Filter, window: &mut Window, cx: &mut Context<FmApp>) {
    search::focus_filter(app, window, cx);
}

/// `ctrl+/` — recursive search.
pub fn act_recursive_search(app: &mut FmApp, _: &RecursiveSearch, window: &mut Window, cx: &mut Context<FmApp>) {
    if app.in_tag_view() {
        return crate::overlays::toast(app, "Recursive search needs a folder · open one first", cx);
    }
    search::enter_search(app, window, cx);
}

/// `ctrl+l` — path bar edit mode.
pub fn act_edit_path(app: &mut FmApp, _: &EditPath, window: &mut Window, cx: &mut Context<FmApp>) {
    path::start_edit(app, window, cx);
}

/// `s` — open the sort / hidden menu.
pub fn act_sort_menu(app: &mut FmApp, _: &SortMenu, window: &mut Window, cx: &mut Context<FmApp>) {
    sort::toggle(app, window, cx);
}

pub fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    // A field that is no longer shown must not keep the keyboard (e.g. the directory changed
    // through `alt+←` while the path was being edited): hand focus back to the list.
    let st = &app.toolbar;
    let stray_focus = (!st.path.editing && st.path.input.read(cx).is_focused(window))
        || (!st.sort.open && st.sort.focus.is_focused(window));
    if stray_focus {
        app.focus_list(window);
    }
    app.toolbar.sort.dismissed_by_mouse = false;

    let t = theme(cx).clone();
    let back = nav_button("nav-back", Icon::ChevronLeft, app.can_go_back(), cx)
        .on_click(cx.listener(|app, _, _, cx| app.go_back(cx)));
    let forward = nav_button("nav-forward", Icon::ChevronRight, app.can_go_forward(), cx)
        .on_click(cx.listener(|app, _, _, cx| app.go_forward(cx)));

    div()
        .h(px(theme::TOOLBAR_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .border_b_1()
        .border_color(t.line)
        .child(back)
        .child(forward)
        .child(path::render(app, window, cx))
        .child(search::render(app, window, cx))
        .child(view_toggle(app, cx))
        .child(quicklook_button(app, cx))
        .child(sort::render(app, window, cx))
        .into_any_element()
}

/// 30×30 ghost icon button; dimmed and inert when `enabled` is false.
fn nav_button(id: &'static str, ic: Icon, enabled: bool, cx: &Context<FmApp>) -> Stateful<gpui::Div> {
    let t = theme(cx);
    let hover_bg = t.surface_2;
    div()
        .id(id)
        .size(px(30.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(t.radius_control)
        .child(icon(ic, 15.).text_color(t.fg))
        .map(|d| if enabled { d.cursor_pointer().hover(move |s| s.bg(hover_bg)) } else { d.opacity(0.4) })
}

/// Segmented list / grid switch: two 30×26 buttons inside 2px padding and a 1px line.
fn view_toggle(app: &FmApp, cx: &mut Context<FmApp>) -> impl IntoElement {
    let t = theme(cx).clone();
    let segment = |id: &'static str, ic: Icon, mode: ViewMode, cx: &mut Context<FmApp>| {
        let active = app.view_mode == mode;
        let hover_bg = t.surface_2;
        div()
            .id(id)
            .w(px(30.))
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.radius_control)
            .cursor_pointer()
            .map(|d| if active { d.bg(t.accent) } else { d.hover(move |s| s.bg(hover_bg)) })
            .child(icon(ic, 14.).text_color(if active { t.on_accent } else { t.fg }))
            .on_click(cx.listener(move |app, _, _, cx| app.set_view_mode(mode, cx)))
    };
    div()
        .flex()
        .flex_none()
        .gap(px(2.))
        .p(px(2.))
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_control)
        .child(segment("view-list", Icon::List, ViewMode::List, cx))
        .child(segment("view-grid", Icon::Grid, ViewMode::Grid, cx))
}

/// Eye icon + `space` keycap; accent filled while Quick Look is open.
fn quicklook_button(app: &FmApp, cx: &mut Context<FmApp>) -> impl IntoElement {
    let t = theme(cx).clone();
    let active = app.quicklook.is_open();
    let hover_bg = t.surface_2;
    let cap = keycap("space", cx).text_color(if active { t.on_accent } else { t.fg });
    div()
        .id("quicklook-button")
        .h(px(30.))
        .px(px(8.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .border_1()
        .rounded(t.radius_control)
        .cursor_pointer()
        .map(|d| {
            if active {
                d.bg(t.accent).border_color(t.accent)
            } else {
                d.border_color(t.line).hover(move |s| s.bg(hover_bg))
            }
        })
        .child(icon(Icon::Eye, 14.).text_color(if active { t.on_accent } else { t.fg }))
        .child(if active { cap.border_color(alpha(t.on_accent, 0.35)) } else { cap })
        .on_click(|_, window, cx| window.dispatch_action(Box::new(actions::ToggleQuickLook), cx))
}

/// Mouse-down listener for popups and fields: keeps the click from moving keyboard focus to
/// the root (which would blur the text field the popup belongs to).
fn keep_focus<E: InteractiveElement>(el: E) -> E {
    el.on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
}
