//! Preview pane 340 (SPEC §4.5) and the shared preview loading state.
//!
//! `app.preview.data` always describes the cursor entry (or is `None` while loading / when
//! the folder is empty). Quick Look reads the same data.
//!
//! OWNER: preview agent. STUB — loading works; the pane shows only the header.

use std::sync::Arc;

use gpui::{AnyElement, Context, FontWeight, Window, div, prelude::*, px};

use crate::{
    app::FmApp,
    preview_content::{self, PreviewData},
    theme::{self, theme},
    ui::label,
};

pub struct PreviewState {
    /// Preview of the cursor entry. `None` while the first load is in flight or nothing is under the cursor.
    pub data: Option<Arc<PreviewData>>,
    /// A load for a newer cursor entry is running (`data` may still hold the previous entry).
    pub loading: bool,
    generation: u64,
}

impl PreviewState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        PreviewState { data: None, loading: false, generation: 0 }
    }
}

/// The cursor entry may have changed: (re)load its preview off the main thread.
pub fn on_cursor_changed(app: &mut FmApp, cx: &mut Context<FmApp>) {
    app.preview.generation += 1;
    let generation = app.preview.generation;
    let Some(entry) = app.cursor_entry().cloned() else {
        app.preview.data = None;
        app.preview.loading = false;
        return;
    };
    if app.preview.data.as_ref().is_some_and(|d| d.entry.path == entry.path && d.entry.modified == entry.modified) {
        app.preview.loading = false;
        return;
    }
    app.preview.loading = true;
    let task = cx.background_spawn(async move { preview_content::load(&entry) });
    cx.spawn(async move |this, cx| {
        let data = task.await;
        this.update(cx, |this, cx| {
            if this.preview.generation == generation {
                this.preview.data = Some(Arc::new(data));
                this.preview.loading = false;
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}

pub fn render(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let mut pane = div()
        .id("preview-pane")
        .w(px(theme::PREVIEW_W))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .gap(px(14.))
        .p(px(16.))
        .border_l_1()
        .border_color(t.line)
        .overflow_hidden();
    if let Some(data) = app.preview.data.clone() {
        pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .pb(px(12.))
                .border_b_1()
                .border_color(t.line)
                .child(
                    div()
                        .font_family(t.font_mono.clone())
                        .text_size(px(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(data.entry.name.clone()),
                )
                .child(label(data.meta.clone(), cx)),
        );
    }
    pane.into_any_element()
}
