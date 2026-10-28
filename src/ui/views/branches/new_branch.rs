//! "New branch" dialog: name field plus "Checkout branch" / "Overwrite existing branch" checkboxes.
//!
//! Creating runs in the background (checking out a branch rewrites the work tree, which can take
//! seconds); the dialog shows each step with a spinner and keeps errors inline.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::branch::{self, Branch};
use crate::git::Repo;
use crate::ui::popups::ConfirmPopup;
use crate::ui::widgets::{centered, panel, TextInput};
use crate::ui::{KeyHelp, Popup};

use super::ops::done;

/// Focusable parts of the dialog, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Checkout,
    Overwrite,
}

impl Field {
    fn next(self) -> Field {
        match self {
            Field::Name => Field::Checkout,
            Field::Checkout => Field::Overwrite,
            Field::Overwrite => Field::Name,
        }
    }

    fn prev(self) -> Field {
        match self {
            Field::Name => Field::Overwrite,
            Field::Checkout => Field::Name,
            Field::Overwrite => Field::Checkout,
        }
    }
}

pub struct NewBranchPopup {
    /// Start point (empty = HEAD)
    start: String,
    /// Set when `start` is a remote branch: the new branch does not track it (see
    /// [`branch::create_branch_with`]), which the dialog explains.
    from_remote: bool,
    input: TextInput,
    checkout: bool,
    overwrite: bool,
    focus: Field,
    error: Option<String>,
    /// The create running in the background.
    job: Option<Job>,
    /// Request waiting for the overwrite confirmation, and the flag the confirmation sets.
    awaiting: Option<CreateRequest>,
    confirmed: Rc<Cell<bool>>,
}

/// What the background create reports back, in order.
enum Progress {
    /// The step now running, shown in the dialog.
    Step(String),
    /// Overwriting would leave `lost` commits (old tip) reachable only through the reflog: ask first.
    WouldLose(u32, String),
    /// Finished: the success message or the inline error.
    Done(Result<String, String>),
}

struct Job {
    rx: Receiver<Progress>,
    step: String,
    started: Instant,
    req: CreateRequest,
}

impl NewBranchPopup {
    /// Start at `start` (None = HEAD). For remote branches the name is prefilled with the short name.
    pub fn new(start: Option<&Branch>) -> Self {
        let initial = start
            .filter(|b| b.is_remote())
            .map(|b| b.short_name().to_string())
            .unwrap_or_default();
        Self {
            start: start.map(|b| b.name.clone()).unwrap_or_default(),
            from_remote: start.is_some_and(Branch::is_remote),
            input: TextInput::new(&initial),
            checkout: true,
            overwrite: false,
            focus: Field::Name,
            error: None,
            job: None,
            awaiting: None,
            confirmed: Rc::new(Cell::new(false)),
        }
    }

    /// Validate, then create the branch in the background; overwriting a branch that would lose
    /// commits asks first.
    fn create(&mut self, ctx: &Ctx) -> Action {
        let name = self.input.value().trim().to_string();
        if name.is_empty() {
            self.error = Some("Enter a branch name".into());
            return Action::None;
        }
        let req = CreateRequest {
            name,
            start: self.start.clone(),
            checkout: self.checkout,
            overwrite: self.overwrite,
        };
        self.start_job(ctx, req, true)
    }

    /// Start creating `req`; `check_lost` first looks for commits an overwrite would drop.
    fn start_job(&mut self, ctx: &Ctx, req: CreateRequest, check_lost: bool) -> Action {
        self.error = None;
        self.awaiting = None;
        self.confirmed.set(false);
        let (tx, rx) = mpsc::channel();
        let repo = ctx.repo.clone();
        let job_req = req.clone();
        let work = move || job_req.run(&repo, check_lost, &tx);
        if cfg!(test) {
            work();
        } else {
            std::thread::spawn(work);
        }
        self.job = Some(Job {
            rx,
            step: "Starting…".into(),
            started: Instant::now(),
            req,
        });
        self.poll(ctx)
    }

    fn toggle(&mut self) {
        match self.focus {
            Field::Checkout => self.checkout = !self.checkout,
            Field::Overwrite => self.overwrite = !self.overwrite,
            Field::Name => {}
        }
    }
}

/// A validated "create branch" request.
#[derive(Debug, Clone)]
struct CreateRequest {
    name: String,
    /// Start point (empty = HEAD)
    start: String,
    checkout: bool,
    overwrite: bool,
}

impl CreateRequest {
    /// Runs on the background thread, reporting each step on `tx`.
    fn run(&self, repo: &Repo, check_lost: bool, tx: &Sender<Progress>) {
        let step = |s: String| {
            let _ = tx.send(Progress::Step(s));
        };
        let name = &self.name;
        if check_lost && self.overwrite {
            step(format!("Checking for commits only on {name}…"));
            if let Some((lost, tip)) = branch::commits_lost_by_reset(repo, name, &self.start) {
                if lost > 0 {
                    let _ = tx.send(Progress::WouldLose(lost, tip));
                    return;
                }
            }
        }
        step(match (self.checkout, self.overwrite) {
            (true, true) => format!("Resetting {name} and switching to it (updating files)…"),
            (true, false) => format!("Creating {name} and switching to it (updating files)…"),
            (false, true) => format!("Resetting {name}…"),
            (false, false) => format!("Creating {name}…"),
        });
        let result =
            branch::create_branch_with(repo, name, &self.start, self.checkout, self.overwrite)
                .map(|()| self.success_message())
                .map_err(|e| {
                    let msg = format!("{e:#}");
                    if msg.contains("already exists") {
                        format!("{msg} — tick \"Overwrite existing branch\" to reset it")
                    } else {
                        msg
                    }
                });
        let _ = tx.send(Progress::Done(result));
    }

    fn success_message(&self) -> String {
        let name = &self.name;
        let from = if self.start.is_empty() {
            String::new()
        } else {
            format!(" from {}", self.start)
        };
        let what = match (self.checkout, self.overwrite) {
            (true, true) => "Reset and checked out",
            (false, true) => "Reset",
            (true, false) => "Created and checked out",
            (false, false) => "Created",
        };
        format!("{what} branch {name}{from}")
    }
}

/// Danger confirm before resetting an existing branch whose `lost` commits would become unreachable.
/// Confirming sets `confirmed`; the dialog underneath then creates the branch (and closes on success,
/// or shows the error).
fn overwrite_confirm(
    req: &CreateRequest,
    lost: u32,
    tip: &str,
    confirmed: Rc<Cell<bool>>,
) -> Action {
    let short: String = tip.chars().take(8).collect();
    let start = if req.start.is_empty() {
        "HEAD"
    } else {
        req.start.as_str()
    };
    let msg = format!(
        "Branch '{}' already exists and has {} that {} not in {start}.\n\
         Overwriting resets it to {start}; those commits will only be reachable via the reflog\n\
         (old tip {short}, restore with: git branch -f {} {short}).\n\nOverwrite it?",
        req.name,
        branch::commits(lost),
        if lost == 1 { "is" } else { "are" },
        req.name,
    );
    Action::popup(
        ConfirmPopup::new("Overwrite existing branch", msg, move |_| {
            confirmed.set(true);
            Action::None
        })
        .danger(),
    )
}

fn checkbox(on: bool, label: &str, focused: bool, ctx: &Ctx, danger: bool) -> Line<'static> {
    let t = &ctx.theme;
    let mark = if on { "[x] " } else { "[ ] " };
    let mut style = if danger && on {
        Style::default().fg(t.warning)
    } else {
        t.text()
    };
    if focused {
        style = style.patch(t.selected());
    }
    Line::from(vec![Span::styled(format!("{mark}{label}"), style)])
}

impl Popup for NewBranchPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered(area, 72, 11);
        f.render_widget(Clear, r);
        let from = if self.start.is_empty() {
            "HEAD"
        } else {
            self.start.as_str()
        };
        let block = panel(format!(" New branch from {from} "), true, t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [label, field, _, cb1, cb2, err, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(Span::styled("Branch name:", t.dimmed()), label);
        let field_style = if self.focus == Field::Name {
            t.text()
        } else {
            t.dimmed()
        };
        self.input
            .render(f, field, field_style, self.focus == Field::Name);
        f.render_widget(
            checkbox(
                self.checkout,
                "Checkout branch",
                self.focus == Field::Checkout,
                ctx,
                false,
            ),
            cb1,
        );
        f.render_widget(
            checkbox(
                self.overwrite,
                "Overwrite existing branch",
                self.focus == Field::Overwrite,
                ctx,
                true,
            ),
            cb2,
        );
        if let Some(job) = &self.job {
            let frame = (job.started.elapsed().as_millis() / 100) as usize;
            let secs = job.started.elapsed().as_secs();
            let mut text = format!(
                "{} {}",
                crate::app::SPINNER[frame % crate::app::SPINNER.len()],
                job.step
            );
            if secs >= 1 {
                text.push_str(&format!(" {secs}s"));
            }
            f.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(t.accent))
                    .wrap(Wrap { trim: true }),
                err,
            );
            f.render_widget(Span::styled("Working… please wait", t.dimmed()), keys);
            return;
        }
        if let Some(e) = &self.error {
            f.render_widget(
                Paragraph::new(e.as_str())
                    .style(Style::default().fg(t.error))
                    .wrap(Wrap { trim: true }),
                err,
            );
        } else if self.from_remote {
            f.render_widget(
                Paragraph::new(format!(
                    "Won't track {}: the first push publishes it under its own name.",
                    self.start
                ))
                .style(t.dimmed())
                .wrap(Wrap { trim: true }),
                err,
            );
        }
        f.render_widget(
            Line::from(vec![
                Span::styled("Enter", t.key()),
                Span::styled(" create   ", t.dimmed()),
                Span::styled("Tab", t.key()),
                Span::styled(" next field   ", t.dimmed()),
                Span::styled("Space", t.key()),
                Span::styled(" toggle   ", t.dimmed()),
                Span::styled("Esc", t.key()),
                Span::styled(" cancel", t.dimmed()),
            ]),
            keys,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.job.is_some() {
            // git is already running; there is nothing safe to cancel.
            return Action::None;
        }
        match key.code {
            KeyCode::Esc => return Action::ClosePopup,
            KeyCode::Enter => return self.create(ctx),
            KeyCode::Tab | KeyCode::Down => self.focus = self.focus.next(),
            KeyCode::BackTab | KeyCode::Up => self.focus = self.focus.prev(),
            KeyCode::Char(' ') if self.focus != Field::Name => self.toggle(),
            _ if self.focus == Field::Name && self.input.handle_key(&key) => self.error = None,
            _ => {}
        }
        Action::None
    }

    /// Collect progress from the background create; after a confirmed overwrite, start it.
    fn poll(&mut self, ctx: &Ctx) -> Action {
        if self.confirmed.take() {
            if let Some(req) = self.awaiting.take() {
                return self.start_job(ctx, req, false);
            }
        }
        let Some(job) = &mut self.job else {
            return Action::None;
        };
        while let Ok(p) = job.rx.try_recv() {
            match p {
                Progress::Step(s) => job.step = s,
                Progress::WouldLose(lost, tip) => {
                    // The dialog is idle again while the confirmation is open; declining just
                    // closes the confirmation.
                    let req = self.job.take().map(|j| j.req).expect("job is running");
                    let a = overwrite_confirm(&req, lost, &tip, self.confirmed.clone());
                    self.awaiting = Some(req);
                    return a;
                }
                Progress::Done(Ok(msg)) => {
                    self.job = None;
                    return Action::ClosePopup.then(done(msg));
                }
                Progress::Done(Err(e)) => {
                    self.job = None;
                    self.error = Some(e);
                    return Action::None;
                }
            }
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        if self.job.is_some() {
            return Vec::new();
        }
        vec![
            ("Enter", "create"),
            ("Tab", "next field"),
            ("Space", "toggle"),
            ("Esc", "cancel"),
        ]
    }
}
