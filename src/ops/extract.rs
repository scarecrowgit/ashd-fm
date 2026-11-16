//! "Extract here": unpacks archives next to themselves, in the background.
//!
//! Built in: zip/jar, tar, tar.gz/zst/xz/bz2 and single compressed files (`x.log.gz` → `x.log`).
//! 7z and rar (and zips using a method the zip crate lacks) go through `7z`/`7zz`, `bsdtar`,
//! `unrar` or `unzip`, whichever is installed.
//!
//! Everything is unpacked into a hidden scratch folder beside the archive first. An archive
//! holding a single top-level item then puts that item next to the archive; anything else
//! becomes a folder named after the archive (`photos.zip` → `photos/`, numbered if taken).
//! Nothing that already exists is ever overwritten.

use std::{
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use gpui::{AppContext, Context};

use crate::{
    app::FmApp,
    ops::{exists, name_of, numbered_name, plural},
    overlays::{self, Toast},
    preview_content::{ArchiveFormat, tar_reader},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Zip,
    Tar(ArchiveFormat),
    /// One compressed file, decoded with the codec of this tar flavour.
    Single(ArchiveFormat),
    SevenZip,
    Rar,
}

/// The archive kind of a file name and the length of its suffix.
pub fn detect(name: &str) -> Option<(Kind, usize)> {
    if let Some((format, len)) = ArchiveFormat::detect(name) {
        let kind = if format == ArchiveFormat::Zip { Kind::Zip } else { Kind::Tar(format) };
        return Some((kind, len));
    }
    const OTHER: [(&str, Kind); 6] = [
        (".7z", Kind::SevenZip),
        (".rar", Kind::Rar),
        (".gz", Kind::Single(ArchiveFormat::TarGz)),
        (".zst", Kind::Single(ArchiveFormat::TarZst)),
        (".xz", Kind::Single(ArchiveFormat::TarXz)),
        (".bz2", Kind::Single(ArchiveFormat::TarBz2)),
    ];
    let lower = name.to_lowercase();
    OTHER.iter().find(|(suffix, _)| lower.ends_with(suffix)).map(|(suffix, kind)| (*kind, suffix.len()))
}

pub fn is_archive(path: &Path) -> bool {
    path.is_file() && path.file_name().is_some_and(|n| detect(&n.to_string_lossy()).is_some())
}

/// Unpack every archive among `paths`; other paths are ignored.
pub fn extract(app: &mut FmApp, paths: Vec<PathBuf>, cx: &mut Context<FmApp>) {
    let archives: Vec<PathBuf> = paths.into_iter().filter(|p| is_archive(p)).collect();
    if archives.is_empty() {
        return overlays::toast_error(app, "Nothing to extract · not an archive", cx);
    }
    let what = match archives.as_slice() {
        [one] => name_of(one),
        many => plural(many.len() as u64, "archive"),
    };
    overlays::toast(app, format!("Extracting {what}…"), cx);
    let task = cx.background_spawn({
        let archives = archives.clone();
        async move { archives.iter().map(|a| extract_one(a).map_err(|e| format!("{} · {e}", name_of(a)))).collect::<Vec<_>>() }
    });
    cx.spawn(async move |this, cx| {
        let results = task.await;
        this.update(cx, |app, cx| {
            let done: Vec<&PathBuf> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
            if let Some(err) = results.iter().find_map(|r| r.as_ref().err()) {
                overlays::toast_error(app, format!("Extract failed · {err}"), cx);
            } else if let [one] = done.as_slice() {
                let parent = one.parent().map(Path::to_path_buf).unwrap_or_default();
                let text = format!("Extracted to {}", name_of(one));
                let open = Some((parent, Some((*one).clone())));
                overlays::show_toast(app, Toast { text: text.into(), error: false, open, undo: None }, cx);
            } else {
                overlays::toast(app, format!("Extracted {}", plural(done.len() as u64, "archive")), cx);
            }
            // Show the result: cursor on it when there is exactly one, else just refresh.
            match done.as_slice() {
                _ if app.in_tag_view() => app.reload(cx),
                [one] if one.parent() == Some(app.cwd()) => app.reload_and_focus((*one).clone(), false, cx),
                _ if archives.iter().any(|a| a.parent() == Some(app.cwd())) => app.reload(cx),
                _ => {}
            }
        })
        .ok();
    })
    .detach();
}

/// Unpack one archive beside itself; returns what was created.
pub fn extract_one(archive: &Path) -> Result<PathBuf, String> {
    extract_into(archive, archive.parent().ok_or("no parent folder")?)
}

/// Unpack `archive` into the folder `parent` (BLOCKING); returns what was created there.
pub fn extract_into(archive: &Path, parent: &Path) -> Result<PathBuf, String> {
    let name = name_of(archive);
    let (kind, suffix) = detect(&name).ok_or("not an archive")?;
    let stem = match &name[..name.len() - suffix] {
        "" => "archive",
        stem => stem,
    };
    let free = |base: &str, is_dir| {
        if exists(&parent.join(base)) { numbered_name(base, is_dir, |n| exists(&parent.join(n))) } else { base.to_string() }
    };

    if let Kind::Single(codec) = kind {
        let dest = parent.join(free(stem, false));
        let part = scratch_path(parent, stem);
        let result = (|| -> io::Result<()> {
            let mut reader = tar_reader(codec, fs::File::open(archive)?)?;
            io::copy(&mut reader, &mut fs::File::create_new(&part)?)?;
            fs::rename(&part, &dest)
        })();
        return match result {
            Ok(()) => Ok(dest),
            Err(err) => {
                let _ = fs::remove_file(&part);
                Err(err.to_string())
            }
        };
    }

    let scratch = scratch_path(parent, stem);
    fs::create_dir(&scratch).map_err(|e| e.to_string())?;
    let placed = unpack(archive, kind, &scratch).and_then(|()| {
        let items: Vec<_> = fs::read_dir(&scratch).map_err(|e| e.to_string())?.flatten().map(|e| e.file_name()).collect();
        match items.as_slice() {
            [] => Err("the archive is empty".to_string()),
            [only] if !exists(&parent.join(only)) => {
                let dest = parent.join(only);
                fs::rename(scratch.join(only), &dest).map_err(|e| e.to_string())?;
                let _ = fs::remove_dir(&scratch);
                Ok(dest)
            }
            _ => {
                let dest = parent.join(free(stem, true));
                fs::rename(&scratch, &dest).map_err(|e| e.to_string())?;
                Ok(dest)
            }
        }
    });
    if placed.is_err() {
        let _ = fs::remove_dir_all(&scratch);
    }
    placed
}

/// A hidden, unique name beside the archive for work in progress.
fn scratch_path(parent: &Path, stem: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{stem}.ashd-fm-extract-{}-{n}", std::process::id()))
}

fn unpack(archive: &Path, kind: Kind, dest: &Path) -> Result<(), String> {
    match kind {
        Kind::Zip => unzip(archive, dest).or_else(|err| {
            // e.g. a compression method the zip crate was built without: try the tools.
            fs::remove_dir_all(dest).and_then(|()| fs::create_dir(dest)).map_err(|e| e.to_string())?;
            external(archive, kind, dest).map_err(|_| err)
        }),
        Kind::Tar(format) => {
            let reader = tar_reader(format, fs::File::open(archive).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let mut tar = tar::Archive::new(reader);
            tar.set_overwrite(false);
            // `unpack` skips absolute and `..` paths and never writes through a symlink
            // that leads outside `dest`.
            tar.unpack(dest).map_err(|e| e.to_string())
        }
        Kind::SevenZip | Kind::Rar => external(archive, kind, dest),
        Kind::Single(_) => unreachable!("single files are decoded by extract_one"),
    }
}

fn unzip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(io::BufReader::new(file)).map_err(|e| e.to_string())?;
    // Symlinks are created last, so no file is ever written through one.
    let mut links = Vec::new();
    for ix in 0..zip.len() {
        let mut entry = zip.by_index(ix).map_err(|e| e.to_string())?;
        let Some(relative) = entry.enclosed_name() else { continue }; // absolute or `..`
        let out = dest.join(relative);
        let io_err = |e: io::Error| format!("{}: {e}", entry_name(&out, dest));
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(io_err)?;
            continue;
        }
        if let Some(dir) = out.parent() {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        if entry.is_symlink() {
            let mut target = String::new();
            entry.read_to_string(&mut target).map_err(io_err)?;
            links.push((out, target));
            continue;
        }
        let mut file = fs::File::create_new(&out).map_err(io_err)?;
        io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
        if let Some(mode) = entry.unix_mode() {
            let _ = fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o777));
        }
    }
    for (path, target) in links {
        let _ = std::os::unix::fs::symlink(target, path);
    }
    Ok(())
}

fn entry_name(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// Unpack with the first installed external tool that handles `kind`.
fn external(archive: &Path, kind: Kind, dest: &Path) -> Result<(), String> {
    let tools: &[&str] = match kind {
        Kind::Rar => &["unrar", "7z", "7zz", "bsdtar"],
        Kind::Zip => &["7z", "7zz", "bsdtar", "unzip"],
        _ => &["7z", "7zz", "bsdtar"],
    };
    let Some(tool) = tools.iter().find(|t| on_path(t)) else {
        return Err(format!("needs one of: {}", tools.join(", ")));
    };
    let mut command = Command::new(tool);
    match *tool {
        "7z" | "7zz" => command.args(["x", "-y", "-bd"]).arg(format!("-o{}", dest.display())).arg("--").arg(archive),
        "unrar" => command.args(["x", "-o-", "-y", "--"]).arg(archive).arg(format!("{}/", dest.display())),
        "unzip" => command.arg("-q").arg(archive).arg("-d").arg(dest),
        _ => command.arg("-xf").arg(archive).arg("-C").arg(dest),
    };
    let output = command.stdin(Stdio::null()).output().map_err(|e| format!("{tool}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim();
    Err(format!("{tool}: {last}"))
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ashd-fm-extract-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_zip(path: &Path, files: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn detects_kinds() {
        assert_eq!(detect("a.tar.gz"), Some((Kind::Tar(ArchiveFormat::TarGz), 7)));
        assert_eq!(detect("a.ZIP"), Some((Kind::Zip, 4)));
        assert_eq!(detect("log.txt.gz"), Some((Kind::Single(ArchiveFormat::TarGz), 3)));
        assert_eq!(detect("x.7z"), Some((Kind::SevenZip, 3)));
        assert_eq!(detect("notes.txt"), None);
    }

    #[test]
    fn zip_with_many_items_goes_into_a_folder() {
        let dir = scratch("many");
        write_zip(&dir.join("photos.zip"), &[("a.txt", "A"), ("sub/b.txt", "B")]);
        let out = extract_one(&dir.join("photos.zip")).unwrap();
        assert_eq!(out, dir.join("photos"));
        assert_eq!(fs::read_to_string(out.join("sub/b.txt")).unwrap(), "B");
        // Again: the name is taken, so the next one is numbered.
        assert_eq!(extract_one(&dir.join("photos.zip")).unwrap(), dir.join("photos (1)"));
        assert!(fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with('.')));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn single_top_level_folder_lands_beside_the_archive() {
        let dir = scratch("single");
        write_zip(&dir.join("pkg-1.0.zip"), &[("pkg/readme", "hi"), ("pkg/src/x", "x")]);
        assert_eq!(extract_one(&dir.join("pkg-1.0.zip")).unwrap(), dir.join("pkg"));
        assert_eq!(fs::read_to_string(dir.join("pkg/readme")).unwrap(), "hi");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zip_slip_entries_are_skipped() {
        let dir = scratch("slip");
        fs::create_dir(dir.join("inner")).unwrap();
        write_zip(&dir.join("inner/evil.zip"), &[("../../escaped.txt", "x"), ("ok.txt", "ok")]);
        let out = extract_one(&dir.join("inner/evil.zip")).unwrap();
        assert_eq!(out, dir.join("inner/ok.txt"));
        assert!(!dir.join("escaped.txt").exists() && !dir.parent().unwrap().join("escaped.txt").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tar_gz_and_single_gz() {
        let dir = scratch("tgz");
        let gz = flate2::write::GzEncoder::new(fs::File::create(dir.join("t.tar.gz")).unwrap(), Default::default());
        let mut tar = tar::Builder::new(gz);
        for (name, body) in [("one.txt", "1"), ("two.txt", "2")] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, body.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        assert_eq!(extract_one(&dir.join("t.tar.gz")).unwrap(), dir.join("t"));
        assert_eq!(fs::read_to_string(dir.join("t/two.txt")).unwrap(), "2");

        let mut gz = flate2::write::GzEncoder::new(fs::File::create(dir.join("log.txt.gz")).unwrap(), Default::default());
        gz.write_all(b"line").unwrap();
        gz.finish().unwrap();
        assert_eq!(extract_one(&dir.join("log.txt.gz")).unwrap(), dir.join("log.txt"));
        assert_eq!(fs::read_to_string(dir.join("log.txt")).unwrap(), "line");
        fs::remove_dir_all(dir).unwrap();
    }
}
