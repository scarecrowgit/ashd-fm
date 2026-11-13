//! Context menus (SPEC §7): file / empty space / multi-select, with the "Open with" and
//! "Add tag" submenus, keyboard navigation and the commands behind the items.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use gpui::{
    Animation, AnimationExt, AnyElement, ClipboardItem, Context, Div, ElementId, MouseButton, MouseMoveEvent,
    Pixels, Point, SharedString, Window, div, prelude::*, px,
};

use super::{
    Layer, MenuActivate, MenuDown, MenuLeft, MenuRight, MenuTarget, MenuUp, Toast, dialogs, modal_layer,
    refocus, show_toast, toast, toast_error,
};
use crate::{
    actions,
    app::FmApp,
    fs_model,
    icons::{Icon, icon},
    ops,
    theme::{self, ease_standard, theme},
};

const MENU_W: f32 = 250.;
const SUB_W: f32 = 210.;
const ITEM_H: f32 = 30.;
/// 1px line + 4 margin above and below.
const SEP_H: f32 = 9.;
const HEADER_H: f32 = 24.;
/// Border + padding around the rows.
const FRAME: f32 = 5.;
/// Gap kept between a menu and the window edge.
const EDGE: f32 = 4.;
/// How far a submenu overlaps its parent.
const SUB_OVERLAP: f32 = 6.;

/// Tag names with their colour token (resolved against the theme when drawn).
const TAGS: [&str; 5] = ["red", "yellow", "green", "blue", "accent"];

// ── model ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SubKind {
    OpenWith,
    Tags,
}

#[derive(Clone, Debug)]
enum Cmd {
    Open,
    /// Index into `Menu::apps`.
    OpenWith(usize),
    OpenDefault,
    Rename,
    CopyPath,
    AddTag(&'static str),
    Compress,
    Properties,
    Trash,
    NewFile,
    NewFolder,
    Paste,
    Terminal,
    ToggleHidden,
    Copy,
    Cut,
    Sub(SubKind),
    Nothing,
}

#[derive(Clone, Debug)]
enum Lead {
    Icon(Icon),
    /// 8px tag dot: index into `TAGS`.
    Dot(usize),
    Blank,
}

#[derive(Clone, Debug)]
struct Item {
    lead: Lead,
    label: SharedString,
    hint: Option<&'static str>,
    cmd: Cmd,
    destructive: bool,
    disabled: bool,
}

impl Item {
    fn new(ic: Icon, label: impl Into<SharedString>, cmd: Cmd) -> Self {
        Item { lead: Lead::Icon(ic), label: label.into(), hint: None, cmd, destructive: false, disabled: false }
    }

    fn hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }

    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    fn lead(mut self, lead: Lead) -> Self {
        self.lead = lead;
        self
    }
}

#[derive(Clone, Debug)]
enum Row {
    Item(Item),
    Sep,
}

/// An application offered by "Open with".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AppChoice {
    name: String,
    /// `Exec=` line of the desktop entry (with field codes).
    exec: String,
    terminal: bool,
    is_default: bool,
}

struct Sub {
    /// Row of the parent menu that owns it.
    parent: usize,
    kind: SubKind,
    highlight: Option<usize>,
}

pub(super) struct Menu {
    seq: u64,
    paths: Vec<PathBuf>,
    /// The single target is a folder.
    is_dir: bool,
    /// Folder the menu was opened in.
    dir: PathBuf,
    position: Point<Pixels>,
    header: Option<SharedString>,
    rows: Vec<Row>,
    highlight: Option<usize>,
    sub: Option<Sub>,
    /// "Open with" candidates; `None` while they are being looked up.
    apps: Option<Vec<AppChoice>>,
    /// Invalidates pending hover timers.
    hover_token: u64,
}

/// What exists on this machine (looked up once, off the main thread).
#[derive(Clone, Debug, Default)]
pub(super) struct Caps {
    tar: bool,
    terminal: Option<String>,
}

pub(super) fn detect_caps(cx: &mut Context<FmApp>) {
    let task = cx.background_spawn(async { Caps { tar: on_path("tar").is_some(), terminal: find_terminal() } });
    cx.spawn(async move |this, cx| {
        let caps = task.await;
        this.update(cx, |app, _| app.overlays.caps = caps).ok();
    })
    .detach();
}

fn on_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Path::new(program).is_file().then(|| PathBuf::from(program));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file())
}

fn find_terminal() -> Option<String> {
    let from_env = std::env::var("TERMINAL").ok().and_then(|t| t.split_whitespace().next().map(str::to_string));
    from_env
        .into_iter()
        .chain(["foot", "kitty", "alacritty", "wezterm"].map(str::to_string))
        .find(|program| on_path(program).is_some())
}

fn file_rows(is_dir: bool, caps: &Caps) -> Vec<Row> {
    vec![
        Row::Item(Item::new(Icon::Folder, "Open", Cmd::Open).hint("↵")),
        Row::Item(Item::new(Icon::Code, "Open with", Cmd::Sub(SubKind::OpenWith)).disabled(is_dir)),
        Row::Item(Item::new(Icon::Pencil, "Rename", Cmd::Rename).hint("r")),
        Row::Item(Item::new(Icon::Clipboard, "Copy path", Cmd::CopyPath).hint("ctrl+c")),
        Row::Item(Item::new(Icon::Tag, "Add tag", Cmd::Sub(SubKind::Tags))),
        Row::Item(Item::new(Icon::Archive, "Compress…", Cmd::Compress).disabled(!caps.tar)),
        Row::Item(Item::new(Icon::Sliders, "Properties", Cmd::Properties)),
        Row::Sep,
        Row::Item(Item::new(Icon::Trash, "Move to trash", Cmd::Trash).hint("d").destructive()),
    ]
}

fn empty_rows(app: &FmApp) -> Vec<Row> {
    let hidden = Item::new(Icon::Check, "Show hidden", Cmd::ToggleHidden).hint(".");
    vec![
        Row::Item(Item::new(Icon::File, "New file", Cmd::NewFile)),
        Row::Item(Item::new(Icon::Folder, "New folder", Cmd::NewFolder)),
        Row::Item(Item::new(Icon::Clipboard, "Paste", Cmd::Paste).hint("p").disabled(!app.ops.has_clipboard())),
        Row::Item(
            Item::new(Icon::Terminal, "Open terminal here", Cmd::Terminal)
                .disabled(app.overlays.caps.terminal.is_none()),
        ),
        Row::Item(if app.show_hidden { hidden } else { hidden.lead(Lead::Blank) }),
    ]
}

fn selection_rows(caps: &Caps) -> Vec<Row> {
    vec![
        Row::Item(Item::new(Icon::Copy, "Copy", Cmd::Copy).hint("y")),
        Row::Item(Item::new(Icon::Move, "Cut", Cmd::Cut).hint("x")),
        Row::Item(Item::new(Icon::Clipboard, "Copy paths", Cmd::CopyPath)),
        Row::Item(Item::new(Icon::Archive, "Compress…", Cmd::Compress).disabled(!caps.tar)),
        Row::Sep,
        Row::Item(Item::new(Icon::Trash, "Move to trash", Cmd::Trash).hint("d").destructive()),
    ]
}

fn sub_rows(menu: &Menu, kind: SubKind) -> Vec<Row> {
    match kind {
        SubKind::Tags => TAGS
            .iter()
            .enumerate()
            .map(|(ix, name)| Row::Item(Item::new(Icon::Tag, *name, Cmd::AddTag(name)).lead(Lead::Dot(ix))))
            .collect(),
        SubKind::OpenWith => {
            let mut rows = match &menu.apps {
                None => vec![Row::Item(Item::new(Icon::Search, "Looking for applications…", Cmd::Nothing).disabled(true))],
                Some(apps) => apps
                    .iter()
                    .enumerate()
                    .map(|(ix, app)| {
                        let ic = if app.terminal { Icon::Terminal } else { Icon::Grid };
                        let item = Item::new(ic, app.name.clone(), Cmd::OpenWith(ix));
                        Row::Item(if app.is_default { item.hint("default") } else { item })
                    })
                    .collect(),
            };
            rows.push(Row::Sep);
            rows.push(Row::Item(Item::new(Icon::Play, "Default application", Cmd::OpenDefault)));
            rows
        }
    }
}

fn panel_height(header: bool, rows: &[Row]) -> f32 {
    let body: f32 = rows.iter().map(|r| if matches!(r, Row::Sep) { SEP_H } else { ITEM_H }).sum();
    FRAME * 2. + if header { HEADER_H } else { 0. } + body
}

/// Next enabled item from `from` in direction `delta`, wrapping around.
fn step(rows: &[Row], from: Option<usize>, delta: isize) -> Option<usize> {
    let n = rows.len() as isize;
    if n == 0 {
        return None;
    }
    let mut ix = match from {
        Some(ix) => ix as isize,
        None if delta > 0 => -1,
        None => n,
    };
    for _ in 0..n {
        ix = (ix + delta).rem_euclid(n);
        if matches!(&rows[ix as usize], Row::Item(item) if !item.disabled) {
            return Some(ix as usize);
        }
    }
    None
}

// ── opening ──────────────────────────────────────────────────────────────────

pub(super) fn open(
    app: &mut FmApp,
    target: MenuTarget,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<FmApp>,
) {
    let seq = app.overlays.next_seq();
    let dir = app.cwd().to_path_buf();
    let caps = app.overlays.caps.clone();
    let (paths, is_dir, header, rows) = match target {
        MenuTarget::Entry(path) => {
            let is_dir = app.tab().entries.iter().find(|e| e.path == path).is_some_and(|e| e.is_dir);
            let header = ops::name_of(&path);
            (vec![path], is_dir, header, file_rows(is_dir, &caps))
        }
        MenuTarget::Empty => (Vec::new(), false, crate::fs_model::short_name(&dir), empty_rows(app)),
        MenuTarget::Selection => {
            let paths = app.tab().targets();
            let header = ops::plural(paths.len() as u64, "item");
            (paths, false, header, selection_rows(&caps))
        }
    };
    // Look the "Open with" candidates up while the menu is already on screen.
    let has_open_with = rows.iter().any(|r| matches!(r, Row::Item(Item { cmd: Cmd::Sub(SubKind::OpenWith), disabled: false, .. })));
    let lookup = paths.first().filter(|_| has_open_with).cloned();
    app.overlays.menu = Some(Menu {
        seq,
        paths,
        is_dir,
        dir,
        position,
        header: Some(header.to_uppercase().into()),
        rows,
        highlight: None,
        sub: None,
        apps: None,
        hover_token: 0,
    });
    if let Some(path) = lookup {
        let task = cx.background_spawn(async move { resolve_apps(&path) });
        cx.spawn(async move |this, cx| {
            let apps = task.await;
            this.update(cx, |app, cx| {
                if let Some(menu) = app.overlays.menu.as_mut().filter(|m| m.seq == seq) {
                    menu.apps = Some(apps);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
    refocus(app, window);
    cx.notify();
}

// ── interaction ──────────────────────────────────────────────────────────────

fn open_sub(menu: &mut Menu, parent: usize, kind: SubKind, highlight_first: bool) {
    let highlight = if highlight_first { step(&sub_rows(menu, kind), None, 1) } else { None };
    menu.sub = Some(Sub { parent, kind, highlight });
}

fn sub_kind_at(menu: &Menu, ix: usize) -> Option<SubKind> {
    match menu.rows.get(ix) {
        Some(Row::Item(Item { cmd: Cmd::Sub(kind), disabled: false, .. })) => Some(*kind),
        _ => None,
    }
}

fn hover(app: &mut FmApp, level: usize, ix: usize, cx: &mut Context<FmApp>) {
    let Some(menu) = app.overlays.menu.as_mut() else { return };
    if level == 1 {
        let Some(sub) = menu.sub.as_mut() else { return };
        if sub.highlight != Some(ix) || menu.highlight != Some(sub.parent) {
            sub.highlight = Some(ix);
            menu.highlight = Some(sub.parent);
            menu.hover_token += 1;
            cx.notify();
        }
        return;
    }
    if menu.highlight == Some(ix) {
        return;
    }
    menu.highlight = Some(ix);
    if let Some(sub) = menu.sub.as_mut() {
        sub.highlight = None;
    }
    menu.hover_token += 1;
    let (seq, token) = (menu.seq, menu.hover_token);
    cx.notify();
    // Submenus follow the pointer after a 120 ms rest.
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(Duration::from_millis(theme::DUR_FAST)).await;
        this.update(cx, |app, cx| {
            let Some(menu) = app.overlays.menu.as_mut().filter(|m| m.seq == seq && m.hover_token == token) else {
                return;
            };
            let want = sub_kind_at(menu, ix);
            if menu.sub.as_ref().map(|s| s.parent) != want.map(|_| ix) {
                match want {
                    Some(kind) => open_sub(menu, ix, kind, false),
                    None => menu.sub = None,
                }
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}

fn key_move(app: &mut FmApp, delta: isize, cx: &mut Context<FmApp>) {
    let Some(menu) = app.overlays.menu.as_mut() else { return };
    menu.hover_token += 1;
    if let Some((kind, current)) = menu.sub.as_ref().map(|s| (s.kind, s.highlight)) {
        let next = step(&sub_rows(menu, kind), current, delta);
        if let Some(sub) = menu.sub.as_mut() {
            sub.highlight = next;
        }
    } else {
        menu.highlight = step(&menu.rows, menu.highlight, delta);
    }
    cx.notify();
}

fn key_right(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let Some(menu) = app.overlays.menu.as_mut() else { return };
    menu.hover_token += 1;
    if let Some(sub) = menu.sub.as_mut() {
        if sub.highlight.is_none() {
            let kind = sub.kind;
            let first = step(&sub_rows(menu, kind), None, 1);
            if let Some(sub) = menu.sub.as_mut() {
                sub.highlight = first;
            }
        }
    } else if let Some(ix) = menu.highlight
        && let Some(kind) = sub_kind_at(menu, ix)
    {
        open_sub(menu, ix, kind, true);
    }
    cx.notify();
}

fn key_left(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let Some(menu) = app.overlays.menu.as_mut() else { return };
    menu.hover_token += 1;
    if let Some(sub) = menu.sub.take() {
        menu.highlight = Some(sub.parent);
    }
    cx.notify();
}

fn key_activate(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some(menu) = app.overlays.menu.as_ref() else { return };
    match (menu.sub.as_ref().and_then(|s| s.highlight), menu.highlight) {
        (Some(ix), _) => activate(app, 1, ix, window, cx),
        (None, Some(ix)) => activate(app, 0, ix, window, cx),
        _ => {}
    }
}

fn activate(app: &mut FmApp, level: usize, ix: usize, window: &mut Window, cx: &mut Context<FmApp>) {
    let Some(menu) = app.overlays.menu.as_mut() else { return };
    let rows = match (level, menu.sub.as_ref()) {
        (0, _) => menu.rows.clone(),
        (_, Some(sub)) => sub_rows(menu, sub.kind),
        _ => return,
    };
    let Some(Row::Item(item)) = rows.get(ix) else { return };
    if item.disabled {
        return;
    }
    menu.hover_token += 1;
    if let Cmd::Sub(kind) = item.cmd {
        menu.highlight = Some(ix);
        open_sub(menu, ix, kind, true);
        return cx.notify();
    }
    // Close the menu and hand the keyboard back to the list before anything runs: actions
    // are dispatched from the focused element.
    let Some(menu) = app.overlays.menu.take() else { return };
    refocus(app, window);
    cx.notify();
    run(app, item.cmd.clone(), menu, window, cx);
}

fn close(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) {
    app.overlays.menu = None;
    refocus(app, window);
    cx.notify();
}

// ── commands ─────────────────────────────────────────────────────────────────

fn run(app: &mut FmApp, cmd: Cmd, menu: Menu, window: &mut Window, cx: &mut Context<FmApp>) {
    let first = menu.paths.first().cloned();
    match cmd {
        Cmd::Open => {
            if let Some(path) = first {
                if menu.is_dir {
                    app.navigate(path, cx);
                } else {
                    ops::open_external(&path, cx);
                }
            }
        }
        Cmd::OpenDefault => {
            if let Some(path) = first {
                ops::open_external(&path, cx);
            }
        }
        Cmd::OpenWith(ix) => {
            if let (Some(path), Some(choice)) = (first, menu.apps.as_ref().and_then(|apps| apps.get(ix))) {
                launch(app, choice, &path, cx);
            }
        }
        Cmd::Rename => {
            // Rename works on the cursor row: put the cursor on the menu's target first.
            if let Some(row) = first.and_then(|p| app.tab().row_of(&p)) {
                if app.tab().cursor != row {
                    app.set_cursor(row, cx);
                }
                window.dispatch_action(Box::new(actions::Rename), cx);
            }
        }
        Cmd::CopyPath => {
            let text = menu.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            toast(app, if menu.paths.len() == 1 { "Path copied" } else { "Paths copied" }, cx);
        }
        Cmd::AddTag(name) => {
            for path in menu.paths {
                super::add_tag(app, path, name, cx);
            }
        }
        Cmd::Compress => compress(app, menu.paths, cx),
        Cmd::Properties => {
            if let Some(path) = first {
                dialogs::open_properties(app, path, window, cx);
            }
        }
        Cmd::Trash => ops::trash_paths(app, menu.paths, cx),
        Cmd::Terminal => open_terminal(app, &menu.dir, cx),
        Cmd::NewFile => window.dispatch_action(Box::new(actions::NewFile), cx),
        Cmd::NewFolder => window.dispatch_action(Box::new(actions::NewFolder), cx),
        Cmd::Paste => window.dispatch_action(Box::new(actions::Paste), cx),
        Cmd::ToggleHidden => window.dispatch_action(Box::new(actions::ToggleHidden), cx),
        Cmd::Copy => window.dispatch_action(Box::new(actions::Copy), cx),
        Cmd::Cut => window.dispatch_action(Box::new(actions::Cut), cx),
        Cmd::Sub(_) | Cmd::Nothing => {}
    }
}

fn is_wezterm(terminal: &str) -> bool {
    Path::new(terminal).file_name().is_some_and(|n| n == "wezterm")
}

fn open_terminal(app: &mut FmApp, dir: &Path, cx: &mut Context<FmApp>) {
    let Some(terminal) = app.overlays.caps.terminal.clone() else {
        return toast_error(app, "No terminal found · set $TERMINAL", cx);
    };
    let mut command = Command::new(&terminal);
    if is_wezterm(&terminal) {
        command.arg("start").arg("--cwd").arg(dir);
    }
    command.current_dir(dir);
    if !ops::spawn_detached(command) {
        toast_error(app, format!("Could not start {terminal}"), cx);
    }
}

fn launch(app: &mut FmApp, choice: &AppChoice, path: &Path, cx: &mut Context<FmApp>) {
    let mut argv = expand_exec(&choice.exec, path);
    if choice.terminal {
        let Some(terminal) = app.overlays.caps.terminal.clone() else {
            return toast_error(app, "No terminal found · set $TERMINAL", cx);
        };
        let prefix: &[&str] = if is_wezterm(&terminal) { &["start", "--"] } else { &["-e"] };
        let mut full = vec![terminal];
        full.extend(prefix.iter().map(|s| s.to_string()));
        full.append(&mut argv);
        argv = full;
    }
    let Some((program, args)) = argv.split_first() else { return };
    let mut command = Command::new(program);
    command.args(args);
    if let Some(parent) = path.parent() {
        command.current_dir(parent);
    }
    if !ops::spawn_detached(command) {
        toast_error(app, format!("Could not start {}", choice.name), cx);
    }
}

/// `tar --zstd -cf <name>.tar.zst` next to the items, in the background.
fn compress(app: &mut FmApp, paths: Vec<PathBuf>, cx: &mut Context<FmApp>) {
    let Some(parent) = paths.first().and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
    toast(app, format!("Compressing {}…", ops::describe(&paths)), cx);
    let task = cx.background_spawn({
        let parent = parent.clone();
        async move {
            let base = match paths.as_slice() {
                [one] => format!("{}.tar.zst", ops::name_of(one)),
                _ => "archive.tar.zst".to_string(),
            };
            let taken = |name: &str| ops::exists(&parent.join(name));
            let name = if taken(&base) { ops::numbered_name(&base, false, taken) } else { base };
            let out = parent.join(&name);
            let output = Command::new("tar")
                .arg("--zstd")
                .arg("-cf")
                .arg(&out)
                .arg("-C")
                .arg(&parent)
                .arg("--")
                .args(paths.iter().filter_map(|p| p.file_name()))
                .stdin(Stdio::null())
                .output();
            match output {
                Ok(o) if o.status.success() => Ok(out),
                Ok(o) => {
                    let _ = fs::remove_file(&out);
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    Err(stderr.lines().last().unwrap_or("tar failed").trim().to_string())
                }
                Err(e) => Err(e.to_string()),
            }
        }
    });
    cx.spawn(async move |this, cx| {
        let result = task.await;
        this.update(cx, |app, cx| {
            match result {
                Ok(out) => {
                    let text = format!("Created {}", ops::name_of(&out));
                    let open = Some((parent.clone(), Some(out)));
                    show_toast(app, Toast { text: text.into(), error: false, open, undo: None }, cx);
                }
                Err(message) => toast_error(app, format!("Compress failed · {message}"), cx),
            }
            if app.cwd() == parent {
                app.reload(cx);
            }
        })
        .ok();
    })
    .detach();
}

// ── "Open with" lookup (background) ──────────────────────────────────────────

fn command_line(program: &str, args: &[&std::ffi::OsStr]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| fs_model::home_dir().join(".local/share"));
    let system = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    std::iter::once(home).chain(system.split(':').filter(|s| !s.is_empty()).map(PathBuf::from)).collect()
}

/// Parse a desktop entry; `Some` when it is launchable and handles `mime`.
fn parse_desktop(text: &str, mime: &str) -> Option<AppChoice> {
    let (mut name, mut exec, mut terminal, mut handles, mut hidden) = (None, None, false, false, false);
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Terminal" => terminal = value == "true",
            "NoDisplay" | "Hidden" => hidden |= value == "true",
            "MimeType" => {
                handles = value.split(';').any(|m| m == mime || (m == "text/plain" && mime.starts_with("text/")))
            }
            _ => {}
        }
    }
    (handles && !hidden).then_some(AppChoice { name: name?, exec: exec?, terminal, is_default: false })
}

/// Split an `Exec=` value into arguments (double quotes and backslash escapes).
fn split_exec(exec: &str) -> Vec<String> {
    let (mut args, mut current, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    started = true;
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Replace the file field codes of an `Exec=` line with `path`, drop the others.
fn expand_exec(exec: &str, path: &Path) -> Vec<String> {
    let file = path.to_string_lossy().to_string();
    let mut used = false;
    let mut args = Vec::new();
    for token in split_exec(exec) {
        if matches!(token.as_str(), "%f" | "%F" | "%u" | "%U") {
            args.push(file.clone());
            used = true;
        } else if token.len() == 2 && token.starts_with('%') && token != "%%" {
            // %i %c %k and deprecated codes expand to nothing.
        } else {
            let mut arg = token.replace("%%", "\u{0}");
            for code in ["%f", "%F", "%u", "%U"] {
                if arg.contains(code) {
                    arg = arg.replace(code, &file);
                    used = true;
                }
            }
            args.push(arg.replace('\u{0}', "%"));
        }
    }
    if !used && !args.is_empty() {
        args.push(file);
    }
    args
}

/// Applications for a file: desktop entries that declare its MIME type, or else whichever of
/// a short list of usual suspects is installed.
fn resolve_apps(path: &Path) -> Vec<AppChoice> {
    let mime = command_line("xdg-mime", &["query".as_ref(), "filetype".as_ref(), path.as_os_str()]);
    let default_id =
        mime.as_ref().and_then(|m| command_line("xdg-mime", &["query".as_ref(), "default".as_ref(), m.as_ref()]));

    let mut found: Vec<AppChoice> = Vec::new();
    if let Some(mime) = &mime {
        let mut seen: HashSet<OsString> = HashSet::new();
        for dir in data_dirs() {
            let Ok(entries) = fs::read_dir(dir.join("applications")) else { continue };
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if !file_name.to_string_lossy().ends_with(".desktop") || !seen.insert(file_name.clone()) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(entry.path()) else { continue };
                if let Some(mut choice) = parse_desktop(&text, mime) {
                    choice.is_default = default_id.as_deref() == file_name.to_str();
                    if !found.iter().any(|c| c.name == choice.name) {
                        found.push(choice);
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    found.truncate(8);
    if found.is_empty() {
        let usual = [("nvim", true), ("imv", false), ("mpv", false), ("zathura", false), ("firefox", false)];
        found = usual
            .into_iter()
            .filter(|(program, _)| on_path(program).is_some())
            .map(|(program, terminal)| AppChoice {
                name: program.to_string(),
                exec: format!("{program} %f"),
                terminal,
                is_default: false,
            })
            .collect();
    }
    found
}

// ── view ─────────────────────────────────────────────────────────────────────

fn panel(
    level: usize,
    width: f32,
    header: Option<SharedString>,
    rows: &[Row],
    highlight: Option<usize>,
    cx: &mut Context<FmApp>,
) -> Div {
    let t = theme(cx).clone();
    let tag_colors = [t.err, t.warn, t.ok, t.info, t.accent];
    let mut body = div().p(px(4.)).flex().flex_col();
    if let Some(header) = header {
        body = body.child(
            div().h(px(HEADER_H)).px(px(10.)).pt(px(4.)).flex().items_center().child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(t.font_mono.clone())
                    .text_size(px(9.5))
                    .text_color(t.fg_muted)
                    .child(header),
            ),
        );
    }
    for (ix, row) in rows.iter().enumerate() {
        let item = match row {
            Row::Sep => {
                body = body.child(div().h(px(1.)).my(px(4.)).flex_none().bg(t.line));
                continue;
            }
            Row::Item(item) => item,
        };
        let lead = match item.lead {
            Lead::Icon(ic) => {
                icon(ic, 14.).text_color(if item.destructive { t.err } else { t.fg_muted }).into_any_element()
            }
            Lead::Dot(tag) => div()
                .size(px(14.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .child(div().size(px(8.)).rounded_full().bg(tag_colors[tag % tag_colors.len()]))
                .into_any_element(),
            Lead::Blank => div().size(px(14.)).flex_none().into_any_element(),
        };
        let mut el = div()
            .id(("menu-item", level * 1000 + ix))
            .h(px(ITEM_H))
            .flex_none()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(t.radius_control)
            .text_size(px(12.5))
            .text_color(if item.destructive { t.err } else { t.fg })
            .when(highlight == Some(ix), |d| d.bg(t.surface_2))
            .child(lead)
            .child(div().flex_1().min_w_0().truncate().child(item.label.clone()));
        if let Some(hint) = item.hint {
            el = el.child(
                div().flex_none().font_family(t.font_mono.clone()).text_size(px(10.5)).text_color(t.fg_muted).child(hint),
            );
        }
        if matches!(item.cmd, Cmd::Sub(_)) {
            el = el.child(icon(Icon::ChevronRight, 12.).text_color(t.fg_muted));
        }
        el = if item.disabled {
            el.opacity(0.4)
        } else {
            el.cursor_pointer()
                .on_mouse_move(cx.listener(move |app, _: &MouseMoveEvent, _, cx| hover(app, level, ix, cx)))
                .on_click(cx.listener(move |app, _, window, cx| activate(app, level, ix, window, cx)))
        };
        body = body.child(el);
    }
    div()
        .absolute()
        .w(px(width))
        .bg(t.surface)
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_panel)
        .shadow(t.shadow_overlay())
        .occlude()
        // Clicks inside the menu are not "outside" clicks.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .child(body)
}

pub(super) fn render(app: &FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> Option<AnyElement> {
    let menu = app.overlays.menu.as_ref()?;
    let viewport = window.viewport_size();
    let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
    let place = |want: f32, size: f32, limit: f32| want.min(limit - size - EDGE).max(EDGE);

    let height = panel_height(menu.header.is_some(), &menu.rows);
    let x = place(f32::from(menu.position.x), MENU_W, vw);
    let y = place(f32::from(menu.position.y), height, vh);
    let fast = Duration::from_millis(theme::DUR_FAST);

    let main = panel(0, MENU_W, menu.header.clone(), &menu.rows, menu.highlight, cx)
        .left(px(x))
        .top(px(y))
        .with_animation(("menu", menu.seq), Animation::new(fast).with_easing(ease_standard), |el, delta| {
            el.opacity(delta)
        });

    let sub = menu.sub.as_ref().map(|sub| {
        let rows = sub_rows(menu, sub.kind);
        let above: f32 =
            menu.rows[..sub.parent.min(menu.rows.len())].iter().map(|r| if matches!(r, Row::Sep) { SEP_H } else { ITEM_H }).sum();
        let row_top = y + FRAME + if menu.header.is_some() { HEADER_H } else { 0. } + above;
        let sub_y = place(row_top - FRAME, panel_height(false, &rows), vh);
        // To the right of the parent, or to its left when there is no room.
        let fits_right = x + MENU_W - SUB_OVERLAP + SUB_W + EDGE <= vw;
        let (sub_x, from) = if fits_right { (x + MENU_W - SUB_OVERLAP, -4.) } else { (x - SUB_W + SUB_OVERLAP, 4.) };
        panel(1, SUB_W, None, &rows, sub.highlight, cx).top(px(sub_y)).with_animation(
            ElementId::Name(format!("submenu-{}-{}", menu.seq, sub.parent).into()),
            Animation::new(fast).with_easing(ease_standard),
            move |el, delta| el.opacity(delta).left(px(sub_x + from * (1. - delta))),
        )
    });

    Some(
        modal_layer(app, Layer::Menu, "FmMenu", false, cx)
            .on_action(cx.listener(|app, _: &MenuUp, _, cx| key_move(app, -1, cx)))
            .on_action(cx.listener(|app, _: &MenuDown, _, cx| key_move(app, 1, cx)))
            .on_action(cx.listener(|app, _: &MenuLeft, _, cx| key_left(app, cx)))
            .on_action(cx.listener(|app, _: &MenuRight, _, cx| key_right(app, cx)))
            .on_action(cx.listener(|app, _: &MenuActivate, window, cx| key_activate(app, window, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|app, _, window, cx| close(app, window, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|app, _, window, cx| close(app, window, cx)))
            .child(main)
            .children(sub)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_field_codes() {
        let path = Path::new("/tmp/a b.txt");
        assert_eq!(expand_exec("nvim %F", path), ["nvim", "/tmp/a b.txt"]);
        assert_eq!(expand_exec("app --name \"My App\" %i %c --file=%f", path), ["app", "--name", "My App", "--file=/tmp/a b.txt"]);
        assert_eq!(expand_exec("viewer", path), ["viewer", "/tmp/a b.txt"]);
        assert_eq!(expand_exec("sh -c \"echo 100%%\" %u", path), ["sh", "-c", "echo 100%", "/tmp/a b.txt"]);
    }

    #[test]
    fn desktop_entries() {
        let text = "[Desktop Entry]\nName=Neovim\nExec=nvim %F\nTerminal=true\nMimeType=text/plain;text/x-c;\n\n[Desktop Action new]\nName=Other\nExec=other\n";
        let app = parse_desktop(text, "text/x-rust").unwrap();
        assert_eq!((app.name.as_str(), app.exec.as_str(), app.terminal), ("Neovim", "nvim %F", true));
        assert!(parse_desktop(text, "image/png").is_none());
        assert!(parse_desktop(&text.replace("Terminal=true", "NoDisplay=true"), "text/plain").is_none());
    }

    #[test]
    fn keyboard_steps_skip_separators_and_disabled_rows() {
        let rows = vec![
            Row::Item(Item::new(Icon::File, "a", Cmd::Nothing)),
            Row::Sep,
            Row::Item(Item::new(Icon::File, "b", Cmd::Nothing).disabled(true)),
            Row::Item(Item::new(Icon::File, "c", Cmd::Nothing)),
        ];
        assert_eq!(step(&rows, None, 1), Some(0));
        assert_eq!(step(&rows, Some(0), 1), Some(3));
        assert_eq!(step(&rows, Some(3), 1), Some(0));
        assert_eq!(step(&rows, None, -1), Some(3));
        assert_eq!(step(&rows, Some(0), -1), Some(3));
        assert_eq!(panel_height(true, &rows), 10. + 24. + 99.);
    }
}
