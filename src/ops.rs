//! File operations engine (SPEC §7): the y/x/p clipboard, copy / move / link jobs with
//! progress, trash, permanent delete, new file / folder, opening files externally.
//! UI for jobs / conflicts / toasts lives in overlays.rs; the worker itself in ops/transfer.rs.
//!
//! OWNER: ops agent.

pub mod extract;
mod transfer;

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use gpui::{App, AppContext, ClipboardItem, Context, SharedString, Window};

use crate::{
    actions::{Copy, Cut, DeletePermanently, NewFile, NewFolder, Paste, Trash},
    app::FmApp,
    fs_model,
    overlays::{self, Toast},
};

pub use transfer::{ConflictInfo, FileInfo, Resolution};
pub(crate) use transfer::{exists, numbered_name, split_ext, tree_size};
use transfer::{Control, Host, Outcome, Reply, Stop};

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

impl TransferKind {
    fn present(self) -> &'static str {
        match self {
            TransferKind::Copy => "Copying",
            TransferKind::Move => "Moving",
            TransferKind::Link => "Linking",
        }
    }

    fn past(self) -> &'static str {
        match self {
            TransferKind::Copy => "Copied",
            TransferKind::Move => "Moved",
            TransferKind::Link => "Linked",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            TransferKind::Copy => "Copy",
            TransferKind::Move => "Move",
            TransferKind::Link => "Link",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Scanning,
    Running,
    Paused,
    WaitingConflict,
    Done,
    Failed(String),
    Cancelled,
}

/// A copy / move / link running on a worker thread.
pub struct Job {
    pub id: u64,
    /// Tab that was active when the job started (tab-strip badge, panel anchor).
    pub tab_id: u64,
    pub kind: TransferKind,
    /// "Copying 214 files" / "Moving palette.png".
    pub title: SharedString,
    pub from: PathBuf,
    pub to: PathBuf,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_files: u64,
    pub done_files: u64,
    /// Bytes per second, smoothed.
    pub speed: f64,
    pub eta: Option<Duration>,
    pub state: JobState,
    /// The question the worker is blocked on (state `WaitingConflict`).
    pub conflict: Option<ConflictInfo>,
    /// Progress-bar tween: the fill animates `bar_from → bar_to`; `bar_gen` changes per update.
    pub bar_from: f32,
    pub bar_to: f32,
    pub bar_gen: u64,
    bar_at: Instant,
    pub started: Instant,
    sources: Vec<PathBuf>,
    ctl: Arc<Control>,
    reply: mpsc::Sender<Option<Reply>>,
    sample_at: Instant,
    sample_bytes: u64,
    last_reload: Instant,
    /// Started by pasting a cut: clear the clipboard when it succeeds.
    from_cut: bool,
}

impl Job {
    /// 0..=1.
    pub fn fraction(&self) -> f32 {
        let f = if self.total_bytes > 0 {
            self.done_bytes as f64 / self.total_bytes as f64
        } else if self.total_files > 0 {
            self.done_files as f64 / self.total_files as f64
        } else {
            0.
        };
        f.clamp(0., 1.) as f32
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, JobState::Done | JobState::Failed(_) | JobState::Cancelled)
    }

    fn make_title(&self) -> SharedString {
        let what = match self.sources.as_slice() {
            [one] => name_of(one),
            _ if self.total_files > 0 && self.kind != TransferKind::Link => plural(self.total_files, "file"),
            many => plural(many.len() as u64, "item"),
        };
        format!("{} {what}", self.kind.present()).into()
    }

    /// Directories whose listing this job changes.
    fn touches(&self, dir: &Path) -> bool {
        self.to == dir || (self.kind == TransferKind::Move && self.sources.iter().any(|s| s.parent() == Some(dir)))
    }
}

/// What the toast's **Undo** reverses.
#[derive(Clone, Debug)]
pub enum UndoOp {
    /// Copies / links that were created: move them to the trash.
    Created(Vec<PathBuf>),
    /// Moves: `(original, new)` — move back.
    Moved(Vec<(PathBuf, PathBuf)>),
    /// Trashed items: restore.
    Trashed(Vec<trash::TrashItem>),
}

pub struct OpsState {
    /// Internal file clipboard (`y` / `x`).
    pub clipboard: Option<Clipboard>,
    /// Unfinished jobs, oldest first (finished ones are removed and reported by a toast).
    pub jobs: Vec<Job>,
    next_job_id: u64,
}

impl OpsState {
    pub fn new(_window: &mut Window, _cx: &mut Context<FmApp>) -> Self {
        OpsState { clipboard: None, jobs: Vec::new(), next_job_id: 1 }
    }

    /// True when `path` was cut (`x`) — the list draws it at 50% opacity, italic.
    pub fn is_cut(&self, path: &Path) -> bool {
        self.clipboard.as_ref().is_some_and(|c| c.mode == ClipMode::Cut && c.paths.iter().any(|p| p == path))
    }

    /// Running jobs started from the tab with this id (tab-strip badge).
    pub fn jobs_for_tab(&self, tab_id: u64) -> usize {
        self.jobs.iter().filter(|j| j.tab_id == tab_id && !j.is_finished()).count()
    }

    pub fn has_clipboard(&self) -> bool {
        self.clipboard.as_ref().is_some_and(|c| !c.paths.is_empty())
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

pub(crate) fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
}

/// `1 file`, `3 files`.
pub(crate) fn plural(n: u64, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// A single name, or `N items`.
pub(crate) fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => name_of(one),
        many => plural(many.len() as u64, "item"),
    }
}

/// Run a detached GUI program without leaving a zombie behind.
pub(crate) fn spawn_detached(mut command: Command) -> bool {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Open a file with the default application.
pub fn open_external(path: &Path, _cx: &mut App) {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    spawn_detached(command);
}

fn reload_if_showing(app: &mut FmApp, dirs: &[&Path], cx: &mut Context<FmApp>) {
    if dirs.contains(&app.cwd()) {
        app.reload(cx);
    }
}

// ── transfer jobs ────────────────────────────────────────────────────────────

enum Event {
    Totals(u64, u64),
    Progress(u64, u64),
    Conflict(Box<ConflictInfo>),
    Finished(Result<Outcome, Stop>),
}

/// The worker's side of the channel pair.
struct ChannelHost {
    events: smol::channel::Sender<Event>,
    replies: mpsc::Receiver<Option<Reply>>,
    last_sent: Instant,
}

impl Host for ChannelHost {
    fn totals(&mut self, bytes: u64, files: u64) {
        let _ = self.events.send_blocking(Event::Totals(bytes, files));
    }

    fn progress(&mut self, bytes: u64, files: u64) {
        // At most 10 UI updates per second.
        if self.last_sent.elapsed() >= Duration::from_millis(100) {
            self.last_sent = Instant::now();
            let _ = self.events.send_blocking(Event::Progress(bytes, files));
        }
    }

    fn conflict(&mut self, conflict: ConflictInfo) -> Option<Reply> {
        self.events.send_blocking(Event::Conflict(Box::new(conflict))).ok()?;
        self.replies.recv().ok().flatten()
    }
}

/// Copy / move / link `sources` into `dest_dir` as a background job (paste, drag & drop).
pub fn start_transfer(
    app: &mut FmApp,
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    kind: TransferKind,
    cx: &mut Context<FmApp>,
) {
    start_job(app, sources, dest_dir, kind, false, cx);
}

fn start_job(
    app: &mut FmApp,
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    kind: TransferKind,
    from_cut: bool,
    cx: &mut Context<FmApp>,
) {
    if sources.is_empty() {
        return;
    }
    let id = app.ops.next_job_id;
    app.ops.next_job_id += 1;
    let ctl = Arc::new(Control::default());
    let (events_tx, events_rx) = smol::channel::unbounded::<Event>();
    let (reply_tx, reply_rx) = mpsc::channel::<Option<Reply>>();
    let now = Instant::now();
    let from = sources[0].parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));

    let mut job = Job {
        id,
        tab_id: app.tab().id,
        kind,
        title: SharedString::default(),
        from,
        to: dest_dir.clone(),
        total_bytes: 0,
        done_bytes: 0,
        total_files: 0,
        done_files: 0,
        speed: 0.,
        eta: None,
        state: JobState::Scanning,
        conflict: None,
        bar_from: 0.,
        bar_to: 0.,
        bar_gen: 0,
        bar_at: now,
        started: now,
        sources: sources.clone(),
        ctl: ctl.clone(),
        reply: reply_tx,
        sample_at: now,
        sample_bytes: 0,
        last_reload: now,
        from_cut,
    };
    job.title = job.make_title();
    app.ops.jobs.push(job);
    overlays::on_job_started(app, cx);

    let spawned = std::thread::Builder::new().name(format!("fm-job-{id}")).spawn({
        let events = events_tx.clone();
        move || {
            let mut host = ChannelHost { events: events.clone(), replies: reply_rx, last_sent: Instant::now() };
            let result = transfer::run_transfer(&sources, &dest_dir, kind, &ctl, &mut host);
            let _ = events.send_blocking(Event::Finished(result));
        }
    });
    if let Err(err) = spawned {
        let _ = events_tx.try_send(Event::Finished(Err(Stop::Failed(format!("could not start a worker: {err}")))));
    }
    drop(events_tx);

    // The panel only shows jobs that outlive `JOB_PANEL_DELAY`; make sure that frame happens.
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(overlays::JOB_PANEL_DELAY + Duration::from_millis(20)).await;
        this.update(cx, |_, cx| cx.notify()).ok();
    })
    .detach();
    cx.spawn(async move |this, cx| {
        while let Ok(event) = events_rx.recv().await {
            if this.update(cx, |app, cx| on_job_event(app, id, event, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    cx.notify();
}

fn on_job_event(app: &mut FmApp, id: u64, event: Event, cx: &mut Context<FmApp>) {
    let Some(ix) = app.ops.jobs.iter().position(|j| j.id == id) else { return };
    let cwd = app.cwd().to_path_buf();
    let job = &mut app.ops.jobs[ix];
    match event {
        Event::Totals(bytes, files) => {
            job.total_bytes = bytes;
            job.total_files = files;
            job.title = job.make_title();
            if job.state == JobState::Scanning {
                job.state = if job.ctl.pause.load(Ordering::Relaxed) { JobState::Paused } else { JobState::Running };
            }
            job.sample_at = Instant::now();
        }
        Event::Progress(bytes, files) => {
            let now = Instant::now();
            let dt = now.duration_since(job.sample_at).as_secs_f64();
            if dt >= 0.05 && job.state == JobState::Running {
                let instant = bytes.saturating_sub(job.sample_bytes) as f64 / dt;
                job.speed = if job.speed <= 0. { instant } else { job.speed * 0.7 + instant * 0.3 };
            }
            job.sample_at = now;
            job.sample_bytes = bytes;
            job.done_bytes = bytes;
            job.done_files = files;
            job.eta = (job.speed > 1. && job.total_bytes > bytes)
                .then(|| Duration::from_secs_f64((job.total_bytes - bytes) as f64 / job.speed));
            // Restart the bar tween from wherever it is on screen right now.
            let t = (now.duration_since(job.bar_at).as_secs_f32() / 0.15).min(1.);
            job.bar_from = job.bar_from + (job.bar_to - job.bar_from) * t;
            job.bar_to = job.fraction().max(job.bar_from);
            job.bar_at = now;
            job.bar_gen += 1;
            if job.touches(&cwd) && now.duration_since(job.last_reload) >= Duration::from_secs(1) {
                job.last_reload = now;
                app.reload(cx);
            }
        }
        Event::Conflict(conflict) => {
            job.conflict = Some(*conflict);
            job.state = JobState::WaitingConflict;
            job.speed = 0.;
            job.eta = None;
        }
        Event::Finished(result) => {
            let job = app.ops.jobs.remove(ix);
            finish_job(app, job, result, cx);
        }
    }
    cx.notify();
}

fn finish_job(app: &mut FmApp, job: Job, result: Result<Outcome, Stop>, cx: &mut Context<FmApp>) {
    let kind = job.kind;
    match result {
        Ok(outcome) => {
            if job.from_cut && app.ops.clipboard.as_ref().is_some_and(|c| c.mode == ClipMode::Cut && c.paths == job.sources) {
                app.ops.clipboard = None;
            }
            let mut text = if outcome.items == 0 {
                format!("Nothing {}", kind.past().to_lowercase())
            } else if let ([one], 1) = (job.sources.as_slice(), outcome.items) {
                format!("{} {}", kind.past(), name_of(one))
            } else if kind == TransferKind::Link {
                format!("{} {}", kind.past(), plural(outcome.items as u64, "item"))
            } else {
                format!("{} {}", kind.past(), plural(outcome.files, "file"))
            };
            if outcome.skipped > 0 && !(outcome.items == 0 && kind == TransferKind::Move && outcome.skipped == job.sources.len()) {
                text += &format!(" · {} skipped", outcome.skipped);
            }
            let undo = match kind {
                _ if outcome.created.is_empty() => None,
                TransferKind::Move => Some(UndoOp::Moved(outcome.moved.clone())),
                _ => Some(UndoOp::Created(outcome.created.clone())),
            };
            let open = (outcome.items > 0).then(|| (job.to.clone(), outcome.created.first().cloned()));
            overlays::show_toast(app, Toast { text: text.into(), error: false, open, undo }, cx);
        }
        Err(Stop::Cancelled) => overlays::toast(app, format!("{} cancelled", kind.noun()), cx),
        Err(Stop::Failed(message)) => overlays::toast_error(app, format!("{} failed · {message}", kind.noun()), cx),
    }
    if job.touches(app.cwd()) {
        app.reload(cx);
    }
}

/// Pause / resume a job.
pub fn toggle_pause(app: &mut FmApp, job_id: u64, cx: &mut Context<FmApp>) {
    let Some(job) = app.ops.jobs.iter_mut().find(|j| j.id == job_id) else { return };
    let paused = !job.ctl.pause.load(Ordering::Relaxed);
    job.ctl.pause.store(paused, Ordering::Relaxed);
    match job.state {
        JobState::Running if paused => {
            job.state = JobState::Paused;
            job.speed = 0.;
            job.eta = None;
        }
        JobState::Paused if !paused => {
            job.state = JobState::Running;
            job.sample_at = Instant::now();
        }
        _ => {}
    }
    cx.notify();
}

/// Ask a job to stop. It disappears (with a toast) once the worker acknowledges.
pub fn cancel_job(app: &mut FmApp, job_id: u64, cx: &mut Context<FmApp>) {
    let Some(job) = app.ops.jobs.iter_mut().find(|j| j.id == job_id) else { return };
    job.ctl.cancel.store(true, Ordering::Relaxed);
    if job.conflict.take().is_some() {
        let _ = job.reply.send(None);
    }
    cx.notify();
}

/// Answer the conflict a job is waiting on.
pub fn resolve_conflict(
    app: &mut FmApp,
    job_id: u64,
    resolution: Resolution,
    apply_to_all: bool,
    cx: &mut Context<FmApp>,
) {
    let Some(job) = app.ops.jobs.iter_mut().find(|j| j.id == job_id) else { return };
    if job.conflict.take().is_none() {
        return;
    }
    let _ = job.reply.send(Some(Reply { resolution, apply_to_all }));
    job.state = if job.ctl.pause.load(Ordering::Relaxed) { JobState::Paused } else { JobState::Running };
    job.sample_at = Instant::now();
    cx.notify();
}

// ── undo ─────────────────────────────────────────────────────────────────────

/// Reverse a finished operation (the toast's **Undo**). Best effort.
pub fn undo(_app: &mut FmApp, op: UndoOp, cx: &mut Context<FmApp>) {
    let task = cx.background_spawn(async move {
        match op {
            UndoOp::Created(paths) => {
                let existing: Vec<PathBuf> = paths.into_iter().filter(|p| transfer::exists(p)).collect();
                let n = existing.len();
                trash::delete_all(&existing).map(|_| n).map_err(|e| e.to_string())
            }
            UndoOp::Moved(pairs) => transfer::move_back(&pairs),
            UndoOp::Trashed(items) => {
                let n = items.len();
                trash::os_limited::restore_all(items).map(|_| n).map_err(|e| e.to_string())
            }
        }
    });
    cx.spawn(async move |this, cx| {
        let result = task.await;
        this.update(cx, |app, cx| {
            match result {
                Ok(n) => overlays::toast(app, format!("Undone · {}", plural(n as u64, "item")), cx),
                Err(message) => overlays::toast_error(app, format!("Undo failed · {message}"), cx),
            }
            app.reload(cx);
        })
        .ok();
    })
    .detach();
}

// ── trash & delete ───────────────────────────────────────────────────────────

/// Move `paths` to the XDG trash.
pub fn trash_paths(app: &mut FmApp, paths: Vec<PathBuf>, cx: &mut Context<FmApp>) {
    if paths.is_empty() {
        return;
    }
    let trash_root = fs_model::trash_dir();
    if paths.iter().any(|p| p.starts_with(&trash_root)) {
        return overlays::toast_error(app, "Already in the trash · shift+d deletes permanently", cx);
    }
    let dirs: Vec<PathBuf> = paths.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    let task = cx.background_spawn(async move {
        let ids = |items: &[trash::TrashItem]| items.iter().map(|i| i.id.clone()).collect::<HashSet<OsString>>();
        let before = trash::os_limited::list().ok().map(|items| ids(&items));
        let mut done: Vec<PathBuf> = Vec::new();
        let mut error = None;
        for path in &paths {
            match trash::delete(path) {
                Ok(()) => done.push(path.clone()),
                Err(e) => {
                    error.get_or_insert(format!("{}: {e}", name_of(path)));
                }
            }
        }
        // What appeared in the trash and came from our paths is what Undo restores.
        let canonical: HashSet<PathBuf> = done
            .iter()
            .filter_map(|p| Some(fs::canonicalize(p.parent()?).ok()?.join(p.file_name()?)))
            .collect();
        let restorable: Vec<trash::TrashItem> = match (before, trash::os_limited::list()) {
            (Some(before), Ok(after)) => after
                .into_iter()
                .filter(|item| !before.contains(&item.id) && canonical.contains(&item.original_path()))
                .collect(),
            _ => Vec::new(),
        };
        (done.len(), error, restorable)
    });
    cx.spawn(async move |this, cx| {
        let (done, error, restorable) = task.await;
        this.update(cx, |app, cx| {
            if done > 0 {
                let undo = (restorable.len() == done).then_some(UndoOp::Trashed(restorable));
                let text = format!("Moved {} to trash", plural(done as u64, "item"));
                overlays::show_toast(app, Toast { text: text.into(), error: false, open: None, undo }, cx);
            }
            if let Some(message) = error
                && done == 0
            {
                overlays::toast_error(app, format!("Trash failed · {message}"), cx);
            }
            let dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
            reload_if_showing(app, &dirs, cx);
        })
        .ok();
    })
    .detach();
}

/// Delete for good (after the confirm dialog).
pub fn delete_permanently(_app: &mut FmApp, paths: Vec<PathBuf>, cx: &mut Context<FmApp>) {
    if paths.is_empty() {
        return;
    }
    let dirs: Vec<PathBuf> = paths.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    let task = cx.background_spawn(async move {
        let mut done = 0u64;
        let mut error = None;
        for path in &paths {
            match transfer::remove_tree(path) {
                Ok(()) => done += 1,
                Err(e) => {
                    error.get_or_insert(format!("{}: {e}", name_of(path)));
                }
            }
        }
        (done, error)
    });
    cx.spawn(async move |this, cx| {
        let (done, error) = task.await;
        this.update(cx, |app, cx| {
            match error {
                Some(message) => overlays::toast_error(app, format!("Delete failed · {message}"), cx),
                None => overlays::toast(app, format!("Deleted {}", plural(done, "item")), cx),
            }
            let dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
            reload_if_showing(app, &dirs, cx);
        })
        .ok();
    })
    .detach();
}

// ── new file / folder ────────────────────────────────────────────────────────

fn create_new(app: &mut FmApp, folder: bool, cx: &mut Context<FmApp>) {
    let dir = app.cwd().to_path_buf();
    let task = cx.background_spawn({
        let dir = dir.clone();
        async move {
            let base = if folder { "untitled folder" } else { "untitled" };
            let mut last = None;
            // Retry if something grabs the name between the check and the create.
            for _ in 0..4 {
                let path = dir.join(transfer::unique_name(base, |n| transfer::exists(&dir.join(n))));
                let made = if folder {
                    fs::create_dir(&path)
                } else {
                    fs::OpenOptions::new().write(true).create_new(true).open(&path).map(|_| ())
                };
                match made {
                    Ok(()) => return Ok(path),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                    Err(e) => return Err(e.to_string()),
                }
            }
            Err(last.map(|e| e.to_string()).unwrap_or_default())
        }
    });
    cx.spawn(async move |this, cx| {
        let result = task.await;
        this.update(cx, |app, cx| match result {
            Ok(path) if app.cwd() == dir => app.reload_and_focus(path, true, cx),
            Ok(_) => {}
            Err(message) => {
                let what = if folder { "folder" } else { "file" };
                overlays::toast_error(app, format!("Could not create the {what} · {message}"), cx)
            }
        })
        .ok();
    })
    .detach();
}

// ── action handlers ──────────────────────────────────────────────────────────

fn set_clipboard(app: &mut FmApp, mode: ClipMode, cx: &mut Context<FmApp>) {
    let paths = app.tab().targets();
    if paths.is_empty() {
        return;
    }
    let text = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    let what = plural(paths.len() as u64, "file");
    let message = match mode {
        ClipMode::Copy => format!("{what} copied to clipboard"),
        ClipMode::Cut => format!("{what} cut"),
    };
    app.ops.clipboard = Some(Clipboard { mode, paths });
    overlays::toast(app, message, cx);
    cx.notify();
}

pub fn act_copy(app: &mut FmApp, _: &Copy, _window: &mut Window, cx: &mut Context<FmApp>) {
    set_clipboard(app, ClipMode::Copy, cx);
}

pub fn act_cut(app: &mut FmApp, _: &Cut, _window: &mut Window, cx: &mut Context<FmApp>) {
    set_clipboard(app, ClipMode::Cut, cx);
}

pub fn act_paste(app: &mut FmApp, _: &Paste, _window: &mut Window, cx: &mut Context<FmApp>) {
    let Some(clip) = app.ops.clipboard.clone().filter(|c| !c.paths.is_empty()) else {
        return overlays::toast(app, "Nothing to paste", cx);
    };
    let dest = app.cwd().to_path_buf();
    match clip.mode {
        ClipMode::Copy => start_job(app, clip.paths, dest, TransferKind::Copy, false, cx),
        ClipMode::Cut => start_job(app, clip.paths, dest, TransferKind::Move, true, cx),
    }
}

pub fn act_trash(app: &mut FmApp, _: &Trash, _window: &mut Window, cx: &mut Context<FmApp>) {
    let paths = app.tab().targets();
    trash_paths(app, paths, cx);
}

pub fn act_delete_permanently(app: &mut FmApp, _: &DeletePermanently, window: &mut Window, cx: &mut Context<FmApp>) {
    let paths = app.tab().targets();
    if !paths.is_empty() {
        overlays::confirm_delete(app, paths, window, cx);
    }
}

pub fn act_new_file(app: &mut FmApp, _: &NewFile, _window: &mut Window, cx: &mut Context<FmApp>) {
    create_new(app, false, cx);
}

pub fn act_new_folder(app: &mut FmApp, _: &NewFolder, _window: &mut Window, cx: &mut Context<FmApp>) {
    create_new(app, true, cx);
}

