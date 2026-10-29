//! What views and popups ask the app to do after handling input.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::git::Repo;
use crate::ui::views::Tab;
use crate::ui::Popup;

pub type Job = Box<dyn FnOnce(&Repo) -> anyhow::Result<String> + Send>;

/// Runs on the UI thread after a [`Task`] finishes and may return a follow-up action
/// (switch tab, open a popup, open another repository...).
pub type Then = Box<dyn FnOnce(&anyhow::Result<String>) -> Action>;

/// A unit of work run on a background thread (fetch, push, pull...).
/// On success a non-empty message is shown as a toast; either way all views refresh.
pub struct Task {
    pub label: String,
    pub job: Job,
    /// Silent tasks show no spinner/toast and do not trigger a refresh (e.g. local-history snapshots).
    pub silent: bool,
    /// Toast the result when done; off when `then` presents the outcome itself.
    pub report: bool,
    /// Follow-up applied after the result toast and refresh.
    pub then: Option<Then>,
}

pub enum Action {
    None,
    /// Reload every view (lazily for inactive tabs) and open popups.
    Refresh,
    SwitchTab(Tab),
    /// Open a popup on top of everything. Fullscreen dialogs are popups too.
    Popup(Box<dyn Popup>),
    /// Close the top-most popup.
    ClosePopup,
    /// Short info message in the status bar.
    Toast(String),
    /// Error message: a toast when short, a scrollable dialog when long or multi-line.
    Error(String),
    /// Run work in the background with a spinner.
    Spawn(Task),
    /// Open a file (optionally at a 1-based line) in the built-in editor, or in the configured
    /// external editor like `Suspend` when `builtin_editor` is off.
    Edit {
        path: PathBuf,
        line: Option<usize>,
    },
    /// Leave the TUI, run an interactive command (editor, ssh prompt...), then come back and refresh.
    Suspend(Command),
    /// Open another repository (worktree, submodule, nested repo, recent repo).
    SwitchRepo(PathBuf),
    /// Several actions, applied in order.
    Batch(Vec<Action>),
}

impl Action {
    pub fn edit(path: impl Into<PathBuf>, line: Option<usize>) -> Self {
        Action::Edit {
            path: path.into(),
            line,
        }
    }

    pub fn popup(p: impl Popup + 'static) -> Self {
        Action::Popup(Box::new(p))
    }

    pub fn spawn(
        label: impl Into<String>,
        job: impl FnOnce(&Repo) -> anyhow::Result<String> + Send + 'static,
    ) -> Self {
        Action::Spawn(Task {
            label: label.into(),
            job: Box::new(job),
            silent: false,
            report: true,
            then: None,
        })
    }

    /// Like [`spawn`](Self::spawn), then applies `then(&result)` on the UI thread.
    pub fn spawn_then(
        label: impl Into<String>,
        job: impl FnOnce(&Repo) -> anyhow::Result<String> + Send + 'static,
        then: impl FnOnce(&anyhow::Result<String>) -> Action + 'static,
    ) -> Self {
        Action::Spawn(Task {
            label: label.into(),
            job: Box::new(job),
            silent: false,
            report: true,
            then: Some(Box::new(then)),
        })
    }

    /// Run `job` off the UI thread (spinner labelled `label` in the status bar), then turn its
    /// result into an action on the UI thread: a toast, a follow-up popup, a tab switch...
    /// In test builds the job runs inline on `repo`.
    pub fn run<T: Send + 'static>(
        repo: &Repo,
        label: impl Into<String>,
        job: impl FnOnce(&Repo) -> T + Send + 'static,
        then: impl FnOnce(T) -> Action + 'static,
    ) -> Self {
        if cfg!(test) {
            return then(job(repo));
        }
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        Action::Spawn(Task {
            label: label.into(),
            job: Box::new(move |repo| {
                let value = job(repo);
                if let Ok(mut s) = out.lock() {
                    *s = Some(value);
                }
                Ok(String::new())
            }),
            silent: false,
            report: false,
            then: Some(Box::new(move |_| {
                match slot.lock().ok().and_then(|mut s| s.take()) {
                    Some(v) => then(v),
                    None => Action::None,
                }
            })),
        })
    }

    /// [`run`](Self::run) for a plain git operation: toast `ok_msg` on success, error otherwise.
    pub fn run_op<T: Send + 'static>(
        repo: &Repo,
        label: impl Into<String>,
        ok_msg: impl Into<String>,
        job: impl FnOnce(&Repo) -> anyhow::Result<T> + Send + 'static,
    ) -> Self {
        let ok_msg = ok_msg.into();
        Action::run(repo, label, job, move |r| Action::from_result(r, ok_msg))
    }

    /// Ok → toast `ok_msg` + refresh; Err → error + refresh.
    pub fn from_result<T>(r: anyhow::Result<T>, ok_msg: impl Into<String>) -> Self {
        match r {
            Ok(_) => Action::Batch(vec![Action::Toast(ok_msg.into()), Action::Refresh]),
            Err(e) => Action::Batch(vec![Action::Error(format!("{e:#}")), Action::Refresh]),
        }
    }

    pub fn error(e: impl std::fmt::Display) -> Self {
        Action::Error(e.to_string())
    }

    pub fn then(self, next: Action) -> Action {
        match self {
            Action::None => next,
            Action::Batch(mut v) => {
                v.push(next);
                Action::Batch(v)
            }
            other => Action::Batch(vec![other, next]),
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Action::None)
    }
}
