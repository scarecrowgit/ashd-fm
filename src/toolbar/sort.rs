//! Sort / hidden menu behind the sliders button (SPEC §4.2, menu style §7).

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, Context, FocusHandle, FontWeight, KeyDownEvent, MouseButton, Window,
    deferred, div, prelude::*, px,
};

use crate::{
    app::FmApp,
    fs_model::{SortKey, SortSpec},
    icons::{Icon, icon},
    text_input,
    theme::{self, theme},
    ui::{icon_button, label},
};

const MENU_W: f32 = 250.;
/// Below the 30px button, lined up with the other toolbar popups.
const MENU_TOP: f32 = 35.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    Key(SortKey),
    Ascending(bool),
    FoldersFirst,
    ShowHidden,
}

/// Menu rows in order; `None` is a separator.
const ROWS: [Option<Item>; 10] = [
    Some(Item::Key(SortKey::Name)),
    Some(Item::Key(SortKey::Size)),
    Some(Item::Key(SortKey::Modified)),
    Some(Item::Key(SortKey::Type)),
    None,
    Some(Item::Ascending(true)),
    Some(Item::Ascending(false)),
    None,
    Some(Item::FoldersFirst),
    Some(Item::ShowHidden),
];

impl Item {
    fn title(self) -> &'static str {
        match self {
            Item::Key(SortKey::Name) => "Name",
            Item::Key(SortKey::Size) => "Size",
            Item::Key(SortKey::Modified) => "Modified",
            Item::Key(SortKey::Type) => "Type",
            Item::Ascending(true) => "Ascending",
            Item::Ascending(false) => "Descending",
            Item::FoldersFirst => "Folders first",
            Item::ShowHidden => "Show hidden",
        }
    }

    /// Key shown at the right of the row.
    fn shortcut(self) -> Option<&'static str> {
        match self {
            Item::Key(SortKey::Name) => Some("n"),
            Item::Key(SortKey::Size) => Some("s"),
            Item::Key(SortKey::Modified) => Some("m"),
            Item::Key(SortKey::Type) => Some("t"),
            Item::ShowHidden => Some("."),
            _ => None,
        }
    }

    fn is_on(self, app: &FmApp) -> bool {
        match self {
            Item::Key(key) => app.sort.key == key,
            Item::Ascending(ascending) => app.sort.ascending == ascending,
            Item::FoldersFirst => app.sort.folders_first,
            Item::ShowHidden => app.show_hidden,
        }
    }

    fn activate(self, app: &mut FmApp, cx: &mut Context<FmApp>) {
        let sort = app.sort;
        match self {
            Item::Key(key) => app.set_sort(SortSpec { key, ..sort }, cx),
            Item::Ascending(ascending) => app.set_sort(SortSpec { ascending, ..sort }, cx),
            Item::FoldersFirst => app.set_sort(SortSpec { folders_first: !sort.folders_first, ..sort }, cx),
            Item::ShowHidden => app.set_show_hidden(!app.show_hidden, cx),
        }
    }
}

pub(super) struct SortMenuState {
    pub(super) open: bool,
    /// Holds the keyboard while the menu is open.
    pub(super) focus: FocusHandle,
    /// Index into `ROWS`.
    highlight: Option<usize>,
    /// Animation key: bumped on every open.
    epoch: usize,
    /// The mouse press that just closed the menu; the sliders button must not reopen it.
    /// Cleared on the next render.
    pub(super) dismissed_by_mouse: bool,
}

impl SortMenuState {
    pub(super) fn new(focus: FocusHandle) -> Self {
        SortMenuState { open: false, focus, highlight: None, epoch: 0, dismissed_by_mouse: false }
    }
}

// ── behaviour ────────────────────────────────────────────────────────────────

pub(super) fn toggle(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    if app.toolbar.sort.open {
        close(app, cx);
        app.focus_list(window);
    } else {
        let key = app.sort.key;
        let st = &mut app.toolbar.sort;
        st.open = true;
        st.highlight = ROWS.iter().position(|row| *row == Some(Item::Key(key)));
        st.epoch += 1;
        st.focus.focus(window);
        cx.notify();
    }
}

/// Close without moving focus (callers do, when they should).
pub(super) fn close(app: &mut FmApp, cx: &mut Context<FmApp>) {
    if app.toolbar.sort.open {
        app.toolbar.sort.open = false;
        cx.notify();
    }
}

fn move_highlight(app: &mut FmApp, delta: isize, cx: &mut Context<FmApp>) {
    let st = &mut app.toolbar.sort;
    let n = ROWS.len() as isize;
    let mut ix = match st.highlight {
        Some(ix) => ix as isize,
        None if delta > 0 => -1,
        None => n,
    };
    // Step over separators, wrapping around.
    loop {
        ix = (ix + delta).rem_euclid(n);
        if ROWS[ix as usize].is_some() {
            break;
        }
    }
    st.highlight = Some(ix as usize);
    cx.notify();
}

fn activate_highlighted(app: &mut FmApp, cx: &mut Context<FmApp>) {
    if let Some(item) = app.toolbar.sort.highlight.and_then(|ix| ROWS[ix]) {
        item.activate(app, cx);
    }
}

fn on_key_down(app: &mut FmApp, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<FmApp>) {
    let keystroke = &event.keystroke;
    if keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
        return;
    }
    let key = keystroke.key.as_str();
    match key {
        "j" => move_highlight(app, 1, cx),
        "k" => move_highlight(app, -1, cx),
        "space" => activate_highlighted(app, cx),
        _ => {
            // A row's own letter picks it and closes the menu, so `s m` reads as one chord.
            let Some(item) = ROWS.iter().flatten().copied().find(|item| item.shortcut() == Some(key)) else {
                return;
            };
            item.activate(app, cx);
            close(app, cx);
            app.focus_list(window);
        }
    }
    cx.stop_propagation();
}

// ── rendering ────────────────────────────────────────────────────────────────

pub(super) fn render(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let open = app.toolbar.sort.open;

    let button = icon_button("sort-button", Icon::Sliders, cx).when(open, |d| d.bg(t.surface_2)).on_mouse_down(
        MouseButton::Left,
        cx.listener(|app, _, window, cx| {
            // Keep the root from taking the focus the menu is about to get.
            window.prevent_default();
            if std::mem::take(&mut app.toolbar.sort.dismissed_by_mouse) {
                return;
            }
            toggle(app, window, cx);
        }),
    );

    div()
        .relative()
        .flex_none()
        .child(button)
        .when(open, |d| d.child(deferred(menu(app, cx))))
        .into_any_element()
}

fn menu(app: &FmApp, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let st = &app.toolbar.sort;

    let mut panel = div()
        .id("sort-menu")
        .absolute()
        .top(px(MENU_TOP))
        .right_0()
        .w(px(MENU_W))
        .p(px(4.))
        .flex()
        .flex_col()
        .bg(t.surface)
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_panel)
        .shadow(t.shadow_overlay())
        .occlude()
        .track_focus(&st.focus)
        // Single-letter app keys are bound in `Fm && !TextInput`; claiming the `TextInput`
        // context keeps them (`d` = trash!) dead while the menu has the keyboard, and routes
        // ↑ ↓ ↵ esc through the text-field actions handled below.
        .key_context("TextInput")
        .on_action(cx.listener(|app, _: &text_input::Up, _, cx| move_highlight(app, -1, cx)))
        .on_action(cx.listener(|app, _: &text_input::Down, _, cx| move_highlight(app, 1, cx)))
        .on_action(cx.listener(|app, _: &text_input::Submit, _, cx| activate_highlighted(app, cx)))
        .on_action(cx.listener(|app, _: &text_input::Escape, window, cx| {
            close(app, cx);
            app.focus_list(window);
        }))
        .on_key_down(cx.listener(on_key_down))
        .on_mouse_down_out(cx.listener(|app, _, window, cx| {
            if app.toolbar.sort.open {
                app.toolbar.sort.dismissed_by_mouse = true;
                close(app, cx);
                app.focus_list(window);
            }
        }))
        .child(div().px(px(10.)).pt(px(6.)).pb(px(4.)).child(label("sort by", cx)));

    for (ix, row) in ROWS.iter().enumerate() {
        let Some(item) = *row else {
            panel = panel.child(div().h(px(1.)).flex_none().my(px(4.)).bg(t.line));
            continue;
        };
        let hover_bg = t.surface_2;
        panel = panel.child(
            div()
                .id(("sort-item", ix))
                .h(px(30.))
                .flex_none()
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(t.radius_control)
                .font_family(t.font_sans.clone())
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.fg)
                .cursor_pointer()
                .when(st.highlight == Some(ix), |d| d.bg(t.surface_2))
                .hover(move |s| s.bg(hover_bg))
                .child(
                    div()
                        .size(px(14.))
                        .flex_none()
                        .when(item.is_on(app), |d| d.child(icon(Icon::Check, 14.).text_color(t.accent))),
                )
                .child(div().flex_1().min_w_0().truncate().child(item.title()))
                .when_some(item.shortcut(), |d, key| {
                    d.child(
                        div()
                            .flex_none()
                            .font_family(t.font_mono.clone())
                            .text_size(px(10.5))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(t.fg_muted)
                            .child(key),
                    )
                })
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.toolbar.sort.highlight = Some(ix);
                    item.activate(app, cx);
                })),
        );
    }

    panel
        .with_animation(
            ("sort-menu", st.epoch),
            Animation::new(Duration::from_millis(theme::DUR_FAST)).with_easing(theme::ease_standard),
            |el, delta| el.opacity(delta).top(px(MENU_TOP - 4. + 4. * delta)),
        )
        .into_any_element()
}
