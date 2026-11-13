//! Sidebar 230 (SPEC §4.1): places, devices, bookmarks, tags. Collapses to a 44px icon
//! rail below `BREAK_SIDEBAR` with hover flyout labels.
//!
//! OWNER: sidebar agent.
//!
//! Everything that touches the disk (XDG dirs, trash count, `/proc/self/mounts`, `statvfs`,
//! the bookmark/tag files, `udisksctl`) runs on the background executor; `render` only reads
//! the last snapshot stored in `SidebarState`.
//!
//! API for other modules: [`toggle_bookmark`], [`is_bookmarked`], [`add_tag`], [`remove_tag`],
//! [`tags_for`], [`tag_color`], [`TAG_NAMES`], [`free_space_label`].

use std::{
    collections::HashMap,
    ffi::CString,
    fs,
    mem::MaybeUninit,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use gpui::{
    Animation, AnimationExt, AnyElement, Context, Div, Hsla, MouseButton, SharedString, Stateful, Window,
    deferred, div, prelude::*, px, relative,
};

use crate::{
    app::FmApp,
    fs_model,
    icons::{Icon, icon},
    theme::{self, Theme, theme},
    ui,
};

/// Tag palette, in display order. Colours: see [`tag_color`].
pub const TAG_NAMES: [&str; 5] = ["red", "yellow", "green", "blue", "accent"];

/// Devices are re-scanned at most this often.
const DEVICE_REFRESH: Duration = Duration::from_secs(5);
/// Delay before a rail item's flyout label appears.
const FLYOUT_DELAY: Duration = Duration::from_millis(120);
const PADDING: f32 = 6.;
const ITEM_H: f32 = 28.;
/// Capacity bar turns `warn` at this fill fraction.
const WARN_FRACTION: f32 = 0.85;

#[derive(Clone)]
struct Place {
    path: PathBuf,
    name: SharedString,
    icon: Icon,
    /// Right-aligned meta: item count (trash) or `git`.
    meta: Option<SharedString>,
}

#[derive(Clone)]
struct Mount {
    /// Device node as written in the mount table (what `udisksctl -b` wants).
    device: String,
    mount: PathBuf,
    name: SharedString,
    total: u64,
    used: u64,
    /// Space available to the user.
    free: u64,
    /// Listed under "devices" (a real block device, deduplicated).
    shown: bool,
    removable: bool,
}

struct Tag {
    name: SharedString,
    path: PathBuf,
}

pub struct SidebarState {
    places: Vec<Place>,
    /// Every mount with a capacity (also the ones not listed), for `free_space_label`.
    mounts: Vec<Mount>,
    bookmarks: Vec<PathBuf>,
    tags: Vec<Tag>,
    /// Bookmark/tag files have been read; until then in-memory edits are merged on load.
    config_loaded: bool,
    places_generation: u64,
    devices_generation: u64,
    last_device_refresh: Instant,
    /// Rail flyout: hovered item key, whether its delay has elapsed, and a counter that
    /// invalidates pending delay timers.
    hover: Option<SharedString>,
    hover_visible: bool,
    hover_generation: u64,
}

impl SidebarState {
    pub fn new(_window: &mut Window, cx: &mut Context<FmApp>) -> Self {
        spawn_refresh(Some(1), 1, true, cx);
        SidebarState {
            places: Vec::new(),
            mounts: Vec::new(),
            bookmarks: Vec::new(),
            tags: Vec::new(),
            config_loaded: false,
            places_generation: 1,
            devices_generation: 1,
            last_device_refresh: Instant::now(),
            hover: None,
            hover_visible: false,
            hover_generation: 0,
        }
    }
}

// ── public API ────────────────────────────────────────────────────────────────

/// Called by the shell after the active tab's directory has (re)loaded.
pub fn on_directory_loaded(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let sb = &mut app.sidebar;
    sb.places_generation += 1;
    let devices = (sb.last_device_refresh.elapsed() >= DEVICE_REFRESH).then(|| {
        sb.last_device_refresh = Instant::now();
        sb.devices_generation += 1;
        sb.devices_generation
    });
    spawn_refresh(devices, sb.places_generation, false, cx);
}

/// Footer text for the device holding the cwd, e.g. `64G free`.
pub fn free_space_label(app: &FmApp) -> Option<String> {
    let cwd = app.cwd();
    app.sidebar
        .mounts
        .iter()
        .filter(|m| cwd.starts_with(&m.mount))
        .max_by_key(|m| m.mount.as_os_str().len())
        .map(|m| format!("{} free", short_size(m.free)))
}

pub fn is_bookmarked(app: &FmApp, path: &Path) -> bool {
    app.sidebar.bookmarks.iter().any(|p| p == path)
}

/// Adds `path` to the bookmarks, or removes it when it is already there. Persists.
pub fn toggle_bookmark(app: &mut FmApp, path: &Path, cx: &mut Context<FmApp>) {
    let sb = &mut app.sidebar;
    match sb.bookmarks.iter().position(|p| p == path) {
        Some(ix) => {
            sb.bookmarks.remove(ix);
        }
        None => sb.bookmarks.push(path.to_path_buf()),
    }
    save_bookmarks(sb, cx);
    cx.notify();
}

/// Colour of a tag: red→err, yellow→warn, green→ok, blue→info, accent→accent
/// (anything else: `fg_muted`).
pub fn tag_color(name: &str, t: &Theme) -> Hsla {
    match name {
        "red" => t.err,
        "yellow" => t.warn,
        "green" => t.ok,
        "blue" => t.info,
        "accent" => t.accent,
        _ => t.fg_muted,
    }
}

/// Tags `path` with `tag_name` (one of [`TAG_NAMES`]; other names are accepted and drawn
/// muted). No-op when already tagged. Persists.
pub fn add_tag(app: &mut FmApp, path: &Path, tag_name: &str, cx: &mut Context<FmApp>) {
    let tag_name = tag_name.trim();
    let sb = &mut app.sidebar;
    if tag_name.is_empty()
        || tag_name.contains(['\t', '\n'])
        || sb.tags.iter().any(|t| t.name.as_ref() == tag_name && t.path == path)
    {
        return;
    }
    sb.tags.push(Tag { name: tag_name.to_string().into(), path: path.to_path_buf() });
    save_tags(sb, cx);
    cx.notify();
}

/// Removes `tag_name` from `path`. Persists.
pub fn remove_tag(app: &mut FmApp, path: &Path, tag_name: &str, cx: &mut Context<FmApp>) {
    let sb = &mut app.sidebar;
    let before = sb.tags.len();
    sb.tags.retain(|t| !(t.name.as_ref() == tag_name && t.path == path));
    if sb.tags.len() != before {
        save_tags(sb, cx);
        cx.notify();
    }
}

/// Names of the tags on `path` (colour them with [`tag_color`]).
pub fn tags_for(app: &FmApp, path: &Path) -> Vec<SharedString> {
    app.sidebar.tags.iter().filter(|t| t.path == path).map(|t| t.name.clone()).collect()
}

// ── refresh (background) ──────────────────────────────────────────────────────

struct Config {
    bookmarks: Vec<PathBuf>,
    tags: Vec<Tag>,
}

/// Scans places (always), devices (when `devices` carries their generation) and the
/// bookmark/tag files (`load_config`, startup only) off the main thread, then applies them.
fn spawn_refresh(devices: Option<u64>, places_generation: u64, load_config: bool, cx: &mut Context<FmApp>) {
    let task = cx.background_spawn(async move {
        let places = scan_places();
        let mounts = devices.map(|_| scan_mounts());
        let config = load_config.then(read_config);
        (places, mounts, config)
    });
    cx.spawn(async move |this, cx| {
        let (places, mounts, config) = task.await;
        this.update(cx, |this, cx| {
            let sb = &mut this.sidebar;
            if sb.places_generation == places_generation {
                sb.places = places;
            }
            if let (Some(mounts), Some(generation)) = (mounts, devices)
                && sb.devices_generation == generation
            {
                sb.mounts = mounts;
            }
            if let Some(config) = config
                && !sb.config_loaded
            {
                // Keep anything added before the files were read.
                let added = std::mem::replace(&mut sb.bookmarks, config.bookmarks);
                for path in added {
                    if !sb.bookmarks.contains(&path) {
                        sb.bookmarks.push(path);
                    }
                }
                let added = std::mem::replace(&mut sb.tags, config.tags);
                for tag in added {
                    if !sb.tags.iter().any(|t| t.name == tag.name && t.path == tag.path) {
                        sb.tags.push(tag);
                    }
                }
                sb.config_loaded = true;
            }
            cx.notify();
        })
        .ok();
    })
    .detach();
}

fn refresh_devices_now(app: &mut FmApp, cx: &mut Context<FmApp>) {
    let sb = &mut app.sidebar;
    sb.last_device_refresh = Instant::now();
    sb.devices_generation += 1;
    sb.places_generation += 1;
    spawn_refresh(Some(sb.devices_generation), sb.places_generation, false, cx);
}

fn eject(device: String, cx: &mut Context<FmApp>) {
    let task = cx.background_spawn(async move {
        // Failure (busy, not installed, not permitted) just leaves the device listed.
        let _ = Command::new("udisksctl")
            .args(["unmount", "-b", &device])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    cx.spawn(async move |this, cx| {
        task.await;
        this.update(cx, refresh_devices_now).ok();
    })
    .detach();
}

// ── places ────────────────────────────────────────────────────────────────────

/// `XDG_<KEY>_DIR` entries of `~/.config/user-dirs.dirs`.
fn xdg_user_dirs(home: &Path) -> HashMap<String, PathBuf> {
    let mut dirs = HashMap::new();
    let Ok(text) = fs::read_to_string(config_home().join("user-dirs.dirs")) else { return dirs };
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        let Some(key) = key.trim().strip_prefix("XDG_").and_then(|k| k.strip_suffix("_DIR")) else { continue };
        let value = value.trim().trim_matches('"');
        let path = if let Some(rest) = value.strip_prefix("$HOME") {
            home.join(rest.trim_start_matches('/'))
        } else if value.starts_with('/') {
            PathBuf::from(value)
        } else {
            continue;
        };
        dirs.insert(key.to_string(), path);
    }
    dirs
}

fn scan_places() -> Vec<Place> {
    let home = fs_model::home_dir();
    let xdg = xdg_user_dirs(&home);
    let mut places = Vec::new();
    let git_meta = |path: &Path| path.join(".git").exists().then(|| SharedString::from("git"));

    if home.is_dir() {
        places.push(Place { meta: git_meta(&home), path: home.clone(), name: "Home".into(), icon: Icon::Home });
    }
    let user_dirs = [
        ("DESKTOP", "Desktop", Icon::Folder),
        ("DOCUMENTS", "Documents", Icon::Folder),
        ("DOWNLOAD", "Downloads", Icon::Download),
        ("PICTURES", "Pictures", Icon::Image),
        ("MUSIC", "Music", Icon::Music),
        ("VIDEOS", "Videos", Icon::Video),
    ];
    for (key, fallback, icon) in user_dirs {
        let path = xdg.get(key).cloned().unwrap_or_else(|| home.join(fallback));
        // A disabled XDG dir points at $HOME itself.
        if path == home || !path.is_dir() || places.iter().any(|p| p.path == path) {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| fallback.to_string());
        places.push(Place { meta: git_meta(&path), path, name: name.into(), icon });
    }
    let trash = fs_model::trash_dir();
    let count = fs::read_dir(&trash).map(|rd| rd.count()).unwrap_or(0);
    places.push(Place {
        path: trash,
        name: "Trash".into(),
        icon: Icon::Trash,
        meta: (count > 0).then(|| count.to_string().into()),
    });
    places.push(Place { path: PathBuf::from("/"), name: "/".into(), icon: Icon::Folder, meta: None });
    places
}

// ── devices ───────────────────────────────────────────────────────────────────

/// Filesystems without a capacity worth asking for (and `autofs`, which may block).
const PSEUDO_FS: [&str; 22] = [
    "proc", "sysfs", "devtmpfs", "devpts", "cgroup", "cgroup2", "securityfs", "debugfs", "tracefs",
    "configfs", "pstore", "bpf", "autofs", "mqueue", "hugetlbfs", "fusectl", "efivarfs", "binfmt_misc",
    "rpc_pipefs", "nsfs", "selinuxfs", "fuse.portal",
];
/// Block-device-backed mounts that are not user storage.
const HIDDEN_FS: [&str; 2] = ["squashfs", "erofs"];
const HIDDEN_MOUNT_PREFIXES: [&str; 6] = ["/boot", "/efi", "/nix", "/snap", "/var/lib/docker", "/var/lib/containers"];

/// Decodes the `\040`-style octal escapes of `/proc/self/mounts`.
fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Decodes the `\x20`-style escapes udev uses in `/dev/disk/by-label` names.
fn unescape_hex(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x')
            .then(|| std::str::from_utf8(&b[i + 2..i + 4]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match hex {
            Some(v) => {
                out.push(v);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(total, used, available to the user)` in bytes.
#[allow(clippy::unnecessary_cast)] // field widths differ between targets
fn statvfs(path: &Path) -> Option<(u64, u64, u64)> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is a valid NUL-terminated string and `buf` is a writable `statvfs`.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled `buf`.
    let s = unsafe { buf.assume_init() };
    let unit = s.f_frsize as u64;
    let total = (s.f_blocks as u64).saturating_mul(unit);
    let free = (s.f_bfree as u64).saturating_mul(unit);
    let avail = (s.f_bavail as u64).saturating_mul(unit);
    Some((total, total.saturating_sub(free), avail))
}

/// Canonical device node → filesystem label.
fn disk_labels() -> HashMap<PathBuf, String> {
    let mut labels = HashMap::new();
    if let Ok(rd) = fs::read_dir("/dev/disk/by-label") {
        for entry in rd.flatten() {
            if let Ok(dev) = fs::canonicalize(entry.path()) {
                labels.insert(dev, unescape_hex(&entry.file_name().to_string_lossy()));
            }
        }
    }
    labels
}

fn is_removable(device: &Path, mount: &Path) -> bool {
    if mount.starts_with("/run/media") || mount.starts_with("/media") {
        return true;
    }
    let Some(name) = device.file_name() else { return false };
    let Ok(sys) = fs::canonicalize(Path::new("/sys/class/block").join(name)) else { return false };
    // A partition's `removable` flag lives on its parent disk.
    let flag = |dir: &Path| fs::read_to_string(dir.join("removable")).is_ok_and(|s| s.trim() == "1");
    flag(&sys) || sys.parent().is_some_and(flag)
}

fn scan_mounts() -> Vec<Mount> {
    let Ok(table) = fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    let labels = disk_labels();
    let mut mounts: Vec<Mount> = Vec::new();
    // Canonical device → index of the mount that represents it in the list.
    let mut shown: HashMap<PathBuf, usize> = HashMap::new();

    for line in table.lines() {
        let mut fields = line.split(' ');
        let (Some(device), Some(mount), Some(fstype)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if PSEUDO_FS.contains(&fstype) {
            continue;
        }
        let mount = PathBuf::from(std::ffi::OsString::from_vec(unescape_octal(mount)));
        if mounts.iter().any(|m| m.mount == mount) {
            continue;
        }
        let Some((total, used, free)) = statvfs(&mount) else { continue };
        if total == 0 {
            continue;
        }
        let device = String::from_utf8_lossy(&unescape_octal(device)).into_owned();
        let real = device.starts_with("/dev/")
            && !HIDDEN_FS.contains(&fstype)
            && !HIDDEN_MOUNT_PREFIXES.iter().any(|p| mount.starts_with(p));
        let canonical = if real { fs::canonicalize(&device).unwrap_or_else(|_| PathBuf::from(&device)) } else { PathBuf::new() };
        let name = if !real {
            SharedString::default()
        } else if let Some(label) = labels.get(&canonical) {
            label.clone().into()
        } else if mount == Path::new("/") {
            "root".into()
        } else {
            mount.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| device.clone()).into()
        };
        let removable = real && is_removable(&canonical, &mount);
        let ix = mounts.len();
        mounts.push(Mount { device, mount, name, total, used, free, shown: real, removable });
        if real {
            // One row per device (btrfs subvolumes, bind mounts): keep the shortest mount point.
            match shown.get(&canonical).copied() {
                Some(prev) if mounts[prev].mount.as_os_str().len() <= mounts[ix].mount.as_os_str().len() => {
                    mounts[ix].shown = false;
                }
                Some(prev) => {
                    mounts[prev].shown = false;
                    shown.insert(canonical, ix);
                }
                None => {
                    shown.insert(canonical, ix);
                }
            }
        }
    }
    mounts
}

/// `812M`, `9.4G`, `27G`, `1.8T` — one decimal below 10, none above.
fn short_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024. && u < UNITS.len() - 1 {
        v /= 1024.;
        u += 1;
    }
    if v >= 9.95 || u == 0 { format!("{v:.0}{}", UNITS[u]) } else { format!("{v:.1}{}", UNITS[u]) }
}

// ── bookmarks & tags (persistence) ────────────────────────────────────────────

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| fs_model::home_dir().join(".config"))
}

fn bookmarks_file() -> PathBuf {
    config_home().join("fm/bookmarks")
}

fn tags_file() -> PathBuf {
    config_home().join("fm/tags")
}

fn read_config() -> Config {
    let mut bookmarks: Vec<PathBuf> = Vec::new();
    for line in fs::read_to_string(bookmarks_file()).unwrap_or_default().lines() {
        let path = PathBuf::from(line);
        if path.is_absolute() && !bookmarks.contains(&path) {
            bookmarks.push(path);
        }
    }
    let mut tags: Vec<Tag> = Vec::new();
    for line in fs::read_to_string(tags_file()).unwrap_or_default().lines() {
        let Some((name, path)) = line.split_once('\t') else { continue };
        let path = PathBuf::from(path);
        if name.is_empty() || !path.is_absolute() || tags.iter().any(|t| t.name.as_ref() == name && t.path == path) {
            continue;
        }
        tags.push(Tag { name: name.to_string().into(), path });
    }
    Config { bookmarks, tags }
}

/// Writes `content` atomically (temp file + rename) on the background executor.
fn write_config(file: PathBuf, content: String, cx: &mut Context<FmApp>) {
    cx.background_spawn(async move {
        let Some(dir) = file.parent() else { return };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = file.with_extension("tmp");
        if fs::write(&tmp, content).is_ok() {
            let _ = fs::rename(&tmp, &file);
        }
    })
    .detach();
}

fn save_bookmarks(sb: &SidebarState, cx: &mut Context<FmApp>) {
    let content: String = sb.bookmarks.iter().map(|p| format!("{}\n", p.display())).collect();
    write_config(bookmarks_file(), content, cx);
}

fn save_tags(sb: &SidebarState, cx: &mut Context<FmApp>) {
    let content: String = sb.tags.iter().map(|t| format!("{}\t{}\n", t.name, t.path.display())).collect();
    write_config(tags_file(), content, cx);
}

// ── render ────────────────────────────────────────────────────────────────────

enum Lead {
    Icon(Icon),
    /// 8px tag dot.
    Dot(Hsla),
}

struct ItemSpec {
    /// Unique element id, also the flyout hover key.
    key: SharedString,
    lead: Lead,
    name: SharedString,
    meta: Option<SharedString>,
    active: bool,
}

fn hover_changed(app: &mut FmApp, key: &SharedString, hovered: bool, cx: &mut Context<FmApp>) {
    let sb = &mut app.sidebar;
    if hovered {
        sb.hover = Some(key.clone());
        sb.hover_visible = false;
        sb.hover_generation += 1;
        let generation = sb.hover_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLYOUT_DELAY).await;
            this.update(cx, |this, cx| {
                if this.sidebar.hover_generation == generation {
                    this.sidebar.hover_visible = true;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    } else if sb.hover.as_ref() == Some(key) {
        sb.hover = None;
        sb.hover_visible = false;
        sb.hover_generation += 1;
        cx.notify();
    }
}

/// Flyout label to the right of a rail item. Deferred, so it escapes the rail's clipping.
fn flyout(spec: &ItemSpec, t: &Theme) -> impl IntoElement + use<> {
    let item_w = theme::SIDEBAR_RAIL_W - 2. * PADDING;
    let body = div()
        .absolute()
        .left(px(item_w + PADDING + 4.))
        .top(px(2.))
        .h(px(ITEM_H - 4.))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(10.))
        .bg(t.surface)
        .border_1()
        .border_color(t.line)
        .rounded(t.radius_control)
        .shadow(t.shadow_flyout())
        .font_family(t.font_sans.clone())
        .text_size(px(12.))
        .text_color(t.fg)
        .whitespace_nowrap()
        .child(spec.name.clone())
        .when_some(spec.meta.clone(), |d, meta| {
            d.child(
                div()
                    .font_family(t.font_mono.clone())
                    .text_size(px(10.5))
                    .text_color(t.fg_muted)
                    .child(SharedString::from(meta.to_uppercase())),
            )
        })
        .with_animation(
            spec.key.clone(),
            Animation::new(Duration::from_millis(theme::DUR_FAST)).with_easing(theme::ease_enter),
            |d, delta| d.opacity(delta),
        );
    deferred(body).with_priority(1)
}

/// One 28px sidebar row (or a centred rail icon). Callers add the click handlers.
fn item(spec: ItemSpec, rail: bool, hover: Option<&SharedString>, t: &Theme, cx: &mut Context<FmApp>) -> Stateful<Div> {
    let hover_bg = t.surface_2;
    let lead = match spec.lead {
        Lead::Icon(ic) => icon(ic, 14.).text_color(t.fg_muted).into_any_element(),
        Lead::Dot(color) => div().size(px(8.)).mx(px(3.)).flex_none().rounded(px(4.)).bg(color).into_any_element(),
    };
    let mut row = div()
        .id(spec.key.clone())
        .relative()
        .h(px(ITEM_H))
        .w_full()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(10.))
        .cursor_pointer()
        .font_family(t.font_sans.clone())
        .text_size(px(12.5))
        .text_color(t.fg)
        .hover(move |s| s.bg(hover_bg));
    if spec.active {
        row = row
            .bg(t.surface_2)
            .child(div().absolute().left_0().top(px(6.)).bottom(px(6.)).w(px(2.)).bg(t.accent));
    }
    if rail {
        let key = spec.key.clone();
        let show = hover == Some(&spec.key);
        row.justify_center()
            .child(lead)
            .when(show, |d| d.child(flyout(&spec, t)))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| hover_changed(this, &key, *hovered, cx)))
    } else {
        row.px(px(10.))
            .child(lead)
            .child(div().flex_1().min_w_0().truncate().child(spec.name))
            .when_some(spec.meta, |d, meta| d.child(ui::label(meta, cx)))
    }
}

fn group_header(text: &'static str, first: bool, rail: bool, cx: &mut Context<FmApp>) -> AnyElement {
    if rail {
        // Icon rail: no labels, thin separators between groups.
        if first {
            return div().flex_none().into_any_element();
        }
        return div().flex_none().py(px(6.)).px(px(6.)).child(ui::hline(cx)).into_any_element();
    }
    div().flex_none().pt(px(14.)).px(px(10.)).pb(px(6.)).child(ui::label(text, cx)).into_any_element()
}

/// Name row, 3px capacity bar and `used/total` readout (SPEC §4.1 "Device").
fn device(ix: usize, m: &Mount, t: &Theme, cx: &mut Context<FmApp>) -> Stateful<Div> {
    let fraction = if m.total == 0 { 0. } else { (m.used as f32 / m.total as f32).clamp(0., 1.) };
    let bar_color = if fraction >= WARN_FRACTION { t.warn } else { t.accent };
    let hover_bg = t.surface_2;
    let mut name_row = div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(icon(Icon::Disk, 14.).text_color(t.fg_muted))
        .child(div().flex_1().min_w_0().truncate().child(m.name.clone()));
    if m.removable {
        let dev = m.device.clone();
        name_row = name_row.child(
            div()
                .id(("device-eject", ix))
                .size(px(22.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(t.radius_control)
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .child(icon(Icon::Eject, 13.).text_color(t.fg_muted))
                .on_click(cx.listener(move |_, _, _, cx| {
                    // Don't also navigate to the mount point (the row's own click).
                    cx.stop_propagation();
                    eject(dev.clone(), cx);
                })),
        );
    }
    div()
        .id(("device", ix))
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(6.))
        .px(px(10.))
        .py(px(6.))
        .cursor_pointer()
        .font_family(t.font_sans.clone())
        .text_size(px(12.5))
        .text_color(t.fg)
        .hover(move |s| s.bg(hover_bg))
        .child(name_row)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div().relative().flex_1().h(px(3.)).bg(t.line).child(
                        div().absolute().left_0().top_0().bottom_0().w(relative(fraction)).bg(bar_color),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(t.font_mono.clone())
                        .text_size(px(10.))
                        .text_color(t.fg_muted)
                        .child(format!("{}/{}", short_size(m.used), short_size(m.total))),
                ),
        )
}

/// Left click navigates, middle click opens a new tab.
/// Dragged files can be dropped on it (except the trash, which is not a plain folder).
fn with_navigation(app: &FmApp, el: Stateful<Div>, path: &Path, cx: &mut Context<FmApp>) -> Stateful<Div> {
    let (click_path, middle_path) = (path.to_path_buf(), path.to_path_buf());
    let el = if path == crate::fs_model::trash_dir() {
        el
    } else {
        crate::file_view::as_drop_target(app, el, path.to_path_buf(), cx)
    };
    el.on_click(cx.listener(move |this, _, _, cx| this.navigate(click_path.clone(), cx))).on_mouse_down(
        MouseButton::Middle,
        cx.listener(move |this, _, _, cx| this.new_tab(Some(middle_path.clone()), cx)),
    )
}

pub fn render(app: &mut FmApp, window: &mut Window, cx: &mut Context<FmApp>) -> AnyElement {
    let t = theme(cx).clone();
    let rail = window.viewport_size().width < px(theme::BREAK_SIDEBAR);
    let cwd = app.cwd().to_path_buf();
    let sb = &app.sidebar;
    let hover = sb.hover.clone().filter(|_| rail && sb.hover_visible);
    let hover = hover.as_ref();
    let mut children: Vec<AnyElement> = Vec::new();

    // places — the deepest place containing the cwd is the active one.
    let active_place = sb
        .places
        .iter()
        .enumerate()
        .filter(|(_, p)| cwd.starts_with(&p.path))
        .max_by_key(|(_, p)| p.path.components().count())
        .map(|(ix, _)| ix);
    children.push(group_header("places", true, rail, cx));
    for (ix, place) in sb.places.iter().enumerate() {
        let spec = ItemSpec {
            key: format!("place-{ix}").into(),
            lead: Lead::Icon(place.icon),
            name: place.name.clone(),
            meta: place.meta.clone(),
            active: active_place == Some(ix),
        };
        children.push(with_navigation(app, item(spec, rail, hover, &t, cx), &place.path, cx).into_any_element());
    }

    // devices
    children.push(group_header("devices", false, rail, cx));
    for (ix, m) in sb.mounts.iter().enumerate().filter(|(_, m)| m.shown) {
        let el = if rail {
            let spec = ItemSpec {
                key: format!("device-rail-{ix}").into(),
                lead: Lead::Icon(Icon::Disk),
                name: m.name.clone(),
                meta: Some(format!("{}/{}", short_size(m.used), short_size(m.total)).into()),
                active: false,
            };
            item(spec, rail, hover, &t, cx)
        } else {
            device(ix, m, &t, cx)
        };
        children.push(with_navigation(app, el, &m.mount, cx).into_any_element());
    }

    // bookmarks
    if !sb.bookmarks.is_empty() {
        children.push(group_header("bookmarks", false, rail, cx));
        for (ix, path) in sb.bookmarks.iter().enumerate() {
            let spec = ItemSpec {
                key: format!("bookmark-{ix}").into(),
                lead: Lead::Icon(Icon::Pin),
                name: fs_model::display_path(path).into(),
                meta: None,
                active: *path == cwd,
            };
            let remove = path.clone();
            children.push(
                with_navigation(app, item(spec, rail, hover, &t, cx), path, cx)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if is_bookmarked(this, &remove) {
                                toggle_bookmark(this, &remove, cx);
                            }
                        }),
                    )
                    .into_any_element(),
            );
        }
    }

    // tags — palette order first, then any other names alphabetically.
    if !sb.tags.is_empty() {
        let mut names: Vec<SharedString> = Vec::new();
        for tag in &sb.tags {
            if !names.contains(&tag.name) {
                names.push(tag.name.clone());
            }
        }
        names.sort_by_key(|n| {
            (TAG_NAMES.iter().position(|p| *p == n.as_ref()).unwrap_or(TAG_NAMES.len()), n.clone())
        });
        children.push(group_header("tags", false, rail, cx));
        for (ix, name) in names.into_iter().enumerate() {
            let tagged: Vec<&PathBuf> = sb.tags.iter().filter(|tag| tag.name == name).map(|tag| &tag.path).collect();
            // A tagged folder is the tag's destination (click opens it, files dropped on the
            // tag move into it); with only files tagged, click shows the first one's folder.
            let folder = tagged.iter().find(|p| p.is_dir()).map(|p| p.to_path_buf());
            let target = folder.clone().or_else(|| tagged.first().and_then(|p| p.parent().map(Path::to_path_buf)));
            let count = tagged.len();
            let spec = ItemSpec {
                key: format!("tag-{ix}").into(),
                lead: Lead::Dot(tag_color(&name, &t)),
                name,
                meta: Some(count.to_string().into()),
                active: false,
            };
            let el = item(spec, rail, hover, &t, cx);
            let el = match folder {
                Some(folder) => crate::file_view::as_drop_target(app, el, folder, cx),
                None => el,
            };
            children.push(match target {
                Some(path) => el.on_click(cx.listener(move |this, _, _, cx| this.navigate(path.clone(), cx))),
                None => el,
            }
            .into_any_element());
        }
    }

    div()
        .id("sidebar")
        .w(px(if rail { theme::SIDEBAR_RAIL_W } else { theme::SIDEBAR_W }))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .p(px(PADDING))
        .bg(t.surface)
        .border_r_1()
        .border_color(t.line)
        .overflow_y_scroll()
        .children(children)
        .into_any_element()
}
