//! The commit dialog: message editor with history, amend / sign-off / GPG / author options,
//! commit checks with "commit anyway", and optional push afterwards.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::commit::{self, CommitRequest};
use crate::git::Repo;
use crate::ui::popups::{MenuItem, MenuPopup};
use crate::ui::widgets::{centered, panel, TextArea, TextInput};
use crate::ui::{KeyHelp, Popup};

/// What a commit will contain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitTarget {
    /// Human description, e.g. `staged changes` or `changelist "Changes"`
    pub label: String,
    /// None = commit the index; Some = commit exactly these paths
    pub paths: Option<Vec<String>>,
    /// Untracked paths among `paths`
    pub untracked: Vec<String>,
    /// `(status letter, path)` for display
    pub files: Vec<(char, String)>,
}

impl CommitTarget {
    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Result of the background commit job, read by the dialog.
#[derive(Debug, Clone)]
enum Outcome {
    ChecksFailed(String),
    Committed(String),
    Failed(String),
}

type Slot = Arc<Mutex<Option<Outcome>>>;

enum State {
    Edit,
    /// Background job running; `checks` = commit checks run before the commit.
    Running {
        since: Instant,
        checks: bool,
    },
    ChecksFailed {
        output: String,
        scroll: u16,
    },
    /// Committed; `checks` = commit checks ran (and passed) first.
    Done {
        summary: String,
        checks: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Message,
    Options,
    Author,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Amend,
    SignOff,
    Gpg,
    Checks,
    Push,
}

const OPTIONS: [Opt; 5] = [Opt::Amend, Opt::SignOff, Opt::Gpg, Opt::Checks, Opt::Push];

impl Opt {
    fn label(self) -> &'static str {
        match self {
            Opt::Amend => "Amend",
            Opt::SignOff => "Sign-off",
            Opt::Gpg => "GPG sign",
            Opt::Checks => "Run checks",
            Opt::Push => "Push after commit",
        }
    }

    fn accel(self) -> char {
        match self {
            Opt::Amend => 'a',
            Opt::SignOff => 's',
            Opt::Gpg => 'g',
            Opt::Checks => 'c',
            Opt::Push => 'p',
        }
    }
}

fn draft_path(repo: &Repo) -> Option<std::path::PathBuf> {
    repo.data_dir().ok().map(|d| d.join("commit_draft.txt"))
}

fn save_draft(repo: &Repo, text: &str) {
    if let Some(p) = draft_path(repo) {
        if text.trim().is_empty() {
            let _ = std::fs::remove_file(p);
        } else {
            let _ = std::fs::write(p, text);
        }
    }
}

fn load_draft(repo: &Repo) -> String {
    draft_path(repo)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// The commit dialog popup.
pub struct CommitDialog {
    target: CommitTarget,
    title: String,
    message: TextArea,
    author: TextInput,
    focus: Focus,
    opt_sel: usize,
    amend: bool,
    sign_off: bool,
    gpg: bool,
    run_checks: bool,
    push: bool,
    has_head: bool,
    last_message: Option<String>,
    before_amend: Option<String>,
    history: Vec<String>,
    hist_idx: Option<usize>,
    hist_draft: String,
    picked: Rc<RefCell<Option<String>>>,
    checks: Vec<(String, String)>,
    subject_limit: usize,
    state: State,
    slot: Slot,
    error: Option<String>,
}

impl CommitDialog {
    /// A dialog committing `target`; `amend` / `push` preset the toggles.
    pub fn new(ctx: &Ctx, target: CommitTarget, amend: bool, push: bool) -> Self {
        let repo = &ctx.repo;
        let has_head = repo.head_commit().is_some();
        let cfg = &ctx.config.commit;
        let checks: Vec<(String, String)> = cfg
            .checks
            .iter()
            .filter(|c| c.enabled && !c.command.trim().is_empty())
            .map(|c| {
                let name = if c.name.trim().is_empty() {
                    c.command.clone()
                } else {
                    c.name.clone()
                };
                (name, c.command.clone())
            })
            .collect();
        let title = match (has_head, repo.head_branch()) {
            (false, _) => " Initial commit ".to_string(),
            (true, Some(b)) => format!(" Commit to {b} "),
            (true, None) => " Commit (detached HEAD) ".to_string(),
        };
        let mut d = Self {
            target,
            title,
            message: TextArea::new(&load_draft(repo)),
            author: TextInput::default(),
            focus: Focus::Message,
            opt_sel: 0,
            amend: false,
            sign_off: cfg.sign_off,
            gpg: commit::gpg_sign_default(repo, cfg.gpg_sign),
            run_checks: !checks.is_empty(),
            push,
            has_head,
            last_message: commit::last_message(repo),
            before_amend: None,
            history: commit::load_history(repo),
            hist_idx: None,
            hist_draft: String::new(),
            picked: Rc::new(RefCell::new(None)),
            checks,
            subject_limit: cfg.subject_limit,
            state: State::Edit,
            slot: Arc::new(Mutex::new(None)),
            error: None,
        };
        if amend {
            d.toggle(Opt::Amend);
        }
        d
    }

    fn enabled(&self, o: Opt) -> bool {
        match o {
            Opt::Amend => self.has_head,
            Opt::Checks => !self.checks.is_empty(),
            _ => true,
        }
    }

    fn value(&self, o: Opt) -> bool {
        match o {
            Opt::Amend => self.amend,
            Opt::SignOff => self.sign_off,
            Opt::Gpg => self.gpg,
            Opt::Checks => self.run_checks && !self.checks.is_empty(),
            Opt::Push => self.push,
        }
    }

    fn toggle(&mut self, o: Opt) {
        if !self.enabled(o) {
            return;
        }
        match o {
            Opt::Amend => self.toggle_amend(),
            Opt::SignOff => self.sign_off = !self.sign_off,
            Opt::Gpg => self.gpg = !self.gpg,
            Opt::Checks => self.run_checks = !self.run_checks,
            Opt::Push => self.push = !self.push,
        }
    }

    /// Enabling amend loads the last commit message; disabling restores what was there before
    /// (unless the user edited the amended message).
    fn toggle_amend(&mut self) {
        self.amend = !self.amend;
        if self.amend {
            if let Some(last) = &self.last_message {
                self.before_amend = Some(self.message.value());
                self.message.set(last);
            }
        } else if let Some(prev) = self.before_amend.take() {
            if Some(self.message.value()) == self.last_message {
                self.message.set(&prev);
            }
        }
    }

    fn history_step(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.hist_idx, older) {
            (None, true) => Some(0),
            (None, false) => return,
            (Some(i), true) => Some((i + 1).min(self.history.len() - 1)),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
        };
        if self.hist_idx.is_none() {
            self.hist_draft = self.message.value();
        }
        self.hist_idx = next;
        match next {
            Some(i) => self.message.set(&self.history[i]),
            None => {
                let d = self.hist_draft.clone();
                self.message.set(&d);
            }
        }
    }

    fn history_menu(&self) -> Action {
        if self.history.is_empty() {
            return Action::Toast("No previous commit messages yet".into());
        }
        let items = self
            .history
            .iter()
            .map(|m| {
                let picked = self.picked.clone();
                let msg = m.clone();
                let label = crate::util::truncate(commit::subject(m), 70);
                MenuItem::new(label, "", move |_| {
                    *picked.borrow_mut() = Some(msg);
                    Action::None
                })
            })
            .collect();
        Action::popup(MenuPopup::new("Commit message history", items).filterable())
    }

    /// Apply results that arrived asynchronously (history menu choice, background job outcome).
    fn poll_results(&mut self) {
        if let Some(m) = self.picked.borrow_mut().take() {
            self.message.set(&m);
            self.hist_idx = None;
        }
        if let State::Running { checks, .. } = self.state {
            let out = self.slot.lock().ok().and_then(|mut s| s.take());
            match out {
                Some(Outcome::ChecksFailed(output)) => {
                    self.state = State::ChecksFailed { output, scroll: 0 }
                }
                Some(Outcome::Committed(summary)) => self.state = State::Done { summary, checks },
                Some(Outcome::Failed(e)) => {
                    self.state = State::Edit;
                    self.error = Some(e);
                }
                None => {}
            }
        }
    }

    fn request(&self) -> Result<CommitRequest, String> {
        let message = self.message.value();
        if message.trim().is_empty() {
            return Err("Enter a commit message".into());
        }
        let author = self.author.value();
        let author = author.trim();
        if !author.is_empty() {
            commit::validate_author(author).map_err(|e| e.to_string())?;
        }
        if self.target.is_empty() && !self.amend {
            return Err("Nothing to commit: no changed files in the selection".into());
        }
        let paths = self.target.paths.clone().filter(|p| !p.is_empty());
        Ok(CommitRequest {
            message,
            amend: self.amend,
            sign_off: self.sign_off,
            gpg_sign: self.gpg,
            author: (!author.is_empty()).then(|| author.to_string()),
            untracked: if paths.is_some() {
                self.target.untracked.clone()
            } else {
                Vec::new()
            },
            paths,
        })
    }

    fn submit(&mut self, ctx: &Ctx, skip_checks: bool) -> Action {
        let req = match self.request() {
            Ok(r) => r,
            Err(e) => {
                self.state = State::Edit;
                self.error = Some(e);
                return Action::None;
            }
        };
        self.error = None;
        save_draft(&ctx.repo, &req.message);
        let with_checks = self.run_checks && !self.checks.is_empty() && !skip_checks;
        if with_checks || self.push {
            // The dialog stays open while the job runs: either to show failed checks with
            // "commit anyway", or to open the push dialog once the commit exists.
            self.state = State::Running {
                since: Instant::now(),
                checks: with_checks,
            };
            if let Ok(mut s) = self.slot.lock() {
                *s = None;
            }
            let checks = if with_checks {
                self.checks.clone()
            } else {
                Vec::new()
            };
            return commit_job(req, checks, self.push, self.slot.clone());
        }
        let slot: Slot = Arc::new(Mutex::new(None));
        Action::ClosePopup.then(commit_job(req, Vec::new(), false, slot))
    }

    fn close_editing(&self, ctx: &Ctx) -> Action {
        let msg = self.message.value();
        let keep = !msg.trim().is_empty() && Some(&msg) != self.last_message.as_ref();
        save_draft(&ctx.repo, if keep { &msg } else { "" });
        Action::ClosePopup
    }

    fn handle_edit_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('s') if ctrl => return self.submit(ctx, false),
            KeyCode::Enter if alt => return self.submit(ctx, false),
            KeyCode::Char('p') if ctrl => {
                self.history_step(true);
                return Action::None;
            }
            KeyCode::Char('n') if ctrl => {
                self.history_step(false);
                return Action::None;
            }
            KeyCode::Char('h') | KeyCode::Char('r') if ctrl => return self.history_menu(),
            KeyCode::Esc => return self.close_editing(ctx),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Message => Focus::Options,
                    Focus::Options => Focus::Author,
                    Focus::Author => Focus::Message,
                };
                return Action::None;
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Message => Focus::Author,
                    Focus::Options => Focus::Message,
                    Focus::Author => Focus::Options,
                };
                return Action::None;
            }
            _ => {}
        }
        match self.focus {
            Focus::Message => {
                if self.message.handle_key(&key) {
                    self.hist_idx = None;
                }
            }
            Focus::Author => {
                if key.code == KeyCode::Enter {
                    self.focus = Focus::Message;
                } else {
                    self.author.handle_key(&key);
                }
            }
            Focus::Options => match key.code {
                KeyCode::Left | KeyCode::Char('h') => self.opt_sel = self.opt_sel.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => {
                    self.opt_sel = (self.opt_sel + 1).min(OPTIONS.len() - 1)
                }
                KeyCode::Char(' ') | KeyCode::Enter => self.toggle(OPTIONS[self.opt_sel]),
                KeyCode::Char(c) => {
                    if let Some(o) = OPTIONS.iter().find(|o| o.accel() == c) {
                        self.toggle(*o);
                    }
                }
                _ => {}
            },
        }
        Action::None
    }

    fn draw_edit(&mut self, f: &mut Frame, inner: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let shown_files = self.target.files.len().min(4) as u16;
        let more = u16::from(self.target.files.len() > 4);
        let [what, msg, counter, opts, author, files, status, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(shown_files + more),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let count = self.target.files.len();
        let what_text = if self.target.is_empty() && self.amend {
            "Amending the last commit (message only)".to_string()
        } else {
            format!(
                "Committing {} ({count} file{})",
                self.target.label,
                if count == 1 { "" } else { "s" }
            )
        };
        f.render_widget(Line::styled(what_text, t.dimmed()), what);

        let mblock = panel(" Message ", self.focus == Focus::Message, t);
        let minner = mblock.inner(msg);
        f.render_widget(mblock, msg);
        let limit = self.subject_limit;
        let warn = Style::default().fg(t.warning);
        let normal = t.text();
        let styler = move |i: usize, s: &str| {
            if i == 0 && limit > 0 && s.chars().count() > limit {
                warn
            } else {
                normal
            }
        };
        self.message.render(
            f,
            minner,
            normal,
            matches!(self.state, State::Edit) && self.focus == Focus::Message,
            Some(&styler),
        );

        let msg_text = self.message.value();
        let subj_len = commit::subject(&msg_text).chars().count();
        let over = commit::subject_too_long(&msg_text, limit);
        let mut counter_spans = vec![Span::styled(
            format!("Subject {subj_len}/{limit}"),
            if over { warn } else { t.dimmed() },
        )];
        if over {
            counter_spans.push(Span::styled(
                "  subject is longer than the configured limit",
                warn,
            ));
        }
        if let Some(i) = self.hist_idx {
            counter_spans.push(Span::styled(
                format!("   history {}/{}", i + 1, self.history.len()),
                t.dimmed(),
            ));
        }
        f.render_widget(Line::from(counter_spans), counter);

        let mut spans = Vec::new();
        for (i, o) in OPTIONS.iter().enumerate() {
            let mark = if self.value(*o) { "[x]" } else { "[ ]" };
            let mut style = if self.enabled(*o) {
                t.text()
            } else {
                t.dimmed()
            };
            if self.focus == Focus::Options && i == self.opt_sel {
                style = style.patch(t.selected());
            }
            let label = if *o == Opt::Checks && !self.checks.is_empty() {
                format!("{mark} {} ({})", o.label(), self.checks.len())
            } else {
                format!("{mark} {}", o.label())
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw("  "));
        }
        f.render_widget(Line::from(spans), opts);

        let [alabel, ainput] =
            Layout::horizontal([Constraint::Length(8), Constraint::Min(1)]).areas(author);
        let afocus = self.focus == Focus::Author;
        f.render_widget(
            Span::styled("Author: ", if afocus { t.title() } else { t.dimmed() }),
            alabel,
        );
        if self.author.is_empty() && !afocus {
            f.render_widget(
                Span::styled("(default)  Name <email> to override", t.dimmed()),
                ainput,
            );
        } else {
            self.author.render(f, ainput, t.text(), afocus);
        }

        let mut flines: Vec<Line> = self
            .target
            .files
            .iter()
            .take(4)
            .map(|(c, p)| {
                Line::from(vec![
                    Span::styled(format!("  {c} "), Style::default().fg(t.modified)),
                    Span::styled(p.clone(), t.text()),
                ])
            })
            .collect();
        if more > 0 {
            flines.push(Line::styled(
                format!("  … and {} more", self.target.files.len() - 4),
                t.dimmed(),
            ));
        }
        f.render_widget(Paragraph::new(flines), files);

        let status_line = match (&self.state, &self.error) {
            (State::Running { since, checks }, _) => {
                let frame = crate::app::spinner_frame(*since);
                let what = if *checks {
                    "Running commit checks…"
                } else {
                    "Committing…"
                };
                Line::styled(format!("{frame} {what}"), Style::default().fg(t.accent))
            }
            (_, Some(e)) => Line::styled(
                e.lines().next().unwrap_or("").to_string(),
                Style::default().fg(t.error),
            ),
            _ => Line::raw(""),
        };
        f.render_widget(status_line, status);

        let hints: &[(&str, &str)] = if matches!(self.state, State::Running { .. }) {
            &[("Esc", "hide (the commit keeps running)")]
        } else {
            &[
                ("Ctrl+S", "commit"),
                ("Tab", "next field"),
                ("Ctrl+P/N", "history"),
                ("Ctrl+H", "history list"),
                ("Esc", "cancel"),
            ]
        };
        f.render_widget(hint_line(hints, ctx), keys);
    }
}

fn hint_line(hints: &[(&str, &str)], ctx: &Ctx) -> Line<'static> {
    let t = &ctx.theme;
    let mut spans = Vec::new();
    for (k, d) in hints {
        spans.push(Span::styled(k.to_string(), t.key()));
        spans.push(Span::styled(format!(" {d}   "), t.dimmed()));
    }
    Line::from(spans)
}

/// Background job: optional checks, then the commit. The outcome goes to `slot` for the dialog;
/// when the dialog is gone, failures are reported in full through the task result, and a
/// requested push that can no longer be started (the dialog opens it) is mentioned there too.
fn commit_job(req: CommitRequest, checks: Vec<(String, String)>, push: bool, slot: Slot) -> Action {
    Action::spawn("Commit", move |repo| {
        let dialog_alive = || Arc::strong_count(&slot) > 1;
        let put = |o: Outcome| {
            if let Ok(mut s) = slot.lock() {
                *s = Some(o);
            }
        };
        if !checks.is_empty() {
            let report = commit::run_checks(repo, &checks);
            if !report.passed {
                let alive = dialog_alive();
                put(Outcome::ChecksFailed(report.output.clone()));
                if alive {
                    anyhow::bail!("commit checks failed");
                }
                anyhow::bail!("commit checks failed:\n{}", report.output);
            }
        }
        match commit::commit(repo, &req) {
            Ok(summary) => {
                save_draft(repo, "");
                let alive = dialog_alive();
                put(Outcome::Committed(summary.clone()));
                Ok(committed_message(summary, push, alive))
            }
            Err(e) => {
                put(Outcome::Failed(format!("{e:#}")));
                Err(e)
            }
        }
    })
}

/// The task-result text after a successful commit.
fn committed_message(summary: String, push: bool, dialog_alive: bool) -> String {
    if push && !dialog_alive {
        format!("{summary}; push not started (the commit dialog was closed)")
    } else {
        summary
    }
}

impl Popup for CommitDialog {
    /// Close by itself once the background commit succeeds (opening the push dialog if requested).
    fn poll(&mut self, ctx: &Ctx) -> Action {
        self.poll_results();
        if let State::Done { summary, .. } = &self.state {
            let done = Action::ClosePopup.then(Action::Toast(summary.clone()));
            return if self.push {
                done.then(crate::ui::views::push::open(ctx))
            } else {
                done
            };
        }
        Action::None
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.poll_results();
        let t = &ctx.theme;
        let r = centered(area, 100, 24);
        f.render_widget(Clear, r);
        let title = if self.amend {
            " Amend last commit ".to_string()
        } else {
            self.title.clone()
        };
        let block = crate::ui::widgets::dialog(title, t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        if matches!(self.state, State::Edit | State::Running { .. }) {
            self.draw_edit(f, inner, ctx);
            return;
        }
        match &mut self.state {
            State::Edit | State::Running { .. } => {}
            State::ChecksFailed { output, scroll } => {
                let [head, body, keys] = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .areas(inner);
                f.render_widget(
                    Line::styled("Commit checks failed", Style::default().fg(t.error)),
                    head,
                );
                let lines = output.lines().count() as u16;
                *scroll = (*scroll).min(lines.saturating_sub(body.height));
                f.render_widget(
                    Paragraph::new(output.clone())
                        .style(t.text())
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0)),
                    body,
                );
                f.render_widget(
                    hint_line(
                        &[
                            ("c", "commit anyway"),
                            ("e/Esc", "back to message"),
                            ("↑↓", "scroll"),
                        ],
                        ctx,
                    ),
                    keys,
                );
            }
            State::Done { summary, checks } => {
                let next = if self.push {
                    "open the push dialog"
                } else {
                    "close"
                };
                let head = if *checks {
                    "✓ All commit checks passed"
                } else {
                    "✓ Committed"
                };
                let text = vec![
                    Line::styled(head, Style::default().fg(t.success)),
                    Line::styled(summary.clone(), t.text()),
                    Line::raw(""),
                    Line::styled(format!("Press any key to {next}"), t.dimmed()),
                ];
                f.render_widget(Paragraph::new(text), inner);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        self.poll_results();
        if matches!(self.state, State::Edit) {
            return self.handle_edit_key(key, ctx);
        }
        if matches!(self.state, State::ChecksFailed { .. }) && key.code == KeyCode::Char('c') {
            return self.submit(ctx, true);
        }
        match &mut self.state {
            State::Edit => Action::None,
            State::Running { .. } => match key.code {
                KeyCode::Esc => Action::ClosePopup,
                _ => Action::None,
            },
            State::ChecksFailed { scroll, .. } => match key.code {
                KeyCode::Char('e') | KeyCode::Esc => {
                    self.state = State::Edit;
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(1);
                    Action::None
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(10);
                    Action::None
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    *scroll = scroll.saturating_add(10);
                    Action::None
                }
                _ => Action::None,
            },
            State::Done { .. } => {
                if self.push {
                    Action::ClosePopup.then(crate::ui::views::push::open(ctx))
                } else {
                    Action::ClosePopup
                }
            }
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        match self.state {
            State::Edit => vec![
                ("Ctrl+S", "commit"),
                ("Tab", "message/options/author"),
                ("Space", "toggle option"),
                ("Ctrl+P/N", "prev/next message"),
                ("Ctrl+H", "message history"),
                ("Esc", "cancel"),
            ],
            State::Running { .. } => vec![("Esc", "hide")],
            State::ChecksFailed { .. } => {
                vec![("c", "commit anyway"), ("e/Esc", "back"), ("↑↓", "scroll")]
            }
            State::Done { .. } => vec![("any key", "close")],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;

    fn ctx_for(repo: Repo) -> Ctx {
        Ctx::new(repo, Config::default())
    }

    fn key(c: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(c, m)
    }

    #[test]
    fn amend_prefills_and_restores() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a", "1\n")], "previous message");
        let ctx = ctx_for(repo);
        let mut d = CommitDialog::new(&ctx, CommitTarget::default(), false, false);
        d.message.set("draft");
        d.toggle(Opt::Amend);
        assert_eq!(d.message.value(), "previous message");
        d.toggle(Opt::Amend);
        assert_eq!(d.message.value(), "draft");
        // Empty target without amend is refused; with amend it's a message-only amend.
        assert!(d.request().is_err());
        d.toggle(Opt::Amend);
        let r = d.request().unwrap();
        assert!(r.amend && r.paths.is_none());
    }

    #[test]
    fn history_navigation_and_author_validation() {
        let (_d, repo) = temp_repo();
        commit::remember_message(&repo, "older").unwrap();
        commit::remember_message(&repo, "newer").unwrap();
        let ctx = ctx_for(repo);
        let target = CommitTarget {
            label: "staged changes".into(),
            files: vec![('M', "a".into())],
            ..Default::default()
        };
        let mut d = CommitDialog::new(&ctx, target, false, false);
        d.message.set("typing");
        d.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL), &ctx);
        assert_eq!(d.message.value(), "newer");
        d.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL), &ctx);
        assert_eq!(d.message.value(), "older");
        d.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL), &ctx);
        d.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL), &ctx);
        assert_eq!(d.message.value(), "typing");
        d.author.set("bad author");
        assert!(d.request().is_err());
        d.author.set("Good Name <g@example.com>");
        assert_eq!(
            d.request().unwrap().author.as_deref(),
            Some("Good Name <g@example.com>")
        );
        // A history pick from the menu is applied on the next poll.
        *d.picked.borrow_mut() = Some("picked".into());
        d.poll_results();
        assert_eq!(d.message.value(), "picked");
    }

    #[test]
    fn commit_and_push_runs_in_background() {
        let (_d, repo) = temp_repo();
        write(&repo, "a.txt", "1\n");
        let ctx = ctx_for(repo);
        let target = CommitTarget {
            label: "files".into(),
            paths: Some(vec!["a.txt".into()]),
            untracked: vec!["a.txt".into()],
            files: vec![('?', "a.txt".into())],
        };
        let mut d = CommitDialog::new(&ctx, target, false, true);
        d.message.set("first");
        let action = d.submit(&ctx, false);
        assert!(matches!(d.state, State::Running { checks: false, .. }));
        assert!(
            ctx.repo.head_commit().is_none(),
            "nothing committed on the UI thread"
        );
        let Action::Spawn(task) = action else {
            panic!("expected a background task")
        };
        let msg = (task.job)(&ctx.repo).unwrap();
        assert!(msg.starts_with("Committed "), "{msg}");
        assert!(ctx.repo.head_commit().is_some());
        d.poll_results();
        assert!(matches!(d.state, State::Done { checks: false, .. }));
    }

    #[test]
    fn hidden_dialog_reports_dropped_push() {
        assert_eq!(
            committed_message("Committed x".into(), false, false),
            "Committed x"
        );
        assert_eq!(
            committed_message("Committed x".into(), true, true),
            "Committed x"
        );
        assert!(committed_message("Committed x".into(), true, false).contains("push not started"));
    }

    #[test]
    fn options_toggle_from_keyboard() {
        let (_d, repo) = temp_repo();
        let ctx = ctx_for(repo);
        let mut d = CommitDialog::new(&ctx, CommitTarget::default(), false, true);
        assert!(d.push);
        assert!(!d.enabled(Opt::Amend), "no amend in an empty repository");
        d.handle_key(key(KeyCode::Tab, KeyModifiers::NONE), &ctx);
        d.handle_key(key(KeyCode::Char('s'), KeyModifiers::NONE), &ctx);
        assert!(d.sign_off);
        d.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE), &ctx);
        assert!(!d.amend);
    }
}
