//! The fullscreen todo-list editor.
//!
//! Dialogs opened from the editor (message editor, context menu) talk back through a shared
//! command queue ([`Inbox`]) that the editor drains before every draw and key press.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::rebase::{self, Editors, RebasePlan, RebaseRange, TodoAction, TodoEntry, ROOT};
use crate::theme::Theme;
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{ConfirmPopup, MenuItem, MenuPopup, TextAreaPopup};
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, Popup};
use crate::util::fit;

/// Edits requested by dialogs opened on top of the editor.
enum Cmd {
    SetAction(TodoAction),
    SetMessage { id: String, message: String },
    Move(isize),
    Reset,
    ResetAll,
}

type Inbox = Rc<RefCell<Vec<Cmd>>>;

/// Width of the fixed columns: change marker, action, hash.
const MARK_W: usize = 2;
const ACTION_W: usize = 8;
const HASH_W: usize = 10;

pub struct RebaseEditor {
    range: RebaseRange,
    rows: Vec<TodoEntry>,
    /// Branch being rebased, for titles.
    branch: String,
    /// "main (abc1234)" style description of the base.
    base_label: String,
    sel: usize,
    offset: usize,
    list_area: Rect,
    /// Full commit messages by id (loaded lazily).
    messages: HashMap<String, String>,
    inbox: Inbox,
    search: Option<TextInput>,
    query: String,
    /// Set while the started rebase runs in the background; the editor ignores keys meanwhile.
    running: Rc<Cell<Option<Instant>>>,
}

impl RebaseEditor {
    /// Read the commits after `base` and start with every commit picked.
    pub fn load(ctx: &Ctx, base: &str) -> Result<Self> {
        let range = rebase::load_range(&ctx.repo, base)?;
        let base_label = if range.base == ROOT {
            "the root (whole history)".to_string()
        } else {
            let short: String = ctx.repo.rev_parse(&range.base)?.chars().take(7).collect();
            if range.base.starts_with(&short) {
                short
            } else {
                format!("{} ({short})", range.base)
            }
        };
        let branch = ctx
            .repo
            .head_branch()
            .unwrap_or_else(|| "detached HEAD".into());
        let rows = range.commits.iter().cloned().map(TodoEntry::pick).collect();
        Ok(Self {
            range,
            rows,
            branch,
            base_label,
            sel: 0,
            offset: 0,
            list_area: Rect::default(),
            messages: HashMap::new(),
            inbox: Rc::new(RefCell::new(Vec::new())),
            search: None,
            query: String::new(),
            running: Rc::new(Cell::new(None)),
        })
    }

    /// "Squash last N commits": everything melds into the oldest commit.
    pub fn preset_squash(&mut self) {
        for r in self.rows.iter_mut().skip(1) {
            r.action = TodoAction::Squash;
            r.message = None;
        }
        self.sel = 0;
    }

    /// Select the row of commit `id` (no-op when it is not listed).
    pub fn select_commit(&mut self, id: &str) {
        if let Some(i) = self.rows.iter().position(|r| r.commit.id == id) {
            self.sel = i;
        }
    }

    fn is_edited(&self) -> bool {
        (0..self.rows.len()).any(|i| self.row_changed(i))
    }

    fn row_changed(&self, i: usize) -> bool {
        let r = &self.rows[i];
        r.action != TodoAction::Pick
            || r.message.is_some()
            || self
                .range
                .commits
                .get(i)
                .map(|c| c.id != r.commit.id)
                .unwrap_or(true)
    }

    fn drain_inbox(&mut self) {
        let cmds: Vec<Cmd> = self.inbox.borrow_mut().drain(..).collect();
        for c in cmds {
            self.apply(c);
        }
    }

    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SetAction(a) => self.set_plain_action(a),
            Cmd::SetMessage { id, message } => {
                if let Some(r) = self.rows.iter_mut().find(|r| r.commit.id == id) {
                    r.action = TodoAction::Reword;
                    r.message = Some(message);
                }
            }
            Cmd::Move(d) => self.move_row(d),
            Cmd::Reset => self.reset_row(),
            Cmd::ResetAll => {
                self.rows = self
                    .range
                    .commits
                    .iter()
                    .cloned()
                    .map(TodoEntry::pick)
                    .collect();
                self.sel = self.sel.min(self.rows.len().saturating_sub(1));
            }
        }
    }

    /// Any action except reword (which needs a message first).
    fn set_plain_action(&mut self, a: TodoAction) {
        if let Some(r) = self.rows.get_mut(self.sel) {
            r.action = a;
            r.message = None;
        }
    }

    fn move_row(&mut self, delta: isize) {
        let to = move_sel(self.sel, delta, self.rows.len());
        if to != self.sel {
            let row = self.rows.remove(self.sel);
            self.rows.insert(to, row);
            self.sel = to;
        }
    }

    /// Back to `pick` with the original message, at the commit's original position.
    fn reset_row(&mut self) {
        let Some(r) = self.rows.get(self.sel) else {
            return;
        };
        let id = r.commit.id.clone();
        let Some(orig) = self.range.commits.iter().position(|c| c.id == id) else {
            return;
        };
        let mut row = self.rows.remove(self.sel);
        row.action = TodoAction::Pick;
        row.message = None;
        let at = orig.min(self.rows.len());
        self.rows.insert(at, row);
        self.sel = at;
    }

    fn full_message(&mut self, ctx: &Ctx, i: usize) -> String {
        let c = &self.rows[i].commit;
        if let Some(m) = self.messages.get(&c.id) {
            return m.clone();
        }
        let m = ctx
            .repo
            .commit_message(&c.id)
            .unwrap_or_else(|_| c.subject.clone());
        self.messages.insert(c.id.clone(), m.clone());
        m
    }

    /// Message editor for the selected row's commit, or for the whole squash group it belongs to.
    pub fn message_popup(&mut self, ctx: &Ctx) -> Action {
        if self.rows.is_empty() {
            return Action::None;
        }
        let i = self.sel.min(self.rows.len() - 1);
        let target = rebase::group_target(&self.rows, i).unwrap_or(i);
        let members = rebase::group_members(&self.rows, target);
        let initial = match self.rows[target].message.clone() {
            Some(m) => m,
            None => {
                let mut text = self.full_message(ctx, target);
                for &j in &members {
                    if self.rows[j].action == TodoAction::Squash {
                        text.push_str("\n\n");
                        text.push_str(&self.full_message(ctx, j));
                    }
                }
                text
            }
        };
        let short = self.rows[target].commit.short.clone();
        let title = if members.is_empty() {
            format!("Reword {short}")
        } else {
            format!(
                "Message for {short} + {} melded commit{}",
                members.len(),
                if members.len() == 1 { "" } else { "s" }
            )
        };
        let id = self.rows[target].commit.id.clone();
        let inbox = self.inbox.clone();
        Action::popup(
            TextAreaPopup::new(title, &initial, move |message, _| {
                inbox.borrow_mut().push(Cmd::SetMessage { id, message });
                Action::None
            })
            .subject_limit(72),
        )
    }

    fn diff_action(&self) -> Action {
        match self.rows.get(self.sel) {
            Some(r) => diff_view::open(
                format!("{} {}", r.commit.short, r.commit.subject),
                DiffSource::Commit {
                    rev: r.commit.id.clone(),
                    paths: Vec::new(),
                },
            ),
            None => Action::None,
        }
    }

    fn original_ids(&self) -> Vec<String> {
        self.range.commits.iter().map(|c| c.id.clone()).collect()
    }

    /// Validate, then ask for confirmation with a summary of the plan.
    fn start_action(&self) -> Action {
        if let Err(e) = rebase::validate(&self.rows) {
            return Action::Error(format!("{e:#}"));
        }
        let plan = RebasePlan {
            base: self.range.base.clone(),
            head: self.range.head.clone(),
            entries: self.rows.clone(),
        };
        let warnings = rebase::warnings(&self.range, &self.rows);
        let mut text = format!(
            "Rebase {} onto {}\n{}\n",
            self.branch,
            self.base_label,
            rebase::summarize(&self.rows, &self.original_ids())
        );
        let changed: Vec<String> = (0..self.rows.len())
            .filter(|&i| self.rows[i].action != TodoAction::Pick)
            .map(|i| describe_change(&self.rows[i]))
            .collect();
        for line in changed.iter().take(12) {
            text.push_str(&format!("\n  {line}"));
        }
        if changed.len() > 12 {
            text.push_str(&format!("\n  … and {} more", changed.len() - 12));
        }
        if !changed.is_empty() {
            text.push('\n');
        }
        for w in &warnings {
            text.push_str(&format!("\n⚠ {w}"));
        }
        text.push_str("\n\nUncommitted changes are stashed and restored automatically.");
        let risky = !warnings.is_empty() || self.rows.iter().any(|r| r.action == TodoAction::Drop);
        let running = self.running.clone();
        let popup = ConfirmPopup::new("Start rebase?", text, move |ctx| {
            execute(ctx, plan, running)
        });
        Action::popup(if risky { popup.danger() } else { popup })
    }

    fn cancel_action(&self) -> Action {
        if !self.is_edited() {
            return Action::ClosePopup;
        }
        Action::popup(ConfirmPopup::new(
            "Discard rebase plan?",
            "Close the editor and lose your changes to the todo list?",
            |_| Action::ClosePopup,
        ))
    }

    fn menu(&mut self, ctx: &Ctx) -> Action {
        let Some(row) = self.rows.get(self.sel) else {
            return Action::None;
        };
        let title = format!(
            "{} {}",
            row.commit.short,
            crate::util::truncate(&row.commit.subject, 40)
        );
        let mut items = Vec::new();
        for a in TodoAction::ALL {
            let label = action_description(a);
            if a == TodoAction::Reword {
                let popup = self.message_popup(ctx);
                items.push(MenuItem::new(label, a.key().to_string(), move |_| popup));
            } else {
                let inbox = self.inbox.clone();
                items.push(MenuItem::new(label, a.key().to_string(), move |_| {
                    inbox.borrow_mut().push(Cmd::SetAction(a));
                    Action::None
                }));
            }
        }
        let push = |items: &mut Vec<MenuItem>, label: &str, key: &str, cmd: Cmd, enabled: bool| {
            let inbox = self.inbox.clone();
            items.push(
                MenuItem::new(label, key, move |_| {
                    inbox.borrow_mut().push(cmd);
                    Action::None
                })
                .enabled(enabled),
            );
        };
        let n = self.rows.len();
        push(&mut items, "Move up", "K", Cmd::Move(-1), self.sel > 0);
        push(&mut items, "Move down", "J", Cmd::Move(1), self.sel + 1 < n);
        push(
            &mut items,
            "Reset this commit",
            "u",
            Cmd::Reset,
            self.row_changed(self.sel),
        );
        push(
            &mut items,
            "Reset all changes",
            "U",
            Cmd::ResetAll,
            self.is_edited(),
        );
        let diff = self.diff_action();
        items.push(MenuItem::new("Show commit diff", "Enter", move |_| diff));
        let start = self.start_action();
        items.push(MenuItem::new("Start rebase…", "x", move |_| start));
        let cancel = self.cancel_action();
        items.push(MenuItem::new(
            "Cancel and close the editor",
            "c",
            move |_| cancel,
        ));
        Action::popup(MenuPopup::new(title, items))
    }

    fn matches(&self, i: usize) -> bool {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            return false;
        }
        let c = &self.rows[i].commit;
        c.subject.to_lowercase().contains(&q)
            || c.id.starts_with(&q)
            || c.author.to_lowercase().contains(&q)
            || self.rows[i]
                .message
                .as_deref()
                .is_some_and(|m| m.to_lowercase().contains(&q))
    }

    /// Select the next (or previous) row matching the search, starting after/before `from`.
    fn find(&mut self, from: usize, forward: bool) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        for k in 0..n {
            let i = if forward {
                (from + k) % n
            } else {
                (from + n - k % n) % n
            };
            if self.matches(i) {
                self.sel = i;
                return;
            }
        }
    }

    fn search_key(&mut self, key: KeyEvent) -> Action {
        let Some(input) = self.search.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.search = None;
                self.query.clear();
            }
            KeyCode::Enter => self.search = None,
            _ => {
                if input.handle_key(&key) {
                    self.query = input.value();
                    let from = self.sel;
                    self.find(from, true);
                }
            }
        }
        Action::None
    }

    fn page(&self) -> isize {
        self.list_area.height.max(1) as isize
    }
}

/// Run the plan in the background; on success close the editor, on refusal keep it open with the
/// error. `running` locks the editor meanwhile.
fn execute(ctx: &Ctx, plan: RebasePlan, running: Rc<Cell<Option<Instant>>>) -> Action {
    running.set(Some(Instant::now()));
    Action::run(
        &ctx.repo,
        "Rebasing",
        move |repo| rebase::start(repo, &plan, &Editors::production()),
        move |r| {
            running.set(None);
            match r {
                Ok(outcome) => Action::ClosePopup.then(super::outcome_action(Ok(outcome))),
                Err(e) => Action::Error(format!("{e:#}")),
            }
        },
    )
}

fn action_description(a: TodoAction) -> &'static str {
    match a {
        TodoAction::Pick => "pick — keep the commit",
        TodoAction::Reword => "reword — edit the message…",
        TodoAction::Edit => "edit — stop to amend the commit",
        TodoAction::Squash => "squash — meld into previous, keep message",
        TodoAction::Fixup => "fixup — meld into previous, discard message",
        TodoAction::Drop => "drop — remove the commit",
    }
}

fn describe_change(e: &TodoEntry) -> String {
    let subject = crate::util::truncate(&e.commit.subject, 50);
    match &e.message {
        Some(m) => format!(
            "{:<7}{} {subject} → {}",
            e.action.label(),
            e.commit.short,
            crate::util::truncate(m.lines().next().unwrap_or(""), 40)
        ),
        None => format!("{:<7}{} {subject}", e.action.label(), e.commit.short),
    }
}

fn action_style(a: TodoAction, t: &Theme) -> Style {
    let fg = match a {
        TodoAction::Pick => t.fg,
        TodoAction::Reword => t.accent,
        TodoAction::Edit => t.warning,
        TodoAction::Squash | TodoAction::Fixup => t.renamed,
        TodoAction::Drop => t.removed,
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

impl RebaseEditor {
    fn row_line(&self, i: usize, subject_w: usize, author_w: usize, t: &Theme) -> Line<'static> {
        let r = &self.rows[i];
        let dropped = r.action == TodoAction::Drop;
        let strike = |s: Style| {
            if dropped {
                s.fg(t.dim).add_modifier(Modifier::CROSSED_OUT)
            } else {
                s
            }
        };
        let pushed = self.range.pushed.contains(&r.commit.id);
        let mark = if self.row_changed(i) { "• " } else { "  " };
        let hash = format!("{}{}", r.commit.short, if pushed { " ↑" } else { "" });
        let hash_style = Style::default().fg(if pushed { t.warning } else { t.hash });
        let fold = if r.action.folds() { "↳ " } else { "" };
        let (subject, subject_style) = match &r.message {
            Some(m) => (
                format!("{fold}✎ {}", m.lines().next().unwrap_or("")),
                Style::default().fg(t.accent).add_modifier(Modifier::ITALIC),
            ),
            None => (format!("{fold}{}", r.commit.subject), t.text()),
        };
        let mut spans = vec![
            Span::styled(mark.to_string(), Style::default().fg(t.warning)),
            Span::styled(fit(r.action.label(), ACTION_W), action_style(r.action, t)),
            Span::styled(fit(&hash, HASH_W), strike(hash_style)),
            Span::styled(fit(&subject, subject_w), strike(subject_style)),
            Span::raw(" "),
            Span::styled(
                fit(&r.commit.author, author_w),
                strike(Style::default().fg(t.author)),
            ),
        ];
        if i == self.sel {
            let sel = t.selected();
            for s in spans.iter_mut() {
                s.style = s.style.patch(sel);
            }
        } else if self.matches(i) {
            for s in spans.iter_mut() {
                s.style = s.style.add_modifier(Modifier::UNDERLINED);
            }
        }
        Line::from(spans)
    }

    fn draw_list(&mut self, f: &mut Frame, header: Rect, area: Rect, t: &Theme) {
        self.list_area = area;
        let w = area.width as usize;
        let author_w = (w / 5).clamp(8, 22);
        let subject_w = w.saturating_sub(MARK_W + ACTION_W + HASH_W + 1 + author_w);
        let head = format!(
            "{}{}{}{} {}",
            " ".repeat(MARK_W),
            fit("ACTION", ACTION_W),
            fit("HASH", HASH_W),
            fit("SUBJECT", subject_w),
            fit("AUTHOR", author_w)
        );
        f.render_widget(Paragraph::new(head).style(t.dimmed()), header);
        let h = area.height as usize;
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
        self.offset = scroll_to_keep_visible(self.sel, self.offset, h);
        let lines: Vec<Line> = (self.offset..self.rows.len().min(self.offset + h))
            .map(|i| self.row_line(i, subject_w, author_w, t))
            .collect();
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_details(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let Some(r) = self.rows.get(self.sel).cloned() else {
            return;
        };
        let title = if r.message.is_some() {
            " New message ".to_string()
        } else if r.action.folds() {
            match rebase::group_target(&self.rows, self.sel) {
                Some(g) => format!(" Message — melds into {} ", self.rows[g].commit.short),
                None => " Message — nothing to meld into! ".to_string(),
            }
        } else {
            " Message ".to_string()
        };
        let block = panel(title, false, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let body = match &r.message {
            Some(m) => m.clone(),
            None => self.full_message(ctx, self.sel),
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(r.commit.id.clone(), Style::default().fg(t.hash)),
            Span::styled(
                format!("  {} <{}>", r.commit.author, r.commit.email),
                Style::default().fg(t.author),
            ),
            Span::styled(
                format!("  {}", crate::util::format_time(r.commit.time)),
                Style::default().fg(t.date),
            ),
        ])];
        if self.range.pushed.contains(&r.commit.id) {
            lines.push(Line::styled(
                "↑ already pushed to a remote",
                Style::default().fg(t.warning),
            ));
        }
        lines.extend(body.lines().map(|l| Line::styled(l.to_string(), t.text())));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

impl Popup for RebaseEditor {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.drain_inbox();
        let t = &ctx.theme;
        // Leave the status and key-hint rows visible so toasts and shortcuts stay readable.
        let area = Rect {
            height: area.height.saturating_sub(2).max(area.height.min(8)),
            ..area
        };
        f.render_widget(Clear, area);
        let n = self.rows.len();
        let title = format!(
            " Interactively rebase {}: {n} commit{} onto {} ",
            self.branch,
            if n == 1 { "" } else { "s" },
            self.base_label
        );
        let block = panel(title, true, t);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let warnings = rebase::warnings(&self.range, &self.rows);
        let invalid = rebase::validate(&self.rows).err().map(|e| format!("{e:#}"));
        let notes = warnings.len() as u16 + u16::from(invalid.is_some());
        let detail_h = (inner.height / 4).clamp(4, 10);
        let search_h = u16::from(self.search.is_some());
        let [hint, notes_area, header, list, details, search] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(notes),
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(detail_h),
            Constraint::Length(search_h),
        ])
        .areas(inner);

        let summary = rebase::summarize(&self.rows, &self.original_ids());
        let hint_line = match self.running.get() {
            Some(started) => Line::styled(
                format!(
                    "{} Rebasing {}… (updating files, this can take a while)",
                    crate::app::spinner_frame(started),
                    self.branch
                ),
                Style::default().fg(t.accent),
            ),
            None => Line::from(vec![
                Span::styled(
                    "Oldest commit at the top — git replays the list top to bottom.  ",
                    t.dimmed(),
                ),
                Span::styled(summary, Style::default().fg(t.accent)),
            ]),
        };
        f.render_widget(hint_line, hint);
        let mut note_lines: Vec<Line> = warnings
            .iter()
            .map(|w| Line::styled(format!("⚠ {w}"), Style::default().fg(t.warning)))
            .collect();
        if let Some(e) = invalid {
            note_lines.push(Line::styled(format!("✖ {e}"), Style::default().fg(t.error)));
        }
        f.render_widget(Paragraph::new(note_lines), notes_area);
        self.draw_list(f, header, list, t);
        self.draw_details(f, details, ctx);
        if let Some(input) = &self.search {
            let [p, field] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(search);
            f.render_widget(Span::styled("/ ", t.key()), p);
            input.render(f, field, t.text(), true);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        self.drain_inbox();
        if self.running.get().is_some() {
            return Action::None;
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        let n = self.rows.len();
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up if alt => self.move_row(-1),
            KeyCode::Down if alt => self.move_row(1),
            KeyCode::Char('K') => self.move_row(-1),
            KeyCode::Char('J') => self.move_row(1),
            KeyCode::Char('s') if ctrl => return self.start_action(),
            KeyCode::Char('x') => return self.start_action(),
            KeyCode::Up | KeyCode::Char('k') => self.sel = move_sel(self.sel, -1, n),
            KeyCode::Down | KeyCode::Char('j') => self.sel = move_sel(self.sel, 1, n),
            KeyCode::PageUp => self.sel = move_sel(self.sel, -self.page(), n),
            KeyCode::PageDown => self.sel = move_sel(self.sel, self.page(), n),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Esc => return self.cancel_action(),
            KeyCode::Enter => return self.diff_action(),
            KeyCode::Char('u') => self.reset_row(),
            KeyCode::Char('U') => self.apply(Cmd::ResetAll),
            KeyCode::Char('/') => {
                self.search = Some(TextInput::new(&self.query));
            }
            KeyCode::Char('n') => {
                let from = self.sel + 1;
                self.find(from, true);
            }
            KeyCode::Char('N') => {
                let from = self.sel + n.max(1) - 1;
                self.find(from, false);
            }
            KeyCode::Char('.') | KeyCode::Char('m') => return self.menu(ctx),
            KeyCode::Char('r') if !ctrl => return self.message_popup(ctx),
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(a) = TodoAction::ALL.into_iter().find(|a| a.key() == c) {
                    self.set_plain_action(a);
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, ev: MouseEvent, _ctx: &Ctx) -> Action {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            let a = self.list_area;
            if ev.column >= a.x
                && ev.column < a.x + a.width
                && ev.row >= a.y
                && ev.row < a.y + a.height
            {
                let i = self.offset + (ev.row - a.y) as usize;
                if i < self.rows.len() {
                    self.sel = i;
                }
            }
        }
        Action::None
    }

    fn refresh(&mut self, _ctx: &Ctx) {
        self.drain_inbox();
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("p/r/e", "pick/reword/edit"),
            ("s/f/d", "squash/fixup/drop"),
            ("Alt+↑↓/K J", "move"),
            ("Enter", "diff"),
            ("u/U", "reset row/all"),
            ("/ n N", "search"),
            (". m", "menu"),
            ("Ctrl+S/x", "start rebase"),
            ("Esc", "cancel"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;

    fn editor(n: usize) -> (TempDir, Ctx, RebaseEditor) {
        let (d, repo) = temp_repo();
        for k in 1..=n {
            commit_files(&repo, &[(&format!("f{k}.txt"), "x\n")], &format!("c{k}"));
        }
        let ctx = Ctx::new(repo, Config::default());
        let ed = RebaseEditor::load(&ctx, ROOT).unwrap();
        (d, ctx, ed)
    }

    fn key(c: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(c, m)
    }

    fn subjects(ed: &RebaseEditor) -> Vec<String> {
        ed.rows.iter().map(|r| r.commit.subject.clone()).collect()
    }

    #[test]
    fn keys_edit_the_plan() {
        let (_d, ctx, mut ed) = editor(3);
        assert_eq!(subjects(&ed), vec!["c1", "c2", "c3"]);
        assert!(!ed.is_edited());
        ed.handle_key(key(KeyCode::Char('j'), KeyModifiers::NONE), &ctx);
        ed.handle_key(key(KeyCode::Char('s'), KeyModifiers::NONE), &ctx);
        assert_eq!(ed.rows[1].action, TodoAction::Squash);
        ed.handle_key(key(KeyCode::Down, KeyModifiers::ALT), &ctx);
        assert_eq!(subjects(&ed), vec!["c1", "c3", "c2"]);
        assert_eq!(ed.sel, 2);
        ed.handle_key(key(KeyCode::Char('K'), KeyModifiers::SHIFT), &ctx);
        assert_eq!(subjects(&ed), vec!["c1", "c2", "c3"]);
        ed.handle_key(key(KeyCode::Char('d'), KeyModifiers::NONE), &ctx);
        assert_eq!(ed.rows[1].action, TodoAction::Drop);
        assert!(ed.is_edited());
        ed.handle_key(key(KeyCode::Char('u'), KeyModifiers::NONE), &ctx);
        assert_eq!(ed.rows[1].action, TodoAction::Pick);
        assert!(!ed.is_edited());
    }

    #[test]
    fn reset_restores_position_and_reword_goes_through_inbox() {
        let (_d, ctx, mut ed) = editor(3);
        ed.sel = 2;
        ed.move_row(-2);
        assert_eq!(subjects(&ed), vec!["c3", "c1", "c2"]);
        ed.reset_row();
        assert_eq!(subjects(&ed), vec!["c1", "c2", "c3"]);
        assert_eq!(ed.sel, 2);
        // The message dialog posts its result to the inbox.
        assert!(matches!(ed.message_popup(&ctx), Action::Popup(_)));
        let id = ed.rows[2].commit.id.clone();
        ed.inbox.borrow_mut().push(Cmd::SetMessage {
            id,
            message: "better".into(),
        });
        ed.drain_inbox();
        assert_eq!(ed.rows[2].action, TodoAction::Reword);
        assert_eq!(ed.rows[2].message.as_deref(), Some("better"));
        ed.set_plain_action(TodoAction::Pick);
        assert!(ed.rows[2].message.is_none());
    }

    #[test]
    fn squash_preset_and_search() {
        let (_d, ctx, mut ed) = editor(3);
        ed.preset_squash();
        let acts: Vec<TodoAction> = ed.rows.iter().map(|r| r.action).collect();
        assert_eq!(
            acts,
            vec![TodoAction::Pick, TodoAction::Squash, TodoAction::Squash]
        );
        ed.query = "c3".into();
        ed.find(0, true);
        assert_eq!(ed.sel, 2);
        ed.handle_key(key(KeyCode::Char('/'), KeyModifiers::NONE), &ctx);
        assert!(ed.search.is_some());
        ed.handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &ctx);
        assert!(ed.search.is_none() && ed.query.is_empty());
    }

    #[test]
    fn start_validates_first_commit() {
        let (_d, ctx, mut ed) = editor(2);
        ed.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE), &ctx);
        assert!(matches!(
            ed.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE), &ctx),
            Action::Error(_)
        ));
        ed.handle_key(key(KeyCode::Char('p'), KeyModifiers::NONE), &ctx);
        assert!(matches!(
            ed.handle_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL), &ctx),
            Action::Popup(_)
        ));
        // Esc without edits closes directly; with edits it asks first.
        assert!(matches!(
            ed.handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &ctx),
            Action::ClosePopup
        ));
        ed.handle_key(key(KeyCode::Char('d'), KeyModifiers::NONE), &ctx);
        assert!(matches!(
            ed.handle_key(key(KeyCode::Esc, KeyModifiers::NONE), &ctx),
            Action::Popup(_)
        ));
    }

    #[test]
    fn draws_without_panicking() {
        let (_d, ctx, mut ed) = editor(4);
        ed.rows[1].action = TodoAction::Drop;
        ed.rows[2].action = TodoAction::Fixup;
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| ed.draw(f, f.area(), &ctx)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("fixup") && text.contains("Oldest commit"));
        let small = ratatui::backend::TestBackend::new(20, 6);
        let mut term = ratatui::Terminal::new(small).unwrap();
        term.draw(|f| ed.draw(f, f.area(), &ctx)).unwrap();
    }
}
