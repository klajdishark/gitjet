//! Commit tool window: local changes grouped by changelist, partial staging, commit dialog.
//!
//! Left pane: changelists (plus "Merge Conflicts" and "Unversioned Files"), flat or as a directory
//! tree, with a staging indicator per file. Right pane: diff preview of the selected file
//! (Local / Unstaged / Staged). Actions apply to marked files or to the selected row.

mod commit_dialog;
mod partial_popup;
mod tree;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub use commit_dialog::{CommitDialog, CommitTarget};
pub use partial_popup::PartialPopup;
use tree::{BuildOpts, Group, Row, RowKind};

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::changelist::Changelists;
use crate::git::commit;
use crate::git::diff::{DiffOptions, FileDiff, Hunk};
use crate::git::status::{FileState, Status, StatusEntry};
use crate::git::Repo;
use crate::theme::Theme;
use crate::ui::bg::{Background, Preview};
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup};
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, PaletteItem, View};
use crate::util::truncate;

/// Maximum number of diff lines kept for the inline preview (Enter shows everything).
const PREVIEW_CAP: usize = 4000;

/// How long the selection must rest on a file before its diff loads (holding an arrow key skips files).
const PREVIEW_DELAY: Duration = Duration::from_millis(60);

/// What a refresh loads off the UI thread.
struct Loaded {
    status: Status,
    lists: Changelists,
    /// Set when the changelists were adjusted to the work tree and must be saved.
    lists_changed: bool,
    /// Where a damaged changelist file was moved.
    backup: Option<PathBuf>,
}

fn load_changes(repo: &Repo) -> anyhow::Result<Loaded> {
    let mut status = repo.status()?;
    status.entries.retain(|e| !e.is_ignored());
    let (mut lists, backup) = Changelists::load_checked(repo)?;
    let tracked: Vec<String> = status
        .entries
        .iter()
        .filter(|e| !e.is_untracked())
        .map(|e| e.path.clone())
        .collect();
    let lists_changed = lists.sync(&tracked);
    Ok(Loaded {
        status,
        lists,
        lists_changed,
        backup,
    })
}

/// Diff of `e` shown in the preview, capped at [`PREVIEW_CAP`] lines (`true` when cut).
type PreviewData = Result<(Vec<FileDiff>, bool), String>;

fn load_preview(
    repo: &Repo,
    e: &StatusEntry,
    mode: PreviewMode,
    opts: &DiffOptions,
) -> PreviewData {
    let mut paths = vec![e.path.clone()];
    if let Some(o) = &e.orig_path {
        paths.push(o.clone());
    }
    let res = if e.is_untracked() {
        match mode {
            PreviewMode::Staged => Ok(Vec::new()),
            _ => repo.untracked_diff(&e.path).map(|d| vec![d]),
        }
    } else {
        match mode {
            PreviewMode::Local => repo.diff_local(&paths, opts),
            PreviewMode::Unstaged => repo.diff_unstaged(&paths, opts),
            PreviewMode::Staged => repo.diff_staged(&paths, opts),
        }
    };
    res.map(|mut d| {
        let cut = cap_diff(&mut d, PREVIEW_CAP);
        (d, cut)
    })
    .map_err(|e| format!("{e:#}"))
}

/// Which diff the preview shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewMode {
    /// Work tree vs HEAD
    Local,
    /// Work tree vs index
    Unstaged,
    /// Index vs HEAD
    Staged,
}

impl PreviewMode {
    fn next(self) -> Self {
        match self {
            PreviewMode::Local => PreviewMode::Unstaged,
            PreviewMode::Unstaged => PreviewMode::Staged,
            PreviewMode::Staged => PreviewMode::Local,
        }
    }

    fn label(self) -> &'static str {
        match self {
            PreviewMode::Local => "Local (HEAD)",
            PreviewMode::Unstaged => "Unstaged",
            PreviewMode::Staged => "Staged",
        }
    }

    fn source(self, paths: Vec<String>) -> DiffSource {
        match self {
            PreviewMode::Local => DiffSource::Local { paths },
            PreviewMode::Unstaged => DiffSource::Unstaged { paths },
            PreviewMode::Staged => DiffSource::Staged { paths },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    List,
    Preview,
}

pub struct ChangesView {
    status: Status,
    lists: Changelists,
    rows: Vec<Row>,
    sel: usize,
    offset: usize,
    list_height: usize,
    list_area: Rect,
    tree: bool,
    collapsed: HashSet<String>,
    marked: BTreeSet<String>,
    filter: TextInput,
    filtering: bool,
    focus: Pane,
    mode: PreviewMode,
    loader: Background<Result<Loaded, String>>,
    /// Why the last refresh failed (shown above the list until the next one succeeds).
    load_error: Option<String>,
    previews: Preview<(String, PreviewMode), PreviewData>,
    preview: Vec<FileDiff>,
    /// The diff the selection wants, and the one `preview` is up to date with.
    preview_key: Option<(String, PreviewMode)>,
    preview_shown: Option<(String, PreviewMode)>,
    /// File whose diff `preview` holds (kept on screen while the same file reloads).
    preview_path: Option<String>,
    preview_truncated: bool,
    preview_scroll: usize,
    preview_height: usize,
    preview_error: Option<String>,
    /// Warning shown above the list until dismissed with Esc (e.g. a damaged changelist file).
    notice: Option<String>,
}

impl ChangesView {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            status: Status::default(),
            lists: Changelists::default(),
            rows: Vec::new(),
            sel: 0,
            offset: 0,
            list_height: 0,
            list_area: Rect::default(),
            tree: false,
            collapsed: HashSet::new(),
            marked: BTreeSet::new(),
            filter: TextInput::default(),
            filtering: false,
            focus: Pane::List,
            mode: PreviewMode::Local,
            loader: Background::new(),
            load_error: None,
            previews: Preview::new(PREVIEW_DELAY, 32),
            preview: Vec::new(),
            preview_key: None,
            preview_shown: None,
            preview_path: None,
            preview_truncated: false,
            preview_scroll: 0,
            preview_height: 0,
            preview_error: None,
            notice: None,
        }
    }

    // ----- selection helpers -------------------------------------------------------------------

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.sel)
    }

    fn selected_entry(&self) -> Option<&StatusEntry> {
        self.selected_row()
            .and_then(Row::entry)
            .and_then(|i| self.status.entries.get(i))
    }

    /// Entries the next action applies to: marked files, else everything in/under the selected row.
    fn targets(&self) -> Vec<StatusEntry> {
        if !self.marked.is_empty() {
            return self
                .status
                .entries
                .iter()
                .filter(|e| self.marked.contains(&e.path))
                .cloned()
                .collect();
        }
        self.selected_row()
            .map(|r| {
                r.files
                    .iter()
                    .filter_map(|i| self.status.entries.get(*i).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn target_paths(&self) -> Vec<String> {
        self.targets().into_iter().map(|e| e.path).collect()
    }

    /// The changelist the selection belongs to (the active list for conflicts / unversioned).
    fn current_list(&self) -> usize {
        match self.selected_row().map(|r| r.group) {
            Some(Group::List(i)) => i,
            _ => self.lists.active(),
        }
    }

    /// Changelist index when the selected row is a changelist header.
    fn selected_list_header(&self) -> Option<usize> {
        match self.selected_row() {
            Some(Row {
                kind: RowKind::Group { .. },
                group: Group::List(i),
                ..
            }) => Some(*i),
            _ => None,
        }
    }

    fn row_key(&self, r: &Row) -> String {
        r.key(&self.lists, &self.status.entries)
    }

    fn rebuild(&mut self, keep: Option<String>) {
        let filter = self.filter.value();
        self.rows = tree::build_rows(
            &self.status.entries,
            &self.lists,
            &BuildOpts {
                tree: self.tree,
                collapsed: &self.collapsed,
                filter: &filter,
            },
        );
        if let Some(k) = keep {
            if let Some(i) = self.rows.iter().position(|r| self.row_key(r) == k) {
                self.sel = i;
            }
        }
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
    }

    fn rebuild_keeping_selection(&mut self) {
        let key = self.selected_row().map(|r| self.row_key(r));
        self.rebuild(key);
    }

    // ----- preview -----------------------------------------------------------------------------

    /// Request the selected file's diff if the selection or mode changed, and show it once loaded.
    fn update_preview(&mut self, ctx: &Ctx) {
        let Some(e) = self.selected_entry().cloned() else {
            self.preview.clear();
            self.preview_key = None;
            self.preview_shown = None;
            self.preview_path = None;
            self.preview_error = None;
            return;
        };
        let key = (e.path.clone(), self.mode);
        if self.preview_key.as_ref() != Some(&key) {
            self.preview_key = Some(key.clone());
            let opts = DiffOptions::from_config(&ctx.config.diff);
            let mode = self.mode;
            self.previews.request(&ctx.repo, key.clone(), move |repo| {
                load_preview(repo, &e, mode, &opts)
            });
        }
        if self.preview_shown.as_ref() == Some(&key) {
            return;
        }
        let Some(res) = self.previews.get(&key) else {
            return;
        };
        if self.preview_path.as_ref() != Some(&key.0) {
            self.preview_scroll = 0;
            self.preview_path = Some(key.0.clone());
        }
        self.preview_shown = Some(key);
        match res {
            Ok((d, cut)) => {
                self.preview = d;
                self.preview_truncated = cut;
                self.preview_error = None;
            }
            Err(err) => {
                self.preview.clear();
                self.preview_truncated = false;
                self.preview_error = Some(err);
            }
        }
    }

    /// True while the selected file's diff is still loading.
    fn preview_loading(&self) -> bool {
        self.preview_key.is_some() && self.preview_shown != self.preview_key
    }

    /// Apply a finished refresh.
    fn apply_loaded(&mut self, ctx: &Ctx) {
        let Some(res) = self.loader.poll() else {
            return;
        };
        let loaded = match res {
            Ok(l) => l,
            Err(e) => {
                self.load_error = Some(e);
                return;
            }
        };
        self.load_error = None;
        if let Some(b) = loaded.backup {
            self.notice = Some(format!(
                "The changelist file was damaged and has been reset; the old file is saved as {} (Esc to dismiss)",
                b.display()
            ));
        }
        if loaded.lists_changed {
            if let Err(e) = loaded.lists.save(&ctx.repo) {
                self.load_error = Some(format!("{e:#}"));
            }
        }
        let key = self.selected_row().map(|r| self.row_key(r));
        let status = loaded.status;
        let present: HashSet<&str> = status.entries.iter().map(|e| e.path.as_str()).collect();
        self.marked.retain(|p| present.contains(p.as_str()));
        self.status = status;
        self.lists = loaded.lists;
        self.rebuild(key);
        // The work tree changed: reload the diff, keeping the old one on screen until it arrives.
        self.previews.invalidate();
        self.preview_key = None;
        self.preview_shown = None;
        self.update_preview(ctx);
    }

    fn scroll_preview(&mut self, delta: isize) {
        let max = diff_view::diff_height(&self.preview).saturating_sub(1);
        self.preview_scroll =
            (self.preview_scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    // ----- drawing -----------------------------------------------------------------------------

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let n = self.status.entries.len();
        let mut title = format!(
            " Local Changes · {n} file{} ",
            if n == 1 { "" } else { "s" }
        );
        if self.tree {
            title.push_str("· tree ");
        }
        if !self.marked.is_empty() {
            title.push_str(&format!("· {} marked ", self.marked.len()));
        }
        let block = panel(title, self.focus == Pane::List && !self.filtering, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let show_filter = self.filtering || !self.filter.is_empty();
        let notice = self
            .load_error
            .as_ref()
            .map(|e| (e, t.error))
            .or(self.notice.as_ref().map(|n| (n, t.warning)));
        let [notice_area, filter_area, list] = Layout::vertical([
            Constraint::Length(u16::from(notice.is_some())),
            Constraint::Length(u16::from(show_filter)),
            Constraint::Min(1),
        ])
        .areas(inner);
        if let Some((n, color)) = notice {
            f.render_widget(
                Line::styled(n.clone(), Style::default().fg(color)),
                notice_area,
            );
        }
        if show_filter {
            let [p, input] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(filter_area);
            f.render_widget(Span::styled("/ ", t.key()), p);
            self.filter.render(f, input, t.text(), self.filtering);
        }
        self.list_area = list;
        self.list_height = list.height as usize;
        self.offset = scroll_to_keep_visible(self.sel, self.offset, self.list_height)
            .min(self.rows.len().saturating_sub(self.list_height));
        let width = list.width as usize;
        let mut lines: Vec<Line> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.list_height)
            .map(|(i, r)| {
                let line = self.row_line(r, width, t);
                if i == self.sel {
                    line.patch_style(t.selected())
                } else {
                    line
                }
            })
            .collect();
        if self.status.entries.is_empty() && lines.len() < self.list_height {
            lines.push(Line::styled(
                "  No local changes — the working tree is clean",
                t.dimmed(),
            ));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn row_line(&self, r: &Row, width: usize, t: &Theme) -> Line<'static> {
        let indent = "  ".repeat(r.depth);
        let arrow = if r.collapsed { "▸ " } else { "▾ " };
        match &r.kind {
            RowKind::Group {
                name,
                description,
                active,
            } => {
                let color = match r.group {
                    Group::Conflicts => t.conflict,
                    Group::Unversioned => t.untracked,
                    Group::List(_) => t.fg,
                };
                let mut style = Style::default().fg(color);
                if *active || r.group == Group::Conflicts {
                    style = style.add_modifier(Modifier::BOLD);
                }
                let count = r.files.len();
                let mut spans = vec![
                    Span::styled(arrow.to_string(), t.dimmed()),
                    Span::styled(name.clone(), style),
                    Span::styled(
                        format!("  {count} file{}", if count == 1 { "" } else { "s" }),
                        t.dimmed(),
                    ),
                ];
                if *active && self.lists.lists.len() > 1 {
                    spans.push(Span::styled("  (active)", Style::default().fg(t.accent)));
                }
                if !description.is_empty() {
                    let room = width.saturating_sub(name.chars().count() + 20);
                    spans.push(Span::styled(
                        format!("  {}", truncate(description, room)),
                        t.dimmed(),
                    ));
                }
                Line::from(spans)
            }
            RowKind::Dir { label, .. } => Line::from(vec![
                Span::raw(indent),
                Span::styled(arrow.to_string(), t.dimmed()),
                Span::styled(format!("{label}/"), Style::default().fg(t.accent)),
                Span::styled(format!("  {}", r.files.len()), t.dimmed()),
            ]),
            RowKind::File { entry, name, dir } => {
                let Some(e) = self.status.entries.get(*entry) else {
                    return Line::raw("");
                };
                let marked = self.marked.contains(&e.path);
                let (ind, ind_color) = stage_indicator(e, t);
                let state = e.display_state();
                let mut spans = vec![
                    Span::raw(indent),
                    Span::styled(
                        if marked { "▪ " } else { "  " },
                        Style::default().fg(t.accent),
                    ),
                    Span::styled(format!("{ind} "), Style::default().fg(ind_color)),
                    Span::styled(
                        format!("{} ", state.letter()),
                        Style::default().fg(state_color(state, t)),
                    ),
                    Span::styled(name.clone(), Style::default().fg(state_color(state, t))),
                ];
                if !self.tree && !dir.is_empty() {
                    spans.push(Span::styled(format!("  {dir}"), t.dimmed()));
                }
                if let Some(o) = &e.orig_path {
                    spans.push(Span::styled(format!("  ← {o}"), t.dimmed()));
                }
                if let Some(c) = e.conflict {
                    spans.push(Span::styled(
                        format!("  ({})", c.label()),
                        Style::default().fg(t.conflict),
                    ));
                }
                if e.submodule {
                    spans.push(Span::styled("  [submodule]", t.dimmed()));
                }
                Line::from(spans)
            }
        }
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let path = self.selected_entry().map(|e| e.path.clone());
        let title = match &path {
            Some(p) => format!(" {} · {} ", self.mode.label(), p),
            None => format!(" {} ", self.mode.label()),
        };
        let block = panel(title, self.focus == Pane::Preview, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [body, foot] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        self.preview_height = body.height as usize;
        if path.is_none() {
            let text = self.selection_summary();
            f.render_widget(Paragraph::new(text).style(t.dimmed()), body);
            return;
        }
        if self.preview_loading() && self.preview_path != path {
            f.render_widget(Paragraph::new("Loading…").style(t.dimmed()), body);
        } else if let Some(e) = &self.preview_error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                body,
            );
        } else if self.preview.is_empty() {
            let msg = match self.mode {
                PreviewMode::Local => "No differences",
                PreviewMode::Unstaged => "No unstaged changes (press d to switch view)",
                PreviewMode::Staged => "Nothing staged for this file (press d to switch view)",
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), body);
        } else {
            diff_view::draw_diff(f, body, &self.preview, self.preview_scroll, ctx);
        }
        let mut spans = vec![
            Span::styled("d", t.key()),
            Span::styled(" Local/Unstaged/Staged  ", t.dimmed()),
            Span::styled("Enter", t.key()),
            Span::styled(" full diff  ", t.dimmed()),
            Span::styled("J/K Ctrl+D/U", t.key()),
            Span::styled(" scroll", t.dimmed()),
        ];
        if self.preview_truncated {
            spans.push(Span::styled(
                "  (preview truncated)",
                Style::default().fg(t.warning),
            ));
        }
        f.render_widget(Line::from(spans), foot);
    }

    fn selection_summary(&self) -> Vec<Line<'static>> {
        let Some(r) = self.selected_row() else {
            return vec![Line::raw("No local changes.")];
        };
        let n = r.files.len();
        let mut v = Vec::new();
        match &r.kind {
            RowKind::Group {
                name,
                description,
                active,
            } => {
                v.push(Line::raw(format!(
                    "{name}: {n} file{}",
                    if n == 1 { "" } else { "s" }
                )));
                if !description.is_empty() {
                    v.push(Line::raw(description.clone()));
                }
                if *active {
                    v.push(Line::raw("New changes are added to this changelist."));
                }
                v.push(Line::raw(""));
                if matches!(r.group, Group::List(_)) {
                    v.push(Line::raw(
                        "c commit this changelist · Space stage all · r rollback",
                    ));
                    v.push(Line::raw(
                        "m menu: rename (R) · delete (X) · description (E) · set active (*)",
                    ));
                } else if r.group == Group::Unversioned {
                    v.push(Line::raw(
                        "Space add to VCS · i add to .gitignore · D delete",
                    ));
                } else {
                    v.push(Line::raw("x resolve conflict · Space mark resolved"));
                }
            }
            RowKind::Dir { path, .. } => {
                v.push(Line::raw(format!(
                    "{path}/: {n} file{}",
                    if n == 1 { "" } else { "s" }
                )));
            }
            RowKind::File { .. } => {}
        }
        v
    }

    // ----- key handling ------------------------------------------------------------------------

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.set("");
                self.rebuild_keeping_selection();
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Up => self.sel = move_sel(self.sel, -1, self.rows.len()),
            KeyCode::Down => self.sel = move_sel(self.sel, 1, self.rows.len()),
            _ => {
                if self.filter.handle_key(&key) {
                    self.sel = 0;
                    self.rebuild(None);
                }
            }
        }
        Action::None
    }

    fn toggle_collapse(&mut self, want: Option<bool>) -> bool {
        let Some(r) = self.selected_row() else {
            return false;
        };
        if matches!(r.kind, RowKind::File { .. }) {
            return false;
        }
        let key = self.row_key(r);
        let is = self.collapsed.contains(&key);
        let target = want.unwrap_or(!is);
        if target == is {
            return false;
        }
        if target {
            self.collapsed.insert(key.clone());
        } else {
            self.collapsed.remove(&key);
        }
        self.rebuild(Some(key));
        true
    }

    /// Left/h: collapse the node, or jump to its parent.
    fn collapse_or_parent(&mut self) {
        if self.toggle_collapse(Some(true)) {
            return;
        }
        let Some(depth) = self.selected_row().map(|r| r.depth) else {
            return;
        };
        if depth == 0 {
            return;
        }
        if let Some(i) = self.rows[..self.sel].iter().rposition(|r| r.depth < depth) {
            self.sel = i;
        }
    }

    fn toggle_mark(&mut self) {
        let paths = match self.selected_row() {
            Some(r) => r
                .files
                .iter()
                .filter_map(|i| self.status.entries.get(*i))
                .map(|e| e.path.clone())
                .collect(),
            None => Vec::<String>::new(),
        };
        if paths.is_empty() {
            return;
        }
        if paths.iter().all(|p| self.marked.contains(p)) {
            for p in &paths {
                self.marked.remove(p);
            }
        } else {
            self.marked.extend(paths);
        }
        self.sel = move_sel(self.sel, 1, self.rows.len());
    }

    fn enter(&mut self, ctx: &Ctx) -> Action {
        let Some(e) = self.selected_entry().cloned() else {
            self.toggle_collapse(None);
            return Action::None;
        };
        if e.is_conflicted() {
            return crate::ui::views::conflicts::open_resolver(ctx, &e.path);
        }
        let mut paths = vec![e.path.clone()];
        if let Some(o) = &e.orig_path {
            paths.push(o.clone());
        }
        diff_view::open(
            format!("{} · {}", self.mode.label(), e.path),
            self.mode.source(paths),
        )
    }

    fn partial(&self, ctx: &Ctx) -> Action {
        match self.selected_entry() {
            Some(e) => open_partial(ctx, e),
            None => Action::Toast("Select a file for partial staging".into()),
        }
    }

    fn commit(&self, ctx: &Ctx, amend: bool, push: bool) -> Action {
        let marked: Vec<String> = self.marked.iter().cloned().collect();
        let target = commit_target(
            &self.status,
            &self.lists,
            Some(self.current_list()),
            &marked,
        );
        Action::popup(CommitDialog::new(ctx, target, amend, push))
    }

    fn file_action(&self, f: impl FnOnce(&StatusEntry) -> Action) -> Action {
        match self.selected_entry() {
            Some(e) => f(e),
            None => Action::Toast("Select a file first".into()),
        }
    }

    fn gitignore(&self) -> Action {
        ignore_action(self.gitignore_patterns())
    }

    fn list_op(&self, op: char) -> Action {
        let Some(i) = self
            .selected_list_header()
            .or_else(|| (op == '*').then(|| self.current_list()))
        else {
            return Action::Toast("Select a changelist header first".into());
        };
        let name = self
            .lists
            .lists
            .get(i)
            .map(|l| l.name.clone())
            .unwrap_or_default();
        let desc = self
            .lists
            .lists
            .get(i)
            .map(|l| l.description.clone())
            .unwrap_or_default();
        match op {
            'R' => rename_changelist(i, &name),
            'X' => delete_changelist(i, &name),
            'E' => edit_description(i, &desc),
            _ => set_active(i, &name),
        }
    }

    fn context_menu(&self, ctx: &Ctx) -> Action {
        let entries = self.targets();
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        let file = self.selected_entry().cloned();
        let mut items: Vec<MenuItem> = Vec::new();
        let has_targets = !entries.is_empty();
        let untracked: Vec<String> = entries
            .iter()
            .filter(|e| e.is_untracked())
            .map(|e| e.path.clone())
            .collect();

        let target = commit_target(
            &self.status,
            &self.lists,
            Some(self.current_list()),
            &self.marked.iter().cloned().collect::<Vec<_>>(),
        );
        let t2 = target.clone();
        items.push(MenuItem::new("Commit…", "c", move |ctx| {
            Action::popup(CommitDialog::new(ctx, target, false, false))
        }));
        items.push(MenuItem::new("Commit and Push…", "C", move |ctx| {
            Action::popup(CommitDialog::new(ctx, t2, false, true))
        }));
        {
            let e = entries.clone();
            let label = if all_fully_staged(&entries) {
                "Unstage"
            } else {
                "Stage"
            };
            items.push(
                MenuItem::new(label, " ", move |ctx| toggle_stage(ctx, &e)).enabled(has_targets),
            );
        }
        if let Some(fe) = file.clone() {
            let fe2 = fe.clone();
            items.push(MenuItem::new(
                "Partial stage / unstage lines…",
                "p",
                move |ctx| open_partial(ctx, &fe),
            ));
            let mode = self.mode;
            items.push(MenuItem::new("Show diff", "d", move |_| {
                let mut p = vec![fe2.path.clone()];
                if let Some(o) = &fe2.orig_path {
                    p.push(o.clone());
                }
                diff_view::open(format!("{} · {}", mode.label(), fe2.path), mode.source(p))
            }));
        }
        {
            let e = entries.clone();
            items.push(
                MenuItem::new("Rollback…", "r", move |_| rollback_confirm(e)).enabled(has_targets),
            );
        }
        {
            let lists = self.lists.clone();
            let p: Vec<String> = entries
                .iter()
                .filter(|e| !e.is_untracked())
                .map(|e| e.path.clone())
                .collect();
            let any = !p.is_empty();
            items.push(
                MenuItem::new("Move to another changelist…", "M", move |_| {
                    move_menu(&lists, p)
                })
                .enabled(any),
            );
        }
        {
            let p: Vec<String> = self.marked.iter().cloned().collect();
            items.push(MenuItem::new("New changelist…", "n", move |_| {
                new_changelist(p)
            }));
        }
        if let Some(i) = self.selected_list_header() {
            let name = self.lists.lists[i].name.clone();
            let desc = self.lists.lists[i].description.clone();
            let (n1, n2) = (name.clone(), name.clone());
            items.push(MenuItem::new("Rename changelist…", "R", move |_| {
                rename_changelist(i, &n1)
            }));
            items.push(MenuItem::new(
                "Edit changelist description…",
                "E",
                move |_| edit_description(i, &desc),
            ));
            items.push(
                MenuItem::new("Set as active changelist", "*", move |_| set_active(i, &n2))
                    .enabled(!self.lists.lists[i].active),
            );
            items.push(
                MenuItem::new("Delete changelist…", "X", move |_| {
                    delete_changelist(i, &name)
                })
                .enabled(i != 0),
            );
        }
        {
            let p = paths.clone();
            items.push(
                MenuItem::new("Shelve changes…", "s", move |ctx| {
                    crate::ui::views::shelf::shelve_paths(ctx, p)
                })
                .enabled(has_targets),
            );
        }
        items.push(MenuItem::new(
            "Stash all changes…",
            "S",
            crate::ui::views::shelf::open_stash_dialog,
        ));
        if let Some(fe) = file.clone() {
            let (p1, p2, p3, p4, p5) = (
                fe.path.clone(),
                fe.path.clone(),
                fe.path.clone(),
                fe.path.clone(),
                fe.path.clone(),
            );
            let tracked = !fe.is_untracked();
            items.push(
                MenuItem::new("Show blame", "b", move |ctx| {
                    crate::ui::views::files::open_blame(ctx, &p1, None)
                })
                .enabled(tracked && fe.worktree != FileState::Deleted),
            );
            items.push(
                MenuItem::new("File history", "H", move |ctx| {
                    crate::ui::views::files::open_file_history(ctx, &p2)
                })
                .enabled(tracked),
            );
            items.push(MenuItem::new("Local history", "L", move |ctx| {
                crate::ui::views::local_history::open_for_path(ctx, &p3)
            }));
            items.push(
                MenuItem::new("Resolve conflict…", "x", move |ctx| {
                    crate::ui::views::conflicts::open_resolver(ctx, &p4)
                })
                .enabled(fe.is_conflicted()),
            );
            items.push(
                MenuItem::new("Open in editor", "o", move |ctx| open_editor(ctx, &p5))
                    .enabled(ctx.repo.path(&fe.path).is_file()),
            );
        }
        {
            let u = untracked.clone();
            items.push(
                MenuItem::new("Delete unversioned files…", "D", move |_| {
                    delete_unversioned(u)
                })
                .enabled(!untracked.is_empty()),
            );
            let ignore = self.gitignore_patterns();
            items.push(
                MenuItem::new("Add to .gitignore…", "i", move |_| ignore_action(ignore))
                    .enabled(!untracked.is_empty()),
            );
        }
        items.push(MenuItem::new("Stage all", "a", stage_all));
        items.push(MenuItem::new("Unstage all", "A", unstage_all));
        items.push(MenuItem::new("Refresh", "F", |_| Action::Refresh));
        let title = match (&file, self.marked.len()) {
            (_, n) if n > 0 => format!("{n} marked files"),
            (Some(e), _) => truncate(&e.path, 60),
            (None, _) => "Changes".to_string(),
        };
        Action::popup(MenuPopup::new(title, items))
    }

    fn gitignore_patterns(&self) -> Vec<String> {
        if let Some(Row {
            kind: RowKind::Dir { path, .. },
            group: Group::Unversioned,
            ..
        }) = self.selected_row()
        {
            if self.marked.is_empty() {
                return vec![format!("{}/", commit::gitignore_pattern(path))];
            }
        }
        self.targets()
            .iter()
            .filter(|e| e.is_untracked())
            .map(|e| commit::gitignore_pattern(&e.path))
            .collect()
    }

    fn handle_list_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.rows.len();
        let page = self.list_height.max(1) as isize;
        let half = (self.preview_height.max(2) / 2) as isize;
        let in_preview = self.focus == Pane::Preview;
        if (ctrl || key.modifiers.contains(KeyModifiers::ALT)) && !is_preview_scroll_key(&key) {
            // Ctrl/Alt chords are not single-letter actions (Ctrl+R must not roll back).
            return Action::None;
        }
        match key.code {
            KeyCode::Char('d') if ctrl => self.scroll_preview(half),
            KeyCode::Char('u') if ctrl => self.scroll_preview(-half),
            KeyCode::Char('J') => self.scroll_preview(3),
            KeyCode::Char('K') => self.scroll_preview(-3),
            KeyCode::Up | KeyCode::Char('k') if in_preview => self.scroll_preview(-1),
            KeyCode::Down | KeyCode::Char('j') if in_preview => self.scroll_preview(1),
            KeyCode::PageUp if in_preview => self.scroll_preview(-(self.preview_height as isize)),
            KeyCode::PageDown if in_preview => self.scroll_preview(self.preview_height as isize),
            KeyCode::Up | KeyCode::Char('k') => self.sel = move_sel(self.sel, -1, len),
            KeyCode::Down | KeyCode::Char('j') => self.sel = move_sel(self.sel, 1, len),
            KeyCode::PageUp => self.sel = move_sel(self.sel, -page, len),
            KeyCode::PageDown => self.sel = move_sel(self.sel, page, len),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = len.saturating_sub(1),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if in_preview {
                    Pane::List
                } else {
                    Pane::Preview
                };
            }
            KeyCode::Left | KeyCode::Char('h') => self.collapse_or_parent(),
            KeyCode::Right | KeyCode::Char('l') => {
                self.toggle_collapse(Some(false));
            }
            KeyCode::Enter => return self.enter(ctx),
            KeyCode::Esc => {
                if self.notice.is_some() {
                    self.notice = None;
                } else if !self.marked.is_empty() {
                    self.marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.set("");
                    self.rebuild_keeping_selection();
                } else {
                    self.focus = Pane::List;
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('t') => {
                self.tree = !self.tree;
                self.rebuild_keeping_selection();
            }
            KeyCode::Char('d') => {
                self.mode = self.mode.next();
                self.preview_key = None;
            }
            KeyCode::Char('v') => self.toggle_mark(),
            KeyCode::Char(' ') => return toggle_stage(ctx, &self.targets()),
            KeyCode::Char('a') => return stage_all(ctx),
            KeyCode::Char('A') => return unstage_all(ctx),
            KeyCode::Char('p') | KeyCode::Char('e') => return self.partial(ctx),
            KeyCode::Char('c') => return self.commit(ctx, false, false),
            KeyCode::Char('C') => return self.commit(ctx, false, true),
            KeyCode::Char('r') => return rollback_confirm(self.targets()),
            KeyCode::Char('D') => {
                let u: Vec<String> = self
                    .targets()
                    .into_iter()
                    .filter(|e| e.is_untracked())
                    .map(|e| e.path)
                    .collect();
                return delete_unversioned(u);
            }
            KeyCode::Char('i') => return self.gitignore(),
            KeyCode::Char('n') => return new_changelist(self.marked.iter().cloned().collect()),
            KeyCode::Char('M') => {
                let paths: Vec<String> = self
                    .targets()
                    .into_iter()
                    .filter(|e| !e.is_untracked())
                    .map(|e| e.path)
                    .collect();
                return move_menu(&self.lists, paths);
            }
            KeyCode::Char(c @ ('R' | 'X' | 'E' | '*')) => return self.list_op(c),
            KeyCode::Char('.') | KeyCode::Char('m') => return self.context_menu(ctx),
            KeyCode::Char('s') => {
                let p = self.target_paths();
                if p.is_empty() {
                    return Action::Toast("Nothing selected to shelve".into());
                }
                return crate::ui::views::shelf::shelve_paths(ctx, p);
            }
            KeyCode::Char('S') => return crate::ui::views::shelf::open_stash_dialog(ctx),
            KeyCode::Char('o') => return self.file_action(|e| open_editor(ctx, &e.path)),
            KeyCode::Char('b') => {
                return self
                    .file_action(|e| crate::ui::views::files::open_blame(ctx, &e.path, None))
            }
            KeyCode::Char('H') => {
                return self
                    .file_action(|e| crate::ui::views::files::open_file_history(ctx, &e.path))
            }
            KeyCode::Char('L') => {
                return self
                    .file_action(|e| crate::ui::views::local_history::open_for_path(ctx, &e.path))
            }
            KeyCode::Char('x') => {
                return self.file_action(|e| {
                    if e.is_conflicted() {
                        crate::ui::views::conflicts::open_resolver(ctx, &e.path)
                    } else {
                        Action::Toast("This file has no conflict".into())
                    }
                })
            }
            _ => {}
        }
        Action::None
    }
}

/// Ctrl+D / Ctrl+U: the only modified keys the list handles.
fn is_preview_scroll_key(key: &KeyEvent) -> bool {
    key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('u'))
}

/// Keep at most `cap` diff lines; returns true when something was cut.
fn cap_diff(diffs: &mut Vec<FileDiff>, cap: usize) -> bool {
    let mut left = cap;
    let mut cut = false;
    let mut keep_files = 0;
    for d in diffs.iter_mut() {
        if left == 0 {
            cut = true;
            break;
        }
        keep_files += 1;
        let mut keep_hunks = 0;
        for h in d.hunks.iter_mut() {
            if left == 0 {
                cut = true;
                break;
            }
            keep_hunks += 1;
            if h.lines.len() > left {
                h.lines.truncate(left);
                cut = true;
            }
            left -= h.lines.len();
        }
        let dropped: Vec<Hunk> = d.hunks.drain(keep_hunks..).collect();
        cut |= !dropped.is_empty();
    }
    diffs.truncate(keep_files);
    cut
}

/// `●` fully staged, `◐` partially staged, `○` unstaged, `!` conflict.
fn stage_indicator(e: &StatusEntry, t: &Theme) -> (&'static str, Color) {
    if e.is_conflicted() {
        ("!", t.conflict)
    } else if e.is_staged() && e.worktree == FileState::Unmodified {
        ("●", t.added)
    } else if e.is_staged() {
        ("◐", t.warning)
    } else {
        ("○", t.dim)
    }
}

fn state_color(s: FileState, t: &Theme) -> Color {
    match s {
        FileState::Modified | FileState::TypeChanged => t.modified,
        FileState::Added => t.added,
        FileState::Deleted => t.removed,
        FileState::Renamed | FileState::Copied => t.renamed,
        FileState::Untracked => t.untracked,
        FileState::Ignored => t.ignored,
        FileState::Conflicted => t.conflict,
        FileState::Unmodified => t.fg,
    }
}

fn all_fully_staged(entries: &[StatusEntry]) -> bool {
    !entries.is_empty() && entries.iter().all(|e| e.is_staged() && !e.is_unstaged())
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Bulleted list of up to 10 paths for confirmations.
fn path_list(paths: &[String]) -> String {
    let mut s: String = paths.iter().take(10).map(|p| format!("  {p}\n")).collect();
    if paths.len() > 10 {
        s.push_str(&format!("  … and {} more\n", paths.len() - 10));
    }
    s
}

// ----- actions (free functions so menus and the palette can use them) ---------------------------

/// Up to this many files, stage/unstage runs on the UI thread for instant feedback;
/// larger batches go to the background.
const SYNC_STAGE_LIMIT: usize = 16;

/// Run a git operation in the background; toast `ok_msg` on success (the app refreshes either way).
fn background(
    label: &str,
    ok_msg: String,
    op: impl FnOnce(&Repo) -> anyhow::Result<()> + Send + 'static,
) -> Action {
    Action::spawn(label, move |repo| op(repo).map(|_| ok_msg))
}

/// Paths to pass to `git restore --staged` for these entries: a staged rename also needs its
/// original path, or the deletion of the old name stays staged.
fn unstage_pathspecs(entries: &[StatusEntry]) -> Vec<String> {
    let mut paths = Vec::with_capacity(entries.len());
    for e in entries {
        paths.push(e.path.clone());
        if e.index == FileState::Renamed {
            if let Some(orig) = &e.orig_path {
                paths.push(orig.clone());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Stage the entries, or unstage them when all are already fully staged.
fn toggle_stage(ctx: &Ctx, entries: &[StatusEntry]) -> Action {
    if entries.is_empty() {
        return Action::Toast("Nothing selected".into());
    }
    let n = entries.len();
    let unstage = all_fully_staged(entries);
    let paths: Vec<String> = if unstage {
        unstage_pathspecs(entries)
    } else {
        entries.iter().map(|e| e.path.clone()).collect()
    };
    let msg = format!(
        "{} {}",
        if unstage { "Unstaged" } else { "Staged" },
        plural(n, "file")
    );
    let op = move |repo: &Repo| {
        if unstage {
            commit::unstage_paths(repo, &paths)
        } else {
            commit::stage_paths(repo, &paths)
        }
    };
    if n <= SYNC_STAGE_LIMIT {
        Action::from_result(op(&ctx.repo), msg)
    } else {
        background(if unstage { "Unstage" } else { "Stage" }, msg, op)
    }
}

fn stage_all(_ctx: &Ctx) -> Action {
    background("Stage all", "Staged all changes".into(), commit::stage_all)
}

fn unstage_all(_ctx: &Ctx) -> Action {
    background(
        "Unstage all",
        "Unstaged all changes".into(),
        commit::unstage_all,
    )
}

fn rollback_confirm(entries: Vec<StatusEntry>) -> Action {
    if entries.is_empty() {
        return Action::Toast("Nothing selected to roll back".into());
    }
    let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    let untracked = entries.iter().filter(|e| e.is_untracked()).count();
    let mut msg = format!(
        "Roll back {} to their last committed state?\n\n{}",
        plural(paths.len(), "file"),
        path_list(&paths)
    );
    if untracked > 0 {
        msg.push_str(&format!(
            "\n{} will be deleted.",
            plural(untracked, "unversioned file")
        ));
    }
    msg.push_str("\nLocal modifications will be lost.");
    Action::popup(
        ConfirmPopup::new("Rollback changes", msg, move |_| {
            let ok = format!("Rolled back {}", plural(paths.len(), "file"));
            background("Rollback", ok, move |repo| commit::rollback(repo, &entries))
        })
        .danger(),
    )
}

fn delete_unversioned(paths: Vec<String>) -> Action {
    if paths.is_empty() {
        return Action::Toast("No unversioned files selected".into());
    }
    let msg = format!(
        "Delete {} from disk?\n\n{}",
        plural(paths.len(), "unversioned file"),
        path_list(&paths)
    );
    Action::popup(
        ConfirmPopup::new("Delete files", msg, move |_| {
            let ok = format!("Deleted {}", plural(paths.len(), "file"));
            background("Delete files", ok, move |repo| {
                commit::delete_files(repo, &paths)
            })
        })
        .danger(),
    )
}

/// One pattern: editable prompt; several: added directly.
fn ignore_action(patterns: Vec<String>) -> Action {
    match patterns.len() {
        0 => Action::Toast("Select unversioned files to ignore".into()),
        1 => Action::popup(
            InputPopup::new("Add to .gitignore", &patterns[0], |pat, ctx| {
                Action::from_result(
                    commit::add_to_gitignore(&ctx.repo, pat.trim()),
                    format!("Added {} to .gitignore", pat.trim()),
                )
            })
            .prompt("Pattern appended to the repository's root .gitignore:"),
        ),
        n => Action::spawn("Add to .gitignore", move |repo| {
            for p in &patterns {
                commit::add_to_gitignore(repo, p)?;
            }
            Ok(format!("Added {n} patterns to .gitignore"))
        }),
    }
}

/// Load the changelists, apply `f`, save, refresh.
fn with_lists(ctx: &Ctx, f: impl FnOnce(&mut Changelists) -> anyhow::Result<String>) -> Action {
    let res = Changelists::load(&ctx.repo).and_then(|mut cl| {
        let msg = f(&mut cl)?;
        cl.save(&ctx.repo)?;
        Ok(msg)
    });
    match res {
        Ok(msg) => Action::Batch(vec![Action::Toast(msg), Action::Refresh]),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

/// Ask for a name, create the list and move `paths` (may be empty) into it.
fn new_changelist(paths: Vec<String>) -> Action {
    let prompt = if paths.is_empty() {
        "Name of the new changelist:".to_string()
    } else {
        format!(
            "Name of the new changelist ({} will be moved to it):",
            plural(paths.len(), "marked file")
        )
    };
    Action::popup(
        InputPopup::new("New changelist", "", move |name, ctx| {
            with_lists(ctx, |cl| {
                let i = cl.create(&name, "")?;
                cl.move_files(&paths, i);
                Ok(format!("Created changelist \"{}\"", name.trim()))
            })
        })
        .prompt(prompt),
    )
}

fn rename_changelist(i: usize, name: &str) -> Action {
    Action::popup(InputPopup::new(
        "Rename changelist",
        name,
        move |new, ctx| {
            with_lists(ctx, |cl| {
                cl.rename(i, &new)?;
                Ok(format!("Renamed changelist to \"{}\"", new.trim()))
            })
        },
    ))
}

fn edit_description(i: usize, desc: &str) -> Action {
    Action::popup(
        InputPopup::new("Changelist description", desc, move |d, ctx| {
            with_lists(ctx, |cl| {
                cl.set_description(i, &d);
                Ok("Description updated".into())
            })
        })
        .allow_empty(),
    )
}

fn set_active(i: usize, name: &str) -> Action {
    let name = name.to_string();
    Action::popup(MenuPopup::new(
        "Set active changelist",
        vec![MenuItem::new(
            format!("Make \"{name}\" the active changelist"),
            "y",
            move |ctx| {
                with_lists(ctx, |cl| {
                    cl.set_active(i);
                    Ok(format!("\"{name}\" is now the active changelist"))
                })
            },
        )],
    ))
}

fn delete_changelist(i: usize, name: &str) -> Action {
    if i == 0 {
        return Action::Toast("The default changelist cannot be deleted".into());
    }
    let msg = format!("Delete changelist \"{name}\"?\nIts files move to the default changelist.");
    Action::popup(
        ConfirmPopup::new("Delete changelist", msg, move |ctx| {
            with_lists(ctx, |cl| {
                cl.remove(i)?;
                Ok("Changelist deleted".into())
            })
        })
        .danger(),
    )
}

/// Menu of target changelists for `paths`.
fn move_menu(lists: &Changelists, paths: Vec<String>) -> Action {
    if paths.is_empty() {
        return Action::Toast(
            "Select tracked files to move (unversioned files have no changelist)".into(),
        );
    }
    let mut items: Vec<MenuItem> = lists
        .lists
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let p = paths.clone();
            let name = l.name.clone();
            let key = if i < 9 {
                (i + 1).to_string()
            } else {
                String::new()
            };
            let current = paths.iter().all(|p| lists.list_of(p) == i);
            let label = if current {
                format!("{} (current)", l.name)
            } else {
                l.name.clone()
            };
            MenuItem::new(label, key, move |ctx| {
                with_lists(ctx, |cl| {
                    cl.move_files(&p, i);
                    Ok(format!("Moved {} to \"{name}\"", plural(p.len(), "file")))
                })
            })
            .enabled(!current)
        })
        .collect();
    items.push(MenuItem::new("New changelist…", "n", move |_| {
        new_changelist(paths)
    }));
    Action::popup(MenuPopup::new("Move to changelist", items))
}

fn open_editor(ctx: &Ctx, path: &str) -> Action {
    Action::Suspend(crate::util::editor_command(
        ctx.config.editor.as_deref(),
        &ctx.repo.path(path),
        None,
    ))
}

fn open_partial(ctx: &Ctx, e: &StatusEntry) -> Action {
    if e.is_conflicted() {
        return Action::Toast("Resolve the conflict first".into());
    }
    if e.submodule {
        return Action::Toast("Submodules can only be staged as a whole".into());
    }
    Action::popup(PartialPopup::new(ctx, &e.path, !e.is_unstaged()))
}

/// What a commit from the Changes tab includes: the index when anything is staged, otherwise the
/// marked files, otherwise the files of changelist `list` (the active one when None).
fn commit_target(
    status: &Status,
    lists: &Changelists,
    list: Option<usize>,
    marked: &[String],
) -> CommitTarget {
    let staged: Vec<&StatusEntry> = status.entries.iter().filter(|e| e.is_staged()).collect();
    if !staged.is_empty() {
        return CommitTarget {
            label: "staged changes".into(),
            paths: None,
            untracked: Vec::new(),
            files: staged
                .iter()
                .map(|e| (e.index.letter(), e.path.clone()))
                .collect(),
        };
    }
    let chosen: Vec<&StatusEntry> = if !marked.is_empty() {
        let set: HashSet<&str> = marked.iter().map(String::as_str).collect();
        status
            .entries
            .iter()
            .filter(|e| set.contains(e.path.as_str()))
            .collect()
    } else {
        let active = lists.active();
        let idx = list.unwrap_or(active);
        let assigned = lists.assignments();
        status
            .entries
            .iter()
            .filter(|e| {
                !e.is_untracked()
                    && !e.is_ignored()
                    && !e.is_conflicted()
                    && assigned.get(e.path.as_str()).copied().unwrap_or(active) == idx
            })
            .collect()
    };
    let label = if !marked.is_empty() {
        plural(chosen.len(), "selected file")
    } else {
        let idx = list.unwrap_or_else(|| lists.active());
        format!(
            "changelist \"{}\"",
            lists.lists.get(idx).map(|l| l.name.as_str()).unwrap_or("")
        )
    };
    let mut paths: Vec<String> = Vec::new();
    for e in &chosen {
        paths.push(e.path.clone());
        if let Some(o) = &e.orig_path {
            paths.push(o.clone());
        }
    }
    CommitTarget {
        label,
        untracked: chosen
            .iter()
            .filter(|e| e.is_untracked())
            .map(|e| e.path.clone())
            .collect(),
        files: chosen
            .iter()
            .map(|e| (e.display_state().letter(), e.path.clone()))
            .collect(),
        paths: Some(paths),
    }
}

/// Status + synced changelists, for palette actions that have no view state.
fn load_state(ctx: &Ctx) -> anyhow::Result<(Status, Changelists)> {
    let status = ctx.repo.status()?;
    let mut lists = Changelists::load(&ctx.repo)?;
    let tracked: Vec<String> = status
        .entries
        .iter()
        .filter(|e| !e.is_untracked() && !e.is_ignored())
        .map(|e| e.path.clone())
        .collect();
    if lists.sync(&tracked) {
        lists.save(&ctx.repo)?;
    }
    Ok((status, lists))
}

fn palette_commit(ctx: &Ctx, amend: bool, push: bool) -> Action {
    match load_state(ctx) {
        Ok((st, lists)) => {
            let target = commit_target(&st, &lists, None, &[]);
            Action::popup(CommitDialog::new(ctx, target, amend, push))
        }
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn palette_rollback_all(ctx: &Ctx) -> Action {
    match ctx.repo.status() {
        Ok(st) => {
            let tracked: Vec<StatusEntry> = st
                .entries
                .into_iter()
                .filter(|e| !e.is_untracked() && !e.is_ignored())
                .collect();
            if tracked.is_empty() {
                Action::Toast("No tracked changes to roll back (unversioned files are kept)".into())
            } else {
                rollback_confirm(tracked)
            }
        }
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn palette_partial(ctx: &Ctx) -> Action {
    let st = match ctx.repo.status() {
        Ok(s) => s,
        Err(e) => return Action::Error(format!("{e:#}")),
    };
    let items: Vec<MenuItem> = st
        .entries
        .into_iter()
        .filter(|e| !e.is_ignored() && !e.is_conflicted() && !e.submodule)
        .map(|e| {
            let label = format!("{} {}", e.display_state().letter(), e.path);
            MenuItem::new(label, "", move |ctx| open_partial(ctx, &e))
        })
        .collect();
    if items.is_empty() {
        return Action::Toast("No local changes".into());
    }
    Action::popup(MenuPopup::new("Partial stage file", items).filterable())
}

impl View for ChangesView {
    fn title(&self) -> String {
        "Changes".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        self.loader.start(&ctx.repo, |repo| {
            load_changes(repo).map_err(|e| format!("{e:#}"))
        });
        self.apply_loaded(ctx);
        Ok(())
    }

    fn poll(&mut self, ctx: &Ctx) {
        self.apply_loaded(ctx);
        self.update_preview(ctx);
    }

    fn loading(&self) -> bool {
        self.loader.busy()
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        self.draw_list(f, left, ctx);
        self.draw_preview(f, right, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let a = if self.filtering {
            self.handle_filter_key(key)
        } else {
            self.handle_list_key(key, ctx)
        };
        self.update_preview(ctx);
        a
    }

    fn handle_mouse(&mut self, ev: MouseEvent, ctx: &Ctx) -> Action {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let a = self.list_area;
            if ev.column >= a.x
                && ev.column < a.x + a.width
                && ev.row >= a.y
                && ev.row < a.y + a.height
            {
                let i = self.offset + (ev.row - a.y) as usize;
                if i < self.rows.len() {
                    self.focus = Pane::List;
                    if i == self.sel {
                        return self.enter(ctx);
                    }
                    self.sel = i;
                    self.update_preview(ctx);
                }
            }
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("Space", "stage/unstage"),
            ("c", "commit"),
            ("Enter", "diff/expand"),
            ("p", "partial"),
            ("d", "diff mode"),
            ("m", "menu"),
            ("r", "rollback"),
            ("v", "mark"),
            ("j/k ↑/↓", "move"),
            ("C", "commit and push"),
            ("e", "partial stage (same as p)"),
            ("a / A", "stage all / unstage all"),
            ("t", "flat list / directory tree"),
            ("h/l ←/→", "collapse / expand"),
            ("/", "filter by path"),
            ("Esc", "dismiss notice / clear marks / filter"),
            ("Tab", "focus list / diff"),
            ("J/K Ctrl+D/U", "scroll diff preview"),
            ("n", "new changelist (takes marked files)"),
            ("M", "move to changelist"),
            (
                "R / X / E / *",
                "rename / delete / describe / activate changelist",
            ),
            ("D", "delete unversioned files"),
            ("i", "add to .gitignore"),
            ("s / S", "shelve selection / stash all"),
            ("o", "open in editor"),
            ("b / H / L", "blame / file history / local history"),
            ("x", "resolve conflict"),
            (". or m", "all actions for the selection"),
        ]
    }

    fn captures_input(&self) -> bool {
        self.filtering
    }

    fn badge(&self) -> Option<String> {
        let n = self.status.entries.len();
        (n > 0).then(|| n.to_string())
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Commit…",
            run: |ctx| palette_commit(ctx, false, false),
        },
        PaletteItem {
            name: "Commit and Push…",
            run: |ctx| palette_commit(ctx, false, true),
        },
        PaletteItem {
            name: "Amend last commit…",
            run: |ctx| palette_commit(ctx, true, false),
        },
        PaletteItem {
            name: "Stage all",
            run: stage_all,
        },
        PaletteItem {
            name: "Unstage all",
            run: unstage_all,
        },
        PaletteItem {
            name: "Rollback all…",
            run: palette_rollback_all,
        },
        PaletteItem {
            name: "New changelist…",
            run: |_| new_changelist(Vec::new()),
        },
        PaletteItem {
            name: "Partial stage file…",
            run: palette_partial,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;
    use crate::git::Repo;

    fn view_for(repo: Repo) -> (ChangesView, Ctx) {
        let ctx = Ctx::new(repo, Config::default());
        let mut v = ChangesView::new(&ctx);
        v.refresh(&ctx).unwrap();
        (v, ctx)
    }

    fn press(v: &mut ChangesView, ctx: &Ctx, code: KeyCode) -> Action {
        v.handle_key(KeyEvent::new(code, KeyModifiers::NONE), ctx)
    }

    fn select_path(v: &mut ChangesView, path: &str) {
        v.sel = v
            .rows
            .iter()
            .position(|r| r.entry().is_some_and(|i| v.status.entries[i].path == path))
            .unwrap();
    }

    #[test]
    fn stage_toggle_and_commit_target() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n"), ("b.txt", "1\n")], "init");
        write(&repo, "a.txt", "2\n");
        write(&repo, "b.txt", "2\n");
        write(&repo, "new.txt", "n\n");
        let (mut v, ctx) = view_for(repo);
        assert!(v.rows.iter().any(
            |r| matches!(&r.kind, RowKind::Group { name, .. } if name == "Unversioned Files")
        ));
        select_path(&mut v, "a.txt");
        press(&mut v, &ctx, KeyCode::Char(' '));
        v.refresh(&ctx).unwrap();
        let a = v.status.entries.iter().find(|e| e.path == "a.txt").unwrap();
        assert!(a.is_staged());
        let t = commit_target(&v.status, &v.lists, None, &[]);
        assert_eq!(t.paths, None);
        assert_eq!(t.files.len(), 1);
        // Toggle again: unstaged; then the changelist (tracked files only) is the target.
        select_path(&mut v, "a.txt");
        press(&mut v, &ctx, KeyCode::Char(' '));
        v.refresh(&ctx).unwrap();
        let t = commit_target(&v.status, &v.lists, None, &[]);
        assert_eq!(
            t.paths,
            Some(vec!["a.txt".to_string(), "b.txt".to_string()])
        );
        // Marked files win over the changelist and may include unversioned files.
        let t = commit_target(&v.status, &v.lists, None, &["new.txt".to_string()]);
        assert_eq!(t.untracked, vec!["new.txt".to_string()]);
    }

    #[test]
    fn changelist_assignment_and_move() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n"), ("b.txt", "1\n")], "init");
        write(&repo, "a.txt", "2\n");
        write(&repo, "b.txt", "2\n");
        let (mut v, ctx) = view_for(repo);
        let i = with_lists_direct(&ctx, |cl| cl.create("Feature", "").unwrap());
        with_lists_direct(&ctx, |cl| cl.move_files(&["b.txt".to_string()], i));
        v.refresh(&ctx).unwrap();
        let t = commit_target(&v.status, &v.lists, Some(i), &[]);
        assert_eq!(t.paths, Some(vec!["b.txt".to_string()]));
        assert_eq!(t.label, "changelist \"Feature\"");
        // Committing the changelist commits only its files.
        let req = commit::CommitRequest {
            message: "feature".into(),
            paths: t.paths.clone(),
            ..Default::default()
        };
        commit::commit(&ctx.repo, &req).unwrap();
        v.refresh(&ctx).unwrap();
        assert_eq!(v.status.entries.len(), 1);
        assert_eq!(v.status.entries[0].path, "a.txt");
        assert!(
            v.lists.lists[i].files.is_empty(),
            "assignment cleaned after commit"
        );
    }

    fn with_lists_direct<T>(ctx: &Ctx, f: impl FnOnce(&mut Changelists) -> T) -> T {
        let mut cl = Changelists::load(&ctx.repo).unwrap();
        let r = f(&mut cl);
        cl.save(&ctx.repo).unwrap();
        r
    }

    #[test]
    fn tree_toggle_collapse_and_marks() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("src/a.rs", "1\n"), ("src/b.rs", "1\n")], "init");
        write(&repo, "src/a.rs", "2\n");
        write(&repo, "src/b.rs", "2\n");
        let (mut v, ctx) = view_for(repo);
        let flat = v.rows.len();
        press(&mut v, &ctx, KeyCode::Char('t'));
        assert_eq!(v.rows.len(), flat + 1, "directory row added");
        v.sel = 1;
        press(&mut v, &ctx, KeyCode::Left);
        assert_eq!(v.rows.len(), 2, "directory collapsed");
        press(&mut v, &ctx, KeyCode::Char('v'));
        assert_eq!(v.marked.len(), 2);
        press(&mut v, &ctx, KeyCode::Esc);
        assert!(v.marked.is_empty());
        v.sel = 0;
        press(&mut v, &ctx, KeyCode::Enter);
        assert_eq!(v.rows.len(), 1, "group collapsed");
        // Filter captures input.
        press(&mut v, &ctx, KeyCode::Char('/'));
        assert!(v.captures_input());
        press(&mut v, &ctx, KeyCode::Esc);
        assert!(!v.captures_input());
    }

    #[test]
    fn preview_modes_and_cap() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "a.txt", "2\n");
        let (mut v, ctx) = view_for(repo);
        select_path(&mut v, "a.txt");
        v.preview_key = None;
        v.update_preview(&ctx);
        assert_eq!(v.preview.len(), 1);
        press(&mut v, &ctx, KeyCode::Char('d'));
        assert_eq!(v.mode, PreviewMode::Unstaged);
        assert_eq!(v.preview.len(), 1);
        press(&mut v, &ctx, KeyCode::Char('d'));
        assert_eq!(v.mode, PreviewMode::Staged);
        assert!(v.preview.is_empty());
        let mut d = crate::git::diff::parse_unified(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n",
        );
        assert!(cap_diff(&mut d, 2));
        assert_eq!(d[0].hunks[0].lines.len(), 2);
    }

    #[test]
    fn empty_repo_works() {
        let (_d, repo) = temp_repo();
        let (mut v, ctx) = view_for(repo);
        assert_eq!(v.rows.len(), 1, "only the default changelist");
        assert!(matches!(
            press(&mut v, &ctx, KeyCode::Char('c')),
            Action::Popup(_)
        ));
        write(&ctx.repo, "first.txt", "x\n");
        v.refresh(&ctx).unwrap();
        select_path(&mut v, "first.txt");
        press(&mut v, &ctx, KeyCode::Char(' '));
        v.refresh(&ctx).unwrap();
        assert!(v.status.entries[0].is_staged());
        let a = press(&mut v, &ctx, KeyCode::Char('A'));
        assert!(
            matches!(a, Action::Spawn(_)),
            "bulk unstage runs in the background"
        );
        run_jobs(&ctx, a).unwrap();
        v.refresh(&ctx).unwrap();
        assert!(v.status.entries[0].is_untracked());
    }

    /// Run the background jobs an action contains, like the app's task runner.
    fn run_jobs(ctx: &Ctx, a: Action) -> anyhow::Result<()> {
        match a {
            Action::Spawn(t) => (t.job)(&ctx.repo).map(|_| ()),
            Action::Batch(v) => v.into_iter().try_for_each(|a| run_jobs(ctx, a)),
            _ => Ok(()),
        }
    }

    #[test]
    fn unstaging_a_rename_restores_both_paths() {
        let (_d, repo) = temp_repo();
        commit_files(
            &repo,
            &[("old.txt", "same content\nline 2\nline 3\n")],
            "init",
        );
        repo.run(&["mv", "old.txt", "new.txt"]).unwrap();
        let (mut v, ctx) = view_for(repo);
        let e = v
            .status
            .entries
            .iter()
            .find(|e| e.path == "new.txt")
            .unwrap();
        assert_eq!(e.index, FileState::Renamed);
        assert_eq!(
            unstage_pathspecs(std::slice::from_ref(e)),
            vec!["new.txt".to_string(), "old.txt".to_string()]
        );
        select_path(&mut v, "new.txt");
        run_jobs(&ctx, press(&mut v, &ctx, KeyCode::Char(' '))).unwrap();
        v.refresh(&ctx).unwrap();
        assert!(
            v.status.entries.iter().all(|e| !e.is_staged()),
            "{:?}",
            v.status.entries
        );
        let old = v
            .status
            .entries
            .iter()
            .find(|e| e.path == "old.txt")
            .unwrap();
        assert_eq!(old.worktree, FileState::Deleted);
    }

    #[test]
    fn modified_letters_do_nothing() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "a.txt", "2\n");
        let (mut v, ctx) = view_for(repo);
        select_path(&mut v, "a.txt");
        for c in ['r', 's', 'a', 'o', 'c', ' '] {
            for m in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
                assert!(
                    v.handle_key(KeyEvent::new(KeyCode::Char(c), m), &ctx)
                        .is_none(),
                    "{m:?}+{c}"
                );
            }
        }
        // Ctrl+D / Ctrl+U still scroll the preview.
        assert!(v
            .handle_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &ctx
            )
            .is_none());
        assert!(is_preview_scroll_key(&KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL
        )));
    }

    #[test]
    fn large_stage_batches_run_in_background() {
        let (_d, repo) = temp_repo();
        let files: Vec<(String, String)> = (0..=SYNC_STAGE_LIMIT)
            .map(|i| (format!("f{i}.txt"), "x\n".to_string()))
            .collect();
        for (p, c) in &files {
            write(&repo, p, c);
        }
        let (v, ctx) = view_for(repo);
        let entries: Vec<StatusEntry> = v.status.entries.clone();
        assert!(entries.len() > SYNC_STAGE_LIMIT);
        let a = toggle_stage(&ctx, &entries);
        assert!(matches!(a, Action::Spawn(_)));
        run_jobs(&ctx, a).unwrap();
        assert!(ctx
            .repo
            .status()
            .unwrap()
            .entries
            .iter()
            .all(|e| e.is_staged()));
        // A small batch stays synchronous.
        assert!(matches!(
            toggle_stage(&ctx, &entries[..1]),
            Action::Batch(_)
        ));
    }

    #[test]
    fn damaged_changelists_show_a_notice() {
        let (_d, repo) = temp_repo();
        let path = crate::git::changelist::store_path(&repo).unwrap();
        std::fs::write(&path, "not json").unwrap();
        let (mut v, ctx) = view_for(repo);
        assert!(v
            .notice
            .as_deref()
            .is_some_and(|n| n.contains("changelists.json.bak")));
        press(&mut v, &ctx, KeyCode::Esc);
        assert!(v.notice.is_none());
    }

    /// Render the view and its popups at several sizes (including tiny ones) without panicking.
    #[test]
    fn renders_at_all_sizes() {
        use crate::ui::Popup;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (_d, repo) = temp_repo();
        commit_files(
            &repo,
            &[("src/a.rs", "1\n2\n3\n"), ("conf.txt", "x\n")],
            "init",
        );
        write(&repo, "src/a.rs", "1\nTWO\n3\n4\n");
        write(&repo, "unicode ü dir/new file.txt", "hello\n");
        let (mut v, ctx) = view_for(repo);
        select_path(&mut v, "src/a.rs");
        v.update_preview(&ctx);
        let target = commit_target(&v.status, &v.lists, None, &[]);
        let mut dialog = CommitDialog::new(&ctx, target, false, false);
        let mut partial = PartialPopup::new(&ctx, "src/a.rs", false);
        for (w, h) in [(120, 40), (80, 24), (30, 8), (5, 3), (1, 1)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| {
                let area = f.area();
                v.draw(f, area, &ctx);
                dialog.draw(f, area, &ctx);
                partial.draw(f, area, &ctx);
            })
            .unwrap();
        }
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| {
            let area = f.area();
            v.draw(f, area, &ctx);
        })
        .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Unversioned Files"));
        assert!(text.contains("TWO"));
        assert!(
            text.contains("▾ Changes"),
            "list starts at the top once it fits"
        );
        let mut term = Terminal::new(TestBackend::new(110, 30)).unwrap();
        term.draw(|f| {
            let area = f.area();
            dialog.draw(f, area, &ctx);
        })
        .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Commit to main") && text.contains("Push after commit"));
        term.draw(|f| {
            let area = f.area();
            partial.draw(f, area, &ctx);
        })
        .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("@@ -1,3 +1,4 @@") && text.contains("+TWO"));
        // The context menu opens for a file selection.
        assert!(matches!(
            press(&mut v, &ctx, KeyCode::Char('m')),
            Action::Popup(_)
        ));
    }
}
