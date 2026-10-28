//! Merge conflicts tab: the operation in progress, the list of
//! conflicted files with per-file and bulk resolutions, continue/abort/skip of the operation, and
//! the 3-way merge resolver ([`open_resolver`]).

mod resolver;
mod rows;

use std::sync::{Arc, Mutex};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::conflict::{self, ConflictFile, MarkerCache, Operation, Side};
use crate::git::{Repo, RepoState};
use crate::ui::bg::Background;
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{ConfirmPopup, MenuItem, MenuPopup};
use crate::ui::views::Tab;
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, PaletteItem, View};
use crate::util;

/// What a refresh loads off the UI thread.
struct Loaded {
    files: Vec<ConflictFile>,
    markers: Vec<Option<usize>>,
    /// The operation and its fingerprint, when the fingerprint changed.
    op: Option<(Operation, String)>,
}

/// Runs on every refresh, even while the tab is hidden (for the badge), so it only starts
/// `git ls-files -u`; the operation and marker counts are recomputed when changed.
fn load_conflicts(
    repo: &Repo,
    old_fingerprint: &str,
    cache: &Mutex<MarkerCache>,
) -> anyhow::Result<Loaded> {
    let fingerprint = conflict::operation_fingerprint(repo);
    let op = (fingerprint != old_fingerprint).then(|| (conflict::operation(repo), fingerprint));
    let files = conflict::list(repo)?;
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let markers = match cache.lock() {
        Ok(mut c) => c.scan(repo, &paths),
        Err(_) => vec![None; paths.len()],
    };
    Ok(Loaded { files, markers, op })
}

pub struct ConflictsView {
    files: Vec<ConflictFile>,
    /// Conflict markers in each work-tree file (None: deleted, binary, or not scanned).
    markers: Vec<Option<usize>>,
    /// Marker counts by file size and mtime: refreshes only re-read files that changed.
    marker_cache: Arc<Mutex<MarkerCache>>,
    loader: Background<Result<Loaded, String>>,
    load_error: Option<String>,
    op: Operation,
    /// [`conflict::operation_fingerprint`] of `op`: unchanged means `op` needs no git calls.
    op_fingerprint: String,
    sel: usize,
    offset: usize,
    filter: TextInput,
    filtering: bool,
    list_area: Rect,
    list_top: u16,
}

impl ConflictsView {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            files: Vec::new(),
            markers: Vec::new(),
            marker_cache: Arc::new(Mutex::new(MarkerCache::default())),
            loader: Background::new(),
            load_error: None,
            // Described by the first refresh (an empty fingerprint never matches).
            op: Operation {
                state: RepoState::Clean,
                branch: None,
                step: None,
                head_name: None,
                onto: None,
                other: None,
                subject: None,
            },
            op_fingerprint: String::new(),
            sel: 0,
            offset: 0,
            filter: TextInput::default(),
            filtering: false,
            list_area: Rect::default(),
            list_top: 0,
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
        let prev = self.selected().map(|f| f.path.clone());
        if let Some((op, fingerprint)) = loaded.op {
            self.op = op;
            self.op_fingerprint = fingerprint;
        }
        self.files = loaded.files;
        self.markers = loaded.markers;
        let vis = self.visible();
        if let Some(p) = prev {
            if let Some(pos) = vis.iter().position(|&i| self.files[i].path == p) {
                self.sel = pos;
            }
        }
        self.sel = self.sel.min(vis.len().saturating_sub(1));
    }

    /// Indices of files matching the filter.
    fn visible(&self) -> Vec<usize> {
        let q = self.filter.value().to_lowercase();
        (0..self.files.len())
            .filter(|&i| q.is_empty() || self.files[i].path.to_lowercase().contains(&q))
            .collect()
    }

    fn selected(&self) -> Option<&ConflictFile> {
        self.visible().get(self.sel).map(|&i| &self.files[i])
    }

    fn selected_markers(&self) -> Option<usize> {
        self.visible().get(self.sel).and_then(|&i| self.markers[i])
    }

    fn all_paths(&self) -> Vec<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }

    // ----- drawing -------------------------------------------------------------------------

    fn hint(&self) -> String {
        let n = self.files.len();
        let noun = self.op.noun();
        let skip = if self.op.can_skip() {
            ", S to skip this commit"
        } else {
            ""
        };
        match (self.op.in_progress(), n) {
            (true, 0) => format!("All conflicts resolved — press C to continue the {noun}, or A to abort it."),
            (true, _) => format!(
                "Resolve the {n} conflicted file(s) (Enter merges), then C continues the {noun}. A aborts{skip}."
            ),
            (false, 0) => "No merge, rebase, cherry-pick, revert or patch application is in progress.".into(),
            (false, _) => "Unmerged files without an operation in progress (e.g. after a stash apply): resolve \
                           them and mark them resolved."
                .into(),
        }
    }

    fn draw_banner(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let title = match self.op.state {
            RepoState::Clean => " Status ".to_string(),
            s => format!(" {} ", s.label()),
        };
        let block = panel(title, false, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let desc = self.op.describe();
        let first = if desc.is_empty() {
            Line::styled("Nothing in progress", t.dimmed())
        } else {
            Line::styled(
                desc,
                Style::default().fg(t.conflict).add_modifier(Modifier::BOLD),
            )
        };
        let lines = vec![first, Line::styled(self.hint(), t.text())];
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    fn draw_empty(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let mut lines = vec![
            Line::raw(""),
            Line::styled("No conflicts", t.title()),
            Line::raw(""),
        ];
        if let Some(e) = &self.load_error {
            lines.push(Line::styled(e.clone(), Style::default().fg(t.error)));
        } else if self.op.in_progress() {
            lines.push(Line::from(vec![
                Span::styled("All conflicts are resolved. Press ", t.text()),
                Span::styled("C", t.key()),
                Span::styled(
                    format!(" to continue the {}, or ", self.op.noun()),
                    t.text(),
                ),
                Span::styled("A", t.key()),
                Span::styled(" to abort.", t.text()),
            ]));
        } else {
            lines.push(Line::styled(
                "Conflicts from merges, rebases and cherry-picks show up here.",
                t.dimmed(),
            ));
        }
        f.render_widget(
            Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center),
            area,
        );
    }

    fn file_line(&self, i: usize, width: usize, selected: bool, ctx: &Ctx) -> Line<'static> {
        let t = &ctx.theme;
        let file = &self.files[i];
        let kind = format!(" {:<16}", file.kind.label());
        let info = match (
            self.markers[i],
            file.ours_missing() || file.theirs_missing(),
        ) {
            (_, true) => "delete/modify".to_string(),
            (Some(0), _) => "no markers".to_string(),
            (Some(n), _) => format!("{n} marker(s)"),
            (None, _) => String::new(),
        };
        let path_w = width.saturating_sub(kind.chars().count() + info.chars().count() + 2);
        let base = if selected {
            t.selected()
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled(kind, base.fg(t.conflict)),
            Span::styled(util::fit(&file.path, path_w), base.fg(t.fg)),
            Span::styled(format!(" {info} "), base.fg(t.dim)),
        ])
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.visible();
        let title = if self.filter.is_empty() {
            format!(" Conflicts ({}) ", self.files.len())
        } else {
            format!(" Conflicts ({}/{}) ", vis.len(), self.files.len())
        };
        let block = panel(title, true, t);
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
        self.list_area = inner;
        self.list_top = inner.y;
        if self.files.is_empty() {
            self.draw_empty(f, inner, ctx);
            return;
        }
        let h = inner.height as usize;
        self.sel = self.sel.min(vis.len().saturating_sub(1));
        self.offset = scroll_to_keep_visible(self.sel, self.offset, h);
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(h)
            .map(|(row, &i)| self.file_line(i, inner.width as usize, row == self.sel, ctx))
            .collect();
        if lines.is_empty() {
            f.render_widget(
                Paragraph::new("No file matches the filter").style(t.dimmed()),
                inner,
            );
        } else {
            f.render_widget(Paragraph::new(lines), inner);
        }
    }

    fn draw_details(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let block = panel(" Details ", false, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(file) = self.selected() else {
            f.render_widget(
                Paragraph::new("Select a conflicted file").style(t.dimmed()),
                inner,
            );
            return;
        };
        let have = |b: bool| if b { "present" } else { "missing" };
        let row = |k: &str, v: String| {
            Line::from(vec![
                Span::styled(format!("{k:<12}"), t.dimmed()),
                Span::styled(v, t.text()),
            ])
        };
        let wt = match self.selected_markers() {
            Some(0) => "no conflict markers (edited?)".to_string(),
            Some(n) => format!("{n} conflict marker block(s)"),
            None if ctx.repo.path(&file.path).exists() => "binary or not scanned".to_string(),
            None => "deleted".to_string(),
        };
        let mut lines = vec![
            Line::styled(file.path.clone(), t.text().add_modifier(Modifier::BOLD)),
            Line::raw(""),
            row("Conflict", file.kind.label().to_string()),
            row("Base", have(file.stages.base.is_some()).to_string()),
            row("Yours", have(!file.ours_missing()).to_string()),
            row("Theirs", have(!file.theirs_missing()).to_string()),
            row("Work tree", wt),
            Line::raw(""),
        ];
        let keys: Vec<(&str, String)> = vec![
            ("Enter", "open the 3-way merge".into()),
            ("o", file.accept_label(Side::Ours)),
            ("t", file.accept_label(Side::Theirs)),
            ("a", "mark as resolved".into()),
            ("e", "edit in external editor".into()),
            ("v", "compare yours with theirs".into()),
            (". / m", "all actions".into()),
        ];
        for (k, d) in keys {
            lines.push(Line::from(vec![
                Span::styled(format!("{k:<7}"), t.key()),
                Span::styled(d, t.text()),
            ]));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    // ----- actions -------------------------------------------------------------------------

    fn menu(&self) -> Action {
        let op = &self.op;
        let mut items = Vec::new();
        if let Some(file) = self.selected() {
            let p = file.path.as_str();
            let markers = self.selected_markers();
            items.extend([
                MenuItem::new(
                    "Merge… (3-way resolver)",
                    "Enter",
                    with_path(p, open_resolver),
                ),
                MenuItem::new(
                    file.accept_label(Side::Ours),
                    "o",
                    with_path(p, |ctx, p| accept_one(ctx, p, Side::Ours)),
                ),
                MenuItem::new(
                    file.accept_label(Side::Theirs),
                    "t",
                    with_path(p, |ctx, p| accept_one(ctx, p, Side::Theirs)),
                ),
                MenuItem::new("Mark as resolved", "a", {
                    let p = p.to_string();
                    move |ctx| mark_resolved(ctx, p, markers)
                }),
                MenuItem::new("Open in external editor", "e", with_path(p, edit)),
                MenuItem::new(
                    "Compare yours with theirs",
                    "v",
                    with_path(p, |ctx, p| {
                        compare(ctx, p, Some(Side::Ours), Some(Side::Theirs))
                    }),
                ),
                MenuItem::new(
                    "Show your changes (base → yours)",
                    "y",
                    with_path(p, |ctx, p| compare(ctx, p, None, Some(Side::Ours))),
                ),
                MenuItem::new(
                    "Show their changes (base → theirs)",
                    "i",
                    with_path(p, |ctx, p| compare(ctx, p, None, Some(Side::Theirs))),
                ),
                MenuItem::new("Resolve by deleting the file…", "d", with_path(p, delete)),
                MenuItem::new(
                    "Show history",
                    "H",
                    with_path(p, crate::ui::views::files::open_file_history),
                ),
                MenuItem::new(
                    "Show local history",
                    "L",
                    with_path(p, crate::ui::views::local_history::open_for_path),
                ),
            ]);
        }
        let any = !self.files.is_empty();
        let noun = op.noun();
        items.extend([
            MenuItem::new("Accept yours for all files…", "O", |ctx| {
                accept_all(ctx, Side::Ours)
            })
            .enabled(any),
            MenuItem::new("Accept theirs for all files…", "T", |ctx| {
                accept_all(ctx, Side::Theirs)
            })
            .enabled(any),
            MenuItem::new(format!("Continue {noun}"), "C", continue_op).enabled(op.in_progress()),
            MenuItem::new("Skip this commit…", "S", skip_op).enabled(op.can_skip()),
            MenuItem::new(format!("Abort {noun}…"), "A", abort_op).enabled(op.in_progress()),
        ]);
        let title = self
            .selected()
            .map(|f| util::truncate(&f.path, 50))
            .unwrap_or_else(|| "Conflicts".into());
        Action::popup(MenuPopup::new(title, items))
    }

    fn on_selected(
        &self,
        ctx: &Ctx,
        f: impl FnOnce(&Ctx, &ConflictFile, Option<usize>) -> Action,
    ) -> Action {
        match self.selected() {
            Some(file) => f(ctx, file, self.selected_markers()),
            None => Action::Toast("No conflicted file selected".into()),
        }
    }

    fn filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.set("");
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.filtering = false,
            _ => {
                if self.filter.handle_key(&key) {
                    self.sel = 0;
                    self.offset = 0;
                }
            }
        }
        Action::None
    }
}

impl View for ConflictsView {
    fn title(&self) -> String {
        "Conflicts".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        let fingerprint = self.op_fingerprint.clone();
        let cache = self.marker_cache.clone();
        self.loader.start(&ctx.repo, move |repo| {
            load_conflicts(repo, &fingerprint, &cache).map_err(|e| format!("{e:#}"))
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
        let [banner, body] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).areas(area);
        self.draw_banner(f, banner, ctx);
        if body.width >= 100 && !self.files.is_empty() {
            let [list, details] =
                Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
                    .areas(body);
            self.draw_list(f, list, ctx);
            self.draw_details(f, details, ctx);
        } else {
            self.draw_list(f, body, ctx);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filtering {
            return self.filter_key(key);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Action::None;
        }
        let n = self.visible().len();
        let page = self.list_area.height.max(1) as isize;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel = move_sel(self.sel, 1, n),
            KeyCode::Up | KeyCode::Char('k') => self.sel = move_sel(self.sel, -1, n),
            KeyCode::PageDown => self.sel = move_sel(self.sel, page, n),
            KeyCode::PageUp => self.sel = move_sel(self.sel, -page, n),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.is_empty() => self.filter.set(""),
            KeyCode::Enter => {
                return self.on_selected(ctx, |ctx, f, _| open_resolver(ctx, &f.path))
            }
            KeyCode::Char('o') => {
                return self.on_selected(ctx, |ctx, f, _| accept_one(ctx, &f.path, Side::Ours))
            }
            KeyCode::Char('t') => {
                return self.on_selected(ctx, |ctx, f, _| accept_one(ctx, &f.path, Side::Theirs))
            }
            KeyCode::Char('a') => {
                return self.on_selected(ctx, |ctx, f, m| mark_resolved(ctx, f.path.clone(), m))
            }
            KeyCode::Char('e') => return self.on_selected(ctx, |ctx, f, _| edit(ctx, &f.path)),
            KeyCode::Char('v') => {
                return self.on_selected(ctx, |ctx, f, _| {
                    compare(ctx, &f.path, Some(Side::Ours), Some(Side::Theirs))
                })
            }
            KeyCode::Char('d') => return self.on_selected(ctx, |ctx, f, _| delete(ctx, &f.path)),
            KeyCode::Char('H') => {
                return self.on_selected(ctx, |ctx, f, _| {
                    crate::ui::views::files::open_file_history(ctx, &f.path)
                })
            }
            KeyCode::Char('L') => {
                return self.on_selected(ctx, |ctx, f, _| {
                    crate::ui::views::local_history::open_for_path(ctx, &f.path)
                })
            }
            KeyCode::Char('O') => return accept_all_paths(self.all_paths(), Side::Ours),
            KeyCode::Char('T') => return accept_all_paths(self.all_paths(), Side::Theirs),
            KeyCode::Char('C') => return continue_op(ctx),
            KeyCode::Char('A') => return abort_op(ctx),
            KeyCode::Char('S') => return skip_op(ctx),
            KeyCode::Char('.') | KeyCode::Char('m') => return self.menu(),
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, ev: MouseEvent, ctx: &Ctx) -> Action {
        if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::None;
        }
        let a = self.list_area;
        let inside = ev.column >= a.x
            && ev.column < a.x + a.width
            && ev.row >= a.y
            && ev.row < a.y + a.height;
        if !inside {
            return Action::None;
        }
        let idx = self.offset + (ev.row - self.list_top) as usize;
        if idx >= self.visible().len() {
            return Action::None;
        }
        if idx == self.sel {
            return self.on_selected(ctx, |ctx, f, _| open_resolver(ctx, &f.path));
        }
        self.sel = idx;
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("Enter", "merge (3-way)"),
            ("o/t", "accept yours/theirs"),
            ("a", "mark resolved"),
            ("C", "continue"),
            ("A", "abort"),
            ("S", "skip commit"),
            ("e", "edit"),
            (". m", "menu"),
            ("O/T", "accept yours/theirs for all"),
            ("v", "compare yours/theirs"),
            ("d", "resolve by deleting"),
            ("H / L", "history / local history"),
            ("/", "filter"),
            ("j/k ↑↓", "move"),
            ("g/G", "first/last"),
        ]
    }

    fn captures_input(&self) -> bool {
        self.filtering
    }

    fn badge(&self) -> Option<String> {
        (!self.files.is_empty()).then(|| self.files.len().to_string())
    }
}

// ----- shared actions ------------------------------------------------------------------------

/// A menu callback running `f` on an owned copy of `path`.
fn with_path(path: &str, f: fn(&Ctx, &str) -> Action) -> impl FnOnce(&Ctx) -> Action {
    let path = path.to_string();
    move |ctx| f(ctx, &path)
}

/// Resolve one file with one side (deleting it when that side removed it).
fn accept_one(ctx: &Ctx, path: &str, side: Side) -> Action {
    Action::from_result(
        conflict::accept_side(&ctx.repo, &[path.to_string()], side),
        format!("Resolved {path} with {}", side.label()),
    )
}

fn accept_all_paths(paths: Vec<String>, side: Side) -> Action {
    if paths.is_empty() {
        return Action::Toast("No conflicts".into());
    }
    let msg = format!(
        "Resolve all {} conflicted file(s) by taking {} version?\n\
         The other side's changes in these files are discarded (files deleted on that side are removed).",
        paths.len(),
        match side {
            Side::Ours => "your",
            Side::Theirs => "their",
        }
    );
    Action::popup(
        ConfirmPopup::new(format!("Accept {} for all", side.label()), msg, move |_| {
            Action::spawn(format!("Accept {}", side.label()), move |repo| {
                let n = conflict::accept_side(repo, &paths, side)?;
                Ok(format!("Resolved {n} file(s) with {}", side.label()))
            })
        })
        .danger(),
    )
}

fn accept_all(ctx: &Ctx, side: Side) -> Action {
    match conflict::list(&ctx.repo) {
        Ok(files) => accept_all_paths(files.into_iter().map(|f| f.path).collect(), side),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn mark_resolved(ctx: &Ctx, path: String, markers: Option<usize>) -> Action {
    match markers {
        Some(n) if n > 0 => Action::popup(ConfirmPopup::new(
            "Mark as resolved",
            format!(
                "{path} still contains {n} conflict marker block(s). Mark it as resolved anyway?"
            ),
            move |ctx| mark_now(ctx, path),
        )),
        _ => mark_now(ctx, path),
    }
}

fn mark_now(ctx: &Ctx, path: String) -> Action {
    let msg = format!("Marked {path} as resolved");
    Action::from_result(conflict::mark_resolved(&ctx.repo, &[path]), msg)
}

fn delete(_ctx: &Ctx, path: &str) -> Action {
    let path = path.to_string();
    Action::popup(
        ConfirmPopup::new(
            "Delete file",
            format!("Resolve the conflict by deleting {path} from the work tree and the index?"),
            move |ctx| {
                let msg = format!("Deleted {path}");
                Action::from_result(conflict::delete(&ctx.repo, &[path]), msg)
            },
        )
        .danger(),
    )
}

fn edit(ctx: &Ctx, path: &str) -> Action {
    let abs = ctx.repo.path(path);
    let line = std::fs::read_to_string(&abs)
        .ok()
        .and_then(|s| conflict::marker_lines(&s).first().map(|l| l + 1));
    Action::Suspend(util::editor_command(
        ctx.config.editor.as_deref(),
        &abs,
        line,
    ))
}

fn side_name(side: Option<Side>) -> &'static str {
    match side {
        None => "base",
        Some(Side::Ours) => "yours",
        Some(Side::Theirs) => "theirs",
    }
}

/// Diff two versions of a conflicted file (stage blobs exported to gitjet's data dir).
fn compare(ctx: &Ctx, path: &str, left: Option<Side>, right: Option<Side>) -> Action {
    let export = || -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
        let dir = ctx.repo.data_dir()?.join("conflict-compare");
        let l = conflict::export_stage(&ctx.repo, path, left, &dir.join(side_name(left)))?;
        let r = conflict::export_stage(&ctx.repo, path, right, &dir.join(side_name(right)))?;
        Ok((l, r))
    };
    match export() {
        Ok((l, r)) => diff_view::open(
            format!("{path}: {} → {}", side_name(left), side_name(right)),
            DiffSource::Files { left: l, right: r },
        ),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn continue_op(ctx: &Ctx) -> Action {
    let op = conflict::operation(&ctx.repo);
    if !op.in_progress() {
        return Action::Toast(
            "Nothing to continue: no merge, rebase, cherry-pick or revert in progress".into(),
        );
    }
    match conflict::unmerged_count(&ctx.repo) {
        Ok(0) => Action::spawn(
            format!("Continue {}", op.noun()),
            conflict::continue_operation,
        ),
        Ok(n) => Action::Error(format!(
            "Resolve all conflicts before continuing the {} ({n} file(s) still in conflict)",
            op.noun()
        )),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn abort_op(ctx: &Ctx) -> Action {
    let op = conflict::operation(&ctx.repo);
    if !op.in_progress() {
        return Action::Toast(
            "Nothing to abort: no merge, rebase, cherry-pick or revert in progress".into(),
        );
    }
    let noun = op.noun();
    Action::popup(
        ConfirmPopup::new(
            format!("Abort {noun}"),
            format!(
                "Abort the {noun} and return to the state before it started?\n{}\nAll resolutions made so far are lost.",
                op.describe()
            ),
            move |_| Action::spawn(format!("Abort {noun}"), conflict::abort_operation),
        )
        .danger(),
    )
}

fn skip_op(ctx: &Ctx) -> Action {
    let op = conflict::operation(&ctx.repo);
    if !op.can_skip() {
        let why = if op.state == RepoState::Merging {
            "A merge cannot skip commits; continue or abort it"
        } else {
            "Nothing to skip: no rebase, cherry-pick, revert or patch application in progress"
        };
        return Action::Toast(why.into());
    }
    let noun = op.noun();
    let what = op
        .subject
        .as_deref()
        .map(|s| format!(" \"{s}\""))
        .unwrap_or_default();
    Action::popup(
        ConfirmPopup::new(
            "Skip commit",
            format!("Skip the current commit{what}? Its changes are dropped from the {noun}."),
            move |_| Action::spawn(format!("Skip ({noun})"), conflict::skip_operation),
        )
        .danger(),
    )
}

fn resolve_palette(ctx: &Ctx) -> Action {
    match conflict::list(&ctx.repo) {
        Ok(files) if files.is_empty() => Action::Toast("No conflicts to resolve".into()),
        Ok(files) if files.len() == 1 => {
            Action::SwitchTab(Tab::Conflicts).then(open_resolver(ctx, &files[0].path))
        }
        Ok(_) => Action::SwitchTab(Tab::Conflicts),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Resolve conflicts…",
            run: resolve_palette,
        },
        PaletteItem {
            name: "Continue operation",
            run: continue_op,
        },
        PaletteItem {
            name: "Abort operation",
            run: abort_op,
        },
        PaletteItem {
            name: "Skip commit (rebase / cherry-pick / am)…",
            run: skip_op,
        },
        PaletteItem {
            name: "Accept all ours…",
            run: |ctx| accept_all(ctx, Side::Ours),
        },
        PaletteItem {
            name: "Accept all theirs…",
            run: |ctx| accept_all(ctx, Side::Theirs),
        },
    ]
}

/// Open the 3-way merge resolver for a conflicted `path`.
pub fn open_resolver(ctx: &Ctx, path: &str) -> Action {
    resolver::open(ctx, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn two_conflicts() -> (TempDir, Ctx) {
        let (d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n"), ("dir/b c.txt", "1\n")], "base");
        repo.run(&["checkout", "-q", "-b", "feature"]).unwrap();
        commit_files(
            &repo,
            &[("a.txt", "theirs\n"), ("dir/b c.txt", "theirs\n")],
            "t",
        );
        repo.run(&["checkout", "-q", "main"]).unwrap();
        commit_files(
            &repo,
            &[("a.txt", "ours\n"), ("dir/b c.txt", "ours\n")],
            "o",
        );
        assert!(repo.run(&["merge", "feature"]).is_err());
        (d, Ctx::new(repo, Config::default()))
    }

    #[test]
    fn view_lists_conflicts_and_resolves() {
        let (_d, ctx) = two_conflicts();
        let mut v = ConflictsView::new(&ctx);
        v.refresh(&ctx).unwrap();
        assert_eq!(v.badge().as_deref(), Some("2"));
        assert_eq!(v.files[1].path, "dir/b c.txt");
        assert_eq!(v.markers, vec![Some(1), Some(1)]);
        assert!(v.hint().contains("C continues the merge"));
        // continue refuses while conflicts remain
        assert!(matches!(v.handle_key(key('C'), &ctx), Action::Error(_)));
        // accept theirs for the first file
        v.handle_key(key('t'), &ctx);
        v.refresh(&ctx).unwrap();
        assert_eq!(v.files.len(), 1);
        assert_eq!(
            std::fs::read_to_string(ctx.repo.path("a.txt"))
                .unwrap()
                .trim(),
            "theirs"
        );
        // filter
        v.handle_key(key('/'), &ctx);
        assert!(v.captures_input());
        for c in "zzz".chars() {
            v.handle_key(key(c), &ctx);
        }
        assert!(v.visible().is_empty());
        v.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctx);
        assert!(!v.captures_input() && v.visible().len() == 1);
        // marking resolved with markers present asks first
        assert!(matches!(v.handle_key(key('a'), &ctx), Action::Popup(_)));
        conflict::write_result(&ctx.repo, "dir/b c.txt", b"merged\n", false).unwrap();
        v.refresh(&ctx).unwrap();
        v.handle_key(key('a'), &ctx);
        v.refresh(&ctx).unwrap();
        assert!(v.files.is_empty());
        assert_eq!(v.badge(), None);
        assert!(v.hint().contains("All conflicts resolved"));
        // now continue is spawned
        assert!(matches!(v.handle_key(key('C'), &ctx), Action::Spawn(_)));
    }

    #[test]
    fn menus_and_confirmations() {
        let (_d, ctx) = two_conflicts();
        let mut v = ConflictsView::new(&ctx);
        v.refresh(&ctx).unwrap();
        assert!(matches!(v.handle_key(key('.'), &ctx), Action::Popup(_)));
        assert!(matches!(v.handle_key(key('O'), &ctx), Action::Popup(_)));
        assert!(matches!(v.handle_key(key('A'), &ctx), Action::Popup(_)));
        assert!(
            matches!(v.handle_key(key('S'), &ctx), Action::Toast(_)),
            "merges cannot skip"
        );
        assert!(matches!(v.handle_key(key('v'), &ctx), Action::Popup(_)));
        assert!(matches!(
            v.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx),
            Action::Popup(_)
        ));
        assert_eq!(palette().len(), 6);
        assert!(matches!(
            resolve_palette(&ctx),
            Action::SwitchTab(Tab::Conflicts)
        ));
    }

    fn render(v: &mut ConflictsView, ctx: &Ctx, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| v.draw(f, f.area(), ctx)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_banner_list_and_details() {
        let (_d, ctx) = two_conflicts();
        let mut v = ConflictsView::new(&ctx);
        v.refresh(&ctx).unwrap();
        let screen = render(&mut v, &ctx, 130, 16);
        assert!(screen.contains("MERGING"), "{screen}");
        assert!(screen.contains("Merging feature into main"));
        assert!(screen.contains("dir/b c.txt"));
        assert!(screen.contains("both modified"));
        assert!(screen.contains("Details"));
        for (w, h) in [(20, 4), (60, 8), (1, 1)] {
            render(&mut v, &ctx, w, h);
        }
        accept_one(&ctx, "a.txt", Side::Ours);
        accept_one(&ctx, "dir/b c.txt", Side::Ours);
        v.refresh(&ctx).unwrap();
        let screen = render(&mut v, &ctx, 130, 16);
        assert!(screen.contains("No conflicts"), "{screen}");
        assert!(screen.contains("to continue the merge"));
    }

    #[test]
    fn empty_repo_view() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let mut v = ConflictsView::new(&ctx);
        v.refresh(&ctx).unwrap();
        assert!(v.files.is_empty());
        assert!(matches!(v.handle_key(key('C'), &ctx), Action::Toast(_)));
        assert!(matches!(v.handle_key(key('o'), &ctx), Action::Toast(_)));
        assert!(matches!(resolve_palette(&ctx), Action::Toast(_)));
    }
}
