//! Interactive rebase editor and rebase-in-progress
//! controls (continue / skip / abort / status), plus quick commands for the action palette.

mod editor;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::rebase::{self, Outcome, Progress, ROOT};
use crate::git::RepoState;
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup, MessagePopup};
use crate::ui::views::Tab;
use crate::ui::PaletteItem;

use editor::RebaseEditor;

/// Open the interactive rebase editor for commits `base..HEAD` (`base` is exclusive, e.g. `<hash>^`
/// to include `<hash>`; `--root` for the whole history).
///
/// While a rebase is already in progress this shows the continue/skip/abort controls instead.
pub fn open(ctx: &Ctx, base: &str) -> Action {
    if let Some(a) = blocked(ctx) {
        return a;
    }
    match RebaseEditor::load(ctx, base) {
        Ok(ed) => Action::popup(ed),
        Err(e) => Action::error(format!("{e:#}")),
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Interactive rebase…",
            run: prompt_interactive,
        },
        PaletteItem {
            name: "Squash last N commits…",
            run: prompt_squash,
        },
        PaletteItem {
            name: "Reword commit…",
            run: prompt_reword,
        },
        PaletteItem {
            name: "Continue rebase",
            run: continue_rebase,
        },
        PaletteItem {
            name: "Abort rebase",
            run: abort_rebase,
        },
        PaletteItem {
            name: "Skip commit",
            run: skip_commit,
        },
        PaletteItem {
            name: "Rebase status…",
            run: status_menu,
        },
    ]
}

/// Turn the result of start/continue/skip into toasts, a tab switch on conflicts, and a refresh.
pub(crate) fn outcome_action(r: anyhow::Result<Outcome>) -> Action {
    match r {
        Ok(o @ Outcome::Conflicts { .. }) => Action::Batch(vec![
            Action::Toast(o.message()),
            Action::SwitchTab(Tab::Conflicts),
            Action::Refresh,
        ]),
        Ok(o @ Outcome::Stopped { .. }) => {
            Action::Batch(vec![Action::Error(o.message()), Action::Refresh])
        }
        Ok(o) => Action::Batch(vec![Action::Toast(o.message()), Action::Refresh]),
        Err(e) => Action::Batch(vec![Action::Error(format!("{e:#}")), Action::Refresh]),
    }
}

/// Why a new rebase cannot start right now (another operation in progress, no commits), as an action.
fn blocked(ctx: &Ctx) -> Option<Action> {
    match ctx.repo.state() {
        RepoState::Clean => {}
        RepoState::Rebasing => return Some(status_menu(ctx)),
        other => {
            return Some(Action::Error(format!(
                "Cannot rebase while {} is in progress — finish or abort it first",
                other.label().to_lowercase()
            )))
        }
    }
    if ctx.repo.head_commit().is_none() {
        return Some(Action::Toast(
            "Nothing to rebase: the repository has no commits yet".into(),
        ));
    }
    None
}

/// Suggested base: the upstream when there are unpushed commits, else the last 10 commits (or root).
fn default_base(ctx: &Ctx) -> String {
    let repo = &ctx.repo;
    let ahead = repo
        .try_run(&["rev-list", "--count", "@{upstream}..HEAD"])
        .and_then(|s| s.trim().parse::<usize>().ok());
    if ahead.is_some_and(|n| n > 0) {
        return "@{upstream}".into();
    }
    let total = repo
        .try_run(&["rev-list", "--first-parent", "--count", "HEAD"])
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if total <= 10 {
        ROOT.into()
    } else {
        "HEAD~10".into()
    }
}

fn prompt_interactive(ctx: &Ctx) -> Action {
    if let Some(a) = blocked(ctx) {
        return a;
    }
    Action::popup(
        InputPopup::new("Interactive rebase", &default_base(ctx), |base, ctx| {
            open(ctx, base.trim())
        })
        .prompt(
            "Rebase the commits after this revision (it is not included).\n\
             Branch, tag, hash, HEAD~N, @{upstream} — or --root for the whole history.",
        ),
    )
}

fn prompt_squash(ctx: &Ctx) -> Action {
    if let Some(a) = blocked(ctx) {
        return a;
    }
    Action::popup(
        InputPopup::new("Squash last N commits", "2", |text, ctx| {
            match text.trim().parse::<usize>() {
                Ok(n) => open_squash(ctx, n),
                Err(_) => Action::Error(format!("'{}' is not a number of commits", text.trim())),
            }
        })
        .prompt("How many commits, counting back from HEAD, should become one?"),
    )
}

/// The editor with the last `n` commits preset to squash, plus the combined-message dialog.
fn open_squash(ctx: &Ctx, n: usize) -> Action {
    let loaded = rebase::squash_base(&ctx.repo, n).and_then(|base| RebaseEditor::load(ctx, &base));
    match loaded {
        Ok(mut ed) => {
            ed.preset_squash();
            let message = ed.message_popup(ctx);
            Action::popup(ed).then(message)
        }
        Err(e) => Action::error(format!("{e:#}")),
    }
}

fn prompt_reword(ctx: &Ctx) -> Action {
    if let Some(a) = blocked(ctx) {
        return a;
    }
    Action::popup(
        InputPopup::new("Reword commit", "HEAD", |rev, ctx| {
            open_reword(ctx, rev.trim())
        })
        .prompt("Commit whose message to change (hash, HEAD~N, tag...)"),
    )
}

/// The editor for `rev..HEAD` with the message dialog for `rev` already open.
fn open_reword(ctx: &Ctx, rev: &str) -> Action {
    let loaded = rebase::base_for_commit(&ctx.repo, rev)
        .and_then(|(base, id)| RebaseEditor::load(ctx, &base).map(|ed| (ed, id)));
    match loaded {
        Ok((mut ed, id)) => {
            ed.select_commit(&id);
            let message = ed.message_popup(ctx);
            Action::popup(ed).then(message)
        }
        Err(e) => Action::error(format!("{e:#}")),
    }
}

fn not_rebasing(what: &str) -> Action {
    Action::Toast(format!("{what}: no rebase in progress"))
}

fn progress_suffix(p: &Option<Progress>) -> String {
    match p.as_ref().map(Progress::describe) {
        Some(d) if !d.is_empty() => format!(" ({d})"),
        _ => String::new(),
    }
}

/// `git rebase --continue`; switches to the Conflicts tab if the next step conflicts.
fn continue_rebase(ctx: &Ctx) -> Action {
    if !rebase::is_rebasing(&ctx.repo) {
        return not_rebasing("Continue rebase");
    }
    outcome_action(rebase::continue_rebase(&ctx.repo))
}

fn abort_rebase(ctx: &Ctx) -> Action {
    if !rebase::is_rebasing(&ctx.repo) {
        return not_rebasing("Abort rebase");
    }
    let p = rebase::progress(&ctx.repo);
    let branch = p
        .as_ref()
        .and_then(|p| p.head_name.clone())
        .unwrap_or_else(|| "HEAD".into());
    Action::popup(
        ConfirmPopup::new(
            "Abort rebase",
            format!(
                "Abort the rebase{}?\n\n{branch} goes back to where it was before the rebase started; \
                 commits already rewritten and resolved conflicts are discarded.",
                progress_suffix(&p)
            ),
            |_| {
                Action::spawn("Abort rebase", |repo| {
                    rebase::abort(repo).map(|_| "Rebase aborted".into())
                })
            },
        )
        .danger(),
    )
}

fn skip_commit(ctx: &Ctx) -> Action {
    if !rebase::is_rebasing(&ctx.repo) {
        return not_rebasing("Skip commit");
    }
    let p = rebase::progress(&ctx.repo);
    let which = p
        .as_ref()
        .and_then(|p| {
            let sha = p.stopped_sha.as_deref()?;
            let short = sha.get(..7).unwrap_or(sha);
            Some(match &p.stopped_subject {
                Some(s) => format!("{short} \"{s}\""),
                None => short.to_string(),
            })
        })
        .unwrap_or_else(|| "the current commit".into());
    Action::popup(
        ConfirmPopup::new(
            "Skip commit",
            format!("Skip {which}?\n\nIts changes are left out of the rebased branch and the rebase goes on."),
            |ctx| outcome_action(rebase::skip(&ctx.repo)),
        )
        .danger(),
    )
}

/// Controls for a rebase in progress: continue, skip, abort, conflicts, details.
fn status_menu(ctx: &Ctx) -> Action {
    if !rebase::is_rebasing(&ctx.repo) {
        return not_rebasing("Rebase status");
    }
    let p = rebase::progress(&ctx.repo);
    let conflicts = ctx
        .repo
        .status()
        .map(|s| s.conflicted().count())
        .unwrap_or(0);
    let mut title = "Rebase in progress".to_string();
    title.push_str(&progress_suffix(&p));
    let details = status_popup(ctx, p, conflicts);
    let items = vec![
        MenuItem::new("Continue rebase", "c", continue_rebase),
        MenuItem::new("Skip commit…", "s", skip_commit),
        MenuItem::new("Abort rebase…", "a", abort_rebase),
        MenuItem::new(format!("Resolve conflicts ({conflicts})"), "g", |_| {
            Action::SwitchTab(Tab::Conflicts)
        })
        .enabled(conflicts > 0),
        MenuItem::new("Show rebase details", "i", move |_| details),
    ];
    Action::popup(MenuPopup::new(title, items))
}

fn status_popup(ctx: &Ctx, p: Option<Progress>, conflicts: usize) -> Action {
    let t = &ctx.theme;
    let p = p.unwrap_or_default();
    let row = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<12}"), t.key()),
            Span::styled(v, t.text()),
        ])
    };
    let mut lines = vec![
        row(
            "Branch",
            p.head_name
                .clone()
                .unwrap_or_else(|| "detached HEAD".into()),
        ),
        row("Onto", p.onto.clone().unwrap_or_else(|| "?".into())),
        row(
            "Step",
            if p.total > 0 {
                format!("{} of {}", p.step.min(p.total), p.total)
            } else {
                "?".into()
            },
        ),
    ];
    if let Some(sha) = &p.stopped_sha {
        let short = sha.get(..7).unwrap_or(sha);
        lines.push(row(
            "Stopped at",
            format!("{short} {}", p.stopped_subject.clone().unwrap_or_default()),
        ));
    }
    let state = if conflicts > 0 {
        format!("{conflicts} conflicted file(s) — resolve them in the Conflicts tab, then continue")
    } else if p.editing {
        "stopped for editing — amend the commit, then continue".into()
    } else {
        "ready to continue".into()
    };
    lines.push(row("State", state));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("Remaining steps ({})", p.remaining.len()),
        t.title(),
    ));
    for l in p.remaining.iter().take(40) {
        lines.push(Line::styled(format!("  {l}"), Style::default().fg(t.fg)));
    }
    if p.remaining.len() > 40 {
        lines.push(Line::styled(
            format!("  … {} more", p.remaining.len() - 40),
            t.dimmed(),
        ));
    }
    Action::popup(MessagePopup::from_lines("Rebase details", lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;

    fn ctx_with(n: usize) -> (TempDir, Ctx) {
        let (d, repo) = temp_repo();
        for k in 1..=n {
            commit_files(&repo, &[(&format!("f{k}.txt"), "x\n")], &format!("c{k}"));
        }
        (d, Ctx::new(repo, Config::default()))
    }

    #[test]
    fn entry_points_without_rebase() {
        let (_d, ctx) = ctx_with(0);
        assert!(matches!(open(&ctx, ROOT), Action::Toast(_)));
        assert!(matches!(prompt_squash(&ctx), Action::Toast(_)));
        let (_d, ctx) = ctx_with(3);
        assert!(matches!(open(&ctx, ROOT), Action::Popup(_)));
        assert!(matches!(open(&ctx, "HEAD"), Action::Error(_)));
        assert!(matches!(continue_rebase(&ctx), Action::Toast(_)));
        assert!(matches!(abort_rebase(&ctx), Action::Toast(_)));
        assert!(matches!(skip_commit(&ctx), Action::Toast(_)));
        assert!(matches!(status_menu(&ctx), Action::Toast(_)));
        assert_eq!(default_base(&ctx), ROOT);
        assert!(matches!(open_squash(&ctx, 2), Action::Batch(_)));
        assert!(matches!(open_squash(&ctx, 9), Action::Error(_)));
        assert!(matches!(open_reword(&ctx, "HEAD~1"), Action::Batch(_)));
        assert!(matches!(open_reword(&ctx, "nope"), Action::Error(_)));
        assert_eq!(palette().len(), 7);
    }

    #[test]
    fn default_base_prefers_upstream_with_unpushed_commits() {
        let (_d, ctx) = ctx_with(12);
        assert_eq!(default_base(&ctx), "HEAD~10");
        let h = ctx.repo.rev_parse("HEAD~2").unwrap();
        ctx.repo
            .run(&["update-ref", "refs/remotes/origin/main", &h])
            .unwrap();
        ctx.repo
            .run(&[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ])
            .unwrap();
        ctx.repo
            .run(&["config", "branch.main.remote", "origin"])
            .unwrap();
        ctx.repo
            .run(&["config", "branch.main.merge", "refs/heads/main"])
            .unwrap();
        assert_eq!(default_base(&ctx), "@{upstream}");
    }

    #[test]
    fn outcome_actions() {
        let a = outcome_action(Ok(Outcome::Conflicts {
            paths: vec!["a".into()],
            progress: Progress::default(),
        }));
        let Action::Batch(v) = a else { panic!("batch") };
        assert!(v
            .iter()
            .any(|a| matches!(a, Action::SwitchTab(Tab::Conflicts))));
        let Action::Batch(v) = outcome_action(Ok(Outcome::Done)) else {
            panic!("batch")
        };
        assert!(matches!(v[0], Action::Toast(_)));
        let Action::Batch(v) = outcome_action(Err(anyhow::anyhow!("boom"))) else {
            panic!("batch")
        };
        assert!(matches!(v[0], Action::Error(_)));
    }

    #[cfg(unix)]
    #[test]
    fn controls_while_rebasing() {
        let (_d, ctx) = ctx_with(3);
        let base = ctx.repo.rev_parse("HEAD~2").unwrap();
        let todo = ctx.repo.data_dir().unwrap().join("test-todo");
        let next = ctx.repo.rev_parse("HEAD~1").unwrap();
        let last = ctx.repo.rev_parse("HEAD").unwrap();
        std::fs::write(&todo, format!("edit {next}\npick {last}\n")).unwrap();
        let mut c = ctx.repo.command();
        c.args(["rebase", "-i", &base]).env(
            "GIT_SEQUENCE_EDITOR",
            format!("cp {}", crate::util::sh_quote(&todo.to_string_lossy())),
        );
        ctx.repo.run_cmd(c, &["rebase"]).unwrap();
        assert!(rebase::is_rebasing(&ctx.repo));
        assert!(matches!(open(&ctx, ROOT), Action::Popup(_)), "status menu");
        assert!(matches!(abort_rebase(&ctx), Action::Popup(_)), "confirm");
        assert!(matches!(skip_commit(&ctx), Action::Popup(_)), "confirm");
        assert!(
            matches!(prompt_interactive(&ctx), Action::Popup(_)),
            "status menu"
        );
        rebase::abort(&ctx.repo).unwrap();
        assert!(!rebase::is_rebasing(&ctx.repo));
    }
}
