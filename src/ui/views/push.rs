//! Push dialog: commits to push, their files, options, background push.
//!
//! The commits and each commit's files load in the background, so the dialog opens at once.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::branch;
use crate::git::log::Commit;
use crate::git::remote::{self, PushSpec};
use crate::ui::bg::{Background, Preview};
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup};
use crate::ui::views::branches::commit_line;
use crate::ui::widgets::{centered_pct, move_sel, panel, scroll_to_keep_visible};
use crate::ui::{KeyHelp, PaletteItem, Popup};

/// Commits listed at most (the push itself is not limited).
const MAX_COMMITS: usize = 1000;

/// Open the push dialog for the current branch.
pub fn open(ctx: &Ctx) -> Action {
    let Some(branch) = ctx.repo.head_branch() else {
        return Action::Error(
            "HEAD is detached: check out a branch to push it (or create one from here with 'n' in the Branches tab)"
                .into(),
        );
    };
    if ctx.repo.head_commit().is_none() {
        return Action::Error(format!("'{branch}' has no commits yet — nothing to push"));
    }
    match PushSpec::for_branch(&ctx.repo, &branch) {
        Ok(spec) => Action::popup(PushPopup::new(ctx, spec)),
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Commits,
    Files,
    Button,
}

impl Focus {
    fn next(self, back: bool) -> Focus {
        match (self, back) {
            (Focus::Commits, false) | (Focus::Button, true) => Focus::Files,
            (Focus::Files, false) | (Focus::Commits, true) => Focus::Button,
            (Focus::Button, false) | (Focus::Files, true) => Focus::Commits,
        }
    }
}

type FileEntry = (char, String, Option<String>);

/// Whether the target branch is new on the remote, and the commits to push.
type Loaded = (bool, Result<Vec<Commit>, String>);

pub struct PushPopup {
    spec: PushSpec,
    /// The target branch does not exist on the remote yet
    is_new: bool,
    commits: Vec<Commit>,
    sel: usize,
    offset: usize,
    /// Files of the selected commit, and which commit they belong to
    files: Vec<FileEntry>,
    files_for: Option<String>,
    file_sel: usize,
    file_offset: usize,
    focus: Focus,
    error: Option<String>,
    page: usize,
    loader: Background<Loaded>,
    /// When the current load started (for the spinner)
    loading_since: Instant,
    file_loader: Preview<String, Vec<FileEntry>>,
}

impl PushPopup {
    pub fn new(ctx: &Ctx, spec: PushSpec) -> Self {
        let mut p = Self {
            spec,
            is_new: false,
            commits: Vec::new(),
            sel: 0,
            offset: 0,
            files: Vec::new(),
            files_for: None,
            file_sel: 0,
            file_offset: 0,
            focus: Focus::Commits,
            error: None,
            page: 10,
            loader: Background::new(),
            loading_since: Instant::now(),
            file_loader: Preview::new(Duration::from_millis(40), 32),
        };
        p.load(ctx);
        p
    }

    /// Start (re)loading the commits to push; the result arrives in [`apply_loaded`](Self::apply_loaded).
    fn load(&mut self, ctx: &Ctx) {
        let s = self.spec.clone();
        self.loading_since = Instant::now();
        self.file_loader.invalidate();
        self.loader.start(&ctx.repo, move |repo| {
            let is_new = !remote::remote_branch_exists(repo, &s.remote, &s.target);
            let commits =
                remote::commits_to_push(repo, &s.local, &s.remote, &s.target, MAX_COMMITS)
                    .map_err(|e| format!("{e:#}"));
            (is_new, commits)
        });
        self.apply_loaded();
    }

    fn apply_loaded(&mut self) {
        let Some((is_new, commits)) = self.loader.poll() else {
            return;
        };
        self.is_new = is_new;
        match commits {
            Ok(c) => {
                self.commits = c;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.sel = self.sel.min(self.commits.len().saturating_sub(1));
        self.files_for = None;
    }

    fn nothing_to_push(&self) -> bool {
        self.commits.is_empty() && !self.is_new && !self.spec.tags
    }

    fn ensure_files(&mut self, ctx: &Ctx) {
        let id = self.commits.get(self.sel).map(|c| c.id.clone());
        let Some(id) = id else {
            self.files.clear();
            self.files_for = None;
            return;
        };
        if self.files_for.as_ref() == Some(&id) {
            return;
        }
        let key = id.clone();
        self.file_loader.request(&ctx.repo, key, move |repo| {
            repo.commit_files(&id).unwrap_or_default()
        });
        let id = self.commits[self.sel].id.clone();
        if let Some(files) = self.file_loader.get(&id) {
            self.files = files;
            self.files_for = Some(id);
            self.file_sel = 0;
            self.file_offset = 0;
        }
    }

    fn push(&self) -> Action {
        if self.loader.busy() {
            return Action::Toast("Still working out what to push…".into());
        }
        if self.nothing_to_push() {
            return Action::Toast("Nothing to push".into());
        }
        let spec = self.spec.clone();
        let run = move || Action::spawn("Push", move |repo| remote::push(repo, &spec));
        if self.spec.force_with_lease {
            let msg = format!(
                "Force-push {} to {} with --force-with-lease?\nCommits on the remote that are not in your branch will be discarded\n(the push is refused if the remote changed since your last fetch).",
                self.spec.local,
                self.spec.target_short()
            );
            return Action::popup(
                ConfirmPopup::new("Force push", msg, move |_| Action::ClosePopup.then(run()))
                    .danger(),
            );
        }
        Action::ClosePopup.then(run())
    }

    /// 'R': choose the remote (if several), then the target branch name.
    fn change_target(&self, ctx: &Ctx) -> Action {
        let remotes = remote::remotes(&ctx.repo).unwrap_or_default();
        if remotes.len() <= 1 {
            return target_prompt(self.spec.clone());
        }
        let items = remotes
            .into_iter()
            .map(|r| {
                let mut spec = self.spec.clone();
                let current = r == spec.remote;
                let label = if current {
                    format!("{r} (current)")
                } else {
                    r.clone()
                };
                spec.remote = r;
                MenuItem::new(label, "", move |_| target_prompt(spec))
            })
            .collect();
        Action::popup(MenuPopup::new("Push to remote", items))
    }

    fn open_selected(&self) -> Action {
        let Some(c) = self.commits.get(self.sel) else {
            return Action::None;
        };
        let paths = match self.focus {
            Focus::Files => self
                .files
                .get(self.file_sel)
                .map(|f| vec![f.1.clone()])
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        diff_view::open(
            format!("{} {}", c.short, c.subject),
            DiffSource::Commit {
                rev: c.id.clone(),
                paths,
            },
        )
    }

    // ----- drawing -------------------------------------------------------------------------

    fn draw_header(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let mut spans = vec![
            Span::styled(
                self.spec.local.clone(),
                Style::default()
                    .fg(t.local_branch)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" → ", t.dimmed()),
            Span::styled(
                self.spec.target_short(),
                Style::default()
                    .fg(t.remote_branch)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if self.is_new {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                " NEW ",
                Style::default()
                    .fg(t.success)
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ));
        }
        spans.push(Span::styled("   R", t.key()));
        spans.push(Span::styled(" change target", t.dimmed()));
        f.render_widget(Line::from(spans), area);
    }

    fn draw_commits(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let focused = self.focus == Focus::Commits;
        let more = if self.commits.len() >= MAX_COMMITS {
            "+"
        } else {
            ""
        };
        let block = panel(
            format!(" Commits to push ({}{more}) ", self.commits.len()),
            focused,
            t,
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        if self.loader.busy() && self.commits.is_empty() {
            let msg = format!(
                "{} Finding commits to push…",
                crate::app::spinner_frame(self.loading_since)
            );
            f.render_widget(
                Paragraph::new(msg).style(Style::default().fg(t.accent)),
                inner,
            );
            return;
        }
        if let Some(e) = &self.error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                inner,
            );
            return;
        }
        if self.commits.is_empty() {
            let msg = if self.is_new {
                format!(
                    "No new commits: {} will be created on {} at the current tip.",
                    self.spec.target, self.spec.remote
                )
            } else if self.spec.tags {
                "No commits to push; tags will be pushed.".to_string()
            } else {
                format!(
                    "Nothing to push — {} is up to date with {}.",
                    self.spec.local,
                    self.spec.target_short()
                )
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), inner);
            return;
        }
        let h = inner.height as usize;
        self.page = h.max(1);
        self.offset = scroll_to_keep_visible(self.sel, self.offset, h);
        let lines: Vec<Line> = self
            .commits
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(h)
            .map(|(i, c)| {
                let l = commit_line(c, inner.width as usize, ctx);
                if i == self.sel {
                    l.patch_style(if focused {
                        t.selected()
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    })
                } else {
                    l
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_files(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        self.ensure_files(ctx);
        let focused = self.focus == Focus::Files;
        let block = panel(format!(" Files ({}) ", self.files.len()), focused, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let h = inner.height as usize;
        self.file_offset = scroll_to_keep_visible(self.file_sel, self.file_offset, h);
        let lines: Vec<Line> = self
            .files
            .iter()
            .enumerate()
            .skip(self.file_offset)
            .take(h)
            .map(|(i, (st, path, old))| {
                let color = match st {
                    'A' => t.added,
                    'D' => t.removed,
                    'R' | 'C' => t.renamed,
                    _ => t.modified,
                };
                let shown = match old {
                    Some(o) => format!("{o} → {path}"),
                    None => path.clone(),
                };
                let l = Line::from(vec![
                    Span::styled(format!("{st} "), Style::default().fg(color)),
                    Span::styled(shown, t.text()),
                ]);
                if focused && i == self.file_sel {
                    l.patch_style(t.selected())
                } else {
                    l
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_options(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let check = |on: bool| if on { "[x] " } else { "[ ] " };
        let force_style = if self.spec.force_with_lease {
            Style::default().fg(t.error).add_modifier(Modifier::BOLD)
        } else {
            t.text()
        };
        let line = Line::from(vec![
            Span::styled(
                format!("{}Force with lease ", check(self.spec.force_with_lease)),
                force_style,
            ),
            Span::styled("(f)", t.key()),
            Span::raw("    "),
            Span::styled(format!("{}Push tags ", check(self.spec.tags)), t.text()),
            Span::styled("(t)", t.key()),
            Span::raw("    "),
            Span::styled(
                format!("{}Set upstream ", check(self.spec.set_upstream)),
                t.text(),
            ),
            Span::styled("(u)", t.key()),
        ]);
        f.render_widget(line, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let disabled = self.nothing_to_push();
        let label = if self.spec.force_with_lease {
            " Force Push "
        } else {
            " Push "
        };
        let mut style = if disabled {
            t.dimmed().add_modifier(Modifier::REVERSED)
        } else if self.spec.force_with_lease {
            Style::default()
                .fg(t.error)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(t.accent)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        };
        if self.focus == Focus::Button {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        let [hints, button] = Layout::horizontal([
            Constraint::Min(1),
            Constraint::Length(label.len() as u16 + 4),
        ])
        .areas(area);
        let hint = |k: &'static str, d: &'static str| {
            [Span::styled(k, t.key()), Span::styled(d, t.dimmed())]
        };
        let spans: Vec<Span> = [
            hint("p/Ctrl+S", " push   "),
            hint("Enter", " show diff   "),
            hint("Tab", " next pane   "),
            hint("Esc", " cancel"),
        ]
        .into_iter()
        .flatten()
        .collect();
        f.render_widget(Line::from(spans), hints);
        let marker = if self.focus == Focus::Button {
            "▶ "
        } else {
            "  "
        };
        f.render_widget(
            Line::from(vec![
                Span::styled(marker, t.key()),
                Span::styled(label, style),
            ]),
            button,
        );
    }
}

/// Ask for the target branch name on `spec.remote`, then reopen the dialog with it.
fn target_prompt(spec: PushSpec) -> Action {
    let title = format!("Push {} to branch on {}", spec.local, spec.remote);
    let initial = spec.target.clone();
    let popup = InputPopup::new(title, &initial, move |name, ctx| {
        let name = name.trim().to_string();
        if let Err(e) = branch::validate_name(&ctx.repo, &name) {
            return Action::Error(format!("{e:#}"));
        }
        let mut spec = spec;
        let tracks_it = remote::upstream_of(&ctx.repo, &spec.local)
            .is_some_and(|u| u.remote == spec.remote && u.branch == name);
        spec.set_upstream = spec.set_upstream
            || !tracks_it && remote::upstream_of(&ctx.repo, &spec.local).is_none();
        spec.target = name;
        Action::ClosePopup.then(Action::popup(PushPopup::new(ctx, spec)))
    })
    .prompt("Name of the branch on the remote (created if it does not exist).");
    Action::popup(popup)
}

impl Popup for PushPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered_pct(area, 90, 85);
        f.render_widget(Clear, r);
        let block = crate::ui::widgets::dialog(" Push Commits ", t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [header, body, options, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(inner);
        self.draw_header(f, header, ctx);
        if body.width >= 90 {
            let [c, fl] =
                Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                    .areas(body);
            self.draw_commits(f, c, ctx);
            self.draw_files(f, fl, ctx);
        } else {
            let [c, fl] =
                Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(body);
            self.draw_commits(f, c, ctx);
            self.draw_files(f, fl, ctx);
        }
        let [opts, _] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(options);
        self.draw_options(f, opts, ctx);
        self.draw_footer(f, footer, ctx);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.push();
        }
        let page = self.page as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::ClosePopup,
            KeyCode::Tab => self.focus = self.focus.next(false),
            KeyCode::BackTab => self.focus = self.focus.next(true),
            KeyCode::Char('p') => return self.push(),
            KeyCode::Char('f') => self.spec.force_with_lease = !self.spec.force_with_lease,
            KeyCode::Char('t') => self.spec.tags = !self.spec.tags,
            KeyCode::Char('u') => self.spec.set_upstream = !self.spec.set_upstream,
            KeyCode::Char('R') => return self.change_target(ctx),
            KeyCode::Enter => {
                return match self.focus {
                    Focus::Button => self.push(),
                    _ => self.open_selected(),
                }
            }
            code => {
                let (sel, len) = match self.focus {
                    Focus::Files => (&mut self.file_sel, self.files.len()),
                    _ => (&mut self.sel, self.commits.len()),
                };
                match code {
                    KeyCode::Up | KeyCode::Char('k') => *sel = move_sel(*sel, -1, len),
                    KeyCode::Down | KeyCode::Char('j') => *sel = move_sel(*sel, 1, len),
                    KeyCode::PageUp => *sel = move_sel(*sel, -page, len),
                    KeyCode::PageDown => *sel = move_sel(*sel, page, len),
                    KeyCode::Home | KeyCode::Char('g') => *sel = 0,
                    KeyCode::End | KeyCode::Char('G') => *sel = len.saturating_sub(1),
                    _ => {}
                }
            }
        }
        Action::None
    }

    fn refresh(&mut self, ctx: &Ctx) {
        self.load(ctx);
    }

    fn poll(&mut self, _ctx: &Ctx) -> Action {
        self.apply_loaded();
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("p/Ctrl+S", "push"),
            ("Enter", "show diff"),
            ("Tab", "next pane"),
            ("f", "force with lease"),
            ("t", "push tags"),
            ("u", "set upstream"),
            ("R", "change target"),
            ("Esc", "cancel"),
        ]
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![PaletteItem {
        name: "Push…",
        run: open,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;

    fn ctx_for(repo: crate::git::Repo) -> Ctx {
        Ctx::new(repo, Config::default())
    }

    #[test]
    fn open_handles_detached_and_empty() {
        let (_d, repo) = temp_repo();
        let ctx = ctx_for(repo.clone());
        assert!(matches!(open(&ctx), Action::Error(m) if m.contains("no commits")));
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        assert!(matches!(open(&ctx), Action::Error(m) if m.contains("no remotes")));
        repo.run(&["checkout", "-q", "--detach"]).unwrap();
        assert!(matches!(open(&ctx), Action::Error(m) if m.contains("detached")));
    }

    #[test]
    fn dialog_state_for_new_and_existing_targets() {
        let bare = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(bare.path())
            .status()
            .unwrap()
            .success());
        let (_d, repo) = temp_repo();
        repo.run(&["remote", "add", "origin", &bare.path().to_string_lossy()])
            .unwrap();
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        let ctx = ctx_for(repo.clone());
        let spec = PushSpec::for_branch(&repo, "main").unwrap();
        let mut p = PushPopup::new(&ctx, spec.clone());
        assert!(p.is_new);
        assert_eq!(p.commits.len(), 1);
        assert!(!p.nothing_to_push());
        p.ensure_files(&ctx);
        assert_eq!(p.files, vec![('A', "a.txt".to_string(), None)]);
        remote::push(&repo, &spec).unwrap();
        p.load(&ctx);
        assert!(!p.is_new);
        assert!(p.commits.is_empty());
        assert!(p.nothing_to_push());
        assert!(matches!(p.push(), Action::Toast(m) if m == "Nothing to push"));
        p.spec.tags = true;
        assert!(!p.nothing_to_push());
        assert_eq!(Focus::Commits.next(false), Focus::Files);
        assert_eq!(Focus::Commits.next(true), Focus::Button);
    }
}
