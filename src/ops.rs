//! File operations engine (SPEC §7): the y/x/p clipboard, copy / move / link jobs with
//! progress, trash, permanent delete, new file / folder, opening files externally.
//! UI for jobs / conflicts / toasts lives in overlays.rs.
//!
//! OWNER: ops agent. STUB — only `open_external` works.

use std::path::{Path, PathBuf};

use gpui::{App, Context, Window};

use crate::{
    actions::{Copy, Cut, DeletePermanently, NewFile, NewFolder, Paste, Trash},
    app::FmApp,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipMode {
    Copy,
    Cut,
}

#[derive(Clone, Debug)]
pub struct Clipboard {
    pub mode: ClipMode,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransferKind {
    Copy,
    Move,
    Link,
}

pub struct OpsState {
    /// Internal file clipboard (`y` / `x`).
    pub clipboard: Option<Clipboard>,
}

impl OpsState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        OpsState { clipboard: None }
    }

    /// True when `path` was cut (`x`) — the list draws it at 50% opacity, italic.
    pub fn is_cut(&self, path: &Path) -> bool {
        self.clipboard.as_ref().is_some_and(|c| c.mode == ClipMode::Cut && c.paths.iter().any(|p| p == path))
    }

    /// Running jobs started from the tab with this id (tab-strip badge).
    pub fn jobs_for_tab(&self, _tab_id: u64) -> usize {
        0
    }
}

/// Open a file with the default application.
pub fn open_external(path: &Path, _cx: &mut App) {
    let _ = std::process::Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Copy / move / link `sources` into `dest_dir` as a background job (paste, drag & drop).
pub fn start_transfer(
    _app: &mut FmApp,
    _sources: Vec<PathBuf>,
    _dest_dir: PathBuf,
    _kind: TransferKind,
    _cx: &mut Context<FmApp>,
) {
}

/// Move `paths` to the XDG trash.
pub fn trash_paths(_app: &mut FmApp, _paths: Vec<PathBuf>, _cx: &mut Context<FmApp>) {}

pub fn act_copy(_app: &mut FmApp, _: &Copy, _window: &mut Window, _cx: &mut Context<FmApp>) {}
pub fn act_cut(_app: &mut FmApp, _: &Cut, _window: &mut Window, _cx: &mut Context<FmApp>) {}
pub fn act_paste(_app: &mut FmApp, _: &Paste, _window: &mut Window, _cx: &mut Context<FmApp>) {}
pub fn act_trash(_app: &mut FmApp, _: &Trash, _window: &mut Window, _cx: &mut Context<FmApp>) {}
pub fn act_delete_permanently(_app: &mut FmApp, _: &DeletePermanently, _window: &mut Window, _cx: &mut Context<FmApp>) {}
pub fn act_new_file(_app: &mut FmApp, _: &NewFile, _window: &mut Window, _cx: &mut Context<FmApp>) {}
pub fn act_new_folder(_app: &mut FmApp, _: &NewFolder, _window: &mut Window, _cx: &mut Context<FmApp>) {}
