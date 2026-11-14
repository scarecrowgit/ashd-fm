//! What a file "looks like": the data model shared by the preview pane (§4.5) and
//! Quick Look (§5), plus the blocking loader that builds it.
//!
//! OWNER: preview agent. The TYPES are a contract with quicklook.rs — extend them
//! (add fields/variants) rather than reshaping them.
//!
//! Everything below the types is pure std + a few small crates: a hand-written tokenizer
//! (`highlight`), markdown block extraction (pulldown-cmark), image header sniffing
//! (imagesize), archive listing / extraction (zip, tar + flate2/zstd/xz2/bzip2) and a couple
//! of subprocess probes (`git`, `ffprobe`, `pdfinfo`) that run under a hard timeout.

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    io::{self, Read},
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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
    /// Folders only: number of non-hidden children before `MAX_DIR_ITEMS` was applied to
    /// `PreviewBody::Folder::items` (0 for everything else).
    pub folder_total: usize,
}

/// Longest text we load for preview.
pub const MAX_TEXT_BYTES: u64 = 512 * 1024;
pub const MAX_TEXT_LINES: usize = 2000;
/// Longest single line kept (characters); the rest of the line is dropped.
pub const MAX_LINE_CHARS: usize = 400;
/// Folder children kept in `PreviewBody::Folder::items`.
pub const MAX_DIR_ITEMS: usize = 500;
/// Entries kept in `PreviewBody::Archive::entries`.
pub const MAX_ARCHIVE_ENTRIES: usize = 2000;

/// Directory names scanned before we stop looking (keeps `/proc`-like monsters cheap).
const MAX_DIR_SCAN: usize = 50_000;
/// Raw archive members fed into the tree builder / counted at all.
const MAX_ARCHIVE_TREE: usize = 20_000;
const MAX_ARCHIVE_SCAN: usize = 500_000;
/// Wall-clock budget for streaming through a (compressed) tar.
const ARCHIVE_BUDGET: Duration = Duration::from_millis(1500);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);
const GIT_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_MD_BLOCKS: usize = 2000;

// ───────────────────────────────────────── entry point ─────────────────────────────────────────

/// Build the preview for `entry`. BLOCKING (disk, maybe subprocesses): background executor only.
/// Never panics: any internal failure becomes `PreviewBody::Error`.
pub fn load(entry: &Entry) -> PreviewData {
    let meta: SharedString = meta_line(entry).into();
    let (body, folder_total) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_body(entry)))
        .unwrap_or_else(|_| (PreviewBody::Error("preview failed".into()), 0));
    PreviewData { entry: entry.clone(), meta, body, folder_total }
}

/// `type · size · perms` for files, `folder · n items · mtime` for folders (SPEC §4.5; the
/// folder size is left out because computing it means walking the tree).
pub fn meta_line(entry: &Entry) -> String {
    if entry.is_dir {
        let count = match entry.child_count {
            Some(1) => "1 item".to_string(),
            Some(n) => format!("{n} items"),
            None => "— items".to_string(),
        };
        format!("folder · {count} · {}", fs_model::format_mtime(entry.modified))
    } else {
        format!(
            "{} · {} · {}",
            entry.file_type.label(),
            fs_model::format_size(entry.size),
            fs_model::format_perms(entry.mode)
        )
    }
}

fn load_body(entry: &Entry) -> (PreviewBody, usize) {
    let md = match fs::metadata(&entry.path) {
        Ok(md) => md,
        Err(err) => {
            let msg = if entry.is_symlink { "broken link".to_string() } else { io_message(&err) };
            return (PreviewBody::Error(msg.into()), 0);
        }
    };
    if md.is_dir() {
        return load_folder(entry);
    }
    if !md.is_file() {
        // FIFOs, sockets and devices are never opened: reading them can block forever.
        let ft = md.file_type();
        let kind = if ft.is_fifo() {
            "fifo"
        } else if ft.is_socket() {
            "socket"
        } else if ft.is_char_device() {
            "character device"
        } else if ft.is_block_device() {
            "block device"
        } else {
            "special file"
        };
        let mut meta = base_meta(entry);
        if let Some(first) = meta.first_mut() {
            first.1 = kind.into();
        }
        return (PreviewBody::Other { meta }, 0);
    }
    let body = match entry.file_type {
        FileType::Folder => return load_folder(entry),
        FileType::Text | FileType::Code | FileType::Markdown => load_text(entry),
        FileType::Image => load_image(entry, &md),
        FileType::Archive => load_archive(entry, md.len()),
        // `.txz`, `.tbz2`, `.jar` …: archives fs_model does not classify as such.
        FileType::Binary if ArchiveFormat::detect(&entry.name).is_some() => load_archive(entry, md.len()),
        FileType::Audio | FileType::Video => load_media(entry),
        FileType::Pdf => load_pdf(entry),
        FileType::Binary => {
            if md.len() > 0 && sniff_is_text(&entry.path) {
                load_text(entry)
            } else {
                PreviewBody::Other { meta: base_meta(entry) }
            }
        }
    };
    (body, 0)
}

fn io_message(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::PermissionDenied => "permission denied".into(),
        io::ErrorKind::NotFound => "no longer exists".into(),
        _ => err.to_string(),
    }
}

fn row(k: &str, v: impl Into<SharedString>) -> (SharedString, SharedString) {
    (SharedString::from(k.to_string()), v.into())
}

/// Facts every file has: type, size, modified, permissions.
fn base_meta(entry: &Entry) -> Vec<(SharedString, SharedString)> {
    let kind = match (entry.file_type, entry.extension()) {
        (FileType::Binary, Some(ext)) => format!("{} file", ext.to_lowercase()),
        (FileType::Binary, None) => "binary".to_string(),
        (ft, Some(ext)) => format!("{} · {}", ft.label(), ext.to_lowercase()),
        (ft, None) => ft.label().to_string(),
    };
    vec![
        row("type", kind),
        row("size", fs_model::format_size(entry.size)),
        row("modified", fs_model::format_mtime(entry.modified)),
        row("perms", fs_model::format_perms(entry.mode)),
    ]
}

// ───────────────────────────────────────── subprocesses ────────────────────────────────────────

/// Run a command and return its stdout, or `None` if it is missing, fails, prints more than
/// 256K or does not finish within `timeout` (it is killed then).
fn run_with_timeout(program: &str, args: &[&OsStr], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(256 * 1024).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {}
            Err(_) => break false,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(4));
    };
    let buf = reader.join().ok()?;
    ok.then(|| String::from_utf8_lossy(&buf).into_owned())
}

// ───────────────────────────────────────────── folder ──────────────────────────────────────────

fn load_folder(entry: &Entry) -> (PreviewBody, usize) {
    let rd = match fs::read_dir(&entry.path) {
        Ok(rd) => rd,
        Err(err) => return (PreviewBody::Error(io_message(&err).into()), 0),
    };
    // Names + "is it a directory" only: no per-child stat until we know which ones we keep.
    let mut names: Vec<(bool, String, PathBuf)> = Vec::new();
    let mut has_git = false;
    for dirent in rd.take(MAX_DIR_SCAN).filter_map(|e| e.ok()) {
        let name = dirent.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            has_git = true;
        }
        if name.starts_with('.') {
            continue;
        }
        let is_dir = match dirent.file_type() {
            Ok(ft) if ft.is_symlink() => fs::metadata(dirent.path()).map(|m| m.is_dir()).unwrap_or(false),
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };
        names.push((is_dir, name, dirent.path()));
    }
    names.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| fs_model::natural_cmp(&a.1, &b.1)));
    let total = names.len();
    let items: Vec<Entry> =
        names.iter().take(MAX_DIR_ITEMS).filter_map(|(_, _, path)| Entry::from_path(path)).collect();
    let git = if has_git { folder_git(&entry.path) } else { None };
    (PreviewBody::Folder { items, git }, total)
}

fn folder_git(dir: &Path) -> Option<FolderGit> {
    let dir_arg = dir.as_os_str();
    let log = run_with_timeout(
        "git",
        &[OsStr::new("-C"), dir_arg, OsStr::new("log"), OsStr::new("-2"), OsStr::new("--format=%h%x09%ct%x09%s")],
        GIT_TIMEOUT,
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let commits: Vec<Commit> = log.as_deref().map(|out| parse_git_log(out, now)).unwrap_or_default();
    let status = run_with_timeout(
        "git",
        &[
            OsStr::new("-C"),
            dir_arg,
            OsStr::new("-c"),
            OsStr::new("core.quotepath=off"),
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("--untracked-files=no"),
        ],
        GIT_TIMEOUT,
    );
    let modified = status.as_deref().map(parse_git_status).unwrap_or_default();
    // A fresh repo has no commits (log fails) but is still a repo if status worked.
    if log.is_none() && status.is_none() {
        return None;
    }
    Some(FolderGit { commits, modified })
}

fn parse_git_log(out: &str, now_secs: u64) -> Vec<Commit> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let hash = parts.next()?.trim();
            let time: u64 = parts.next()?.trim().parse().ok()?;
            let subject = parts.next().unwrap_or("").trim();
            if hash.is_empty() {
                return None;
            }
            Some(Commit {
                hash: hash.to_string().into(),
                subject: subject.to_string().into(),
                age: relative_age(now_secs.saturating_sub(time)).into(),
            })
        })
        .take(2)
        .collect()
}

/// Paths with an `M` in either porcelain column.
fn parse_git_status(out: &str) -> Vec<SharedString> {
    out.lines()
        .filter(|l| l.len() > 3 && l.is_char_boundary(3) && l.as_bytes()[..2].contains(&b'M'))
        .map(|l| l[3..].trim_matches('"').to_string().into())
        .take(50)
        .collect()
}

/// `now`, `12m`, `2h`, `3d`, `5w`, `7mo`, `2y`.
pub fn relative_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < MIN => "now".into(),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 14 * DAY => format!("{}d", s / DAY),
        s if s < 60 * DAY => format!("{}w", s / (7 * DAY)),
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

// ────────────────────────────────────────────── text ───────────────────────────────────────────

fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    fs::File::open(path)?.take(cap).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Unknown extension: text if the first 8K has no NUL and is (almost entirely) valid UTF-8.
fn sniff_is_text(path: &Path) -> bool {
    let Ok(head) = read_capped(path, 8192) else { return false };
    if head.is_empty() || head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(&head) {
        Ok(_) => true,
        // A multi-byte character cut by the 8K boundary is fine.
        Err(err) => err.error_len().is_none() && err.valid_up_to() + 4 > head.len(),
    }
}

fn load_text(entry: &Entry) -> PreviewBody {
    let buf = match read_capped(&entry.path, MAX_TEXT_BYTES) {
        Ok(buf) => buf,
        Err(err) => return PreviewBody::Error(io_message(&err).into()),
    };
    if buf.contains(&0) {
        return PreviewBody::Other { meta: base_meta(entry) };
    }
    let text = String::from_utf8_lossy(&buf);
    let byte_capped = buf.len() as u64 >= MAX_TEXT_BYTES;
    let lang = lang_for_file(&entry.name, &text);
    let (mut lines, line_capped) = highlight_lang(lang, &text);
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    if entry.file_type == FileType::Markdown {
        return PreviewBody::Markdown { blocks: markdown_blocks(&text), source_lines: lines };
    }
    PreviewBody::Text { lines, truncated: byte_capped || line_capped }
}

// ──────────────────────────────────────────── tokenizer ────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Code,
    /// `key = value`, `[section]`
    Toml,
    Yaml,
    Json,
    /// html / xml
    Markup,
    Css,
    Kdl,
    Markdown,
    Plain,
}

struct Lang {
    kind: Kind,
    keywords: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// (delimiter, backslash escapes, may span lines)
    quotes: &'static [(&'static str, bool, bool)],
    /// Long string delimiters (open, close), always multi-line, no escapes.
    long_strings: &'static [(&'static str, &'static str)],
    /// `'` only starts a character literal (`'a'`, `'\n'`), otherwise it is a lifetime tick.
    char_tick: bool,
    /// Keywords match case-insensitively.
    nocase: bool,
    /// `-` is an identifier character.
    dash_idents: bool,
}

const BASE: Lang = Lang {
    kind: Kind::Code,
    keywords: &[],
    line_comments: &[],
    block_comment: None,
    quotes: Q_STD,
    long_strings: &[],
    char_tick: false,
    nocase: false,
    dash_idents: false,
};

const Q_STD: &[(&str, bool, bool)] = &[("\"", true, false), ("'", true, false)];
const Q_JS: &[(&str, bool, bool)] = &[("\"", true, false), ("'", true, false), ("`", true, true)];
const Q_GO: &[(&str, bool, bool)] = &[("\"", true, false), ("'", true, false), ("`", false, true)];
const Q_DOUBLE_ML: &[(&str, bool, bool)] = &[("\"", true, true)];
const Q_DOUBLE: &[(&str, bool, bool)] = &[("\"", true, false)];
const Q_SHELL: &[(&str, bool, bool)] = &[("\"", true, true), ("'", false, true)];
const Q_TOML: &[(&str, bool, bool)] = &[("\"", true, false), ("'", false, false)];
const Q_SQL: &[(&str, bool, bool)] = &[("'", false, true), ("\"", false, false)];
const C_BLOCK: Option<(&str, &str)> = Some(("/*", "*/"));
const TRIPLE: &[(&str, &str)] = &[("\"\"\"", "\"\"\""), ("'''", "'''")];

static PLAIN: Lang = Lang { kind: Kind::Plain, ..BASE };
static MARKDOWN: Lang = Lang { kind: Kind::Markdown, ..BASE };
static MARKUP: Lang = Lang { kind: Kind::Markup, ..BASE };

static RUST: Lang = Lang {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self",
        "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    ],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: Q_DOUBLE_ML,
    long_strings: &[("r#\"", "\"#"), ("r\"", "\"")],
    char_tick: true,
    ..BASE
};

static PYTHON: Lang = Lang {
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
        "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not",
        "or", "pass", "raise", "return", "True", "try", "while", "with", "yield", "match", "case", "self",
    ],
    line_comments: &["#"],
    long_strings: TRIPLE,
    ..BASE
};

static JS: Lang = Lang {
    keywords: &[
        "abstract", "any", "as", "async", "await", "boolean", "break", "case", "catch", "class", "const", "continue",
        "debugger", "declare", "default", "delete", "do", "else", "enum", "export", "extends", "false", "finally",
        "for", "from", "function", "if", "implements", "import", "in", "instanceof", "interface", "keyof", "let",
        "namespace", "new", "null", "number", "of", "private", "protected", "public", "readonly", "return", "static",
        "string", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined", "var", "void",
        "while", "yield",
    ],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: Q_JS,
    ..BASE
};

static C: Lang = Lang {
    keywords: &[
        "auto", "bool", "break", "case", "catch", "char", "class", "const", "constexpr", "continue", "default",
        "define", "delete", "do", "double", "else", "enum", "explicit", "extern", "false", "float", "for", "friend",
        "goto", "if", "ifdef", "ifndef", "endif", "include", "inline", "int", "long", "namespace", "new", "noexcept",
        "nullptr", "NULL", "operator", "override", "private", "protected", "public", "register", "return", "short",
        "signed", "sizeof", "static", "struct", "switch", "template", "this", "throw", "true", "try", "typedef",
        "typename", "union", "unsigned", "using", "virtual", "void", "volatile", "while",
    ],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    ..BASE
};

static GO: Lang = Lang {
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "false", "for",
        "func", "go", "goto", "if", "import", "interface", "map", "nil", "package", "range", "return", "select",
        "struct", "switch", "true", "type", "var",
    ],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: Q_GO,
    ..BASE
};

/// java / kotlin / swift / zig / php / c# / dart: one merged keyword set is good enough.
static CLIKE: Lang = Lang {
    keywords: &[
        "abstract", "as", "async", "await", "boolean", "break", "case", "catch", "class", "comptime", "const",
        "continue", "data", "default", "defer", "do", "echo", "else", "enum", "extends", "extension", "false", "final",
        "finally", "fn", "for", "foreach", "fun", "func", "function", "guard", "if", "implements", "import", "in",
        "init", "instanceof", "interface", "internal", "is", "let", "namespace", "new", "nil", "null", "object",
        "override", "package", "private", "protected", "protocol", "pub", "public", "return", "self", "static",
        "struct", "super", "switch", "this", "throw", "throws", "true", "try", "typealias", "use", "val", "var",
        "void", "when", "where", "while",
    ],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    ..BASE
};

static LUA: Lang = Lang {
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
        "not", "or", "repeat", "return", "then", "true", "until", "while",
    ],
    line_comments: &["--"],
    block_comment: Some(("--[[", "]]")),
    long_strings: &[("[[", "]]")],
    ..BASE
};

static SHELL: Lang = Lang {
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function",
        "return", "exit", "local", "export", "readonly", "declare", "set", "unset", "source", "alias", "echo", "cd",
        "exec", "eval", "trap", "shift", "break", "continue", "begin", "end", "switch", "true", "false",
    ],
    line_comments: &["#"],
    quotes: Q_SHELL,
    ..BASE
};

static NIX: Lang = Lang {
    keywords: &[
        "assert", "else", "false", "if", "import", "in", "inherit", "let", "null", "or", "rec", "then", "true", "with",
    ],
    line_comments: &["#"],
    block_comment: C_BLOCK,
    quotes: Q_DOUBLE_ML,
    long_strings: &[("''", "''")],
    dash_idents: true,
    ..BASE
};

static TOML: Lang = Lang {
    kind: Kind::Toml,
    keywords: &["true", "false"],
    line_comments: &["#"],
    quotes: Q_TOML,
    long_strings: TRIPLE,
    ..BASE
};

static INI: Lang = Lang {
    kind: Kind::Toml,
    keywords: &["true", "false", "yes", "no", "on", "off"],
    line_comments: &["#", ";"],
    ..BASE
};

static YAML: Lang = Lang {
    kind: Kind::Yaml,
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comments: &["#"],
    ..BASE
};

static JSON: Lang = Lang {
    kind: Kind::Json,
    keywords: &["true", "false", "null"],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: Q_DOUBLE,
    ..BASE
};

static CSS: Lang = Lang { kind: Kind::Css, block_comment: C_BLOCK, dash_idents: true, ..BASE };
static SCSS: Lang =
    Lang { kind: Kind::Css, line_comments: &["//"], block_comment: C_BLOCK, dash_idents: true, ..BASE };

static KDL: Lang = Lang {
    kind: Kind::Kdl,
    keywords: &["true", "false", "null"],
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: Q_DOUBLE_ML,
    dash_idents: true,
    ..BASE
};

static SQL: Lang = Lang {
    keywords: &[
        "add", "all", "alter", "and", "as", "asc", "begin", "between", "by", "case", "check", "column", "commit",
        "constraint", "create", "cross", "database", "default", "delete", "desc", "distinct", "drop", "else", "end",
        "exists", "false", "foreign", "from", "full", "group", "having", "if", "in", "index", "inner", "insert", "into",
        "is", "join", "key", "left", "like", "limit", "not", "null", "offset", "on", "or", "order", "outer", "primary",
        "references", "returning", "right", "rollback", "select", "set", "table", "then", "true", "union", "unique",
        "update", "values", "view", "when", "where", "with",
    ],
    line_comments: &["--"],
    block_comment: C_BLOCK,
    quotes: Q_SQL,
    nocase: true,
    ..BASE
};

static RUBY: Lang = Lang {
    keywords: &[
        "alias", "and", "begin", "break", "case", "class", "def", "defmodule", "defp", "do", "else", "elsif", "end",
        "ensure", "false", "fn", "for", "if", "import", "in", "module", "next", "nil", "not", "or", "require",
        "rescue", "return", "self", "then", "true", "unless", "until", "use", "when", "while", "yield",
    ],
    line_comments: &["#"],
    ..BASE
};

static HASKELL: Lang = Lang {
    keywords: &[
        "case", "class", "data", "deriving", "do", "else", "if", "import", "in", "infixl", "infixr", "instance", "let",
        "module", "newtype", "of", "qualified", "then", "type", "where",
    ],
    line_comments: &["--"],
    block_comment: Some(("{-", "-}")),
    quotes: Q_DOUBLE,
    ..BASE
};

static LISP: Lang = Lang { line_comments: &[";"], quotes: Q_DOUBLE_ML, dash_idents: true, ..BASE };

/// Makefiles, Dockerfiles, dotfiles: `#` comments, strings and numbers.
static HASH: Lang = Lang { line_comments: &["#"], ..BASE };

/// Language for an extension, a file name or a fenced-code info string (lower-case).
fn lang_by_tag(tag: &str) -> Option<&'static Lang> {
    Some(match tag {
        "rs" | "rust" => &RUST,
        "py" | "python" | "pyi" | "python3" => &PYTHON,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "javascript" | "typescript" => &JS,
        "c" | "h" | "cpp" | "hpp" | "cc" | "hh" | "cxx" | "c++" | "glsl" | "wgsl" => &C,
        "go" | "golang" => &GO,
        "java" | "kt" | "kts" | "kotlin" | "swift" | "zig" | "php" | "cs" | "dart" | "scala" | "groovy" | "gradle" => {
            &CLIKE
        }
        "lua" => &LUA,
        "sh" | "bash" | "zsh" | "fish" | "shell" | "console" | "ksh" | "pkgbuild" | "bashrc" | "zshrc" | "profile"
        | "zprofile" | "zshenv" | "bash_profile" | "envrc" => &SHELL,
        "nix" => &NIX,
        "toml" => &TOML,
        "ini" | "conf" | "cfg" | "desktop" | "service" | "editorconfig" | "gitconfig" | "properties" | "env" => &INI,
        "yaml" | "yml" => &YAML,
        "json" | "jsonc" | "json5" | "webmanifest" => &JSON,
        "css" => &CSS,
        "scss" | "sass" | "less" => &SCSS,
        "html" | "htm" | "xml" | "xhtml" | "svg" | "vue" | "svelte" | "plist" | "rss" => &MARKUP,
        "kdl" => &KDL,
        "sql" | "psql" | "sqlite" => &SQL,
        "rb" | "ruby" | "ex" | "exs" | "elixir" | "gemfile" | "rakefile" => &RUBY,
        "hs" | "haskell" => &HASKELL,
        "el" | "lisp" | "clj" | "scm" => &LISP,
        "md" | "markdown" => &MARKDOWN,
        "mk" | "cmake" | "dockerfile" | "makefile" | "gitignore" | "gitattributes" | "dockerignore" | "justfile"
        | "containerfile" | "make" | "docker" | "ignore" => &HASH,
        _ => return None,
    })
}

fn lang_for_file(name: &str, text: &str) -> &'static Lang {
    let lower = name.to_lowercase();
    let bare = lower.trim_start_matches('.');
    let ext = bare.rsplit_once('.').map(|(_, e)| e);
    if ext == Some("lock") {
        // flake.lock / package-lock are JSON, Cargo.lock / poetry.lock are TOML.
        return if text.trim_start().starts_with('{') { &JSON } else { &TOML };
    }
    if let Some(lang) = ext.and_then(lang_by_tag).or_else(|| lang_by_tag(bare)) {
        return lang;
    }
    // Shebang.
    let first = text.lines().next().unwrap_or("");
    if let Some(rest) = first.strip_prefix("#!") {
        let interp = rest.split_whitespace().rfind(|w| !w.starts_with('-')).unwrap_or("");
        let interp = interp.rsplit('/').next().unwrap_or("");
        let interp = interp.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        if let Some(lang) = lang_by_tag(interp).or_else(|| lang_by_tag(match interp {
            "node" | "deno" | "bun" => "js",
            "dash" | "ash" => "sh",
            other => other,
        })) {
            return lang;
        }
    }
    &PLAIN
}

/// Highlight `text` as the language named by `lang` (an extension like `rs` or a fenced-code
/// info string like `rust`); unknown names give all-`Plain` lines. Tabs become 4 spaces;
/// at most `MAX_TEXT_LINES` lines of at most `MAX_LINE_CHARS` characters are returned.
pub fn highlight(lang: &str, text: &str) -> Vec<Line> {
    let tag = lang.trim().to_lowercase();
    let tag = tag.split(|c: char| c == ',' || c == '{' || c.is_whitespace()).next().unwrap_or("");
    highlight_lang(lang_by_tag(tag).unwrap_or(&PLAIN), text).0
}

/// Returns the lines and whether `MAX_TEXT_LINES` cut the text short.
fn highlight_lang(lang: &Lang, text: &str) -> (Vec<Line>, bool) {
    let mut state = TokState::default();
    let mut lines = Vec::new();
    let mut capped = false;
    for (ix, raw) in text.lines().enumerate() {
        if ix >= MAX_TEXT_LINES {
            capped = true;
            break;
        }
        let clipped = match raw.char_indices().nth(MAX_LINE_CHARS) {
            Some((end, _)) => &raw[..end],
            None => raw,
        };
        let clean: std::borrow::Cow<str> = if clipped.contains(['\t', '\r']) {
            clipped.replace('\t', "    ").replace('\r', "").into()
        } else {
            clipped.into()
        };
        lines.push(tokenize_line(lang, &clean, &mut state));
    }
    (lines, capped)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Mode {
    #[default]
    Normal,
    /// Inside a block comment that ends with the given delimiter.
    Block(&'static str),
    /// Inside a multi-line string.
    Str { close: &'static str, esc: bool },
    /// Inside a markdown fenced code block.
    Fence,
}

#[derive(Clone, Copy, Default)]
struct TokState {
    mode: Mode,
    /// Markup: between `<name` and `>`.
    in_tag: bool,
}

#[derive(Default)]
struct Out {
    spans: Vec<(Syn, String)>,
}

impl Out {
    fn push(&mut self, syn: Syn, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some((last, buf)) if *last == syn => buf.push_str(text),
            _ => self.spans.push((syn, text.to_string())),
        }
    }

    fn finish(self) -> Line {
        self.spans.into_iter().map(|(syn, text)| Span { text: text.into(), syn }).collect()
    }
}

/// Index just past the closing delimiter of a string whose body starts at `from`.
fn find_close(s: &str, from: usize, close: &str, esc: bool) -> Option<usize> {
    let mut it = s[from..].char_indices();
    while let Some((i, c)) = it.next() {
        if esc && c == '\\' {
            it.next();
            continue;
        }
        if s[from + i..].starts_with(close) {
            return Some(from + i + close.len());
        }
    }
    None
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char, dash: bool) -> bool {
    c.is_alphanumeric() || c == '_' || (dash && c == '-')
}

fn tokenize_line(lang: &Lang, line: &str, st: &mut TokState) -> Line {
    let mut out = Out::default();
    match lang.kind {
        Kind::Plain => {
            out.push(Syn::Plain, line);
            return out.finish();
        }
        Kind::Markdown => {
            markdown_source_line(line, st, &mut out);
            return out.finish();
        }
        _ => {}
    }

    // Finish a construct left open by an earlier line.
    let mut i = 0;
    match st.mode {
        Mode::Block(end) => match line.find(end) {
            Some(p) => {
                i = p + end.len();
                out.push(Syn::Comment, &line[..i]);
                st.mode = Mode::Normal;
            }
            None => {
                out.push(Syn::Comment, line);
                return out.finish();
            }
        },
        Mode::Str { close, esc } => match find_close(line, 0, close, esc) {
            Some(end) => {
                i = end;
                out.push(Syn::String, &line[..i]);
                st.mode = Mode::Normal;
            }
            None => {
                out.push(Syn::String, line);
                return out.finish();
            }
        },
        Mode::Normal | Mode::Fence => {}
    }

    if lang.kind == Kind::Markup {
        markup_line(line, i, st, &mut out);
        return out.finish();
    }
    if i == 0 {
        i = match lang.kind {
            Kind::Toml => toml_prefix(line, &mut out),
            Kind::Yaml => yaml_prefix(line, &mut out),
            Kind::Kdl => kdl_prefix(line, &mut out),
            _ => 0,
        };
    }
    let css_selector_line = lang.kind == Kind::Css && line.trim_end().ends_with(['{', ',']);

    'scan: while i < line.len() {
        let rest = &line[i..];
        let Some(c) = rest.chars().next() else { break };
        let prev = line[..i].chars().next_back();
        let after_ident = prev.is_some_and(|p| is_ident_char(p, lang.dash_idents));

        // Block comment (before line comments: lua's `--[[` starts with `--`).
        if let Some((open, close)) = lang.block_comment
            && rest.starts_with(open)
        {
            match rest[open.len()..].find(close) {
                Some(p) => {
                    let end = i + open.len() + p + close.len();
                    out.push(Syn::Comment, &line[i..end]);
                    i = end;
                }
                None => {
                    out.push(Syn::Comment, rest);
                    st.mode = Mode::Block(close);
                    break 'scan;
                }
            }
            continue;
        }
        // Line comment. `#` must start a word (`$#`, `${x#y}`, `a#b` are not comments).
        for marker in lang.line_comments {
            if rest.starts_with(marker) && (*marker != "#" || prev.is_none_or(|p| p.is_whitespace())) {
                out.push(Syn::Comment, rest);
                break 'scan;
            }
        }
        // Long strings (`"""`, `''`, `[[`, `r#"`).
        for (open, close) in lang.long_strings {
            if rest.starts_with(open) && !(open.starts_with(|c: char| c.is_alphabetic()) && after_ident) {
                match rest[open.len()..].find(close) {
                    Some(p) => {
                        let end = i + open.len() + p + close.len();
                        out.push(Syn::String, &line[i..end]);
                        i = end;
                    }
                    None => {
                        out.push(Syn::String, rest);
                        st.mode = Mode::Str { close, esc: false };
                        break 'scan;
                    }
                }
                continue 'scan;
            }
        }
        // Rust: `'a'` / `'\n'` are literals, `'a` is a lifetime.
        if lang.char_tick && c == '\'' {
            let mut chars = rest[1..].char_indices();
            let end = match chars.next() {
                Some((_, '\\')) => rest[1..].char_indices().skip(2).take(10).find(|(_, c)| *c == '\'').map(|(p, _)| p + 2),
                Some((_, ch)) if ch != '\'' => match chars.next() {
                    Some((p, '\'')) => Some(p + 2),
                    _ => None,
                },
                _ => None,
            };
            match end {
                Some(len) => {
                    out.push(Syn::String, &rest[..len]);
                    i += len;
                }
                None => {
                    out.push(Syn::Plain, "'");
                    i += 1;
                }
            }
            continue;
        }
        // Ordinary strings.
        for (quote, esc, multiline) in lang.quotes {
            if !rest.starts_with(quote) {
                continue;
            }
            let (end, closed) = match find_close(line, i + quote.len(), quote, *esc) {
                Some(end) => (end, true),
                None => (line.len(), false),
            };
            // JSON: a string followed by `:` is a key.
            let syn = if lang.kind == Kind::Json && closed && line[end..].trim_start().starts_with(':') {
                Syn::Keyword
            } else {
                Syn::String
            };
            out.push(syn, &line[i..end]);
            i = end;
            if !closed && *multiline {
                st.mode = Mode::Str { close: quote, esc: *esc };
            }
            continue 'scan;
        }
        // Numbers (with suffixes / units: `0xff`, `1_000u64`, `12px`, `1.5`).
        if c.is_ascii_digit() {
            let mut end = i;
            let mut chars = rest.char_indices().peekable();
            while let Some((p, ch)) = chars.next() {
                let ok = ch.is_ascii_alphanumeric()
                    || ch == '_'
                    || ch == '%'
                    || (ch == '.' && chars.peek().is_some_and(|(_, n)| n.is_ascii_digit()));
                if !ok {
                    break;
                }
                end = i + p + ch.len_utf8();
            }
            out.push(Syn::Number, &line[i..end]);
            i = end;
            continue;
        }
        if lang.kind == Kind::Css {
            // `#fff` colours and `@media` rules.
            if c == '#' {
                let len = rest[1..].chars().take_while(|ch| ch.is_ascii_hexdigit()).count();
                let word_end = rest[1 + len..].chars().next().is_none_or(|ch| !is_ident_char(ch, true));
                if matches!(len, 3 | 4 | 6 | 8) && word_end {
                    out.push(Syn::Number, &rest[..1 + len]);
                    i += 1 + len;
                    continue;
                }
            }
            if c == '@' {
                let len: usize = rest[1..].chars().take_while(|ch| is_ident_char(*ch, true)).map(char::len_utf8).sum();
                out.push(Syn::Keyword, &rest[..1 + len]);
                i += 1 + len;
                continue;
            }
        }
        // Identifiers / keywords.
        let dash_start = lang.dash_idents && c == '-' && rest[1..].starts_with(|n: char| is_ident_start(n) || n == '-');
        if is_ident_start(c) || dash_start {
            let len: usize =
                rest.chars().take_while(|ch| is_ident_char(*ch, lang.dash_idents)).map(char::len_utf8).sum();
            let word = &rest[..len];
            let is_keyword = if lang.nocase {
                lang.keywords.iter().any(|k| k.eq_ignore_ascii_case(word))
            } else {
                lang.keywords.contains(&word)
            };
            let css_property = lang.kind == Kind::Css && !css_selector_line && rest[len..].starts_with(':');
            out.push(if is_keyword || css_property { Syn::Keyword } else { Syn::Plain }, word);
            i += len;
            continue;
        }
        out.push(Syn::Plain, &rest[..c.len_utf8()]);
        i += c.len_utf8();
    }
    out.finish()
}

/// `[section]` headers and the key of `key = value`. Returns where normal scanning resumes.
fn toml_prefix(line: &str, out: &mut Out) -> usize {
    let trimmed = line.trim_start();
    let lead = line.len() - trimmed.len();
    if trimmed.starts_with('[') {
        let body = trimmed.split(" #").next().unwrap_or(trimmed).trim_end();
        // `[a.b]` / `[[bin]]` — but not an inline array like `["a", "b"]`.
        if body.ends_with(']') && !body.contains([',', '=']) && !body[1..].trim_start().starts_with(['"', '\'']) {
            out.push(Syn::Plain, &line[..lead]);
            out.push(Syn::Keyword, body);
            return lead + body.len();
        }
        return 0;
    }
    let Some(eq) = trimmed.find('=') else { return 0 };
    let key = trimmed[..eq].trim_end();
    let valid = !key.is_empty()
        && key.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '"' | '\'' | ' ' | '/' | ':'));
    if !valid {
        return 0;
    }
    out.push(Syn::Plain, &line[..lead]);
    out.push(Syn::Keyword, key);
    lead + key.len()
}

/// The key of `key: value` (after indentation and `- ` markers).
fn yaml_prefix(line: &str, out: &mut Out) -> usize {
    let mut start = line.len() - line.trim_start().len();
    while line[start..].starts_with("- ") {
        start += 2;
        start += line[start..].len() - line[start..].trim_start().len();
    }
    let rest = &line[start..];
    if rest.starts_with(['#', '"', '\'', '[', '{', '&', '*', '!', '|', '>']) {
        return 0;
    }
    let comment = rest.find(" #").unwrap_or(rest.len());
    let colon = rest[..comment]
        .char_indices()
        .find(|(p, c)| *c == ':' && rest[p + 1..].chars().next().is_none_or(|n| n == ' '))
        .map(|(p, _)| p);
    match colon {
        Some(p) if p > 0 => {
            out.push(Syn::Plain, &line[..start]);
            out.push(Syn::Keyword, &rest[..p]);
            start + p
        }
        _ => 0,
    }
}

/// The node name: the first bare word on a line.
fn kdl_prefix(line: &str, out: &mut Out) -> usize {
    let trimmed = line.trim_start();
    let lead = line.len() - trimmed.len();
    if !trimmed.starts_with(is_ident_start) {
        return 0;
    }
    let len: usize = trimmed.chars().take_while(|c| is_ident_char(*c, true) || *c == '.').map(char::len_utf8).sum();
    if trimmed[len..].starts_with('=') {
        return 0;
    }
    out.push(Syn::Plain, &line[..lead]);
    out.push(Syn::Keyword, &trimmed[..len]);
    lead + len
}

/// html / xml: comments, tag names as keywords, attribute values as strings.
fn markup_line(line: &str, mut i: usize, st: &mut TokState, out: &mut Out) {
    while i < line.len() {
        let rest = &line[i..];
        if st.in_tag {
            match rest.find(['>', '"', '\'']) {
                None => {
                    out.push(Syn::Plain, rest);
                    return;
                }
                Some(p) => {
                    out.push(Syn::Plain, &rest[..p]);
                    i += p;
                    if line[i..].starts_with('>') {
                        out.push(Syn::Plain, ">");
                        st.in_tag = false;
                        i += 1;
                        continue;
                    }
                    let quote: &'static str = if line[i..].starts_with('"') { "\"" } else { "'" };
                    match find_close(line, i + 1, quote, false) {
                        Some(end) => {
                            out.push(Syn::String, &line[i..end]);
                            i = end;
                        }
                        None => {
                            out.push(Syn::String, &line[i..]);
                            st.mode = Mode::Str { close: quote, esc: false };
                            return;
                        }
                    }
                }
            }
            continue;
        }
        if let Some(comment) = rest.strip_prefix("<!--") {
            match comment.find("-->") {
                Some(p) => {
                    out.push(Syn::Comment, &rest[..4 + p + 3]);
                    i += 4 + p + 3;
                }
                None => {
                    out.push(Syn::Comment, rest);
                    st.mode = Mode::Block("-->");
                    return;
                }
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix('<') {
            let punct = after.chars().take_while(|c| matches!(c, '/' | '?' | '!')).count();
            let name: usize = after[punct..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, ':' | '_' | '-' | '.'))
                .map(char::len_utf8)
                .sum();
            out.push(Syn::Plain, &rest[..1 + punct]);
            if name > 0 {
                out.push(Syn::Keyword, &after[punct..punct + name]);
                st.in_tag = true;
            }
            i += 1 + punct + name;
            continue;
        }
        let p = rest.find('<').unwrap_or(rest.len());
        out.push(Syn::Plain, &rest[..p]);
        i += p;
    }
}

/// Raw markdown the way the prototype's pane shows it: headings in the keyword colour, prose
/// in the string colour, code (fenced or inline) in `fg`, fences / quotes / comments muted.
fn markdown_source_line(line: &str, st: &mut TokState, out: &mut Out) {
    let trimmed = line.trim_start();
    let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
    if st.mode == Mode::Fence {
        if is_fence {
            st.mode = Mode::Normal;
            out.push(Syn::Comment, line);
        } else {
            out.push(Syn::Plain, line);
        }
        return;
    }
    if is_fence {
        st.mode = Mode::Fence;
        out.push(Syn::Comment, line);
        return;
    }
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].chars().next().is_none_or(|c| c == ' ') {
        out.push(Syn::Keyword, line);
        return;
    }
    if trimmed.starts_with('>') || trimmed.starts_with("<!--") {
        out.push(Syn::Comment, line);
        return;
    }
    // Inline code spans.
    let mut code = false;
    let mut rest = line;
    while let Some(p) = rest.find('`') {
        if code {
            out.push(Syn::Plain, &rest[..p + 1]);
            rest = &rest[p + 1..];
        } else if rest[p + 1..].contains('`') {
            out.push(Syn::String, &rest[..p]);
            out.push(Syn::Plain, "`");
            rest = &rest[p + 1..];
        } else {
            break;
        }
        code = !code;
    }
    out.push(Syn::String, rest);
}

// ───────────────────────────────────────────── markdown ────────────────────────────────────────

/// Block structure of a markdown document with inline markup flattened to plain text (link
/// text kept, URLs dropped). Fenced code is highlighted by its info string.
pub fn markdown_blocks(text: &str) -> Vec<MdBlock> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    struct Builder {
        blocks: Vec<MdBlock>,
        buf: String,
        /// (next ordinal for ordered lists, current item already emitted)
        lists: Vec<(Option<u64>, bool)>,
        quote: usize,
        code: Option<(String, String)>,
    }

    impl Builder {
        fn flush(&mut self) {
            let text = std::mem::take(&mut self.buf);
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let text: SharedString = text.to_string().into();
            if self.quote > 0 {
                self.blocks.push(MdBlock::Quote { text });
            } else if let Some((next, emitted)) = self.lists.last_mut().filter(|(_, emitted)| !*emitted) {
                let ordered = *next;
                if let Some(n) = next {
                    *n += 1;
                }
                *emitted = true;
                let depth = self.lists.len() - 1;
                self.blocks.push(MdBlock::ListItem { depth, ordered, text });
            } else {
                self.blocks.push(MdBlock::Paragraph { text });
            }
        }
    }

    let mut b = Builder { blocks: Vec::new(), buf: String::new(), lists: Vec::new(), quote: 0, code: None };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        if b.blocks.len() >= MAX_MD_BLOCKS {
            break;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } | Tag::Paragraph => b.flush(),
                Tag::BlockQuote(_) => {
                    b.flush();
                    b.quote += 1;
                }
                Tag::List(start) => {
                    b.flush();
                    b.lists.push((start, true));
                }
                Tag::Item => {
                    b.flush();
                    if let Some(top) = b.lists.last_mut() {
                        top.1 = false;
                    }
                }
                Tag::CodeBlock(kind) => {
                    b.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.code = Some((lang, String::new()));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(level) => {
                    let text = std::mem::take(&mut b.buf).trim().to_string();
                    b.blocks.push(MdBlock::Heading { level: level as u8, text: text.into() });
                }
                TagEnd::Paragraph => b.flush(),
                TagEnd::BlockQuote(_) => {
                    b.flush();
                    b.quote = b.quote.saturating_sub(1);
                }
                TagEnd::Item => {
                    b.flush();
                    if let Some(top) = b.lists.last_mut() {
                        top.1 = true;
                    }
                }
                TagEnd::List(_) => {
                    b.flush();
                    b.lists.pop();
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = b.code.take() {
                        let lines = highlight(&lang, &code);
                        b.blocks.push(MdBlock::Code { lang: lang.into(), lines });
                    }
                }
                _ => {}
            },
            Event::Text(t) | Event::Code(t) => match &mut b.code {
                Some((_, code)) => code.push_str(&t),
                None => b.buf.push_str(&t),
            },
            Event::SoftBreak | Event::HardBreak => b.buf.push(' '),
            Event::TaskListMarker(done) => b.buf.push_str(if done { "[x] " } else { "[ ] " }),
            Event::Rule => {
                b.flush();
                b.blocks.push(MdBlock::Rule);
            }
            _ => {}
        }
    }
    b.flush();
    b.blocks
}

// ────────────────────────────────────────────── image ──────────────────────────────────────────

fn load_image(entry: &Entry, md: &fs::Metadata) -> PreviewBody {
    let ext = entry.extension().unwrap_or("").to_lowercase();
    let head = read_capped(&entry.path, 64).unwrap_or_default();
    let sniffed = imagesize::image_type(&head).ok();
    let (width, height) = if ext == "svg" {
        (0, 0)
    } else {
        imagesize::size(&entry.path)
            .ok()
            .map(|s| (u32::try_from(s.width).unwrap_or(0), u32::try_from(s.height).unwrap_or(0)))
            .unwrap_or((0, 0))
    };
    let format: String = match sniffed {
        Some(kind) if ext != "svg" => {
            let name = format!("{kind:?}").to_uppercase();
            if name == "JPEG" { "JPEG".into() } else { name }
        }
        _ if ext == "jpg" => "JPEG".into(),
        _ => ext.to_uppercase(),
    };
    // PNG: the colour type is byte 25 of the header.
    let colour = if head.starts_with(b"\x89PNG") && head.len() > 25 {
        match head[25] {
            0 => Some("Gray"),
            2 => Some("RGB"),
            3 => Some("Indexed"),
            4 => Some("Gray · Alpha"),
            6 => Some("RGBA"),
            _ => None,
        }
    } else {
        None
    };
    let mut meta = Vec::new();
    if width > 0 && height > 0 {
        meta.push(row("size", format!("{width} × {height}")));
    }
    meta.push(row("format", match colour {
        Some(c) => format!("{format} · {c}"),
        None => format.clone(),
    }));
    meta.push(row("created", fs_model::format_mtime(md.created().ok().or(entry.modified))));
    meta.push(row("file size", fs_model::format_size(entry.size)));
    PreviewBody::Image { path: entry.path.clone(), width, height, format: format.into(), meta }
}

// ─────────────────────────────────────────── media / pdf ───────────────────────────────────────

fn format_duration(secs: f64) -> String {
    let total = secs.max(0.).round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn load_media(entry: &Entry) -> PreviewBody {
    let mut meta = base_meta(entry);
    let probe = run_with_timeout(
        "ffprobe",
        &[
            OsStr::new("-v"),
            OsStr::new("error"),
            OsStr::new("-show_entries"),
            OsStr::new("format=duration:stream=codec_name,width,height"),
            OsStr::new("-of"),
            OsStr::new("default=noprint_wrappers=1"),
            entry.path.as_os_str(),
        ],
        PROBE_TIMEOUT,
    );
    if let Some(out) = probe {
        meta.extend(parse_ffprobe(&out));
    }
    PreviewBody::Other { meta }
}

fn parse_ffprobe(out: &str) -> Vec<(SharedString, SharedString)> {
    let (mut codecs, mut dims, mut duration) = (Vec::new(), None, None);
    let (mut w, mut h) = (None, None);
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "codec_name" if !codecs.iter().any(|c| c == v) => codecs.push(v.to_string()),
            "width" => w = v.parse::<u32>().ok(),
            "height" => h = v.parse::<u32>().ok(),
            "duration" => duration = v.parse::<f64>().ok(),
            _ => {}
        }
        if let (Some(w), Some(h), None) = (w, h, &dims) {
            dims = Some(format!("{w} × {h}"));
        }
    }
    let mut rows = Vec::new();
    if let Some(d) = duration {
        rows.push(row("duration", format_duration(d)));
    }
    if let Some(d) = dims {
        rows.push(row("frame", d));
    }
    if !codecs.is_empty() {
        rows.push(row("codec", codecs.join(" · ")));
    }
    rows
}

fn load_pdf(entry: &Entry) -> PreviewBody {
    let mut meta = base_meta(entry);
    if let Some(out) = run_with_timeout("pdfinfo", &[entry.path.as_os_str()], PROBE_TIMEOUT) {
        for line in out.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let v = v.trim();
            match k.trim() {
                "Pages" => meta.push(row("pages", v.to_string())),
                "Title" if !v.is_empty() => meta.push(row("title", v.to_string())),
                "Page size" => meta.push(row("page size", v.to_string())),
                _ => {}
            }
        }
    }
    PreviewBody::Other { meta }
}

// ───────────────────────────────────────────── archives ────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    TarZst,
    TarXz,
    TarBz2,
}

impl ArchiveFormat {
    /// Format and the length of the archive suffix (`.tar.gz` → 7), by file name.
    pub(crate) fn detect(name: &str) -> Option<(ArchiveFormat, usize)> {
        const SUFFIXES: [(&str, ArchiveFormat); 12] = [
            (".tar.gz", ArchiveFormat::TarGz),
            (".tgz", ArchiveFormat::TarGz),
            (".tar.zst", ArchiveFormat::TarZst),
            (".tzst", ArchiveFormat::TarZst),
            (".tar.xz", ArchiveFormat::TarXz),
            (".txz", ArchiveFormat::TarXz),
            (".tar.bz2", ArchiveFormat::TarBz2),
            (".tbz2", ArchiveFormat::TarBz2),
            (".tbz", ArchiveFormat::TarBz2),
            (".tar", ArchiveFormat::Tar),
            (".zip", ArchiveFormat::Zip),
            (".jar", ArchiveFormat::Zip),
        ];
        let lower = name.to_lowercase();
        SUFFIXES.iter().find(|(suffix, _)| lower.ends_with(suffix)).map(|(suffix, format)| (*format, suffix.len()))
    }

    /// Compression name for the summary line.
    pub fn label(self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Tar => "tar",
            ArchiveFormat::TarGz => "gzip",
            ArchiveFormat::TarZst => "zstd",
            ArchiveFormat::TarXz => "xz",
            ArchiveFormat::TarBz2 => "bzip2",
        }
    }
}

/// Can `extract_archive` unpack this file (by name)?
pub fn can_extract(path: &Path) -> bool {
    path.file_name().and_then(|n| crate::ops::extract::detect(&n.to_string_lossy())).is_some()
}

/// `dots.tar.zst` → `dots`.
pub fn archive_stem(name: &str) -> String {
    let stem = match ArchiveFormat::detect(name) {
        Some((_, suffix)) => &name[..name.len() - suffix],
        None => name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name),
    };
    if stem.is_empty() { "archive".into() } else { stem.to_string() }
}

/// One archive member as listed: full path, directory flag, uncompressed size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Default)]
struct Listing {
    raw: Vec<RawEntry>,
    files: usize,
    unpacked: u64,
    /// The scan stopped early (budget, cap or a damaged archive): totals are lower bounds.
    partial: bool,
}

impl Listing {
    fn add(&mut self, path: String, is_dir: bool, size: u64) {
        if !is_dir {
            self.files += 1;
            self.unpacked = self.unpacked.saturating_add(size);
        }
        if self.raw.len() < MAX_ARCHIVE_TREE {
            self.raw.push(RawEntry { path, is_dir, size });
        }
    }
}

/// Fails with `TimedOut` once the deadline has passed, so a huge compressed stream cannot
/// keep the loader busy.
struct DeadlineReader<R> {
    inner: R,
    until: Instant,
}

impl<R: Read> Read for DeadlineReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.until {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.inner.read(buf)
    }
}

pub(crate) fn tar_reader(format: ArchiveFormat, file: fs::File) -> io::Result<Box<dyn Read>> {
    let file = io::BufReader::with_capacity(64 * 1024, file);
    Ok(match format {
        ArchiveFormat::Tar | ArchiveFormat::Zip => Box::new(file),
        ArchiveFormat::TarGz => Box::new(flate2::bufread::MultiGzDecoder::new(file)),
        ArchiveFormat::TarZst => Box::new(zstd::stream::read::Decoder::with_buffer(file)?),
        ArchiveFormat::TarXz => Box::new(xz2::bufread::XzDecoder::new_multi_decoder(file)),
        ArchiveFormat::TarBz2 => Box::new(bzip2::bufread::MultiBzDecoder::new(file)),
    })
}

fn list_archive(path: &Path, format: ArchiveFormat) -> Result<Listing, String> {
    let file = fs::File::open(path).map_err(|e| io_message(&e))?;
    let mut listing = Listing::default();
    if format == ArchiveFormat::Zip {
        let mut zip = zip::ZipArchive::new(io::BufReader::new(file)).map_err(|e| format!("not a readable zip: {e}"))?;
        let until = Instant::now() + ARCHIVE_BUDGET;
        for ix in 0..zip.len() {
            if ix >= MAX_ARCHIVE_SCAN || (ix % 512 == 0 && Instant::now() >= until) {
                listing.partial = true;
                break;
            }
            match zip.by_index_raw(ix) {
                Ok(member) => listing.add(member.name().to_string(), member.is_dir(), member.size()),
                Err(_) => listing.partial = true,
            }
        }
        return Ok(listing);
    }
    let reader = tar_reader(format, file).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(DeadlineReader { inner: reader, until: Instant::now() + ARCHIVE_BUDGET });
    let entries = archive.entries().map_err(|e| e.to_string())?;
    let mut seen = 0usize;
    for member in entries {
        let member = match member {
            Ok(member) => member,
            Err(err) => {
                // Budget exhausted or truncated / corrupt stream: keep what we have.
                if seen == 0 && err.kind() != io::ErrorKind::TimedOut {
                    return Err(format!("not a readable {} archive", format.label()));
                }
                listing.partial = true;
                break;
            }
        };
        seen += 1;
        if seen > MAX_ARCHIVE_SCAN {
            listing.partial = true;
            break;
        }
        let kind = member.header().entry_type();
        if matches!(kind, tar::EntryType::XGlobalHeader | tar::EntryType::XHeader) {
            continue;
        }
        let name = String::from_utf8_lossy(&member.path_bytes()).into_owned();
        listing.add(name, kind.is_dir(), member.size());
    }
    Ok(listing)
}

/// Turn a flat member list into a depth-first tree: directories first at each level (then by
/// name), parents synthesised when the archive does not list them, at most `cap` rows.
pub fn build_archive_tree(raw: &[RawEntry], cap: usize) -> Vec<ArchiveEntry> {
    struct Node {
        name: String,
        is_dir: bool,
        size: u64,
        children: Vec<usize>,
    }
    let mut nodes = vec![Node { name: String::new(), is_dir: true, size: 0, children: Vec::new() }];
    let mut index: HashMap<(usize, String), usize> = HashMap::new();
    for entry in raw {
        let parts: Vec<&str> = entry.path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        let mut parent = 0;
        for (ix, part) in parts.iter().enumerate() {
            let last = ix + 1 == parts.len();
            let id = match index.get(&(parent, part.to_string())) {
                Some(id) => *id,
                None => {
                    let id = nodes.len();
                    nodes.push(Node { name: part.to_string(), is_dir: true, size: 0, children: Vec::new() });
                    nodes[parent].children.push(id);
                    index.insert((parent, part.to_string()), id);
                    id
                }
            };
            if last {
                // A path seen both as `a` and `a/b` stays a directory.
                nodes[id].is_dir = entry.is_dir || !nodes[id].children.is_empty();
                if !nodes[id].is_dir {
                    nodes[id].size = entry.size;
                }
            } else {
                nodes[id].is_dir = true;
                nodes[id].size = 0;
            }
            parent = id;
        }
    }
    let sorted_children = |nodes: &[Node], id: usize| -> Vec<usize> {
        let mut kids = nodes[id].children.clone();
        kids.sort_by(|a, b| {
            let (a, b) = (&nodes[*a], &nodes[*b]);
            b.is_dir.cmp(&a.is_dir).then_with(|| fs_model::natural_cmp(&a.name, &b.name)).then_with(|| a.name.cmp(&b.name))
        });
        kids
    };
    // Explicit stack: archive paths can be arbitrarily deep.
    let mut out = Vec::new();
    let mut stack: Vec<(usize, usize)> = sorted_children(&nodes, 0).into_iter().rev().map(|id| (id, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        let node = &nodes[id];
        out.push(ArchiveEntry { name: node.name.clone().into(), depth, is_dir: node.is_dir, size: node.size });
        stack.extend(sorted_children(&nodes, id).into_iter().rev().map(|kid| (kid, depth + 1)));
    }
    out
}

/// `zstd · 2.8M → 11.4M · 214 files`
pub fn archive_summary(compression: &str, packed: u64, unpacked: u64, files: usize, partial: bool) -> String {
    format!(
        "{compression} · {} → {}{plus} · {files}{plus} {}",
        fs_model::format_size(packed),
        fs_model::format_size(unpacked),
        if files == 1 && !partial { "file" } else { "files" },
        plus = if partial { "+" } else { "" },
    )
}

fn load_archive(entry: &Entry, packed: u64) -> PreviewBody {
    let Some((format, _)) = ArchiveFormat::detect(&entry.name) else {
        // .gz / .7z / .rar …: nothing we can list.
        return PreviewBody::Other { meta: base_meta(entry) };
    };
    match list_archive(&entry.path, format) {
        Ok(listing) => PreviewBody::Archive {
            summary: archive_summary(format.label(), packed, listing.unpacked, listing.files, listing.partial).into(),
            packed,
            unpacked: listing.unpacked,
            files: listing.files,
            entries: build_archive_tree(&listing.raw, MAX_ARCHIVE_ENTRIES),
        },
        Err(err) => PreviewBody::Error(err.into()),
    }
}

/// Extract `archive` into `dest_dir` and return the path that now holds its contents.
/// BLOCKING. The work is done by `ops::extract` (shared with the context menu and `e`).
pub fn extract_archive(archive: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    crate::ops::extract::extract_into(archive, dest_dir)
}

// ─────────────────────────────────────────────── tests ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(lines: &[Line]) -> Vec<Vec<(Syn, String)>> {
        lines.iter().map(|l| l.iter().map(|s| (s.syn, s.text.to_string())).collect()).collect()
    }

    fn syn_of(line: &Line, needle: &str) -> Syn {
        line.iter()
            .find(|s| s.text.contains(needle))
            .unwrap_or_else(|| panic!("no span containing {needle:?} in {line:?}"))
            .syn
    }

    fn text_of(line: &Line) -> String {
        line.iter().map(|s| s.text.as_ref()).collect()
    }

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string().into(),
            path: PathBuf::from("/nonexistent").join(name),
            is_dir,
            is_symlink: false,
            symlink_target: None,
            size: if is_dir { 0 } else { 2150 },
            child_count: is_dir.then_some(12),
            modified: None,
            mode: 0o644,
            hidden: false,
            file_type: FileType::from_name(name, is_dir),
        }
    }

    #[test]
    fn rust_tokens() {
        let src = "fn main() { // entry\n    let x: &'a str = \"hi\\\"there\"; let c = 'x'; let n = 0xff_u8;\n}";
        let lines = highlight("rs", src);
        assert_eq!(lines.len(), 3);
        assert_eq!(syn_of(&lines[0], "fn"), Syn::Keyword);
        assert_eq!(syn_of(&lines[0], "main"), Syn::Plain);
        assert_eq!(syn_of(&lines[0], "// entry"), Syn::Comment);
        assert_eq!(syn_of(&lines[1], "let"), Syn::Keyword);
        assert_eq!(syn_of(&lines[1], "\"hi\\\"there\""), Syn::String);
        assert_eq!(syn_of(&lines[1], "'x'"), Syn::String);
        assert_eq!(syn_of(&lines[1], "0xff_u8"), Syn::Number);
        // The lifetime tick must not open a string.
        assert!(lines[1].iter().any(|s| s.syn == Syn::Keyword && s.text.as_ref() == "str") || syn_of(&lines[1], "str") == Syn::Plain);
        assert_eq!(text_of(&lines[1]), "    let x: &'a str = \"hi\\\"there\"; let c = 'x'; let n = 0xff_u8;");
    }

    #[test]
    fn block_comments_and_strings_span_lines() {
        let lines = highlight("c", "int a; /* one\n two */ int b;\nreturn 1.5;");
        assert_eq!(syn_of(&lines[0], "/* one"), Syn::Comment);
        assert_eq!(syn_of(&lines[1], "two */"), Syn::Comment);
        assert_eq!(syn_of(&lines[1], "int"), Syn::Keyword);
        assert_eq!(syn_of(&lines[2], "1.5"), Syn::Number);

        let py = highlight("py", "x = \"\"\"doc\nstill # not a comment\n\"\"\" # real\ndef f(): pass");
        assert_eq!(syn_of(&py[1], "still"), Syn::String);
        assert_eq!(syn_of(&py[2], "# real"), Syn::Comment);
        assert_eq!(syn_of(&py[3], "def"), Syn::Keyword);

        let js = highlight("ts", "const s = `a\n${b}` + 'c';");
        assert_eq!(syn_of(&js[0], "`a"), Syn::String);
        assert_eq!(syn_of(&js[1], "${b}`"), Syn::String);
        assert_eq!(syn_of(&js[1], "'c'"), Syn::String);

        let lua = highlight("lua", "--[[ long\ncomment ]] local x = 1 -- tail");
        assert_eq!(syn_of(&lua[1], "comment ]]"), Syn::Comment);
        assert_eq!(syn_of(&lua[1], "local"), Syn::Keyword);
        assert_eq!(syn_of(&lua[1], "-- tail"), Syn::Comment);
    }

    #[test]
    fn data_languages_colour_keys() {
        let toml = highlight("toml", "[package]\nname = \"fm\" # c\nedition = 2024\nflag = true");
        assert_eq!(flat(&toml)[0], vec![(Syn::Keyword, "[package]".to_string())]);
        assert_eq!(syn_of(&toml[1], "name"), Syn::Keyword);
        assert_eq!(syn_of(&toml[1], "\"fm\""), Syn::String);
        assert_eq!(syn_of(&toml[1], "# c"), Syn::Comment);
        assert_eq!(syn_of(&toml[2], "2024"), Syn::Number);
        assert_eq!(syn_of(&toml[3], "true"), Syn::Keyword);

        let yaml = highlight("yml", "services:\n  - name: \"web\" # c\n    port: 8080");
        assert_eq!(syn_of(&yaml[0], "services"), Syn::Keyword);
        assert_eq!(syn_of(&yaml[1], "name"), Syn::Keyword);
        assert_eq!(syn_of(&yaml[1], "\"web\""), Syn::String);
        assert_eq!(syn_of(&yaml[1], "# c"), Syn::Comment);
        assert_eq!(syn_of(&yaml[2], "8080"), Syn::Number);

        let json = highlight("json", "{ \"key\": \"value\", \"n\": 12, \"ok\": null }");
        assert_eq!(syn_of(&json[0], "\"key\""), Syn::Keyword);
        assert_eq!(syn_of(&json[0], "\"value\""), Syn::String);
        assert_eq!(syn_of(&json[0], "12"), Syn::Number);
        assert_eq!(syn_of(&json[0], "null"), Syn::Keyword);
    }

    #[test]
    fn misc_languages() {
        let sh = highlight("sh", "if [ $# -gt 0 ]; then echo 'a b' # done");
        assert_eq!(syn_of(&sh[0], "if"), Syn::Keyword);
        assert_eq!(syn_of(&sh[0], "'a b'"), Syn::String);
        assert_eq!(syn_of(&sh[0], "# done"), Syn::Comment);
        assert!(text_of(&sh[0]).contains("$# -gt"));
        assert_eq!(syn_of(&sh[0], "$"), Syn::Plain);

        let html = highlight("html", "<!-- c --><a href=\"x\">it's</a>");
        assert_eq!(syn_of(&html[0], "<!-- c -->"), Syn::Comment);
        assert_eq!(syn_of(&html[0], "\"x\""), Syn::String);
        assert!(html[0].iter().any(|s| s.syn == Syn::Keyword && s.text.as_ref() == "a"));
        assert_eq!(syn_of(&html[0], "it's"), Syn::Plain);

        let css = highlight("css", "a:hover {\n  color: #fff; margin: 12px;\n}\n@media print {}");
        assert_eq!(syn_of(&css[0], "a"), Syn::Plain);
        assert_eq!(syn_of(&css[1], "color"), Syn::Keyword);
        assert_eq!(syn_of(&css[1], "#fff"), Syn::Number);
        assert_eq!(syn_of(&css[1], "12px"), Syn::Number);
        assert_eq!(syn_of(&css[3], "@media"), Syn::Keyword);

        let kdl = highlight("kdl", "layout {\n    gaps 12 // px\n    name \"a\"\n}");
        assert_eq!(syn_of(&kdl[0], "layout"), Syn::Keyword);
        assert_eq!(syn_of(&kdl[1], "gaps"), Syn::Keyword);
        assert_eq!(syn_of(&kdl[1], "12"), Syn::Number);
        assert_eq!(syn_of(&kdl[1], "// px"), Syn::Comment);

        let sql = highlight("sql", "SELECT a FROM t WHERE b = 'x' -- c");
        assert_eq!(syn_of(&sql[0], "SELECT"), Syn::Keyword);
        assert_eq!(syn_of(&sql[0], "'x'"), Syn::String);
        assert_eq!(syn_of(&sql[0], "-- c"), Syn::Comment);

        let nix = highlight("nix", "{ pkgs ? import <nixpkgs> { } }: let s = ''\n  multi\n''; in s # c");
        assert_eq!(syn_of(&nix[0], "import"), Syn::Keyword);
        assert_eq!(syn_of(&nix[1], "multi"), Syn::String);
        assert_eq!(syn_of(&nix[2], "in"), Syn::Keyword);
        assert_eq!(syn_of(&nix[2], "# c"), Syn::Comment);
    }

    #[test]
    fn plain_tabs_and_caps() {
        let lines = highlight("unknown-language", "\tfn x\r\n\nlast");
        assert_eq!(flat(&lines)[0], vec![(Syn::Plain, "    fn x".to_string())]);
        assert!(lines[1].is_empty());
        let long = "x".repeat(MAX_LINE_CHARS * 3);
        assert_eq!(text_of(&highlight("txt", &long)[0]).chars().count(), MAX_LINE_CHARS);
        let many = "a\n".repeat(MAX_TEXT_LINES + 50);
        let (lines, capped) = highlight_lang(&PLAIN, &many);
        assert_eq!(lines.len(), MAX_TEXT_LINES);
        assert!(capped);
        // Weird input must not panic in any language.
        let nasty = "é'\\\n\"\\\n'''\u{1F600}`\\\n<a b='é\n-- [[ ]] /* 0x é: =\n- - k: v\n[é]\n#!";
        for tag in ["rs", "py", "js", "c", "go", "lua", "sh", "nix", "toml", "ini", "yaml", "json", "css", "scss", "html", "kdl", "sql", "md", "hs", "rb", "el", "mk"] {
            let lines = highlight(tag, nasty);
            assert_eq!(lines.len(), nasty.lines().count(), "{tag}");
            for (line, raw) in lines.iter().zip(nasty.lines()) {
                assert_eq!(text_of(line), raw, "{tag}");
            }
        }
    }

    #[test]
    fn language_detection() {
        assert!(std::ptr::eq(lang_for_file("main.rs", ""), &RUST));
        assert!(std::ptr::eq(lang_for_file("Makefile", ""), &HASH));
        assert!(std::ptr::eq(lang_for_file(".gitignore", ""), &HASH));
        assert!(std::ptr::eq(lang_for_file(".zshrc", ""), &SHELL));
        assert!(std::ptr::eq(lang_for_file("Cargo.lock", "# generated\n[[package]]"), &TOML));
        assert!(std::ptr::eq(lang_for_file("flake.lock", "{\n"), &JSON));
        assert!(std::ptr::eq(lang_for_file("run", "#!/usr/bin/env python3\n"), &PYTHON));
        assert!(std::ptr::eq(lang_for_file("run", "#!/bin/sh -e\n"), &SHELL));
        assert!(std::ptr::eq(lang_for_file("notes", "hello"), &PLAIN));
    }

    #[test]
    fn markdown_source_and_blocks() {
        let src = "# dots\n\nniri + *ags* rice, see [the docs](https://example.org) and `code`.\n\n## install\n```sh\n./install.sh --theme ash # go\n```\n\n- rail\n- status\n  1. nested one\n  2. nested two\n\n> quoted\n> text\n\n---\n\n- [x] done\n";
        let lines = highlight("md", src);
        assert_eq!(flat(&lines)[0], vec![(Syn::Keyword, "# dots".to_string())]);
        assert_eq!(syn_of(&lines[2], "niri"), Syn::String);
        assert_eq!(syn_of(&lines[2], "`code`"), Syn::Plain);
        assert_eq!(syn_of(&lines[5], "```sh"), Syn::Comment);
        assert_eq!(flat(&lines)[6], vec![(Syn::Plain, "./install.sh --theme ash # go".to_string())]);
        assert_eq!(syn_of(&lines[9], "- rail"), Syn::String);

        let blocks = markdown_blocks(src);
        let mut it = blocks.iter();
        assert!(matches!(it.next(), Some(MdBlock::Heading { level: 1, text }) if text.as_ref() == "dots"));
        assert!(
            matches!(it.next(), Some(MdBlock::Paragraph { text }) if text.as_ref() == "niri + ags rice, see the docs and code.")
        );
        assert!(matches!(it.next(), Some(MdBlock::Heading { level: 2, text }) if text.as_ref() == "install"));
        match it.next() {
            Some(MdBlock::Code { lang, lines }) => {
                assert_eq!(lang.as_ref(), "sh");
                assert_eq!(lines.len(), 1);
                assert_eq!(syn_of(&lines[0], "# go"), Syn::Comment);
            }
            other => panic!("expected code, got {other:?}"),
        }
        assert!(matches!(it.next(), Some(MdBlock::ListItem { depth: 0, ordered: None, text }) if text.as_ref() == "rail"));
        assert!(matches!(it.next(), Some(MdBlock::ListItem { depth: 0, ordered: None, text }) if text.as_ref() == "status"));
        assert!(
            matches!(it.next(), Some(MdBlock::ListItem { depth: 1, ordered: Some(1), text }) if text.as_ref() == "nested one")
        );
        assert!(
            matches!(it.next(), Some(MdBlock::ListItem { depth: 1, ordered: Some(2), text }) if text.as_ref() == "nested two")
        );
        assert!(matches!(it.next(), Some(MdBlock::Quote { text }) if text.as_ref() == "quoted text"));
        assert!(matches!(it.next(), Some(MdBlock::Rule)));
        assert!(matches!(it.next(), Some(MdBlock::ListItem { depth: 0, ordered: None, text }) if text.as_ref() == "[x] done"));
        assert!(it.next().is_none());
        assert!(markdown_blocks("").is_empty());
    }

    #[test]
    fn meta_lines() {
        assert_eq!(meta_line(&entry("README.md", false)), "markdown · 2.1K · rw-r--r--");
        assert_eq!(meta_line(&entry("niri", true)), "folder · 12 items · —");
        let mut one = entry("x", true);
        one.child_count = Some(1);
        assert_eq!(meta_line(&one), "folder · 1 item · —");
    }

    #[test]
    fn ages_and_git_parsing() {
        assert_eq!(relative_age(5), "now");
        assert_eq!(relative_age(125), "2m");
        assert_eq!(relative_age(2 * 3600 + 5), "2h");
        assert_eq!(relative_age(3 * 86400), "3d");
        assert_eq!(relative_age(21 * 86400), "3w");
        assert_eq!(relative_age(100 * 86400), "3mo");
        assert_eq!(relative_age(800 * 86400), "2y");
        let commits = parse_git_log("a1f3e09\t1000\tgaps 12, center column\n7c22b4d\t100\t\nbad line\n", 1000 + 7200);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash.as_ref(), "a1f3e09");
        assert_eq!(commits[0].subject.as_ref(), "gaps 12, center column");
        assert_eq!(commits[0].age.as_ref(), "2h");
        let modified = parse_git_status(" M config.kdl\nM  src/a b.rs\nA  new.rs\n D gone\n");
        assert_eq!(modified.iter().map(|s| s.to_string()).collect::<Vec<_>>(), vec!["config.kdl", "src/a b.rs"]);
        assert_eq!(parse_ffprobe("codec_name=h264\nwidth=1920\nheight=1080\ncodec_name=aac\nduration=192.4\n").len(), 3);
        assert_eq!(format_duration(192.4), "3:12");
        assert_eq!(format_duration(3725.), "1:02:05");
    }

    fn raw(path: &str, is_dir: bool, size: u64) -> RawEntry {
        RawEntry { path: path.into(), is_dir, size }
    }

    #[test]
    fn archive_tree() {
        let entries = [
            raw("dots/install.sh", false, 1100),
            raw("dots/README.md", false, 4000),
            raw("dots/niri/config.kdl", false, 6800),
            raw("./dots/ags/", true, 0),
            raw("dots/ags/widgets/bar.ts", false, 10),
            raw("top.txt", false, 1),
        ];
        let tree = build_archive_tree(&entries, 100);
        let shape: Vec<(String, usize, bool, u64)> =
            tree.iter().map(|e| (e.name.to_string(), e.depth, e.is_dir, e.size)).collect();
        assert_eq!(
            shape,
            vec![
                ("dots".to_string(), 0, true, 0),
                ("ags".to_string(), 1, true, 0),
                ("widgets".to_string(), 2, true, 0),
                ("bar.ts".to_string(), 3, false, 10),
                ("niri".to_string(), 1, true, 0),
                ("config.kdl".to_string(), 2, false, 6800),
                ("install.sh".to_string(), 1, false, 1100),
                ("README.md".to_string(), 1, false, 4000),
                ("top.txt".to_string(), 0, false, 1),
            ]
        );
        assert_eq!(build_archive_tree(&entries, 3).len(), 3);
        assert!(build_archive_tree(&[], 10).is_empty());
        // Very deep paths must not blow the stack.
        let deep = raw(&"d/".repeat(5000), true, 0);
        assert_eq!(build_archive_tree(&[deep], 10_000).len(), 5000);
    }

    #[test]
    fn archive_names_and_summary() {
        assert_eq!(archive_stem("dots.tar.zst"), "dots");
        assert_eq!(archive_stem("Dots.TGZ"), "Dots");
        assert_eq!(archive_stem("a.b.zip"), "a.b");
        assert_eq!(archive_stem("weird.7z"), "weird");
        assert_eq!(ArchiveFormat::detect("x.tar.bz2").map(|f| f.0), Some(ArchiveFormat::TarBz2));
        assert_eq!(ArchiveFormat::detect("x.gz"), None);
        assert_eq!(
            archive_summary("zstd", 2936013, 11953767, 214, false),
            "zstd · 2.8M → 11.4M · 214 files"
        );
        assert_eq!(archive_summary("zip", 10, 20, 1, false), "zip · 10B → 20B · 1 file");
        assert_eq!(archive_summary("gzip", 10, 20, 3, true), "gzip · 10B → 20B+ · 3+ files");
    }

    /// A scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("fm-preview-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_tar<W: io::Write>(writer: W, files: &[(&str, &[u8])]) -> W {
        let mut builder = tar::Builder::new(writer);
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn archives_round_trip() {
        use std::io::Write;
        let scratch = Scratch::new("archives");
        let dir = &scratch.0;
        let files: [(&str, &[u8]); 3] = [("dots/README.md", b"# dots\n"), ("dots/niri/config.kdl", b"gaps 12\n"), ("dots/a.txt", b"a")];

        // zip
        let zip_path = dir.join("pack.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("one.txt", options).unwrap();
        zip.write_all(b"hello hello hello").unwrap();
        zip.start_file("sub/two.txt", options).unwrap();
        zip.write_all(b"two").unwrap();
        zip.finish().unwrap();

        // tar flavours
        fs::write(dir.join("dots.tar"), write_tar(Vec::new(), &files)).unwrap();
        let gz = write_tar(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()), &files);
        fs::write(dir.join("dots.tar.gz"), gz.finish().unwrap()).unwrap();
        let zst = write_tar(zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap(), &files);
        fs::write(dir.join("dots.tar.zst"), zst.finish().unwrap()).unwrap();
        let xz = write_tar(xz2::write::XzEncoder::new(Vec::new(), 1), &files);
        fs::write(dir.join("dots.txz"), xz.finish().unwrap()).unwrap();
        let bz = write_tar(bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast()), &files);
        fs::write(dir.join("dots.tar.bz2"), bz.finish().unwrap()).unwrap();

        for (name, label) in
            [("dots.tar", "tar"), ("dots.tar.gz", "gzip"), ("dots.tar.zst", "zstd"), ("dots.txz", "xz"), ("dots.tar.bz2", "bzip2")]
        {
            let data = load(&Entry::from_path(&dir.join(name)).unwrap());
            match &data.body {
                PreviewBody::Archive { summary, unpacked, files, entries, .. } => {
                    assert!(summary.starts_with(label), "{name}: {summary}");
                    assert!(summary.ends_with("16B · 3 files"), "{name}: {summary}");
                    assert_eq!((*unpacked, *files), (16, 3), "{name}");
                    let names: Vec<&str> = entries.iter().map(|e| e.name.as_ref()).collect();
                    assert_eq!(names, ["dots", "niri", "config.kdl", "a.txt", "README.md"], "{name}");
                }
                other => panic!("{name}: expected archive, got {other:?}"),
            }
        }
        match load(&Entry::from_path(&zip_path).unwrap()).body {
            PreviewBody::Archive { files, unpacked, entries, .. } => {
                assert_eq!((files, unpacked), (2, 20));
                let names: Vec<(&str, usize)> = entries.iter().map(|e| (e.name.as_ref(), e.depth)).collect();
                assert_eq!(names, [("sub", 0), ("two.txt", 1), ("one.txt", 0)]);
            }
            other => panic!("expected archive, got {other:?}"),
        }

        // Extraction: one top-level entry keeps its name, several go into `<stem>/`, and
        // nothing existing is overwritten.
        let first = extract_archive(&dir.join("dots.tar.zst"), dir).unwrap();
        assert_eq!(first, dir.join("dots"));
        assert_eq!(fs::read(first.join("niri/config.kdl")).unwrap(), b"gaps 12\n");
        let second = extract_archive(&dir.join("dots.tar.gz"), dir).unwrap();
        assert_eq!(second, dir.join("dots (1)"));
        let packed = extract_archive(&zip_path, dir).unwrap();
        assert_eq!(packed, dir.join("pack"));
        assert_eq!(fs::read(packed.join("sub/two.txt")).unwrap(), b"two");
        assert!(fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("ashd-fm-extract")));

        // Truncated and garbage archives: an error or a partial listing, never a panic.
        let gz_bytes = fs::read(dir.join("dots.tar.gz")).unwrap();
        fs::write(dir.join("cut.tar.gz"), &gz_bytes[..gz_bytes.len() / 2]).unwrap();
        fs::write(dir.join("junk.zip"), b"not a zip at all").unwrap();
        fs::write(dir.join("junk.tar.xz"), b"not xz").unwrap();
        fs::write(dir.join("empty.tar"), b"").unwrap();
        for name in ["cut.tar.gz", "junk.zip", "junk.tar.xz", "empty.tar"] {
            let data = load(&Entry::from_path(&dir.join(name)).unwrap());
            assert!(matches!(data.body, PreviewBody::Archive { .. } | PreviewBody::Error(_)), "{name}: {:?}", data.body);
        }
        assert!(extract_archive(&dir.join("junk.zip"), dir).is_err());
        assert!(fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("ashd-fm-extract")));
    }

    #[test]
    fn folder_git_summary() {
        let scratch = Scratch::new("git");
        let dir = &scratch.0;
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.org", "-c", "commit.gpgsign=false"])
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        fs::write(dir.join("config.kdl"), "gaps 12\n").unwrap();
        fs::write(dir.join("other.txt"), "x").unwrap();
        assert!(git(&["add", "."]) && git(&["commit", "-q", "-m", "gaps 12, center column"]));
        fs::write(dir.join("config.kdl"), "gaps 14\n").unwrap();
        let data = load(&Entry::from_path(dir).unwrap());
        match &data.body {
            PreviewBody::Folder { git: Some(git), items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(git.commits.len(), 1);
                assert_eq!(git.commits[0].subject.as_ref(), "gaps 12, center column");
                assert_eq!(git.commits[0].age.as_ref(), "now");
                assert_eq!(git.modified.iter().map(|s| s.as_ref()).collect::<Vec<_>>(), ["config.kdl"]);
            }
            other => panic!("expected a folder with git, got {other:?}"),
        }
        assert!(run_with_timeout("sleep", &[OsStr::new("5")], Duration::from_millis(50)).is_none());
        assert!(run_with_timeout("fm-no-such-program", &[], Duration::from_millis(50)).is_none());
    }

    #[test]
    fn loads_files_from_disk() {
        let scratch = Scratch::new("files");
        let dir = &scratch.0;
        fs::write(dir.join("empty.txt"), b"").unwrap();
        fs::write(dir.join("bad.txt"), b"ok \xff\xfe bytes\nsecond").unwrap();
        fs::write(dir.join("blob.txt"), b"a\0b").unwrap();
        fs::write(dir.join("noext.weird"), b"plain words\n").unwrap();
        fs::write(dir.join("notes.md"), b"# t\n\nbody\n").unwrap();
        fs::write(dir.join(".hidden"), b"x").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        // 1×1 PNG header is enough for the size probe.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&[0, 0, 5, 120, 0, 0, 0, 200, 8, 6, 0, 0, 0]);
        fs::write(dir.join("p.png"), &png).unwrap();
        fs::write(dir.join("broken.png"), b"nope").unwrap();

        let body = |name: &str| load(&Entry::from_path(&dir.join(name)).unwrap()).body;
        assert!(matches!(body("empty.txt"), PreviewBody::Text { lines, truncated: false } if lines.len() == 1 && lines[0].is_empty()));
        assert!(matches!(body("bad.txt"), PreviewBody::Text { lines, .. } if lines.len() == 2));
        assert!(matches!(body("blob.txt"), PreviewBody::Other { meta } if !meta.is_empty()));
        assert!(matches!(body("noext.weird"), PreviewBody::Text { .. }));
        assert!(matches!(body("notes.md"), PreviewBody::Markdown { blocks, source_lines } if blocks.len() == 2 && source_lines.len() == 3));
        match body("p.png") {
            PreviewBody::Image { width, height, format, meta, .. } => {
                assert_eq!((width, height), (1400, 200));
                assert_eq!(format.as_ref(), "PNG");
                assert_eq!(meta[0].1.as_ref(), "1400 × 200");
                assert_eq!(meta[1].1.as_ref(), "PNG · RGBA");
            }
            other => panic!("expected image, got {other:?}"),
        }
        assert!(matches!(body("broken.png"), PreviewBody::Image { width: 0, height: 0, .. }));

        let folder = load(&Entry::from_path(dir).unwrap());
        assert_eq!(folder.folder_total, 8);
        match &folder.body {
            PreviewBody::Folder { items, git } => {
                assert!(git.is_none());
                assert_eq!(items[0].name.as_ref(), "sub");
                assert!(items.iter().all(|e| !e.hidden));
                assert_eq!(items.len(), 8);
            }
            other => panic!("expected folder, got {other:?}"),
        }
        let gone = entry("definitely-missing.txt", false);
        assert!(matches!(load(&gone).body, PreviewBody::Error(_)));
    }
}
