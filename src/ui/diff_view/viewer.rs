//! The fullscreen diff viewer: file list + unified or side-by-side diff, jump-between-changes navigation.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::{anyhow, bail, Result};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::model::{self, Row};
use super::render::{self, DiffCache, Flags, Painter, Viewport};
use super::DiffSource;
use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::diff::{ChangeKind, DiffOptions, FileDiff};
use crate::ui::popups::{fuzzy_match, MenuItem, MenuPopup};
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, Popup};
use crate::util;

/// Context used for "show whole file" (git clamps it to the file length).
const WHOLE_FILE_CONTEXT: u32 = 1_000_000;
/// Upper bound for the `+` key.
const MAX_CONTEXT: u32 = 1000;
/// Columns moved per ←/→ press.
const HSCROLL_STEP: u16 = 8;
/// Reply to options that need regenerating an already-parsed diff.
const NOT_RELOADABLE: &str = "Not available for this diff";

/// Operations reachable from keys and from the `.` menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    NextDiff,
    PrevDiff,
    NextFile,
    PrevFile,
    ToggleSplit,
    ToggleWords,
    ToggleSyntax,
    ToggleWhitespace,
    MoreContext,
    LessContext,
    WholeFile,
    ToggleFiles,
    Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Files,
    Diff,
}

enum Load {
    Loading,
    Ready,
    Failed(String),
}

/// Where to put the caret once rows are (re)built.
#[derive(Debug, Clone, Copy)]
enum Jump {
    FirstChange,
    LastChange,
    Row(usize),
    /// Same hunk line as before a mode switch
    Anchor {
        hunk: usize,
        line: usize,
    },
}

/// Fullscreen diff viewer popup.
pub struct DiffViewer {
    title: String,
    source: DiffSource,
    started: bool,
    opts: DiffOptions,
    whole_file: bool,
    flags: Flags,
    side_by_side: bool,
    show_files: bool,
    focus: Focus,
    load: Load,
    rx: Option<Receiver<Result<Vec<FileDiff>>>>,
    diffs: Vec<FileDiff>,
    cache: DiffCache,
    file: usize,
    list_scroll: usize,
    filter: TextInput,
    filtering: bool,
    rows: Vec<Row>,
    starts: Vec<usize>,
    rows_for: Option<(usize, bool)>,
    cursor: usize,
    scroll: usize,
    hscroll: u16,
    height: usize,
    recenter: bool,
    jump: Option<Jump>,
    restore: Option<(String, usize)>,
    pending: Rc<RefCell<Vec<Cmd>>>,
    list_rect: Rect,
    diff_rect: Rect,
}

impl DiffViewer {
    pub fn new(title: String, source: DiffSource) -> Self {
        Self {
            title,
            source,
            started: false,
            opts: DiffOptions::default(),
            whole_file: false,
            flags: Flags {
                word_diff: true,
                syntax: true,
            },
            side_by_side: false,
            show_files: true,
            focus: Focus::Diff,
            load: Load::Loading,
            rx: None,
            diffs: Vec::new(),
            cache: DiffCache::default(),
            file: 0,
            list_scroll: 0,
            filter: TextInput::default(),
            filtering: false,
            rows: Vec::new(),
            starts: Vec::new(),
            rows_for: None,
            cursor: 0,
            scroll: 0,
            hscroll: 0,
            height: 0,
            recenter: false,
            jump: None,
            restore: None,
            pending: Rc::new(RefCell::new(Vec::new())),
            list_rect: Rect::default(),
            diff_rect: Rect::default(),
        }
    }

    // ----- loading -----------------------------------------------------------------------

    fn start(&mut self, ctx: &Ctx) {
        self.started = true;
        self.opts = DiffOptions::from_config(&ctx.config.diff);
        self.flags = Flags::from_config(&ctx.config.diff);
        self.side_by_side = ctx.config.diff.side_by_side;
        self.reload(ctx);
    }

    fn effective_opts(&self) -> DiffOptions {
        DiffOptions {
            context: if self.whole_file {
                WHOLE_FILE_CONTEXT
            } else {
                self.opts.context
            },
            ..self.opts
        }
    }

    /// Load (again) in the background, keeping the current file and caret when possible.
    fn reload(&mut self, ctx: &Ctx) {
        if let Some(d) = self.diffs.get(self.file) {
            self.restore = Some((d.path().to_string(), self.cursor));
        }
        self.load = Load::Loading;
        if let DiffSource::Raw(d) = &self.source {
            let d = d.clone();
            self.set_diffs(d);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let repo = ctx.repo.clone();
        let source = self.source.clone();
        let opts = self.effective_opts();
        std::thread::spawn(move || {
            let _ = tx.send(source.load(&repo, &opts));
        });
        self.rx = Some(rx);
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(d)) => {
                self.rx = None;
                self.set_diffs(d);
            }
            Ok(Err(e)) => {
                self.rx = None;
                self.load = Load::Failed(format!("{e:#}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.load = Load::Failed("the diff could not be loaded".into());
            }
        }
    }

    fn set_diffs(&mut self, diffs: Vec<FileDiff>) {
        self.diffs = diffs;
        self.cache.clear();
        self.load = Load::Ready;
        self.rows_for = None;
        match self.restore.take() {
            Some((path, cursor)) => match self.diffs.iter().position(|d| d.path() == path) {
                Some(i) => {
                    self.file = i;
                    self.jump = Some(Jump::Row(cursor));
                }
                None => {
                    self.file = self.file.min(self.diffs.len().saturating_sub(1));
                    self.jump = Some(Jump::FirstChange);
                }
            },
            None => {
                self.file = 0;
                self.jump = Some(Jump::FirstChange);
            }
        }
    }

    // ----- rows & navigation -------------------------------------------------------------

    fn ensure_rows(&mut self) {
        let key = (self.file, self.side_by_side);
        if self.rows_for == Some(key) {
            return;
        }
        self.rows = if self.side_by_side {
            model::split_rows(&self.diffs, self.file)
        } else {
            model::unified_rows(&self.diffs, self.file..self.file + 1, false)
        };
        self.starts = model::change_starts(&self.diffs, &self.rows);
        self.rows_for = Some(key);
        if let Some(j) = self.jump.take() {
            self.cursor = match j {
                Jump::FirstChange => self.starts.first().copied().unwrap_or(0),
                Jump::LastChange => self.starts.last().copied().unwrap_or(0),
                Jump::Row(r) => r,
                Jump::Anchor { hunk, line } => self.find_anchor(hunk, line).unwrap_or(self.cursor),
            };
            self.recenter = true;
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }

    fn anchor(&self) -> Option<Jump> {
        match self.rows.get(self.cursor)? {
            Row::Line { hunk, line, .. } => Some(Jump::Anchor {
                hunk: *hunk,
                line: *line,
            }),
            Row::Split { hunk, old, new, .. } => Some(Jump::Anchor {
                hunk: *hunk,
                line: old.or(*new)?,
            }),
            Row::Hunk(_, h) => Some(Jump::Anchor {
                hunk: *h,
                line: usize::MAX,
            }),
            _ => None,
        }
    }

    fn find_anchor(&self, hunk: usize, line: usize) -> Option<usize> {
        self.rows.iter().position(|r| match r {
            Row::Hunk(_, h) => line == usize::MAX && *h == hunk,
            Row::Line {
                hunk: h, line: l, ..
            } => *h == hunk && *l == line,
            Row::Split {
                hunk: h, old, new, ..
            } => *h == hunk && (*old == Some(line) || *new == Some(line)),
            _ => false,
        })
    }

    /// Bring `rows` in line with the selected file and mode. Keys can arrive faster than redraws,
    /// so every handler calls this before reading `rows`, `starts` or `cursor`.
    fn sync_rows(&mut self) {
        if !self.diffs.is_empty() {
            self.ensure_rows();
        }
    }

    fn select_file(&mut self, i: usize, jump: Jump) {
        if i >= self.diffs.len() {
            return;
        }
        self.file = i;
        self.rows_for = None;
        self.jump = Some(jump);
        self.scroll = 0;
        self.hscroll = 0;
    }

    fn goto_row(&mut self, r: usize) {
        self.cursor = r;
        self.recenter = true;
    }

    fn has_changes(d: &FileDiff) -> bool {
        !d.hunks.is_empty()
    }

    /// Next change block in this file, or the first file after it that has changes.
    fn next_diff_target(&self) -> Option<Result<usize, usize>> {
        if let Some(&r) = self.starts.iter().find(|&&s| s > self.cursor) {
            return Some(Ok(r));
        }
        (self.file + 1..self.diffs.len())
            .find(|&i| Self::has_changes(&self.diffs[i]))
            .map(Err)
    }

    /// Previous change block in this file, or the last file before it that has changes.
    fn prev_diff_target(&self) -> Option<Result<usize, usize>> {
        if let Some(&r) = self.starts.iter().rev().find(|&&s| s < self.cursor) {
            return Some(Ok(r));
        }
        (0..self.file)
            .rev()
            .find(|&i| Self::has_changes(&self.diffs[i]))
            .map(Err)
    }

    /// Move to `target`: `Ok(row)` in this file, `Err(file)` into another file.
    fn go_diff(&mut self, target: Option<Result<usize, usize>>, jump: Jump) {
        match target {
            Some(Ok(r)) => self.goto_row(r),
            Some(Err(i)) => self.select_file(i, jump),
            None => {}
        }
    }

    /// Files shown in the list (after the filter), as indices into `diffs`.
    fn visible_files(&self) -> Vec<usize> {
        let q = self.filter.value().to_lowercase();
        (0..self.diffs.len())
            .filter(|&i| q.is_empty() || fuzzy_match(&self.diffs[i].path().to_lowercase(), &q))
            .collect()
    }

    /// The file `delta` steps away in the (filtered) list; `None` when the list is empty.
    fn file_step_target(&self, delta: isize) -> Option<usize> {
        let vis = self.visible_files();
        if vis.is_empty() {
            return None;
        }
        Some(match vis.iter().position(|&i| i == self.file) {
            Some(p) => vis[move_sel(p, delta, vis.len())],
            None => vis[0],
        })
    }

    /// Toast for a file step that cannot move further.
    fn file_step_outcome(&self, delta: isize) -> Option<String> {
        let next = self.file_step_target(delta)?;
        (next == self.file && self.visible_files().len() > 1)
            .then(|| if delta > 0 { "Last file" } else { "First file" }.to_string())
    }

    fn step_file(&mut self, delta: isize) -> Action {
        let msg = self.file_step_outcome(delta);
        if let Some(next) = self.file_step_target(delta) {
            if next != self.file {
                self.select_file(next, Jump::FirstChange);
            }
        }
        msg.map_or(Action::None, Action::Toast)
    }

    fn list_visible(&self) -> bool {
        self.show_files && self.diffs.len() > 1
    }

    // ----- commands ----------------------------------------------------------------------

    /// Whether options that reload the diff (whitespace, context) can change it. Already-parsed
    /// diffs cannot be regenerated with other options.
    fn reloadable(&self) -> bool {
        !matches!(self.source, DiffSource::Raw(_))
    }

    /// Context line count after `cmd` (`MoreContext` or `LessContext`).
    fn stepped_context(&self, cmd: Cmd) -> u32 {
        let c = self.opts.context;
        if cmd == Cmd::MoreContext {
            (c + 1).min(MAX_CONTEXT)
        } else {
            c.saturating_sub(1)
        }
    }

    /// The message `cmd` reports, computed from the state before it runs. Menu items use this to
    /// return their toast right away, since the command itself is applied on the next draw.
    fn outcome(&self, cmd: Cmd) -> Option<String> {
        let on_off = |b: bool| if b { "on" } else { "off" };
        let reload_option = matches!(
            cmd,
            Cmd::ToggleWhitespace | Cmd::MoreContext | Cmd::LessContext | Cmd::WholeFile
        );
        if reload_option && !self.reloadable() {
            return Some(NOT_RELOADABLE.into());
        }
        match cmd {
            Cmd::NextDiff => self
                .next_diff_target()
                .is_none()
                .then(|| "Last difference".into()),
            Cmd::PrevDiff => self
                .prev_diff_target()
                .is_none()
                .then(|| "First difference".into()),
            Cmd::NextFile => self.file_step_outcome(1),
            Cmd::PrevFile => self.file_step_outcome(-1),
            Cmd::ToggleSplit | Cmd::Reload => None,
            Cmd::ToggleWords => Some(format!(
                "Word-level highlighting {}",
                on_off(!self.flags.word_diff)
            )),
            Cmd::ToggleSyntax => Some(format!(
                "Syntax highlighting {}",
                on_off(!self.flags.syntax)
            )),
            Cmd::ToggleWhitespace => Some(format!(
                "Ignore whitespace {}",
                on_off(!self.opts.ignore_whitespace)
            )),
            Cmd::MoreContext | Cmd::LessContext if self.whole_file => {
                Some("Showing the whole file (press e to show changes only)".into())
            }
            Cmd::MoreContext | Cmd::LessContext => {
                Some(format!("Context: {} lines", self.stepped_context(cmd)))
            }
            Cmd::WholeFile => Some(
                if self.whole_file {
                    "Showing changes only"
                } else {
                    "Showing whole files"
                }
                .into(),
            ),
            Cmd::ToggleFiles => {
                (self.diffs.len() <= 1).then(|| "Only one file in this diff".into())
            }
        }
    }

    /// Run `cmd` and return its toast (if any).
    fn apply(&mut self, cmd: Cmd, ctx: &Ctx) -> Action {
        self.sync_rows();
        let msg = self.outcome(cmd);
        self.run(cmd, ctx);
        msg.map_or(Action::None, Action::Toast)
    }

    /// The state change of `cmd`, without feedback (see [`Self::outcome`]).
    fn run(&mut self, cmd: Cmd, ctx: &Ctx) {
        let reload_option = matches!(
            cmd,
            Cmd::ToggleWhitespace | Cmd::MoreContext | Cmd::LessContext | Cmd::WholeFile
        );
        if reload_option && !self.reloadable() {
            return;
        }
        match cmd {
            Cmd::NextDiff => self.go_diff(self.next_diff_target(), Jump::FirstChange),
            Cmd::PrevDiff => self.go_diff(self.prev_diff_target(), Jump::LastChange),
            Cmd::NextFile => {
                self.step_file(1);
            }
            Cmd::PrevFile => {
                self.step_file(-1);
            }
            Cmd::ToggleSplit => {
                self.jump = self.anchor();
                self.side_by_side = !self.side_by_side;
                self.rows_for = None;
                self.hscroll = 0;
            }
            Cmd::ToggleWords => self.flags.word_diff = !self.flags.word_diff,
            Cmd::ToggleSyntax => self.flags.syntax = !self.flags.syntax,
            Cmd::ToggleWhitespace => {
                self.opts.ignore_whitespace = !self.opts.ignore_whitespace;
                self.reload(ctx);
            }
            Cmd::MoreContext | Cmd::LessContext => {
                let c = self.stepped_context(cmd);
                if !self.whole_file && c != self.opts.context {
                    self.opts.context = c;
                    self.reload(ctx);
                }
            }
            Cmd::WholeFile => {
                self.whole_file = !self.whole_file;
                self.reload(ctx);
            }
            Cmd::ToggleFiles => {
                if self.diffs.len() > 1 {
                    self.show_files = !self.show_files;
                    self.focus = if self.show_files {
                        Focus::Files
                    } else {
                        Focus::Diff
                    };
                }
            }
            Cmd::Reload => self.reload(ctx),
        }
    }

    /// The file's path in the work tree and the new-side line at the caret.
    fn editor_target(&self, ctx: &Ctx) -> Result<(PathBuf, usize)> {
        let d = self
            .diffs
            .get(self.file)
            .ok_or_else(|| anyhow!("no file selected"))?;
        let path = match &self.source {
            DiffSource::Files { right, .. } => right.clone(),
            _ => {
                let p = d
                    .new_path
                    .as_deref()
                    .ok_or_else(|| anyhow!("{} was deleted; there is nothing to open", d.path()))?;
                ctx.repo.path(p)
            }
        };
        if !path.exists() {
            bail!("{} does not exist in the work tree", path.display());
        }
        Ok((path, self.caret_line()))
    }

    /// New-side line number at the caret (nearest one for removed lines and headers).
    /// Indices are checked, so a row that does not belong to the current file cannot panic.
    fn caret_line(&self) -> usize {
        let Some(d) = self.diffs.get(self.file) else {
            return 1;
        };
        let (hi, li) = match self.rows.get(self.cursor) {
            Some(Row::Line { hunk, line, .. }) => (*hunk, Some(*line)),
            Some(Row::Split { hunk, old, new, .. }) => (*hunk, new.or(*old)),
            Some(Row::Hunk(_, h)) => (*h, None),
            _ => {
                return d
                    .hunks
                    .first()
                    .map(|h| h.new_start.max(1) as usize)
                    .unwrap_or(1)
            }
        };
        let Some(h) = d.hunks.get(hi) else {
            return 1;
        };
        let n = li.and_then(|li| {
            let after = h.lines.get(li..).unwrap_or_default();
            let before = h.lines.get(..li.min(h.lines.len())).unwrap_or_default();
            after
                .iter()
                .find_map(|l| l.new_no)
                .or_else(|| before.iter().rev().find_map(|l| l.new_no))
        });
        n.unwrap_or(h.new_start).max(1) as usize
    }

    fn open_editor(&self, ctx: &Ctx) -> Action {
        match self.editor_target(ctx) {
            Ok((path, line)) => Action::edit(path, Some(line)),
            Err(e) => Action::error(format!("{e:#}")),
        }
    }

    /// Path and revision to annotate: the new side, or the old side for deleted files. `None`
    /// revision means the work tree. When the chosen side does not hold the file (for example a
    /// staged file later deleted from the work tree), HEAD is used if it has the file; otherwise
    /// there is nothing to annotate.
    fn blame_target(&self, ctx: &Ctx) -> Option<(String, Option<String>)> {
        if matches!(self.source, DiffSource::Files { .. }) {
            return None;
        }
        let d = self.diffs.get(self.file)?;
        let (path, rev) = match (&d.new_path, &d.old_path) {
            (Some(p), _) => (p.clone(), self.source.new_side_rev()),
            (None, Some(p)) => (p.clone(), self.source.old_side_rev()),
            _ => return None,
        };
        let has = |rev: Option<&str>| match rev {
            Some(r) => ctx
                .repo
                .run(&["cat-file", "-e", &format!("{r}:{path}")])
                .is_ok(),
            None => ctx.repo.path(&path).is_file(),
        };
        if has(rev.as_deref()) {
            return Some((path, rev));
        }
        (rev.as_deref() != Some("HEAD") && has(Some("HEAD")))
            .then(|| (path, Some("HEAD".to_string())))
    }

    fn blame(&self, ctx: &Ctx) -> Action {
        match self.blame_target(ctx) {
            Some((p, rev)) => crate::ui::views::files::open_blame(ctx, &p, rev.as_deref()),
            None => Action::Toast("Nothing to annotate here".into()),
        }
    }

    fn repo_path(&self) -> Option<String> {
        if matches!(self.source, DiffSource::Files { .. }) {
            return None;
        }
        self.diffs.get(self.file).map(|d| d.path().to_string())
    }

    /// A menu item that queues `c` for the next draw and shows the command's toast now.
    fn cmd_item(&self, label: impl Into<String>, key: &str, c: Cmd) -> MenuItem {
        let pending = self.pending.clone();
        let msg = self.outcome(c);
        MenuItem::new(label, key, move |_| {
            pending.borrow_mut().push(c);
            msg.map_or(Action::None, Action::Toast)
        })
    }

    fn menu(&self, ctx: &Ctx) -> Action {
        let on = |b: bool| if b { "on" } else { "off" };
        let ready = matches!(self.load, Load::Ready) && !self.diffs.is_empty();
        let reloadable = self.reloadable();
        let target = self.editor_target(ctx).ok();
        let blame = if ready { self.blame_target(ctx) } else { None };
        let can_blame = blame.is_some();
        let path = self.repo_path();
        let path2 = path.clone();
        let items = vec![
            self.cmd_item("Next difference", "n", Cmd::NextDiff)
                .enabled(ready),
            self.cmd_item("Previous difference", "N", Cmd::PrevDiff)
                .enabled(ready),
            self.cmd_item("Next file", "]", Cmd::NextFile)
                .enabled(self.diffs.len() > 1),
            self.cmd_item("Previous file", "[", Cmd::PrevFile)
                .enabled(self.diffs.len() > 1),
            self.cmd_item(
                if self.side_by_side {
                    "Unified view"
                } else {
                    "Side-by-side view"
                },
                "s",
                Cmd::ToggleSplit,
            ),
            self.cmd_item(
                format!("Word-level highlighting ({})", on(self.flags.word_diff)),
                "w",
                Cmd::ToggleWords,
            ),
            self.cmd_item(
                format!("Syntax highlighting ({})", on(self.flags.syntax)),
                "h",
                Cmd::ToggleSyntax,
            ),
            self.cmd_item(
                format!("Ignore whitespace ({})", on(self.opts.ignore_whitespace)),
                "W",
                Cmd::ToggleWhitespace,
            )
            .enabled(reloadable),
            self.cmd_item(
                format!("More context ({} lines)", self.opts.context),
                "+",
                Cmd::MoreContext,
            )
            .enabled(reloadable && !self.whole_file),
            self.cmd_item("Less context", "-", Cmd::LessContext)
                .enabled(reloadable && !self.whole_file),
            self.cmd_item(
                if self.whole_file {
                    "Show changes only"
                } else {
                    "Show whole file"
                },
                "e",
                Cmd::WholeFile,
            )
            .enabled(reloadable),
            self.cmd_item(
                if self.show_files {
                    "Hide file list"
                } else {
                    "Show file list"
                },
                "f",
                Cmd::ToggleFiles,
            )
            .enabled(self.diffs.len() > 1),
            self.cmd_item("Reload", "r", Cmd::Reload),
            MenuItem::new("Open in editor", "o", move |_| match target {
                Some((p, line)) => Action::edit(p, Some(line)),
                None => Action::Toast("This file is not in the work tree".into()),
            })
            .enabled(ready),
            MenuItem::new("Annotate (blame)", "b", move |ctx| match blame {
                Some((p, rev)) => crate::ui::views::files::open_blame(ctx, &p, rev.as_deref()),
                None => Action::None,
            })
            .enabled(can_blame),
            MenuItem::new("Show file history", "H", move |ctx| match path {
                Some(p) => crate::ui::views::files::open_file_history(ctx, &p),
                None => Action::None,
            })
            .enabled(ready && self.repo_path().is_some()),
            MenuItem::new("Show local history", "L", move |ctx| match path2 {
                Some(p) => crate::ui::views::local_history::open_for_path(ctx, &p),
                None => Action::None,
            })
            .enabled(ready && self.repo_path().is_some()),
        ];
        Action::popup(MenuPopup::new("Diff actions", items))
    }

    // ----- keys --------------------------------------------------------------------------

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filter.set("");
                self.filtering = false;
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Up => return self.step_file(-1),
            KeyCode::Down => return self.step_file(1),
            _ => {
                if self.filter.handle_key(&key) {
                    let vis = self.visible_files();
                    if !vis.contains(&self.file) {
                        if let Some(&first) = vis.first() {
                            self.select_file(first, Jump::FirstChange);
                        }
                    }
                }
            }
        }
        Action::None
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Action {
        let page = self.list_rect.height.saturating_sub(2).max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.step_file(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step_file(1),
            KeyCode::PageUp => self.step_file(-page),
            KeyCode::PageDown => self.step_file(page),
            KeyCode::Home | KeyCode::Char('g') => self.step_file(-(self.diffs.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.step_file(self.diffs.len() as isize),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = Focus::Diff;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_diff_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let page = self.height.max(1);
        let len = self.rows.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = move_sel(self.cursor, 1, len),
            KeyCode::Char('u') if ctrl => self.cursor = self.cursor.saturating_sub(page / 2),
            KeyCode::Char('d') if ctrl => {
                self.cursor = move_sel(self.cursor, (page / 2) as isize, len)
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(page);
                self.scroll = self.scroll.saturating_sub(page);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.cursor = move_sel(self.cursor, page as isize, len);
                self.scroll = (self.scroll + page).min(len.saturating_sub(page));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.cursor = 0;
                self.hscroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => self.cursor = len.saturating_sub(1),
            KeyCode::Left => self.hscroll = self.hscroll.saturating_sub(HSCROLL_STEP),
            KeyCode::Right => self.hscroll = self.hscroll.saturating_add(HSCROLL_STEP),
            KeyCode::Enter => return self.open_editor(ctx),
            _ => {}
        }
        Action::None
    }

    // ----- drawing -----------------------------------------------------------------------

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.visible_files();
        let focused = self.focus == Focus::Files;
        let title = format!(" Files {}/{} ", vis.len(), self.diffs.len());
        let block = panel(title, focused, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let show_filter = self.filtering || !self.filter.is_empty();
        let [filter_area, list_area] = Layout::vertical([
            Constraint::Length(u16::from(show_filter)),
            Constraint::Min(0),
        ])
        .areas(inner);
        if show_filter {
            let [p, input] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(filter_area);
            f.render_widget(Span::styled("/ ", t.key()), p);
            self.filter.render(f, input, t.text(), self.filtering);
        }
        self.list_rect = list_area;
        let h = list_area.height as usize;
        let sel = vis.iter().position(|&i| i == self.file);
        if let Some(s) = sel {
            self.list_scroll = scroll_to_keep_visible(s, self.list_scroll, h);
        }
        self.list_scroll = self.list_scroll.min(vis.len().saturating_sub(h));
        let w = list_area.width as usize;
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.list_scroll)
            .take(h)
            .map(|(pos, &i)| self.list_line(&self.diffs[i], w, Some(pos) == sel, focused, ctx))
            .collect();
        if lines.is_empty() {
            f.render_widget(
                Paragraph::new("No matching files").style(t.dimmed()),
                list_area,
            );
        } else {
            f.render_widget(Paragraph::new(lines), list_area);
        }
    }

    fn list_line(
        &self,
        d: &FileDiff,
        width: usize,
        selected: bool,
        focused: bool,
        ctx: &Ctx,
    ) -> Line<'static> {
        let t = &ctx.theme;
        let color = match d.kind {
            ChangeKind::Added => t.added,
            ChangeKind::Deleted => t.removed,
            ChangeKind::Renamed | ChangeKind::Copied => t.renamed,
            ChangeKind::Modified | ChangeKind::ModeChanged => t.modified,
        };
        let path = d.path();
        let (dir, name) = match path.rsplit_once('/') {
            Some((dir, name)) => (format!(" {dir}"), name.to_string()),
            None => (String::new(), path.to_string()),
        };
        let stats = if d.binary {
            " bin".to_string()
        } else {
            format!(" +{} -{}", d.added_count(), d.removed_count())
        };
        let fixed = 2 + unicode_width::UnicodeWidthStr::width(stats.as_str());
        let avail = width.saturating_sub(fixed);
        let name = util::truncate(&name, avail);
        let dir = util::truncate(
            &dir,
            avail.saturating_sub(unicode_width::UnicodeWidthStr::width(name.as_str())),
        );
        let used = fixed
            + unicode_width::UnicodeWidthStr::width(name.as_str())
            + unicode_width::UnicodeWidthStr::width(dir.as_str());
        let mut line = Line::from(vec![
            Span::styled(
                format!("{} ", model::kind_letter(d.kind)),
                Style::default().fg(color),
            ),
            Span::styled(name, Style::default().fg(color)),
            Span::styled(dir, t.dimmed()),
            Span::raw(" ".repeat(width.saturating_sub(used))),
            Span::styled(stats, t.dimmed()),
        ]);
        if selected {
            line = line.style(if focused {
                t.selected()
            } else {
                Style::default().bg(t.selection_bg)
            });
        }
        line
    }

    fn pane_title(&self, ctx: &Ctx) -> Line<'static> {
        let t = &ctx.theme;
        let mut spans = vec![Span::styled(format!(" {} ", self.title), t.title())];
        if let Some(d) = self.diffs.get(self.file) {
            spans.push(Span::styled("· ", t.dimmed()));
            if let (Some(o), Some(n), ChangeKind::Renamed | ChangeKind::Copied) =
                (&d.old_path, &d.new_path, d.kind)
            {
                spans.push(Span::styled(format!("{o} → "), t.dimmed()));
                spans.push(Span::styled(n.clone(), t.text()));
            } else {
                spans.push(Span::styled(d.path().to_string(), t.text()));
            }
            spans.push(Span::styled(
                format!("  {}", model::kind_label(d.kind)),
                t.dimmed(),
            ));
            spans.extend(render::counts(d, ctx));
            spans.push(Span::raw(" "));
        }
        Line::from(spans)
    }

    fn status_line(&self, ctx: &Ctx) -> Line<'static> {
        let t = &ctx.theme;
        let mut parts: Vec<String> = Vec::new();
        if !self.starts.is_empty() {
            let k = self
                .starts
                .iter()
                .filter(|&&s| s <= self.cursor)
                .count()
                .max(1);
            parts.push(format!("change {k}/{}", self.starts.len()));
        }
        if self.diffs.len() > 1 {
            parts.push(format!("file {}/{}", self.file + 1, self.diffs.len()));
        }
        let (l, r) = self.source.side_labels();
        parts.push(format!("{l} → {r}"));
        parts.push(
            if self.side_by_side {
                "side-by-side"
            } else {
                "unified"
            }
            .into(),
        );
        parts.push(if self.whole_file {
            "whole file".into()
        } else {
            format!("context {}", self.opts.context)
        });
        if self.opts.ignore_whitespace {
            parts.push("whitespace ignored".into());
        }
        if self.hscroll > 0 {
            parts.push(format!("col {}", self.hscroll + 1));
        }
        Line::from(Span::styled(format!(" {} ", parts.join(" · ")), t.dimmed())).right_aligned()
    }

    fn draw_diff_pane(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let focused = self.focus == Focus::Diff || !self.list_visible();
        let block = panel(self.pane_title(ctx), focused, t).title_bottom(self.status_line(ctx));
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.diff_rect = inner;
        match &self.load {
            Load::Loading if self.diffs.is_empty() => {
                f.render_widget(Paragraph::new("Loading…").style(t.dimmed()), inner);
                return;
            }
            Load::Failed(e) => {
                let text = vec![
                    Line::styled(e.clone(), Style::default().fg(t.error)),
                    Line::raw(""),
                    Line::styled("Press r to retry, Esc to close.", t.dimmed()),
                ];
                f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
                return;
            }
            _ => {}
        }
        if self.diffs.is_empty() {
            let mut msg = "No differences".to_string();
            if self.opts.ignore_whitespace {
                msg.push_str(" (whitespace ignored — press W to show them)");
            }
            let y = inner.y + inner.height / 2;
            let r = Rect {
                y,
                height: 1.min(inner.height),
                ..inner
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()).centered(), r);
            return;
        }
        self.ensure_rows();
        let body = if self.side_by_side {
            self.draw_side_headers(f, inner, ctx)
        } else {
            inner
        };
        self.height = body.height as usize;
        let h = self.height.max(1);
        if self.recenter {
            self.scroll = self.cursor.saturating_sub(h / 3);
            self.recenter = false;
        } else {
            self.scroll = scroll_to_keep_visible(self.cursor, self.scroll, h);
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
        self.cache.begin_frame();
        let vp = Viewport {
            scroll: self.scroll,
            hscroll: self.hscroll,
            cursor: Some(self.cursor),
        };
        let mut p = Painter {
            diffs: &self.diffs,
            cache: &mut self.cache,
            ctx,
            flags: self.flags,
        };
        if self.side_by_side {
            p.split(f, body, &self.rows, vp);
        } else {
            p.unified(f, body, &self.rows, vp);
        }
    }

    /// Column titles above the two sides; returns the remaining area.
    fn draw_side_headers(&self, f: &mut Frame, inner: Rect, ctx: &Ctx) -> Rect {
        let t = &ctx.theme;
        let [head, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let lw = head.width.saturating_sub(1) / 2;
        let [l, s, r] = Layout::horizontal([
            Constraint::Length(lw),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(head);
        let (ll, rl) = self.source.side_labels();
        let d = self.diffs.get(self.file);
        let side = |label: String, path: Option<&String>| {
            Line::from(vec![
                Span::styled(format!(" {label}"), t.title()),
                Span::styled(
                    path.map(|p| format!("  {p}"))
                        .unwrap_or_else(|| "  (none)".into()),
                    t.dimmed(),
                ),
            ])
        };
        f.render_widget(
            Paragraph::new(side(ll, d.and_then(|d| d.old_path.as_ref()))),
            l,
        );
        f.render_widget(Paragraph::new("│").style(t.border(false)), s);
        f.render_widget(
            Paragraph::new(side(rl, d.and_then(|d| d.new_path.as_ref()))),
            r,
        );
        body
    }
}

impl Popup for DiffViewer {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if !self.started {
            self.start(ctx);
        }
        let cmds: Vec<Cmd> = self.pending.borrow_mut().drain(..).collect();
        for c in cmds {
            // The menu item already returned this command's toast (see `cmd_item`).
            self.sync_rows();
            self.run(c, ctx);
        }
        self.poll();
        f.render_widget(Clear, area);
        if self.list_visible() {
            let w = (area.width / 4).clamp(24, 48).min(area.width / 2);
            let [list, diff] =
                Layout::horizontal([Constraint::Length(w), Constraint::Min(1)]).areas(area);
            self.draw_list(f, list, ctx);
            self.draw_diff_pane(f, diff, ctx);
        } else {
            self.list_rect = Rect::default();
            self.draw_diff_pane(f, area, ctx);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        self.sync_rows();
        if self.filtering {
            return self.handle_filter_key(key);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let cmd = match key.code {
            KeyCode::Char('n') => Some(Cmd::NextDiff),
            KeyCode::Char('N') => Some(Cmd::PrevDiff),
            KeyCode::F(7) if shift => Some(Cmd::PrevDiff),
            KeyCode::F(7) => Some(Cmd::NextDiff),
            KeyCode::Char(']') => Some(Cmd::NextFile),
            KeyCode::Char('[') => Some(Cmd::PrevFile),
            KeyCode::Char('s') => Some(Cmd::ToggleSplit),
            KeyCode::Char('w') => Some(Cmd::ToggleWords),
            KeyCode::Char('h') => Some(Cmd::ToggleSyntax),
            KeyCode::Char('W') => Some(Cmd::ToggleWhitespace),
            KeyCode::Char('+') | KeyCode::Char('=') => Some(Cmd::MoreContext),
            KeyCode::Char('-') => Some(Cmd::LessContext),
            KeyCode::Char('e') => Some(Cmd::WholeFile),
            KeyCode::Char('f') => Some(Cmd::ToggleFiles),
            KeyCode::Char('r') | KeyCode::F(5) => Some(Cmd::Reload),
            _ => None,
        };
        let ready = matches!(self.load, Load::Ready) && !self.diffs.is_empty();
        if let Some(c) = cmd {
            let needs_diffs = matches!(
                c,
                Cmd::NextDiff | Cmd::PrevDiff | Cmd::NextFile | Cmd::PrevFile
            );
            return if needs_diffs && !ready {
                Action::None
            } else {
                self.apply(c, ctx)
            };
        }
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.set("");
                Action::None
            }
            KeyCode::Esc | KeyCode::Char('q') => Action::ClosePopup,
            KeyCode::Char('.') | KeyCode::Char('m') => self.menu(ctx),
            KeyCode::Tab | KeyCode::BackTab => {
                if self.list_visible() {
                    self.focus = if self.focus == Focus::Files {
                        Focus::Diff
                    } else {
                        Focus::Files
                    };
                }
                Action::None
            }
            KeyCode::Char('/') if self.diffs.len() > 1 => {
                self.show_files = true;
                self.focus = Focus::Files;
                self.filtering = true;
                Action::None
            }
            _ if !ready => Action::None,
            KeyCode::Char('o') | KeyCode::F(4) => self.open_editor(ctx),
            KeyCode::Char('b') => self.blame(ctx),
            KeyCode::Char('H') => match self.repo_path() {
                Some(p) => crate::ui::views::files::open_file_history(ctx, &p),
                None => Action::None,
            },
            KeyCode::Char('L') => match self.repo_path() {
                Some(p) => crate::ui::views::local_history::open_for_path(ctx, &p),
                None => Action::None,
            },
            _ if self.focus == Focus::Files && self.list_visible() => self.handle_list_key(key),
            _ => self.handle_diff_key(key, ctx),
        }
    }

    fn handle_mouse(&mut self, ev: MouseEvent, _ctx: &Ctx) -> Action {
        self.sync_rows();
        let inside = |r: Rect| {
            ev.column >= r.x
                && ev.column < r.x + r.width
                && ev.row >= r.y
                && ev.row < r.y + r.height
        };
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) if inside(self.list_rect) => {
                self.focus = Focus::Files;
                let vis = self.visible_files();
                let pos = self.list_scroll + (ev.row - self.list_rect.y) as usize;
                if let Some(&i) = vis.get(pos) {
                    if i != self.file {
                        self.select_file(i, Jump::FirstChange);
                    }
                }
                // Non-None so the app does not translate the event into keys.
                Action::Batch(Vec::new())
            }
            MouseEventKind::Down(MouseButton::Left) if inside(self.diff_rect) => {
                self.focus = Focus::Diff;
                let header = u16::from(self.side_by_side);
                if let Some(off) = (ev.row - self.diff_rect.y).checked_sub(header) {
                    self.cursor =
                        (self.scroll + off as usize).min(self.rows.len().saturating_sub(1));
                }
                Action::Batch(Vec::new())
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if inside(self.diff_rect) && self.focus == Focus::Files =>
            {
                self.focus = Focus::Diff;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn refresh(&mut self, ctx: &Ctx) {
        if self.started {
            self.reload(ctx);
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("n/N F7", "next/prev change"),
            ("]/[", "next/prev file"),
            ("s", "side-by-side"),
            ("w", "word diff"),
            ("h", "syntax colors"),
            ("W", "ignore whitespace"),
            ("+/-", "context lines"),
            ("e", "whole file"),
            ("o/F4", "open in editor"),
            ("f", "file list"),
            ("Tab", "switch pane"),
            ("/", "filter files"),
            ("←/→", "scroll sideways"),
            ("j/k PgUp/PgDn", "move"),
            ("g/G", "top/bottom"),
            ("b", "annotate"),
            ("H", "file history"),
            ("L", "local history"),
            ("r", "reload"),
            (". / m", "all actions"),
            ("Esc/q", "close"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn setup() -> (TempDir, Ctx) {
        let (d, repo) = temp_repo();
        let base: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        commit_files(&repo, &[("a.rs", &base), ("b.txt", "one\ntwo\n")], "base");
        let changed = base
            .replace("line 3\n", "line three\n")
            .replace("line 30\n", "line thirty\n");
        write(&repo, "a.rs", &changed);
        write(&repo, "b.txt", "one\n2\n");
        (d, Ctx::new(repo, Config::default()))
    }

    fn draw(v: &mut DiffViewer, term: &mut Terminal<TestBackend>, ctx: &Ctx) {
        term.draw(|f| v.draw(f, f.area(), ctx)).unwrap();
    }

    fn wait_ready(v: &mut DiffViewer, term: &mut Terminal<TestBackend>, ctx: &Ctx) {
        for _ in 0..500 {
            draw(v, term, ctx);
            if matches!(v.load, Load::Ready) {
                draw(v, term, ctx);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("diff did not load");
    }

    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn navigates_changes_across_files() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        assert_eq!(v.diffs.len(), 2);
        assert_eq!(v.file, 0);
        let first = v.cursor;
        assert_eq!(Some(first), v.starts.first().copied());
        assert!(v.handle_key(key(KeyCode::Char('n')), &ctx).is_none());
        assert!(v.cursor > first);
        // Past the last change of a.rs jumps into b.txt.
        v.handle_key(key(KeyCode::F(7)), &ctx);
        draw(&mut v, &mut term, &ctx);
        assert_eq!(v.file, 1);
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('n')), &ctx),
            Action::Toast(_)
        ));
        // Back again lands on the last change of a.rs.
        v.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::SHIFT), &ctx);
        draw(&mut v, &mut term, &ctx);
        assert_eq!(v.file, 0);
        assert_eq!(Some(v.cursor), v.starts.last().copied());
        assert!(screen(&term).contains("line thirty"));
    }

    #[test]
    fn toggles_modes_and_keeps_position() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        v.handle_key(key(KeyCode::Char('n')), &ctx);
        draw(&mut v, &mut term, &ctx);
        v.handle_key(key(KeyCode::Char('s')), &ctx);
        draw(&mut v, &mut term, &ctx);
        assert!(v.side_by_side);
        let s = screen(&term);
        assert!(s.contains("Index"), "{s}");
        assert!(s.contains("Working tree"));
        assert!(matches!(v.rows.get(v.cursor), Some(Row::Split { .. })));
        // Whole-file context reloads with every line.
        v.handle_key(key(KeyCode::Char('e')), &ctx);
        wait_ready(&mut v, &mut term, &ctx);
        assert!(v.rows.len() > 40);
        v.handle_key(key(KeyCode::Char('e')), &ctx);
        v.handle_key(key(KeyCode::Char('-')), &ctx);
        wait_ready(&mut v, &mut term, &ctx);
        assert_eq!(v.opts.context, 2);
    }

    #[test]
    fn file_list_filter_and_switch() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        assert!(screen(&term).contains("Files 2/2"));
        v.handle_key(key(KeyCode::Char('/')), &ctx);
        v.handle_key(key(KeyCode::Char('b')), &ctx);
        v.handle_key(key(KeyCode::Enter), &ctx);
        draw(&mut v, &mut term, &ctx);
        assert_eq!(v.file, 1);
        assert!(screen(&term).contains("Files 1/2"));
        v.handle_key(key(KeyCode::Esc), &ctx);
        assert!(v.filter.is_empty());
        v.handle_key(key(KeyCode::Char('[')), &ctx);
        draw(&mut v, &mut term, &ctx);
        assert_eq!(v.file, 0);
        v.handle_key(key(KeyCode::Char('f')), &ctx);
        draw(&mut v, &mut term, &ctx);
        assert!(!screen(&term).contains("Files 2/2"));
        assert!(matches!(
            v.handle_key(key(KeyCode::Esc), &ctx),
            Action::ClosePopup
        ));
    }

    #[test]
    fn caret_line_and_editor_target() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        // Caret on the first change of a.rs: "-line 3" maps to new line 3.
        let (path, line) = v.editor_target(&ctx).unwrap();
        assert!(path.ends_with("a.rs"));
        assert_eq!(line, 3);
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('o')), &ctx),
            Action::Edit { .. }
        ));
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('.')), &ctx),
            Action::Popup(_)
        ));
    }

    #[test]
    fn empty_error_and_raw_states() {
        let (_d, ctx) = setup();
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut v = DiffViewer::new("Staged".into(), DiffSource::Staged { paths: Vec::new() });
        wait_ready(&mut v, &mut term, &ctx);
        assert!(screen(&term).contains("No differences"));
        let mut v = DiffViewer::new(
            "Bad".into(),
            DiffSource::Commit {
                rev: "nope".into(),
                paths: Vec::new(),
            },
        );
        for _ in 0..500 {
            draw(&mut v, &mut term, &ctx);
            if matches!(v.load, Load::Failed(_)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        draw(&mut v, &mut term, &ctx);
        assert!(screen(&term).contains("retry"));
        let raw = crate::git::diff::parse_unified(
            "diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n",
        );
        let mut v = DiffViewer::new("Raw".into(), DiffSource::Raw(raw));
        draw(&mut v, &mut term, &ctx);
        draw(&mut v, &mut term, &ctx);
        assert!(screen(&term).contains("binary file"));
    }

    #[test]
    fn menu_commands_apply_on_next_draw() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        v.pending.borrow_mut().push(Cmd::ToggleSplit);
        draw(&mut v, &mut term, &ctx);
        assert!(v.side_by_side);
    }

    #[test]
    fn menu_items_return_their_toast() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        assert!(v.flags.word_diff);
        let item = v.cmd_item("Word-level", "w", Cmd::ToggleWords);
        match (item.run)(&ctx) {
            Action::Toast(t) => assert_eq!(t, "Word-level highlighting off"),
            other => panic!("expected a toast, got {:?}", std::mem::discriminant(&other)),
        }
        assert!(v.flags.word_diff, "applied only on the next draw");
        draw(&mut v, &mut term, &ctx);
        assert!(!v.flags.word_diff);
        // Keys and menu agree on the message.
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('w')), &ctx),
            Action::Toast(t) if t == "Word-level highlighting on"
        ));
    }

    #[test]
    fn keys_faster_than_redraws_do_not_panic() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        // Caret deep inside a.rs's first hunk, then switch files and act with no draw between.
        v.handle_key(key(KeyCode::Char('n')), &ctx);
        v.handle_key(key(KeyCode::Char('j')), &ctx);
        v.handle_key(key(KeyCode::Char('j')), &ctx);
        v.handle_key(key(KeyCode::Char(']')), &ctx);
        assert_eq!(v.file, 1);
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('o')), &ctx),
            Action::Edit { .. }
        ));
        assert_eq!(v.editor_target(&ctx).unwrap().1, 2);
        v.handle_key(key(KeyCode::Char('[')), &ctx);
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('.')), &ctx),
            Action::Popup(_)
        ));
        v.handle_key(key(KeyCode::Char(']')), &ctx);
        assert!(matches!(
            v.handle_key(key(KeyCode::F(4)), &ctx),
            Action::Edit { .. }
        ));
        v.handle_key(key(KeyCode::Char('N')), &ctx);
        v.handle_key(key(KeyCode::Char('s')), &ctx);
        assert!(matches!(
            v.handle_key(key(KeyCode::Enter), &ctx),
            Action::Edit { .. }
        ));
    }

    #[test]
    fn caret_line_survives_stale_rows() {
        let (_d, ctx) = setup();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        wait_ready(&mut v, &mut term, &ctx);
        v.rows = vec![Row::Line {
            file: 0,
            hunk: 0,
            line: 10_000,
        }];
        v.cursor = 0;
        assert!(v.caret_line() >= 1);
    }

    #[test]
    fn raw_diffs_refuse_reload_options() {
        let (_d, ctx) = setup();
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let raw = crate::git::diff::parse_unified(
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n",
        );
        let mut v = DiffViewer::new("Raw".into(), DiffSource::Raw(raw));
        draw(&mut v, &mut term, &ctx);
        let context = v.opts.context;
        for c in ['W', '+', '-', 'e'] {
            assert!(matches!(
                v.handle_key(key(KeyCode::Char(c)), &ctx),
                Action::Toast(t) if t == NOT_RELOADABLE
            ));
        }
        assert!(!v.opts.ignore_whitespace);
        assert!(!v.whole_file);
        assert_eq!(v.opts.context, context);
        assert!(v.outcome(Cmd::ToggleWords).is_some());
    }

    #[test]
    fn blame_targets_existing_versions() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("gone.txt", "x\n"), ("keep.txt", "1\n")], "base");
        std::fs::remove_file(repo.path("gone.txt")).unwrap();
        write(&repo, "keep.txt", "2\n");
        let ctx = Ctx::new(repo.clone(), Config::default());
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut v = DiffViewer::new("Local".into(), DiffSource::Unstaged { paths: Vec::new() });
        wait_ready(&mut v, &mut term, &ctx);
        let gone = v.diffs.iter().position(|d| d.path() == "gone.txt").unwrap();
        v.select_file(gone, Jump::FirstChange);
        assert_eq!(
            v.blame_target(&ctx),
            Some(("gone.txt".to_string(), Some("HEAD".to_string())))
        );
        let keep = v.diffs.iter().position(|d| d.path() == "keep.txt").unwrap();
        v.select_file(keep, Jump::FirstChange);
        assert_eq!(v.blame_target(&ctx), Some(("keep.txt".to_string(), None)));
        // Staged new file later deleted from the work tree: nothing holds it.
        write(&repo, "new.txt", "n\n");
        repo.run(&["add", "new.txt"]).unwrap();
        std::fs::remove_file(repo.path("new.txt")).unwrap();
        let mut v = DiffViewer::new("Staged".into(), DiffSource::Staged { paths: Vec::new() });
        wait_ready(&mut v, &mut term, &ctx);
        let i = v.diffs.iter().position(|d| d.path() == "new.txt").unwrap();
        v.select_file(i, Jump::FirstChange);
        assert_eq!(v.blame_target(&ctx), None);
        assert!(matches!(v.blame(&ctx), Action::Toast(_)));
    }
}
