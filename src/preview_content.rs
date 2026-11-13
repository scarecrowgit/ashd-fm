//! What a file "looks like": the data model shared by the preview pane (§4.5) and
//! Quick Look (§5), plus the blocking loader that builds it.
//!
//! OWNER: preview agent. The TYPES are a contract with quicklook.rs — extend them
//! (add fields/variants) rather than reshaping them. The loader below is a minimal stub:
//! plain un-highlighted text, folder listing, no archive/markdown/image metadata.

use std::{fs, path::PathBuf};

use gpui::SharedString;

use crate::{
    fs_model::{self, Entry, FileType},
    git::Commit,
};

/// Syntax class → `--syn-*` token. `Plain` is `fg`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Syn {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: SharedString,
    pub syn: Syn,
}

/// One source line as coloured spans (no trailing newline).
pub type Line = Vec<Span>;

#[derive(Clone, Debug)]
pub enum MdBlock {
    /// `level` 1..=6
    Heading { level: u8, text: SharedString },
    Paragraph { text: SharedString },
    ListItem { depth: usize, ordered: Option<u64>, text: SharedString },
    Code { lang: SharedString, lines: Vec<Line> },
    Quote { text: SharedString },
    Rule,
}

#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    /// Last path component.
    pub name: SharedString,
    /// Nesting level (0 = archive root); Quick Look indents 22px per level.
    pub depth: usize,
    pub is_dir: bool,
    /// Uncompressed bytes.
    pub size: u64,
}

#[derive(Clone, Debug)]
pub enum PreviewBody {
    /// Children sorted folders-first by name. `git` is set when the folder is a repo root.
    Folder { items: Vec<Entry>, git: Option<FolderGit> },
    /// Code or plain text. `truncated` when the file was longer than the cap.
    Text { lines: Vec<Line>, truncated: bool },
    /// `source_lines` is the highlighted raw text (preview pane); `blocks` the rendered form (Quick Look).
    Markdown { blocks: Vec<MdBlock>, source_lines: Vec<Line> },
    /// `meta` rows for the pane's grid: size (`1400 × 200`), format, colours, created.
    Image { path: PathBuf, width: u32, height: u32, format: SharedString, meta: Vec<(SharedString, SharedString)> },
    /// `summary` like `zstd · 2.8M → 11.4M · 214 files`.
    Archive { summary: SharedString, packed: u64, unpacked: u64, files: usize, entries: Vec<ArchiveEntry> },
    /// Video / audio / PDF / unknown binary: just facts, e.g. ("duration", "3:12"), ("pages", "14").
    Other { meta: Vec<(SharedString, SharedString)> },
    Error(SharedString),
}

#[derive(Clone, Debug)]
pub struct FolderGit {
    pub commits: Vec<Commit>,
    /// Modified file names relative to the folder.
    pub modified: Vec<SharedString>,
}

#[derive(Clone, Debug)]
pub struct PreviewData {
    pub entry: Entry,
    /// Label line under the name: `markdown · 2.1K · rw-r--r--` or `folder · 12 items · 13 Nov 12:47`.
    pub meta: SharedString,
    pub body: PreviewBody,
}

/// Longest text we load for preview.
pub const MAX_TEXT_BYTES: u64 = 512 * 1024;
pub const MAX_TEXT_LINES: usize = 2000;

/// Build the preview for `entry`. BLOCKING (disk, maybe subprocesses): background executor only.
pub fn load(entry: &Entry) -> PreviewData {
    let meta: SharedString = if entry.is_dir {
        format!(
            "folder · {} items · {}",
            entry.child_count.unwrap_or(0),
            fs_model::format_mtime(entry.modified)
        )
        .into()
    } else {
        format!(
            "{} · {} · {}",
            entry.file_type.label(),
            fs_model::format_size(entry.size),
            fs_model::format_perms(entry.mode)
        )
        .into()
    };

    let body = match entry.file_type {
        FileType::Folder => match fs_model::read_dir(&entry.path) {
            Ok(mut items) => {
                let sort = fs_model::SortSpec::default();
                items.retain(|e| !e.hidden);
                items.sort_by(|a, b| sort.compare(a, b));
                PreviewBody::Folder { items, git: None }
            }
            Err(err) => PreviewBody::Error(format!("{err:?}").into()),
        },
        FileType::Text | FileType::Code | FileType::Markdown => load_plain_text(entry),
        FileType::Image => PreviewBody::Image {
            path: entry.path.clone(),
            width: 0,
            height: 0,
            format: entry.extension().unwrap_or("").to_uppercase().into(),
            meta: Vec::new(),
        },
        _ => PreviewBody::Other { meta: Vec::new() },
    };
    PreviewData { entry: entry.clone(), meta, body }
}

fn load_plain_text(entry: &Entry) -> PreviewBody {
    use std::io::Read;
    let mut buf = Vec::new();
    let read = fs::File::open(&entry.path).and_then(|f| f.take(MAX_TEXT_BYTES).read_to_end(&mut buf));
    if let Err(err) = read {
        return PreviewBody::Error(err.to_string().into());
    }
    if buf.contains(&0) {
        return PreviewBody::Other { meta: Vec::new() };
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<Line> = text
        .lines()
        .take(MAX_TEXT_LINES)
        .map(|l| vec![Span { text: l.replace('\t', "    ").into(), syn: Syn::Plain }])
        .collect();
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    PreviewBody::Text { truncated: entry.size > MAX_TEXT_BYTES || text.lines().count() > MAX_TEXT_LINES, lines }
}
