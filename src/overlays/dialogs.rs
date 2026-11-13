//! Modal dialogs: the transfer conflict dialog (SPEC §7), the permanent-delete confirmation
//! and the properties sheet.

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, Focusable, FontWeight, MouseButton, ObjectFit, SharedString,
    Window, div, img, prelude::*, px,
};

use super::{
    ConflictKeepBoth, ConflictRename, ConflictReplace, ConflictSkip, DialogAccept, Layer, dialog_box,
    dialog_button, fade_in, modal_layer, refocus,
};
use crate::{
    app::FmApp,
    fs_model::{FileType, display_path, format_mtime, format_perms, format_size},
    icons::{Icon, icon},
    ops::{self, ConflictInfo, FileInfo, Job, Resolution, TransferKind},
    text_input::{TextInput, TextInputEvent},
    theme::theme,
    ui::{ButtonKind, label},
};

// ── conflict ─────────────────────────────────────────────────────────────────

/// View state of the conflict dialog; reset whenever a different conflict is shown.
#[derive(Default)]
pub(super) struct ConflictUi {
    key: Option<(u64, PathBuf)>,
    pub apply_all: bool,
    pub renaming: bool,
}

/// The conflict on screen: the oldest job that is waiting for an answer.
pub(super) fn current_conflict(app: &FmApp) -> Option<(&Job, &ConflictInfo)> {
    app.ops.jobs.iter().find_map(|job| job.conflict.as_ref().map(|c| (job, c)))
}

pub(super) fn sync_conflict_ui(app: &mut FmApp) {
    let key = current_conflict(app).map(|(job, c)| (job.id, c.dst.path.clone()));
    if app.overlays.conflict.key != key {
        app.overlays.conflict = ConflictUi { key, apply_all: false, renaming: false };
    }
}

fn reply(app: &mut FmApp, resolution: Resolution, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some((job_id, remaining)) = current_conflict(app).map(|(job, c)| (job.id, c.remaining)) else { return };
    let apply_to_all = app.overlays.conflict.apply_all && remaining > 0;
    app.overlays.conflict.renaming = false;
    ops::resolve_conflict(app, job_id, resolution, apply_to_all, cx);
    refocus(app, window);
    cx.notify();
}

fn start_rename(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some((name, is_dir)) = current_conflict(app).map(|(_, c)| (c.src.name.clone(), c.src.is_dir)) else { return };
    let stem = ops::split_ext(&name, is_dir).0.len();
    app.overlays.rename_input.update(cx, |input, cx| {
        input.set_text(name, cx);
        input.select_range(0..stem, cx);
    });
    app.overlays.conflict.renaming = true;
    app.overlays.rename_input.focus_handle(cx).focus(window);
    cx.notify();
}

fn stop_rename(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    app.overlays.conflict.renaming = false;
    refocus(app, window);
    cx.notify();
}

fn submit_rename(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let name = app.overlays.rename_input.read(cx).text().trim().to_string();
    if !name.is_empty() && !name.contains('/') && name != "." && name != ".." {
        reply(app, Resolution::Rename(name), window, cx);
    }
}

/// Events of the "Rename…" field.
pub(super) fn on_rename_input_event(
    app: &mut FmApp,
    _input: &Entity<TextInput>,
    event: &TextInputEvent,
    window: &mut Window,
    cx: &mut Context<FmApp>,
) {
    if !app.overlays.conflict.renaming {
        return;
    }
    match event {
        TextInputEvent::Submit => submit_rename(app, window, cx),
        TextInputEvent::Cancel => stop_rename(app, window, cx),
        _ => {}
    }
}

fn conflict_card(title: &str, info: &FileInfo, newer: bool, cx: &App) -> Div {
    let t = theme(cx);
    let heading = if newer { format!("{title} · newer") } else { title.to_string() };
    let thumbnail = if info.dims.is_some() {
        img(info.path.clone()).size_full().object_fit(ObjectFit::Contain).into_any_element()
    } else {
        icon(FileType::from_name(&info.name, info.is_dir).icon(), 26.).text_color(t.fg_muted).into_any_element()
    };
    let row = |key: &'static str, value: String| {
        div()
            .flex()
            .gap(px(4.))
            .child(div().w(px(70.)).flex_none().text_color(t.fg_muted).child(key))
            .child(div().flex_1().min_w_0().truncate().child(value))
    };
    let size = if info.is_dir { "folder".to_string() } else { format_size(info.size) };
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(14.))
        .border_1()
        .border_color(if newer { t.accent } else { t.line })
        .child(label(heading, cx).text_color(if newer { t.accent } else { t.fg_muted }))
        .child(
            div()
                .h(px(96.))
                .w_full()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .bg(t.bg)
                .child(thumbnail),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .font_family(t.font_mono.clone())
                .text_size(px(11.5))
                .child(row("size", size))
                .child(row("modified", format_mtime(info.modified)))
                .children(info.dims.map(|(w, h)| row("dims", format!("{w}×{h}")))),
        )
}

pub(super) fn render_conflict(app: &FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let (job, conflict) = current_conflict(app)?;
    let t = theme(cx).clone();
    let ui = &app.overlays.conflict;
    let verb = match job.kind {
        TransferKind::Copy => "Copying",
        TransferKind::Move => "Moving",
        TransferKind::Link => "Linking",
    };
    let subtitle = format!(
        // Count first: the path can be long and the line is clipped at the dialog edge.
        "conflict {} of {} · {verb} to {}",
        conflict.index,
        conflict.index + conflict.remaining,
        display_path(&job.to)
    );
    // The newer side gets the accent.
    let incoming_newer = match (conflict.src.modified, conflict.dst.modified) {
        (Some(a), Some(b)) if a != b => Some(a > b),
        _ => None,
    };

    let title: AnyElement = if ui.renaming {
        div()
            .h(px(32.))
            .px(px(10.))
            .flex()
            .items_center()
            .bg(t.bg)
            .border_1()
            .border_color(t.accent)
            .rounded(t.radius_control)
            .font_family(t.font_mono.clone())
            .text_size(px(12.))
            .child(app.overlays.rename_input.clone())
            .into_any_element()
    } else {
        div()
            .truncate()
            .text_size(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(format!("{} already exists", conflict.dst.name))
            .into_any_element()
    };

    let mut dialog = dialog_box(560., cx)
        .child(div().flex().flex_col().gap(px(4.)).child(title).child(label(subtitle, cx).truncate()))
        .child(
            div()
                .flex()
                .gap(px(12.))
                .child(conflict_card("in folder", &conflict.dst, incoming_newer == Some(false), cx))
                .child(conflict_card("incoming", &conflict.src, incoming_newer == Some(true), cx)),
        );

    if conflict.remaining > 0 && !ui.renaming {
        let on = ui.apply_all;
        dialog = dialog.child(
            div()
                .id("conflict-apply-all")
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.5))
                .cursor_pointer()
                .child(
                    div()
                        .size(px(14.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .border_1()
                        .border_color(if on { t.accent } else { t.fg_muted })
                        .when(on, |d| d.bg(t.accent).child(icon(Icon::Check, 10.).text_color(t.on_accent))),
                )
                .child(format!("Apply to all ({} more)", conflict.remaining))
                .on_click(cx.listener(|app, _, _, cx| {
                    app.overlays.conflict.apply_all = !app.overlays.conflict.apply_all;
                    cx.notify();
                })),
        );
    }

    let buttons = div().flex().justify_end().gap(px(8.));
    dialog = dialog.child(if ui.renaming {
        buttons
            .child(
                dialog_button("conflict-rename-cancel", "Cancel", ButtonKind::Ghost, cx)
                    .on_click(cx.listener(|app, _, window, cx| stop_rename(app, window, cx))),
            )
            .child(
                dialog_button("conflict-rename-ok", "Rename", ButtonKind::Primary, cx)
                    .on_click(cx.listener(|app, _, window, cx| submit_rename(app, window, cx))),
            )
    } else {
        buttons
            .child(
                dialog_button("conflict-skip", "Skip", ButtonKind::Ghost, cx)
                    .on_click(cx.listener(|app, _, window, cx| reply(app, Resolution::Skip, window, cx))),
            )
            .child(
                dialog_button("conflict-keep", "Keep both", ButtonKind::Secondary, cx)
                    .on_click(cx.listener(|app, _, window, cx| reply(app, Resolution::KeepBoth, window, cx))),
            )
            .child(
                dialog_button("conflict-rename", "Rename…", ButtonKind::Secondary, cx)
                    .on_click(cx.listener(|app, _, window, cx| start_rename(app, window, cx))),
            )
            .child(
                dialog_button("conflict-replace", "Replace", ButtonKind::Primary, cx)
                    .on_click(cx.listener(|app, _, window, cx| reply(app, Resolution::Replace, window, cx))),
            )
    });

    // While the name is being edited the letter keys belong to the text field.
    let context = if ui.renaming { "FmConflictRename" } else { "FmConflict" };
    let layer = modal_layer(app, Layer::Conflict, context, true, cx)
        .on_action(cx.listener(|app, _: &ConflictSkip, window, cx| reply(app, Resolution::Skip, window, cx)))
        .on_action(cx.listener(|app, _: &ConflictKeepBoth, window, cx| reply(app, Resolution::KeepBoth, window, cx)))
        .on_action(cx.listener(|app, _: &ConflictRename, window, cx| start_rename(app, window, cx)))
        .on_action(cx.listener(|app, _: &ConflictReplace, window, cx| reply(app, Resolution::Replace, window, cx)))
        .child(dialog);
    Some(fade_in(layer, ("conflict", job.id)))
}

// ── permanent delete ─────────────────────────────────────────────────────────

fn accept_delete(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    if let Some(paths) = app.overlays.confirm.take() {
        ops::delete_permanently(app, paths, cx);
    }
    refocus(app, window);
    cx.notify();
}

fn close_confirm(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    app.overlays.confirm = None;
    refocus(app, window);
    cx.notify();
}

/// Key name inside a dialog button.
fn button_key(text: &'static str, cx: &App) -> Div {
    div().font_family(theme(cx).font_mono.clone()).text_size(px(10.5)).opacity(0.7).child(text)
}

pub(super) fn render_confirm(app: &FmApp, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let paths = app.overlays.confirm.as_ref()?;
    let t = theme(cx).clone();
    const SHOWN: usize = 5;

    let mut names = div().flex().flex_col().gap(px(2.)).font_family(t.font_mono.clone()).text_size(px(12.));
    for path in paths.iter().take(SHOWN) {
        names = names.child(div().truncate().child(ops::name_of(path)));
    }
    if paths.len() > SHOWN {
        names = names.child(div().text_color(t.fg_muted).child(format!("and {} more", paths.len() - SHOWN)));
    }

    let dialog = dialog_box(440., cx)
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Delete {} permanently?", ops::plural(paths.len() as u64, "item"))),
        )
        .child(names)
        .child(div().text_size(px(12.5)).text_color(t.err).child("This cannot be undone."))
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    dialog_button("confirm-cancel", "Cancel", ButtonKind::Ghost, cx)
                        .child(button_key("esc", cx))
                        .on_click(cx.listener(|app, _, window, cx| close_confirm(app, window, cx))),
                )
                .child(
                    dialog_button("confirm-delete", "Delete", ButtonKind::Primary, cx)
                        .bg(t.err)
                        .child(button_key("↵", cx))
                        .on_click(cx.listener(|app, _, window, cx| accept_delete(app, window, cx))),
                ),
        );

    let layer = modal_layer(app, Layer::Confirm, "FmDialog", true, cx)
        .on_action(cx.listener(|app, _: &DialogAccept, window, cx| accept_delete(app, window, cx)))
        .on_mouse_down(MouseButton::Left, cx.listener(|app, _, window, cx| close_confirm(app, window, cx)))
        .child(dialog);
    Some(fade_in(layer, ("confirm", app.overlays.seq)))
}

// ── properties ───────────────────────────────────────────────────────────────

pub(super) struct Properties {
    path: PathBuf,
    name: SharedString,
    rows: Vec<(&'static str, SharedString)>,
}

/// `uid` → user name from a passwd / group style file.
fn lookup_id(file: &str, id: u32) -> Option<String> {
    let text = fs::read_to_string(file).ok()?;
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse::<u32>().ok()? == id).then(|| name.to_string())
    })
}

/// Everything the sheet shows (blocking: folder sizes are summed recursively).
fn read_properties(path: &Path) -> Vec<(&'static str, SharedString)> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return vec![("type", "missing".into())];
    };
    let name = ops::name_of(path);
    let is_dir = meta.is_dir();
    let mut kind = FileType::from_name(&name, is_dir).label().to_string();
    if meta.file_type().is_symlink() {
        kind = match fs::read_link(path) {
            Ok(target) => format!("symlink → {}", target.display()),
            Err(_) => "symlink".to_string(),
        };
    }
    let size = if is_dir {
        let (bytes, files) = ops::tree_size(path, None);
        format!("{} · {}", format_size(bytes), ops::plural(files, "file"))
    } else {
        format!("{} · {} bytes", format_size(meta.len()), meta.len())
    };
    let owner = format!(
        "{}:{}",
        lookup_id("/etc/passwd", meta.uid()).unwrap_or_else(|| meta.uid().to_string()),
        lookup_id("/etc/group", meta.gid()).unwrap_or_else(|| meta.gid().to_string())
    );
    vec![
        ("type", kind.into()),
        ("size", size.into()),
        ("location", path.parent().map(display_path).unwrap_or_default().into()),
        ("modified", format_mtime(meta.modified().ok()).into()),
        ("permissions", format!("{} · {:o}", format_perms(meta.mode()), meta.mode() & 0o7777).into()),
        ("owner", owner.into()),
    ]
}

pub(super) fn open_properties(app: &mut FmApp, path: PathBuf, window: &mut Window, cx: &mut Context<FmApp>) {
    // What the listing already knows is shown at once; the rest arrives from the background.
    let known = app.tab().entries.iter().find(|e| e.path == path);
    let rows = vec![
        ("type", known.map(|e| e.file_type.label()).unwrap_or("…").into()),
        ("size", "…".into()),
        ("location", path.parent().map(display_path).unwrap_or_default().into()),
        ("modified", known.map(|e| format_mtime(e.modified)).unwrap_or_else(|| "…".into()).into()),
        ("permissions", known.map(|e| format_perms(e.mode)).unwrap_or_else(|| "…".into()).into()),
        ("owner", "…".into()),
    ];
    app.overlays.properties = Some(Properties { name: ops::name_of(&path).into(), path: path.clone(), rows });
    app.overlays.next_seq();
    refocus(app, window);
    cx.notify();

    let task = cx.background_spawn({
        let path = path.clone();
        async move { read_properties(&path) }
    });
    cx.spawn(async move |this, cx| {
        let rows = task.await;
        this.update(cx, |app, cx| {
            if let Some(properties) = app.overlays.properties.as_mut().filter(|p| p.path == path) {
                properties.rows = rows;
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}

fn close_properties(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    app.overlays.properties = None;
    refocus(app, window);
    cx.notify();
}

pub(super) fn render_properties(app: &FmApp, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let properties = app.overlays.properties.as_ref()?;
    let t = theme(cx).clone();

    let mut rows = div().flex().flex_col().gap(px(6.));
    for (key, value) in &properties.rows {
        rows = rows.child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(label(*key, cx).w(px(96.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(t.font_mono.clone())
                        .text_size(px(12.))
                        .child(value.clone()),
                ),
        );
    }

    let dialog = dialog_box(440., cx)
        .child(div().truncate().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(properties.name.clone()))
        .child(rows)
        .child(
            div().flex().justify_end().child(
                dialog_button("properties-close", "Close", ButtonKind::Secondary, cx)
                    .on_click(cx.listener(|app, _, window, cx| close_properties(app, window, cx))),
            ),
        );

    let layer = modal_layer(app, Layer::Properties, "FmDialog", true, cx)
        .on_action(cx.listener(|app, _: &DialogAccept, window, cx| close_properties(app, window, cx)))
        .on_mouse_down(MouseButton::Left, cx.listener(|app, _, window, cx| close_properties(app, window, cx)))
        .child(dialog);
    Some(fade_in(layer, ElementId::Name(format!("properties-{}", app.overlays.seq).into())))
}
