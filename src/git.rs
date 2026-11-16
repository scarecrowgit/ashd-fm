//! Git integration for the current directory: per-file status letters (list view git
//! column), branch / ahead / modified (footer), recent commits (folder preview).
//!
//! OWNER: sidebar agent. `refresh` shells out to the `git` CLI on the background executor;
//! when `git` is missing or the directory is not in a repository the state is simply empty.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui::{AppContext as _, Context, Hsla, SharedString};

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
    /// Bumped per refresh; a result is applied only if it is still the newest request.
    generation: u64,
    /// Directory and time of the last refresh that was started (throttle).
    last: Option<(PathBuf, Instant)>,
}

impl GitState {
    pub fn status_for(&self, path: &Path) -> Option<GitStatus> {
        self.statuses.get(path).copied()
    }
}

/// Minimum interval between two refreshes of the same directory.
const THROTTLE: Duration = Duration::from_secs(1);

/// Called by the shell after the active tab's directory has (re)loaded.
/// Must not block: run `git` on the background executor and `cx.notify()` when done.
pub fn refresh(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let cwd = app.cwd().to_path_buf();
    if crate::fs_model::tag_of(&cwd).is_some() {
        // Not a folder: no repository. Drops any result still in flight.
        app.git = GitState { generation: app.git.generation + 1, ..Default::default() };
        cx.notify();
        return;
    }
    if let Some((last_cwd, at)) = &app.git.last
        && *last_cwd == cwd
        && at.elapsed() < THROTTLE
    {
        return;
    }
    app.git.last = Some((cwd.clone(), Instant::now()));
    app.git.generation += 1;
    let generation = app.git.generation;

    let task = cx.background_spawn(async move { scan(&cwd) });
    cx.spawn(async move |this, cx| {
        let snapshot = task.await;
        this.update(cx, |this, cx| {
            let git = &mut this.git;
            if git.generation != generation {
                return;
            }
            let snapshot = snapshot.unwrap_or_default();
            git.root = snapshot.root;
            git.branch = snapshot.branch;
            git.ahead = snapshot.ahead;
            git.behind = snapshot.behind;
            git.modified = snapshot.modified;
            git.statuses = snapshot.statuses;
            git.commits = snapshot.commits;
            cx.notify();
        })
        .ok();
    })
    .detach();
}

// ── background work ───────────────────────────────────────────────────────────

#[derive(Default)]
struct Snapshot {
    root: Option<PathBuf>,
    branch: Option<SharedString>,
    ahead: usize,
    behind: usize,
    modified: usize,
    statuses: HashMap<PathBuf, GitStatus>,
    commits: Vec<Commit>,
}

/// Runs `git -C dir <args>`; `None` when git is missing or exits non-zero.
fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Never take the index lock or prompt from a file manager's background poll.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn scan(cwd: &Path) -> Option<Snapshot> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let top = String::from_utf8_lossy(&top);
    let top = top.trim_end_matches(['\n', '\r']);
    if top.is_empty() {
        return None;
    }
    let root = PathBuf::from(top);
    let mut snap = Snapshot { root: Some(root.clone()), ..Default::default() };

    if let Some(out) = git(&root, &["status", "--porcelain=v2", "--branch", "-z"]) {
        parse_status(&out, &root, &mut snap);
    }
    if let Some(out) = git(&root, &["log", "-5", "--format=%h%x1f%ct%x1f%s"]) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for line in String::from_utf8_lossy(&out).lines() {
            let mut parts = line.splitn(3, '\x1f');
            let (Some(hash), Some(time), Some(subject)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let age = time.parse::<u64>().map(|t| format_age(now.saturating_sub(t))).unwrap_or_default();
            snap.commits.push(Commit {
                hash: hash.to_string().into(),
                subject: subject.to_string().into(),
                age: age.into(),
            });
        }
    }
    Some(snap)
}

/// Parses `git status --porcelain=v2 --branch -z` (NUL-separated records).
fn parse_status(out: &[u8], root: &Path, snap: &mut Snapshot) {
    let text = String::from_utf8_lossy(out);
    let mut records = text.split('\0');
    let mut oid = None;
    while let Some(rec) = records.next() {
        let (kind, rest) = match rec.split_once(' ') {
            Some(parts) => parts,
            None => continue,
        };
        let (status, path) = match kind {
            "#" => {
                if let Some(head) = rest.strip_prefix("branch.head ") {
                    snap.branch = Some(head.to_string().into());
                } else if let Some(o) = rest.strip_prefix("branch.oid ") {
                    oid = Some(o.to_string());
                } else if let Some(ab) = rest.strip_prefix("branch.ab ") {
                    for part in ab.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            snap.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            snap.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                continue;
            }
            // 1 XY sub mH mI mW hH hI path
            "1" => (xy_status(rest), rest.splitn(8, ' ').nth(7)),
            // 2 XY sub mH mI mW hH hI Xscore path  (followed by a separate origPath record)
            "2" => {
                records.next();
                (xy_status(rest), rest.splitn(9, ' ').nth(8))
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => (GitStatus::Modified, rest.splitn(10, ' ').nth(9)),
            "?" => (GitStatus::Untracked, Some(rest)),
            _ => continue,
        };
        let Some(path) = path else { continue };
        snap.modified += 1;
        let abs = root.join(path.trim_end_matches('/'));
        // Every ancestor up to and including the repo root shows the strongest status below it.
        for dir in abs.ancestors().skip(1) {
            merge(&mut snap.statuses, dir.to_path_buf(), status);
            if dir == root {
                break;
            }
        }
        merge(&mut snap.statuses, abs, status);
    }
    // Detached HEAD: show the short commit id instead of `(detached)`.
    if snap.branch.as_ref().is_some_and(|b| b.as_ref() == "(detached)")
        && let Some(oid) = oid.filter(|o| o.len() >= 7 && o.is_char_boundary(7))
    {
        snap.branch = Some(oid[..7].to_string().into());
    }
}

/// Status from the `XY` pair that starts a changed-entry record.
fn xy_status(rest: &str) -> GitStatus {
    let mut chars = rest.chars();
    let (x, y) = (chars.next().unwrap_or('.'), chars.next().unwrap_or('.'));
    if x == 'D' || y == 'D' {
        GitStatus::Deleted
    } else if x == 'A' && y == '.' {
        GitStatus::Added
    } else {
        GitStatus::Modified
    }
}

/// Deleted > Modified > Added > Untracked.
fn rank(status: GitStatus) -> u8 {
    match status {
        GitStatus::Deleted => 3,
        GitStatus::Modified => 2,
        GitStatus::Added => 1,
        GitStatus::Untracked => 0,
    }
}

fn merge(map: &mut HashMap<PathBuf, GitStatus>, path: PathBuf, status: GitStatus) {
    map.entry(path).and_modify(|s| {
        if rank(status) > rank(*s) {
            *s = status;
        }
    })
    .or_insert(status);
}

/// `45s`, `12m`, `2h`, `3d`, `5w`, `7mo`, `2y`.
fn format_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < MIN => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 14 * DAY => format!("{}d", s / DAY),
        s if s < 60 * DAY => format!("{}w", s / (7 * DAY)),
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}
