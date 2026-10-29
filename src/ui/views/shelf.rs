//! Stash list, shelf, patch files.
//!
//! Layout: Stashes and Shelf lists on the left, the selected entry's files and the selected
//! file's diff on the right. Tab cycles the panes; `.`/`m` lists every action for the selection.

mod actions;
mod form;
mod preview;

use std::collections::BTreeSet;
use std::time::Duration;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::diff::{DiffOptions, FileDiff};
use crate::git::shelf::{self, ShelfEntry};
use crate::git::stash::{self, StashEntry};
use crate::git::Repo;
use crate::ui::bg::{Background, Preview};
use crate::ui::diff_view::{self, diff_height, draw_diff, DiffSource};
use crate::ui::popups::{MenuItem, MenuPopup};
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, PaletteItem, View};
use crate::util::{fit, relative_time, truncate};

use preview::file_line;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Stashes,
    Shelf,
    Files,
    Diff,
}

impl Focus {
    const ORDER: [Focus; 4] = [Focus::Stashes, Focus::Shelf, Focus::Files, Focus::Diff];

    fn cycle(self, forward: bool) -> Focus {
        let i = Focus::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let n = Focus::ORDER.len();
        Focus::ORDER[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// Which list drives the preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Stash,
    Shelf,
}

/// Selection, scroll offset and filter of one list pane.
#[derive(Debug, Default, Clone)]
struct ListPane {
    sel: usize,
    offset: usize,
    filter: String,
    /// Rows visible at the last draw (for paging and mouse hits)
    height: usize,
}

impl ListPane {
    fn matches(&self, hay: &str) -> bool {
        self.filter.is_empty() || hay.to_lowercase().contains(&self.filter.to_lowercase())
    }
}

/// Inner areas of the panes at the last draw (mouse support).
#[derive(Debug, Default, Clone, Copy)]
struct Areas {
    stashes: Rect,
    shelf: Rect,
    files: Rect,
    diff: Rect,
}

/// How long the selection must rest on an entry before its files load.
const PREVIEW_DELAY: Duration = Duration::from_millis(60);

type PreviewData = Result<Vec<FileDiff>, String>;
type PreviewJob = Box<dyn FnOnce(&Repo) -> PreviewData + Send>;

/// What a refresh loads off the UI thread (each list fails on its own).
struct Loaded {
    stashes: Result<Vec<StashEntry>, String>,
    shelves: Result<Vec<ShelfEntry>, String>,
}

fn load_lists(repo: &Repo) -> Loaded {
    Loaded {
        stashes: stash::list(repo).map_err(|e| format!("{e:#}")),
        shelves: shelf::list(repo).map_err(|e| format!("{e:#}")),
    }
}

pub struct ShelfView {
    stashes: Vec<StashEntry>,
    shelves: Vec<ShelfEntry>,
    stash_pane: ListPane,
    shelf_pane: ListPane,
    file_pane: ListPane,
    focus: Focus,
    source: Source,
    /// Diffs of the selected entry
    files: Vec<FileDiff>,
    preview_key: Option<(String, String)>,
    /// Set by refresh: reload the preview even if the selection is unchanged
    preview_stale: bool,
    preview_error: Option<String>,
    previews: Preview<(String, String), PreviewData>,
    /// Set while `preview_key`'s files are loading: (same entry as before, file to keep selected)
    preview_pending: Option<(bool, Option<String>)>,
    loader: Background<Loaded>,
    load_error: Option<String>,
    /// Files marked (Space) for a partial unshelve
    marked: BTreeSet<String>,
    diff_scroll: usize,
    diff_page: usize,
    /// Filter being edited for the focused list
    filter_input: Option<TextInput>,
    areas: Areas,
}

impl ShelfView {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            stashes: Vec::new(),
            shelves: Vec::new(),
            stash_pane: ListPane::default(),
            shelf_pane: ListPane::default(),
            file_pane: ListPane::default(),
            focus: Focus::Stashes,
            source: Source::Stash,
            files: Vec::new(),
            preview_key: None,
            preview_stale: false,
            preview_error: None,
            previews: Preview::new(PREVIEW_DELAY, 16),
            preview_pending: None,
            loader: Background::new(),
            load_error: None,
            marked: BTreeSet::new(),
            diff_scroll: 0,
            diff_page: 10,
            filter_input: None,
            areas: Areas::default(),
        }
    }

    // ----- selection -----------------------------------------------------------------------

    fn stash_visible(&self) -> Vec<usize> {
        self.stashes
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let hay = format!(
                    "{} {} {}",
                    s.refname(),
                    s.message,
                    s.branch.as_deref().unwrap_or("")
                );
                self.stash_pane.matches(&hay)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn shelf_visible(&self) -> Vec<usize> {
        self.shelves
            .iter()
            .enumerate()
            .filter(|(_, e)| self.shelf_pane.matches(&e.meta.name))
            .map(|(i, _)| i)
            .collect()
    }

    fn file_visible(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, d)| self.file_pane.matches(&preview::file_label(d)))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_stash(&self) -> Option<&StashEntry> {
        self.stash_visible()
            .get(self.stash_pane.sel)
            .map(|&i| &self.stashes[i])
    }

    fn selected_shelf(&self) -> Option<&ShelfEntry> {
        self.shelf_visible()
            .get(self.shelf_pane.sel)
            .map(|&i| &self.shelves[i])
    }

    fn selected_file(&self) -> Option<&FileDiff> {
        self.file_visible()
            .get(self.file_pane.sel)
            .map(|&i| &self.files[i])
    }

    fn pane_mut(&mut self, f: Focus) -> Option<&mut ListPane> {
        match f {
            Focus::Stashes => Some(&mut self.stash_pane),
            Focus::Shelf => Some(&mut self.shelf_pane),
            Focus::Files => Some(&mut self.file_pane),
            Focus::Diff => None,
        }
    }

    fn visible_len(&self, f: Focus) -> usize {
        match f {
            Focus::Stashes => self.stash_visible().len(),
            Focus::Shelf => self.shelf_visible().len(),
            Focus::Files => self.file_visible().len(),
            Focus::Diff => 0,
        }
    }

    fn set_focus(&mut self, f: Focus) {
        self.focus = f;
        match f {
            Focus::Stashes => self.source = Source::Stash,
            Focus::Shelf => self.source = Source::Shelf,
            _ => {}
        }
    }

    // ----- preview -------------------------------------------------------------------------

    /// (entry identity, version) of the selected entry; the version changes when a shelf entry's
    /// unshelved files change so the file list is reloaded.
    fn current_key(&self) -> Option<(String, String)> {
        match self.source {
            Source::Stash => self
                .selected_stash()
                .map(|s| (format!("stash:{}", s.hash), String::new())),
            Source::Shelf => self.selected_shelf().map(|e| {
                (
                    format!("shelf:{}", e.id),
                    e.meta.unshelved.len().to_string(),
                )
            }),
        }
    }

    /// Reload the file list when the selected entry changed (or after a refresh).
    /// Request the selected entry's files when the selection changed (or after a refresh), and
    /// show them once loaded.
    fn sync_preview(&mut self, ctx: &Ctx) {
        let key = self.current_key();
        if key != self.preview_key || self.preview_stale {
            self.preview_stale = false;
            let same_entry = matches!((&key, &self.preview_key), (Some(a), Some(b)) if a.0 == b.0);
            let keep_path = same_entry
                .then(|| self.selected_file().map(|d| d.path().to_string()))
                .flatten();
            self.preview_key = key.clone();
            self.preview_pending = Some((same_entry, keep_path));
            if !same_entry {
                // Don't show the previous entry's files under the new selection.
                self.files.clear();
                self.preview_error = None;
            }
            let opts = DiffOptions::from_config(&ctx.config.diff);
            let job: Option<PreviewJob> = match self.source {
                Source::Stash => self.selected_stash().cloned().map(|s| -> PreviewJob {
                    Box::new(move |repo| {
                        stash::diffs(repo, &s, &opts).map_err(|e| format!("{e:#}"))
                    })
                }),
                Source::Shelf => self.selected_shelf().cloned().map(|e| -> PreviewJob {
                    Box::new(move |_| e.diffs().map_err(|e| format!("{e:#}")))
                }),
            };
            if let (Some(k), Some(job)) = (key, job) {
                self.previews.request(&ctx.repo, k, job);
            }
        }
        if self.preview_pending.is_none() {
            return;
        }
        let loaded = match &self.preview_key {
            Some(k) => match self.previews.get(k) {
                Some(r) => Some(r),
                None => return,
            },
            None => None,
        };
        let Some((same_entry, keep_path)) = self.preview_pending.take() else {
            return;
        };
        self.preview_error = None;
        self.files = match loaded {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                self.preview_error = Some(e);
                Vec::new()
            }
            None => Vec::new(),
        };
        if !same_entry {
            self.marked.clear();
            self.file_pane.filter.clear();
        }
        self.file_pane.sel = keep_path
            .and_then(|p| {
                self.file_visible()
                    .iter()
                    .position(|&i| self.files[i].path() == p)
            })
            .unwrap_or(0);
        self.file_pane.offset = 0;
        self.diff_scroll = 0;
    }

    /// True while a newly selected entry's files are loading (a reload of the same entry keeps
    /// showing the old files).
    fn preview_loading(&self) -> bool {
        matches!(self.preview_pending, Some((false, _)))
    }

    /// Apply a finished refresh.
    fn apply_loaded(&mut self, ctx: &Ctx) {
        let Some(loaded) = self.loader.poll() else {
            return;
        };
        let sel_stash = self.selected_stash().map(|s| s.hash.clone());
        let sel_shelf = self.selected_shelf().map(|e| e.id.clone());
        self.load_error = loaded
            .stashes
            .as_ref()
            .err()
            .or(loaded.shelves.as_ref().err())
            .cloned();
        self.stashes = loaded.stashes.unwrap_or_default();
        self.shelves = loaded.shelves.unwrap_or_default();
        if let Some(h) = sel_stash {
            if let Some(p) = self
                .stash_visible()
                .iter()
                .position(|&i| self.stashes[i].hash == h)
            {
                self.stash_pane.sel = p;
            }
        }
        if let Some(id) = sel_shelf {
            if let Some(p) = self
                .shelf_visible()
                .iter()
                .position(|&i| self.shelves[i].id == id)
            {
                self.shelf_pane.sel = p;
            }
        }
        self.stash_pane.sel = self
            .stash_pane
            .sel
            .min(self.stash_visible().len().saturating_sub(1));
        self.shelf_pane.sel = self
            .shelf_pane
            .sel
            .min(self.shelf_visible().len().saturating_sub(1));
        // Force a reload of the preview (files may have changed on disk), keeping the file selection.
        self.previews.invalidate();
        self.preview_stale = true;
        self.sync_preview(ctx);
    }

    // ----- actions -------------------------------------------------------------------------

    fn open_entry_diff(&self) -> Action {
        match self.source {
            Source::Stash => match self.selected_stash() {
                Some(s) => diff_view::open(
                    format!("{}: {}", s.refname(), s.message),
                    DiffSource::Stash { index: s.index },
                ),
                None => Action::None,
            },
            Source::Shelf => match self.selected_shelf() {
                Some(e) => match e.diffs() {
                    Ok(d) => diff_view::open(format!("Shelf: {}", e.meta.name), DiffSource::Raw(d)),
                    Err(err) => Action::error(format!("{err:#}")),
                },
                None => Action::None,
            },
        }
    }

    fn open_file_diff(&self) -> Action {
        match self.selected_file() {
            Some(d) => diff_view::open(preview::file_label(d), DiffSource::Raw(vec![d.clone()])),
            None => self.open_entry_diff(),
        }
    }

    /// Files to unshelve from the Files pane: the marked ones, else the selected one.
    fn files_to_unshelve(&self) -> Vec<String> {
        if !self.marked.is_empty() {
            return self.marked.iter().cloned().collect();
        }
        self.selected_file()
            .map(|d| vec![d.path().to_string()])
            .unwrap_or_default()
    }

    fn unshelve_from_files(&self, ctx: &Ctx) -> Action {
        match self.selected_shelf() {
            Some(e) if self.source == Source::Shelf => {
                let paths = self.files_to_unshelve();
                if paths.is_empty() {
                    Action::None
                } else {
                    actions::unshelve_files(ctx, &e.id, paths)
                }
            }
            _ => Action::None,
        }
    }

    fn toggle_mark(&mut self) {
        if self.source != Source::Shelf {
            return;
        }
        if let Some(p) = self.selected_file().map(|d| d.path().to_string()) {
            if !self.marked.remove(&p) {
                self.marked.insert(p);
            }
            let n = self.file_visible().len();
            self.file_pane.sel = move_sel(self.file_pane.sel, 1, n);
        }
    }

    fn stash_menu_items(&self, items: &mut Vec<MenuItem>) {
        let Some(s) = self.selected_stash().cloned() else {
            return;
        };
        let (s1, s2, s3, s4) = (s.clone(), s.clone(), s.clone(), s.clone());
        let index = s.index;
        let title = format!("{}: {}", s.refname(), s.message);
        items.push(MenuItem::new("Show diff", "Enter", move |_| {
            diff_view::open(title, DiffSource::Stash { index })
        }));
        items.push(MenuItem::new("Apply stash…", "a", move |_| {
            actions::apply_stash_dialog(&s1, false)
        }));
        items.push(MenuItem::new("Pop stash…", "p", move |_| {
            actions::apply_stash_dialog(&s2, true)
        }));
        items.push(MenuItem::new("New branch from stash…", "b", move |_| {
            actions::branch_from_stash(&s3)
        }));
        items.push(MenuItem::new("Drop stash", "d", move |_| {
            actions::drop_stash(&s4)
        }));
        items.push(MenuItem::new(
            "Clear all stashes",
            "C",
            actions::clear_stashes,
        ));
    }

    fn shelf_menu_items(&self, items: &mut Vec<MenuItem>) {
        let Some(e) = self.selected_shelf().cloned() else {
            return;
        };
        if let Ok(d) = e.diffs() {
            let title = format!("Shelf: {}", e.meta.name);
            items.push(MenuItem::new("Show diff", "Enter", move |_| {
                diff_view::open(title, DiffSource::Raw(d))
            }));
        }
        let (e1, e2, e3, e4) = (e.clone(), e.clone(), e.clone(), e.clone());
        items.push(MenuItem::new("Unshelve…", "u", move |_| {
            actions::unshelve_dialog(&e1)
        }));
        if matches!(self.focus, Focus::Files | Focus::Diff) {
            let paths = self.files_to_unshelve();
            if !paths.is_empty() {
                let id = e.id.clone();
                let label = if self.marked.is_empty() {
                    "Unshelve selected file".to_string()
                } else {
                    format!("Unshelve {} marked files", self.marked.len())
                };
                items.push(MenuItem::new(label, "f", move |ctx| {
                    actions::unshelve_files(ctx, &id, paths)
                }));
            }
        }
        items.push(MenuItem::new("Rename…", "R", move |_| {
            actions::rename_shelf(&e2)
        }));
        items.push(MenuItem::new("Export as patch…", "x", move |ctx| {
            actions::export_shelf(ctx, &e3)
        }));
        items.push(MenuItem::new("Delete", "d", move |_| {
            actions::delete_shelf(&e4)
        }));
        items.push(MenuItem::new(
            "Clean up unshelved entries",
            "D",
            actions::delete_applied,
        ));
    }

    fn context_menu(&self, ctx: &Ctx) -> Action {
        let mut items = Vec::new();
        match self.source {
            Source::Stash => self.stash_menu_items(&mut items),
            Source::Shelf => self.shelf_menu_items(&mut items),
        }
        let has_head = ctx.repo.head_commit().is_some();
        items.push(MenuItem::new("Stash changes…", "s", actions::stash_dialog).enabled(has_head));
        items.push(MenuItem::new(
            "Shelve all changes…",
            "S",
            actions::shelve_all,
        ));
        items.push(MenuItem::new(
            "Create patch from local changes…",
            "c",
            actions::create_local_patch,
        ));
        items.push(
            MenuItem::new(
                "Create patch from commit…",
                "F",
                actions::create_commit_patch_prompt,
            )
            .enabled(has_head),
        );
        items.push(MenuItem::new(
            "Apply patch…",
            "P",
            actions::apply_patch_prompt,
        ));
        let title = match self.source {
            Source::Stash => self
                .selected_stash()
                .map(|s| s.refname())
                .unwrap_or_else(|| "Stash".into()),
            Source::Shelf => self
                .selected_shelf()
                .map(|e| truncate(&e.meta.name, 40))
                .unwrap_or_else(|| "Shelf".into()),
        };
        Action::popup(MenuPopup::new(title, items))
    }

    /// Keys acting on the selected stash.
    fn stash_key(&self, c: char, ctx: &Ctx) -> Action {
        let Some(s) = self.selected_stash() else {
            return Action::None;
        };
        match c {
            'a' => actions::apply_stash_dialog(s, false),
            'p' => actions::apply_stash_dialog(s, true),
            'd' => actions::drop_stash(s),
            'b' => actions::branch_from_stash(s),
            'C' => actions::clear_stashes(ctx),
            _ => Action::None,
        }
    }

    /// Keys acting on the selected shelf entry.
    fn shelf_key(&self, c: char, ctx: &Ctx) -> Action {
        if c == 'D' {
            return actions::delete_applied(ctx);
        }
        let Some(e) = self.selected_shelf() else {
            return Action::None;
        };
        match c {
            'u' => actions::unshelve_dialog(e),
            'd' => actions::delete_shelf(e),
            'R' => actions::rename_shelf(e),
            'x' => actions::export_shelf(ctx, e),
            _ => Action::None,
        }
    }

    fn move_focused(&mut self, delta: isize) {
        if self.focus == Focus::Diff {
            self.scroll_diff(delta);
            return;
        }
        let n = self.visible_len(self.focus);
        if let Some(p) = self.pane_mut(self.focus) {
            p.sel = move_sel(p.sel, delta, n);
        }
        if self.focus == Focus::Files {
            self.diff_scroll = 0;
        }
    }

    fn scroll_diff(&mut self, delta: isize) {
        let max = self
            .selected_file()
            .map(|d| diff_height(std::slice::from_ref(d)))
            .unwrap_or(0);
        self.diff_scroll =
            (self.diff_scroll as isize + delta).clamp(0, max.saturating_sub(1) as isize) as usize;
    }

    fn page(&self) -> isize {
        let h = match self.focus {
            Focus::Stashes => self.stash_pane.height,
            Focus::Shelf => self.shelf_pane.height,
            Focus::Files => self.file_pane.height,
            Focus::Diff => self.diff_page,
        };
        h.max(1) as isize
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        let Some(input) = self.filter_input.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.filter_input = None;
                if let Some(p) = self.pane_mut(self.focus) {
                    p.filter.clear();
                    p.sel = 0;
                }
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                self.filter_input = None;
            }
            _ => {
                input.handle_key(&key);
                let v = input.value();
                if let Some(p) = self.pane_mut(self.focus) {
                    p.filter = v;
                    p.sel = 0;
                    p.offset = 0;
                }
            }
        }
        Action::None
    }

    fn global_key(&self, c: char, ctx: &Ctx) -> Option<Action> {
        Some(match c {
            's' => actions::stash_dialog(ctx),
            'S' => actions::shelve_all(ctx),
            'c' => actions::create_local_patch(ctx),
            'F' => actions::create_commit_patch_prompt(ctx),
            'P' => actions::apply_patch_prompt(ctx),
            '.' | 'm' => self.context_menu(ctx),
            _ => return None,
        })
    }

    // ----- drawing -------------------------------------------------------------------------

    fn pane_title(&self, name: &str, count: usize, pane: &ListPane, focus: Focus) -> Line<'static> {
        let mut s = format!(" {name} {count} ");
        let editing = self.filter_input.is_some() && self.focus == focus;
        if editing || !pane.filter.is_empty() {
            s.push_str(&format!("/{} ", pane.filter));
        }
        Line::raw(s)
    }

    fn draw_stashes(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.stash_visible();
        let focused = self.focus == Focus::Stashes;
        let block = panel(
            self.pane_title(
                "Stashes",
                self.stashes.len(),
                &self.stash_pane,
                Focus::Stashes,
            ),
            focused,
            t,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.areas.stashes = inner;
        self.stash_pane.height = inner.height as usize;
        if vis.is_empty() {
            let msg = if self.stashes.is_empty() {
                "No stashes. Press s to stash local changes."
            } else {
                "No stash matches the filter."
            };
            f.render_widget(
                Paragraph::new(msg)
                    .style(t.dimmed())
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        self.stash_pane.sel = self.stash_pane.sel.min(vis.len() - 1);
        self.stash_pane.offset = scroll_to_keep_visible(
            self.stash_pane.sel,
            self.stash_pane.offset,
            inner.height as usize,
        );
        let w = inner.width as usize;
        let highlight = self.source == Source::Stash;
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.stash_pane.offset)
            .take(inner.height as usize)
            .map(|(row, &i)| {
                let s = &self.stashes[i];
                let r = format!("{{{}}} ", s.index);
                let branch = s.branch.as_deref().unwrap_or("detached");
                let time = relative_time(s.time);
                let right = format!(" {branch} · {time}");
                let msg_w = w.saturating_sub(r.chars().count() + right.chars().count());
                let msg = if s.wip {
                    format!("WIP {}", s.message)
                } else {
                    s.message.clone()
                };
                let line = Line::from(vec![
                    Span::styled(r, Style::default().fg(t.hash)),
                    Span::styled(fit(&msg, msg_w), t.text()),
                    Span::styled(format!(" {branch}"), Style::default().fg(t.local_branch)),
                    Span::styled(format!(" · {time}"), Style::default().fg(t.date)),
                ]);
                if row == self.stash_pane.sel && highlight {
                    line.style(if focused {
                        t.selected()
                    } else {
                        t.selected().remove_modifier(Modifier::BOLD)
                    })
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_shelf(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.shelf_visible();
        let focused = self.focus == Focus::Shelf;
        let block = panel(
            self.pane_title("Shelf", self.shelves.len(), &self.shelf_pane, Focus::Shelf),
            focused,
            t,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.areas.shelf = inner;
        self.shelf_pane.height = inner.height as usize;
        if vis.is_empty() {
            let msg = if self.shelves.is_empty() {
                "The shelf is empty. Press S to shelve local changes."
            } else {
                "No shelf entry matches the filter."
            };
            f.render_widget(
                Paragraph::new(msg)
                    .style(t.dimmed())
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        self.shelf_pane.sel = self.shelf_pane.sel.min(vis.len() - 1);
        self.shelf_pane.offset = scroll_to_keep_visible(
            self.shelf_pane.sel,
            self.shelf_pane.offset,
            inner.height as usize,
        );
        let w = inner.width as usize;
        let highlight = self.source == Source::Shelf;
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.shelf_pane.offset)
            .take(inner.height as usize)
            .map(|(row, &i)| {
                let e = &self.shelves[i];
                let n = e.meta.files.len();
                let done = e
                    .meta
                    .unshelved
                    .iter()
                    .filter(|p| e.meta.files.iter().any(|f| &f.path == *p))
                    .count();
                let mut parts = Vec::new();
                if e.is_applied() {
                    parts.push("✓ unshelved".to_string());
                } else if done > 0 {
                    parts.push(format!("{done}/{n} unshelved"));
                }
                parts.push(format!("{n} file{}", if n == 1 { "" } else { "s" }));
                parts.push(relative_time(e.meta.created));
                let right = format!(" {}", parts.join(" · "));
                let name_w = w.saturating_sub(right.chars().count());
                let name_style = if e.is_applied() { t.dimmed() } else { t.text() };
                let line = Line::from(vec![
                    Span::styled(fit(&e.meta.name, name_w), name_style),
                    Span::styled(right, t.dimmed()),
                ]);
                if row == self.shelf_pane.sel && highlight {
                    line.style(if focused {
                        t.selected()
                    } else {
                        t.selected().remove_modifier(Modifier::BOLD)
                    })
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn entry_info(&self) -> Option<String> {
        match self.source {
            Source::Stash => self.selected_stash().map(|s| {
                let untracked = if s.untracked_commit().is_some() {
                    " · includes untracked files"
                } else {
                    ""
                };
                format!(
                    "{} on {} · {}{untracked}",
                    s.refname(),
                    s.branch.as_deref().unwrap_or("detached HEAD"),
                    crate::util::format_time(s.time)
                )
            }),
            Source::Shelf => self.selected_shelf().map(|e| {
                let base = e
                    .meta
                    .base
                    .as_deref()
                    .map(|b| format!(" @ {}", &b[..b.len().min(8)]))
                    .unwrap_or_default();
                format!(
                    "on {}{base} · {}{}",
                    e.meta.branch.as_deref().unwrap_or("detached HEAD"),
                    crate::util::format_time(e.meta.created),
                    if self.marked.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} marked", self.marked.len())
                    }
                )
            }),
        }
    }

    fn draw_files(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let focused = self.focus == Focus::Files;
        let block = panel(
            self.pane_title("Files", self.files.len(), &self.file_pane, Focus::Files),
            focused,
            t,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [info, list] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        self.areas.files = list;
        self.file_pane.height = list.height as usize;
        if let Some(i) = self.entry_info() {
            f.render_widget(
                Span::styled(truncate(&i, info.width as usize), t.dimmed()),
                info,
            );
        }
        if let Some(e) = self.load_error.as_ref().or(self.preview_error.as_ref()) {
            f.render_widget(
                Paragraph::new(e.as_str())
                    .style(Style::default().fg(t.error))
                    .wrap(Wrap { trim: true }),
                list,
            );
            return;
        }
        if self.preview_loading() {
            f.render_widget(Paragraph::new("Loading…").style(t.dimmed()), list);
            return;
        }
        let vis = self.file_visible();
        if vis.is_empty() {
            let msg = if self.preview_key.is_none() {
                "Select a stash or shelf entry."
            } else {
                "No files."
            };
            f.render_widget(Span::styled(msg, t.dimmed()), list);
            return;
        }
        self.file_pane.sel = self.file_pane.sel.min(vis.len() - 1);
        self.file_pane.offset = scroll_to_keep_visible(
            self.file_pane.sel,
            self.file_pane.offset,
            list.height as usize,
        );
        let shelf_entry = (self.source == Source::Shelf)
            .then(|| self.selected_shelf())
            .flatten();
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.file_pane.offset)
            .take(list.height as usize)
            .map(|(row, &i)| {
                let d = &self.files[i];
                let mark = shelf_entry.map(|_| self.marked.contains(d.path()));
                let dim = shelf_entry.is_some_and(|e| !e.is_applied() && e.is_unshelved(d.path()));
                let line = file_line(d, list.width as usize, mark, dim, ctx);
                if row == self.file_pane.sel {
                    line.style(if focused {
                        t.selected()
                    } else {
                        t.selected().remove_modifier(Modifier::BOLD)
                    })
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_diff_pane(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let focused = self.focus == Focus::Diff;
        let title = self
            .selected_file()
            .map(|d| format!(" {} ", preview::file_label(d)))
            .unwrap_or_else(|| " Diff ".into());
        let block = panel(
            truncate(&title, area.width.saturating_sub(4) as usize),
            focused,
            t,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.areas.diff = inner;
        self.diff_page = inner.height.max(1) as usize;
        let max = self
            .selected_file()
            .map(|d| diff_height(std::slice::from_ref(d)).saturating_sub(1));
        self.diff_scroll = self.diff_scroll.min(max.unwrap_or(0));
        match self.selected_file() {
            Some(d) => draw_diff(f, inner, std::slice::from_ref(d), self.diff_scroll, ctx),
            None => f.render_widget(Span::styled("No file selected.", t.dimmed()), inner),
        }
    }

    fn click(&mut self, col: u16, row: u16) {
        let pos = Position::new(col, row);
        let hit = |r: Rect| r.contains(pos);
        let (focus, offset, top) = if hit(self.areas.stashes) {
            (Focus::Stashes, self.stash_pane.offset, self.areas.stashes.y)
        } else if hit(self.areas.shelf) {
            (Focus::Shelf, self.shelf_pane.offset, self.areas.shelf.y)
        } else if hit(self.areas.files) {
            (Focus::Files, self.file_pane.offset, self.areas.files.y)
        } else if hit(self.areas.diff) {
            self.set_focus(Focus::Diff);
            return;
        } else {
            return;
        };
        self.set_focus(focus);
        let idx = offset + (row - top) as usize;
        let n = self.visible_len(focus);
        if let Some(p) = self.pane_mut(focus) {
            if idx < n {
                p.sel = idx;
            }
        }
    }
}

impl View for ShelfView {
    fn title(&self) -> String {
        "Stash & Shelf".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        self.loader.start(&ctx.repo, load_lists);
        self.apply_loaded(ctx);
        Ok(())
    }

    fn poll(&mut self, ctx: &Ctx) {
        self.apply_loaded(ctx);
        self.sync_preview(ctx);
    }

    fn loading(&self) -> bool {
        self.loader.busy()
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.sync_preview(ctx);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(area);
        let [top, bottom] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(left);
        self.draw_stashes(f, top, ctx);
        self.draw_shelf(f, bottom, ctx);
        let files_h = files_pane_height(self.files.len(), right.height);
        let [files, diff] =
            Layout::vertical([Constraint::Length(files_h), Constraint::Min(3)]).areas(right);
        self.draw_files(f, files, ctx);
        self.draw_diff_pane(f, diff, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filter_input.is_some() {
            return self.handle_filter_key(key);
        }
        self.sync_preview(ctx);
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return match key.code {
                KeyCode::Char('d') => {
                    self.scroll_diff(self.diff_page as isize / 2);
                    Action::None
                }
                KeyCode::Char('u') => {
                    self.scroll_diff(-(self.diff_page as isize / 2));
                    Action::None
                }
                _ => Action::None,
            };
        }
        match key.code {
            KeyCode::Tab => self.set_focus(self.focus.cycle(true)),
            KeyCode::BackTab => self.set_focus(self.focus.cycle(false)),
            KeyCode::Up | KeyCode::Char('k') => self.move_focused(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_focused(1),
            KeyCode::PageUp => self.move_focused(-self.page()),
            KeyCode::PageDown => self.move_focused(self.page()),
            KeyCode::Home | KeyCode::Char('g') => self.move_focused(-(isize::MAX / 2)),
            KeyCode::End | KeyCode::Char('G') => self.move_focused(isize::MAX / 2),
            KeyCode::Char('J') => self.scroll_diff(1),
            KeyCode::Char('K') => self.scroll_diff(-1),
            KeyCode::Left | KeyCode::Char('h')
                if matches!(self.focus, Focus::Files | Focus::Diff) =>
            {
                self.set_focus(if self.source == Source::Stash {
                    Focus::Stashes
                } else {
                    Focus::Shelf
                });
            }
            KeyCode::Right | KeyCode::Char('l')
                if matches!(self.focus, Focus::Stashes | Focus::Shelf) =>
            {
                self.set_focus(Focus::Files);
            }
            KeyCode::Esc if self.focus != Focus::Stashes && self.focus != Focus::Shelf => {
                self.set_focus(if self.source == Source::Stash {
                    Focus::Stashes
                } else {
                    Focus::Shelf
                });
            }
            KeyCode::Esc => {
                if let Some(p) = self.pane_mut(self.focus) {
                    p.filter.clear();
                }
            }
            KeyCode::Enter => {
                return match self.focus {
                    Focus::Stashes | Focus::Shelf => self.open_entry_diff(),
                    Focus::Files | Focus::Diff => self.open_file_diff(),
                }
            }
            KeyCode::Char('/') if self.focus != Focus::Diff => {
                let current = self
                    .pane_mut(self.focus)
                    .map(|p| p.filter.clone())
                    .unwrap_or_default();
                self.filter_input = Some(TextInput::new(&current));
            }
            KeyCode::Char(' ') if self.focus == Focus::Files => self.toggle_mark(),
            KeyCode::Char('u')
                if matches!(self.focus, Focus::Files | Focus::Diff)
                    && self.source == Source::Shelf =>
            {
                return self.unshelve_from_files(ctx);
            }
            KeyCode::Char(c) => {
                if let Some(a) = self.global_key(c, ctx) {
                    return a;
                }
                return match (self.focus, self.source) {
                    (Focus::Stashes, _) | (Focus::Files | Focus::Diff, Source::Stash) => {
                        self.stash_key(c, ctx)
                    }
                    (Focus::Shelf, _) | (Focus::Files | Focus::Diff, Source::Shelf) => {
                        self.shelf_key(c, ctx)
                    }
                };
            }
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, ev: MouseEvent, _ctx: &Ctx) -> Action {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            self.click(ev.column, ev.row);
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        let mut v: Vec<KeyHelp> = vec![("Tab", "next pane"), ("Enter", "diff")];
        match (self.focus, self.source) {
            (Focus::Stashes, _) => v.extend([
                ("a", "apply…"),
                ("p", "pop…"),
                ("d", "drop"),
                ("b", "branch"),
                ("C", "clear all"),
            ]),
            (Focus::Shelf, _) => v.extend([
                ("u", "unshelve…"),
                ("R", "rename"),
                ("x", "export patch"),
                ("d", "delete"),
                ("D", "clean up unshelved"),
            ]),
            (Focus::Files, Source::Shelf) => {
                v.extend([("Space", "mark file"), ("u", "unshelve marked/selected")])
            }
            (Focus::Files, Source::Stash) => v.extend([("a", "apply stash…"), ("p", "pop stash…")]),
            (Focus::Diff, _) => v.extend([("j/k", "scroll"), ("PgUp/PgDn", "page")]),
        }
        v.extend([
            (".", "all actions"),
            ("s", "stash changes…"),
            ("S", "shelve all…"),
            ("c", "patch from changes…"),
            ("F", "patch from commit…"),
            ("P", "apply patch…"),
            ("/", "filter"),
            ("j/k", "move"),
            ("h/l", "lists/files"),
            ("J/K", "scroll diff"),
            ("Ctrl+D/U", "half-page diff"),
            ("g/G", "first/last"),
            ("m", "all actions"),
            ("Esc", "back / clear filter"),
        ]);
        v
    }

    fn captures_input(&self) -> bool {
        self.filter_input.is_some()
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Stash changes…",
            run: actions::stash_dialog,
        },
        PaletteItem {
            name: "Unstash (pop latest)",
            run: actions::pop_latest,
        },
        PaletteItem {
            name: "Clear all stashes…",
            run: actions::clear_stashes,
        },
        PaletteItem {
            name: "Shelve all changes…",
            run: actions::shelve_all,
        },
        PaletteItem {
            name: "Unshelve…",
            run: actions::choose_unshelve,
        },
        PaletteItem {
            name: "Create patch from local changes…",
            run: actions::create_local_patch,
        },
        PaletteItem {
            name: "Create patch from commit…",
            run: actions::create_commit_patch_prompt,
        },
        PaletteItem {
            name: "Apply patch…",
            run: actions::apply_patch_prompt,
        },
        PaletteItem {
            name: "Continue applying patches (git am --continue)",
            run: actions::am_continue,
        },
        PaletteItem {
            name: "Skip current patch (git am --skip)",
            run: actions::am_skip,
        },
        PaletteItem {
            name: "Abort applying patches (git am --abort)",
            run: actions::am_abort,
        },
    ]
}

/// Shelve the given paths (open the "Shelve changes" dialog prefilled with them). Used by the Changes tab.
pub fn shelve_paths(ctx: &Ctx, paths: Vec<String>) -> Action {
    if paths.is_empty() {
        return actions::shelve_all(ctx);
    }
    actions::shelve_dialog(ctx, paths)
}

/// Stash all local changes (open the "Stash changes" dialog). Used by the Changes tab and branch checkout.
pub fn open_stash_dialog(ctx: &Ctx) -> Action {
    actions::stash_dialog(ctx)
}

/// Height of the Files pane: its rows plus borders and the info line, at least 5 rows and at most
/// two fifths of the right column. Saturating, so huge entries or areas never overflow.
fn files_pane_height(files: usize, right_height: u16) -> u16 {
    let wanted = u16::try_from(files).unwrap_or(u16::MAX).saturating_add(3);
    let max = (u32::from(right_height) * 2 / 5).max(5);
    wanted.clamp(5, u16::try_from(max).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::stash::StashOptions;
    use crate::git::testutil::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn setup() -> (TempDir, Ctx) {
        let (d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n"), ("b.txt", "b\n")], "init");
        write(&repo, "a.txt", "2\n");
        stash::push(
            &repo,
            &StashOptions {
                message: "stashed work".into(),
                include_untracked: true,
                keep_index: false,
            },
            &[],
        )
        .unwrap();
        write(&repo, "b.txt", "B\n");
        write(&repo, "n.txt", "new\n");
        shelf::shelve(
            &repo,
            "shelved work",
            &["b.txt".into(), "n.txt".into()],
            false,
        )
        .unwrap();
        (d, Ctx::new(repo, Config::default()))
    }

    fn render(v: &mut ShelfView, ctx: &Ctx) -> String {
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| v.draw(f, f.area(), ctx)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn renders_lists_and_preview() {
        let (_d, ctx) = setup();
        let mut v = ShelfView::new(&ctx);
        v.refresh(&ctx).unwrap();
        let screen = render(&mut v, &ctx);
        assert!(screen.contains("stashed work"), "{screen}");
        assert!(screen.contains("shelved work"));
        assert!(screen.contains("a.txt"));
        // Switch to the shelf: its files appear.
        v.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(v.focus, Focus::Shelf);
        let screen = render(&mut v, &ctx);
        assert!(
            screen.contains("n.txt") && screen.contains("b.txt"),
            "{screen}"
        );
        assert_eq!(v.files.len(), 2);
    }

    #[test]
    fn filter_captures_input() {
        let (_d, ctx) = setup();
        let mut v = ShelfView::new(&ctx);
        v.refresh(&ctx).unwrap();
        v.handle_key(key(KeyCode::Char('/')), &ctx);
        assert!(v.captures_input());
        for c in "zzz".chars() {
            v.handle_key(key(KeyCode::Char(c)), &ctx);
        }
        assert!(v.stash_visible().is_empty());
        v.handle_key(key(KeyCode::Esc), &ctx);
        assert!(!v.captures_input());
        assert_eq!(v.stash_visible().len(), 1);
    }

    #[test]
    fn marks_and_partial_unshelve_from_files() {
        let (_d, ctx) = setup();
        let mut v = ShelfView::new(&ctx);
        v.refresh(&ctx).unwrap();
        v.handle_key(key(KeyCode::Tab), &ctx);
        v.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(v.focus, Focus::Files);
        v.handle_key(key(KeyCode::Char(' ')), &ctx);
        assert_eq!(v.marked.len(), 1);
        let marked = v.marked.iter().next().cloned().unwrap();
        let a = v.handle_key(key(KeyCode::Char('u')), &ctx);
        assert!(!a.is_none());
        assert!(ctx.repo.path(&marked).exists());
        v.refresh(&ctx).unwrap();
        let e = v.selected_shelf().unwrap();
        assert!(e.is_unshelved(&marked) && !e.is_applied());
    }

    #[test]
    fn menus_and_keys_produce_actions() {
        let (_d, ctx) = setup();
        let mut v = ShelfView::new(&ctx);
        v.refresh(&ctx).unwrap();
        for c in ['.', 'a', 'p', 'd', 'b', 's', 'P', 'F'] {
            assert!(
                matches!(v.handle_key(key(KeyCode::Char(c)), &ctx), Action::Popup(_)),
                "key {c}"
            );
        }
        assert!(matches!(
            v.handle_key(key(KeyCode::Enter), &ctx),
            Action::Popup(_)
        ));
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('S')), &ctx),
            Action::Toast(_)
        ));
        write(&ctx.repo, "c.txt", "new\n");
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('S')), &ctx),
            Action::Popup(_)
        ));
        v.handle_key(key(KeyCode::Tab), &ctx);
        for c in ['.', 'u', 'd', 'R', 'x'] {
            assert!(
                matches!(v.handle_key(key(KeyCode::Char(c)), &ctx), Action::Popup(_)),
                "key {c}"
            );
        }
        assert!(matches!(
            v.handle_key(key(KeyCode::Enter), &ctx),
            Action::Popup(_)
        ));
        assert!(!v.help().is_empty());
        assert_eq!(palette().len(), 11);
    }

    #[test]
    fn empty_repo_renders() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let mut v = ShelfView::new(&ctx);
        v.refresh(&ctx).unwrap();
        let screen = render(&mut v, &ctx);
        assert!(screen.contains("No stashes"));
        assert!(screen.contains("shelf is empty"));
        for c in [
            KeyCode::Char('j'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Char(' '),
            KeyCode::Char('u'),
        ] {
            v.handle_key(key(c), &ctx);
        }
        assert!(matches!(open_stash_dialog(&ctx), Action::Error(_)));
        assert!(matches!(shelve_paths(&ctx, vec![]), Action::Toast(_)));
    }

    #[test]
    fn files_pane_height_saturates() {
        assert_eq!(files_pane_height(0, 30), 5);
        assert_eq!(files_pane_height(4, 30), 7);
        assert_eq!(files_pane_height(100, 30), 12);
        assert_eq!(files_pane_height(100, 2), 5);
        assert_eq!(files_pane_height(usize::MAX, u16::MAX), 26_214);
        assert_eq!(files_pane_height(65_534, u16::MAX), 26_214);
    }
}
