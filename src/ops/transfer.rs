//! The transfer engine: scan, copy / move / link with progress, pause, cancel and conflict
//! resolution. Pure std (plus `trash` for displaced folders) — no GPUI, so it runs on a plain
//! worker thread and is unit-tested below.
//!
//! Safety rules: nothing is overwritten without an explicit `Replace`; a replaced file is
//! written next to the target first and renamed over it; the source of a cross-device move is
//! deleted only after its copy completed.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, symlink},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime},
};

use super::TransferKind;

/// Copy buffer: 128 KiB.
const CHUNK: usize = 128 * 1024;
const EXDEV: i32 = 18;

/// Flags shared between the UI and the worker; checked between chunks.
#[derive(Default)]
pub struct Control {
    pub pause: AtomicBool,
    pub cancel: AtomicBool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Skip,
    /// Auto-rename the incoming item: `name (1).ext`.
    KeepBoth,
    Rename(String),
    Replace,
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub resolution: Resolution,
    pub apply_to_all: bool,
}

/// One side of a conflict.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    /// Pixel size, for images.
    pub dims: Option<(usize, usize)>,
}

impl FileInfo {
    pub fn read(path: &Path) -> FileInfo {
        let meta = fs::metadata(path).or_else(|_| fs::symlink_metadata(path)).ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let lower = name.to_lowercase();
        let is_image = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "avif", "tiff"]
            .iter()
            .any(|ext| lower.rsplit_once('.').is_some_and(|(_, e)| e == *ext));
        FileInfo {
            path: path.to_path_buf(),
            name,
            size: meta.as_ref().map(|m| if is_dir { 0 } else { m.len() }).unwrap_or(0),
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            is_dir,
            dims: if is_image && !is_dir { imagesize::size(path).ok().map(|s| (s.width, s.height)) } else { None },
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConflictInfo {
    /// Incoming item.
    pub src: FileInfo,
    /// What is already in the folder.
    pub dst: FileInfo,
    /// 1-based number of this conflict within the job.
    pub index: usize,
    /// Conflicts still to come after this one.
    pub remaining: usize,
}

/// What a finished transfer did (drives the toast and Undo).
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// Top-level destination paths that did not exist before.
    pub created: Vec<PathBuf>,
    /// For moves: `(original, new)` of every top-level item that landed on a fresh path.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Top-level items transferred.
    pub items: usize,
    pub files: u64,
    pub skipped: usize,
    pub replaced: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    Failed(String),
}

/// The worker's view of the UI.
pub trait Host {
    fn totals(&mut self, bytes: u64, files: u64);
    fn progress(&mut self, bytes: u64, files: u64);
    /// Blocks until the user answers; `None` cancels the job.
    fn conflict(&mut self, conflict: ConflictInfo) -> Option<Reply>;
}

// ── names ────────────────────────────────────────────────────────────────────

/// `("archive", ".tar.gz")`, `("notes", ".md")`, `(".bashrc", "")`; folders have no extension.
pub fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    let lower = name.to_ascii_lowercase();
    for multi in [".tar.gz", ".tar.zst", ".tar.xz", ".tar.bz2"] {
        if lower.ends_with(multi) && name.len() > multi.len() {
            return name.split_at(name.len() - multi.len());
        }
    }
    match name.rfind('.') {
        Some(ix) if ix > 0 => name.split_at(ix),
        _ => (name, ""),
    }
}

/// Keep-both name: `name (1).ext`, `name (2).ext`, …
pub fn numbered_name(name: &str, is_dir: bool, exists: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = split_ext(name, is_dir);
    (1u32..).map(|n| format!("{stem} ({n}){ext}")).find(|c| !exists(c)).unwrap_or_else(|| name.to_string())
}

/// Duplicate-in-place name: `name copy.ext`, `name copy 2.ext`, …
pub fn copy_name(name: &str, is_dir: bool, exists: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = split_ext(name, is_dir);
    let first = format!("{stem} copy{ext}");
    if !exists(&first) {
        return first;
    }
    (2u32..).map(|n| format!("{stem} copy {n}{ext}")).find(|c| !exists(c)).unwrap_or(first)
}

/// New-item name: `untitled`, `untitled 2`, `untitled 3`, …
pub fn unique_name(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    (2u32..).map(|n| format!("{base} {n}")).find(|c| !exists(c)).unwrap_or_else(|| base.to_string())
}

pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn exists_in(dir: &Path) -> impl Fn(&str) -> bool + '_ {
    move |name| exists(&dir.join(name))
}

/// True when `dir` is `ancestor` itself or lies inside it (after resolving symlinks).
pub fn is_same_or_descendant(dir: &Path, ancestor: &Path) -> bool {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(dir).starts_with(canon(ancestor))
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
}

fn fail(path: &Path, err: impl std::fmt::Display) -> Stop {
    Stop::Failed(format!("{}: {err}", name_of(path)))
}

/// Total bytes and number of files (symlinks count as files) under `path`.
pub fn tree_size(path: &Path, ctl: Option<&Control>) -> (u64, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else { return (0, 0) };
    if !meta.is_dir() {
        return (if meta.is_file() { meta.len() } else { 0 }, 1);
    }
    let (mut bytes, mut files) = (0, 0);
    if let Ok(rd) = fs::read_dir(path) {
        for child in rd.flatten() {
            if ctl.is_some_and(|c| c.cancel.load(Ordering::Relaxed)) {
                break;
            }
            let (b, f) = tree_size(&child.path(), ctl);
            bytes += b;
            files += f;
        }
    }
    (bytes, files)
}

/// Delete a file, symlink or whole folder.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}

// ── engine ───────────────────────────────────────────────────────────────────

struct Engine<'a> {
    ctl: &'a Control,
    host: &'a mut dyn Host,
    bytes: u64,
    files: u64,
    buf: Vec<u8>,
}

impl Engine<'_> {
    /// Honour pause (sleeping) and cancel.
    fn checkpoint(&self) -> Result<(), Stop> {
        loop {
            if self.ctl.cancel.load(Ordering::Relaxed) {
                return Err(Stop::Cancelled);
            }
            if !self.ctl.pause.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn report(&mut self) {
        self.host.progress(self.bytes, self.files);
    }

    /// Something other than a folder is in the way of a `Replace`: a folder goes to the
    /// trash (it may hold a lot), a file or symlink is simply removed.
    fn displace(&self, dst: &Path) -> Result<(), Stop> {
        match fs::symlink_metadata(dst) {
            Ok(meta) if meta.is_dir() => trash::delete(dst).map_err(|e| fail(dst, e)),
            Ok(_) => fs::remove_file(dst).map_err(|e| fail(dst, e)),
            Err(_) => Ok(()),
        }
    }

    fn pump(&mut self, input: &mut File, output: &mut File) -> Result<(), Stop> {
        let mut buf = std::mem::take(&mut self.buf);
        let result = (|| -> Result<(), Stop> {
            loop {
                self.checkpoint()?;
                let n = match input.read(&mut buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(Stop::Failed(format!("read failed: {e}"))),
                };
                output.write_all(&buf[..n]).map_err(|e| Stop::Failed(format!("write failed: {e}")))?;
                self.bytes += n as u64;
                self.report();
            }
        })();
        self.buf = buf;
        result
    }

    fn copy_file(&mut self, src: &Path, dst: &Path, meta: &fs::Metadata, replace: bool) -> Result<(), Stop> {
        let occupied = exists(dst);
        if occupied && !replace {
            return Err(fail(dst, "already exists"));
        }
        // A replacement is written beside the target and renamed over it when complete.
        let target = if occupied { dst.with_file_name(format!(".{}.fm-part", name_of(dst))) } else { dst.to_path_buf() };
        if occupied {
            let _ = fs::remove_file(&target);
        }
        let mut input = File::open(src).map_err(|e| fail(src, e))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(meta.mode() & 0o777)
            .open(&target)
            .map_err(|e| fail(&target, e))?;
        let mut result = self.pump(&mut input, &mut output).map_err(|stop| match stop {
            Stop::Failed(msg) => fail(src, msg),
            other => other,
        });
        if result.is_ok() {
            if let Ok(mtime) = meta.modified() {
                let _ = output.set_modified(mtime);
            }
            let _ = fs::set_permissions(&target, meta.permissions());
            drop(output);
            if occupied {
                result = self.displace_dir_only(dst).and_then(|_| fs::rename(&target, dst).map_err(|e| fail(dst, e)));
            }
        }
        if let Err(stop) = result {
            let _ = fs::remove_file(&target);
            return Err(stop);
        }
        self.files += 1;
        self.report();
        Ok(())
    }

    /// Before renaming a finished file over `dst`: a folder there has to go first.
    fn displace_dir_only(&self, dst: &Path) -> Result<(), Stop> {
        match fs::symlink_metadata(dst) {
            Ok(meta) if meta.is_dir() => self.displace(dst),
            _ => Ok(()),
        }
    }

    /// Copy a file, symlink or folder tree to `dst`. Without `replace`, `dst` must not exist.
    fn copy_item(&mut self, src: &Path, dst: &Path, replace: bool) -> Result<(), Stop> {
        self.checkpoint()?;
        let meta = fs::symlink_metadata(src).map_err(|e| fail(src, e))?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            let link = fs::read_link(src).map_err(|e| fail(src, e))?;
            if exists(dst) {
                if !replace {
                    return Err(fail(dst, "already exists"));
                }
                self.displace(dst)?;
            }
            symlink(link, dst).map_err(|e| fail(dst, e))?;
            self.files += 1;
            self.report();
        } else if file_type.is_dir() {
            match fs::symlink_metadata(dst) {
                // Replace folder with folder = merge into it.
                Ok(m) if m.is_dir() && replace => {}
                Ok(_) if replace => {
                    self.displace(dst)?;
                    fs::create_dir(dst).map_err(|e| fail(dst, e))?;
                }
                Ok(_) => return Err(fail(dst, "already exists")),
                Err(_) => fs::create_dir(dst).map_err(|e| fail(dst, e))?,
            }
            for child in fs::read_dir(src).map_err(|e| fail(src, e))? {
                let child = child.map_err(|e| fail(src, e))?;
                self.copy_item(&child.path(), &dst.join(child.file_name()), replace)?;
            }
            let _ = fs::set_permissions(dst, meta.permissions());
            if let (Ok(mtime), Ok(dir)) = (meta.modified(), File::open(dst)) {
                let _ = dir.set_modified(mtime);
            }
        } else if file_type.is_file() {
            self.copy_file(src, dst, &meta, replace)?;
        } else {
            // Sockets, fifos and devices are not copied.
            self.files += 1;
            self.report();
        }
        Ok(())
    }

    /// `rename`, falling back to copy + delete across filesystems.
    fn rename_or_copy(&mut self, src: &Path, dst: &Path, replace: bool) -> Result<(), Stop> {
        if !replace && exists(dst) {
            return Err(fail(dst, "already exists"));
        }
        let (bytes, files) = tree_size(src, None);
        match fs::rename(src, dst) {
            Ok(()) => {
                self.bytes += bytes;
                self.files += files;
                self.report();
                Ok(())
            }
            Err(e) if e.raw_os_error() == Some(EXDEV) => {
                self.copy_item(src, dst, replace)?;
                // The copy is complete (any error returned above): only now drop the source.
                remove_tree(src).map_err(|e| fail(src, format!("copied, but the original could not be removed: {e}")))
            }
            Err(e) => Err(fail(src, e)),
        }
    }

    fn move_item(&mut self, src: &Path, dst: &Path, replace: bool) -> Result<(), Stop> {
        self.checkpoint()?;
        let Ok(dst_meta) = fs::symlink_metadata(dst) else {
            return self.rename_or_copy(src, dst, false);
        };
        if !replace {
            return Err(fail(dst, "already exists"));
        }
        let src_meta = fs::symlink_metadata(src).map_err(|e| fail(src, e))?;
        if src_meta.is_dir() && dst_meta.is_dir() {
            // Merge folder into folder.
            for child in fs::read_dir(src).map_err(|e| fail(src, e))? {
                let child = child.map_err(|e| fail(src, e))?;
                self.move_item(&child.path(), &dst.join(child.file_name()), true)?;
            }
            let _ = fs::remove_dir(src);
            Ok(())
        } else {
            if src_meta.is_dir() || dst_meta.is_dir() {
                self.displace(dst)?;
            }
            self.rename_or_copy(src, dst, true)
        }
    }

    fn link_item(&mut self, src: &Path, dst: &Path, replace: bool) -> Result<(), Stop> {
        if exists(dst) {
            if !replace {
                return Err(fail(dst, "already exists"));
            }
            self.displace(dst)?;
        }
        symlink(src, dst).map_err(|e| fail(dst, e))?;
        self.files += 1;
        self.report();
        Ok(())
    }
}

/// Run a whole job on the calling thread.
pub fn run_transfer(
    sources: &[PathBuf],
    dest_dir: &Path,
    kind: TransferKind,
    ctl: &Control,
    host: &mut dyn Host,
) -> Result<Outcome, Stop> {
    if !dest_dir.is_dir() {
        return Err(Stop::Failed(format!("{} is not a folder", dest_dir.display())));
    }
    let dest_canon = fs::canonicalize(dest_dir).map_err(|e| fail(dest_dir, e))?;
    let verb = match kind {
        TransferKind::Copy => "copy",
        TransferKind::Move => "move",
        TransferKind::Link => "link",
    };

    // Pre-flight: every source exists, and no folder goes into itself.
    for src in sources {
        let meta = fs::symlink_metadata(src).map_err(|_| Stop::Failed(format!("{} no longer exists", name_of(src))))?;
        if kind != TransferKind::Link && meta.is_dir() && is_same_or_descendant(&dest_canon, src) {
            return Err(Stop::Failed(format!("Can't {verb} “{}” into itself", name_of(src))));
        }
    }

    // Scan for totals.
    let (mut total_bytes, mut total_files) = (0, 0);
    for src in sources {
        if ctl.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        if kind == TransferKind::Link {
            total_files += 1;
        } else {
            let (b, f) = tree_size(src, Some(ctl));
            total_bytes += b;
            total_files += f;
        }
    }
    host.totals(total_bytes, total_files);

    let in_dest = |src: &Path| src.parent().and_then(|p| fs::canonicalize(p).ok()).as_deref() == Some(&*dest_canon);
    let collides = |src: &Path| !in_dest(src) && src.file_name().is_some_and(|n| exists(&dest_dir.join(n)));

    let mut engine = Engine { ctl, host, bytes: 0, files: 0, buf: vec![0; CHUNK] };
    let mut outcome = Outcome::default();
    let mut sticky: Option<Resolution> = None;
    let mut asked = 0;
    let mut skipped_files = 0;

    for (ix, src) in sources.iter().enumerate() {
        engine.checkpoint()?;
        let Some(name) = src.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        let is_dir = fs::symlink_metadata(src).is_ok_and(|m| m.is_dir());
        let mut dst = dest_dir.join(&name);
        let mut replace = false;

        if in_dest(src) {
            if kind == TransferKind::Move {
                // Already there.
                outcome.skipped += 1;
                continue;
            }
            // Duplicate in place: never ask.
            dst = dest_dir.join(copy_name(&name, is_dir, exists_in(dest_dir)));
        } else if exists(&dst) {
            let mut resolution = sticky.clone();
            let mut counted = false;
            let skip = loop {
                let current = match resolution.take() {
                    Some(r) => r,
                    None => {
                        if !counted {
                            asked += 1;
                            counted = true;
                        }
                        let conflict = ConflictInfo {
                            src: FileInfo::read(src),
                            dst: FileInfo::read(&dst),
                            index: asked,
                            remaining: sources[ix + 1..].iter().filter(|s| collides(s)).count(),
                        };
                        let Some(reply) = engine.host.conflict(conflict) else { return Err(Stop::Cancelled) };
                        engine.checkpoint()?;
                        if reply.apply_to_all && !matches!(reply.resolution, Resolution::Rename(_)) {
                            sticky = Some(reply.resolution.clone());
                        }
                        reply.resolution
                    }
                };
                match current {
                    Resolution::Skip => break true,
                    Resolution::KeepBoth => {
                        dst = dest_dir.join(numbered_name(&name, is_dir, exists_in(dest_dir)));
                        break false;
                    }
                    Resolution::Rename(new_name) => {
                        let new_name = new_name.trim().to_string();
                        if new_name.is_empty() || new_name.contains('/') || new_name == "." || new_name == ".." {
                            continue; // ask again
                        }
                        dst = dest_dir.join(new_name);
                        if !exists(&dst) {
                            break false;
                        }
                        // Taken as well: ask again, about the new name.
                    }
                    Resolution::Replace => {
                        replace = exists(&dst);
                        break false;
                    }
                }
            };
            if skip {
                outcome.skipped += 1;
                let (b, f) = if kind == TransferKind::Link { (0, 1) } else { tree_size(src, None) };
                engine.bytes += b;
                engine.files += f;
                skipped_files += f;
                engine.report();
                continue;
            }
        }

        match kind {
            TransferKind::Copy => engine.copy_item(src, &dst, replace)?,
            TransferKind::Move => engine.move_item(src, &dst, replace)?,
            TransferKind::Link => engine.link_item(src, &dst, replace)?,
        }
        outcome.items += 1;
        if replace {
            outcome.replaced += 1;
        } else {
            outcome.created.push(dst.clone());
            if kind == TransferKind::Move {
                outcome.moved.push((src.clone(), dst));
            }
        }
    }
    outcome.files = engine.files - skipped_files;
    engine.report();
    Ok(outcome)
}

/// Undo of a move: put `to` back at `from` (skipped when `from` is taken again).
pub fn move_back(pairs: &[(PathBuf, PathBuf)]) -> Result<usize, String> {
    struct Quiet;
    impl Host for Quiet {
        fn totals(&mut self, _: u64, _: u64) {}
        fn progress(&mut self, _: u64, _: u64) {}
        fn conflict(&mut self, _: ConflictInfo) -> Option<Reply> {
            None
        }
    }
    let ctl = Control::default();
    let mut engine = Engine { ctl: &ctl, host: &mut Quiet, bytes: 0, files: 0, buf: vec![0; CHUNK] };
    let mut restored = 0;
    for (from, to) in pairs {
        if exists(from) || !exists(to) {
            continue;
        }
        match engine.rename_or_copy(to, from, false) {
            Ok(()) => restored += 1,
            Err(Stop::Failed(msg)) => return Err(msg),
            Err(Stop::Cancelled) => break,
        }
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct TestHost {
        replies: VecDeque<Reply>,
        conflicts: Vec<ConflictInfo>,
        totals: (u64, u64),
        last: (u64, u64),
    }

    impl TestHost {
        fn new(replies: Vec<Reply>) -> Self {
            TestHost { replies: replies.into(), conflicts: Vec::new(), totals: (0, 0), last: (0, 0) }
        }
    }

    impl Host for TestHost {
        fn totals(&mut self, bytes: u64, files: u64) {
            self.totals = (bytes, files);
        }
        fn progress(&mut self, bytes: u64, files: u64) {
            self.last = (bytes, files);
        }
        fn conflict(&mut self, conflict: ConflictInfo) -> Option<Reply> {
            self.conflicts.push(conflict);
            self.replies.pop_front()
        }
    }

    fn reply(resolution: Resolution, apply_to_all: bool) -> Reply {
        Reply { resolution, apply_to_all }
    }

    /// Fresh scratch directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("fm-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.path(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
        fn file(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.path(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(sources: &[PathBuf], dest: &Path, kind: TransferKind, host: &mut TestHost) -> Result<Outcome, Stop> {
        run_transfer(sources, dest, kind, &Control::default(), host)
    }

    #[test]
    fn splits_extensions() {
        assert_eq!(split_ext("notes.md", false), ("notes", ".md"));
        assert_eq!(split_ext("dots.tar.zst", false), ("dots", ".tar.zst"));
        assert_eq!(split_ext(".bashrc", false), (".bashrc", ""));
        assert_eq!(split_ext("Makefile", false), ("Makefile", ""));
        assert_eq!(split_ext("v1.2", true), ("v1.2", ""));
        assert_eq!(split_ext("фото.JPG", false), ("фото", ".JPG"));
    }

    #[test]
    fn keep_both_names() {
        let taken = ["palette.png", "palette (1).png"];
        let exists = |n: &str| taken.contains(&n);
        assert_eq!(numbered_name("palette.png", false, exists), "palette (2).png");
        assert_eq!(numbered_name("src", true, |_| false), "src (1)");
        assert_eq!(numbered_name("a.tar.gz", false, |_| false), "a (1).tar.gz");
    }

    #[test]
    fn copy_names() {
        assert_eq!(copy_name("a.txt", false, |_| false), "a copy.txt");
        let taken = ["a copy.txt", "a copy 2.txt"];
        assert_eq!(copy_name("a.txt", false, |n| taken.contains(&n)), "a copy 3.txt");
        assert_eq!(copy_name("dir.d", true, |_| false), "dir.d copy");
    }

    #[test]
    fn unique_names() {
        assert_eq!(unique_name("untitled", |_| false), "untitled");
        let taken = ["untitled", "untitled 2"];
        assert_eq!(unique_name("untitled", |n| taken.contains(&n)), "untitled 3");
        assert_eq!(unique_name("untitled folder", |n| n == "untitled folder"), "untitled folder 2");
    }

    #[test]
    fn descendant_check() {
        let s = Scratch::new("desc");
        let a = s.dir("a");
        let inner = s.dir("a/b/c");
        let other = s.dir("ab");
        assert!(is_same_or_descendant(&a, &a));
        assert!(is_same_or_descendant(&inner, &a));
        assert!(!is_same_or_descendant(&other, &a));
        assert!(!is_same_or_descendant(&a, &inner));
        // Through a symlink.
        let link = s.path("link");
        symlink(&inner, &link).unwrap();
        assert!(is_same_or_descendant(&link, &a));
    }

    #[test]
    fn copies_a_tree() {
        let s = Scratch::new("copy");
        let src = s.dir("src/tree");
        s.file("src/tree/a.txt", "alpha");
        s.file("src/tree/sub/b.bin", &"x".repeat(300_000));
        s.dir("src/tree/empty");
        symlink("a.txt", src.join("link")).unwrap();
        fs::set_permissions(src.join("a.txt"), std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
        let dest = s.dir("dest");

        let mut host = TestHost::new(vec![]);
        let out = run(std::slice::from_ref(&src), &dest, TransferKind::Copy, &mut host).unwrap();

        assert_eq!(out.created, vec![dest.join("tree")]);
        assert_eq!(out.files, 3);
        assert_eq!(host.totals, (300_005, 3));
        assert_eq!(host.last, (300_005, 3));
        assert!(host.conflicts.is_empty());
        assert_eq!(fs::read_to_string(dest.join("tree/a.txt")).unwrap(), "alpha");
        assert_eq!(fs::metadata(dest.join("tree/sub/b.bin")).unwrap().len(), 300_000);
        assert!(dest.join("tree/empty").is_dir());
        assert_eq!(fs::read_link(dest.join("tree/link")).unwrap(), PathBuf::from("a.txt"));
        assert_eq!(fs::metadata(dest.join("tree/a.txt")).unwrap().mode() & 0o777, 0o640);
        let (m1, m2) = (
            fs::metadata(src.join("a.txt")).unwrap().modified().unwrap(),
            fs::metadata(dest.join("tree/a.txt")).unwrap().modified().unwrap(),
        );
        assert_eq!(m1, m2);
        // Source untouched.
        assert!(src.join("sub/b.bin").exists());
    }

    #[test]
    fn copy_into_own_folder_keeps_both_without_asking() {
        let s = Scratch::new("dup");
        let f = s.file("a.txt", "one");
        let d = s.dir("folder");
        let mut host = TestHost::new(vec![]);
        run(&[f.clone(), d.clone()], &s.0, TransferKind::Copy, &mut host).unwrap();
        let out = run(&[f], &s.0, TransferKind::Copy, &mut host).unwrap();
        assert!(host.conflicts.is_empty());
        assert_eq!(out.created, vec![s.path("a copy 2.txt")]);
        assert!(s.path("a copy.txt").is_file());
        assert!(s.path("folder copy").is_dir());
    }

    #[test]
    fn refuses_to_copy_a_folder_into_itself() {
        let s = Scratch::new("self");
        let a = s.dir("a");
        let inner = s.dir("a/inner");
        s.file("a/f", "x");
        let mut host = TestHost::new(vec![]);
        for dest in [&a, &inner] {
            for kind in [TransferKind::Copy, TransferKind::Move] {
                let err = run(std::slice::from_ref(&a), dest, kind, &mut host).unwrap_err();
                assert!(matches!(err, Stop::Failed(ref m) if m.contains("into itself")), "{err:?}");
            }
        }
        assert!(s.path("a/f").exists());
        assert_eq!(fs::read_dir(&inner).unwrap().count(), 0);
    }

    #[test]
    fn moves_without_conflict() {
        let s = Scratch::new("move");
        let f = s.file("from/a.txt", "alpha");
        let d = s.dir("from/dir");
        s.file("from/dir/b.txt", "beta");
        let dest = s.dir("to");
        let mut host = TestHost::new(vec![]);
        let out = run(&[f.clone(), d.clone()], &dest, TransferKind::Move, &mut host).unwrap();
        assert!(!f.exists() && !d.exists());
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(dest.join("dir/b.txt")).unwrap(), "beta");
        assert_eq!(out.moved, vec![(f.clone(), dest.join("a.txt")), (d.clone(), dest.join("dir"))]);
        assert_eq!(host.last, host.totals);

        // …and back again.
        assert_eq!(move_back(&out.moved), Ok(2));
        assert!(f.exists() && d.join("b.txt").exists());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn move_within_same_folder_is_a_no_op() {
        let s = Scratch::new("noop");
        let f = s.file("a.txt", "alpha");
        let mut host = TestHost::new(vec![]);
        let out = run(std::slice::from_ref(&f), &s.0, TransferKind::Move, &mut host).unwrap();
        assert_eq!((out.items, out.skipped), (0, 1));
        assert_eq!(fs::read_to_string(f).unwrap(), "alpha");
    }

    #[test]
    fn conflicts_are_asked_and_resolved() {
        let s = Scratch::new("conflict");
        let sources: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt", "d.txt"].iter().map(|n| s.file(&format!("in/{n}"), "new")).collect();
        let dest = s.dir("out");
        for n in ["a.txt", "b.txt", "c.txt", "d.txt", "taken.txt"] {
            s.file(&format!("out/{n}"), "old");
        }
        let mut host = TestHost::new(vec![
            reply(Resolution::Skip, false),
            reply(Resolution::Rename("taken.txt".into()), false),
            reply(Resolution::Rename("fresh.txt".into()), false),
            reply(Resolution::KeepBoth, false),
            reply(Resolution::Replace, false),
        ]);
        let out = run(&sources, &dest, TransferKind::Copy, &mut host).unwrap();

        assert_eq!(host.conflicts.len(), 5);
        assert_eq!((host.conflicts[0].index, host.conflicts[0].remaining), (1, 3));
        assert_eq!(host.conflicts[2].dst.name, "taken.txt");
        assert_eq!(host.conflicts[4].remaining, 0);
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("b.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("taken.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("fresh.txt")).unwrap(), "new");
        assert_eq!(fs::read_to_string(dest.join("c (1).txt")).unwrap(), "new");
        assert_eq!(fs::read_to_string(dest.join("c.txt")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("d.txt")).unwrap(), "new");
        assert_eq!((out.skipped, out.replaced, out.items), (1, 1, 3));
        assert_eq!(out.created, vec![dest.join("fresh.txt"), dest.join("c (1).txt")]);
        assert!(!dest.join(".d.txt.fm-part").exists());
    }

    #[test]
    fn apply_to_all_is_sticky_and_unanswered_conflict_cancels() {
        let s = Scratch::new("sticky");
        let sources: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| s.file(&format!("in/{n}"), "new")).collect();
        let dest = s.dir("out");
        for n in ["a", "b", "c"] {
            s.file(&format!("out/{n}"), "old");
        }
        let mut host = TestHost::new(vec![reply(Resolution::Skip, true)]);
        let out = run(&sources, &dest, TransferKind::Move, &mut host).unwrap();
        assert_eq!(host.conflicts.len(), 1);
        assert_eq!(out.skipped, 3);
        assert!(sources.iter().all(|p| p.exists()));

        // No reply = cancel; nothing is touched.
        let mut host = TestHost::new(vec![]);
        assert_eq!(run(&sources, &dest, TransferKind::Move, &mut host).unwrap_err(), Stop::Cancelled);
        assert_eq!(fs::read_to_string(dest.join("a")).unwrap(), "old");
        assert!(sources[0].exists());
    }

    #[test]
    fn replace_merges_folders_on_move() {
        let s = Scratch::new("merge");
        let src = s.dir("in/d");
        s.file("in/d/same", "new");
        s.file("in/d/only-src", "new");
        let dest = s.dir("out");
        s.file("out/d/same", "old");
        s.file("out/d/only-dst", "old");
        let mut host = TestHost::new(vec![reply(Resolution::Replace, false)]);
        let out = run(std::slice::from_ref(&src), &dest, TransferKind::Move, &mut host).unwrap();
        assert_eq!(out.replaced, 1);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dest.join("d/same")).unwrap(), "new");
        assert_eq!(fs::read_to_string(dest.join("d/only-src")).unwrap(), "new");
        assert_eq!(fs::read_to_string(dest.join("d/only-dst")).unwrap(), "old");
    }

    #[test]
    fn cancel_stops_before_copying() {
        let s = Scratch::new("cancel");
        let f = s.file("in/a", "data");
        let dest = s.dir("out");
        let ctl = Control::default();
        ctl.cancel.store(true, Ordering::Relaxed);
        let mut host = TestHost::new(vec![]);
        assert_eq!(run_transfer(&[f], &dest, TransferKind::Copy, &ctl, &mut host).unwrap_err(), Stop::Cancelled);
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn links_point_at_the_source() {
        let s = Scratch::new("link");
        let f = s.file("in/a", "data");
        let dest = s.dir("out");
        let mut host = TestHost::new(vec![]);
        let out = run(std::slice::from_ref(&f), &dest, TransferKind::Link, &mut host).unwrap();
        assert_eq!(fs::read_link(dest.join("a")).unwrap(), f);
        assert_eq!(out.created, vec![dest.join("a")]);
    }
}
