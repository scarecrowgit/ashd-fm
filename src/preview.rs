//! Preview pane 340 (SPEC §4.5) and the shared preview loading state.
//!
//! `app.preview.data` always describes the cursor entry (or is `None` while loading / when
//! the folder is empty). Quick Look reads the same data.
//!
//! OWNER: preview agent.
//!
//! Layout: header (name + meta label, bottom line) and a body that scrolls vertically inside
//! the pane. The body is keyed by the previewed path: a new path gets a fresh scroll position
//! and a 100 ms fade-in (the "crossfade" of SPEC §9). While a newer entry is loading the
//! previous one stays on screen, so moving the cursor quickly never flickers.

use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Div, FontWeight, HighlightStyle, Hsla, ObjectFit, SharedString,
    StyledText, Window, div, img, prelude::*, px, relative,
};

use crate::{
    app::FmApp,
    fs_model::Entry,
    git::Commit,
    icons::{Icon, icon},
    preview_content::{self, ArchiveEntry, FolderGit, Line, PreviewBody, PreviewData, Syn},
    theme::{self, Theme, theme},
    ui::{ButtonKind, button, label},
};

/// Rows shown before "+ N more".
const FOLDER_ROWS: usize = 12;
const ARCHIVE_ROWS: usize = 14;
const TEXT_LINES: usize = 60;
const MODIFIED_ROWS: usize = 6;
/// Content width: pane − left line − 2 × padding.
const INNER_W: f32 = theme::PREVIEW_W - 1. - 2. * PAD;
const PAD: f32 = 16.;
const IMAGE_MAX_H: f32 = 220.;
/// Images larger than this are described but not decoded for the pane.
const IMAGE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Preview swap (SPEC §9).
const SWAP_MS: u64 = 100;

pub struct PreviewState {
    /// Preview of the cursor entry. `None` while the first load is in flight or nothing is under the cursor.
    pub data: Option<Arc<PreviewData>>,
    /// A load for a newer cursor entry is running (`data` may still hold the previous entry).
    pub loading: bool,
    generation: u64,
    /// Archive currently being unpacked by `extract_here`.
    extracting: Option<PathBuf>,
    /// Last failed extraction: (archive, message). Shown under that archive's buttons.
    extract_error: Option<(PathBuf, SharedString)>,
}

impl PreviewState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        PreviewState { data: None, loading: false, generation: 0, extracting: None, extract_error: None }
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
    if app.preview.data.as_ref().is_some_and(|d| {
        d.entry.path == entry.path
            && d.entry.modified == entry.modified
            && d.entry.size == entry.size
            && d.entry.child_count == entry.child_count
    }) {
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

/// **Extract here**: unpack `archive` into the current directory off the main thread, then
/// reload and put the cursor on the result. A single top-level entry keeps its own name;
/// anything else goes into a new folder named after the archive stem. Existing files are
/// never overwritten (`name 2` … on collisions). Also Quick Look's primary action for archives.
pub fn extract_here(app: &mut FmApp, archive: PathBuf, cx: &mut Context<FmApp>) {
    if app.preview.extracting.is_some() {
        return;
    }
    let dest = app.cwd().to_path_buf();
    app.preview.extracting = Some(archive.clone());
    app.preview.extract_error = None;
    cx.notify();
    let task = cx.background_spawn({
        let (archive, dest) = (archive.clone(), dest.clone());
        async move { preview_content::extract_archive(&archive, &dest) }
    });
    cx.spawn(async move |this, cx| {
        let result = task.await;
        this.update(cx, |this, cx| {
            this.preview.extracting = None;
            match result {
                Ok(path) => {
                    if this.cwd() == dest.as_path() {
                        this.reload_and_focus(path, false, cx);
                    }
                }
                Err(message) => this.preview.extract_error = Some((archive, message.into())),
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// True while `extract_here` is unpacking `archive`.
pub fn is_extracting(app: &FmApp, archive: &Path) -> bool {
    app.preview.extracting.as_deref() == Some(archive)
}

pub fn render(app: &mut FmApp, _window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let pane = div()
        .id("preview-pane")
        .w(px(theme::PREVIEW_W))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .gap(px(14.))
        .p(px(PAD))
        .border_l_1()
        .border_color(t.line)
        .text_color(t.fg)
        .overflow_hidden();

    let Some(data) = app.preview.data.clone() else {
        return if app.preview.loading {
            pane.child(skeleton(&t)).into_any_element()
        } else {
            pane.child(label("Nothing selected", cx)).into_any_element()
        };
    };

    let header = div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(4.))
        .pb(px(12.))
        .border_b_1()
        .border_color(t.line)
        .child(
            div()
                .font_family(t.font_mono.clone())
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .truncate()
                .child(data.entry.name.clone()),
        )
        .child(label(data.meta.clone(), cx).truncate());

    let content = match &data.body {
        PreviewBody::Folder { items, git } => folder_body(&data, items, git.as_ref(), &t, cx),
        PreviewBody::Text { lines, truncated } => text_body(lines, *truncated, data.entry.size == 0, &t, cx),
        PreviewBody::Markdown { source_lines, .. } => text_body(source_lines, false, data.entry.size == 0, &t, cx),
        PreviewBody::Image { path, width, height, meta, .. } => {
            image_body(path, *width, *height, data.entry.size, meta, &t)
        }
        PreviewBody::Archive { summary, entries, .. } => archive_body(app, &data.entry, summary, entries, &t, cx),
        PreviewBody::Other { meta } => meta_grid(meta, &t),
        PreviewBody::Error(message) => div()
            .font_family(t.font_mono.clone())
            .text_size(px(11.5))
            .line_height(relative(1.6))
            .text_color(t.err)
            .child(message.clone()),
    };

    let key = path_key(&data.entry.path);
    let body = div()
        .id(("preview-body", key))
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_y_scroll()
        .overflow_x_hidden()
        .child(content.w_full())
        .with_animation(
            ("preview-swap", key),
            Animation::new(Duration::from_millis(SWAP_MS)).with_easing(theme::ease_standard),
            |el, delta| el.opacity(delta),
        );

    pane.child(header).child(body).into_any_element()
}

fn path_key(path: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// First load: header-shaped bars plus a few rows, pulsing like the list skeleton (SPEC §6).
fn skeleton(t: &Theme) -> impl IntoElement + use<> {
    let bar = |w: f32, h: f32| div().w(px(w)).h(px(h)).flex_none().bg(t.line);
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(bar(150., 14.))
        .child(bar(210., 9.))
        .child(div().h(px(1.)).w_full().mt(px(2.)).mb(px(4.)).bg(t.line))
        .children([190., 240., 130., 220., 170.].into_iter().map(|w| bar(w, 10.)))
        .with_animation(
            "preview-skeleton",
            Animation::new(Duration::from_millis(1400)).repeat().with_easing(gpui::pulsating_between(0.35, 0.8)),
            |el, delta| el.opacity(delta),
        )
}

fn syn_color(syn: Syn, t: &Theme) -> Hsla {
    match syn {
        Syn::Plain => t.fg,
        Syn::Keyword => t.syn_keyword,
        Syn::String => t.syn_string,
        Syn::Number => t.syn_number,
        Syn::Comment => t.syn_comment,
    }
}

/// One source line as a single text element with colour runs. Inherits font, size and line
/// height from its parent; never wraps (the parent clips).
pub fn styled_line(line: &Line, t: &Theme) -> StyledText {
    let mut text = String::new();
    let mut highlights = Vec::with_capacity(line.len());
    for span in line {
        let start = text.len();
        text.push_str(&span.text);
        if span.syn != Syn::Plain && text.len() > start {
            let style = HighlightStyle { color: Some(syn_color(span.syn, t)), ..Default::default() };
            highlights.push((start..text.len(), style));
        }
    }
    if text.is_empty() {
        // Keeps the height of blank lines.
        text.push(' ');
    }
    StyledText::new(text).with_highlights(highlights)
}

fn more_label(text: String, cx: &Context<FmApp>) -> Div {
    label(text, cx).mt(px(4.))
}

fn text_body(lines: &[Line], truncated: bool, empty_file: bool, t: &Theme, cx: &Context<FmApp>) -> Div {
    if empty_file {
        return div().child(label("empty file", cx));
    }
    let mut col = div()
        .flex()
        .flex_col()
        .font_family(t.font_mono.clone())
        .text_size(px(11.5))
        .line_height(relative(1.7))
        .text_color(t.fg)
        .children(
            lines
                .iter()
                .take(TEXT_LINES)
                .map(|line| div().w_full().whitespace_nowrap().overflow_hidden().child(styled_line(line, t))),
        );
    if lines.len() > TEXT_LINES {
        let more = lines.len() - TEXT_LINES;
        col = col.child(more_label(format!("+ {more}{} more lines", if truncated { "+" } else { "" }), cx));
    }
    col
}

/// icon 13 · name (mono 11.5, flexible, ellipsis) · right-aligned muted size.
fn entry_row(ic: Icon, icon_color: Hsla, name: SharedString, size: SharedString, indent: f32, t: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .pl(px(indent))
        .font_family(t.font_mono.clone())
        .text_size(px(11.5))
        .child(icon(ic, 13.).text_color(icon_color))
        .child(div().flex_1().min_w_0().truncate().child(name))
        .child(div().flex_none().text_size(px(11.)).text_color(t.fg_muted).child(size))
}

fn folder_body(data: &PreviewData, items: &[Entry], git: Option<&FolderGit>, t: &Theme, cx: &Context<FmApp>) -> Div {
    let mut col = div().flex().flex_col().gap(px(10.));
    if items.is_empty() {
        col = col.child(label("empty folder", cx));
    }
    col = col.children(items.iter().take(FOLDER_ROWS).map(|item| {
        entry_row(item.file_type.icon(), t.fg, item.display_name(), item.size_label().into(), 0., t)
    }));
    let total = data.folder_total.max(items.len());
    if total > FOLDER_ROWS {
        col = col.child(label(format!("+ {} more", total - FOLDER_ROWS), cx));
    }
    if let Some(git) = git {
        col = col.child(git_summary(&data.entry, git, t, cx));
    }
    col
}

fn git_summary(entry: &Entry, git: &FolderGit, t: &Theme, cx: &Context<FmApp>) -> Div {
    let mono = |color: Hsla| div().font_family(t.font_mono.clone()).text_size(px(11.5)).text_color(color);
    let commit_row = |commit: &Commit| {
        div()
            .flex()
            .gap(px(8.))
            .child(mono(t.accent).flex_none().child(commit.hash.clone()))
            .child(mono(t.fg).flex_1().min_w_0().truncate().child(commit.subject.clone()))
            .child(mono(t.fg_muted).flex_none().child(commit.age.clone()))
    };
    let modified_row = |name: &SharedString| {
        div()
            .flex()
            .gap(px(8.))
            .child(mono(t.warn).flex_none().child("M"))
            .child(mono(t.fg_muted).flex_1().min_w_0().truncate().child(name.clone()))
    };
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .mt(px(8.))
        .child(label(format!("git · {}/", entry.name), cx).truncate())
        .children(git.commits.iter().take(2).map(commit_row))
        .children(git.modified.iter().take(MODIFIED_ROWS).map(modified_row));
    if git.commits.is_empty() && git.modified.is_empty() {
        col = col.child(mono(t.fg_muted).child("no commits yet"));
    }
    if git.modified.len() > MODIFIED_ROWS {
        col = col.child(label(format!("+ {} more modified", git.modified.len() - MODIFIED_ROWS), cx));
    }
    col
}

/// 2 columns `90px | 1fr`, gap 4 10, mono 11.5, keys muted.
fn meta_grid(meta: &[(SharedString, SharedString)], t: &Theme) -> Div {
    div().flex().flex_col().gap(px(4.)).font_family(t.font_mono.clone()).text_size(px(11.5)).children(meta.iter().map(
        |(key, value)| {
            div()
                .flex()
                .gap(px(10.))
                .child(div().w(px(90.)).flex_none().truncate().text_color(t.fg_muted).child(key.clone()))
                .child(div().flex_1().min_w_0().truncate().child(value.clone()))
        },
    ))
}

fn image_body(
    path: &Path,
    width: u32,
    height: u32,
    file_size: u64,
    meta: &[(SharedString, SharedString)],
    t: &Theme,
) -> Div {
    // The box hugs the image's aspect ratio when the header told us the size.
    let box_w = INNER_W - 2.;
    let box_h = if width > 0 && height > 0 {
        (box_w * height as f32 / width as f32).clamp(48., IMAGE_MAX_H)
    } else {
        150.
    };
    let muted = t.fg_muted;
    let placeholder = move || icon(Icon::Image, 26.).text_color(muted).into_any_element();
    let picture = if file_size <= IMAGE_MAX_BYTES {
        img(path.to_path_buf())
            .size_full()
            .object_fit(ObjectFit::Contain)
            .with_fallback(placeholder)
            .into_any_element()
    } else {
        placeholder()
    };
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .w_full()
                .h(px(box_h + 2.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.bg)
                .border_1()
                .border_color(t.line)
                .overflow_hidden()
                .child(picture),
        )
        .child(meta_grid(meta, t))
}

fn archive_body(
    app: &FmApp,
    entry: &Entry,
    summary: &SharedString,
    entries: &[ArchiveEntry],
    t: &Theme,
    cx: &Context<FmApp>,
) -> Div {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(label(summary.clone(), cx).truncate())
        .children(entries.iter().take(ARCHIVE_ROWS).map(|e| {
            let (ic, name, size): (Icon, SharedString, SharedString) = if e.is_dir {
                (Icon::Folder, format!("{}/", e.name).into(), "".into())
            } else {
                (Icon::File, e.name.clone(), crate::fs_model::format_size(e.size).into())
            };
            // Deep trees stop indenting before the name column disappears.
            entry_row(ic, t.fg_muted, name, size, 14. * e.depth.min(8) as f32, t)
        }));
    if entries.len() > ARCHIVE_ROWS {
        col = col.child(label(format!("+ {} more", entries.len() - ARCHIVE_ROWS), cx));
    }

    let busy = is_extracting(app, &entry.path);
    let small = |b: gpui::Stateful<Div>| b.h(px(26.)).text_size(px(11.5));
    let mut extract =
        small(button("preview-extract", if busy { "Extracting…" } else { "Extract here" }, ButtonKind::Primary, cx));
    if busy || app.preview.extracting.is_some() {
        extract = extract.opacity(0.6).cursor_default();
    } else {
        let archive = entry.path.clone();
        extract = extract.on_click(cx.listener(move |this, _, _, cx| extract_here(this, archive.clone(), cx)));
    }
    // Browse: placeholder — browsing inside archives is not implemented yet.
    let browse = small(button("preview-browse", "Browse", ButtonKind::Secondary, cx));
    col = col.child(div().flex().gap(px(6.)).mt(px(10.)).child(extract).child(browse));

    if let Some((_, message)) = app.preview.extract_error.as_ref().filter(|(path, _)| *path == entry.path) {
        col = col.child(
            div()
                .font_family(t.font_mono.clone())
                .text_size(px(11.5))
                .line_height(relative(1.6))
                .text_color(t.err)
                .child(message.clone()),
        );
    }
    col
}
