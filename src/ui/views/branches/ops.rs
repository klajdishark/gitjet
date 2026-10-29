//! Branch and network operations as [`Action`]s, shared by the Branches tab, its context menu and the
//! action palette.

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::branch::{
    self, Branch, CheckoutBlockers, CheckoutTarget, Favorites, Integration, MergeMode, SmartOutcome,
};
use crate::git::remote::{self, PullMode, PushSpec};
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup, MessagePopup};
use crate::ui::views::Tab;

use super::compare::ComparePopup;
use super::new_branch::NewBranchPopup;

/// Toast + refresh.
pub fn done(msg: impl Into<String>) -> Action {
    Action::Batch(vec![Action::Toast(msg.into()), Action::Refresh])
}

/// Error + refresh.
pub fn fail(e: anyhow::Error) -> Action {
    Action::Batch(vec![Action::Error(format!("{e:#}")), Action::Refresh])
}

/// Point the user at the Conflicts tab after an operation stopped with conflicts.
pub fn conflicts(what: &str) -> Action {
    Action::Batch(vec![
        Action::Toast(format!(
            "{what} stopped with conflicts — resolve them in the Conflicts tab"
        )),
        Action::SwitchTab(Tab::Conflicts),
        Action::Refresh,
    ])
}

/// Turn a merge/rebase result into the matching action.
pub fn integration(r: anyhow::Result<Integration>, what: &str) -> Action {
    match r {
        Ok(Integration::Done(msg)) => done(msg),
        Ok(Integration::Conflicts) => conflicts(what),
        Err(e) => fail(e),
    }
}

fn current_name(ctx: &Ctx) -> String {
    ctx.repo.head_branch().unwrap_or_else(|| "HEAD".into())
}

// ----- checkout ------------------------------------------------------------------------------

/// Check out a branch; remote branches ask for the name of the local tracking branch.
pub fn checkout(ctx: &Ctx, b: &Branch) -> Action {
    if b.is_head {
        return Action::Toast(format!("Already on {}", b.name));
    }
    if ctx.config.branches.checkout_from_remote {
        return super::remote_checkout::open(ctx, b);
    }
    if b.is_remote() {
        return checkout_remote(ctx, b);
    }
    run_checkout(ctx, CheckoutTarget::Existing(b.name.clone()))
}

fn checkout_remote(ctx: &Ctx, b: &Branch) -> Action {
    let short = b.short_name().to_string();
    let remote_ref = b.name.clone();
    let prompt = if branch::local_exists(&ctx.repo, &short) {
        format!("A local branch '{short}' already exists: Enter checks it out,\nor type another name to create a new branch tracking {remote_ref}.")
    } else {
        format!("Creates a local branch tracking {remote_ref} and checks it out.")
    };
    let popup = InputPopup::new(
        format!("Checkout {remote_ref}"),
        &short,
        move |name, ctx| {
            let name = name.trim().to_string();
            if branch::local_exists(&ctx.repo, &name) {
                run_checkout(ctx, CheckoutTarget::Existing(name))
            } else {
                run_checkout(
                    ctx,
                    CheckoutTarget::Tracking {
                        local: name,
                        remote_ref,
                    },
                )
            }
        },
    )
    .prompt(prompt);
    Action::popup(popup)
}

/// Run a checkout; when local changes are in the way, offer smart / force checkout.
pub fn run_checkout(ctx: &Ctx, target: CheckoutTarget) -> Action {
    let job_target = target.clone();
    Action::run(
        &ctx.repo,
        format!("Checking out {}", target.label()),
        move |repo| job_target.run(repo, false),
        move |r| match r {
            Ok(()) => done(format!("Checked out {}", target.label())),
            Err(e) => {
                let msg = format!("{e:#}");
                if branch::is_local_changes_error(&msg) {
                    local_changes_menu(target, msg)
                } else {
                    fail(e)
                }
            }
        },
    )
}

pub(super) fn local_changes_menu(target: CheckoutTarget, err: String) -> Action {
    let label = target.label().to_string();
    let blockers = CheckoutBlockers::parse(&err);
    let mut items = Vec::new();
    if blockers.smart_checkout_possible() {
        let smart = target.clone();
        items.push(MenuItem::new(
            "Smart checkout (stash, checkout, unstash)",
            "s",
            move |ctx| smart_checkout(ctx, &smart),
        ));
    }
    if !blockers.untracked.is_empty() {
        let text = untracked_explanation(&label, &blockers.untracked);
        let n = blockers.untracked.len();
        items.push(MenuItem::new(
            format!(
                "Show the {n} untracked file{} in the way",
                if n == 1 { "" } else { "s" }
            ),
            "v",
            move |_| Action::popup(MessagePopup::new("Untracked files block checkout", text)),
        ));
    }
    let force_label = if blockers.untracked.is_empty() {
        "Force checkout (discard local changes)".to_string()
    } else {
        format!(
            "Force checkout (discard local changes, overwrite {} untracked file{})",
            blockers.untracked.len(),
            if blockers.untracked.len() == 1 {
                ""
            } else {
                "s"
            }
        )
    };
    let confirm_msg = force_checkout_message(&label, &blockers);
    items.push(MenuItem::new(force_label, "f", move |_| {
        Action::popup(
            ConfirmPopup::new("Force checkout", confirm_msg, move |ctx| {
                let label = target.label().to_string();
                Action::run(
                    &ctx.repo,
                    format!("Force checking out {label}"),
                    move |repo| target.run(repo, true),
                    move |r| match r {
                        Ok(()) => done(format!("Checked out {label} (local changes discarded)")),
                        Err(e) => fail(e),
                    },
                )
            })
            .danger(),
        )
    }));
    items.push(MenuItem::new("Cancel", "Esc", |_| Action::None));
    let title = if blockers.smart_checkout_possible() {
        format!("Local changes would be overwritten by checkout of {label}")
    } else {
        format!("Untracked files block checkout of {label}")
    };
    Action::popup(MenuPopup::new(title, items))
}

/// Why smart checkout cannot help with untracked files, and which files they are.
fn untracked_explanation(label: &str, untracked: &[String]) -> String {
    let mut s = format!(
        "These untracked files exist in {label} too, so checking it out would overwrite them.\n\
         Stashing cannot help: the files would collide again when the stash is re-applied.\n\
         Move or remove them and check out again, or force checkout to overwrite them.\n\n"
    );
    for p in untracked {
        s.push_str(&format!("  {p}\n"));
    }
    s
}

/// Danger prompt text for a force checkout, naming every file that will be lost (capped).
fn force_checkout_message(label: &str, b: &CheckoutBlockers) -> String {
    const MAX: usize = 10;
    let list = |title: &str, paths: &[String]| -> String {
        if paths.is_empty() {
            return String::new();
        }
        let mut s = format!("\n{title}:\n");
        for p in paths.iter().take(MAX) {
            s.push_str(&format!("  {p}\n"));
        }
        if paths.len() > MAX {
            s.push_str(&format!("  … and {} more\n", paths.len() - MAX));
        }
        s
    };
    let what = if b.untracked.is_empty() {
        "Discard your local changes"
    } else {
        "Discard your local changes and OVERWRITE the untracked files below"
    };
    format!(
        "{what} and check out {label}?\n{}{}",
        list("Local changes that will be lost", &b.changed),
        list("Untracked files that will be overwritten", &b.untracked)
    )
    .trim_end()
    .to_string()
}

fn smart_checkout(ctx: &Ctx, target: &CheckoutTarget) -> Action {
    let label = target.label().to_string();
    let target = target.clone();
    Action::run(
        &ctx.repo,
        format!("Smart checkout of {label} (stash, checkout, unstash)"),
        move |repo| branch::smart_checkout(repo, &target),
        move |r| {
            match r {
            Ok(SmartOutcome::Clean) => {
                done(format!("Checked out {label} (local changes restored)"))
            }
            Ok(SmartOutcome::Conflicts) => Action::Batch(vec![
                Action::Toast(format!(
                    "Checked out {label}; restoring local changes conflicted (they are kept in the stash)"
                )),
                Action::SwitchTab(Tab::Conflicts),
                Action::Refresh,
            ]),
            Err(e) => fail(e),
        }
        },
    )
}

// ----- create / rename / delete --------------------------------------------------------------

/// "New branch" dialog starting at `start` (None = HEAD).
pub fn new_branch(start: Option<&Branch>) -> Action {
    Action::popup(NewBranchPopup::new(start))
}

/// Rename a local branch (favorites follow the rename).
pub fn rename(b: &Branch) -> Action {
    if b.is_remote() {
        return Action::Error(
            "Remote branches cannot be renamed here; check out and push under a new name".into(),
        );
    }
    let old = b.name.clone();
    let initial = old.clone();
    let popup = InputPopup::new(format!("Rename {old}"), &initial, move |new, ctx| {
        let new = new.trim().to_string();
        if new == old {
            return Action::None;
        }
        let (o, n) = (old.clone(), new.clone());
        Action::run(
            &ctx.repo,
            format!("Renaming {old} to {new}"),
            move |repo| {
                branch::rename_branch(repo, &o, &n)?;
                let mut fav = Favorites::load(repo);
                if fav.refs.remove(&format!("refs/heads/{o}")) {
                    fav.refs.insert(format!("refs/heads/{n}"));
                    let _ = fav.save(repo);
                }
                Ok(())
            },
            move |r: anyhow::Result<()>| match r {
                Ok(()) => done(format!("Renamed {old} to {new}")),
                Err(e) => fail(e),
            },
        )
    });
    Action::popup(popup)
}

/// Safe-delete a local branch (offering force delete when unmerged) or delete a remote branch.
pub fn delete(ctx: &Ctx, b: &Branch) -> Action {
    if let Some(remote) = b.remote.clone() {
        let name = b.short_name().to_string();
        let msg = format!("Delete branch '{name}' on remote '{remote}'?\nThis runs: git push {remote} --delete {name}");
        return Action::popup(
            ConfirmPopup::new("Delete remote branch", msg, move |_| {
                Action::spawn(format!("Delete {remote}/{name}"), move |repo| {
                    remote::delete_remote_branch(repo, &remote, &name)
                })
            })
            .danger(),
        );
    }
    if b.is_head {
        return Action::Error(format!(
            "Cannot delete the current branch '{}'; check out another branch first",
            b.name
        ));
    }
    let (name, short) = (b.name.clone(), b.short.clone());
    let job_name = name.clone();
    Action::run(
        &ctx.repo,
        format!("Deleting {name}"),
        move |repo| branch::delete_branch(repo, &job_name, false),
        move |r| match r {
            Ok(()) => done(format!("Deleted branch {name} (was {short})")),
            Err(e) if branch::is_not_merged_error(&format!("{e:#}")) => {
                let msg = format!(
                    "Branch '{name}' is not fully merged into the current branch.\nForce delete it? Its unmerged commits will only be reachable via the reflog (tip {short})."
                );
                Action::popup(
                    ConfirmPopup::new("Force delete branch", msg, move |ctx| {
                        let job_name = name.clone();
                        Action::run(
                            &ctx.repo,
                            format!("Force deleting {name}"),
                            move |repo| branch::delete_branch(repo, &job_name, true),
                            move |r| match r {
                                Ok(()) => done(format!("Deleted branch {name} (was {short})")),
                                Err(e) => fail(e),
                            },
                        )
                    })
                    .danger(),
                )
            }
            Err(e) => fail(e),
        },
    )
}

// ----- merge / rebase / compare --------------------------------------------------------------

/// Menu of merge flavors for merging `name` into the current branch.
pub fn merge_menu(ctx: &Ctx, name: &str) -> Action {
    let current = current_name(ctx);
    let keys = ["m", "n", "s", "f"];
    let items = MergeMode::ALL
        .iter()
        .zip(keys)
        .map(|(&mode, key)| {
            let name = name.to_string();
            MenuItem::new(mode.label(), key, move |ctx| {
                let job_name = name.clone();
                Action::run(
                    &ctx.repo,
                    format!("Merging {name}"),
                    move |repo| branch::merge(repo, &job_name, mode),
                    |r| integration(r, "Merge"),
                )
            })
        })
        .collect();
    Action::popup(MenuPopup::new(
        format!("Merge {name} into {current}"),
        items,
    ))
}

/// Rebase the current branch onto `name`.
pub fn rebase_onto(ctx: &Ctx, name: &str) -> Action {
    let job_name = name.to_string();
    Action::run(
        &ctx.repo,
        format!("Rebasing onto {name}"),
        move |repo| branch::rebase_onto(repo, &job_name),
        |r| integration(r, "Rebase"),
    )
}

/// Check out `name` and rebase it onto the current branch.
pub fn checkout_and_rebase(ctx: &Ctx, b: &Branch) -> Action {
    if b.is_head || b.is_remote() {
        return Action::Error(
            "Choose another local branch to check out and rebase onto the current one".into(),
        );
    }
    let onto = match ctx.repo.head_branch().or_else(|| ctx.repo.head_commit()) {
        Some(o) => o,
        None => return Action::Error("No commits yet".into()),
    };
    let name = b.name.clone();
    Action::run(
        &ctx.repo,
        format!("Checking out {name} and rebasing onto {onto}"),
        move |repo| branch::checkout_and_rebase(repo, &name, &onto),
        |r| integration(r, "Rebase"),
    )
}

/// Commits in the branch vs. the current branch.
pub fn compare(ctx: &Ctx, name: &str) -> Action {
    Action::popup(ComparePopup::new(ctx, name))
}

/// Diff between a branch and the working tree.
pub fn diff_with_worktree(name: &str) -> Action {
    diff_view::open(
        format!("{name} ↔ working tree"),
        DiffSource::RevToWorktree {
            rev: name.to_string(),
            paths: Vec::new(),
        },
    )
}

// ----- upstream / push -----------------------------------------------------------------------

/// Choose (or unset) the upstream of a local branch.
pub fn upstream_menu(ctx: &Ctx, b: &Branch) -> Action {
    if b.is_remote() {
        return Action::Error("Upstreams are set on local branches".into());
    }
    let all = match branch::list_branches(&ctx.repo) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let local = b.name.clone();
    let mut items = Vec::new();
    if let Some(up) = &b.upstream {
        let local = local.clone();
        items.push(MenuItem::new(
            format!("Unset upstream (currently {up})"),
            "",
            move |ctx| {
                let msg = format!("Unset upstream of {local}");
                Action::run_op(&ctx.repo, "Unsetting upstream", msg, move |repo| {
                    branch::unset_upstream(repo, &local)
                })
            },
        ));
    }
    for r in all.into_iter().filter(Branch::is_remote) {
        let local = local.clone();
        let current = b.upstream.as_deref() == Some(r.name.as_str());
        let label = if current {
            format!("{} (current)", r.name)
        } else {
            r.name.clone()
        };
        items.push(MenuItem::new(label, "", move |ctx| {
            let msg = format!("{local} now tracks {}", r.name);
            Action::run_op(&ctx.repo, "Setting upstream", msg, move |repo| {
                branch::set_upstream(repo, &local, &r.name)
            })
        }));
    }
    if items.is_empty() {
        return Action::Error("No remote branches: fetch first or push with set-upstream".into());
    }
    Action::popup(MenuPopup::new(format!("Upstream of {local}"), items).filterable())
}

/// Push dialog for the current branch, or a background push of another local branch.
pub fn push_branch(ctx: &Ctx, b: Option<&Branch>) -> Action {
    match b {
        Some(b) if !b.is_head && !b.is_remote() => {
            let name = b.name.clone();
            Action::spawn(format!("Push {name}"), move |repo| {
                let spec = PushSpec::for_branch(repo, &name)?;
                remote::push(repo, &spec)
            })
        }
        _ => crate::ui::views::push::open(ctx),
    }
}

// ----- network -------------------------------------------------------------------------------

/// Fetch all remotes (prune per config, tags) in the background.
pub fn fetch(ctx: &Ctx) -> Action {
    let prune = ctx.config.update.prune;
    Action::spawn("Fetch", move |repo| remote::fetch_all(repo, prune))
}

/// Pull the current branch with a given mode in the background.
pub fn pull(mode: PullMode) -> Action {
    Action::spawn(format!("Pull ({})", mode.label()), move |repo| {
        remote::pull(repo, mode)
    })
}

/// Menu: pull with merge / rebase / ff-only, or in the terminal.
pub fn pull_menu(ctx: &Ctx) -> Action {
    let target = remote::upstream_of(&ctx.repo, &current_name(ctx))
        .map(|u| format!(" from {}", u.short()))
        .unwrap_or_default();
    let items = vec![
        MenuItem::new("Pull, merge", "m", |_| pull(PullMode::Merge)),
        MenuItem::new("Pull, rebase", "r", |_| pull(PullMode::Rebase)),
        MenuItem::new("Pull, fast-forward only", "f", |_| pull(PullMode::FfOnly)),
        MenuItem::new("Pull (in terminal)", "t", pull_in_terminal),
    ];
    Action::popup(MenuPopup::new(
        format!("Pull {}{target}", current_name(ctx)),
        items,
    ))
}

/// Fetch, then merge/rebase the current branch onto its upstream (config `update.method`).
pub fn update_project(ctx: &Ctx) -> Action {
    let mode = PullMode::from_config(&ctx.config.update.method);
    let prune = ctx.config.update.prune;
    Action::spawn("Update project", move |repo| {
        remote::update_project(repo, mode, prune)
    })
}

/// `git fetch` on the real terminal (credential prompts work).
pub fn fetch_in_terminal(ctx: &Ctx) -> Action {
    let mut args = vec!["fetch", "--all", "--tags"];
    if ctx.config.update.prune {
        args.push("--prune");
    }
    Action::Suspend(remote::terminal_command(&ctx.repo, &args))
}

/// `git pull` on the real terminal.
pub fn pull_in_terminal(ctx: &Ctx) -> Action {
    let flag = match PullMode::from_config(&ctx.config.update.method) {
        PullMode::Merge => "--no-rebase",
        _ => "--rebase",
    };
    Action::Suspend(remote::terminal_command(
        &ctx.repo,
        &["pull", "--autostash", flag],
    ))
}

/// `git push` of the current branch on the real terminal.
pub fn push_in_terminal(ctx: &Ctx) -> Action {
    let Some(name) = ctx.repo.head_branch() else {
        return Action::Error("HEAD is detached; check out a branch to push".into());
    };
    match PushSpec::for_branch(&ctx.repo, &name) {
        Ok(spec) => {
            let args = spec.terminal_args();
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            Action::Suspend(remote::terminal_command(&ctx.repo, &refs))
        }
        Err(e) => Action::Error(format!("{e:#}")),
    }
}

/// Network entries shared by context menus.
pub fn network_items() -> Vec<MenuItem> {
    vec![
        MenuItem::new("Fetch all remotes", "f", fetch),
        MenuItem::new("Pull…", "p", pull_menu),
        MenuItem::new("Update project", "U", update_project),
        MenuItem::new("Fetch (in terminal)", "", fetch_in_terminal),
        MenuItem::new("Pull (in terminal)", "", pull_in_terminal),
        MenuItem::new("Push (in terminal)", "", push_in_terminal),
    ]
}

// ----- palette pickers -----------------------------------------------------------------------

/// Filterable chooser over branches matching `keep`, running `then` on the chosen one.
pub fn pick(
    ctx: &Ctx,
    title: &str,
    keep: fn(&Branch) -> bool,
    then: fn(&Ctx, &Branch) -> Action,
) -> Action {
    let all = match branch::list_branches(&ctx.repo) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let items: Vec<MenuItem> = all
        .into_iter()
        .filter(keep)
        .map(|b| {
            let label = if b.is_head {
                format!("{} (current)", b.name)
            } else {
                b.name.clone()
            };
            MenuItem::new(label, "", move |ctx| then(ctx, &b))
        })
        .collect();
    if items.is_empty() {
        return Action::Toast("No matching branches".into());
    }
    Action::popup(MenuPopup::new(title, items).filterable())
}
