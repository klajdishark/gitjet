//! Fullscreen history popup shared by File History, directory history and History for Selection:
//! commit list on top, diff preview below, actions on the selected revision.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::common::{commit_details, status_letter_color, LineNav, Loader};
use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::diff::{DiffOptions, FileDiff};
use crate::git::log::{Commit, LogQuery};
use crate::git::Repo;
use crate::ui::diff_view::{self, diff_height, draw_diff, DiffSource};
use crate::ui::popups::{ConfirmPopup, MenuItem, MenuPopup};
use crate::ui::widgets::{panel, TextInput};
use crate::ui::{KeyHelp, Popup};
use crate::util::{fit, format_date, truncate};

/// Commits loaded per page.
const PAGE: usize = 500;

/// What the history is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One file, following renames
    File(String),
    /// Every commit touching a directory
    Directory(String),
    /// Lines `start..=end` of a file as seen at `rev` (HEAD when `None`)
    Lines {
        path: String,
        start: u32,
        end: u32,
        rev: Option<String>,
    },
}

impl Target {
    /// Current path of the file or directory.
    pub fn path(&self) -> &str {
        match self {
            Target::File(p) | Target::Directory(p) => p,
            Target::Lines { path, .. } => path,
        }
    }

    fn title(&self) -> String {
        match self {
            Target::File(p) => format!("History of {p}"),
            Target::Directory(p) => format!("History of {p}/"),
            Target::Lines {
                path, start, end, ..
            } if start == end => format!("History of {path}:{start}"),
            Target::Lines {
                path, start, end, ..
            } => format!("History of {path}:{start}-{end}"),
        }
    }
}

/// One revision in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub commit: Commit,
    /// Path of the file at this commit
    pub path: String,
    pub old_path: Option<String>,
    /// Name-status letter (`M` for directories and line histories)
    pub status: char,
    /// Pre-computed hunks (line history only)
    pub diffs: Option<Vec<FileDiff>>,
}

impl Entry {
    /// Revision whose tree contains the file: the commit itself, or its parent for deletions.
    fn content_rev(&self) -> String {
        if self.status == 'D' {
            format!("{}^", self.commit.id)
        } else {
            self.commit.id.clone()
        }
    }

    fn paths(&self) -> Vec<String> {
        let mut v = vec![self.path.clone()];
        if let Some(o) = self.old_path.as_ref().filter(|o| **o != self.path) {
            v.push(o.clone());
        }
        v
    }

    fn matches(&self, q: &str) -> bool {
        let c = &self.commit;
        [&c.subject, &c.author, &c.id, &self.path]
            .iter()
            .any(|s| s.to_lowercase().contains(q))
    }
}

fn load_entries(repo: &Repo, target: &Target, max: usize) -> anyhow::Result<Vec<Entry>> {
    Ok(match target {
        Target::File(p) => repo
            .file_log(p, max)?
            .into_iter()
            .map(|r| Entry {
                commit: r.commit,
                path: r.path,
                old_path: r.old_path,
                status: r.status,
                diffs: None,
            })
            .collect(),
        Target::Directory(p) => {
            let q = LogQuery {
                paths: vec![p.clone()],
                max,
                topo: false,
                ..Default::default()
            };
            repo.log(&q)?
                .into_iter()
                .map(|commit| Entry {
                    commit,
                    path: p.clone(),
                    old_path: None,
                    status: 'M',
                    diffs: None,
                })
                .collect()
        }
        Target::Lines {
            path,
            start,
            end,
            rev,
        } => repo
            .line_log(path, *start, *end, rev.as_deref(), max)?
            .into_iter()
            .map(|r| {
                let added = r.diffs.first().is_some_and(|d| d.old_path.is_none());
                Entry {
                    commit: r.commit,
                    path: r.path,
                    old_path: None,
                    status: if added { 'A' } else { 'M' },
                    diffs: Some(r.diffs),
                }
            })
            .collect(),
    })
}

/// Commands from the context menu that change the popup's own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Filter,
    TogglePreview,
}

/// Fullscreen history of a file, directory or line range.
pub struct HistoryPopup {
    target: Target,
    loader: Loader<Vec<Entry>>,
    entries: Vec<Entry>,
    max: usize,
    complete: bool,
    error: Option<String>,
    nav: LineNav,
    keep_id: Option<String>,
    preview_focus: bool,
    preview_cache: HashMap<String, Result<Vec<FileDiff>, String>>,
    preview_scroll: usize,
    preview_height: usize,
    preview_id: String,
    filter: TextInput,
    filter_focus: bool,
    pending: Rc<Cell<Option<Cmd>>>,
}

impl HistoryPopup {
    pub fn new(ctx: &Ctx, target: Target) -> Self {
        let mut p = Self {
            target,
            loader: Loader::idle(),
            entries: Vec::new(),
            max: PAGE,
            complete: false,
            error: None,
            nav: LineNav::default(),
            keep_id: None,
            preview_focus: false,
            preview_cache: HashMap::new(),
            preview_scroll: 0,
            preview_height: 0,
            preview_id: String::new(),
            filter: TextInput::default(),
            filter_focus: false,
            pending: Rc::new(Cell::new(None)),
        };
        p.reload(ctx);
        p
    }

    fn reload(&mut self, ctx: &Ctx) {
        self.keep_id = self.selected().map(|e| e.commit.id.clone());
        let target = self.target.clone();
        let max = self.max;
        self.loader = Loader::start(&ctx.repo, move |repo| load_entries(repo, &target, max));
    }

    fn poll(&mut self) {
        let Some(res) = self.loader.poll() else {
            return;
        };
        match res {
            Ok(entries) => {
                self.complete = entries.len() < self.max;
                self.entries = entries;
                self.error = None;
                if let Some(id) = self.keep_id.take() {
                    let vis = self.visible();
                    if let Some(i) = vis.iter().position(|&i| self.entries[i].commit.id == id) {
                        self.nav.cursor = i;
                    }
                }
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Indices of entries matching the filter.
    fn visible(&self) -> Vec<usize> {
        let q = self.filter.value().to_lowercase();
        (0..self.entries.len())
            .filter(|&i| q.is_empty() || self.entries[i].matches(&q))
            .collect()
    }

    fn selected_index(&self) -> Option<usize> {
        self.visible().get(self.nav.cursor).copied()
    }

    fn selected(&self) -> Option<&Entry> {
        self.selected_index().map(|i| &self.entries[i])
    }

    /// Everything needed for the actions on the selected revision.
    fn snapshot(&self) -> Option<Snapshot> {
        let i = self.selected_index()?;
        Some(Snapshot {
            target: self.target.clone(),
            entry: self.entries[i].clone(),
            older: self.entries.get(i + 1).cloned(),
        })
    }

    fn maybe_load_more(&mut self, ctx: &Ctx) {
        let at_end = self.nav.cursor + 1 >= self.visible().len();
        if at_end && !self.complete && !self.loader.is_loading() && !self.entries.is_empty() {
            self.max *= 2;
            self.reload(ctx);
        }
    }

    fn preview(&mut self, ctx: &Ctx) -> Option<&Result<Vec<FileDiff>, String>> {
        let e = self.selected()?.clone();
        let id = e.commit.id.clone();
        if self.preview_id != id {
            self.preview_id = id.clone();
            self.preview_scroll = 0;
        }
        if !self.preview_cache.contains_key(&id) {
            let res = match &e.diffs {
                Some(d) => Ok(d.clone()),
                None => {
                    let opts = DiffOptions::from_config(&ctx.config.diff);
                    ctx.repo
                        .diff_commit(&id, &e.paths(), &opts)
                        .map_err(|err| format!("{err:#}"))
                }
            };
            if self.preview_cache.len() > 200 {
                self.preview_cache.clear();
            }
            self.preview_cache.insert(id.clone(), res);
        }
        self.preview_cache.get(&id)
    }

    fn menu(&self) -> Action {
        let pending = self.pending.clone();
        let set = move |c: Cmd| {
            let p = pending.clone();
            move |_: &Ctx| {
                p.set(Some(c));
                Action::None
            }
        };
        let s = self.snapshot();
        let has = s.is_some();
        let is_dir = matches!(self.target, Target::Directory(_));
        let file_ok = has && !is_dir;
        let run = |f: fn(&Snapshot, &Ctx) -> Action| {
            let s = s.clone();
            move |ctx: &Ctx| s.as_ref().map(|s| f(s, ctx)).unwrap_or(Action::None)
        };
        let items = vec![
            MenuItem::new("Show diff", "d", run(Snapshot::full_diff)).enabled(has),
            MenuItem::new(
                "Compare with previous revision",
                "p",
                run(Snapshot::compare_previous),
            )
            .enabled(has),
            MenuItem::new(
                "Compare with local (work tree)",
                "w",
                run(Snapshot::compare_worktree),
            )
            .enabled(has),
            MenuItem::new(
                "Show file at this revision",
                "o",
                run(Snapshot::show_at_revision),
            )
            .enabled(file_ok),
            MenuItem::new("Annotate this revision", "b", run(Snapshot::blame)).enabled(file_ok),
            MenuItem::new("Commit details", "c", run(Snapshot::details)).enabled(has),
            MenuItem::new(
                "Get this revision (restore file)…",
                "R",
                run(Snapshot::restore),
            )
            .enabled(file_ok),
            MenuItem::new("Filter commits", "/", set(Cmd::Filter)),
            MenuItem::new("Focus diff preview", "Tab", set(Cmd::TogglePreview)),
        ];
        Action::popup(MenuPopup::new(truncate(&self.target.title(), 50), items))
    }

    fn run_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Filter => {
                self.filter_focus = true;
                self.preview_focus = false;
            }
            Cmd::TogglePreview => self.preview_focus = !self.preview_focus,
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.visible();
        let mut title = vec![Span::styled(
            format!(" {} ", self.target.title()),
            t.title(),
        )];
        let count = if self.complete || self.entries.is_empty() {
            format!("— {} commits ", vis.len())
        } else {
            format!("— {}+ commits ", vis.len())
        };
        title.push(Span::styled(count, t.dimmed()));
        let block = panel(Line::from(title), !self.preview_focus, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let show_filter = self.filter_focus || !self.filter.is_empty();
        let [filter_area, list] = Layout::vertical([
            Constraint::Length(u16::from(show_filter)),
            Constraint::Min(1),
        ])
        .areas(inner);
        if show_filter {
            let [l, r] =
                Layout::horizontal([Constraint::Length(8), Constraint::Min(1)]).areas(filter_area);
            f.render_widget(Span::styled("Filter: ", t.key()), l);
            self.filter.render(f, r, t.text(), self.filter_focus);
        }
        if let Some(e) = &self.error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                list,
            );
            return;
        }
        if vis.is_empty() {
            let msg = if self.loader.is_loading() {
                "Loading history…"
            } else if self.entries.is_empty() {
                "No commits touch this path"
            } else {
                "No commits match the filter"
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), list);
            return;
        }
        self.nav.fit(list.height as usize, vis.len());
        let w = list.width as usize;
        let mut lines = Vec::new();
        for (row, &i) in vis
            .iter()
            .enumerate()
            .skip(self.nav.offset)
            .take(list.height as usize)
        {
            let e = &self.entries[i];
            let c = &e.commit;
            let mut spans = vec![
                Span::styled(
                    format!("{} ", fit(&c.short, 8)),
                    Style::default().fg(t.hash),
                ),
                Span::styled(
                    format!("{} ", format_date(c.time)),
                    Style::default().fg(t.date),
                ),
                Span::styled(
                    format!("{} ", fit(&c.author, 16)),
                    Style::default().fg(t.author),
                ),
                Span::styled(
                    format!("{} ", e.status),
                    Style::default().fg(status_letter_color(t, e.status)),
                ),
            ];
            let used = 8 + 1 + 10 + 1 + 16 + 1 + 2;
            let mut rest = c.subject.clone();
            if let Some(o) = &e.old_path {
                rest.push_str(&format!("  [{o} → {}]", e.path));
            }
            let rest = truncate(&rest, w.saturating_sub(used));
            let left = w.saturating_sub(used + rest.chars().count() + 1);
            spans.push(Span::styled(rest, t.text()));
            if !c.refs.is_empty() && left > 3 {
                let refs = truncate(&format!("({})", c.refs.join(", ")), left);
                spans.push(Span::styled(
                    format!(" {refs}"),
                    Style::default().fg(t.local_branch),
                ));
            }
            let mut line = Line::from(spans);
            if row == self.nav.cursor {
                line = line.style(if self.preview_focus {
                    Style::default().bg(t.selection_bg)
                } else {
                    t.selected()
                });
            }
            lines.push(line);
        }
        if self.loader.is_loading() && lines.len() < list.height as usize {
            lines.push(Line::styled("Loading more…", t.dimmed()));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let focused = self.preview_focus;
        let block = panel(" Diff ", focused, &ctx.theme);
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.preview_height = inner.height as usize;
        let scroll = self.preview_scroll;
        match self.preview(ctx) {
            None => {}
            Some(Err(e)) => {
                let e = e.clone();
                f.render_widget(
                    Paragraph::new(e).style(Style::default().fg(ctx.theme.error)),
                    inner,
                );
            }
            Some(Ok(d)) if d.is_empty() => f.render_widget(
                Paragraph::new("No changes to this path in the commit").style(ctx.theme.dimmed()),
                inner,
            ),
            Some(Ok(d)) => {
                let d = d.clone();
                let max = diff_height(&d).saturating_sub(1);
                self.preview_scroll = scroll.min(max);
                draw_diff(f, inner, &d, self.preview_scroll, ctx);
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filter.set("");
                self.filter_focus = false;
            }
            KeyCode::Enter | KeyCode::Tab => self.filter_focus = false,
            KeyCode::Up => self.nav.move_by(-1, self.visible().len()),
            KeyCode::Down => self.nav.move_by(1, self.visible().len()),
            _ => {
                if self.filter.handle_key(&key) {
                    self.nav.cursor = 0;
                    self.nav.offset = 0;
                }
            }
        }
        Action::None
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> bool {
        let page = self.preview_height.max(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.preview_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1)
            }
            KeyCode::PageDown | KeyCode::Char(' ') => self.preview_scroll += page,
            KeyCode::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(page),
            KeyCode::Home => self.preview_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.preview_scroll = usize::MAX / 2,
            _ => return false,
        }
        true
    }
}

/// Selected revision plus the next older one, detached from the popup for menu callbacks.
#[derive(Clone)]
struct Snapshot {
    target: Target,
    entry: Entry,
    older: Option<Entry>,
}

impl Snapshot {
    fn is_dir(&self) -> bool {
        matches!(self.target, Target::Directory(_))
    }

    fn full_diff(&self, _ctx: &Ctx) -> Action {
        let e = &self.entry;
        diff_view::open(
            format!("{} {} — {}", e.commit.short, e.path, e.commit.subject),
            DiffSource::Commit {
                rev: e.commit.id.clone(),
                paths: e.paths(),
            },
        )
    }

    fn compare_previous(&self, ctx: &Ctx) -> Action {
        let e = &self.entry;
        match &self.older {
            Some(o) => {
                let mut paths = e.paths();
                if !paths.contains(&o.path) {
                    paths.push(o.path.clone());
                }
                diff_view::open(
                    format!(
                        "{} {} → {} {}",
                        o.commit.short, o.path, e.commit.short, e.path
                    ),
                    DiffSource::Range {
                        from: o.commit.id.clone(),
                        to: e.commit.id.clone(),
                        paths,
                    },
                )
            }
            None => self.full_diff(ctx),
        }
    }

    fn compare_worktree(&self, _ctx: &Ctx) -> Action {
        let e = &self.entry;
        let mut paths = vec![e.path.clone()];
        let current = self.target.path().to_string();
        if current != e.path {
            paths.push(current.clone());
        }
        diff_view::open(
            format!("{} {} ↔ local {}", e.commit.short, e.path, current),
            DiffSource::RevToWorktree {
                rev: e.content_rev(),
                paths,
            },
        )
    }

    fn show_at_revision(&self, ctx: &Ctx) -> Action {
        if self.is_dir() {
            return Action::None;
        }
        super::open_file_at_revision(ctx, &self.entry.path, &self.entry.content_rev())
    }

    fn blame(&self, ctx: &Ctx) -> Action {
        if self.is_dir() {
            return Action::None;
        }
        super::open_blame(ctx, &self.entry.path, Some(&self.entry.content_rev()))
    }

    fn details(&self, ctx: &Ctx) -> Action {
        commit_details(ctx, &self.entry.commit.id)
    }

    fn restore(&self, _ctx: &Ctx) -> Action {
        if self.is_dir() {
            return Action::None;
        }
        let rev = self.entry.content_rev();
        let rev_path = self.entry.path.clone();
        let dest = self.target.path().to_string();
        let short = self.entry.commit.short.clone();
        let msg = format!(
            "Replace the local content of {dest} with the version from {short}{}?\n\nUncommitted changes to this file will be lost.",
            if rev_path != dest { format!(" (then named {rev_path})") } else { String::new() }
        );
        Action::popup(
            ConfirmPopup::new("Get revision", msg, move |_| {
                Action::spawn("Get revision", move |repo| {
                    repo.restore_file(&rev, &rev_path, &dest)
                        .map(|_| format!("Restored {dest} from {short}"))
                })
            })
            .danger(),
        )
    }
}

impl Popup for HistoryPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.poll();
        if let Some(cmd) = self.pending.take() {
            self.run_cmd(cmd);
        }
        self.maybe_load_more(ctx);
        f.render_widget(Clear, area);
        let list_h = (area.height * 2 / 5).max(6);
        let [top, bottom] =
            Layout::vertical([Constraint::Length(list_h), Constraint::Min(3)]).areas(area);
        self.draw_list(f, top, ctx);
        self.draw_preview(f, bottom, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filter_focus {
            return self.handle_filter_key(key);
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.preview_focus = !self.preview_focus;
            return Action::None;
        }
        if self.preview_focus && self.handle_preview_key(key) {
            return Action::None;
        }
        if !self.preview_focus && self.nav.handle_key(&key, self.visible().len()) {
            return Action::None;
        }
        let snap = self.snapshot();
        let on = |f: fn(&Snapshot, &Ctx) -> Action| {
            snap.as_ref().map(|s| f(s, ctx)).unwrap_or(Action::None)
        };
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.set("");
                Action::None
            }
            KeyCode::Esc if self.preview_focus => {
                self.preview_focus = false;
                Action::None
            }
            KeyCode::Esc | KeyCode::Char('q') => Action::ClosePopup,
            KeyCode::Enter | KeyCode::Char('d') => on(Snapshot::full_diff),
            KeyCode::Char('p') => on(Snapshot::compare_previous),
            KeyCode::Char('w') => on(Snapshot::compare_worktree),
            KeyCode::Char('o') => on(Snapshot::show_at_revision),
            KeyCode::Char('b') => on(Snapshot::blame),
            KeyCode::Char('c') => on(Snapshot::details),
            KeyCode::Char('R') => on(Snapshot::restore),
            KeyCode::Char('/') => {
                self.run_cmd(Cmd::Filter);
                Action::None
            }
            KeyCode::Char('r') => {
                self.preview_cache.clear();
                self.reload(ctx);
                Action::None
            }
            KeyCode::Char('.') | KeyCode::Char('m') => self.menu(),
            _ => Action::None,
        }
    }

    fn refresh(&mut self, ctx: &Ctx) {
        self.preview_cache.clear();
        self.reload(ctx);
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("↑↓/jk", "select"),
            ("Tab", "list/diff"),
            ("Enter", "diff"),
            ("p", "vs previous"),
            ("w", "vs local"),
            ("o", "show file"),
            ("b", "annotate"),
            ("c", "details"),
            ("R", "get revision"),
            ("/", "filter"),
            ("r", "reload"),
            (".", "menu"),
            ("Esc", "close"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::*;

    fn wait(p: &mut HistoryPopup) {
        for _ in 0..400 {
            if !p.loader.is_loading() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            p.poll();
        }
        panic!("history did not load");
    }

    fn ctx_for(repo: &Repo) -> Ctx {
        Ctx::new(repo.clone(), crate::config::Config::default())
    }

    #[test]
    fn loads_file_directory_and_line_histories() {
        let (_d, repo) = temp_repo();
        let c1 = commit_files(&repo, &[("d/a.txt", "1\n2\n"), ("d/b.txt", "x\n")], "one");
        let c2 = commit_files(&repo, &[("d/a.txt", "1\nTWO\n")], "two");
        commit_files(&repo, &[("d/b.txt", "y\n")], "three");
        let ctx = ctx_for(&repo);

        let mut p = HistoryPopup::new(&ctx, Target::File("d/a.txt".into()));
        wait(&mut p);
        assert_eq!(p.entries.len(), 2);
        assert!(p.complete);
        assert_eq!(p.entries[0].commit.id, c2);
        let s = p.snapshot().unwrap();
        assert_eq!(
            s.older.as_ref().map(|o| o.commit.id.as_str()),
            Some(c1.as_str())
        );
        assert!(matches!(p.preview(&ctx), Some(Ok(d)) if !d.is_empty()));

        p.filter.set("one");
        assert_eq!(p.visible(), vec![1]);
        assert_eq!(
            p.selected().map(|e| e.commit.id.as_str()),
            Some(c1.as_str())
        );

        let mut d = HistoryPopup::new(&ctx, Target::Directory("d".into()));
        wait(&mut d);
        assert_eq!(d.entries.len(), 3);

        let mut l = HistoryPopup::new(
            &ctx,
            Target::Lines {
                path: "d/a.txt".into(),
                start: 2,
                end: 2,
                rev: None,
            },
        );
        wait(&mut l);
        assert_eq!(l.entries.len(), 2);
        assert!(l.entries[0].diffs.as_ref().is_some_and(|d| !d.is_empty()));
        assert_eq!(l.entries[1].status, 'A');
    }

    #[test]
    fn content_rev_for_deletions() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("gone.txt", "x\n")], "add");
        repo.run(&["rm", "-q", "gone.txt"]).unwrap();
        repo.run(&["commit", "-q", "-m", "delete"]).unwrap();
        let ctx = ctx_for(&repo);
        let mut p = HistoryPopup::new(&ctx, Target::File("gone.txt".into()));
        wait(&mut p);
        assert_eq!(p.entries[0].status, 'D');
        let rev = p.entries[0].content_rev();
        assert!(rev.ends_with('^'));
        assert_eq!(repo.show_file(&rev, "gone.txt").unwrap(), b"x\n");
    }
}
