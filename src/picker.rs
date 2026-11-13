//! File-chooser mode: `fm --pick --out FILE [--multiple|--directory|--save] [path]`.
//!
//! fm runs as a modal picker for other applications (through
//! xdg-desktop-portal-termfilechooser, see packaging/). The chosen absolute paths are
//! written to `--out`, one per line; an empty file / no file means "cancelled".
//! The hint bar is replaced by a bar with the mode, the file-name field (save) and
//! Cancel / accept buttons. `↵` on a file accepts it, `ctrl+↵` accepts in every mode.

use std::path::PathBuf;

use gpui::{AnyElement, Context, Entity, Focusable, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use crate::{
    app::FmApp,
    text_input::{TextInput, TextInputEvent},
    theme::theme,
    ui::{ButtonKind, button, keycap, label},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickKind {
    /// One existing file.
    File,
    /// One or more existing files.
    Files,
    /// A folder.
    Directory,
    /// A new (or replaced) file name in a folder.
    Save,
}

/// Parsed command-line request.
#[derive(Clone, Debug)]
pub struct PickRequest {
    pub kind: PickKind,
    pub out: PathBuf,
    /// Suggested file name for `Save`.
    pub name: Option<String>,
}

pub struct PickerState {
    pub kind: PickKind,
    out: PathBuf,
    name_input: Entity<TextInput>,
    /// Save: the name exists and the user was warned; the next accept replaces.
    confirm_replace: Option<PathBuf>,
    needs_focus: bool,
    _subscriptions: Vec<Subscription>,
}

impl PickerState {
    pub fn new(request: PickRequest, window: &mut Window, cx: &mut Context<FmApp>) -> Self {
        let name_input = cx.new(|cx| {
            let mut input = TextInput::new("file name", cx);
            if let Some(name) = &request.name {
                input.set_text(name.clone(), cx);
                let stem = name.rfind('.').filter(|ix| *ix > 0).unwrap_or(name.len());
                input.select_range(0..stem, cx);
            }
            input
        });
        let subscription = cx.subscribe_in(&name_input, window, |this, _, event: &TextInputEvent, window, cx| match event {
            TextInputEvent::Submit => accept(this, cx),
            TextInputEvent::Cancel | TextInputEvent::Tab => this.focus_list(window),
            TextInputEvent::Up => this.move_cursor(-1, cx),
            TextInputEvent::Down => this.move_cursor(1, cx),
            TextInputEvent::Changed => {
                if let Some(picker) = &mut this.picker {
                    picker.confirm_replace = None;
                }
                cx.notify();
            }
            TextInputEvent::Blur => {}
        });
        PickerState {
            kind: request.kind,
            out: request.out,
            name_input,
            confirm_replace: None,
            needs_focus: request.kind == PickKind::Save,
            _subscriptions: vec![subscription],
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            PickKind::File => "Open file",
            PickKind::Files => "Open files",
            PickKind::Directory => "Select folder",
            PickKind::Save => "Save file",
        }
    }
}

/// `↵` / double-click on a file while picking. Returns true when it was handled
/// (the caller must not open the file).
pub fn on_open_file(app: &mut FmApp, path: PathBuf, cx: &mut Context<FmApp>) -> bool {
    let Some(picker) = &app.picker else { return false };
    match picker.kind {
        PickKind::File | PickKind::Files => finish(app, vec![path], cx),
        PickKind::Directory => {}
        PickKind::Save => {
            // Picking an existing file proposes its name.
            if let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) {
                let input = picker.name_input.clone();
                input.update(cx, |input, cx| input.set_text(name, cx));
                app.picker.as_mut().unwrap().needs_focus = true;
                cx.notify();
            }
        }
    }
    true
}

/// `↵` with a multi-selection in `Files` mode. Returns true when handled.
pub fn on_open_selection(app: &mut FmApp, cx: &mut Context<FmApp>) -> bool {
    let multi = app.picker.as_ref().is_some_and(|p| p.kind == PickKind::Files) && !app.tab().selected.is_empty();
    if multi {
        accept(app, cx);
    }
    multi
}

/// `ctrl+↵` / the primary button.
pub fn accept(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let Some(picker) = &app.picker else { return };
    match picker.kind {
        PickKind::File => {
            if let Some(entry) = app.cursor_entry().filter(|e| !e.is_dir) {
                let path = entry.path.clone();
                finish(app, vec![path], cx);
            }
        }
        PickKind::Files => {
            let tab = app.tab();
            let files: Vec<PathBuf> = tab.targets().into_iter().filter(|p| !p.is_dir()).collect();
            if !files.is_empty() {
                finish(app, files, cx);
            }
        }
        PickKind::Directory => {
            let dir = app.cwd().to_path_buf();
            finish(app, vec![dir], cx);
        }
        PickKind::Save => {
            let name = picker.name_input.read(cx).text().trim().to_string();
            if name.is_empty() || name.contains('/') {
                return;
            }
            let path = app.cwd().join(name);
            if path.is_dir() {
                return app.navigate(path, cx);
            }
            if path.exists() && picker.confirm_replace.as_ref() != Some(&path) {
                app.picker.as_mut().unwrap().confirm_replace = Some(path);
                return cx.notify();
            }
            finish(app, vec![path], cx);
        }
    }
}

/// Close without choosing anything.
pub fn cancel(app: &mut FmApp, cx: &mut Context<FmApp>) {
    if let Some(picker) = &app.picker {
        let _ = std::fs::write(&picker.out, "");
    }
    cx.quit();
}

fn finish(app: &mut FmApp, paths: Vec<PathBuf>, cx: &mut Context<FmApp>) {
    let Some(picker) = &app.picker else { return };
    let text: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    if let Err(err) = std::fs::write(&picker.out, text) {
        eprintln!("fm: cannot write {}: {err}", picker.out.display());
    }
    cx.quit();
}

/// The bar that replaces the key-hint bar while picking.
pub fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let Some(picker) = app.picker.as_mut() else { return div().into_any_element() };
    if std::mem::take(&mut picker.needs_focus) {
        picker.name_input.focus_handle(cx).focus(window);
    }
    let picker = app.picker.as_ref().unwrap();
    let kind = picker.kind;
    let tab = app.tab();

    let (action, enabled): (&str, bool) = match kind {
        PickKind::File => ("Open", app.cursor_entry().is_some_and(|e| !e.is_dir)),
        PickKind::Files => ("Open", !tab.selected.is_empty() || app.cursor_entry().is_some_and(|e| !e.is_dir)),
        PickKind::Directory => ("Select this folder", true),
        PickKind::Save if picker.confirm_replace.is_some() => ("Replace", true),
        PickKind::Save => ("Save", !picker.name_input.read(cx).text().trim().is_empty()),
    };
    let detail: SharedString = match kind {
        PickKind::File => app.cursor_entry().filter(|e| !e.is_dir).map(|e| e.name.clone()).unwrap_or_default(),
        PickKind::Files if !tab.selected.is_empty() => format!("{} selected", tab.selected.len()).into(),
        PickKind::Files => app.cursor_entry().filter(|e| !e.is_dir).map(|e| e.name.clone()).unwrap_or_default(),
        PickKind::Directory => crate::fs_model::display_path(app.cwd()).into(),
        PickKind::Save => SharedString::default(),
    };

    let mut bar = div()
        .h(px(48.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .border_t_1()
        .border_color(t.accent)
        .bg(t.surface)
        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).flex_none().child(picker.title()));

    if kind == PickKind::Save {
        let focused = picker.name_input.read(cx).is_focused(window);
        let warn = picker.confirm_replace.is_some();
        bar = bar.child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(30.))
                .px(px(10.))
                .flex()
                .items_center()
                .bg(t.bg)
                .border_1()
                .border_color(if warn { t.err } else if focused { t.accent } else { t.line })
                .rounded(t.radius_control)
                .font_family(t.font_mono.clone())
                .text_size(px(12.))
                .child(picker.name_input.clone()),
        );
        if warn {
            bar = bar.child(label("exists · ↵ replace", cx).text_color(t.err));
        }
    } else {
        bar = bar.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(t.font_mono.clone())
                .text_size(px(12.))
                .text_color(t.fg_muted)
                .child(detail),
        );
    }

    bar.child(
        button("picker-cancel", "Cancel", ButtonKind::Ghost, cx)
            .child(keycap("esc", cx))
            .on_click(cx.listener(|this, _, _, cx| cancel(this, cx))),
    )
    .child(
        button("picker-accept", action, ButtonKind::Primary, cx)
            .when(!enabled, |b| b.opacity(0.4))
            .on_click(cx.listener(|this, _, _, cx| accept(this, cx))),
    )
    .child(keycap("ctrl ↵", cx))
    .into_any_element()
}
