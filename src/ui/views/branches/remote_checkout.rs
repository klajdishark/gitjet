//! "Check out from the remote" (`[branches] checkout_from_remote`): fetch the branch, then create or
//! reset the local branch to the remote one. Runs in the background; the dialog lists each step as
//! it happens, and unpushed local commits are never dropped without asking.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::branch::{self, Branch, CheckoutTarget, RemoteSource};
use crate::git::Repo;
use crate::ui::popups::{MenuItem, MenuPopup};
use crate::ui::widgets::{centered, dialog};
use crate::ui::{KeyHelp, Popup};

use super::ops::{done, local_changes_menu};

/// Open the progress dialog that checks out `b` from its remote.
pub fn open(ctx: &Ctx, b: &Branch) -> Action {
    Action::popup(RemoteCheckoutPopup::new(ctx, b.clone(), Mode::Check))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Fetch, then ask before dropping local commits.
    Check,
    /// The user chose to reset: no fetch (it just ran), no question.
    Reset,
    /// The user chose to keep the local branch as it is.
    Local,
}

/// What the background job reports back, in order.
enum Progress {
    Step(String),
    /// Something the user should know that does not stop the checkout (e.g. fetch failed).
    Note(String),
    /// The local branch has `lost` commits not on the remote (local tip `tip`): ask.
    WouldLose {
        src: RemoteSource,
        lost: u32,
        tip: String,
    },
    /// Local changes are in the way: offer smart / force checkout.
    LocalChanges(CheckoutTarget, String),
    Done(Result<String, String>),
}

pub struct RemoteCheckoutPopup {
    title: String,
    branch: Branch,
    /// Finished steps, then the one running.
    steps: Vec<String>,
    notes: Vec<String>,
    rx: Receiver<Progress>,
    started: Instant,
}

impl RemoteCheckoutPopup {
    fn new(ctx: &Ctx, branch: Branch, mode: Mode) -> Self {
        let (tx, rx) = mpsc::channel();
        let repo = ctx.repo.clone();
        let b = branch.clone();
        let work = move || run(&repo, &b, mode, &tx);
        if cfg!(test) {
            work();
        } else {
            std::thread::spawn(work);
        }
        let name = if branch.is_remote() {
            branch.short_name().to_string()
        } else {
            branch.name.clone()
        };
        Self {
            title: format!(" Checkout {name} "),
            branch,
            steps: vec!["Starting…".into()],
            notes: Vec::new(),
            rx,
            started: Instant::now(),
        }
    }

    /// `msg` plus the notes (e.g. "fetch failed, used the last fetched state").
    fn with_notes(&self, msg: String) -> String {
        if self.notes.is_empty() {
            msg
        } else {
            format!("{msg} ({})", self.notes.join("; "))
        }
    }
}

/// The background job.
fn run(repo: &Repo, b: &Branch, mode: Mode, tx: &Sender<Progress>) {
    let send = |p: Progress| {
        let _ = tx.send(p);
    };
    let step = |s: String| send(Progress::Step(s));
    if mode == Mode::Local {
        step(format!("Switching to {} (updating files)…", b.name));
        return finish(repo, CheckoutTarget::Existing(b.name.clone()), tx);
    }
    step("Looking up the remote branch…".into());
    let all = branch::list_branches(repo).unwrap_or_default();
    let Some(src) = branch::remote_source(repo, b, &all) else {
        // Exists only locally: nothing to take from a remote.
        step(format!("Switching to {} (updating files)…", b.name));
        return finish(repo, CheckoutTarget::Existing(b.name.clone()), tx);
    };
    let remote_ref = src.remote_ref();
    if mode == Mode::Check {
        step(format!("Fetching {remote_ref}…"));
        if let Err(e) = branch::fetch_one(repo, &src) {
            let msg = format!("{e:#}");
            if msg.contains("couldn't find remote ref") {
                if branch::local_exists(repo, &src.local) {
                    send(Progress::Note(format!(
                        "{remote_ref} no longer exists on {}; checked out the local branch",
                        src.remote
                    )));
                    step(format!("Switching to {} (updating files)…", src.local));
                    return finish(repo, CheckoutTarget::Existing(src.local.clone()), tx);
                }
                return send(Progress::Done(Err(format!(
                    "{remote_ref} no longer exists on {}",
                    src.remote
                ))));
            }
            let first = msg.lines().next().unwrap_or("").to_string();
            send(Progress::Note(format!(
                "could not fetch, used {remote_ref} as last fetched: {first}"
            )));
        }
        step(format!("Comparing {} with {remote_ref}…", src.local));
        if let Some((lost, tip)) = branch::commits_only_local(repo, &src.local, &remote_ref) {
            if lost > 0 {
                return send(Progress::WouldLose { src, lost, tip });
            }
        }
    }
    step(format!(
        "Switching to {} at {remote_ref} (updating files)…",
        src.local
    ));
    finish(
        repo,
        CheckoutTarget::FromRemote {
            local: src.local.clone(),
            remote_ref,
        },
        tx,
    );
}

/// Run the checkout and report how it went.
fn finish(repo: &Repo, target: CheckoutTarget, tx: &Sender<Progress>) {
    let p = match target.run(repo, false) {
        Ok(()) => Progress::Done(Ok(match &target {
            CheckoutTarget::FromRemote { local, remote_ref } => {
                format!("Checked out {local} from {remote_ref}")
            }
            t => format!("Checked out {}", t.label()),
        })),
        Err(e) => {
            let msg = format!("{e:#}");
            if branch::is_local_changes_error(&msg) {
                Progress::LocalChanges(target, msg)
            } else {
                Progress::Done(Err(msg))
            }
        }
    };
    let _ = tx.send(p);
}

/// Unpushed commits on the local branch: reset it to the remote, keep it, or cancel.
fn lost_commits_menu(b: &Branch, src: &RemoteSource, lost: u32, tip: &str) -> Action {
    let short: String = tip.chars().take(8).collect();
    let remote_ref = src.remote_ref();
    let local = &src.local;
    let (reset, keep) = (b.clone(), b.clone());
    let items = vec![
        MenuItem::new(
            format!(
                "Reset {local} to {remote_ref} (drops {}; restore with: git branch -f {local} {short})",
                branch::commits(lost)
            ),
            "r",
            move |ctx| Action::popup(RemoteCheckoutPopup::new(ctx, reset, Mode::Reset)),
        ),
        MenuItem::new(
            format!("Check out local {local} as it is (keep the commits)"),
            "l",
            move |ctx| Action::popup(RemoteCheckoutPopup::new(ctx, keep, Mode::Local)),
        ),
        MenuItem::new("Cancel", "c", |_| Action::None),
    ];
    Action::popup(MenuPopup::new(
        format!("{local} has {} not on {remote_ref}", branch::commits(lost)),
        items,
    ))
}

impl Popup for RemoteCheckoutPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let height = (self.steps.len() + self.notes.len()) as u16 + 2;
        let r = centered(area, 76, height);
        f.render_widget(Clear, r);
        let block = dialog(self.title.clone(), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let elapsed = self.started.elapsed();
        let frame = (elapsed.as_millis() / 100) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let last = self.steps.len().saturating_sub(1);
        for (i, s) in self.steps.iter().enumerate() {
            if i < last {
                lines.push(Line::styled(format!("✓ {s}"), t.dimmed()));
            } else {
                let mut text = format!(
                    "{} {s}",
                    crate::app::SPINNER[frame % crate::app::SPINNER.len()]
                );
                if elapsed.as_secs() >= 1 {
                    text.push_str(&format!(" {}s", elapsed.as_secs()));
                }
                lines.push(Line::styled(text, Style::default().fg(t.accent)));
            }
        }
        for n in &self.notes {
            lines.push(Line::styled(
                format!("! {n}"),
                Style::default().fg(t.warning),
            ));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    fn handle_key(&mut self, _key: KeyEvent, _ctx: &Ctx) -> Action {
        // git is running; there is nothing safe to cancel.
        Action::None
    }

    fn poll(&mut self, _ctx: &Ctx) -> Action {
        while let Ok(p) = self.rx.try_recv() {
            match p {
                Progress::Step(s) => {
                    if self.steps.last().is_some_and(|l| l == "Starting…") {
                        self.steps.pop();
                    }
                    self.steps.push(s);
                }
                Progress::Note(n) => self.notes.push(n),
                Progress::WouldLose { src, lost, tip } => {
                    return Action::ClosePopup.then(lost_commits_menu(
                        &self.branch,
                        &src,
                        lost,
                        &tip,
                    ));
                }
                Progress::LocalChanges(target, msg) => {
                    return Action::ClosePopup.then(local_changes_menu(target, msg));
                }
                Progress::Done(Ok(msg)) => {
                    return Action::ClosePopup.then(done(self.with_notes(msg)));
                }
                Progress::Done(Err(e)) => {
                    return Action::ClosePopup.then(Action::Batch(vec![
                        Action::Error(self.with_notes(e)),
                        Action::Refresh,
                    ]));
                }
            }
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("", "working… please wait")]
    }
}
