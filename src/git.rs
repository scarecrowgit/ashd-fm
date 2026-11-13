//! Git integration for the current directory: per-file status letters (list view git
//! column), branch / ahead / modified (footer), recent commits (folder preview).
//!
//! OWNER: sidebar agent. STUB — `refresh` does nothing yet.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use gpui::{Context, Hsla, SharedString};

use crate::{app::FmApp, theme::Theme};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitStatus {
    Modified,
    Untracked,
    Added,
    Deleted,
}

impl GitStatus {
    /// Letter for the git column (SPEC §4.3): colour is never the only signal.
    pub fn letter(self) -> &'static str {
        match self {
            GitStatus::Modified => "M",
            GitStatus::Untracked => "?",
            GitStatus::Added => "A",
            GitStatus::Deleted => "D",
        }
    }

    pub fn color(self, t: &Theme) -> Hsla {
        match self {
            GitStatus::Modified => t.warn,
            GitStatus::Untracked => t.info,
            GitStatus::Added => t.ok,
            GitStatus::Deleted => t.err,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: SharedString,
    pub subject: SharedString,
    /// Relative age, e.g. `2h`, `3d`.
    pub age: SharedString,
}

#[derive(Default)]
pub struct GitState {
    /// Repository root containing the active tab's directory, if any.
    pub root: Option<PathBuf>,
    pub branch: Option<SharedString>,
    pub ahead: usize,
    pub behind: usize,
    /// Number of modified/untracked paths in the repo.
    pub modified: usize,
    /// Status by absolute path. Directories carry the "strongest" status of their contents.
    pub statuses: HashMap<PathBuf, GitStatus>,
    /// Last few commits (newest first).
    pub commits: Vec<Commit>,
}

impl GitState {
    pub fn status_for(&self, path: &Path) -> Option<GitStatus> {
        self.statuses.get(path).copied()
    }
}

/// Called by the shell after the active tab's directory has (re)loaded.
/// Must not block: run `git` on the background executor and `cx.notify()` when done.
pub fn refresh(_app: &mut FmApp, _cx: &mut Context<FmApp>) {}
