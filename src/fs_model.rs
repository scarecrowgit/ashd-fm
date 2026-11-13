//! Filesystem model: directory entries, sorting, and the formatters used all over the UI.
//! Pure std + chrono; nothing here touches GPUI except `SharedString` and `Icon`.

use std::{
    cmp::Ordering,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

use gpui::SharedString;

use crate::icons::Icon;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum FileType {
    Folder,
    Text,
    Code,
    Markdown,
    Image,
    Archive,
    Audio,
    Video,
    Pdf,
    Binary,
}

impl FileType {
    pub fn icon(self) -> Icon {
        match self {
            FileType::Folder => Icon::Folder,
            FileType::Code => Icon::Code,
            FileType::Image => Icon::Image,
            FileType::Archive => Icon::Archive,
            FileType::Audio => Icon::Music,
            FileType::Video => Icon::Video,
            _ => Icon::File,
        }
    }

    /// Lower-case word for meta lines: `markdown · 2.1K · rw-r--r--`.
    pub fn label(self) -> &'static str {
        match self {
            FileType::Folder => "folder",
            FileType::Text => "text",
            FileType::Code => "code",
            FileType::Markdown => "markdown",
            FileType::Image => "image",
            FileType::Archive => "archive",
            FileType::Audio => "audio",
            FileType::Video => "video",
            FileType::Pdf => "pdf",
            FileType::Binary => "file",
        }
    }

    pub fn from_name(name: &str, is_dir: bool) -> Self {
        if is_dir {
            return FileType::Folder;
        }
        let lower = name.to_lowercase();
        let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if lower.ends_with(".tar.gz") || lower.ends_with(".tar.zst") || lower.ends_with(".tar.xz") || lower.ends_with(".tar.bz2") {
            return FileType::Archive;
        }
        match ext {
            "md" | "markdown" => FileType::Markdown,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "avif" | "tiff" => FileType::Image,
            "zip" | "tar" | "gz" | "tgz" | "zst" | "xz" | "bz2" | "7z" | "rar" => FileType::Archive,
            "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" => FileType::Audio,
            "mp4" | "mkv" | "webm" | "mov" | "avi" => FileType::Video,
            "pdf" => FileType::Pdf,
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "kt" | "lua"
            | "sh" | "bash" | "zsh" | "fish" | "nix" | "toml" | "yaml" | "yml" | "json" | "kdl" | "css" | "scss"
            | "html" | "xml" | "sql" | "rb" | "php" | "swift" | "zig" | "hs" | "ex" | "exs" | "vim" | "el"
            | "ini" | "conf" | "cfg" | "mk" | "cmake" | "dockerfile" | "lock" => FileType::Code,
            "txt" | "log" | "csv" | "tsv" | "rst" | "org" | "" => FileType::Text,
            _ => FileType::Binary,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: SharedString,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<PathBuf>,
    /// Bytes. For directories this is 0 (use `child_count`).
    pub size: u64,
    /// Number of direct children, for directories we could read.
    pub child_count: Option<usize>,
    pub modified: Option<SystemTime>,
    /// Unix mode bits.
    pub mode: u32,
    /// Dotfile.
    pub hidden: bool,
    pub file_type: FileType,
}

impl Entry {
    pub fn from_path(path: &Path) -> Option<Entry> {
        let name = path.file_name()?.to_string_lossy().to_string();
        let link_meta = fs::symlink_metadata(path).ok()?;
        let is_symlink = link_meta.file_type().is_symlink();
        let meta = if is_symlink { fs::metadata(path).unwrap_or_else(|_| link_meta.clone()) } else { link_meta };
        let is_dir = meta.is_dir();
        Some(Entry {
            hidden: name.starts_with('.'),
            file_type: FileType::from_name(&name, is_dir),
            name: name.into(),
            path: path.to_path_buf(),
            is_dir,
            is_symlink,
            symlink_target: if is_symlink { fs::read_link(path).ok() } else { None },
            size: if is_dir { 0 } else { meta.size() },
            child_count: if is_dir { fs::read_dir(path).ok().map(|d| d.count()) } else { None },
            modified: meta.modified().ok(),
            mode: meta.permissions().mode(),
        })
    }

    /// `name/` for folders, plain name otherwise (SPEC §4.3).
    pub fn display_name(&self) -> SharedString {
        if self.is_dir { format!("{}/", self.name).into() } else { self.name.clone() }
    }

    /// Size column text: bytes for files, item count for folders, `—` when unknown.
    pub fn size_label(&self) -> String {
        if self.is_dir {
            match self.child_count {
                Some(n) => format!("{n}"),
                None => "—".into(),
            }
        } else {
            format_size(self.size)
        }
    }

    pub fn extension(&self) -> Option<&str> {
        if self.is_dir {
            return None;
        }
        let (stem, ext) = self.name.rsplit_once('.')?;
        if stem.is_empty() { None } else { Some(ext) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirError {
    PermissionDenied,
    NotFound,
    Other(String),
}

/// Read a directory (blocking — call it from the background executor).
pub fn read_dir(path: &Path) -> Result<Vec<Entry>, DirError> {
    let rd = fs::read_dir(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => DirError::PermissionDenied,
        std::io::ErrorKind::NotFound => DirError::NotFound,
        _ => DirError::Other(e.to_string()),
    })?;
    Ok(rd.filter_map(|e| e.ok()).filter_map(|e| Entry::from_path(&e.path())).collect())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Type,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SortSpec {
    pub key: SortKey,
    pub ascending: bool,
    pub folders_first: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        SortSpec { key: SortKey::Name, ascending: true, folders_first: true }
    }
}

impl SortSpec {
    pub fn compare(&self, a: &Entry, b: &Entry) -> Ordering {
        if self.folders_first && a.is_dir != b.is_dir {
            return if a.is_dir { Ordering::Less } else { Ordering::Greater };
        }
        let by_name = || natural_cmp(&a.name, &b.name);
        let ord = match self.key {
            SortKey::Name => by_name(),
            SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
            SortKey::Modified => a.modified.cmp(&b.modified).then_with(by_name),
            SortKey::Type => a.extension().unwrap_or("").cmp(b.extension().unwrap_or("")).then_with(by_name),
        };
        if self.ascending { ord } else { ord.reverse() }
    }
}

/// Case-insensitive compare that orders `file2` before `file10`; leading dots are ignored.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('.').to_lowercase();
    let b = b.trim_start_matches('.').to_lowercase();
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut n = 0u128;
                    while let Some(d) = it.peek().and_then(|c| c.to_digit(10)) {
                        n = n.saturating_mul(10).saturating_add(d as u128);
                        it.next();
                    }
                    n
                };
                let (na, nb) = (take(&mut ai), take(&mut bi));
                if na != nb {
                    return na.cmp(&nb);
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// `812B`, `2.8K`, `11.4M`, `1.2G` — the compact mono style from the prototype.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024. && u < UNITS.len() - 1 {
        v /= 1024.;
        u += 1;
    }
    if v >= 100. { format!("{v:.0}{}", UNITS[u]) } else { format!("{v:.1}{}", UNITS[u]) }
}

/// `13 Nov 12:47` this year, `13 Nov 2024` otherwise.
pub fn format_mtime(t: Option<SystemTime>) -> String {
    use chrono::{DateTime, Datelike, Local};
    let Some(t) = t else { return "—".into() };
    let dt: DateTime<Local> = t.into();
    if dt.year() == Local::now().year() {
        dt.format("%-d %b %H:%M").to_string()
    } else {
        dt.format("%-d %b %Y").to_string()
    }
}

/// `rwxr-xr-x`
pub fn format_perms(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// XDG trash `files` directory.
pub fn trash_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"))
        .join("Trash/files")
}

/// `/home/me/src/dots` → `~/src/dots`.
pub fn display_path(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Inverse of `display_path`: expands a leading `~`.
pub fn expand_path(input: &str) -> PathBuf {
    let input = input.trim();
    if input == "~" {
        home_dir()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(input)
    }
}

/// Last path component for tab titles (`/` for the root, `~` for home).
pub fn short_name(path: &Path) -> String {
    if path == home_dir() {
        return "~".into();
    }
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "/".into())
}
