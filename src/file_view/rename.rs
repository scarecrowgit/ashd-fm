//! Inline rename (SPEC §6): the name cell of the cursor row / tile becomes a text field with
//! the stem pre-selected and the extension protected.

use std::{fs, path::PathBuf};

use gpui::{AnyElement, Context, Div, Entity, Focusable, Window, deferred, div, prelude::*, px};

use crate::{
    app::FmApp,
    text_input::{TextInput, TextInputEvent},
    theme::theme,
};

pub struct RenameState {
    pub path: PathBuf,
    original: String,
    problem: Option<Problem>,
    /// Started without a `Window` (after "New file"): take focus on the next render.
    needs_focus: bool,
}

enum Problem {
    /// A file with this name exists; a second `enter` replaces it.
    Exists(String),
    Failed(String),
}

/// Put `path` into rename mode. Returns false if it is not in the active tab.
fn begin(app: &mut FmApp, path: PathBuf, cx: &mut Context<FmApp>) -> bool {
    let Some(row) = app.tab().row_of(&path) else { return false };
    let Some(entry) = app.tab().entry_at(row) else { return false };
    let name = entry.name.to_string();
    // `notes.md` → stem `notes`, protected `.md`. Folders and dotfiles (`.gitignore`): everything.
    let split = if entry.is_dir {
        None
    } else {
        name.rsplit_once('.').filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty()).map(|(stem, ext)| (stem.len(), ext.len() + 1))
    };
    if app.tab().cursor != row {
        app.set_cursor(row, cx);
    }
    app.files.rename_input.update(cx, |input, cx| {
        input.set_text(name.clone(), cx);
        match split {
            Some((stem_len, suffix_len)) => {
                input.set_protected_suffix(suffix_len, cx);
                input.select_range(0..stem_len, cx);
            }
            None => input.select_all_text(cx),
        }
    });
    app.files.rename = Some(RenameState { path, original: name, problem: None, needs_focus: true });
    cx.notify();
    true
}

pub fn request(app: &mut FmApp, path: PathBuf, cx: &mut Context<FmApp>) {
    begin(app, path, cx);
}

pub fn start_on_cursor(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some(path) = app.cursor_entry().map(|e| e.path.clone()) else { return };
    if begin(app, path, cx) {
        focus_field(app, window, cx);
    }
}

fn focus_field(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    app.files.rename_input.focus_handle(cx).focus(window);
    if let Some(state) = app.files.rename.as_mut() {
        state.needs_focus = false;
    }
}

/// Drop the rename without touching the disk. True if one was active.
pub fn discard(app: &mut FmApp, cx: &mut Context<FmApp>) -> bool {
    let was_active = app.files.rename.take().is_some();
    if was_active {
        cx.notify();
    }
    was_active
}

/// Called at the top of every render: take pending focus, drop a rename whose row vanished.
pub fn sync(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some(state) = app.files.rename.as_ref() else { return };
    if app.tab().row_of(&state.path).is_none() {
        let had_focus = app.files.rename_input.read(cx).is_focused(window);
        app.files.rename = None;
        if had_focus {
            app.focus_list(window);
        }
    } else if state.needs_focus {
        focus_field(app, window, cx);
    }
}

pub fn on_input_event(
    app: &mut FmApp,
    _: &Entity<TextInput>,
    event: &TextInputEvent,
    window: &mut Window,
    cx: &mut Context<FmApp>,
) {
    match event {
        TextInputEvent::Submit => commit(app, window, cx),
        TextInputEvent::Cancel => {
            if discard(app, cx) {
                app.focus_list(window);
            }
        }
        TextInputEvent::Blur => {
            discard(app, cx);
        }
        TextInputEvent::Changed => {
            // Editing the name withdraws the "enter again to replace" confirmation.
            if let Some(state) = app.files.rename.as_mut()
                && state.problem.take().is_some()
            {
                cx.notify();
            }
        }
        _ => {}
    }
}

/// Focus left the field (click elsewhere, another text field): cancel.
pub fn on_blur(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    if app.files.rename.as_ref().is_some_and(|s| !s.needs_focus) && discard(app, cx) {
        // The field can also "blur" by scrolling out of the virtualised list while it still
        // owns the focus; hand the keyboard back to the list in that case.
        if app.files.rename_input.read(cx).is_focused(window) {
            app.focus_list(window);
        }
    }
}

fn commit(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let new_name = app.files.rename_input.read(cx).text().to_string();
    let Some(state) = app.files.rename.as_mut() else { return };
    if new_name.is_empty() || new_name == state.original {
        discard(app, cx);
        app.focus_list(window);
        return;
    }
    cx.notify();
    if new_name.contains('/') || new_name == "." || new_name == ".." {
        state.problem = Some(Problem::Failed("not a valid name".into()));
        return;
    }
    let Some(dest) = state.path.parent().map(|parent| parent.join(&new_name)) else {
        state.problem = Some(Problem::Failed("cannot rename this".into()));
        return;
    };
    let confirmed = matches!(&state.problem, Some(Problem::Exists(name)) if *name == new_name);
    if !confirmed && fs::symlink_metadata(&dest).is_ok() {
        state.problem = Some(Problem::Exists(new_name));
        return;
    }
    match fs::rename(&state.path, &dest) {
        Ok(()) => {
            app.files.rename = None;
            app.reload_and_focus(dest, false, cx);
            app.focus_list(window);
        }
        Err(err) => state.problem = Some(Problem::Failed(err.to_string())),
    }
}

/// The 28-high field that replaces the name. `text_size` is 12 in rows, 11 in tiles.
pub fn field(app: &FmApp, text_size: f32, cx: &Context<FmApp>) -> Div {
    let t = theme(cx);
    let has_problem = app.files.rename.as_ref().is_some_and(|s| s.problem.is_some());
    div()
        .h(px(28.))
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .px(px(8.))
        .bg(t.bg)
        .border_1()
        .border_color(if has_problem { t.err } else { t.accent })
        .font_family(t.font_mono.clone())
        .text_size(px(text_size))
        .text_color(t.fg)
        .child(app.files.rename_input.clone())
}

/// Message under the field (`README.md exists · ↵ replace, esc cancel`). It floats over the
/// next row, `top` px below the top of its (relative) parent, because rows have a fixed height.
pub fn message(app: &FmApp, top: f32, cx: &Context<FmApp>) -> Option<AnyElement> {
    let t = theme(cx);
    let text = match app.files.rename.as_ref()?.problem.as_ref()? {
        Problem::Exists(name) => format!("{name} exists · ↵ replace, esc cancel"),
        Problem::Failed(reason) => format!("{reason} · esc cancel"),
    };
    Some(
        deferred(
            div()
                .absolute()
                .top(px(top))
                .left_0()
                .h(px(22.))
                .max_w_full()
                .px(px(8.))
                .flex()
                .items_center()
                .bg(t.surface)
                .font_family(t.font_mono.clone())
                .text_size(px(11.))
                .text_color(t.err)
                .child(div().truncate().child(text)),
        )
        .into_any_element(),
    )
}
