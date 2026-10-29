//! Tracked file browser with blame/annotate, file history, line history, file at revision.
//!
//! The tab shows the project tree (`git ls-files` plus untracked files) with status colors and a
//! content preview. Entry points used by other features: [`open_blame`], [`open_file_history`];
//! this module additionally offers [`open_line_history`] and [`open_file_at_revision`].

mod blame_view;
mod common;
mod content_view;
mod history_view;
mod tree;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use self::blame_view::BlamePopup;
use self::common::{expand_tabs, open_in_editor, parse_line_range, state_color, LineNav};
use self::content_view::ContentPopup;
use self::history_view::{HistoryPopup, Target};
use self::tree::{FileItem, Node, Row};
use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::file_history::{is_binary, text_lines};
use crate::git::status::FileState;
use crate::git::Repo;
use crate::ui::bg::Background;
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{InputPopup, MenuItem, MenuPopup};
use crate::ui::widgets::{panel, TextInput};
use crate::ui::{KeyHelp, PaletteItem, View};
use crate::util::truncate;

/// Bytes of a file read for the preview pane.
const PREVIEW_BYTES: u64 = 256 * 1024;
/// Maximum rows in the filtered (flat) list.
const FILTER_LIMIT: usize = 5000;

/// What the preview pane shows for the selected row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PreviewBody {
    Text { lines: Vec<String>, truncated: bool },
    Binary(u64),
    Missing,
    Dir { files: usize, changed: usize },
    Error(String),
}

/// Commands from the context menu that change the view's own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewCmd {
    Toggle,
    ExpandAll,
    CollapseAll,
    Filter,
}

/// The Files tab.
pub struct FilesView {
    items: Vec<FileItem>,
    root: Node,
    expanded: HashSet<String>,
    rows: Vec<Row>,
    nav: LineNav,
    filter: TextInput,
    filter_focus: bool,
    preview_focus: bool,
    preview: Option<(String, PreviewBody)>,
    preview_scroll: usize,
    preview_height: usize,
    list_area: Rect,
    preview_area: Rect,
    pending: Rc<Cell<Option<ViewCmd>>>,
    loader: Background<Result<Vec<FileItem>, String>>,
    load_error: Option<String>,
}

impl FilesView {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            items: Vec::new(),
            root: Node::default(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            nav: LineNav::default(),
            filter: TextInput::default(),
            filter_focus: false,
            preview_focus: false,
            preview: None,
            preview_scroll: 0,
            preview_height: 0,
            list_area: Rect::default(),
            preview_area: Rect::default(),
            pending: Rc::new(Cell::new(None)),
            loader: Background::new(),
            load_error: None,
        }
    }

    fn selected(&self) -> Option<&Row> {
        self.rows.get(self.nav.cursor)
    }

    fn filtering(&self) -> bool {
        !self.filter.value().trim().is_empty()
    }

    /// Recompute the visible rows, keeping the selection on `keep` (a path) when possible.
    fn rebuild(&mut self, keep: Option<String>) {
        self.rows = if self.filtering() {
            tree::filter(&self.items, &self.filter.value(), FILTER_LIMIT)
        } else {
            tree::flatten(&self.root, &self.expanded)
        };
        if let Some(p) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.path == p) {
                self.nav.cursor = i;
            }
        }
        self.nav.cursor = self.nav.cursor.min(self.rows.len().saturating_sub(1));
    }

    /// Expand the ancestors of `path` and select it in the tree.
    fn reveal(&mut self, path: &str) {
        self.expanded.extend(tree::ancestors(path));
        self.rebuild(Some(path.to_string()));
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.nav.jump(i, self.rows.len());
        }
    }

    fn toggle(&mut self) {
        let Some(r) = self.selected().cloned() else {
            return;
        };
        if !r.is_dir {
            return;
        }
        if !self.expanded.remove(&r.path) {
            self.expanded.insert(r.path.clone());
        }
        self.rebuild(Some(r.path));
    }

    fn collapse_or_parent(&mut self) {
        let Some(r) = self.selected().cloned() else {
            return;
        };
        if r.is_dir && r.expanded {
            self.toggle();
            return;
        }
        let mut p = tree::parent_of(&r.path).to_string();
        while !p.is_empty() {
            if let Some(i) = self.rows.iter().position(|x| x.path == p) {
                self.nav.cursor = i;
                return;
            }
            p = tree::parent_of(&p).to_string();
        }
    }

    fn expand_or_child(&mut self) {
        let Some(r) = self.selected().cloned() else {
            return;
        };
        if !r.is_dir {
            return;
        }
        if r.expanded {
            self.nav.move_by(1, self.rows.len());
        } else {
            self.toggle();
        }
    }

    fn run_cmd(&mut self, cmd: ViewCmd) {
        let keep = self.selected().map(|r| r.path.clone());
        match cmd {
            ViewCmd::Toggle => return self.toggle(),
            ViewCmd::ExpandAll => self.expanded.extend(tree::all_dirs(&self.root)),
            ViewCmd::CollapseAll => self.expanded.clear(),
            ViewCmd::Filter => {
                self.filter_focus = true;
                return;
            }
        }
        self.rebuild(keep);
    }

    /// Changed (non-clean) files at or below the selected row, for shelve.
    fn changed_paths_under(&self, path: &str, is_dir: bool) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.state.is_some_and(|s| s != FileState::Unmodified))
            .filter(|i| {
                if is_dir {
                    i.path.starts_with(&format!("{path}/"))
                } else {
                    i.path == path
                }
            })
            .map(|i| i.path.clone())
            .collect()
    }

    fn menu(&self) -> Action {
        let Some(r) = self.selected().cloned() else {
            return Action::None;
        };
        let pending = self.pending.clone();
        let set = move |c: ViewCmd| {
            let p = pending.clone();
            move |_: &Ctx| {
                p.set(Some(c));
                Action::None
            }
        };
        let changed = self.changed_paths_under(&r.path, r.is_dir);
        let untracked = r.state == Some(FileState::Untracked);
        let conflicted = r.state == Some(FileState::Conflicted);
        let p = r.path.clone();
        let mut items: Vec<MenuItem> = Vec::new();
        if r.is_dir {
            items.push(MenuItem::new(
                if r.expanded { "Collapse" } else { "Expand" },
                "Enter",
                set(ViewCmd::Toggle),
            ));
            items.push(MenuItem::new(
                "Show history",
                "h",
                with_path(&p, open_file_history),
            ));
        } else {
            items.push(
                MenuItem::new(
                    "Annotate (blame)",
                    "b",
                    with_path(&p, |c, p| open_blame(c, p, None)),
                )
                .enabled(!untracked),
            );
            items.push(
                MenuItem::new("Show history", "h", with_path(&p, open_file_history))
                    .enabled(!untracked),
            );
            items.push(
                MenuItem::new(
                    "Show history for lines…",
                    "L",
                    with_path(&p, ask_line_history),
                )
                .enabled(!untracked),
            );
            items.push(
                MenuItem::new(
                    "Show file at revision…",
                    "r",
                    with_path(&p, ask_file_at_revision),
                )
                .enabled(!untracked),
            );
            items.push(
                MenuItem::new(
                    "Compare with revision…",
                    "w",
                    with_path(&p, ask_compare_with_revision),
                )
                .enabled(!untracked),
            );
            items.push(MenuItem::new(
                "Local history",
                "l",
                with_path(&p, crate::ui::views::local_history::open_for_path),
            ));
            items.push(MenuItem::new(
                "Open in editor",
                "o",
                with_path(&p, |c, p| open_in_editor(c, p, None)),
            ));
            items.push(
                MenuItem::new(
                    "Resolve conflict…",
                    "x",
                    with_path(&p, crate::ui::views::conflicts::open_resolver),
                )
                .enabled(conflicted),
            );
        }
        let diff_paths = changed.clone();
        items.push(
            MenuItem::new("Show local changes", "d", move |_| {
                diff_view::open("Local changes", DiffSource::Local { paths: diff_paths })
            })
            .enabled(!changed.is_empty()),
        );
        let shelve = changed.clone();
        items.push(
            MenuItem::new("Shelve changes…", "s", move |ctx| {
                crate::ui::views::shelf::shelve_paths(ctx, shelve)
            })
            .enabled(!changed.is_empty() && !conflicted),
        );
        items.push(MenuItem::new("Filter files", "/", set(ViewCmd::Filter)));
        items.push(
            MenuItem::new("Expand all", "E", set(ViewCmd::ExpandAll)).enabled(!self.filtering()),
        );
        items.push(
            MenuItem::new("Collapse all", "C", set(ViewCmd::CollapseAll))
                .enabled(!self.filtering()),
        );
        Action::popup(MenuPopup::new(truncate(&r.path, 50), items))
    }

    /// Apply a finished refresh.
    fn apply_loaded(&mut self) {
        let Some(res) = self.loader.poll() else {
            return;
        };
        match res {
            Ok(items) => {
                self.load_error = None;
                let keep = self.selected().map(|r| r.path.clone());
                self.items = items;
                self.root = tree::build(&self.items);
                self.preview = None;
                self.rebuild(keep);
            }
            Err(e) => self.load_error = Some(e),
        }
    }

    /// Load the preview for the selected row if it changed.
    fn update_preview(&mut self, ctx: &Ctx) {
        let Some(r) = self.selected().cloned() else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_some_and(|(p, _)| *p == r.path) {
            return;
        }
        self.preview_scroll = 0;
        let body = if r.is_dir {
            PreviewBody::Dir {
                files: r.files,
                changed: r.changed,
            }
        } else {
            read_preview(&ctx.repo.path(&r.path))
        };
        self.preview = Some((r.path, body));
    }

    fn draw_tree(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let title = if self.filtering() {
            format!(
                " Files — {} match{} ",
                self.rows.len(),
                if self.rows.len() == 1 { "" } else { "es" }
            )
        } else {
            format!(" Files — {} ", self.items.len())
        };
        let block = panel(title, !self.preview_focus, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let show_filter = self.filter_focus || self.filtering();
        let [filter_area, list] = Layout::vertical([
            Constraint::Length(u16::from(show_filter)),
            Constraint::Min(1),
        ])
        .areas(inner);
        if show_filter {
            let [l, r] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(filter_area);
            f.render_widget(Span::styled("/ ", t.key()), l);
            self.filter.render(f, r, t.text(), self.filter_focus);
        }
        self.list_area = list;
        if let Some(e) = &self.load_error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                list,
            );
            return;
        }
        if self.rows.is_empty() {
            let msg = if self.loader.busy() {
                "Loading…"
            } else if self.filtering() {
                "No files match"
            } else {
                "No files in this repository"
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), list);
            return;
        }
        self.nav.fit(list.height as usize, self.rows.len());
        let lines: Vec<Line> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.nav.offset)
            .take(list.height as usize)
            .map(|(i, r)| {
                let mut line = row_line(r, self.filtering(), ctx);
                if i == self.nav.cursor {
                    line = line.style(if self.preview_focus {
                        Style::default().bg(t.selection_bg)
                    } else {
                        t.selected()
                    });
                }
                line
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        self.preview_area = area;
        let name = self
            .preview
            .as_ref()
            .map(|(p, _)| p.clone())
            .unwrap_or_default();
        let block = panel(
            format!(
                " {} ",
                truncate(&name, area.width.saturating_sub(4) as usize)
            ),
            self.preview_focus,
            t,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.preview_height = inner.height as usize;
        let Some((_, body)) = &self.preview else {
            return;
        };
        let dim = |s: String| Paragraph::new(s).style(t.dimmed());
        match body {
            PreviewBody::Text { lines, truncated } => {
                let max = lines.len().saturating_sub(inner.height as usize);
                self.preview_scroll = self.preview_scroll.min(max + usize::from(*truncated));
                let digits = lines.len().max(1).to_string().len();
                let mut out: Vec<Line> = lines
                    .iter()
                    .enumerate()
                    .skip(self.preview_scroll)
                    .take(inner.height as usize)
                    .map(|(i, l)| {
                        Line::from(vec![
                            Span::styled(format!("{:>digits$} ", i + 1), t.dimmed()),
                            Span::styled(expand_tabs(l), t.text()),
                        ])
                    })
                    .collect();
                if lines.is_empty() {
                    out.push(Line::styled("(empty file)", t.dimmed()));
                } else if *truncated && out.len() < inner.height as usize {
                    out.push(Line::styled(
                        "… (preview truncated — open the file to see more)",
                        t.dimmed(),
                    ));
                }
                f.render_widget(Paragraph::new(out), inner);
            }
            PreviewBody::Binary(n) => {
                f.render_widget(dim(format!("Binary file ({n} bytes)")), inner)
            }
            PreviewBody::Missing => f.render_widget(
                dim("The file does not exist in the work tree (deleted locally)".into()),
                inner,
            ),
            PreviewBody::Dir { files, changed } => f.render_widget(
                dim(format!("Directory: {files} file(s), {changed} changed")),
                inner,
            ),
            PreviewBody::Error(e) => f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                inner,
            ),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => self.clear_filter(),
            KeyCode::Enter | KeyCode::Tab => self.filter_focus = false,
            KeyCode::Up => self.nav.move_by(-1, self.rows.len()),
            KeyCode::Down => self.nav.move_by(1, self.rows.len()),
            _ => {
                if self.filter.handle_key(&key) {
                    self.nav.cursor = 0;
                    self.nav.offset = 0;
                    self.rebuild(None);
                }
            }
        }
        Action::None
    }

    /// Leave filter mode and reveal the file that was selected in the flat list.
    fn clear_filter(&mut self) {
        let keep = self.selected().map(|r| r.path.clone());
        self.filter.set("");
        self.filter_focus = false;
        match keep {
            Some(p) => self.reveal(&p),
            None => self.rebuild(None),
        }
    }

    fn handle_preview_key(&mut self, key: &KeyEvent) -> bool {
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
            KeyCode::Esc => self.preview_focus = false,
            _ => return false,
        }
        true
    }

    /// Action keys that work on the selected row in both panes.
    fn handle_action_key(&mut self, key: &KeyEvent, ctx: &Ctx) -> Action {
        let Some(r) = self.selected().cloned() else {
            return match key.code {
                KeyCode::Char('/') => {
                    self.filter_focus = true;
                    Action::None
                }
                _ => Action::None,
            };
        };
        let untracked = r.state == Some(FileState::Untracked);
        match key.code {
            KeyCode::Enter if r.is_dir => {
                self.toggle();
                Action::None
            }
            KeyCode::Enter | KeyCode::Char('b') if !r.is_dir => open_blame(ctx, &r.path, None),
            KeyCode::Char('h') if !untracked => open_file_history(ctx, &r.path),
            KeyCode::Char('l') if !r.is_dir => {
                crate::ui::views::local_history::open_for_path(ctx, &r.path)
            }
            KeyCode::Char('o') if !r.is_dir => open_in_editor(ctx, &r.path, None),
            KeyCode::Char('L') if !r.is_dir && !untracked => ask_line_history(ctx, &r.path),
            KeyCode::Char('r') if !r.is_dir && !untracked => ask_file_at_revision(ctx, &r.path),
            KeyCode::Char('w') if !r.is_dir && !untracked => {
                ask_compare_with_revision(ctx, &r.path)
            }
            KeyCode::Char('d') => {
                let paths = self.changed_paths_under(&r.path, r.is_dir);
                if paths.is_empty() {
                    Action::Toast("No local changes".into())
                } else {
                    diff_view::open(
                        format!("Local changes: {}", r.path),
                        DiffSource::Local { paths },
                    )
                }
            }
            KeyCode::Char('h' | 'L' | 'r' | 'w' | 'b') if untracked => {
                Action::Toast(format!("{} is not under version control", r.path))
            }
            KeyCode::Char('/') => {
                self.filter_focus = true;
                Action::None
            }
            KeyCode::Char('.') | KeyCode::Char('m') => self.menu(),
            _ => Action::None,
        }
    }
}

/// Render one tree/list row.
fn row_line(r: &Row, flat: bool, ctx: &Ctx) -> Line<'static> {
    let t = &ctx.theme;
    let indent = "  ".repeat(r.depth);
    let marker = if r.is_dir {
        if r.expanded {
            "▾ "
        } else {
            "▸ "
        }
    } else {
        "  "
    };
    let mut spans = vec![Span::raw(indent), Span::styled(marker, t.dimmed())];
    let color = r.state.map(|s| state_color(t, s)).unwrap_or(t.fg);
    if r.is_dir {
        let style = if r.changed > 0 {
            Style::default().fg(t.modified)
        } else {
            t.text()
        };
        spans.push(Span::styled(
            r.name.clone(),
            style.add_modifier(Modifier::BOLD),
        ));
        if r.changed > 0 {
            spans.push(Span::styled(format!("  {} changed", r.changed), t.dimmed()));
        }
    } else if flat {
        let (dir, name) = match r.path.rsplit_once('/') {
            Some((d, n)) => (format!("{d}/"), n.to_string()),
            None => (String::new(), r.path.clone()),
        };
        spans.push(Span::styled(name, Style::default().fg(color)));
        if !dir.is_empty() {
            spans.push(Span::styled(format!("  {dir}"), t.dimmed()));
        }
    } else {
        let mut st = Style::default().fg(color);
        if r.state == Some(FileState::Deleted) {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        spans.push(Span::styled(r.name.clone(), st));
    }
    if let Some(s) = r.state.filter(|s| *s != FileState::Unmodified) {
        spans.push(Span::styled(
            format!("  {}", s.letter()),
            Style::default().fg(color),
        ));
    }
    Line::from(spans)
}

/// Read the beginning of a work tree file for the preview pane.
fn read_preview(abs: &Path) -> PreviewBody {
    let meta = match std::fs::metadata(abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PreviewBody::Missing,
        Err(e) => return PreviewBody::Error(e.to_string()),
    };
    if meta.is_dir() {
        return PreviewBody::Dir {
            files: 0,
            changed: 0,
        };
    }
    let mut buf = Vec::new();
    let read = std::fs::File::open(abs).and_then(|f| f.take(PREVIEW_BYTES).read_to_end(&mut buf));
    if let Err(e) = read {
        return PreviewBody::Error(e.to_string());
    }
    if is_binary(&buf) {
        return PreviewBody::Binary(meta.len());
    }
    let truncated = meta.len() > PREVIEW_BYTES;
    let mut lines = text_lines(&buf);
    if truncated && !buf.ends_with(b"\n") {
        // The last line was cut in the middle.
        lines.pop();
    }
    PreviewBody::Text { lines, truncated }
}

/// Wrap a `(ctx, path)` function as a menu callback.
fn with_path(
    path: &str,
    f: impl Fn(&Ctx, &str) -> Action + 'static,
) -> impl FnOnce(&Ctx) -> Action + 'static {
    let p = path.to_string();
    move |ctx| f(ctx, &p)
}

/// Load the file list: tracked files plus untracked ones, with status.
fn load_items(repo: &Repo) -> anyhow::Result<Vec<FileItem>> {
    let tracked = tracked_files(repo)?;
    let status = repo.status()?;
    let mut states: HashMap<String, FileState> = HashMap::new();
    let mut untracked = Vec::new();
    for e in status.entries.iter().filter(|e| !e.is_ignored()) {
        if e.is_untracked() {
            untracked.push(e.path.clone());
        }
        states.insert(
            e.path.clone(),
            if e.is_untracked() {
                FileState::Untracked
            } else {
                e.display_state()
            },
        );
    }
    let mut seen = HashSet::new();
    Ok(tracked
        .into_iter()
        .chain(untracked)
        .filter(|p| seen.insert(p.clone()))
        .map(|p| FileItem {
            state: states.get(&p).copied(),
            path: p,
        })
        .collect())
}

impl View for FilesView {
    fn title(&self) -> String {
        "Files".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        self.loader.start(&ctx.repo, |repo| {
            load_items(repo).map_err(|e| format!("{e:#}"))
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
        if let Some(cmd) = self.pending.take() {
            self.run_cmd(cmd);
        }
        self.update_preview(ctx);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        self.draw_tree(f, left, ctx);
        self.draw_preview(f, right, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filter_focus {
            return self.handle_filter_key(key);
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.preview_focus = !self.preview_focus;
            return Action::None;
        }
        if self.preview_focus {
            if self.handle_preview_key(&key) {
                return Action::None;
            }
            return self.handle_action_key(&key, ctx);
        }
        let len = self.rows.len();
        if self.nav.handle_key(&key, len) {
            return Action::None;
        }
        match key.code {
            KeyCode::Left => self.collapse_or_parent(),
            KeyCode::Right => self.expand_or_child(),
            KeyCode::Esc if self.filtering() => self.clear_filter(),
            KeyCode::Char('E') if !self.filtering() => self.run_cmd(ViewCmd::ExpandAll),
            KeyCode::Char('C') if !self.filtering() => self.run_cmd(ViewCmd::CollapseAll),
            _ => return self.handle_action_key(&key, ctx),
        }
        Action::None
    }

    fn handle_mouse(&mut self, ev: MouseEvent, _ctx: &Ctx) -> Action {
        if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::None;
        }
        let pos = Position::new(ev.column, ev.row);
        if self.list_area.contains(pos) {
            let i = self.nav.offset + (ev.row - self.list_area.y) as usize;
            if i < self.rows.len() {
                if i == self.nav.cursor && self.rows[i].is_dir {
                    self.toggle();
                } else {
                    self.nav.cursor = i;
                }
            }
            self.preview_focus = false;
            self.filter_focus = false;
        } else if self.preview_area.contains(pos) {
            self.preview_focus = true;
            self.filter_focus = false;
        } else {
            return Action::None;
        }
        Action::Batch(Vec::new())
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("↑↓/jk", "move"),
            ("←→", "collapse/expand"),
            ("Enter", "annotate / toggle"),
            ("h", "history"),
            ("l", "local history"),
            ("o", "open in editor"),
            ("/", "filter"),
            (".", "menu"),
            ("L", "history for lines"),
            ("r", "file at revision"),
            ("w", "compare with revision"),
            ("d", "local changes"),
            ("b", "annotate"),
            ("E/C", "expand/collapse all"),
            ("Tab", "tree/preview"),
            ("Esc", "clear filter"),
        ]
    }

    fn captures_input(&self) -> bool {
        self.filter_focus
    }
}

// ----- entry points ------------------------------------------------------------------------

/// Turn user input (`./a`, `a\b`, an absolute path inside the work tree) into a repo-relative path.
fn normalize_path(ctx: &Ctx, path: &str) -> String {
    let p = Path::new(path.trim());
    let rel = p
        .strip_prefix(&ctx.repo.root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.trim().to_string());
    rel.replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn is_tracked(ctx: &Ctx, path: &str) -> bool {
    ctx.repo
        .run(&["ls-files", "--error-unmatch", "--", path])
        .is_ok()
}

/// Open blame/annotate for `path` at `rev` (None = work tree).
pub fn open_blame(ctx: &Ctx, path: &str, rev: Option<&str>) -> Action {
    let path = normalize_path(ctx, path);
    if path.is_empty() {
        return Action::Error("No file to annotate".into());
    }
    let mut rev = rev.map(str::to_string);
    if rev.is_none() {
        let abs = ctx.repo.path(&path);
        if abs.is_dir() {
            return Action::Error(format!("{path} is a directory; choose a file to annotate"));
        }
        if !abs.exists() {
            if ctx.repo.head_commit().is_none() {
                return Action::Error(format!("{path} does not exist"));
            }
            // Deleted locally: annotate the last committed version.
            rev = Some("HEAD".into());
        } else if !is_tracked(ctx, &path) {
            return Action::Error(format!(
                "{path} is not under version control — nothing to annotate"
            ));
        }
    }
    Action::popup(BlamePopup::new(ctx, path, rev, None))
}

/// Open the history of `path` (follows renames). Directories show every commit touching them.
pub fn open_file_history(ctx: &Ctx, path: &str) -> Action {
    let path = normalize_path(ctx, path);
    if path.is_empty() {
        return Action::Error("No file selected for history".into());
    }
    let target = if ctx.repo.path(&path).is_dir() {
        Target::Directory(path)
    } else {
        Target::File(path)
    };
    Action::popup(HistoryPopup::new(ctx, target))
}

/// Open the history of lines `start..=end` (1-based) of `path` as seen at `rev` (HEAD when `None`).
pub fn open_line_history(ctx: &Ctx, path: &str, start: u32, end: u32, rev: Option<&str>) -> Action {
    let path = normalize_path(ctx, path);
    if start == 0 || end < start {
        return Action::Error(format!("Invalid line range {start}-{end}"));
    }
    let target = Target::Lines {
        path,
        start,
        end,
        rev: rev.map(str::to_string),
    };
    Action::popup(HistoryPopup::new(ctx, target))
}

/// Show the content of `path` at `rev` in a read-only viewer.
pub fn open_file_at_revision(ctx: &Ctx, path: &str, rev: &str) -> Action {
    let path = normalize_path(ctx, path);
    Action::popup(ContentPopup::new(ctx, path, rev.to_string()))
}

/// Ask for a line range, then open the line history of `path`.
fn ask_line_history(_ctx: &Ctx, path: &str) -> Action {
    let p = path.to_string();
    Action::popup(
        InputPopup::new(
            "History for lines",
            "",
            move |s, ctx| match parse_line_range(&s) {
                Some((a, b)) => open_line_history(ctx, &p, a, b, None),
                None => Action::Error(format!("Not a line range: {s} (use e.g. 10-20)")),
            },
        )
        .prompt(format!("Lines of {path} to track, e.g. 10-20 or 42")),
    )
}

/// Ask for a revision (validated), then run `then(ctx, rev)`.
fn ask_revision(
    title: &str,
    prompt: String,
    then: impl FnOnce(&Ctx, String) -> Action + 'static,
) -> Action {
    Action::popup(
        InputPopup::new(title, "HEAD", move |s, ctx| {
            let rev = s.trim().to_string();
            match ctx.repo.rev_parse(&rev) {
                Ok(_) => then(ctx, rev),
                Err(_) => Action::Error(format!("Unknown revision: {rev}")),
            }
        })
        .prompt(prompt),
    )
}

fn ask_file_at_revision(_ctx: &Ctx, path: &str) -> Action {
    let p = path.to_string();
    ask_revision(
        "Show file at revision",
        format!("Revision of {path} (branch, tag, hash, HEAD~2…)"),
        move |ctx, rev| open_file_at_revision(ctx, &p, &rev),
    )
}

fn ask_compare_with_revision(_ctx: &Ctx, path: &str) -> Action {
    let p = path.to_string();
    ask_revision(
        "Compare with revision",
        format!("Compare local {path} with revision (branch, tag, hash…)"),
        move |_, rev| {
            diff_view::open(
                format!("{p} @ {rev} ↔ local"),
                DiffSource::RevToWorktree {
                    rev,
                    paths: vec![p.clone()],
                },
            )
        },
    )
}

// ----- palette -----------------------------------------------------------------------------

type Pick = Rc<dyn Fn(&Ctx, String) -> Action>;

fn tracked_files(repo: &Repo) -> anyhow::Result<Vec<String>> {
    let out = repo.run_bytes(&["ls-files", "-z", "--cached"])?;
    let mut seen = HashSet::new();
    Ok(String::from_utf8_lossy(&out)
        .split('\0')
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_string()))
        .map(str::to_string)
        .collect())
}

fn files_at_revision(ctx: &Ctx, rev: &str) -> anyhow::Result<Vec<String>> {
    let out = ctx
        .repo
        .run_bytes(&["ls-tree", "-r", "-z", "--name-only", "--full-tree", rev])?;
    Ok(String::from_utf8_lossy(&out)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// Fuzzy chooser over `files`, with a first entry to type a path by hand.
fn choose_file(title: &str, files: Vec<String>, pick: Pick) -> Action {
    let typed = pick.clone();
    let t = title.to_string();
    let mut items = vec![MenuItem::new("Type a path…", "", move |_| {
        Action::popup(
            InputPopup::new(t, "", move |s, ctx| {
                let p = normalize_path(ctx, &s);
                typed(ctx, p)
            })
            .prompt("Repository-relative path"),
        )
    })];
    items.extend(files.into_iter().map(|f| {
        let pick = pick.clone();
        MenuItem::new(f.clone(), "", move |ctx| pick(ctx, f))
    }));
    Action::popup(MenuPopup::new(title, items).filterable())
}

fn choose_tracked(ctx: &Ctx, title: &str, pick: Pick) -> Action {
    match tracked_files(&ctx.repo) {
        Ok(files) => choose_file(title, files, pick),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn palette_annotate(ctx: &Ctx) -> Action {
    choose_tracked(
        ctx,
        "Annotate file",
        Rc::new(|ctx, p| open_blame(ctx, &p, None)),
    )
}

fn palette_annotate_at_revision(_ctx: &Ctx) -> Action {
    ask_revision(
        "Annotate at revision",
        "Revision to annotate (branch, tag, hash…)".into(),
        |ctx, rev| match files_at_revision(ctx, &rev) {
            Ok(files) => {
                let r = rev.clone();
                choose_file(
                    &format!("Annotate file at {}", truncate(&rev, 16)),
                    files,
                    Rc::new(move |ctx, p| open_blame(ctx, &p, Some(&r))),
                )
            }
            Err(e) => Action::Error(format!("{e:#}")),
        },
    )
}

fn palette_history(ctx: &Ctx) -> Action {
    choose_tracked(
        ctx,
        "Show file history",
        Rc::new(|ctx, p| open_file_history(ctx, &p)),
    )
}

fn palette_line_history(ctx: &Ctx) -> Action {
    choose_tracked(
        ctx,
        "Show history for lines",
        Rc::new(|ctx, p| ask_line_history(ctx, &p)),
    )
}

fn palette_file_at_revision(_ctx: &Ctx) -> Action {
    ask_revision(
        "Show file at revision",
        "Revision (branch, tag, hash, HEAD~2…)".into(),
        |ctx, rev| match files_at_revision(ctx, &rev) {
            Ok(files) => {
                let r = rev.clone();
                choose_file(
                    &format!("File at {}", truncate(&rev, 16)),
                    files,
                    Rc::new(move |ctx, p| open_file_at_revision(ctx, &p, &r)),
                )
            }
            Err(e) => Action::Error(format!("{e:#}")),
        },
    )
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Annotate file…",
            run: palette_annotate,
        },
        PaletteItem {
            name: "Annotate file at revision…",
            run: palette_annotate_at_revision,
        },
        PaletteItem {
            name: "Show file history…",
            run: palette_history,
        },
        PaletteItem {
            name: "Show history for lines…",
            run: palette_line_history,
        },
        PaletteItem {
            name: "Show file at revision…",
            run: palette_file_at_revision,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::*;
    use crate::ui::Popup;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn setup() -> (TempDir, Ctx) {
        let (d, repo) = temp_repo();
        commit_files(
            &repo,
            &[
                ("src/main.rs", "fn main() {}\r\n"),
                ("src/ui/app.rs", "x\n"),
                ("README.md", "hi\n"),
            ],
            "init",
        );
        write(&repo, "src/main.rs", "fn main() { changed }\n");
        write(&repo, "notes.txt", "untracked\n");
        std::fs::remove_file(repo.path("README.md")).unwrap();
        (d, Ctx::new(repo, crate::config::Config::default()))
    }

    #[test]
    fn view_loads_tree_with_states_and_navigates() {
        let (_d, ctx) = setup();
        let mut v = FilesView::new(&ctx);
        v.refresh(&ctx).unwrap();
        let state = |p: &str| v.items.iter().find(|i| i.path == p).and_then(|i| i.state);
        assert_eq!(state("src/main.rs"), Some(FileState::Modified));
        assert_eq!(state("notes.txt"), Some(FileState::Untracked));
        assert_eq!(state("README.md"), Some(FileState::Deleted));
        assert_eq!(state("src/ui/app.rs"), None);
        let paths: Vec<&str> = v.rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["src", "notes.txt", "README.md"]);

        v.handle_key(key(KeyCode::Right), &ctx);
        assert!(v.rows[0].expanded);
        assert_eq!(v.rows.len(), 5);
        v.handle_key(key(KeyCode::Right), &ctx);
        assert_eq!(v.nav.cursor, 1);
        v.handle_key(key(KeyCode::Left), &ctx);
        assert_eq!(v.nav.cursor, 0);
        v.handle_key(key(KeyCode::Left), &ctx);
        assert_eq!(v.rows.len(), 3);

        v.handle_key(key(KeyCode::Char('/')), &ctx);
        assert!(v.captures_input());
        for c in "app".chars() {
            v.handle_key(key(KeyCode::Char(c)), &ctx);
        }
        assert_eq!(v.rows[0].path, "src/ui/app.rs");
        v.handle_key(key(KeyCode::Enter), &ctx);
        assert!(!v.captures_input());
        v.handle_key(key(KeyCode::Esc), &ctx);
        assert!(!v.filtering());
        assert_eq!(v.selected().map(|r| r.path.as_str()), Some("src/ui/app.rs"));

        v.run_cmd(ViewCmd::ExpandAll);
        assert_eq!(v.rows.len(), 6);
        v.run_cmd(ViewCmd::CollapseAll);
        assert_eq!(v.rows.len(), 3);
    }

    #[test]
    fn preview_reads_text_binary_and_missing() {
        let (_d, ctx) = setup();
        match read_preview(&ctx.repo.path("src/main.rs")) {
            PreviewBody::Text { lines, truncated } => {
                assert_eq!(lines, vec!["fn main() { changed }"]);
                assert!(!truncated);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            read_preview(&ctx.repo.path("README.md")),
            PreviewBody::Missing
        );
        std::fs::write(ctx.repo.path("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        assert_eq!(
            read_preview(&ctx.repo.path("bin.dat")),
            PreviewBody::Binary(4)
        );
    }

    #[test]
    fn entry_points_validate_input() {
        let (_d, ctx) = setup();
        assert!(matches!(
            open_blame(&ctx, "notes.txt", None),
            Action::Error(_)
        ));
        assert!(matches!(open_blame(&ctx, "src", None), Action::Error(_)));
        assert!(matches!(
            open_blame(&ctx, "src/main.rs", None),
            Action::Popup(_)
        ));
        assert!(matches!(
            open_blame(&ctx, "README.md", None),
            Action::Popup(_)
        ));
        assert!(matches!(open_file_history(&ctx, "src"), Action::Popup(_)));
        assert!(matches!(
            open_line_history(&ctx, "src/main.rs", 3, 1, None),
            Action::Error(_)
        ));
        assert!(matches!(
            open_file_at_revision(&ctx, "src/main.rs", "HEAD"),
            Action::Popup(_)
        ));
        assert_eq!(normalize_path(&ctx, "./src\\main.rs"), "src/main.rs");
        let abs = ctx.repo.path("src/main.rs").to_string_lossy().into_owned();
        assert_eq!(normalize_path(&ctx, &abs), "src/main.rs");
        assert_eq!(files_at_revision(&ctx, "HEAD").unwrap().len(), 3);
        assert_eq!(tracked_files(&ctx.repo).unwrap().len(), 3);
        assert_eq!(palette().len(), 5);
    }

    /// Draw `p` repeatedly (letting background loaders finish) and return the screen text.
    fn render_popup(p: &mut dyn Popup, ctx: &Ctx, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let mut text = String::new();
        for _ in 0..100 {
            term.draw(|f| p.draw(f, f.area(), ctx)).unwrap();
            text = term
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            if !text.contains("Loading") && !text.contains("Annotating") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        text
    }

    #[test]
    fn renders_view_and_popups() {
        let (_d, ctx) = setup();
        commit_files(
            &ctx.repo,
            &[("src/main.rs", "fn main() { changed }\nline 2\n")],
            "second",
        );
        write(
            &ctx.repo,
            "src/main.rs",
            "fn main() { changed }\nline 2\nlocal\n",
        );
        let mut v = FilesView::new(&ctx);
        v.refresh(&ctx).unwrap();
        for (w, h) in [(120, 30), (40, 10)] {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            term.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
        }
        v.reveal("src/main.rs");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        term.draw(|f| v.draw(f, f.area(), &ctx)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("fn main() { changed }"));

        for (w, h) in [(120, 20), (50, 8)] {
            let mut b = BlamePopup::new(&ctx, "src/main.rs".into(), None, Some(2));
            let text = render_popup(&mut b, &ctx, w, h);
            assert!(text.contains("Not Committed Yet"), "{text}");
            for k in ['v', 'j', 'n', 'N', 'g'] {
                b.handle_key(key(KeyCode::Char(k)), &ctx);
            }
            b.handle_key(key(KeyCode::Char('1')), &ctx);
            b.handle_key(key(KeyCode::Enter), &ctx);
            assert!(matches!(
                b.handle_key(key(KeyCode::Char('p')), &ctx),
                Action::Popup(_)
            ));
            assert!(matches!(
                b.handle_key(key(KeyCode::Char('c')), &ctx),
                Action::Popup(_)
            ));
            assert!(matches!(
                b.handle_key(key(KeyCode::Char('L')), &ctx),
                Action::Popup(_)
            ));
            assert!(matches!(
                b.handle_key(key(KeyCode::Char('.')), &ctx),
                Action::Popup(_)
            ));
            render_popup(&mut b, &ctx, w, h);

            let mut hp = HistoryPopup::new(&ctx, Target::File("src/main.rs".into()));
            let text = render_popup(&mut hp, &ctx, w, h);
            assert!(text.contains("second"), "{text}");
            for k in [KeyCode::Down, KeyCode::Tab, KeyCode::Down, KeyCode::Tab] {
                hp.handle_key(key(k), &ctx);
            }
            for k in ['p', 'w', 'o', 'b', 'R', '.', 'd'] {
                assert!(
                    matches!(hp.handle_key(key(KeyCode::Char(k)), &ctx), Action::Popup(_)),
                    "{k}"
                );
            }
            render_popup(&mut hp, &ctx, w, h);

            let mut cp = ContentPopup::new(&ctx, "src/main.rs".into(), "HEAD~1".into());
            let text = render_popup(&mut cp, &ctx, w, h);
            assert!(text.contains("fn main() {}"), "{text}");
            cp.handle_key(key(KeyCode::Char('/')), &ctx);
            render_popup(&mut cp, &ctx, w, h);
        }
    }

    #[test]
    fn empty_repository() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, crate::config::Config::default());
        let mut v = FilesView::new(&ctx);
        v.refresh(&ctx).unwrap();
        assert!(v.rows.is_empty());
        assert!(v.handle_key(key(KeyCode::Enter), &ctx).is_none());
        assert!(v.menu().is_none());
    }
}
