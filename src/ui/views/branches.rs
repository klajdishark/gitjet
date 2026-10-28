//! Branches tab: Recent / Favorites / Local / Remote tree with
//! tracking info, every branch operation, and fetch / pull / update project.

mod compare;
mod new_branch;
mod ops;
#[cfg(test)]
mod tests;
mod tree;

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::branch::{self, Branch, Favorites};
use crate::git::log::{Commit, LogQuery};
use crate::git::{Repo, RepoState};
use crate::ui::bg::{Background, Preview};
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{MenuItem, MenuPopup};
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, PaletteItem, View};
use crate::util::{fit, relative_time, truncate};

use tree::{build_rows, Row, Section, TreeInput};

/// Commits shown in the details pane for the selected branch.
const DETAIL_COMMITS: usize = 100;
/// Below this width the details pane is hidden.
const DETAIL_MIN_WIDTH: u16 = 100;
/// The selection must rest this long before the (possibly slow) ahead/behind count starts.
const VS_CURRENT_DELAY: Duration = Duration::from_millis(150);
/// How long the selection must rest on a branch before its commits load.
const DETAIL_DELAY: Duration = Duration::from_millis(60);

type DetailData = Result<Vec<Commit>, String>;

/// What a refresh loads off the UI thread.
struct Loaded {
    branches: Vec<Branch>,
    recent: Vec<String>,
    favorites: Favorites,
    head_commit: Option<String>,
    head_name: Option<String>,
}

fn load_branches(repo: &Repo) -> anyhow::Result<Loaded> {
    Ok(Loaded {
        branches: branch::list_branches(repo)?,
        recent: branch::recent_branches(repo, 20),
        favorites: Favorites::load(repo),
        head_commit: repo.head_commit(),
        head_name: repo.head_branch(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Commits,
}

/// Recent commits of the selected branch (right pane).
#[derive(Default)]
struct Detail {
    /// Full ref the commits were loaded for
    key: Option<String>,
    commits: Vec<Commit>,
    error: Option<String>,
    /// True until `commits` for `key` arrive
    loading: bool,
    /// (commits only in the branch, commits only in HEAD) for branches other than the current one
    vs_current: Option<(u32, u32)>,
    /// When the selection settled on `key`; `Some` while the ahead/behind count still has to start
    vs_due: Option<Instant>,
    /// Background ahead/behind count in flight (it walks history, which can take seconds)
    vs_pending: Option<Receiver<Option<(u32, u32)>>>,
    sel: usize,
    offset: usize,
    height: usize,
}

pub struct BranchesView {
    branches: Vec<Branch>,
    recent: Vec<String>,
    favorites: Favorites,
    collapsed: HashSet<String>,
    rows: Vec<Row>,
    sel: usize,
    offset: usize,
    page: usize,
    filter: TextInput,
    filtering: bool,
    focus: Focus,
    detail: Detail,
    detail_visible: bool,
    /// Where the tree rows were drawn (for mouse clicks)
    tree_rows: Rect,
    detail_area: Rect,
    /// HEAD commit and symbolic branch, cached for the header (detached HEAD, unborn branch)
    head_commit: Option<String>,
    head_name: Option<String>,
    loader: Background<Result<Loaded, String>>,
    load_error: Option<String>,
    details: Preview<String, DetailData>,
}

impl BranchesView {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            branches: Vec::new(),
            recent: Vec::new(),
            favorites: Favorites::default(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            sel: 0,
            offset: 0,
            page: 10,
            filter: TextInput::default(),
            filtering: false,
            focus: Focus::Tree,
            detail: Detail::default(),
            detail_visible: true,
            tree_rows: Rect::default(),
            detail_area: Rect::default(),
            head_commit: None,
            head_name: None,
            loader: Background::new(),
            load_error: None,
            details: Preview::new(DETAIL_DELAY, 32),
        }
    }

    /// Apply a finished refresh.
    fn apply_loaded(&mut self) {
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
        let keep = self.rows.get(self.sel).map(|r| r.id(&self.branches));
        self.branches = loaded.branches;
        self.recent = loaded.recent;
        self.favorites = loaded.favorites;
        self.head_commit = loaded.head_commit;
        self.head_name = loaded.head_name;
        self.details.invalidate();
        self.detail.key = None;
        self.rebuild_keeping(keep);
    }

    fn rebuild(&mut self) {
        let keep = self.rows.get(self.sel).map(|r| r.id(&self.branches));
        self.rebuild_keeping(keep);
    }

    /// Rebuild the rows and re-select the row with id `keep` (or the current branch / clamp).
    fn rebuild_keeping(&mut self, keep: Option<String>) {
        let filter = self.filter.value();
        self.rows = build_rows(&TreeInput {
            branches: &self.branches,
            recent: &self.recent,
            favorites: &self.favorites,
            collapsed: &self.collapsed,
            filter: &filter,
        });
        let found = keep.and_then(|id| self.rows.iter().position(|r| r.id(&self.branches) == id));
        self.sel = found
            .or_else(|| self.first_branch_row())
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
    }

    /// While filtering: the first branch row; otherwise the current branch in Local.
    fn first_branch_row(&self) -> Option<usize> {
        let current = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Branch { idx, section: Section::Local, .. } if self.branches[*idx].is_head));
        current.or_else(|| {
            self.rows
                .iter()
                .position(|r| matches!(r, Row::Branch { .. }))
        })
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.sel)
    }

    fn selected(&self) -> Option<&Branch> {
        match self.selected_row()? {
            Row::Branch { idx, .. } => self.branches.get(*idx),
            _ => None,
        }
    }

    fn current(&self) -> Option<&Branch> {
        self.branches.iter().find(|b| b.is_head)
    }

    fn toggle_collapse(&mut self, expand: Option<bool>) {
        let Some(key) = self.selected_row().and_then(Row::collapse_key) else {
            return;
        };
        let collapsed = self.collapsed.contains(&key);
        let want_collapsed = match expand {
            Some(e) => !e,
            None => !collapsed,
        };
        if want_collapsed {
            self.collapsed.insert(key.clone());
        } else {
            self.collapsed.remove(&key);
        }
        self.rebuild_keeping(Some(key));
    }

    /// Left on a branch row jumps to its parent header.
    fn select_parent(&mut self) {
        let nested = matches!(self.selected_row(), Some(Row::Branch { nested: true, .. }));
        let target = (0..self.sel).rev().find(|&i| match &self.rows[i] {
            Row::Group { .. } => nested,
            Row::Section { .. } => true,
            Row::Branch { .. } => false,
        });
        if let Some(i) = target {
            self.sel = i;
        }
    }

    fn toggle_favorite(&mut self, ctx: &Ctx) -> Action {
        let Some(b) = self.selected().cloned() else {
            return Action::None;
        };
        let on = self.favorites.toggle(&b.full_ref);
        if let Err(e) = self.favorites.save(&ctx.repo) {
            return Action::Error(format!("{e:#}"));
        }
        self.rebuild();
        Action::Toast(if on {
            format!("Added {} to favorites", b.name)
        } else {
            format!("Removed {} from favorites", b.name)
        })
    }

    /// Load the details of the selected branch in the background once the selection rests: its
    /// recent commits, then the ahead/behind count versus HEAD.
    fn ensure_detail(&mut self, ctx: &Ctx) {
        let key = self.selected().map(|b| b.full_ref.clone());
        if key != self.detail.key {
            self.load_detail(key, ctx);
        }
        if self.detail.loading {
            if let Some(res) = self.detail.key.as_ref().and_then(|k| self.details.get(k)) {
                self.detail.loading = false;
                match res {
                    Ok(c) => self.detail.commits = c,
                    Err(e) => self.detail.error = Some(e),
                }
            }
        }
        self.poll_vs_current(ctx);
    }

    fn load_detail(&mut self, key: Option<String>, ctx: &Ctx) {
        self.detail = Detail {
            key: key.clone(),
            height: self.detail.height,
            ..Default::default()
        };
        let is_head = self.selected().is_some_and(|b| b.is_head);
        if let Some(full) = key {
            if !is_head && self.head_commit.is_some() {
                self.detail.vs_due = Some(Instant::now() + VS_CURRENT_DELAY);
            }
            let q = LogQuery {
                revs: vec![full.clone()],
                max: DETAIL_COMMITS,
                ..Default::default()
            };
            self.detail.loading = true;
            self.details.request(&ctx.repo, full, move |repo| {
                repo.log(&q).map_err(|e| format!("{e:#}"))
            });
        }
    }

    /// Start the ahead/behind count once due; pick up its result when it arrives.
    fn poll_vs_current(&mut self, ctx: &Ctx) {
        let d = &mut self.detail;
        if let Some(rx) = &d.vs_pending {
            match rx.try_recv() {
                Ok(v) => {
                    d.vs_current = v;
                    d.vs_pending = None;
                }
                Err(TryRecvError::Disconnected) => d.vs_pending = None,
                Err(TryRecvError::Empty) => {}
            }
            return;
        }
        let (Some(due), Some(full)) = (d.vs_due, d.key.clone()) else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        d.vs_due = None;
        let (tx, rx) = mpsc::channel();
        let repo = ctx.repo.clone();
        std::thread::spawn(move || {
            // The receiver is gone when the selection moved on; nothing to report then.
            let _ = tx.send(branch::ahead_behind(&repo, &full, "HEAD").ok());
        });
        d.vs_pending = Some(rx);
    }

    // ----- drawing -------------------------------------------------------------------------

    fn draw_header(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        if let Some(e) = &self.load_error {
            f.render_widget(
                Line::styled(format!(" {e}"), Style::default().fg(t.error)),
                area,
            );
            return;
        }
        let mut spans = Vec::new();
        match self.current() {
            Some(b) => {
                spans.push(Span::styled(" On ", t.dimmed()));
                spans.push(Span::styled(
                    b.name.clone(),
                    Style::default()
                        .fg(t.local_branch)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.extend(tracking_spans(b, ctx));
            }
            None => match &self.head_commit {
                Some(h) => {
                    spans.push(Span::styled(
                        " HEAD detached at ",
                        Style::default().fg(t.warning),
                    ));
                    spans.push(Span::styled(
                        h.chars().take(8).collect::<String>(),
                        Style::default().fg(t.hash),
                    ));
                }
                None => match &self.head_name {
                    Some(b) => spans.push(Span::styled(
                        format!(" On {b} — no commits yet"),
                        t.dimmed(),
                    )),
                    None => spans.push(Span::styled(" No commits yet", t.dimmed())),
                },
            },
        }
        let state = ctx.repo.state();
        if state != RepoState::Clean {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!(" {} ", state.label()),
                Style::default()
                    .fg(t.conflict)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ));
        }
        let locals = self.branches.iter().filter(|b| !b.is_remote()).count();
        spans.push(Span::styled(
            format!(
                "   {locals} local, {} remote branches",
                self.branches.len() - locals
            ),
            t.dimmed(),
        ));
        f.render_widget(Line::from(spans), area);
    }

    fn draw_tree(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let focused = self.focus == Focus::Tree;
        let block = panel(" Branches ", focused, t);
        let mut inner = block.inner(area);
        f.render_widget(block, area);
        if self.filtering || !self.filter.is_empty() {
            let [fl, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
            let [p, input] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(fl);
            f.render_widget(Span::styled("/ ", t.key()), p);
            self.filter.render(f, input, t.text(), self.filtering);
            inner = rest;
        }
        self.tree_rows = inner;
        let h = inner.height as usize;
        self.page = h.max(1);
        if self.rows.is_empty() {
            let msg = if self.filter.is_empty() {
                "No branches yet — make the first commit, or press n to create a branch"
            } else {
                "No branches match the filter (Esc clears it)"
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), inner);
            return;
        }
        self.offset = scroll_to_keep_visible(self.sel, self.offset, h);
        let cols = self.columns(inner.width as usize);
        let lines: Vec<Line> = (self.offset..self.rows.len().min(self.offset + h))
            .map(|i| {
                let line = self.row_line(&self.rows[i], cols, ctx);
                if i == self.sel {
                    line.patch_style(if focused {
                        t.selected()
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    })
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// (name column, tracking column) widths for the visible rows.
    fn columns(&self, width: usize) -> (usize, usize) {
        let mut name_w = 10;
        let mut track_w = 0;
        for r in &self.rows {
            if let Row::Branch { idx, nested, .. } = r {
                let b = &self.branches[*idx];
                name_w = name_w.max(b.name.width() + if *nested { 8 } else { 6 });
                track_w = track_w.max(tracking_text(b).width());
            }
        }
        (name_w.min(width * 45 / 100).max(10), track_w.min(28))
    }

    fn row_line(&self, row: &Row, (name_w, track_w): (usize, usize), ctx: &Ctx) -> Line<'static> {
        let t = &ctx.theme;
        match row {
            Row::Section {
                section,
                count,
                collapsed,
            } => Line::from(vec![
                Span::styled(
                    format!("{} {}", arrow(*collapsed), section.label()),
                    t.title(),
                ),
                Span::styled(format!(" ({count})"), t.dimmed()),
            ]),
            Row::Group {
                remote,
                count,
                collapsed,
            } => Line::from(vec![
                Span::styled(
                    format!("  {} {remote}", arrow(*collapsed)),
                    Style::default()
                        .fg(t.remote_branch)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" ({count})"), t.dimmed()),
            ]),
            Row::Branch { idx, nested, .. } => {
                let b = &self.branches[*idx];
                let indent = if *nested { "    " } else { "  " };
                let marker = if b.is_head { "● " } else { "  " };
                let star = if self.favorites.contains(&b.full_ref) {
                    "★ "
                } else {
                    "  "
                };
                let mut name_style = Style::default().fg(if b.is_remote() {
                    t.remote_branch
                } else {
                    t.local_branch
                });
                if b.is_head {
                    name_style = name_style.add_modifier(Modifier::BOLD);
                }
                let fixed = indent.len() + 4;
                let mut spans = vec![
                    Span::raw(indent),
                    Span::styled(marker, Style::default().fg(t.head)),
                    Span::styled(star, Style::default().fg(t.tag)),
                    Span::styled(fit(&b.name, name_w.saturating_sub(fixed)), name_style),
                    Span::raw(" "),
                ];
                if track_w > 0 {
                    let style = if b.upstream_gone {
                        Style::default().fg(t.error)
                    } else if b.ahead > 0 || b.behind > 0 {
                        Style::default().fg(t.warning)
                    } else {
                        t.dimmed()
                    };
                    spans.push(Span::styled(fit(&tracking_text(b), track_w), style));
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(
                    fit(&relative_time(b.time), 15),
                    Style::default().fg(t.date),
                ));
                spans.push(Span::styled(b.subject.clone(), t.text()));
                Line::from(spans)
            }
        }
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        self.detail_area = area;
        self.ensure_detail(ctx);
        let focused = self.focus == Focus::Commits;
        let title = match self.selected() {
            Some(b) => format!(" {} ", b.name),
            None => " Details ".into(),
        };
        let block = panel(title, focused, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(b) = self.selected().cloned() else {
            f.render_widget(Paragraph::new(self.group_help()).style(t.dimmed()), inner);
            return;
        };
        let mut info = vec![Line::from(vec![
            Span::styled("Tip ", t.dimmed()),
            Span::styled(b.short.clone(), Style::default().fg(t.hash)),
            Span::styled(
                format!("  {}", relative_time(b.time)),
                Style::default().fg(t.date),
            ),
        ])];
        if !b.is_remote() {
            let mut up = vec![Span::styled("Upstream ", t.dimmed())];
            match &b.upstream {
                Some(u) => {
                    up.push(Span::styled(
                        u.clone(),
                        Style::default().fg(t.remote_branch),
                    ));
                    up.extend(tracking_counts(&b, ctx));
                }
                None => up.push(Span::styled(
                    "none (u to set, P to push with set-upstream)",
                    t.dimmed(),
                )),
            }
            info.push(Line::from(up));
        }
        let current = self
            .current()
            .map(|b| b.name.clone())
            .unwrap_or_else(|| "HEAD".into());
        if let Some((only_branch, only_head)) = self.detail.vs_current {
            info.push(Line::from(vec![
                Span::styled(format!("vs {current} "), t.dimmed()),
                Span::styled(
                    format!("{} ahead, {} behind", only_branch, only_head),
                    Style::default().fg(t.warning),
                ),
                Span::styled("  (= to compare)", t.dimmed()),
            ]));
        } else if self.detail.vs_due.is_some() || self.detail.vs_pending.is_some() {
            info.push(Line::from(Span::styled(
                format!("vs {current} counting…"),
                t.dimmed(),
            )));
        }
        let [head, list] = Layout::vertical([
            Constraint::Length(info.len() as u16 + 1),
            Constraint::Min(0),
        ])
        .areas(inner);
        f.render_widget(Paragraph::new(info), head);
        let h = list.height as usize;
        self.detail.height = h.max(1);
        if let Some(e) = &self.detail.error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                list,
            );
            return;
        }
        if self.detail.loading {
            f.render_widget(Paragraph::new("Loading…").style(t.dimmed()), list);
            return;
        }
        self.detail.offset = scroll_to_keep_visible(self.detail.sel, self.detail.offset, h);
        let lines: Vec<Line> = self
            .detail
            .commits
            .iter()
            .enumerate()
            .skip(self.detail.offset)
            .take(h)
            .map(|(i, c)| {
                let line = commit_line(c, list.width as usize, ctx);
                if focused && i == self.detail.sel {
                    line.patch_style(t.selected())
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }

    fn group_help(&self) -> String {
        match self.selected_row() {
            Some(Row::Section { section, count, .. }) => {
                format!(
                    "{} — {count} branches\n\nEnter/Space collapses or expands, ←/→ too.",
                    section.label()
                )
            }
            Some(Row::Group { remote, count, .. }) => {
                format!("Remote {remote} — {count} branches\n\nEnter/Space collapses or expands.")
            }
            _ => "Select a branch".into(),
        }
    }

    // ----- input ---------------------------------------------------------------------------

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filter.set("");
                self.filtering = false;
                self.rebuild();
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Up => self.sel = move_sel(self.sel, -1, self.rows.len()),
            KeyCode::Down => self.sel = move_sel(self.sel, 1, self.rows.len()),
            _ => {
                if self.filter.handle_key(&key) {
                    self.rebuild_keeping(None);
                }
            }
        }
        Action::None
    }

    fn handle_commits_key(&mut self, key: KeyEvent) -> Option<Action> {
        let len = self.detail.commits.len();
        let page = self.detail.height as isize;
        let d = &mut self.detail;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => d.sel = move_sel(d.sel, -1, len),
            KeyCode::Down | KeyCode::Char('j') => d.sel = move_sel(d.sel, 1, len),
            KeyCode::PageUp => d.sel = move_sel(d.sel, -page, len),
            KeyCode::PageDown => d.sel = move_sel(d.sel, page, len),
            KeyCode::Home | KeyCode::Char('g') => d.sel = 0,
            KeyCode::End | KeyCode::Char('G') => d.sel = len.saturating_sub(1),
            KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.focus = Focus::Tree
            }
            KeyCode::Enter => {
                return Some(match d.commits.get(d.sel) {
                    Some(c) => diff_view::open(
                        format!("{} {}", c.short, c.subject),
                        DiffSource::Commit {
                            rev: c.id.clone(),
                            paths: Vec::new(),
                        },
                    ),
                    None => Action::None,
                })
            }
            _ => return None,
        }
        Some(Action::None)
    }

    fn handle_tree_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Option<Action> {
        let len = self.rows.len();
        let on_header = matches!(
            self.selected_row(),
            Some(Row::Section { .. } | Row::Group { .. })
        );
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.sel = move_sel(self.sel, -1, len),
            KeyCode::Down | KeyCode::Char('j') => self.sel = move_sel(self.sel, 1, len),
            KeyCode::PageUp => self.sel = move_sel(self.sel, -(self.page as isize), len),
            KeyCode::PageDown => self.sel = move_sel(self.sel, self.page as isize, len),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = len.saturating_sub(1),
            KeyCode::Tab | KeyCode::BackTab if self.detail_visible && self.selected().is_some() => {
                self.focus = Focus::Commits
            }
            KeyCode::Enter | KeyCode::Char(' ') if on_header => self.toggle_collapse(None),
            KeyCode::Left | KeyCode::Char('h') => {
                if on_header {
                    self.toggle_collapse(Some(false))
                } else {
                    self.select_parent()
                }
            }
            KeyCode::Right | KeyCode::Char('l') if on_header => self.toggle_collapse(Some(true)),
            KeyCode::Right | KeyCode::Char('l')
                if self.detail_visible && self.selected().is_some() =>
            {
                self.focus = Focus::Commits
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.set("");
                self.rebuild();
            }
            KeyCode::Char('*') => return Some(self.toggle_favorite(ctx)),
            _ => return None,
        }
        Some(Action::None)
    }

    /// Branch operations, available from both panes.
    fn handle_action_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let b = self.selected().cloned();
        let with = |f: &dyn Fn(&Branch) -> Action| b.as_ref().map(f).unwrap_or(Action::None);
        match key.code {
            KeyCode::Enter | KeyCode::Char('c') => with(&|b| ops::checkout(ctx, b)),
            KeyCode::Char('n') => ops::new_branch(b.as_ref()),
            KeyCode::Char('C') => with(&|b| ops::checkout_and_rebase(ctx, b)),
            KeyCode::Char('R') => with(&ops::rename),
            KeyCode::Char('D') => with(&|b| ops::delete(ctx, b)),
            KeyCode::Char('M') => with(&|b| not_current(b, || ops::merge_menu(ctx, &b.name))),
            KeyCode::Char('r') => with(&|b| not_current(b, || ops::rebase_onto(ctx, &b.name))),
            KeyCode::Char('=') => with(&|b| not_current(b, || ops::compare(ctx, &b.name))),
            KeyCode::Char('w') => with(&|b| ops::diff_with_worktree(&b.name)),
            KeyCode::Char('u') => with(&|b| ops::upstream_menu(ctx, b)),
            KeyCode::Char('P') => ops::push_branch(ctx, b.as_ref()),
            KeyCode::Char('f') => ops::fetch(ctx),
            KeyCode::Char('p') => ops::pull_menu(ctx),
            KeyCode::Char('U') => ops::update_project(ctx),
            KeyCode::Char('.') | KeyCode::Char('m') => self.context_menu(ctx),
            _ => Action::None,
        }
    }

    fn context_menu(&self, ctx: &Ctx) -> Action {
        let current = ctx.repo.head_branch().unwrap_or_else(|| "HEAD".into());
        let mut items = Vec::new();
        let title = match self.selected().cloned() {
            Some(b) => {
                items = branch_items(&b, &current, self.favorites.contains(&b.full_ref));
                b.name
            }
            None => {
                items.push(MenuItem::new("New branch from HEAD…", "n", |_| {
                    ops::new_branch(None)
                }));
                "Branches".into()
            }
        };
        items.extend(ops::network_items());
        Action::popup(MenuPopup::new(title, items))
    }
}

/// Run `f` unless `b` is the current branch.
fn not_current(b: &Branch, f: impl FnOnce() -> Action) -> Action {
    if b.is_head {
        Action::Toast(format!("{} is the current branch", b.name))
    } else {
        f()
    }
}

/// Context-menu entries for one branch; accelerators match the tab's keys.
fn branch_items(b: &Branch, current: &str, favorite: bool) -> Vec<MenuItem> {
    let local = !b.is_remote();
    let other = !b.is_head;
    let name = b.name.clone();
    let item = |label: String, key: &str, f: fn(&Ctx, &Branch) -> Action| {
        let b = b.clone();
        MenuItem::new(label, key, move |ctx| f(ctx, &b))
    };
    vec![
        item(
            if local {
                "Checkout".into()
            } else {
                format!("Checkout as new local branch tracking {name}…")
            },
            "c",
            ops::checkout,
        )
        .enabled(other),
        item(format!("New branch from {name}…"), "n", |_, b| {
            ops::new_branch(Some(b))
        }),
        item(
            format!("Checkout and rebase onto {current}"),
            "C",
            ops::checkout_and_rebase,
        )
        .enabled(local && other),
        item(format!("Compare with {current}"), "=", |ctx, b| {
            ops::compare(ctx, &b.name)
        })
        .enabled(other),
        item("Show diff with working tree".into(), "w", |_, b| {
            ops::diff_with_worktree(&b.name)
        }),
        item(format!("Rebase {current} onto {name}"), "r", |ctx, b| {
            ops::rebase_onto(ctx, &b.name)
        })
        .enabled(other),
        item(format!("Merge {name} into {current}…"), "M", |ctx, b| {
            ops::merge_menu(ctx, &b.name)
        })
        .enabled(other),
        item("Push…".into(), "P", |ctx, b| {
            ops::push_branch(ctx, Some(b))
        })
        .enabled(local),
        item("Rename…".into(), "R", |_, b| ops::rename(b)).enabled(local),
        item("Set upstream…".into(), "u", ops::upstream_menu).enabled(local),
        item(
            if favorite {
                "Remove from favorites".into()
            } else {
                "Add to favorites".into()
            },
            "*",
            |ctx, b| {
                let mut fav = Favorites::load(&ctx.repo);
                fav.toggle(&b.full_ref);
                Action::from_result(fav.save(&ctx.repo), "Favorites updated")
            },
        ),
        item(
            if local {
                "Delete".into()
            } else {
                format!("Delete {name} on the remote…")
            },
            "D",
            ops::delete,
        )
        .enabled(other),
    ]
}

fn arrow(collapsed: bool) -> &'static str {
    if collapsed {
        "▸"
    } else {
        "▾"
    }
}

/// `origin/main ↑1 ↓2` / `origin/x (gone)` / empty.
fn tracking_text(b: &Branch) -> String {
    let Some(up) = &b.upstream else {
        return String::new();
    };
    if b.upstream_gone {
        return format!("{up} (gone)");
    }
    let mut s = up.clone();
    if b.ahead > 0 {
        s.push_str(&format!(" ↑{}", b.ahead));
    }
    if b.behind > 0 {
        s.push_str(&format!(" ↓{}", b.behind));
    }
    s
}

fn tracking_counts(b: &Branch, ctx: &Ctx) -> Vec<Span<'static>> {
    let t = &ctx.theme;
    if b.upstream_gone {
        return vec![Span::styled(
            " (gone on the remote)",
            Style::default().fg(t.error),
        )];
    }
    let mut v = Vec::new();
    if b.ahead > 0 {
        v.push(Span::styled(
            format!(" ↑{}", b.ahead),
            Style::default().fg(t.warning),
        ));
    }
    if b.behind > 0 {
        v.push(Span::styled(
            format!(" ↓{}", b.behind),
            Style::default().fg(t.warning),
        ));
    }
    if v.is_empty() {
        v.push(Span::styled(" (up to date)", t.dimmed()));
    }
    v
}

fn tracking_spans(b: &Branch, ctx: &Ctx) -> Vec<Span<'static>> {
    let t = &ctx.theme;
    match &b.upstream {
        Some(up) => {
            let mut v = vec![
                Span::styled(" → ", t.dimmed()),
                Span::styled(up.clone(), Style::default().fg(t.remote_branch)),
            ];
            v.extend(tracking_counts(b, ctx));
            v
        }
        None => vec![Span::styled("  (no upstream)", t.dimmed())],
    }
}

/// One commit as `hash subject … author date`, fitted to `width` columns.
pub fn commit_line(c: &Commit, width: usize, ctx: &Ctx) -> Line<'static> {
    let t = &ctx.theme;
    let right = format!(" {}  {}", truncate(&c.author, 18), relative_time(c.time));
    let right_w = right.width();
    let subject_w = width.saturating_sub(c.short.width() + 1 + right_w);
    let mut spans = vec![
        Span::styled(c.short.clone(), Style::default().fg(t.hash)),
        Span::raw(" "),
        Span::styled(fit(&c.subject, subject_w), t.text()),
    ];
    if subject_w > 8 {
        spans.push(Span::styled(right, Style::default().fg(t.author)));
    }
    Line::from(spans)
}

impl View for BranchesView {
    fn title(&self) -> String {
        "Branches".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        self.loader.start(&ctx.repo, |repo| {
            load_branches(repo).map_err(|e| format!("{e:#}"))
        });
        self.apply_loaded();
        Ok(())
    }

    fn poll(&mut self, _ctx: &Ctx) {
        self.apply_loaded();
    }

    fn loading(&self) -> bool {
        self.loader.busy()
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        self.draw_header(f, header, ctx);
        self.detail_visible = body.width >= DETAIL_MIN_WIDTH;
        if !self.detail_visible {
            self.focus = Focus::Tree;
            self.draw_tree(f, body, ctx);
            return;
        }
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
        self.draw_tree(f, left, ctx);
        self.draw_detail(f, right, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filtering {
            return self.handle_filter_key(key);
        }
        let handled = match self.focus {
            Focus::Tree => self.handle_tree_key(key, ctx),
            Focus::Commits => self.handle_commits_key(key),
        };
        handled.unwrap_or_else(|| self.handle_action_key(key, ctx))
    }

    fn handle_mouse(&mut self, ev: MouseEvent, _ctx: &Ctx) -> Action {
        if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::None;
        }
        let pos = Position::new(ev.column, ev.row);
        if self.tree_rows.contains(pos) {
            let i = self.offset + (ev.row - self.tree_rows.y) as usize;
            if i < self.rows.len() {
                self.sel = i;
                self.focus = Focus::Tree;
            }
        } else if self.detail_visible && self.detail_area.contains(pos) && self.selected().is_some()
        {
            self.focus = Focus::Commits;
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("Enter/c", "checkout"),
            ("n", "new branch"),
            ("M", "merge into current"),
            ("r", "rebase current onto"),
            ("f", "fetch"),
            ("p", "pull"),
            ("P", "push"),
            (".", "all actions"),
            ("U", "update project"),
            ("C", "checkout & rebase onto current"),
            ("=", "compare with current"),
            ("w", "diff with working tree"),
            ("u", "set upstream"),
            ("R", "rename"),
            ("D", "delete"),
            ("*", "toggle favorite"),
            ("/", "filter"),
            ("Space/←/→", "collapse / expand"),
            ("Tab", "branch commits pane"),
            ("j/k ↑↓", "move"),
            ("m", "context menu"),
        ]
    }

    fn captures_input(&self) -> bool {
        self.filtering
    }

    fn badge(&self) -> Option<String> {
        let behind = self.current().map(|b| b.behind).unwrap_or(0);
        (behind > 0).then(|| format!("↓{behind}"))
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Fetch",
            run: ops::fetch,
        },
        PaletteItem {
            name: "Pull…",
            run: ops::pull_menu,
        },
        PaletteItem {
            name: "Update Project",
            run: ops::update_project,
        },
        PaletteItem {
            name: "New branch…",
            run: |_| ops::new_branch(None),
        },
        PaletteItem {
            name: "Checkout branch…",
            run: |ctx| ops::pick(ctx, "Checkout branch", |_| true, ops::checkout),
        },
        PaletteItem {
            name: "Merge branch…",
            run: |ctx| {
                ops::pick(
                    ctx,
                    "Merge into current",
                    |b| !b.is_head,
                    |ctx, b| ops::merge_menu(ctx, &b.name),
                )
            },
        },
        PaletteItem {
            name: "Rebase onto…",
            run: |ctx| {
                ops::pick(
                    ctx,
                    "Rebase current branch onto",
                    |b| !b.is_head,
                    |ctx, b| ops::rebase_onto(ctx, &b.name),
                )
            },
        },
        PaletteItem {
            name: "Delete branch…",
            run: |ctx| ops::pick(ctx, "Delete branch", |b| !b.is_head, ops::delete),
        },
        PaletteItem {
            name: "Rename branch…",
            run: |ctx| {
                ops::pick(
                    ctx,
                    "Rename branch",
                    |b| !b.is_remote(),
                    |_, b| ops::rename(b),
                )
            },
        },
        PaletteItem {
            name: "Compare with branch…",
            run: |ctx| {
                ops::pick(
                    ctx,
                    "Compare with current",
                    |b| !b.is_head,
                    |ctx, b| ops::compare(ctx, &b.name),
                )
            },
        },
        PaletteItem {
            name: "Fetch (in terminal)",
            run: ops::fetch_in_terminal,
        },
        PaletteItem {
            name: "Pull (in terminal)",
            run: ops::pull_in_terminal,
        },
        PaletteItem {
            name: "Push (in terminal)",
            run: ops::push_in_terminal,
        },
    ]
}
