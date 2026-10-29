//! Pull request operations as [`Action`]s: diff viewer and checkout (plain git), create dialog,
//! comment, review, merge, close/reopen (provider API), browser links and sign-in. Network work
//! always runs in [`Action::spawn`] jobs or loader threads, so the UI stays responsive.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::action::Action;
use crate::config::HostingConfig;
use crate::ctx::Ctx;
use crate::git::hosting::refs::{self, PrSource};
use crate::git::hosting::{
    self, auth, web, Connection, Hosting, MergeMethod, NewPr, PrState, PullRequest, PushTarget,
    ReviewKind, UreqTransport,
};
use crate::git::Repo;
use crate::ui::diff_view::{DiffSource, DiffViewer};
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup, TextAreaPopup};
use crate::ui::widgets::{centered, panel, TextArea, TextInput};
use crate::ui::{KeyHelp, Popup};

use super::render::spinner;

/// The repository's hosting plus the config needed to connect from a background job.
#[derive(Debug, Clone)]
pub struct Target {
    pub h: Hosting,
    pub cfg: HostingConfig,
}

impl Target {
    pub fn new(h: &Hosting, ctx: &Ctx) -> Target {
        Target {
            h: h.clone(),
            cfg: ctx.config.hosting.clone(),
        }
    }

    /// Resolve credentials and build the API client (background jobs only).
    fn connect(&self, repo: &Repo) -> Result<Connection> {
        hosting::connect(repo, &self.h, &self.cfg)
    }

    fn noun(&self) -> &'static str {
        self.h.kind.noun()
    }
}

/// `#12 "Title"` for messages.
fn describe(h: &Hosting, pr: &PullRequest) -> String {
    format!(
        "{} \"{}\"",
        h.reference(pr.number),
        crate::util::truncate(&pr.title, 60)
    )
}

// ----- browser ---------------------------------------------------------------------------------

/// Open `url` in the browser, reporting the outcome.
pub fn open_in_browser(url: &str) -> Action {
    match hosting::open_url(url) {
        Ok(()) => Action::Toast(format!("Opened {url}")),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

pub fn open_repo_web(h: &Hosting) -> Action {
    open_in_browser(&web::repo_url(h))
}

pub fn open_pr_web(h: &Hosting, pr: &PullRequest) -> Action {
    let url = if pr.url.starts_with("http") {
        pr.url.clone()
    } else {
        web::pr_url(h, pr.number)
    };
    open_in_browser(&url)
}

pub fn open_branch_web(h: &Hosting, ctx: &Ctx) -> Action {
    match ctx.repo.head_branch() {
        Some(b) => open_in_browser(&web::branch_url(h, &b)),
        None => Action::Error("HEAD is detached — there is no current branch to show".into()),
    }
}

pub fn open_commit_web(h: &Hosting, ctx: &Ctx) -> Action {
    match ctx.repo.head_commit() {
        Some(sha) => open_in_browser(&web::commit_url(h, &sha)),
        None => Action::Error("The repository has no commits yet".into()),
    }
}

/// Push the current branch (never onto the base or default branch), then open the provider's
/// "create pull request" page.
pub fn create_in_browser(t: &Target, default_base: Option<&str>, ctx: &Ctx) -> Action {
    let repo = &ctx.repo;
    if web::create_url(&t.h, "x", "y", None).is_none() {
        return Action::Error(format!(
            "{} has no known \"create pull request\" page. Open the repository (W) and create it there.",
            t.h.url.host
        ));
    }
    if repo.head_commit().is_none() {
        return Action::Error("This repository has no commits yet — commit something first".into());
    }
    let Some(head) = repo.head_branch() else {
        return Action::Error("HEAD is detached — check out a branch first".into());
    };
    let base = default_base
        .map(str::to_string)
        .unwrap_or_else(|| hosting::local_default_branch(repo, &t.h.remote));
    if head == base {
        return Action::Error(format!(
            "'{head}' is the default branch — create a feature branch for the change first"
        ));
    }
    let t = t.clone();
    Action::spawn(format!("Create {} in browser", t.noun()), move |repo| {
        let pushed = hosting::ensure_pushed(repo, &head, &t.h.remote, &base, &base)?;
        let fork = hosting::fork_of(repo, &t.h, &pushed.remote);
        let url = web::create_url(
            &t.h,
            &pushed.branch,
            &base,
            fork.as_ref().map(|f| f.owner.as_str()),
        )
        .unwrap_or_else(|| web::repo_url(&t.h));
        hosting::open_url(&url)?;
        Ok(format!("Opened {url}"))
    })
}

// ----- git-based diff & checkout ---------------------------------------------------------------

/// Show the request's diff: fetch its ref into `pr/<N>` and diff against the merge base.
pub fn diff(t: &Target, pr: &PullRequest, ctx: &Ctx) -> Action {
    let title = format!(
        "{} {} — {}",
        t.h.reference(pr.number),
        crate::util::truncate(&pr.title, 70),
        pr.head
    );
    let (h, number, src) = (t.h.clone(), pr.number, PrSource::of(&t.h, pr));
    Action::popup(PrDiffPopup::start(title, ctx.repo.clone(), move |repo| {
        let f = refs::fetch(repo, &h, number, Some(&src))?;
        let (from, to) = refs::diff_range(repo, &f)?;
        Ok((from, to, f.note))
    }))
}

/// Diff a request known only by number (no API): its base is the local default branch.
pub fn diff_number(t: &Target, number: u64, ctx: &Ctx) -> Action {
    let base = hosting::local_default_branch(&ctx.repo, &t.h.remote);
    let title = format!("{} — {} into {base}", t.h.reference(number), t.noun());
    let h = t.h.clone();
    Action::popup(PrDiffPopup::start(title, ctx.repo.clone(), move |repo| {
        let src = PrSource {
            base,
            ..PrSource::default()
        };
        let f = refs::fetch(repo, &h, number, Some(&src))?;
        let (from, to) = refs::diff_range(repo, &f)?;
        Ok((from, to, f.note))
    }))
}

/// Check out the request as `pr/<N>` with plain git.
pub fn checkout(t: &Target, pr: &PullRequest) -> Action {
    let (h, number, src) = (t.h.clone(), pr.number, PrSource::of(&t.h, pr));
    Action::spawn(format!("Checkout {}", h.reference(number)), move |repo| {
        refs::checkout(repo, &h, number, Some(&src))
    })
}

/// Check out a request known only by number. Providers without a git ref (Bitbucket Cloud) look
/// up the source branch through the API first.
pub fn checkout_number(t: &Target, number: u64) -> Action {
    let t = t.clone();
    Action::spawn(format!("Checkout {}", t.h.reference(number)), move |repo| {
        let src = if refs::pr_ref(t.h.kind, number).is_some() {
            None
        } else {
            let conn = t.connect(repo)?;
            let d = conn.api.details(number)?;
            Some(PrSource::of(&t.h, &d.pr))
        };
        refs::checkout(repo, &t.h, number, src.as_ref())
    })
}

/// Ask for a request number (or URL), then run `f`.
pub fn ask_number(
    t: &Target,
    verb: &str,
    f: impl FnOnce(&Target, u64, &Ctx) -> Action + 'static,
) -> Action {
    let noun = t.noun();
    let prompt = format!("{noun} number ({}12) or web URL", t.h.kind.sigil());
    let t = t.clone();
    Action::popup(
        InputPopup::new(
            format!("{verb} {noun}"),
            "",
            move |text, ctx| match hosting::parse_pr_ref(&text) {
                Some(n) => f(&t, n, ctx),
                None => Action::Error(format!("'{}' is not a {noun} number or URL", text.trim())),
            },
        )
        .prompt(prompt),
    )
}

// ----- API operations --------------------------------------------------------------------------

/// Add a conversation comment.
pub fn comment(t: &Target, pr: &PullRequest) -> Action {
    let t = t.clone();
    let number = pr.number;
    let title = format!("Comment on {}", describe(&t.h, pr));
    Action::popup(TextAreaPopup::new(title, "", move |body, _ctx| {
        Action::spawn(
            format!("Comment on {}", t.h.reference(number)),
            move |repo| {
                t.connect(repo)?.api.comment(number, &body)?;
                Ok(format!("Commented on {}", t.h.reference(number)))
            },
        )
    }))
}

fn submit_review(t: Target, number: u64, kind: ReviewKind, body: String) -> Action {
    Action::spawn(
        format!("{} {}", kind.label(), t.h.reference(number)),
        move |repo| {
            let msg = t.connect(repo)?.api.review(number, kind, &body)?;
            Ok(format!("{}: {msg}", t.h.reference(number)))
        },
    )
}

fn review_body(t: &Target, number: u64, kind: ReviewKind) -> Action {
    let t = t.clone();
    let title = format!("{} — {}", kind.label(), t.h.reference(number));
    Action::popup(TextAreaPopup::new(title, "", move |body, _ctx| {
        submit_review(t, number, kind, body)
    }))
}

/// Review menu: approve, approve with comment, request changes, comment.
pub fn review(t: &Target, pr: &PullRequest) -> Action {
    if pr.state != PrState::Open {
        return Action::Error(format!(
            "{} is {} — only open {}s can be reviewed",
            t.h.reference(pr.number),
            pr.state.label().to_lowercase(),
            t.noun()
        ));
    }
    let n = pr.number;
    let (a, b, c, d) = (t.clone(), t.clone(), t.clone(), t.clone());
    let items = vec![
        MenuItem::new("Approve", "a", move |_| {
            submit_review(a, n, ReviewKind::Approve, String::new())
        }),
        MenuItem::new("Approve with comment…", "A", move |_| {
            review_body(&b, n, ReviewKind::Approve)
        }),
        MenuItem::new("Request changes…", "r", move |_| {
            review_body(&c, n, ReviewKind::RequestChanges)
        }),
        MenuItem::new("Comment…", "c", move |_| {
            review_body(&d, n, ReviewKind::Comment)
        }),
    ];
    Action::popup(MenuPopup::new(
        format!("Review {}", describe(&t.h, pr)),
        items,
    ))
}

fn confirm_merge(t: Target, pr: PullRequest, method: MergeMethod, delete_branch: bool) -> Action {
    let mut msg = format!(
        "Merge {} into {} using {}?\n{} → {}",
        describe(&t.h, &pr),
        pr.base,
        method.label(),
        pr.head,
        pr.base
    );
    if delete_branch {
        msg.push_str(&format!(
            "\nThe branch {} will be deleted afterwards.",
            pr.head
        ));
    }
    Action::popup(
        ConfirmPopup::new(format!("Merge {}", t.noun()), msg, move |_| {
            Action::spawn(format!("Merge {}", t.h.reference(pr.number)), move |repo| {
                t.connect(repo)?.api.merge(&pr, method, delete_branch)?;
                Ok(format!("Merged {}", t.h.reference(pr.number)))
            })
        })
        .danger(),
    )
}

/// Merge menu: method choice plus a "delete branch" toggle, then a danger confirmation.
pub fn merge(t: &Target, pr: &PullRequest, delete_branch: bool) -> Action {
    if pr.state != PrState::Open {
        return Action::Error(format!(
            "{} is {} — only open {}s can be merged",
            t.h.reference(pr.number),
            pr.state.label().to_lowercase(),
            t.noun()
        ));
    }
    if pr.draft {
        return Action::Error(format!(
            "{} is a draft — mark it ready for review first",
            t.h.reference(pr.number)
        ));
    }
    let item = |label: &str, key: &str, method: MergeMethod| {
        let (t, pr) = (t.clone(), pr.clone());
        MenuItem::new(label, key, move |_| {
            confirm_merge(t, pr, method, delete_branch)
        })
    };
    let (tt, tpr) = (t.clone(), pr.clone());
    let items = vec![
        item("Create a merge commit", "m", MergeMethod::Merge),
        item("Squash and merge", "s", MergeMethod::Squash),
        item("Rebase and merge", "r", MergeMethod::Rebase),
        MenuItem::new(
            format!(
                "[{}] Delete branch {} after merge",
                if delete_branch { "x" } else { " " },
                pr.head
            ),
            "d",
            move |_| merge(&tt, &tpr, !delete_branch),
        ),
    ];
    Action::popup(MenuPopup::new(
        format!("Merge {} into {}", t.h.reference(pr.number), pr.base),
        items,
    ))
}

/// Close an open request (danger confirm) or reopen a closed one.
pub fn close_or_reopen(t: &Target, pr: &PullRequest) -> Action {
    let reopen = match pr.state {
        PrState::Open => false,
        PrState::Closed => true,
        PrState::Merged => {
            return Action::Error(format!("{} is already merged", t.h.reference(pr.number)))
        }
        PrState::Locked => return Action::Error(format!("{} is locked", t.h.reference(pr.number))),
    };
    let (tc, prc) = (t.clone(), pr.clone());
    let verb = if reopen { "Reopen" } else { "Close" };
    let popup = ConfirmPopup::new(
        format!("{verb} {}", t.noun()),
        format!("{verb} {}?", describe(&t.h, pr)),
        move |_| {
            Action::spawn(
                format!("{verb} {}", tc.h.reference(prc.number)),
                move |repo| {
                    tc.connect(repo)?.api.set_state(&prc, reopen)?;
                    Ok(format!(
                        "{}{} {}",
                        verb,
                        if reopen { "ed" } else { "d" },
                        tc.h.reference(prc.number)
                    ))
                },
            )
        },
    );
    Action::popup(if reopen { popup } else { popup.danger() })
}

// ----- sign in ---------------------------------------------------------------------------------

/// Sign in: ask for a user name where the provider may need one, then the token; the token is
/// checked against the API and stored with `git credential approve`.
pub fn sign_in(t: &Target) -> Action {
    if !t.h.kind.has_api() {
        return Action::Error(format!(
            "{} has no known pull request API to sign in to",
            t.h.url.host
        ));
    }
    if t.h.kind.asks_username() {
        let t = t.clone();
        Action::popup(
            InputPopup::new(
                format!("Sign in to {}", t.h.url.credential_host()),
                "",
                move |user, _| Action::popup(TokenPopup::new(t, Some(user))),
            )
            .prompt(
                "User name (for an app password / personal token);\nleave empty for an access token",
            )
            .allow_empty(),
        )
    } else {
        Action::popup(TokenPopup::new(t.clone(), None))
    }
}

/// Masked token entry.
pub struct TokenPopup {
    target: Target,
    username: Option<String>,
    input: TextInput,
}

impl TokenPopup {
    fn new(target: Target, username: Option<String>) -> Self {
        Self {
            target,
            username: username.filter(|u| !u.trim().is_empty()),
            input: TextInput::default(),
        }
    }

    fn submit(&mut self) -> Action {
        let secret = self.input.value().trim().to_string();
        if secret.is_empty() {
            return Action::None;
        }
        let t = self.target.clone();
        let user = self.username.clone();
        let host = t.h.url.credential_host();
        Action::ClosePopup.then(Action::spawn(format!("Sign in to {host}"), move |repo| {
            let who = auth::sign_in(repo, &t.h, &t.cfg, user, &secret, UreqTransport::shared())?;
            Ok(format!(
                "Signed in to {host} as {who}; the token is stored by your git credential helper"
            ))
        }))
    }
}

impl Popup for TokenPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered(area, 84, 10);
        f.render_widget(Clear, r);
        let block = crate::ui::widgets::dialog(
            format!(
                " Sign in to {} ({}) ",
                self.target.h.url.credential_host(),
                self.target.h.kind.name()
            ),
            t,
        );
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [info, field, keys] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let mut text = format!(
            "Personal access token — create one under {}.",
            self.target.h.kind.token_help()
        );
        if let Some(u) = &self.username {
            text.push_str(&format!(" User: {u}."));
        }
        text.push_str(" It is checked, then stored by your git credential helper.");
        f.render_widget(
            Paragraph::new(text)
                .style(t.dimmed())
                .wrap(Wrap { trim: true }),
            info,
        );
        let [label, value] =
            Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).areas(field);
        f.render_widget(Span::styled("Token ", t.key()), label);
        let masked = TextInput::new(&"•".repeat(self.input.value().chars().count()));
        masked.render(f, value, t.text(), true);
        f.render_widget(
            Line::from(vec![
                Span::styled("Enter", t.key()),
                Span::styled(" sign in   ", t.dimmed()),
                Span::styled("Esc", t.key()),
                Span::styled(" cancel", t.dimmed()),
            ]),
            keys,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> Action {
        match key.code {
            KeyCode::Esc => Action::ClosePopup,
            KeyCode::Enter => self.submit(),
            _ => {
                self.input.handle_key(&key);
                Action::None
            }
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("Enter", "sign in"), ("Esc", "cancel")]
    }
}

// ----- diff popup ------------------------------------------------------------------------------

/// (from, to, note) once fetched.
type DiffResult = Result<(String, String, Option<String>), String>;
type DiffSlot = Arc<Mutex<Option<DiffResult>>>;

/// Fetches the request with git on a thread, then hands the range to the standard [`DiffViewer`].
pub struct PrDiffPopup {
    title: String,
    slot: DiffSlot,
    viewer: Option<DiffViewer>,
    error: Option<String>,
    started: Instant,
}

impl PrDiffPopup {
    fn start(
        title: String,
        repo: Repo,
        job: impl FnOnce(&Repo) -> Result<(String, String, Option<String>)> + Send + 'static,
    ) -> Self {
        let slot: DiffSlot = Arc::default();
        let out = slot.clone();
        std::thread::spawn(move || {
            let r = job(&repo).map_err(|e| format!("{e:#}"));
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        });
        Self {
            title,
            slot,
            viewer: None,
            error: None,
            started: Instant::now(),
        }
    }

    fn poll_slot(&mut self) -> Action {
        if self.viewer.is_some() || self.error.is_some() {
            return Action::None;
        }
        let done = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        match done {
            Some(Ok((from, to, note))) => {
                self.viewer = Some(DiffViewer::new(
                    self.title.clone(),
                    DiffSource::Range {
                        from,
                        to,
                        paths: Vec::new(),
                    },
                ));
                note.map(Action::Toast).unwrap_or(Action::None)
            }
            Some(Err(e)) => {
                self.error = Some(e);
                Action::None
            }
            None => Action::None,
        }
    }
}

impl Popup for PrDiffPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let _ = self.poll_slot();
        if let Some(v) = self.viewer.as_mut() {
            v.draw(f, area, ctx);
            return;
        }
        f.render_widget(Clear, area);
        let block = panel(format!(" {} ", self.title), true, &ctx.theme);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let p = match &self.error {
            Some(e) => Paragraph::new(e.as_str()).style(Style::default().fg(ctx.theme.error)),
            None => Paragraph::new(format!("{} Fetching with git…", spinner(self.started)))
                .style(ctx.theme.dimmed()),
        };
        f.render_widget(p.wrap(Wrap { trim: false }), inner);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let polled = self.poll_slot();
        let a = match self.viewer.as_mut() {
            Some(v) => v.handle_key(key, ctx),
            None => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => Action::ClosePopup,
                _ => Action::None,
            },
        };
        polled.then(a)
    }

    fn poll(&mut self, _ctx: &Ctx) -> Action {
        self.poll_slot()
    }

    fn refresh(&mut self, ctx: &Ctx) {
        if let Some(v) = self.viewer.as_mut() {
            v.refresh(ctx);
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        match &self.viewer {
            Some(v) => v.help(),
            None => vec![("Esc", "close")],
        }
    }
}

// ----- create dialog ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Base,
    Draft,
    Body,
}

const FIELDS: [Field; 4] = [Field::Title, Field::Base, Field::Draft, Field::Body];

/// "Create pull request" form: title, base branch, draft toggle, description.
pub struct CreatePrPopup {
    target: Target,
    head: String,
    /// Where the branch will be pushed (from local config; re-resolved when submitting).
    push_target: PushTarget,
    /// The provider's default branch when already known; looked up in the background otherwise.
    default_branch: Option<String>,
    title: TextInput,
    base: TextInput,
    draft: bool,
    body: TextArea,
    focus: Field,
    error: Option<String>,
}

/// Open the create dialog for the current branch; `default_base` falls back to local refs.
pub fn create(t: &Target, default_base: Option<&str>, ctx: &Ctx) -> Action {
    let repo = &ctx.repo;
    if !t.h.kind.has_api() {
        return create_in_browser(t, default_base, ctx);
    }
    if repo.head_commit().is_none() {
        return Action::Error("This repository has no commits yet — commit something first".into());
    }
    let Some(head) = repo.head_branch() else {
        return Action::Error(format!(
            "HEAD is detached — check out a branch to create a {}",
            t.noun()
        ));
    };
    let base = default_base
        .map(str::to_string)
        .unwrap_or_else(|| hosting::local_default_branch(repo, &t.h.remote));
    Action::popup(CreatePrPopup {
        target: t.clone(),
        push_target: hosting::push_target(repo, &head, &t.h.remote),
        default_branch: default_base.map(str::to_string),
        title: TextInput::new(&hosting::last_commit_subject(repo)),
        base: TextInput::new(&base),
        draft: false,
        body: TextArea::new(&hosting::suggested_description(repo, &t.h.remote, &base)),
        head,
        focus: Field::Title,
        error: None,
    })
}

impl CreatePrPopup {
    fn cycle(&mut self, delta: isize) {
        let i = FIELDS.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = FIELDS[(i + delta).rem_euclid(FIELDS.len() as isize) as usize];
    }

    fn submit(&mut self) -> Action {
        let title = self.title.value().trim().to_string();
        let base = self.base.value().trim().to_string();
        if title.is_empty() {
            self.error = Some("Title is required".into());
            self.focus = Field::Title;
            return Action::None;
        }
        if base.is_empty() {
            self.error = Some("Base branch is required".into());
            self.focus = Field::Base;
            return Action::None;
        }
        let same_repo = self.push_target.remote == self.target.h.remote;
        if base == self.head || (same_repo && base == self.push_target.branch) {
            self.error = Some(format!(
                "Base and head are both {base}; pick another base branch or create a feature branch"
            ));
            self.focus = Field::Base;
            return Action::None;
        }
        let t = self.target.clone();
        let (head, body, draft) = (self.head.clone(), self.body.value(), self.draft);
        let known_default = self.default_branch.clone();
        let noun = t.noun();
        Action::ClosePopup.then(Action::spawn(format!("Create {noun}"), move |repo| {
            let conn = t.connect(repo)?;
            let default_branch = known_default
                .or_else(|| conn.api.default_branch().ok().filter(|b| !b.is_empty()))
                .unwrap_or_else(|| hosting::local_default_branch(repo, &t.h.remote));
            let pushed = hosting::ensure_pushed(repo, &head, &t.h.remote, &base, &default_branch)?;
            let head_repo = hosting::fork_of(repo, &t.h, &pushed.remote);
            if head_repo.is_none() && pushed.branch == base {
                anyhow::bail!("Head and base are both {base}; create a feature branch first");
            }
            let pr = NewPr {
                title,
                body,
                base,
                head: pushed.branch.clone(),
                head_repo,
                draft,
            };
            let created = conn.api.create(&pr)?;
            let pushed_note = if pushed.pushed {
                format!(" (pushed {} to {})", head, pushed.remote)
            } else {
                String::new()
            };
            Ok(format!(
                "Created {noun} {}: {}{pushed_note}",
                t.h.reference(created.number),
                created.url
            ))
        }))
    }

    fn label<'a>(&self, field: Field, text: &'a str, ctx: &Ctx) -> Span<'a> {
        let style = if self.focus == field {
            ctx.theme.key()
        } else {
            ctx.theme.dimmed()
        };
        Span::styled(text, style)
    }
}

impl Popup for CreatePrPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered(area, 96, 26);
        f.render_widget(Clear, r);
        let title = format!(
            " Create {} — {} ",
            self.target.noun(),
            self.target.h.display()
        );
        let block = crate::ui::widgets::dialog(title, t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [info, title_row, base_row, draft_row, body_label, body, msg, keys] =
            Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(inner);
        let push_note = if self.push_target.reuse_upstream {
            format!(
                "Unpushed commits will be pushed to {} first.",
                self.push_target.display()
            )
        } else {
            format!(
                "The branch will be pushed with `git push -u` to {} first.",
                self.push_target.display()
            )
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(self.head.clone(), Style::default().fg(t.local_branch)),
                    Span::styled(" → ", t.dimmed()),
                    Span::styled(self.base.value(), Style::default().fg(t.remote_branch)),
                ]),
                Line::styled(push_note, t.dimmed()),
            ]),
            info,
        );
        let field_row = |row: Rect| {
            Layout::horizontal([Constraint::Length(13), Constraint::Min(1)]).areas::<2>(row)
        };
        let [l, v] = field_row(title_row);
        f.render_widget(self.label(Field::Title, "Title", ctx), l);
        self.title
            .render(f, v, t.text(), self.focus == Field::Title);
        let [l, v] = field_row(base_row);
        f.render_widget(self.label(Field::Base, "Base branch", ctx), l);
        self.base.render(f, v, t.text(), self.focus == Field::Base);
        let [l, v] = field_row(draft_row);
        f.render_widget(self.label(Field::Draft, "Draft", ctx), l);
        let check = format!("[{}] create as draft", if self.draft { "x" } else { " " });
        let style = if self.focus == Field::Draft {
            t.selected()
        } else {
            t.text()
        };
        f.render_widget(Span::styled(check, style), v);
        f.render_widget(self.label(Field::Body, "Description", ctx), body_label);
        let body_block = panel("", self.focus == Field::Body, t);
        let body_inner = body_block.inner(body);
        f.render_widget(body_block, body);
        self.body
            .render(f, body_inner, t.text(), self.focus == Field::Body, None);
        if let Some(e) = &self.error {
            f.render_widget(Span::styled(e.clone(), Style::default().fg(t.error)), msg);
        }
        let hint = |k: &'static str, d: &'static str| {
            [
                Span::styled(k, t.key()),
                Span::styled(format!(" {d}   "), t.dimmed()),
            ]
        };
        let mut spans = Vec::new();
        spans.extend(hint("Ctrl+S", "create"));
        spans.extend(hint("Tab/Shift+Tab", "next/prev field"));
        spans.extend(hint("Space", "toggle draft"));
        spans.extend(hint("Ctrl+D", "draft"));
        spans.extend(hint("Esc", "cancel"));
        f.render_widget(Line::from(spans), keys);
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Action::ClosePopup,
            KeyCode::Char('s') if ctrl => return self.submit(),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => return self.submit(),
            KeyCode::Char('d') if ctrl => self.draft = !self.draft,
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(-1),
            _ => match self.focus {
                Field::Title | Field::Base => {
                    if matches!(key.code, KeyCode::Enter | KeyCode::Down) {
                        self.cycle(1);
                    } else if key.code == KeyCode::Up {
                        self.cycle(-1);
                    } else {
                        let input = if self.focus == Field::Title {
                            &mut self.title
                        } else {
                            &mut self.base
                        };
                        input.handle_key(&key);
                    }
                }
                Field::Draft => match key.code {
                    KeyCode::Char(' ') | KeyCode::Char('x') => self.draft = !self.draft,
                    KeyCode::Enter | KeyCode::Down => self.cycle(1),
                    KeyCode::Up => self.cycle(-1),
                    _ => {}
                },
                Field::Body => {
                    self.body.handle_key(&key);
                }
            },
        }
        self.error = None;
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("Ctrl+S", "create"),
            ("Tab", "next field"),
            ("Ctrl+D", "toggle draft"),
            ("Esc", "cancel"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::hosting::{parse_remote_url, DetectedBy, ProviderKind};

    fn target(kind: ProviderKind, url: &str) -> Target {
        Target {
            h: Hosting::new(
                kind,
                "origin",
                url,
                parse_remote_url(url).expect("url"),
                None,
                DetectedBy::KnownHost,
            ),
            cfg: HostingConfig::default(),
        }
    }

    fn popup() -> CreatePrPopup {
        CreatePrPopup {
            target: target(ProviderKind::GitHub, "https://github.com/o/r"),
            head: "feature".into(),
            push_target: PushTarget {
                remote: "origin".into(),
                branch: "feature".into(),
                reuse_upstream: false,
            },
            default_branch: Some("main".into()),
            title: TextInput::new("Subject"),
            base: TextInput::new("main"),
            draft: false,
            body: TextArea::new(""),
            focus: Field::Title,
            error: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctx() -> (tempfile::TempDir, Ctx) {
        let (d, repo) = crate::git::testutil::temp_repo();
        (d, Ctx::new(repo, crate::config::Config::default()))
    }

    #[test]
    fn create_form_navigation_and_validation() {
        let mut p = popup();
        p.cycle(-1);
        assert_eq!(p.focus, Field::Body);
        p.cycle(1);
        assert_eq!(p.focus, Field::Title);
        p.base = TextInput::new("feature");
        assert!(p.submit().is_none());
        assert!(p
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Base and head")));
        p.title = TextInput::new("  ");
        assert!(p.submit().is_none());
        assert_eq!(p.focus, Field::Title);
        p.title = TextInput::new("Real title");
        p.base = TextInput::new("main");
        assert!(matches!(p.submit(), Action::Batch(_)));
        // A branch whose upstream is origin/main (push.default=upstream) cannot target main.
        p.push_target.branch = "main".into();
        p.push_target.reuse_upstream = true;
        p.error = None;
        assert!(p.submit().is_none());
        assert!(p.error.is_some());
        // A fork's main may target the upstream's main.
        p.push_target.remote = "fork".into();
        assert!(matches!(p.submit(), Action::Batch(_)));
    }

    #[test]
    fn keys_drive_the_form() {
        let (_d, ctx) = ctx();
        let mut p = popup();
        p.handle_key(key(KeyCode::Char('!')), &ctx);
        assert_eq!(p.title.value(), "Subject!");
        p.handle_key(key(KeyCode::Enter), &ctx);
        assert_eq!(p.focus, Field::Base);
        p.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(p.focus, Field::Draft);
        p.handle_key(key(KeyCode::Char(' ')), &ctx);
        assert!(p.draft);
        p.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &ctx,
        );
        assert!(!p.draft);
        p.handle_key(key(KeyCode::Enter), &ctx);
        assert_eq!(p.focus, Field::Body);
        p.handle_key(key(KeyCode::Char('a')), &ctx);
        p.handle_key(key(KeyCode::Enter), &ctx);
        p.handle_key(key(KeyCode::Char('b')), &ctx);
        assert_eq!(p.body.value(), "a\nb");
        p.handle_key(key(KeyCode::BackTab), &ctx);
        assert_eq!(p.focus, Field::Draft);
        assert!(matches!(
            p.handle_key(key(KeyCode::Esc), &ctx),
            Action::ClosePopup
        ));
    }

    #[test]
    fn create_requires_commits_and_a_branch() {
        let (_d, ctx) = ctx();
        let t = popup().target;
        assert!(matches!(create(&t, None, &ctx), Action::Error(_)));
        crate::git::testutil::commit_files(&ctx.repo, &[("a", "1\n")], "init");
        assert!(matches!(create(&t, Some("main"), &ctx), Action::Popup(_)));
        // On the default branch itself, "create in browser" refuses before pushing anything.
        assert!(
            matches!(create_in_browser(&t, Some("main"), &ctx), Action::Error(e) if e.contains("default branch"))
        );
        let generic = target(ProviderKind::Generic, "https://git.sr.ht/~me/x");
        assert!(matches!(create(&generic, None, &ctx), Action::Error(e) if e.contains("no known")));
        ctx.repo.run(&["checkout", "-q", "--detach"]).unwrap();
        assert!(matches!(create(&t, None, &ctx), Action::Error(e) if e.contains("detached")));
    }

    #[test]
    fn state_guards() {
        let t = popup().target;
        let mut pr = PullRequest {
            number: 3,
            title: "T".into(),
            head: "f".into(),
            base: "main".into(),
            state: PrState::Merged,
            ..Default::default()
        };
        assert!(matches!(merge(&t, &pr, false), Action::Error(_)));
        assert!(matches!(close_or_reopen(&t, &pr), Action::Error(_)));
        assert!(matches!(review(&t, &pr), Action::Error(_)));
        pr.state = PrState::Open;
        assert!(matches!(merge(&t, &pr, true), Action::Popup(_)));
        assert!(matches!(close_or_reopen(&t, &pr), Action::Popup(_)));
        assert!(matches!(review(&t, &pr), Action::Popup(_)));
        pr.draft = true;
        assert!(matches!(merge(&t, &pr, false), Action::Error(_)));
        pr.state = PrState::Closed;
        assert!(matches!(close_or_reopen(&t, &pr), Action::Popup(_)));
    }

    #[test]
    fn sign_in_dialogs() {
        let (_d, ctx) = ctx();
        let github = popup().target;
        let mut p = TokenPopup::new(github.clone(), None);
        assert!(p.submit().is_none(), "empty token is ignored");
        for c in "tok".chars() {
            p.handle_key(key(KeyCode::Char(c)), &ctx);
        }
        assert!(matches!(p.submit(), Action::Batch(_)));
        assert!(matches!(sign_in(&github), Action::Popup(_)));
        let bb = target(ProviderKind::BitbucketCloud, "git@bitbucket.org:t/r.git");
        assert!(matches!(sign_in(&bb), Action::Popup(_)));
        let generic = target(ProviderKind::Generic, "https://git.sr.ht/~me/x");
        assert!(matches!(sign_in(&generic), Action::Error(_)));
    }

    #[test]
    fn diff_popup_shows_errors_from_the_fetch() {
        let (_d, ctx) = ctx();
        let t = target(ProviderKind::GitHub, "https://github.com/o/r");
        let pr = PullRequest {
            number: 1,
            head: "f".into(),
            base: "main".into(),
            ..Default::default()
        };
        let Action::Popup(mut p) = diff(&t, &pr, &ctx) else {
            panic!("expected popup")
        };
        // No remote named origin: the fetch fails and the popup reports it.
        let start = Instant::now();
        loop {
            let _ = p.poll(&ctx);
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
            term.draw(|f| p.draw(f, f.area(), &ctx)).unwrap();
            let buf = term.backend().buffer().clone();
            let s: String = buf.content().iter().map(|c| c.symbol()).collect();
            if s.contains("Could not fetch") {
                break;
            }
            assert!(start.elapsed().as_secs() < 20, "{s}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(matches!(
            p.handle_key(key(KeyCode::Esc), &ctx),
            Action::ClosePopup
        ));
    }
}
