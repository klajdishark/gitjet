//! The 3-way merge resolver: "Yours (HEAD)" | "Result" |
//! "Theirs" panes with synchronized scrolling, per-chunk accept/discard/undo, the magic wand for
//! non-conflicting changes, external editing, and save-and-mark-resolved.
//!
//! Binary files and delete/modify conflicts get a simpler "choose a version" screen.

use std::cell::{self, RefCell};
use std::rc::Rc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::rows::{self, Cell, Row, Rows};
use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::conflict::{
    self, Chunk, ChunkKind, MarkerStyle, MergeModel, Resolution, Side, Stages,
};
use crate::git::status::ConflictKind;
use crate::theme::Theme;
use crate::ui::popups::{ConfirmPopup, MenuItem, MenuPopup, MessagePopup};
use crate::ui::widgets::{centered, panel};
use crate::ui::{KeyHelp, Popup};
use crate::util;

const TEXT_HELP: [KeyHelp; 19] = [
    ("n / N", "next / previous unresolved change"),
    ("Tab ] / S-Tab [", "next / previous change"),
    ("h", "accept yours for this change"),
    ("l", "accept theirs for this change"),
    ("b", "accept both (yours, then theirs)"),
    ("B", "accept both (theirs, then yours)"),
    ("x", "discard both (keep base)"),
    ("u", "undo this change's resolution"),
    ("A", "apply all non-conflicting changes"),
    ("H / L", "accept yours / theirs for every change"),
    ("R", "reset: drop edits, then resolutions (asks first)"),
    (
        "e",
        "edit the result in the external editor (asks before replacing edits)",
    ),
    ("W", "use the work-tree file as the result"),
    ("j k ↑ ↓", "scroll"),
    ("PgUp PgDn g G", "page / top / bottom"),
    ("← →", "scroll sideways"),
    ("Ctrl+S", "save and mark resolved"),
    ("Esc", "close"),
    ("?", "this help"),
];

const CHOICE_HELP: [KeyHelp; 3] = [
    ("h / o", "accept yours"),
    ("l / t", "accept theirs"),
    ("Esc", "close"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Ours,
    Result,
    Theirs,
}

/// What popups opened by the resolver (confirmations, menus) report back through: their callbacks
/// only receive `&Ctx`, so they flag the request here and the resolver applies it.
#[derive(Default)]
struct Shared {
    /// The editor was started on the work-tree file: reload it as the result on the next refresh.
    reload: cell::Cell<bool>,
    /// A confirmed reset, applied before the next draw or key.
    reset: cell::Cell<bool>,
    /// What the resolver last wrote to the work-tree file.
    last_written: RefCell<Option<Vec<u8>>>,
}

/// A result edited outside the chunk model (external editor or the work-tree file).
struct Edited {
    raw: String,
    lines: Vec<String>,
    /// Lines inside conflict-marker blocks (including the markers).
    in_conflict: Vec<bool>,
    markers: Vec<usize>,
}

impl Edited {
    fn new(raw: String) -> Self {
        let lines: Vec<String> = raw
            .lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect();
        let mut inside = false;
        let in_conflict = lines
            .iter()
            .map(|l| {
                if l.starts_with("<<<<<<<") {
                    inside = true;
                    return true;
                }
                let was = inside;
                if l.starts_with(">>>>>>>") {
                    inside = false;
                }
                was
            })
            .collect();
        let markers = conflict::marker_lines(&raw);
        Self {
            raw,
            lines,
            in_conflict,
            markers,
        }
    }
}

struct TextMerge {
    model: MergeModel,
    rows: Rows,
    /// Selected change chunk.
    current: Option<usize>,
    edited: Option<Edited>,
    ours_lines: Vec<String>,
    theirs_lines: Vec<String>,
}

impl TextMerge {
    fn rebuild(&mut self) {
        self.rows = rows::build(&self.model);
    }

    fn total_rows(&self) -> usize {
        match &self.edited {
            Some(e) => e
                .lines
                .len()
                .max(self.ours_lines.len())
                .max(self.theirs_lines.len()),
            None => self.rows.rows.len(),
        }
    }
}

/// Binary or delete/modify conflict: only a whole-file choice is possible.
struct Choice {
    stages: Stages,
    binary: bool,
    sizes: [Option<usize>; 2],
    /// First lines of the surviving text version (delete/modify conflicts).
    preview: Vec<String>,
}

enum Mode {
    Text(Box<TextMerge>),
    Choice(Choice),
}

pub struct MergeResolver {
    path: String,
    kind: ConflictKind,
    ours_title: String,
    theirs_title: String,
    marker_style: MarkerStyle,
    mode: Mode,
    scroll: usize,
    hscroll: usize,
    height: usize,
    dirty: bool,
    focus_pending: bool,
    shared: Rc<Shared>,
    /// The work-tree file as it was when the resolver opened (to tell later hand edits apart).
    loaded_worktree: Option<Vec<u8>>,
    notice: Option<(String, bool)>,
}

/// Open the resolver for `path`, or explain why it cannot be opened.
pub fn open(ctx: &Ctx, path: &str) -> Action {
    match MergeResolver::load(ctx, path) {
        Ok(r) => Action::popup(r),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

impl MergeResolver {
    fn load(ctx: &Ctx, path: &str) -> anyhow::Result<Self> {
        let repo = &ctx.repo;
        let stages = conflict::stages(repo, path)?;
        if stages.is_empty() {
            anyhow::bail!("{path} is not in conflict");
        }
        let input = conflict::load_input(repo, path)?;
        let op = conflict::operation(repo);
        let other = op.in_progress().then(|| op.theirs_label());
        let theirs_title = match &other {
            Some(o) => format!("Theirs ({o})"),
            None => "Theirs".to_string(),
        };
        let marker_style = MarkerStyle {
            ours_label: "HEAD".to_string(),
            theirs_label: other.unwrap_or_else(|| "theirs".to_string()),
            diff3: conflict::uses_diff3(repo),
        };
        let binary = input.is_binary();
        let mode = if binary || stages.is_delete_conflict() {
            let surviving = input.ours.as_ref().or(input.theirs.as_ref());
            let preview = match surviving {
                Some(b) if !binary => String::from_utf8_lossy(b)
                    .lines()
                    .take(200)
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            Mode::Choice(Choice {
                stages: stages.clone(),
                binary,
                sizes: [
                    input.ours.as_ref().map(Vec::len),
                    input.theirs.as_ref().map(Vec::len),
                ],
                preview,
            })
        } else {
            let model = input.model();
            let lines = |b: &Option<Vec<u8>>| -> Vec<String> {
                b.as_deref()
                    .map(|b| conflict::merge::SplitText::new(&String::from_utf8_lossy(b)).lines)
                    .unwrap_or_default()
            };
            let mut tm = TextMerge {
                rows: rows::build(&model),
                current: None,
                edited: None,
                ours_lines: lines(&input.ours),
                theirs_lines: lines(&input.theirs),
                model,
            };
            tm.current = tm
                .model
                .find(None, true, |c| c.is_conflict() && !c.is_resolved())
                .or_else(|| tm.model.find(None, true, Chunk::is_change));
            Mode::Text(Box::new(tm))
        };
        let notice = match conflict::worktree_markers(repo, path) {
            Some(0) if !binary && !stages.is_delete_conflict() => Some((
                "The work-tree file has no conflict markers (already edited?) — W uses it as the result".to_string(),
                false,
            )),
            _ => None,
        };
        Ok(Self {
            path: path.to_string(),
            kind: stages.kind(),
            ours_title: "Yours (HEAD)".to_string(),
            theirs_title,
            marker_style,
            mode,
            scroll: 0,
            hscroll: 0,
            height: 0,
            dirty: false,
            focus_pending: true,
            shared: Rc::default(),
            loaded_worktree: std::fs::read(repo.path(path)).ok(),
            notice,
        })
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), false));
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), true));
    }

    fn text(&self) -> Option<&TextMerge> {
        match &self.mode {
            Mode::Text(tm) => Some(tm),
            Mode::Choice(_) => None,
        }
    }

    fn text_mut(&mut self) -> Option<&mut TextMerge> {
        match &mut self.mode {
            Mode::Text(tm) => Some(tm),
            Mode::Choice(_) => None,
        }
    }

    // ----- navigation ----------------------------------------------------------------------

    fn scroll_by(&mut self, delta: isize) {
        let total = self.text().map(TextMerge::total_rows).unwrap_or(0);
        let max = total.saturating_sub(self.height.max(1));
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        self.sync_current_to_viewport();
    }

    /// After manual scrolling, keep the selected change on screen.
    fn sync_current_to_viewport(&mut self) {
        let (scroll, height) = (self.scroll, self.height.max(1));
        let Some(tm) = self.text_mut() else { return };
        if tm.edited.is_some() {
            return;
        }
        let visible = |ci: usize| {
            let (s, e) = tm.rows.chunk_rows(ci);
            s < scroll + height && e > scroll
        };
        if tm.current.is_some_and(visible) {
            return;
        }
        if let Some(ci) =
            (0..tm.model.chunks.len()).find(|&i| tm.model.chunks[i].is_change() && visible(i))
        {
            tm.current = Some(ci);
        }
    }

    fn reveal_current(&mut self) {
        let height = self.height;
        let scroll = self.scroll;
        let target = self
            .text()
            .and_then(|tm| tm.current.map(|ci| tm.rows.chunk_rows(ci).0));
        if let Some(t) = target {
            self.scroll = rows::scroll_for(t, scroll, height);
        }
    }

    fn goto(&mut self, forward: bool, unresolved_only: bool) {
        let scroll = self.scroll;
        let Some(tm) = self.text_mut() else { return };
        if let Some(e) = &tm.edited {
            let next = if forward {
                e.markers
                    .iter()
                    .find(|&&l| l > scroll)
                    .or(e.markers.first())
            } else {
                e.markers
                    .iter()
                    .rev()
                    .find(|&&l| l < scroll)
                    .or(e.markers.last())
            };
            match next.copied() {
                Some(l) => self.scroll = l,
                None => self.note("No conflict markers left in the edited result"),
            }
            return;
        }
        let found = if unresolved_only {
            tm.model
                .find(tm.current, forward, |c| c.is_change() && !c.is_resolved())
        } else {
            tm.model.find(tm.current, forward, Chunk::is_change)
        };
        match found {
            Some(ci) => {
                tm.current = Some(ci);
                self.reveal_current();
            }
            None if unresolved_only => self.note("All changes are resolved — Ctrl+S saves"),
            None => self.note("This file has no changes"),
        }
    }

    // ----- resolutions ---------------------------------------------------------------------

    fn edited_guard(&mut self) -> bool {
        if self.text().is_some_and(|tm| tm.edited.is_some()) {
            self.warn("The result was edited as text — press R to return to the merge model");
            return true;
        }
        false
    }

    /// Resolve (Some) or undo (None) the current chunk.
    fn act(&mut self, res: Option<Resolution>) {
        if self.edited_guard() {
            return;
        }
        let Some(tm) = self.text_mut() else { return };
        let Some(ci) = tm.current else {
            self.note("No change selected — press n to go to the next one");
            return;
        };
        let changed = match res {
            Some(r) => tm.model.resolve(ci, r),
            None => tm.model.undo(ci),
        };
        if changed {
            tm.rebuild();
            let left = tm.model.conflicts_remaining();
            self.dirty = true;
            let what = res.map(Resolution::label).unwrap_or("undone");
            self.note(format!("Change {what} — {left} conflict(s) remaining"));
        }
    }

    fn apply_non_conflicting(&mut self) {
        if self.edited_guard() {
            return;
        }
        let Some(tm) = self.text_mut() else { return };
        let n = tm.model.apply_non_conflicting();
        tm.rebuild();
        let left = tm.model.conflicts_remaining();
        if n > 0 {
            self.dirty = true;
        }
        self.note(format!(
            "Applied {n} non-conflicting change(s) — {left} conflict(s) remaining"
        ));
    }

    fn accept_everywhere(&mut self, res: Resolution) {
        if self.edited_guard() {
            return;
        }
        let Some(tm) = self.text_mut() else { return };
        for ci in tm.model.changes() {
            tm.model.resolve(ci, res);
        }
        tm.rebuild();
        self.dirty = true;
        self.note(format!("{} for every change", capitalize(res.label())));
    }

    /// 'R': reset after a confirmation, since it throws away manual work.
    fn request_reset(&mut self) -> Action {
        let Some(tm) = self.text() else {
            return Action::None;
        };
        let msg = if tm.edited.is_some() {
            "Drop the text edits and return to the merge model?"
        } else if tm.model.chunks.iter().any(|c| c.resolution.is_some()) {
            "Clear every resolution in this file? This cannot be undone."
        } else {
            self.note("Nothing to reset");
            return Action::None;
        };
        let shared = Rc::clone(&self.shared);
        Action::popup(
            ConfirmPopup::new("Reset merge", msg, move |_| {
                shared.reset.set(true);
                Action::None
            })
            .danger(),
        )
    }

    /// Apply requests flagged by popups this resolver opened.
    fn apply_requests(&mut self) {
        if self.shared.reset.take() {
            self.reset();
        }
    }

    fn reset(&mut self) {
        let Some(tm) = self.text_mut() else { return };
        if tm.edited.take().is_some() {
            self.note("Dropped the text edits — back to the merge model");
        } else {
            tm.model.reset();
            tm.rebuild();
            self.note("All resolutions cleared");
        }
        self.dirty = true;
    }

    fn set_edited(&mut self, raw: String, msg: &str) {
        if let Some(tm) = self.text_mut() {
            tm.edited = Some(Edited::new(raw));
            self.dirty = true;
            self.scroll = 0;
            self.note(msg.to_string());
        }
    }

    fn load_worktree(&mut self, ctx: &Ctx) {
        match std::fs::read(ctx.repo.path(&self.path)) {
            Ok(b) if conflict::is_binary(&b) => self.warn("The work-tree file is not UTF-8 text"),
            Ok(b) => self.set_edited(
                String::from_utf8_lossy(&b).into_owned(),
                "Using the work-tree file as the result — Ctrl+S marks it resolved, R returns to the merge model",
            ),
            Err(e) => self.warn(format!("Cannot read the work-tree file: {e}")),
        }
    }

    // ----- external editor -----------------------------------------------------------------

    /// 'e': write the result (conflict markers for unresolved chunks) to the work-tree file and open
    /// it in the editor; the file is reloaded as the result afterwards. When the file holds content
    /// the resolver did not write (hand edits), ask before replacing it.
    fn edit_externally(&mut self, ctx: &Ctx) -> Action {
        let Some(tm) = self.text() else {
            return Action::None;
        };
        let content = match &tm.edited {
            Some(e) => e.raw.clone(),
            None => tm.model.text_with_markers(&self.marker_style),
        };
        let fallback = tm.current.map(|ci| tm.rows.result_line_of(ci));
        let current = std::fs::read(ctx.repo.path(&self.path)).ok();
        let Some(cur) = current.filter(|cur| {
            cur.as_slice() != content.as_bytes()
                && self.shared.last_written.borrow().as_deref() != Some(cur.as_slice())
        }) else {
            return write_and_edit(ctx, &self.path, &content, fallback, &self.shared);
        };
        let cur_text = String::from_utf8_lossy(&cur).into_owned();
        // Unchanged since the resolver opened and still full of markers: most likely git's output.
        let pristine = self.loaded_worktree.as_deref() == Some(cur.as_slice())
            && conflict::count_markers(&cur_text) > 0;
        let (path, shared) = (self.path.clone(), Rc::clone(&self.shared));
        let write = MenuItem::new(
            "Replace it with the resolver's result, then edit",
            "w",
            move |ctx| write_and_edit(ctx, &path, &content, fallback, &shared),
        );
        let (path, shared) = (self.path.clone(), Rc::clone(&self.shared));
        let keep = MenuItem::new(
            "Edit the work-tree file as it is (keep its content)",
            "a",
            move |ctx| {
                let line = first_marker_line(&cur_text);
                open_editor(ctx, &path, line, &shared)
            },
        );
        let cancel = MenuItem::new("Cancel", "c", |_| Action::None);
        let (title, items) = if pristine {
            (
                "The work-tree file differs from the result",
                vec![write, keep, cancel],
            )
        } else {
            (
                "The work-tree file has edits the resolver did not make",
                vec![keep, write, cancel],
            )
        };
        Action::popup(MenuPopup::new(title, items))
    }

    // ----- saving --------------------------------------------------------------------------

    fn save(&mut self, ctx: &Ctx) -> Action {
        let Some(tm) = self.text() else {
            return Action::None;
        };
        let path = self.path.clone();
        if let Some(e) = &tm.edited {
            if e.markers.is_empty() {
                return save_action(ctx, &path, &e.raw, true);
            }
            let (a, b) = (e.raw.clone(), e.raw.clone());
            let p2 = path.clone();
            return Action::popup(MenuPopup::new(
                format!("{} conflict marker(s) remain", e.markers.len()),
                vec![
                    MenuItem::new("Save and mark resolved anyway", "s", move |ctx| {
                        save_action(ctx, &path, &a, true)
                    }),
                    MenuItem::new("Save, keep the file unresolved", "m", move |ctx| {
                        save_action(ctx, &p2, &b, false)
                    }),
                    MenuItem::new("Cancel", "c", |_| Action::None),
                ],
            ));
        }
        let (conflicts, pending) = (tm.model.conflicts_remaining(), tm.model.pending_changes());
        if tm.model.is_fully_resolved() {
            return save_action(ctx, &path, &tm.model.result_text(), true);
        }
        let mut applied = tm.model.clone();
        applied.apply_non_conflicting();
        let applied_text = applied.result_text();
        let shown = tm.model.result_text();
        let with_markers = tm.model.text_with_markers(&self.marker_style);
        let (p1, p2, p3) = (path.clone(), path.clone(), path);
        let title = match (conflicts, pending) {
            (0, p) => format!("{p} non-conflicting change(s) not applied"),
            (c, 0) => format!("{c} conflict(s) unresolved"),
            (c, p) => format!("{c} conflict(s), {p} change(s) unresolved"),
        };
        Action::popup(MenuPopup::new(
            title,
            vec![
                MenuItem::new(
                    "Apply non-conflicting changes, save and mark resolved",
                    "a",
                    move |ctx| save_action(ctx, &p1, &applied_text, true),
                )
                .enabled(conflicts == 0),
                MenuItem::new(
                    "Save as shown and mark resolved (base text kept)",
                    "s",
                    move |ctx| save_action(ctx, &p2, &shown, true),
                ),
                MenuItem::new(
                    "Save with conflict markers (file stays unresolved)",
                    "m",
                    move |ctx| save_action(ctx, &p3, &with_markers, false),
                )
                .enabled(conflicts > 0),
                MenuItem::new("Cancel", "c", |_| Action::None),
            ],
        ))
    }

    fn close(&self) -> Action {
        if !self.dirty {
            return Action::ClosePopup;
        }
        let msg = if self.text().is_some_and(|tm| tm.edited.is_some()) {
            "Close the merge resolver without saving? Edits made in the external editor stay in the work-tree file, \
             but the file is not marked resolved."
        } else {
            "Close the merge resolver without saving? Your resolutions will be lost."
        };
        Action::popup(ConfirmPopup::new("Discard merge", msg, |_| Action::ClosePopup).danger())
    }

    fn accept_file(&self, ctx: &Ctx, side: Side) -> Action {
        let deleted = match &self.mode {
            Mode::Choice(c) => c.stages.side(side).is_none(),
            Mode::Text(_) => false,
        };
        match conflict::accept_side(&ctx.repo, std::slice::from_ref(&self.path), side) {
            Ok(_) => {
                let msg = if deleted {
                    format!("Resolved {} by deleting it", self.path)
                } else {
                    format!("Resolved {} with {}", self.path, side.label())
                };
                Action::ClosePopup
                    .then(Action::Toast(msg))
                    .then(Action::Refresh)
            }
            Err(e) => Action::Error(format!("{e:#}")),
        }
    }

    fn show_help(&self, ctx: &Ctx) -> Action {
        let t = &ctx.theme;
        let keys: &[KeyHelp] = match self.mode {
            Mode::Text(_) => &TEXT_HELP,
            Mode::Choice(_) => &CHOICE_HELP,
        };
        let lines = keys
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(format!("  {k:<18}"), t.key()),
                    Span::styled(d.to_string(), t.text()),
                ])
            })
            .collect();
        Action::popup(MessagePopup::from_lines("Merge resolver keys", lines))
    }

    // ----- drawing -------------------------------------------------------------------------

    fn draw_header(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let mut spans = vec![
            Span::styled(" Merge ", t.title().add_modifier(Modifier::REVERSED)),
            Span::styled(format!(" {} ", self.path), t.text()),
            Span::styled(format!("— {}", self.kind.label()), t.dimmed()),
        ];
        if self.dirty {
            spans.push(Span::styled("  ● unsaved", Style::default().fg(t.warning)));
        }
        f.render_widget(Line::from(spans), area);
    }

    fn draw_keys(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let keys: &[(&str, &str)] = match self.mode {
            Mode::Text(_) => &[
                ("n/N", "next/prev"),
                ("h", "yours"),
                ("l", "theirs"),
                ("b/B", "both"),
                ("x", "discard"),
                ("u", "undo"),
                ("A", "apply non-conflicting"),
                ("e", "editor"),
                ("Ctrl+S", "save"),
                ("Esc", "close"),
                ("?", "keys"),
            ],
            Mode::Choice(_) => &[
                ("h/o", "accept yours"),
                ("l/t", "accept theirs"),
                ("Esc", "close"),
            ],
        };
        let mut spans = Vec::new();
        for (k, d) in keys {
            spans.push(Span::styled(format!(" {k}"), t.key()));
            spans.push(Span::styled(format!(" {d} "), t.dimmed()));
        }
        f.render_widget(Line::from(spans), area);
    }

    fn status_spans(&self, t: &Theme) -> Vec<Span<'static>> {
        let Some(tm) = self.text() else {
            return Vec::new();
        };
        let mut spans = Vec::new();
        if let Some(e) = &tm.edited {
            let n = e.markers.len();
            let style = if n == 0 {
                Style::default().fg(t.success)
            } else {
                Style::default().fg(t.conflict)
            };
            spans.push(Span::styled(
                format!(" Edited result · {n} conflict marker block(s) "),
                style,
            ));
        } else {
            let c = tm.model.conflicts_remaining();
            let p = tm.model.pending_changes();
            let changes = tm.model.changes();
            if tm.model.is_fully_resolved() {
                spans.push(Span::styled(
                    " All changes resolved — Ctrl+S to save ",
                    Style::default().fg(t.success),
                ));
            } else {
                let style = if c > 0 {
                    Style::default().fg(t.conflict)
                } else {
                    Style::default().fg(t.warning)
                };
                spans.push(Span::styled(
                    format!(" {c} conflict(s) remaining "),
                    style.add_modifier(Modifier::BOLD),
                ));
                if p > 0 {
                    spans.push(Span::styled(
                        format!("· {p} non-conflicting change(s) to apply (A) "),
                        t.dimmed(),
                    ));
                }
            }
            if let Some(pos) = tm
                .current
                .and_then(|ci| changes.iter().position(|&x| x == ci))
            {
                spans.push(Span::styled(
                    format!("· change {}/{} ", pos + 1, changes.len()),
                    t.dimmed(),
                ));
            }
        }
        if let Some((msg, err)) = &self.notice {
            let style = if *err {
                Style::default().fg(t.error)
            } else {
                Style::default().fg(t.accent)
            };
            spans.push(Span::styled(format!(" {msg}"), style));
        }
        spans
    }

    fn draw_text(&mut self, f: &mut Frame, body: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let [left, mid, right] = Layout::horizontal([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .areas(body);
        self.height = body.height.saturating_sub(2) as usize;
        if self.focus_pending && self.height > 0 {
            self.focus_pending = false;
            self.reveal_current();
        }
        let total = self.text().map(TextMerge::total_rows).unwrap_or(0);
        self.scroll = self.scroll.min(total.saturating_sub(self.height.max(1)));
        let Some(tm) = self.text() else { return };
        let panes = [
            (Pane::Ours, left, self.ours_title.as_str()),
            (Pane::Result, mid, "Result"),
            (Pane::Theirs, right, self.theirs_title.as_str()),
        ];
        for (pane, rect, title) in panes {
            let block = panel(
                format!(
                    " {} ",
                    util::truncate(title, rect.width.saturating_sub(4) as usize)
                ),
                pane == Pane::Result,
                t,
            );
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let lines = match &tm.edited {
                Some(e) => edited_lines(tm, e, pane, self.scroll, inner, self.hscroll, t),
                None => model_lines(tm, pane, self.scroll, inner, self.hscroll, t),
            };
            f.render_widget(Paragraph::new(lines), inner);
        }
    }

    fn draw_choice(&self, f: &mut Frame, body: Rect, c: &Choice, t: &Theme) {
        let r = centered(body, body.width.min(100), body.height);
        let block = crate::ui::widgets::dialog(" Choose a version ", t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let size = |s: Option<usize>| match s {
            Some(n) => format!("{} bytes", n),
            None => "deleted".to_string(),
        };
        let reason = if c.binary {
            "Binary file — it cannot be merged line by line. Pick one version:".to_string()
        } else {
            format!(
                "This file was {} — keep one version or delete it:",
                self.kind.label()
            )
        };
        let accept = |side: Side| match (side, c.stages.side(side).is_some()) {
            (Side::Ours, true) => "keep yours",
            (Side::Theirs, true) => "keep theirs",
            _ => "delete the file",
        };
        let mut lines = vec![
            Line::styled(
                reason,
                Style::default().fg(t.conflict).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled(format!("  {:<24}", self.ours_title), t.text()),
                Span::styled(size(c.sizes[0]), t.dimmed()),
            ]),
            Line::from(vec![
                Span::styled(
                    format!("  {:<24}", util::truncate(&self.theirs_title, 24)),
                    t.text(),
                ),
                Span::styled(size(c.sizes[1]), t.dimmed()),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  h / o  ", t.key()),
                Span::styled(format!("Accept yours ({})", accept(Side::Ours)), t.text()),
            ]),
            Line::from(vec![
                Span::styled("  l / t  ", t.key()),
                Span::styled(
                    format!("Accept theirs ({})", accept(Side::Theirs)),
                    t.text(),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Esc    ", t.key()),
                Span::styled("Close", t.text()),
            ]),
        ];
        if !c.preview.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Content of the surviving version:",
                t.dimmed(),
            ));
            lines.extend(
                c.preview
                    .iter()
                    .map(|l| Line::styled(format!("  {l}"), t.text())),
            );
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

/// 1-based line of the first conflict marker in `text`.
fn first_marker_line(text: &str) -> Option<usize> {
    conflict::marker_lines(text).first().map(|l| l + 1)
}

/// Write `content` to the work-tree file (not staged) and open it in the editor.
fn write_and_edit(
    ctx: &Ctx,
    path: &str,
    content: &str,
    fallback_line: Option<usize>,
    shared: &Shared,
) -> Action {
    if let Err(e) = conflict::write_result(&ctx.repo, path, content.as_bytes(), false) {
        return Action::Error(format!("{e:#}"));
    }
    *shared.last_written.borrow_mut() = Some(content.as_bytes().to_vec());
    open_editor(
        ctx,
        path,
        first_marker_line(content).or(fallback_line),
        shared,
    )
}

/// Open the work-tree file in the external editor; it is reloaded as the result on the next refresh.
fn open_editor(ctx: &Ctx, path: &str, line: Option<usize>, shared: &Shared) -> Action {
    shared.reload.set(true);
    Action::edit(ctx.repo.path(path), line)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Write `content` (optionally staging it), then close the resolver. On failure the resolver stays
/// open so no work is lost.
fn save_action(ctx: &Ctx, path: &str, content: &str, resolve: bool) -> Action {
    match conflict::write_result(&ctx.repo, path, content.as_bytes(), resolve) {
        Ok(()) => {
            let msg = if resolve {
                format!("Resolved {path}")
            } else {
                format!("Saved {path} (still unresolved)")
            };
            Action::ClosePopup
                .then(Action::Toast(msg))
                .then(Action::Refresh)
        }
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

fn digits(n: usize) -> usize {
    n.max(1).to_string().len()
}

/// Whether a resolved chunk's lines from `pane` made it into the result.
fn included(c: &Chunk, pane: Pane, r: Resolution) -> bool {
    let both = matches!(r, Resolution::OursThenTheirs | Resolution::TheirsThenOurs);
    match pane {
        Pane::Result => true,
        Pane::Ours => {
            both || r == Resolution::Ours
                || (c.kind == ChunkKind::BothSame && r == Resolution::Theirs)
        }
        Pane::Theirs => {
            both || r == Resolution::Theirs
                || (c.kind == ChunkKind::BothSame && r == Resolution::Ours)
        }
    }
}

fn chunk_style(t: &Theme, c: &Chunk, pane: Pane) -> Style {
    if !c.is_change() {
        return t.text();
    }
    match c.resolution {
        None => match (c.kind, pane) {
            (ChunkKind::Conflict, _) => Style::default().fg(t.conflict).bg(t.removed_bg),
            (ChunkKind::OursOnly, Pane::Ours)
            | (ChunkKind::TheirsOnly, Pane::Theirs)
            | (ChunkKind::BothSame, Pane::Ours | Pane::Theirs) => {
                Style::default().fg(t.added).bg(t.added_bg)
            }
            (_, Pane::Result) => Style::default().fg(t.modified),
            _ => t.dimmed(),
        },
        Some(r) if included(c, pane, r) => Style::default().fg(t.success),
        Some(_) => t.dimmed().add_modifier(Modifier::CROSSED_OUT),
    }
}

/// Gutter marker for a row: chunk state in the result pane, selection elsewhere.
fn marker(t: &Theme, c: &Chunk, row: &Row, pane: Pane, current: bool) -> Span<'static> {
    if !c.is_change() {
        return Span::raw(" ");
    }
    if pane == Pane::Result {
        if !row.first {
            return Span::styled(if current { "│" } else { " " }, t.key());
        }
        let (sym, color) = match (c.resolution, c.kind) {
            (Some(_), _) => ("✓", t.success),
            (None, ChunkKind::Conflict) => ("!", t.conflict),
            (None, _) => ("•", t.modified),
        };
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        return Span::styled(
            sym,
            if current {
                style.bg(t.selection_bg)
            } else {
                style
            },
        );
    }
    match (current, row.first) {
        (true, true) => Span::styled("▶", t.key()),
        (true, false) => Span::styled("│", t.key()),
        _ => Span::raw(" "),
    }
}

fn padded(text: String, width: usize, style: Style) -> Span<'static> {
    let w = unicode_width::UnicodeWidthStr::width(text.as_str());
    Span::styled(
        format!("{text}{}", " ".repeat(width.saturating_sub(w))),
        style,
    )
}

fn model_lines(
    tm: &TextMerge,
    pane: Pane,
    scroll: usize,
    area: Rect,
    hscroll: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    let max_no = match pane {
        Pane::Ours => tm.ours_lines.len(),
        Pane::Theirs => tm.theirs_lines.len(),
        Pane::Result => tm.rows.rows.len(),
    };
    let nw = digits(max_no);
    let text_w = (area.width as usize).saturating_sub(nw + 2);
    tm.rows
        .rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .map(|row| {
            let c = &tm.model.chunks[row.chunk];
            let cell: Option<Cell> = match pane {
                Pane::Ours => row.ours,
                Pane::Result => row.result,
                Pane::Theirs => row.theirs,
            };
            let text: String = match (pane, cell) {
                (Pane::Ours, Some(x)) => c.ours[x.idx].clone(),
                (Pane::Theirs, Some(x)) => c.theirs[x.idx].clone(),
                (Pane::Result, Some(x)) => c
                    .result_lines()
                    .get(x.idx)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let no = cell
                .map(|x| format!("{:>nw$}", x.line_no))
                .unwrap_or_else(|| " ".repeat(nw));
            let current = tm.current == Some(row.chunk);
            let style = chunk_style(t, c, pane);
            Line::from(vec![
                Span::styled(no, t.dimmed()),
                marker(t, c, row, pane, current),
                Span::raw(" "),
                padded(rows::clip(&text, hscroll, text_w), text_w, style),
            ])
        })
        .collect()
}

fn edited_lines(
    tm: &TextMerge,
    e: &Edited,
    pane: Pane,
    scroll: usize,
    area: Rect,
    hscroll: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    let lines: &[String] = match pane {
        Pane::Ours => &tm.ours_lines,
        Pane::Result => &e.lines,
        Pane::Theirs => &tm.theirs_lines,
    };
    let nw = digits(lines.len());
    let text_w = (area.width as usize).saturating_sub(nw + 2);
    (scroll..scroll + area.height as usize)
        .map(|i| {
            let Some(l) = lines.get(i) else {
                return Line::raw("");
            };
            let conflicted = pane == Pane::Result && e.in_conflict.get(i).copied().unwrap_or(false);
            let style = if conflicted {
                Style::default().fg(t.conflict).bg(t.removed_bg)
            } else {
                t.text()
            };
            let mark = if pane == Pane::Result && e.markers.contains(&i) {
                Span::styled(
                    "!",
                    Style::default().fg(t.conflict).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ")
            };
            Line::from(vec![
                Span::styled(format!("{:>nw$}", i + 1), t.dimmed()),
                mark,
                Span::raw(" "),
                padded(rows::clip(l, hscroll, text_w), text_w, style),
            ])
        })
        .collect()
}

impl Popup for MergeResolver {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.apply_requests();
        let t = &ctx.theme;
        f.render_widget(Clear, area);
        let [header, body, status, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, header, t);
        match &self.mode {
            Mode::Choice(c) => self.draw_choice(f, body, c, t),
            Mode::Text(_) => self.draw_text(f, body, ctx),
        }
        f.render_widget(Line::from(self.status_spans(t)), status);
        self.draw_keys(f, keys, t);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        self.apply_requests();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(self.mode, Mode::Choice(_)) {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Action::ClosePopup,
                KeyCode::Char('h' | 'o') => self.accept_file(ctx, Side::Ours),
                KeyCode::Char('l' | 't') => self.accept_file(ctx, Side::Theirs),
                KeyCode::Char('?') => self.show_help(ctx),
                _ => Action::None,
            };
        }
        if ctrl {
            return match key.code {
                KeyCode::Char('s') => self.save(ctx),
                KeyCode::Char('d') => {
                    self.scroll_by(self.height as isize / 2);
                    Action::None
                }
                KeyCode::Char('u') => {
                    self.scroll_by(-(self.height as isize / 2));
                    Action::None
                }
                _ => Action::None,
            };
        }
        self.notice = None;
        let page = self.height.max(1) as isize;
        match key.code {
            KeyCode::Esc => return self.close(),
            KeyCode::Char('?') => return self.show_help(ctx),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page),
            KeyCode::PageUp => self.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(isize::MAX / 2),
            KeyCode::Left => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Right => self.hscroll += 8,
            KeyCode::Char('n') => self.goto(true, true),
            KeyCode::Char('N') => self.goto(false, true),
            KeyCode::Tab | KeyCode::Char(']') => self.goto(true, false),
            KeyCode::BackTab | KeyCode::Char('[') => self.goto(false, false),
            KeyCode::Char('h') => self.act(Some(Resolution::Ours)),
            KeyCode::Char('l') => self.act(Some(Resolution::Theirs)),
            KeyCode::Char('b') => self.act(Some(Resolution::OursThenTheirs)),
            KeyCode::Char('B') => self.act(Some(Resolution::TheirsThenOurs)),
            KeyCode::Char('x') => self.act(Some(Resolution::Base)),
            KeyCode::Char('u') => self.act(None),
            KeyCode::Char('A') => self.apply_non_conflicting(),
            KeyCode::Char('H') => self.accept_everywhere(Resolution::Ours),
            KeyCode::Char('L') => self.accept_everywhere(Resolution::Theirs),
            KeyCode::Char('R') => return self.request_reset(),
            KeyCode::Char('W') => self.load_worktree(ctx),
            KeyCode::Char('e') => return self.edit_externally(ctx),
            _ => {}
        }
        Action::None
    }

    fn refresh(&mut self, ctx: &Ctx) {
        self.apply_requests();
        if !self.shared.reload.take() {
            return;
        }
        match std::fs::read(ctx.repo.path(&self.path)) {
            Ok(b) if !conflict::is_binary(&b) => self.set_edited(
                String::from_utf8_lossy(&b).into_owned(),
                "Loaded your edits — Ctrl+S saves and marks resolved, R returns to the merge model",
            ),
            Ok(_) => {
                self.warn("The edited file is not UTF-8 text; press R to return to the merge model")
            }
            Err(e) => self.warn(format!("Cannot read the edited file: {e}")),
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        match self.mode {
            Mode::Text(_) => TEXT_HELP.to_vec(),
            Mode::Choice(_) => CHOICE_HELP.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;
    use crate::git::Repo;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn conflict_repo() -> (TempDir, Ctx) {
        let (d, repo) = temp_repo();
        commit_files(&repo, &[("f.txt", "a\nb\nc\nd\n")], "base");
        repo.run(&["checkout", "-q", "-b", "feature"]).unwrap();
        commit_files(&repo, &[("f.txt", "a\nB-feature\nc\nd\n")], "feature");
        repo.run(&["checkout", "-q", "main"]).unwrap();
        commit_files(&repo, &[("f.txt", "a\nB-main\nc\nD\n")], "main");
        assert!(repo.run(&["merge", "feature"]).is_err());
        let ctx = Ctx::new(Repo::discover(&repo.root).unwrap(), Config::default());
        (d, ctx)
    }

    fn model(r: &MergeResolver) -> &MergeModel {
        &r.text().unwrap().model
    }

    #[test]
    fn keys_resolve_chunks() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        assert_eq!(r.theirs_title, "Theirs (feature)");
        assert_eq!(r.kind, ConflictKind::BothModified);
        // Line 2 conflicts, line 4 changed only on our side. The resolver starts on the conflict.
        let m = model(&r);
        assert_eq!(m.conflicts_remaining(), 1);
        let ci = r.text().unwrap().current.unwrap();
        assert!(m.chunks[ci].is_conflict());
        r.handle_key(key('l'), &ctx);
        assert_eq!(model(&r).chunks[ci].resolution, Some(Resolution::Theirs));
        assert!(r.dirty);
        r.handle_key(key('u'), &ctx);
        assert_eq!(model(&r).chunks[ci].resolution, None);
        r.handle_key(key('b'), &ctx);
        r.handle_key(key('A'), &ctx);
        assert!(model(&r).is_fully_resolved());
        assert_eq!(model(&r).result_text(), "a\nB-main\nB-feature\nc\nD\n");
        // n with nothing unresolved keeps the selection and notes it.
        r.handle_key(key('n'), &ctx);
        assert!(r.notice.is_some());
        // Esc with unsaved changes asks first.
        assert!(matches!(
            r.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctx),
            Action::Popup(_)
        ));
        // Ctrl+S on a fully resolved model saves, marks resolved and closes.
        let a = r.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &ctx,
        );
        let Action::Batch(v) = a else {
            panic!("expected batch")
        };
        assert!(matches!(v[0], Action::ClosePopup));
        assert_eq!(conflict::unmerged_count(&ctx.repo).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(ctx.repo.path("f.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "a\nB-main\nB-feature\nc\nD\n"
        );
    }

    #[test]
    fn unresolved_save_offers_choices_and_edited_mode() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        let a = r.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &ctx,
        );
        assert!(
            matches!(a, Action::Popup(_)),
            "unresolved conflicts open the save menu"
        );
        // Work-tree file (git's conflict markers) as result.
        r.handle_key(key('W'), &ctx);
        let e = r.text().unwrap().edited.as_ref().expect("edited");
        assert_eq!(e.markers.len(), 1);
        assert!(e.in_conflict.iter().any(|x| *x));
        // chunk keys are refused while edited
        r.handle_key(key('h'), &ctx);
        assert!(r.notice.as_ref().is_some_and(|(_, err)| *err));
        let Action::Popup(mut confirm) = r.handle_key(key('R'), &ctx) else {
            panic!("reset asks")
        };
        confirm.handle_key(key('y'), &ctx);
        r.handle_key(key('j'), &ctx);
        assert!(r.text().unwrap().edited.is_none());
    }

    fn has_suspend(a: &Action) -> bool {
        match a {
            Action::Edit { .. } => true,
            Action::Batch(v) => v.iter().any(has_suspend),
            _ => false,
        }
    }

    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test]
    fn editor_round_trip_reloads_result() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        // git's conflict output equals the resolver's text with markers here: nothing to protect.
        assert!(has_suspend(&r.handle_key(key('e'), &ctx)));
        let written = std::fs::read_to_string(ctx.repo.path("f.txt")).unwrap();
        assert!(
            written.starts_with("a\n<<<<<<< HEAD\nB-main\n=======\n"),
            "markers for conflicts: {written}"
        );
        assert!(
            written.ends_with(">>>>>>> feature\nc\nD\n"),
            "non-conflicting changes applied: {written}"
        );
        // Simulate the user's editor.
        std::fs::write(ctx.repo.path("f.txt"), "merged by hand\n").unwrap();
        r.refresh(&ctx);
        let e = r.text().unwrap().edited.as_ref().expect("reloaded");
        assert_eq!(e.lines, vec!["merged by hand".to_string()]);
        assert!(e.markers.is_empty());
        // Unrelated refreshes do not reload again.
        std::fs::write(ctx.repo.path("f.txt"), "changed elsewhere\n").unwrap();
        r.refresh(&ctx);
        assert_eq!(
            r.text().unwrap().edited.as_ref().unwrap().lines,
            vec!["merged by hand".to_string()]
        );
        // The file differs from the edited result again, so 'e' asks; 'w' writes the result.
        let Action::Popup(mut menu) = r.handle_key(key('e'), &ctx) else {
            panic!("asks")
        };
        assert!(has_suspend(&menu.handle_key(key('w'), &ctx)));
        assert_eq!(
            std::fs::read_to_string(ctx.repo.path("f.txt")).unwrap(),
            "merged by hand\n"
        );
        // Unchanged since the resolver wrote it: no question.
        assert!(has_suspend(&r.handle_key(key('e'), &ctx)));
    }

    #[test]
    fn editor_offers_to_replace_gits_markers_first() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        r.handle_key(key('l'), &ctx);
        // The file still holds git's markers, which differ from the resolved result: ask, with
        // "replace" as the first entry.
        let Action::Popup(mut menu) = r.handle_key(key('e'), &ctx) else {
            panic!("asks")
        };
        assert!(has_suspend(&menu.handle_key(enter(), &ctx)));
        assert_eq!(
            std::fs::read_to_string(ctx.repo.path("f.txt")).unwrap(),
            "a\nB-feature\nc\nD\n"
        );
    }

    #[test]
    fn editor_keeps_hand_edits_by_default() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        r.handle_key(key('l'), &ctx);
        std::fs::write(ctx.repo.path("f.txt"), "resolved by hand\n").unwrap();
        let Action::Popup(mut menu) = r.handle_key(key('e'), &ctx) else {
            panic!("asks")
        };
        // The first entry edits the file as it is.
        assert!(has_suspend(&menu.handle_key(enter(), &ctx)));
        assert_eq!(
            std::fs::read_to_string(ctx.repo.path("f.txt")).unwrap(),
            "resolved by hand\n"
        );
        r.refresh(&ctx);
        assert_eq!(
            r.text().unwrap().edited.as_ref().unwrap().lines,
            vec!["resolved by hand".to_string()]
        );
        // Cancelling leaves everything alone.
        std::fs::write(ctx.repo.path("f.txt"), "again\n").unwrap();
        let Action::Popup(mut menu) = r.handle_key(key('e'), &ctx) else {
            panic!("asks")
        };
        assert!(!has_suspend(&menu.handle_key(key('c'), &ctx)));
        assert_eq!(
            std::fs::read_to_string(ctx.repo.path("f.txt")).unwrap(),
            "again\n"
        );
    }

    #[test]
    fn reset_asks_first() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        assert!(r.handle_key(key('R'), &ctx).is_none(), "nothing to reset");
        r.handle_key(key('l'), &ctx);
        let resolved = |r: &MergeResolver| model(r).chunks.iter().any(|c| c.resolution.is_some());
        let Action::Popup(mut confirm) = r.handle_key(key('R'), &ctx) else {
            panic!("asks")
        };
        confirm.handle_key(key('n'), &ctx);
        r.handle_key(key('j'), &ctx);
        assert!(resolved(&r), "declining keeps the resolutions");
        let Action::Popup(mut confirm) = r.handle_key(key('R'), &ctx) else {
            panic!("asks")
        };
        confirm.handle_key(key('y'), &ctx);
        r.refresh(&ctx);
        assert!(!resolved(&r), "confirming clears them");
        // Text edits are dropped the same way.
        r.handle_key(key('W'), &ctx);
        let Action::Popup(mut confirm) = r.handle_key(key('R'), &ctx) else {
            panic!("asks")
        };
        confirm.handle_key(enter(), &ctx);
        render(&mut r, &ctx, 80, 10);
        assert!(r.text().unwrap().edited.is_none());
    }

    #[test]
    fn binary_conflicts_use_choice_mode() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("b.bin", "\0base")], "base");
        repo.run(&["checkout", "-q", "-b", "feature"]).unwrap();
        commit_files(&repo, &[("b.bin", "\0theirs")], "t");
        repo.run(&["checkout", "-q", "main"]).unwrap();
        commit_files(&repo, &[("b.bin", "\0ours")], "o");
        assert!(repo.run(&["merge", "feature"]).is_err());
        let ctx = Ctx::new(repo, Config::default());
        let mut r = MergeResolver::load(&ctx, "b.bin").unwrap();
        assert!(matches!(r.mode, Mode::Choice(Choice { binary: true, .. })));
        let a = r.handle_key(key('t'), &ctx);
        assert!(matches!(a, Action::Batch(_)));
        assert_eq!(std::fs::read(ctx.repo.path("b.bin")).unwrap(), b"\0theirs");
        assert!(MergeResolver::load(&ctx, "missing.txt").is_err());
    }

    fn render(r: &mut MergeResolver, ctx: &Ctx, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| r.draw(f, f.area(), ctx)).unwrap();
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
    fn renders_three_panes_at_any_size() {
        let (_d, ctx) = conflict_repo();
        let mut r = MergeResolver::load(&ctx, "f.txt").unwrap();
        let screen = render(&mut r, &ctx, 120, 20);
        assert!(screen.contains("Yours (HEAD)"), "{screen}");
        assert!(screen.contains("Result"));
        assert!(screen.contains("Theirs (feature)"));
        assert!(screen.contains("B-main") && screen.contains("B-feature"));
        assert!(screen.contains("1 conflict(s) remaining"));
        for (w, h) in [(10, 3), (30, 6), (1, 1)] {
            render(&mut r, &ctx, w, h);
        }
        r.handle_key(key('W'), &ctx);
        let screen = render(&mut r, &ctx, 120, 20);
        assert!(screen.contains("<<<<<<< HEAD"), "{screen}");
    }

    #[test]
    fn styles_follow_resolution() {
        let t = Theme::dark();
        let mut m = MergeModel::new("a\n", "b\n", "c\n");
        let c = &m.chunks[0];
        assert_eq!(chunk_style(&t, c, Pane::Ours).fg, Some(t.conflict));
        m.resolve(0, Resolution::Ours);
        let c = &m.chunks[0];
        assert_eq!(chunk_style(&t, c, Pane::Ours).fg, Some(t.success));
        assert!(chunk_style(&t, c, Pane::Theirs)
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
        assert!(included(c, Pane::Result, Resolution::Base));
    }
}
