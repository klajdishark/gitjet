//! Local history of file snapshots (tab 8), plus the per-file history popup.
//!
//! Left: files with snapshots (most recent change first). Right: that file's snapshots and a diff of
//! the selected snapshot against the current file. `t` switches to a timeline of all snapshots.

mod browser;

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use self::browser::{
    full_diff_action, label_action, revert_action, snapshot_now_action, SnapshotPane,
};
use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::local_history::{self, HistoryFile, Snapshot};
use crate::ui::popups::{InputPopup, MenuItem, MenuPopup};
use crate::ui::views::Tab;
use crate::ui::widgets::{centered_pct, move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, PaletteItem, Popup, View};
use crate::util::{relative_time, truncate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// File list, or the timeline list in timeline mode
    List,
    Snapshots,
    Diff,
}

pub struct LocalHistoryView {
    files: Vec<HistoryFile>,
    /// Whole index, newest first
    all: Vec<Snapshot>,
    stamp: Option<(u64, SystemTime)>,
    loaded: bool,
    file_sel: usize,
    file_offset: usize,
    filter: TextInput,
    filtering: bool,
    timeline: bool,
    focus: Focus,
    pane: SnapshotPane,
    error: Option<String>,
    /// Set by the context menu's "toggle timeline" item; applied on the next draw
    toggle_requested: Rc<Cell<bool>>,
    /// When the index stamp was last compared in `draw` (background snapshots are silent tasks)
    last_check: Option<Instant>,
}

/// How often `draw` checks whether background snapshots changed the index.
const STAMP_CHECK_EVERY: Duration = Duration::from_secs(1);

/// Rows of the snapshot list in files mode: room for the entries plus borders, at least 5 rows,
/// at most half of the available height (never more than `height`, never panicking).
fn snapshot_rows(entries: usize, height: u16) -> u16 {
    let max = (height / 2).max(1).min(height);
    let wanted = u16::try_from(entries).unwrap_or(u16::MAX).saturating_add(2);
    wanted.max(5).min(max)
}

impl LocalHistoryView {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            files: Vec::new(),
            all: Vec::new(),
            stamp: None,
            loaded: false,
            file_sel: 0,
            file_offset: 0,
            filter: TextInput::default(),
            filtering: false,
            timeline: false,
            focus: Focus::List,
            pane: SnapshotPane::default(),
            error: None,
            toggle_requested: Rc::default(),
            last_check: None,
        }
    }

    /// Reload when the index changed since the last load (cheap stat, throttled).
    fn reload_if_changed(&mut self, ctx: &Ctx) {
        if !self.loaded
            || self
                .last_check
                .is_some_and(|t| t.elapsed() < STAMP_CHECK_EVERY)
        {
            return;
        }
        self.last_check = Some(Instant::now());
        if local_history::index_stamp(&ctx.repo) != self.stamp {
            self.pane.invalidate();
            self.load(ctx);
        }
    }

    /// Read the index (unless unchanged since the last load) keeping the selected file.
    fn load(&mut self, ctx: &Ctx) {
        let stamp = local_history::index_stamp(&ctx.repo);
        if self.loaded && stamp == self.stamp {
            return;
        }
        let keep = self.selected_file().map(|f| f.path.clone());
        match local_history::all_snapshots(&ctx.repo)
            .and_then(|all| Ok((local_history::list_files(&ctx.repo)?, all)))
        {
            Ok((files, all)) => {
                self.files = files;
                self.all = all;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        self.stamp = stamp;
        self.loaded = true;
        if let Some(p) = keep {
            if let Some(pos) = self
                .visible_files()
                .iter()
                .position(|&i| self.files[i].path == p)
            {
                self.file_sel = pos;
            }
        }
        self.file_sel = self
            .file_sel
            .min(self.visible_files().len().saturating_sub(1));
        self.sync_pane();
    }

    fn query(&self) -> String {
        self.filter.value().to_lowercase()
    }

    /// Files matching the filter (indices into `files`).
    fn visible_files(&self) -> Vec<usize> {
        let q = self.query();
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| q.is_empty() || f.path.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_file(&self) -> Option<&HistoryFile> {
        self.visible_files()
            .get(self.file_sel)
            .map(|&i| &self.files[i])
    }

    /// Fill the snapshot pane from the selected file (or the filtered timeline).
    fn sync_pane(&mut self) {
        let snaps: Vec<Snapshot> = if self.timeline {
            let q = self.query();
            self.all
                .iter()
                .filter(|s| {
                    q.is_empty()
                        || s.path.to_lowercase().contains(&q)
                        || s.label
                            .as_deref()
                            .is_some_and(|l| l.to_lowercase().contains(&q))
                })
                .cloned()
                .collect()
        } else {
            match self.selected_file() {
                Some(f) => {
                    let p = f.path.clone();
                    self.all.iter().filter(|s| s.path == p).cloned().collect()
                }
                None => Vec::new(),
            }
        };
        let same_file = !self.timeline
            && self.pane.selected().map(|s| s.path.as_str())
                == snaps.first().map(|s| s.path.as_str());
        if !same_file && !self.timeline {
            self.pane = SnapshotPane::default();
        }
        self.pane.set(snaps);
    }

    fn move_files(&mut self, delta: isize) {
        let n = self.visible_files().len();
        self.file_sel = move_sel(self.file_sel, delta, n);
        self.sync_pane();
    }

    fn list_len(&self) -> usize {
        if self.timeline {
            self.pane.snaps.len()
        } else {
            self.visible_files().len()
        }
    }

    fn move_focused(&mut self, delta: isize) {
        match self.focus {
            Focus::List if !self.timeline => self.move_files(delta),
            Focus::List | Focus::Snapshots => self.pane.move_by(delta),
            Focus::Diff => self.pane.scroll_diff(delta),
        }
    }

    fn focus_order(&self) -> &'static [Focus] {
        if self.timeline {
            &[Focus::List, Focus::Diff]
        } else {
            &[Focus::List, Focus::Snapshots, Focus::Diff]
        }
    }

    fn cycle_focus(&mut self, back: bool) {
        let order = self.focus_order();
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if back { (i + n - 1) % n } else { (i + 1) % n }];
    }

    fn toggle_timeline(&mut self) {
        self.timeline = !self.timeline;
        self.focus = Focus::List;
        self.pane = SnapshotPane::default();
        self.sync_pane();
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filter.set("");
                self.filtering = false;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.filtering = false,
            _ => {
                if self.filter.handle_key(&key) {
                    self.file_sel = 0;
                    self.pane.sel = 0;
                }
            }
        }
        self.sync_pane();
        Action::None
    }

    fn context_menu(&self, ctx: &Ctx) -> Action {
        let snap = self.pane.selected().cloned();
        let path = snap
            .as_ref()
            .filter(|s| !s.is_marker())
            .map(|s| s.path.clone())
            .or_else(|| {
                (!self.timeline)
                    .then(|| self.selected_file().map(|f| f.path.clone()))
                    .flatten()
            });
        let has_snap = snap.as_ref().is_some_and(|s| !s.is_marker());
        let s1 = snap.clone();
        let p1 = path.clone();
        let p2 = path.clone();
        let p3 = path.clone();
        let toggle = self.toggle_requested.clone();
        let items = vec![
            MenuItem::new(
                "Show diff with current file",
                "Enter",
                move |ctx| match s1 {
                    Some(s) => {
                        let mut pane = SnapshotPane::default();
                        pane.set(vec![s]);
                        full_diff_action(&mut pane, ctx)
                    }
                    None => Action::None,
                },
            )
            .enabled(has_snap),
            MenuItem::new("Revert to this snapshot…", "R", move |_| match snap {
                Some(s) => revert_action(&s),
                None => Action::None,
            })
            .enabled(has_snap),
            MenuItem::new("Put label…", "l", |_| label_action()),
            MenuItem::new("Snapshot changed files now", "s", snapshot_now_action),
            MenuItem::new("Prune old snapshots", "P", prune_action),
            MenuItem::new(
                if self.timeline {
                    "Show files"
                } else {
                    "Show timeline (all files)"
                },
                "t",
                move |_| {
                    toggle.set(true);
                    Action::None
                },
            ),
            MenuItem::new("Git history of this file", "h", move |ctx| match p1 {
                Some(p) => crate::ui::views::files::open_file_history(ctx, &p),
                None => Action::None,
            })
            .enabled(path.is_some()),
            MenuItem::new("Annotate (blame) this file", "b", move |ctx| match p2 {
                Some(p) => crate::ui::views::files::open_blame(ctx, &p, None),
                None => Action::None,
            })
            .enabled(path.is_some()),
            MenuItem::new("Open file in editor", "e", move |ctx| match p3 {
                Some(p) => Action::edit(ctx.repo.path(&p), None),
                None => Action::None,
            })
            .enabled(path.as_ref().is_some_and(|p| ctx.repo.path(p).is_file())),
        ];
        Action::popup(MenuPopup::new("Local history", items))
    }

    fn draw_files(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.visible_files();
        let title = format!(" Files ({}) ", vis.len());
        let block = panel(title, self.focus == Focus::List, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [filter_area, list] = filter_layout(inner, self.filtering || !self.filter.is_empty());
        self.draw_filter(f, filter_area, ctx);
        if vis.is_empty() {
            let msg = if self.files.is_empty() {
                "No local history yet.\nChanged files are snapshotted automatically;\npress s to snapshot now."
            } else {
                "No files match the filter"
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), list);
            return;
        }
        let h = list.height as usize;
        self.file_offset = scroll_to_keep_visible(self.file_sel, self.file_offset, h);
        let w = list.width as usize;
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.file_offset)
            .take(h)
            .map(|(row, &i)| {
                let file = &self.files[i];
                let when = relative_time(file.last_time);
                let style = if file.deleted {
                    Style::default().fg(t.removed)
                } else {
                    t.text()
                };
                let room = w.saturating_sub(when.chars().count() + 3);
                let name = truncate(&file.path, room);
                let pad = room.saturating_sub(unicode_width::UnicodeWidthStr::width(name.as_str()));
                let line = Line::from(vec![
                    Span::styled(format!(" {name}"), style),
                    Span::raw(" ".repeat(pad + 1)),
                    Span::styled(when, t.dimmed()),
                ]);
                if row == self.file_sel {
                    line.patch_style(t.selected())
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_filter(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if area.height == 0 {
            return;
        }
        let t = &ctx.theme;
        let [p, inp] = Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        f.render_widget(Span::styled("/ ", t.key()), p);
        self.filter.render(f, inp, t.text(), self.filtering);
    }
}

/// Split off one row for the filter field when it is shown.
fn filter_layout(inner: Rect, show: bool) -> [Rect; 2] {
    Layout::vertical([Constraint::Length(u16::from(show)), Constraint::Min(1)]).areas(inner)
}

impl View for LocalHistoryView {
    fn title(&self) -> String {
        "Local History".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        self.pane.invalidate();
        self.load(ctx);
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if self.toggle_requested.take() {
            self.toggle_timeline();
        }
        self.reload_if_changed(ctx);
        let t = &ctx.theme;
        if let Some(e) = &self.error {
            let block = panel(" Local History ", true, t);
            let inner = block.inner(area);
            f.render_widget(block, area);
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                inner,
            );
            return;
        }
        let auto = if ctx.config.local_history.enabled {
            ""
        } else {
            " (automatic snapshots off)"
        };
        if self.timeline {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .areas(area);
            let show = self.filtering || !self.filter.is_empty();
            let [filter_row, list] =
                Layout::vertical([Constraint::Length(u16::from(show)), Constraint::Min(3)])
                    .areas(left);
            self.draw_filter(f, filter_row, ctx);
            self.pane.draw_list(
                f,
                list,
                &format!("Timeline{auto}"),
                self.focus == Focus::List,
                true,
                ctx,
            );
            self.pane
                .draw_preview(f, right, self.focus == Focus::Diff, ctx);
            return;
        }
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
                .areas(area);
        self.draw_files(f, left, ctx);
        let snap_rows = snapshot_rows(self.pane.snaps.len(), right.height);
        let [top, bottom] =
            Layout::vertical([Constraint::Length(snap_rows), Constraint::Min(3)]).areas(right);
        self.pane.draw_list(
            f,
            top,
            &format!("Snapshots{auto}"),
            self.focus == Focus::Snapshots,
            false,
            ctx,
        );
        self.pane
            .draw_preview(f, bottom, self.focus == Focus::Diff, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filtering {
            return self.handle_filter_key(key);
        }
        let shift_tab = key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        match key.code {
            _ if shift_tab => self.cycle_focus(true),
            KeyCode::Tab => self.cycle_focus(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_focused(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_focused(-1),
            KeyCode::PageDown => {
                let page = if self.focus == Focus::Diff {
                    self.pane.diff_page()
                } else {
                    10
                };
                self.move_focused(page);
            }
            KeyCode::PageUp => {
                let page = if self.focus == Focus::Diff {
                    self.pane.diff_page()
                } else {
                    10
                };
                self.move_focused(-page);
            }
            KeyCode::Home | KeyCode::Char('g') => match self.focus {
                Focus::List if !self.timeline => {
                    self.file_sel = 0;
                    self.sync_pane();
                }
                Focus::Diff => self.pane.diff_scroll = 0,
                _ => self.pane.home(),
            },
            KeyCode::End | KeyCode::Char('G') => match self.focus {
                Focus::List if !self.timeline => {
                    self.file_sel = self.list_len().saturating_sub(1);
                    self.sync_pane();
                }
                Focus::Diff => self.pane.scroll_diff(isize::MAX / 2),
                _ => self.pane.end(),
            },
            KeyCode::Char('/') if self.focus == Focus::List => self.filtering = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.set("");
                    self.sync_pane();
                } else {
                    self.focus = Focus::List;
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                if self.focus == Focus::List && !self.timeline {
                    self.focus = Focus::Snapshots;
                } else {
                    return full_diff_action(&mut self.pane, ctx);
                }
            }
            KeyCode::Left => self.focus = Focus::List,
            KeyCode::Char('R') => {
                return match self.pane.selected() {
                    Some(s) => revert_action(s),
                    None => Action::Toast("No snapshot selected".into()),
                }
            }
            KeyCode::Char('l') => return label_action(),
            KeyCode::Char('s') => return snapshot_now_action(ctx),
            KeyCode::Char('P') => return prune_action(ctx),
            KeyCode::Char('t') => self.toggle_timeline(),
            KeyCode::Char('h') | KeyCode::Char('b') | KeyCode::Char('e') => {
                let path = self
                    .pane
                    .selected()
                    .filter(|s| !s.is_marker())
                    .map(|s| s.path.clone())
                    .or_else(|| self.selected_file().map(|f| f.path.clone()));
                let Some(p) = path else { return Action::None };
                return match key.code {
                    KeyCode::Char('h') => crate::ui::views::files::open_file_history(ctx, &p),
                    KeyCode::Char('b') => crate::ui::views::files::open_blame(ctx, &p, None),
                    _ if ctx.repo.path(&p).is_file() => Action::edit(ctx.repo.path(&p), None),
                    _ => Action::Toast(format!("{p} does not exist in the work tree")),
                };
            }
            KeyCode::Char('.') | KeyCode::Char('m') => {
                let a = self.context_menu(ctx);
                return a;
            }
            _ => {}
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("j/k", "move"),
            ("Tab", "switch pane"),
            ("Enter", "open / full diff"),
            ("R", "revert to snapshot"),
            ("l", "put label"),
            ("t", "timeline / files"),
            ("s", "snapshot now"),
            ("P", "prune old snapshots"),
            ("/", "filter"),
            (". / m", "all actions"),
            ("h", "git history of file"),
            ("b", "blame file"),
            ("e", "edit file"),
            ("g/G", "first / last"),
            ("PgUp/PgDn", "page"),
            ("Esc", "clear filter / back"),
        ]
    }

    fn captures_input(&self) -> bool {
        self.filtering
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Put local history label…",
            run: |_| label_action(),
        },
        PaletteItem {
            name: "Show local history",
            run: |_| Action::SwitchTab(Tab::LocalHistory),
        },
        PaletteItem {
            name: "Snapshot changed files to local history",
            run: snapshot_now_action,
        },
        PaletteItem {
            name: "Show local history for file…",
            run: |_| {
                Action::popup(
                    InputPopup::new("Local history for file", "", |path, ctx| {
                        open_for_path(ctx, path.trim())
                    })
                    .prompt("Path relative to the repository root"),
                )
            },
        },
        PaletteItem {
            name: "Prune old local history",
            run: prune_action,
        },
    ]
}

/// Remove snapshots older than `keep_days` now (normally done after each automatic snapshot).
fn prune_action(ctx: &Ctx) -> Action {
    let days = ctx.config.local_history.keep_days;
    Action::spawn("Prune local history", move |repo| {
        let n = local_history::prune(repo, days)?;
        Ok(format!(
            "Removed {n} snapshot{} older than {days} days",
            if n == 1 { "" } else { "s" }
        ))
    })
}

/// Show local history (snapshots) for one file.
pub fn open_for_path(ctx: &Ctx, path: &str) -> Action {
    let mut p = FileHistoryPopup {
        path: path.replace('\\', "/"),
        pane: SnapshotPane::default(),
        focus_diff: false,
        error: None,
    };
    p.reload(ctx);
    Action::popup(p)
}

/// Popup with one file's snapshots, a preview and revert.
struct FileHistoryPopup {
    path: String,
    pane: SnapshotPane,
    focus_diff: bool,
    error: Option<String>,
}

impl FileHistoryPopup {
    fn reload(&mut self, ctx: &Ctx) {
        self.pane.invalidate();
        match local_history::snapshots_for(&ctx.repo, &self.path) {
            Ok(s) => {
                self.pane.set(s);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }
}

impl Popup for FileHistoryPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered_pct(area, 92, 88);
        f.render_widget(Clear, r);
        let block = crate::ui::widgets::dialog(format!(" Local history: {} ", self.path), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [body, keys] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
        if let Some(e) = &self.error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                body,
            );
        } else if self.pane.snaps.is_empty() {
            f.render_widget(
                Paragraph::new("No local history for this file yet. Press s to snapshot it now.")
                    .style(t.dimmed()),
                body,
            );
        } else {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
                    .areas(body);
            self.pane
                .draw_list(f, left, "Snapshots", !self.focus_diff, false, ctx);
            self.pane.draw_preview(f, right, self.focus_diff, ctx);
        }
        let mut spans = Vec::new();
        for (k, d) in self.help() {
            spans.push(Span::styled(format!(" {k}"), t.key()));
            spans.push(Span::styled(format!(" {d} "), t.dimmed()));
        }
        f.render_widget(Line::from(spans), keys);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let delta = |d: isize, pane: &mut SnapshotPane, diff: bool| {
            if diff {
                pane.scroll_diff(d)
            } else {
                pane.move_by(d)
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::ClosePopup,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.focus_diff = !self.focus_diff
            }
            KeyCode::Down | KeyCode::Char('j') => delta(1, &mut self.pane, self.focus_diff),
            KeyCode::Up | KeyCode::Char('k') => delta(-1, &mut self.pane, self.focus_diff),
            KeyCode::PageDown => {
                let p = if self.focus_diff {
                    self.pane.diff_page()
                } else {
                    10
                };
                delta(p, &mut self.pane, self.focus_diff)
            }
            KeyCode::PageUp => {
                let p = if self.focus_diff {
                    self.pane.diff_page()
                } else {
                    10
                };
                delta(-p, &mut self.pane, self.focus_diff)
            }
            KeyCode::Home | KeyCode::Char('g') => self.pane.home(),
            KeyCode::End | KeyCode::Char('G') => self.pane.end(),
            KeyCode::Enter => return full_diff_action(&mut self.pane, ctx),
            KeyCode::Char('R') => {
                if let Some(s) = self.pane.selected() {
                    return revert_action(s);
                }
            }
            KeyCode::Char('l') => return label_action(),
            KeyCode::Char('s') => {
                let r =
                    local_history::snapshot_path(&ctx.repo, &ctx.config.local_history, &self.path);
                let msg = match &r {
                    Ok(0) => "No changes since the last snapshot".to_string(),
                    _ => format!("Snapshotted {}", self.path),
                };
                return Action::from_result(r, msg);
            }
            _ => {}
        }
        Action::None
    }

    fn refresh(&mut self, ctx: &Ctx) {
        self.reload(ctx);
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("j/k", "move"),
            ("Tab", "list/diff"),
            ("Enter", "full diff"),
            ("R", "revert"),
            ("s", "snapshot now"),
            ("l", "put label"),
            ("Esc", "close"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(path: &str, time: i64) -> Snapshot {
        Snapshot {
            path: path.into(),
            time,
            hash: Some(format!("{time:020x}")),
            label: None,
        }
    }

    fn view_with(files: &[&str]) -> LocalHistoryView {
        let mut v = LocalHistoryView {
            files: Vec::new(),
            all: Vec::new(),
            stamp: None,
            loaded: true,
            file_sel: 0,
            file_offset: 0,
            filter: TextInput::default(),
            filtering: false,
            timeline: false,
            focus: Focus::List,
            pane: SnapshotPane::default(),
            error: None,
            toggle_requested: Rc::default(),
            last_check: None,
        };
        for (i, p) in files.iter().enumerate() {
            let t = 100 - i as i64;
            v.all.push(snap(p, t));
            v.all.push(snap(p, t - 50));
            v.files.push(HistoryFile {
                path: p.to_string(),
                last_time: t,
                count: 2,
                deleted: false,
            });
        }
        v.all.sort_by_key(|s| std::cmp::Reverse(s.time));
        v.sync_pane();
        v
    }

    #[test]
    fn filter_and_selection_follow_files() {
        let mut v = view_with(&["src/a.rs", "src/b.rs", "docs/c.md"]);
        assert_eq!(v.pane.snaps.len(), 2);
        assert_eq!(v.pane.snaps[0].path, "src/a.rs");
        v.move_files(1);
        assert_eq!(v.pane.snaps[0].path, "src/b.rs");
        v.filter.set("md");
        v.file_sel = 0;
        v.sync_pane();
        assert_eq!(v.visible_files().len(), 1);
        assert_eq!(v.pane.snaps[0].path, "docs/c.md");
    }

    fn screen(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buf = term.backend().buffer();
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
    fn renders_real_history_and_popup() {
        use crate::config::Config;
        use crate::git::testutil::*;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("src/main.rs", "fn main() {}\n")], "init");
        write(&repo, "src/main.rs", "fn main() { println!(\"hi\"); }\n");
        write(&repo, "notes ü.txt", "draft\n");
        let ctx = Ctx::new(repo, Config::default());
        local_history::snapshot_changed(&ctx.repo, &ctx.config.local_history).unwrap();
        write(&ctx.repo, "src/main.rs", "fn main() {}\n");
        local_history::snapshot_with_label(&ctx.repo, &ctx.config.local_history, "checkpoint")
            .unwrap();

        let mut v = LocalHistoryView::new(&ctx);
        v.refresh(&ctx).unwrap();
        let mut term = Terminal::new(TestBackend::new(140, 32)).unwrap();
        term.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
        let s = screen(&term);
        assert!(
            s.contains("src/main.rs") && s.contains("notes ü.txt"),
            "{s}"
        );
        assert!(s.contains("checkpoint"), "{s}");
        assert!(matches!(
            v.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE), &ctx),
            Action::Popup(_)
        ));
        v.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &ctx);
        term.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
        assert!(screen(&term).contains("Timeline"));

        let Action::Popup(mut p) = open_for_path(&ctx, "src/main.rs") else {
            panic!("expected popup")
        };
        term.draw(|f| p.draw(f, f.area(), &ctx)).unwrap();
        let s = screen(&term);
        assert!(s.contains("Identical to the current file"), "{s}");
        p.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &ctx);
        term.draw(|f| p.draw(f, f.area(), &ctx)).unwrap();
        let s = screen(&term);
        assert!(s.contains("Local history: src/main.rs"), "{s}");
        assert!(s.contains("println"), "{s}");
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx),
            Action::Popup(_) | Action::Toast(_)
        ));
        let mut small = Terminal::new(TestBackend::new(24, 6)).unwrap();
        small.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
        small.draw(|f| p.draw(f, f.area(), &ctx)).unwrap();
    }

    #[test]
    fn snapshot_rows_never_exceeds_height() {
        for h in 0..40u16 {
            for n in [0usize, 1, 3, 10, 100, 70_000, usize::MAX] {
                let r = snapshot_rows(n, h);
                assert!(r <= h.max(1), "n={n} h={h} r={r}");
            }
        }
        assert_eq!(snapshot_rows(1, 40), 5);
        assert_eq!(snapshot_rows(10, 40), 12);
        assert_eq!(snapshot_rows(100, 40), 20);
        assert_eq!(snapshot_rows(usize::MAX, 40), 20);
    }

    #[test]
    fn files_mode_draws_at_tiny_heights() {
        use crate::config::Config;
        use crate::git::testutil::*;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let mut v = view_with(&["a.rs", "b.rs", "c.rs"]);
        assert!(!v.timeline);
        for h in 1..=12u16 {
            for w in [1u16, 20, 80] {
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                term.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
            }
        }
    }

    #[test]
    fn draw_picks_up_background_snapshots() {
        use crate::config::Config;
        use crate::git::testutil::*;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (_d, repo) = temp_repo();
        write(&repo, "first.txt", "1\n");
        let ctx = Ctx::new(repo, Config::default());
        local_history::snapshot_changed(&ctx.repo, &ctx.config.local_history).unwrap();
        let mut v = LocalHistoryView::new(&ctx);
        v.refresh(&ctx).unwrap();
        assert_eq!(v.files.len(), 1);
        // A silent background snapshot: no refresh() call, only redraws.
        write(&ctx.repo, "second.txt", "2\n");
        local_history::snapshot_changed(&ctx.repo, &ctx.config.local_history).unwrap();
        v.last_check = None;
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
        assert_eq!(v.files.len(), 2);
        assert!(screen(&term).contains("second.txt"));
    }

    #[test]
    fn timeline_lists_everything_newest_first() {
        let mut v = view_with(&["a", "b"]);
        v.toggle_timeline();
        assert_eq!(v.pane.snaps.len(), 4);
        assert!(v.pane.snaps.windows(2).all(|w| w[0].time >= w[1].time));
        assert_eq!(v.focus_order().len(), 2);
        v.cycle_focus(false);
        assert_eq!(v.focus, Focus::Diff);
        v.toggle_timeline();
        assert_eq!(v.focus, Focus::List);
        assert_eq!(v.pane.snaps.len(), 2);
    }
}
