//! The main column below the toolbar: list view (§4.3), grid view (§4.4), the states of §6
//! (empty / no results / permission denied / device unavailable / loading skeleton),
//! inline rename, the floating selection bar and drag & drop.
//!
//! OWNER: file-view agent. STUB — a bare list of names so the shell is usable.

use std::path::PathBuf;

use gpui::{AnyElement, Context, Window, div, prelude::*, px, uniform_list};

use crate::{
    actions::Rename,
    app::FmApp,
    theme::{self, theme},
    ui::mono,
};

pub struct FileViewState {
    /// Number of columns the grid view laid out last frame. The shell uses it for ↑/↓ in
    /// grid mode — keep it up to date when rendering the grid.
    pub grid_columns: usize,
}

impl FileViewState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        FileViewState { grid_columns: 5 }
    }
}

/// Start an inline rename of `path` in the active tab (used after "New file"/"New folder").
/// No `Window` here on purpose: it is called from an async completion; take focus on the
/// next render.
pub fn request_rename(_app: &mut FmApp, _path: PathBuf, _cx: &mut Context<FmApp>) {}

/// `r` / `F2` — rename the cursor row.
pub fn act_rename(_app: &mut FmApp, _: &Rename, _window: &mut Window, _cx: &mut Context<FmApp>) {}

/// `esc` cascade: cancel an inline rename or a drag. Return true if something was cancelled.
pub fn cancel(_app: &mut FmApp, _window: &mut Window, _cx: &mut Context<FmApp>) -> bool {
    false
}

pub fn render(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let count = app.tab().visible.len();
    let scroll = app.tab().scroll.clone();
    div()
        .flex_1()
        .min_h_0()
        .child(
            uniform_list(
                "file-list",
                count,
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    let t = theme(cx).clone();
                    let tab = this.tab();
                    range
                        .filter_map(|row| {
                            let entry = tab.entry_at(row)?;
                            let is_cursor = row == tab.cursor;
                            Some(
                                div()
                                    .id(row)
                                    .h(px(theme::ROW_H))
                                    .px(px(14.))
                                    .flex()
                                    .items_center()
                                    .border_l_2()
                                    .border_color(if is_cursor { t.accent } else { gpui::transparent_black() })
                                    .when(is_cursor, |d| d.bg(t.surface_2))
                                    .child(mono(entry.display_name(), cx))
                                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
                                        this.set_cursor(row, cx);
                                        if ev.click_count() >= 2 {
                                            this.open_cursor(cx);
                                        }
                                    })),
                            )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(scroll)
            .size_full(),
        )
        .into_any_element()
}
