//! Root view: owns all state, wires actions, lays out the window (SPEC §3).
//!
//! Each region lives in its own module that exposes a `XxxState` struct (a field on
//! `FmApp`) and a `render(app, window, cx)` function. Modules talk to each other
//! through the public methods here, or by dispatching actions
//! (`window.dispatch_action(Box::new(actions::Copy), cx)`).

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use gpui::{
    ClipboardItem, Context, FocusHandle, Focusable, KeyContext, SharedString, UniformListScrollHandle,
    ScrollStrategy, Window, div, prelude::*, px,
};

use crate::{
    actions::*,
    chrome, file_view,
    fs_model::{self, DirError, Entry, SortKey, SortSpec},
    git::{self, GitState},
    ops::{self, OpsState},
    overlays::{self, OverlaysState},
    preview::{self, PreviewState},
    quicklook::{self, QuickLookState},
    sidebar::{self, SidebarState},
    theme::{self, Theme, theme},
    toolbar::{self, ToolbarState},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    List,
    Grid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    Loading,
    Ready,
    Error(DirError),
}

pub struct Tab {
    /// Stable id (jobs are anchored to the tab that started them).
    pub id: u64,
    pub path: PathBuf,
    pub history: Vec<PathBuf>,
    pub history_ix: usize,
    /// Everything in `path`, unsorted and unfiltered.
    pub entries: Vec<Entry>,
    /// Indices into `entries` after hidden-filter, text filter and sort. This is what is on screen.
    pub visible: Vec<usize>,
    /// Cursor row: an index into `visible`.
    pub cursor: usize,
    /// Multi-selection (paths). Empty means "the cursor row is the selection".
    pub selected: BTreeSet<PathBuf>,
    /// Anchor row for shift-extend.
    pub anchor: Option<usize>,
    /// Live filter text (toolbar filter field).
    pub filter: String,
    pub load: LoadState,
    /// Scroll handle for the list view's `uniform_list`.
    pub scroll: UniformListScrollHandle,
    /// After the next load, put the cursor on this path…
    pub pending_cursor: Option<PathBuf>,
    /// …and start an inline rename on it.
    pub pending_rename: bool,
    generation: u64,
}

impl Tab {
    fn new(id: u64, path: PathBuf) -> Self {
        Tab {
            id,
            history: vec![path.clone()],
            history_ix: 0,
            path,
            entries: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            selected: BTreeSet::new(),
            anchor: None,
            filter: String::new(),
            load: LoadState::Loading,
            scroll: UniformListScrollHandle::new(),
            pending_cursor: None,
            pending_rename: false,
            generation: 0,
        }
    }

    pub fn title(&self) -> String {
        fs_model::short_name(&self.path)
    }

    /// Entry at a row of the visible list.
    pub fn entry_at(&self, row: usize) -> Option<&Entry> {
        self.visible.get(row).and_then(|ix| self.entries.get(*ix))
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.entry_at(self.cursor)
    }

    pub fn visible_entries(&self) -> impl Iterator<Item = &Entry> {
        self.visible.iter().filter_map(|ix| self.entries.get(*ix))
    }

    pub fn row_of(&self, path: &Path) -> Option<usize> {
        self.visible.iter().position(|ix| self.entries[*ix].path == path)
    }

    pub fn is_selected(&self, path: &Path) -> bool {
        self.selected.contains(path)
    }

    /// What an operation acts on: the multi-selection, or else the cursor row.
    pub fn targets(&self) -> Vec<PathBuf> {
        if self.selected.is_empty() {
            self.cursor_entry().map(|e| vec![e.path.clone()]).unwrap_or_default()
        } else {
            self.selected.iter().cloned().collect()
        }
    }

    pub fn selected_size(&self) -> u64 {
        self.entries.iter().filter(|e| self.selected.contains(&e.path)).map(|e| e.size).sum()
    }

    /// Rebuild `visible`, keeping the cursor on the same entry when possible.
    fn recompute(&mut self, sort: SortSpec, show_hidden: bool) {
        let keep = self.pending_cursor.clone().or_else(|| self.cursor_entry().map(|e| e.path.clone()));
        let needle = self.filter.to_lowercase();
        let mut vis: Vec<usize> = (0..self.entries.len())
            .filter(|ix| {
                let e = &self.entries[*ix];
                (show_hidden || !e.hidden) && (needle.is_empty() || e.name.to_lowercase().contains(&needle))
            })
            .collect();
        vis.sort_by(|a, b| sort.compare(&self.entries[*a], &self.entries[*b]));
        self.visible = vis;
        self.cursor = keep
            .and_then(|p| self.row_of(&p))
            .unwrap_or_else(|| self.cursor.min(self.visible.len().saturating_sub(1)));
        let present: BTreeSet<PathBuf> = self.visible_entries().map(|e| e.path.clone()).collect();
        self.selected.retain(|p| present.contains(p));
    }
}

pub struct FmApp {
    /// Focus of the file list (the "nothing else is focused" state). Single-letter keys need it.
    pub focus: FocusHandle,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    next_tab_id: u64,
    pub view_mode: ViewMode,
    pub sort: SortSpec,
    pub show_hidden: bool,
    /// Preview pane toggle (`z`); it also auto-collapses below `BREAK_PREVIEW`.
    pub show_preview: bool,

    pub sidebar: SidebarState,
    pub toolbar: ToolbarState,
    pub files: file_view::FileViewState,
    pub preview: PreviewState,
    pub quicklook: QuickLookState,
    pub overlays: OverlaysState,
    pub ops: OpsState,
    pub git: GitState,
}

impl Focusable for FmApp {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FmApp {
    pub fn new(start: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        cx.observe_window_activation(window, |_, _, cx| cx.notify()).detach();
        let mut this = FmApp {
            focus,
            tabs: vec![Tab::new(0, start)],
            active_tab: 0,
            next_tab_id: 1,
            view_mode: ViewMode::List,
            sort: SortSpec::default(),
            show_hidden: false,
            show_preview: true,
            sidebar: SidebarState::new(window, cx),
            toolbar: ToolbarState::new(window, cx),
            files: file_view::FileViewState::new(window, cx),
            preview: PreviewState::new(window, cx),
            quicklook: QuickLookState::new(window, cx),
            overlays: OverlaysState::new(window, cx),
            ops: OpsState::new(window, cx),
            git: GitState::default(),
        };
        this.load_active(cx);
        this
    }

    // ── accessors ────────────────────────────────────────────────────────────

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    /// Current directory of the active tab.
    pub fn cwd(&self) -> &Path {
        &self.tab().path
    }

    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.tab().cursor_entry()
    }

    /// Give keyboard focus back to the file list (call after closing any text field/overlay).
    pub fn focus_list(&self, window: &mut Window) {
        self.focus.focus(window);
    }

    // ── navigation ───────────────────────────────────────────────────────────

    /// Go to `path` in the active tab, pushing history.
    pub fn navigate(&mut self, path: impl Into<PathBuf>, cx: &mut Context<Self>) {
        let path = path.into();
        let tab = self.tab_mut();
        if tab.path == path {
            return self.reload(cx);
        }
        tab.history.truncate(tab.history_ix + 1);
        tab.history.push(path.clone());
        tab.history_ix = tab.history.len() - 1;
        self.enter(path, None, cx);
    }

    fn enter(&mut self, path: PathBuf, cursor_on: Option<PathBuf>, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.path = path;
        tab.entries.clear();
        tab.visible.clear();
        tab.cursor = 0;
        tab.selected.clear();
        tab.anchor = None;
        tab.filter.clear();
        tab.pending_cursor = cursor_on;
        tab.pending_rename = false;
        toolbar::on_directory_changed(self, cx);
        self.load_active(cx);
    }

    pub fn can_go_back(&self) -> bool {
        self.tab().history_ix > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.tab().history_ix + 1 < self.tab().history.len()
    }

    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        if self.can_go_back() {
            let from = self.tab().path.clone();
            let tab = self.tab_mut();
            tab.history_ix -= 1;
            let path = tab.history[tab.history_ix].clone();
            self.enter(path, Some(from), cx);
        }
    }

    pub fn go_forward(&mut self, cx: &mut Context<Self>) {
        if self.can_go_forward() {
            let tab = self.tab_mut();
            tab.history_ix += 1;
            let path = tab.history[tab.history_ix].clone();
            self.enter(path, None, cx);
        }
    }

    /// Parent folder, leaving the cursor on the folder we came from.
    pub fn go_parent(&mut self, cx: &mut Context<Self>) {
        let from = self.tab().path.clone();
        if let Some(parent) = from.parent().map(Path::to_path_buf) {
            let tab = self.tab_mut();
            tab.history.truncate(tab.history_ix + 1);
            tab.history.push(parent.clone());
            tab.history_ix = tab.history.len() - 1;
            self.enter(parent, Some(from), cx);
        }
    }

    /// Re-read the current directory, keeping cursor and selection.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_active(cx);
    }

    /// Re-read, then put the cursor on `path` (and optionally start renaming it).
    pub fn reload_and_focus(&mut self, path: PathBuf, rename: bool, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.pending_cursor = Some(path);
        tab.pending_rename = rename;
        self.load_active(cx);
    }

    fn load_active(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.generation += 1;
        let (tab_id, generation, path) = (tab.id, tab.generation, tab.path.clone());
        if tab.entries.is_empty() {
            tab.load = LoadState::Loading;
        }
        let task = cx.background_spawn({
            let path = path.clone();
            async move { fs_model::read_dir(&path) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let (sort, show_hidden) = (this.sort, this.show_hidden);
                let Some(tab) = this.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
                if tab.generation != generation {
                    return;
                }
                match result {
                    Ok(entries) => {
                        tab.entries = entries;
                        tab.load = LoadState::Ready;
                    }
                    Err(err) => {
                        tab.entries.clear();
                        tab.load = LoadState::Error(err);
                    }
                }
                tab.recompute(sort, show_hidden);
                let rename = tab.pending_cursor.take().filter(|_| std::mem::take(&mut tab.pending_rename));
                let (row, is_active) = (tab.cursor, this.tabs[this.active_tab].id == tab_id);
                if is_active {
                    this.tab().scroll.scroll_to_item(row, ScrollStrategy::Center);
                    git::refresh(this, cx);
                    sidebar::on_directory_loaded(this, cx);
                    this.selection_changed(cx);
                    if let Some(path) = rename {
                        file_view::request_rename(this, path, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Re-apply sort / hidden / filter to the active tab.
    pub fn recompute(&mut self, cx: &mut Context<Self>) {
        let (sort, show_hidden) = (self.sort, self.show_hidden);
        self.tab_mut().recompute(sort, show_hidden);
        self.selection_changed(cx);
        cx.notify();
    }

    /// Live filter (toolbar). Never animated.
    pub fn set_filter(&mut self, filter: impl Into<String>, cx: &mut Context<Self>) {
        self.tab_mut().filter = filter.into();
        self.recompute(cx);
    }

    pub fn set_sort(&mut self, sort: SortSpec, cx: &mut Context<Self>) {
        self.sort = sort;
        let show_hidden = self.show_hidden;
        for tab in &mut self.tabs {
            tab.recompute(sort, show_hidden);
        }
        self.selection_changed(cx);
        cx.notify();
    }

    /// Click on a column header: sort by it, or flip direction if already sorted by it.
    pub fn sort_by(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let mut sort = self.sort;
        if sort.key == key {
            sort.ascending = !sort.ascending;
        } else {
            sort.key = key;
            sort.ascending = true;
        }
        self.set_sort(sort, cx);
    }

    pub fn set_show_hidden(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_hidden = show;
        self.set_sort(self.sort, cx);
    }

    pub fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        self.view_mode = mode;
        cx.notify();
    }

    // ── cursor & selection ───────────────────────────────────────────────────

    /// Move the cursor to a row of the visible list and scroll it into view.
    pub fn set_cursor(&mut self, row: usize, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        if tab.visible.is_empty() {
            return;
        }
        let row = row.min(tab.visible.len() - 1);
        tab.cursor = row;
        tab.scroll.scroll_to_item(row, ScrollStrategy::Top);
        self.selection_changed(cx);
        cx.notify();
    }

    pub fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let cur = self.tab().cursor as isize;
        self.tab_mut().anchor = None;
        self.set_cursor((cur + delta).max(0) as usize, cx);
    }

    /// shift+↑↓: grow/shrink the selection from the anchor row.
    pub fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        if tab.visible.is_empty() {
            return;
        }
        let anchor = *tab.anchor.get_or_insert(tab.cursor);
        let row = (tab.cursor as isize + delta).clamp(0, tab.visible.len() as isize - 1) as usize;
        let (lo, hi) = (anchor.min(row), anchor.max(row));
        tab.selected = (lo..=hi).filter_map(|r| tab.entry_at(r).map(|e| e.path.clone())).collect();
        tab.cursor = row;
        tab.scroll.scroll_to_item(row, ScrollStrategy::Top);
        self.selection_changed(cx);
        cx.notify();
    }

    /// Select rows `a..=b` (shift-click).
    pub fn select_range(&mut self, a: usize, b: usize, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        let (lo, hi) = (a.min(b), a.max(b));
        tab.selected = (lo..=hi).filter_map(|r| tab.entry_at(r).map(|e| e.path.clone())).collect();
        cx.notify();
    }

    /// ctrl-click / insert: toggle one row in the multi-selection.
    pub fn toggle_selected(&mut self, row: usize, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        if let Some(path) = tab.entry_at(row).map(|e| e.path.clone())
            && !tab.selected.remove(&path)
        {
            tab.selected.insert(path);
        }
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.selected = tab.visible_entries().map(|e| e.path.clone()).collect();
        cx.notify();
    }

    pub fn invert_selection(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.selected =
            tab.visible_entries().map(|e| e.path.clone()).filter(|p| !tab.selected.contains(p)).collect();
        cx.notify();
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        tab.selected.clear();
        tab.anchor = None;
        cx.notify();
    }

    /// Called whenever the cursor entry may have changed. Fans out to preview / Quick Look.
    pub fn selection_changed(&mut self, cx: &mut Context<Self>) {
        preview::on_cursor_changed(self, cx);
        quicklook::on_cursor_changed(self, cx);
    }

    // ── opening ──────────────────────────────────────────────────────────────

    /// `↵`: enter a folder or open a file with the default application.
    pub fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.cursor_entry().cloned() else { return };
        self.open_entry(&entry, cx);
    }

    pub fn open_entry(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if entry.is_dir {
            self.navigate(entry.path.clone(), cx);
        } else {
            ops::open_external(&entry.path, cx);
        }
    }

    pub fn copy_path_to_clipboard(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<String> = self.tab().targets().iter().map(|p| p.display().to_string()).collect();
        if !paths.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(paths.join("\n")));
        }
    }

    // ── tabs ─────────────────────────────────────────────────────────────────

    pub fn new_tab(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        let path = path.unwrap_or_else(|| self.cwd().to_path_buf());
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.insert(self.active_tab + 1, Tab::new(id, path));
        self.active_tab += 1;
        toolbar::on_directory_changed(self, cx);
        self.load_active(cx);
    }

    pub fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return cx.quit();
        }
        self.tabs.remove(ix);
        if self.active_tab > ix || self.active_tab >= self.tabs.len() {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
        self.activate_tab(self.active_tab, cx);
    }

    pub fn activate_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.active_tab = ix;
            toolbar::on_directory_changed(self, cx);
            self.load_active(cx);
        }
    }

    pub fn set_theme(&mut self, name: theme::ThemeName, cx: &mut Context<Self>) {
        cx.set_global(Theme::resolved(name, cx));
        cx.refresh_windows();
    }

    // ── action handlers owned by the shell ───────────────────────────────────

    fn step(&self) -> isize {
        match self.view_mode {
            ViewMode::List => 1,
            ViewMode::Grid => self.files.grid_columns.max(1) as isize,
        }
    }

    fn act_cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Innermost thing first.
        if overlays::cancel(self, window, cx)
            || quicklook::cancel(self, window, cx)
            || toolbar::cancel(self, window, cx)
            || file_view::cancel(self, window, cx)
        {
            return;
        }
        if !self.tab().selected.is_empty() {
            self.clear_selection(cx);
        } else if !self.tab().filter.is_empty() {
            self.set_filter("", cx);
            toolbar::on_directory_changed(self, cx);
        }
        self.focus_list(window);
    }

    fn act_move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == ViewMode::Grid || self.quicklook.is_open() {
            self.move_cursor(-1, cx)
        } else {
            self.go_parent(cx)
        }
    }

    fn act_move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == ViewMode::Grid || self.quicklook.is_open() {
            self.move_cursor(1, cx)
        } else if self.cursor_entry().is_some_and(|e| e.is_dir) {
            self.open_cursor(cx)
        }
    }

    fn act_go_dots(&mut self, _: &GoDots, _: &mut Window, cx: &mut Context<Self>) {
        let home = fs_model::home_dir();
        let dots = [home.join("src/dots"), home.join(".dotfiles"), home.join(".config")]
            .into_iter()
            .find(|p| p.is_dir())
            .unwrap_or(home);
        self.navigate(dots, cx);
    }
}

impl Render for FmApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let width = window.viewport_size().width;
        let show_preview = self.show_preview && width >= px(theme::BREAK_PREVIEW);

        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("Fm");
        if self.quicklook.is_open() {
            key_context.add("QuickLook");
        }
        window.set_window_title(&format!("fm — {}", fs_model::display_path(self.cwd())));

        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(sidebar::render(self, window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(toolbar::render(self, window, cx))
                    .child(file_view::render(self, window, cx)),
            )
            .when(show_preview, |d| d.child(preview::render(self, window, cx)));

        div()
            .id("fm-root")
            .key_context(key_context)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(t.surface_2)
            .border_1()
            .border_color(if window.is_window_active() { t.accent } else { t.frame })
            .rounded(t.radius_window)
            .text_color(t.fg)
            .font_family(t.font_sans.clone())
            .text_size(px(13.))
            .line_height(px(20.))
            // shell
            .on_action(cx.listener(|this, _: &Open, _, cx| {
                if !quicklook::open_primary(this, cx) {
                    this.open_cursor(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_cursor(-this.step(), cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_cursor(this.step(), cx)))
            .on_action(cx.listener(Self::act_move_left))
            .on_action(cx.listener(Self::act_move_right))
            .on_action(cx.listener(|this, _: &MoveFirst, _, cx| this.set_cursor(0, cx)))
            .on_action(cx.listener(|this, _: &MoveLast, _, cx| this.set_cursor(usize::MAX, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_cursor(-20 * this.step(), cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_cursor(20 * this.step(), cx)))
            .on_action(cx.listener(|this, _: &Parent, _, cx| this.go_parent(cx)))
            .on_action(cx.listener(|this, _: &ExtendUp, _, cx| this.extend_selection(-this.step(), cx)))
            .on_action(cx.listener(|this, _: &ExtendDown, _, cx| this.extend_selection(this.step(), cx)))
            .on_action(cx.listener(|this, _: &ToggleSelect, _, cx| {
                this.toggle_selected(this.tab().cursor, cx);
                this.move_cursor(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &InvertSelection, _, cx| this.invert_selection(cx)))
            .on_action(cx.listener(|this, _: &CopyPath, _, cx| this.copy_path_to_clipboard(cx)))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.new_tab(None, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.close_tab(this.active_tab, cx)))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| {
                this.activate_tab((this.active_tab + 1) % this.tabs.len(), cx)
            }))
            .on_action(cx.listener(|this, _: &PrevTab, _, cx| {
                this.activate_tab((this.active_tab + this.tabs.len() - 1) % this.tabs.len(), cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleGrid, _, cx| {
                let mode = if this.view_mode == ViewMode::List { ViewMode::Grid } else { ViewMode::List };
                this.set_view_mode(mode, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleHidden, _, cx| this.set_show_hidden(!this.show_hidden, cx)))
            .on_action(cx.listener(|this, _: &TogglePreview, _, cx| {
                this.show_preview = !this.show_preview;
                cx.notify()
            }))
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.go_back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &GoHome, _, cx| this.navigate(fs_model::home_dir(), cx)))
            .on_action(cx.listener(Self::act_go_dots))
            .on_action(cx.listener(|this, _: &GoTrash, _, cx| this.navigate(fs_model::trash_dir(), cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &NextTheme, _, cx| {
                let next = theme(cx).name.next();
                this.set_theme(next, cx)
            }))
            .on_action(cx.listener(Self::act_cancel))
            // toolbar.rs
            .on_action(cx.listener(toolbar::act_filter))
            .on_action(cx.listener(toolbar::act_recursive_search))
            .on_action(cx.listener(toolbar::act_edit_path))
            .on_action(cx.listener(toolbar::act_sort_menu))
            // file_view.rs
            .on_action(cx.listener(file_view::act_rename))
            // quicklook.rs
            .on_action(cx.listener(quicklook::act_toggle))
            .on_action(cx.listener(quicklook::act_fullscreen))
            // ops.rs / overlays.rs
            .on_action(cx.listener(ops::act_copy))
            .on_action(cx.listener(ops::act_cut))
            .on_action(cx.listener(ops::act_paste))
            .on_action(cx.listener(ops::act_trash))
            .on_action(cx.listener(ops::act_delete_permanently))
            .on_action(cx.listener(ops::act_new_file))
            .on_action(cx.listener(ops::act_new_folder))
            .on_action(cx.listener(overlays::act_key_hints))
            // layout
            .child(chrome::titlebar(self, window, cx))
            .child(chrome::tabs(self, window, cx))
            .child(body)
            .child(chrome::footer(self, window, cx))
            .child(chrome::hint_bar(self, window, cx))
            // layers above the window content, bottom to top
            .children(quicklook::render(self, window, cx))
            .children(overlays::render(self, window, cx))
    }
}

/// Convenience for modules: `SharedString` of the cwd for display (`~/src/dots`).
pub fn cwd_label(app: &FmApp) -> SharedString {
    fs_model::display_path(app.cwd()).into()
}
