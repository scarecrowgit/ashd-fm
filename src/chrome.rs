//! Window chrome: titlebar 28, tab strip 36, status footer 30, key-hint bar 34 (SPEC §3, §4.6).

use gpui::{AnyElement, Context, MouseButton, SharedString, Window, div, prelude::*, px};

use crate::{
    actions,
    app::{FmApp, cwd_label},
    fs_model::format_size,
    icons::Icon,
    sidebar,
    theme::{self, theme},
    ui::{icon_button, keycap, label},
};

pub fn titlebar(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx);
    div()
        .h(px(theme::TITLEBAR_H))
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px(px(12.))
        .border_b_1()
        .border_color(t.line)
        .font_family(t.font_mono.clone())
        .text_size(px(11.))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child("ashd-fm")
                // SAFETY: geteuid has no preconditions.
                .when(unsafe { libc::geteuid() } == 0, |d| d.child(div().text_color(t.err).child("ROOT"))),
        )
        .child(div().text_color(t.fg_muted).child(cwd_label(app)))
        .into_any_element()
}

pub fn tabs(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let active = app.active_tab;
    let mut strip = div()
        .h(px(theme::TABS_H))
        .flex_none()
        .flex()
        .items_end()
        .gap(px(2.))
        .px(px(10.))
        .border_b_1()
        .border_color(t.line)
        .bg(t.surface);

    for (ix, tab) in app.tabs.iter().enumerate() {
        let is_active = ix == active;
        let jobs = app.ops.jobs_for_tab(tab.id);
        strip = strip.child(
            div()
                .id(("tab", tab.id))
                .relative()
                .h(px(30.))
                // Sit on top of the strip's bottom line so the active tab merges into the toolbar.
                .mb(px(-1.))
                .px(px(14.))
                .flex()
                .items_center()
                .gap(px(8.))
                .border_1()
                .border_b_0()
                .font_family(t.font_mono.clone())
                .text_size(px(12.))
                .cursor_pointer()
                .map(|d| {
                    if is_active {
                        d.bg(t.surface_2).border_color(t.line).text_color(t.fg).child(
                            div().absolute().left_0().right_0().top_0().h(px(1.)).bg(t.accent),
                        )
                    } else {
                        d.border_color(gpui::transparent_black()).text_color(t.fg_muted)
                    }
                })
                .child(SharedString::from(tab.title()))
                .when(jobs > 0, |d| {
                    let text = if jobs == 1 { "1 job".to_string() } else { format!("{jobs} jobs") };
                    d.child(
                        div()
                            .id(("tab-jobs", tab.id))
                            .font_family(t.font_mono.clone())
                            .text_size(px(9.5))
                            .text_color(t.accent)
                            .child(text.to_uppercase())
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                crate::overlays::toggle_jobs_panel(this, cx)
                            })),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(ix, cx)))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, _, cx| this.close_tab(ix, cx)),
                ),
        );
    }

    strip
        .child(
            icon_button("tab-new", Icon::Plus, cx)
                .on_click(cx.listener(|this, _, _, cx| this.new_tab(None, cx))),
        )
        .into_any_element()
}

pub fn footer(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let tab = app.tab();
    let dot = || label("·", cx);

    let mut row = div()
        .h(px(theme::FOOTER_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .border_t_1()
        .border_color(t.line)
        .bg(t.surface)
        .child(label(format!("{} items", tab.visible.len()), cx));

    if !tab.selected.is_empty() {
        row = row.child(dot()).child(
            label(format!("{} selected · {}", tab.selected.len(), format_size(tab.selected_size())), cx)
                .text_color(t.fg),
        );
    }
    row = row
        .child(dot())
        .child(label(if app.show_hidden { "hidden on" } else { "hidden off" }, cx))
        .child(div().flex_1());

    if let Some(branch) = app.git.branch.clone() {
        let mut text = format!("git {branch}");
        if app.git.ahead > 0 {
            text += &format!(" ↑{}", app.git.ahead);
        }
        if app.git.behind > 0 {
            text += &format!(" ↓{}", app.git.behind);
        }
        if app.git.modified > 0 {
            text += &format!(" ~{}", app.git.modified);
        }
        row = row.child(label(text, cx));
    }
    if let Some(free) = sidebar::free_space_label(app) {
        if app.git.branch.is_some() {
            row = row.child(dot());
        }
        row = row.child(label(free, cx));
    }
    row.child(dot()).child(label(t.name.label(), cx)).into_any_element()
}

pub fn hint_bar(_app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let hints: [(&[&str], &str); 8] = [
        (&["↵"], "open"),
        (&["space"], "quick look"),
        (&["↑", "↓"], "move"),
        (&["r"], "rename"),
        (&["y", "p"], "copy/paste"),
        (&["d"], "trash"),
        (&["/"], "filter"),
        (&["?"], "all keys"),
    ];
    let mut row = div()
        .id("hint-bar")
        .h(px(theme::HINTBAR_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(18.))
        .px(px(14.))
        .border_t_1()
        .border_color(t.line)
        .overflow_hidden();
    for (keys, text) in hints {
        row = row.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .children(keys.iter().map(|k| keycap(*k, cx)))
                .child(div().text_size(px(11.5)).text_color(t.fg_muted).child(text)),
        );
    }
    row.on_click(|_, window, cx| window.dispatch_action(Box::new(actions::KeyHints), cx)).into_any_element()
}
