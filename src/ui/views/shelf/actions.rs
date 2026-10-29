//! Stash, shelf and patch operations as [`Action`]s: dialogs, confirmations and outcome toasts.
//! Shared by the Stash & Shelf tab, its context menus and the action palette.

use std::path::{Path, PathBuf};

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::patch::{self, ApplyOutcome, PatchFileOutcome};
use crate::git::shelf::{self, ShelfEntry, UnshelveOutcome};
use crate::git::stash::{self, StashApplyOutcome, StashEntry, StashOptions};
use crate::git::status::StatusEntry;
use crate::git::Repo;
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup};
use crate::ui::views::Tab;

use super::form::{CheckItem, FormPopup};
use super::preview::PatchPreviewPopup;

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Toast + refresh + jump to the Conflicts tab.
fn conflicts_action(what: &str, files: &[String]) -> Action {
    Action::Batch(vec![
        Action::Toast(format!(
            "{what} with conflicts in {} — resolve them in the Conflicts tab",
            plural(files.len(), "file")
        )),
        Action::Refresh,
        Action::SwitchTab(Tab::Conflicts),
    ])
}

// ----- stash -------------------------------------------------------------------------------

/// "Stash changes" dialog: message, include untracked, keep index.
pub fn stash_dialog(ctx: &Ctx) -> Action {
    if ctx.repo.head_commit().is_none() {
        return Action::Error("Cannot stash in a repository without commits".into());
    }
    let popup = FormPopup::new("Stash Changes", "stash", |v, _ctx| {
        let opts = StashOptions {
            message: v.text.trim().to_string(),
            include_untracked: v.options[0],
            keep_index: v.options[1],
        };
        Action::spawn("Stashing changes", move |repo| {
            let out = stash::push(repo, &opts, &[])?;
            Ok(if out.contains("No local changes") {
                "No local changes to stash".into()
            } else {
                "Changes stashed".into()
            })
        })
    })
    .text("Message (optional)", "", false)
    .option("Include untracked files", 'u', true)
    .option("Keep index (leave staged changes in place)", 'i', false);
    Action::popup(popup)
}

/// "Apply stash" / "Pop stash" dialog with the "Restore index" option.
pub fn apply_stash_dialog(entry: &StashEntry, pop: bool) -> Action {
    let index = entry.index;
    let hash = entry.hash.clone();
    let title = format!(
        "{} {}: {}",
        if pop { "Pop" } else { "Apply" },
        entry.refname(),
        crate::util::truncate(&entry.message, 40)
    );
    let popup = FormPopup::new(title, if pop { "pop" } else { "apply" }, move |v, ctx| {
        let pop = v.options[0];
        let restore_index = v.options[1];
        run_unstash(&ctx.repo, index, &hash, pop, restore_index)
    })
    .option("Pop (drop the stash after applying)", 'p', pop)
    .option("Restore index (reinstate staged changes)", 'r', false);
    Action::popup(popup)
}

/// Apply or pop `stash@{index}` (which must still be the stash `hash`) and turn the outcome into
/// toasts / a tab switch (git runs in the background).
pub fn run_unstash(
    repo: &Repo,
    index: usize,
    hash: &str,
    pop: bool,
    restore_index: bool,
) -> Action {
    let hash = hash.to_string();
    let label = format!("{} stash", if pop { "Popping" } else { "Applying" });
    Action::run(
        repo,
        label,
        move |repo| {
            stash::ensure_unchanged(repo, index, &hash)?;
            if pop {
                stash::pop(repo, index, restore_index)
            } else {
                stash::apply(repo, index, restore_index)
            }
        },
        move |r| unstash_outcome_action(r, index, pop),
    )
}

fn unstash_outcome_action(r: anyhow::Result<StashApplyOutcome>, index: usize, pop: bool) -> Action {
    match r {
        Ok(StashApplyOutcome::Clean) => Action::Batch(vec![
            Action::Toast(format!(
                "{} stash@{{{index}}}",
                if pop { "Popped" } else { "Applied" }
            )),
            Action::Refresh,
        ]),
        Ok(StashApplyOutcome::Conflicts(files)) => {
            let what = if pop {
                "Stash applied (and kept)"
            } else {
                "Stash applied"
            };
            conflicts_action(what, &files)
        }
        Err(e) => Action::Batch(vec![Action::Error(format!("{e:#}")), Action::Refresh]),
    }
}

/// Palette "Unstash (pop latest)".
pub fn pop_latest(ctx: &Ctx) -> Action {
    match stash::list(&ctx.repo) {
        Ok(l) => match l.first() {
            Some(latest) => run_unstash(&ctx.repo, latest.index, &latest.hash, true, false),
            None => Action::Toast("There are no stashes".into()),
        },
        Err(e) => Action::error(format!("{e:#}")),
    }
}

/// Drop one stash after confirmation (refused if the stash list changed in the meantime).
pub fn drop_stash(entry: &StashEntry) -> Action {
    let index = entry.index;
    let hash = entry.hash.clone();
    let msg = format!(
        "Drop {} \"{}\"?\nThis cannot be undone.",
        entry.refname(),
        entry.message
    );
    Action::popup(
        ConfirmPopup::new("Drop Stash", msg, move |_| drop_stash_job(index, hash)).danger(),
    )
}

/// Background job dropping `stash@{index}` if it is still the stash `hash`.
fn drop_stash_job(index: usize, hash: String) -> Action {
    Action::spawn("Dropping stash", move |repo| {
        stash::ensure_unchanged(repo, index, &hash)?;
        stash::drop(repo, index)?;
        Ok(format!("Dropped stash@{{{index}}}"))
    })
}

/// Remove every stash after confirmation.
pub fn clear_stashes(ctx: &Ctx) -> Action {
    let n = stash::list(&ctx.repo).map(|l| l.len()).unwrap_or(0);
    if n == 0 {
        return Action::Toast("There are no stashes".into());
    }
    let msg = format!("Remove all {}?\nThis cannot be undone.", plural(n, "stash"));
    Action::popup(
        ConfirmPopup::new("Clear Stashes", msg, |_| {
            Action::spawn("Clearing stashes", |repo| {
                stash::clear(repo)?;
                Ok("All stashes removed".into())
            })
        })
        .danger(),
    )
}

/// Create a branch from a stash (`git stash branch`).
pub fn branch_from_stash(entry: &StashEntry) -> Action {
    let index = entry.index;
    let hash = entry.hash.clone();
    let initial = entry
        .branch
        .as_deref()
        .map(|b| format!("{b}-stash"))
        .unwrap_or_default();
    Action::popup(
        InputPopup::new(
            format!("New Branch from {}", entry.refname()),
            &initial,
            move |name, _| {
                let name = name.trim().to_string();
                Action::spawn("Creating branch from stash", move |repo| {
                    stash::ensure_unchanged(repo, index, &hash)?;
                    stash::branch(repo, &name, index)?;
                    Ok(format!("Created branch {name} from stash"))
                })
            },
        )
        .prompt(
            "Checks out a new branch at the stash's base commit, applies the stash and drops it.",
        ),
    )
}

// ----- shelf -------------------------------------------------------------------------------

/// "Shelve changes" dialog for `paths` (name, file checklist, keep in work tree).
pub fn shelve_dialog(ctx: &Ctx, paths: Vec<String>) -> Action {
    let st = match ctx.repo.status() {
        Ok(s) => s,
        Err(e) => return Action::error(format!("{e:#}")),
    };
    let entries: Vec<&StatusEntry> = st
        .entries
        .iter()
        .filter(|e| !e.is_ignored() && !e.submodule && shelf::path_selected(&e.path, &paths))
        .collect();
    if entries.is_empty() {
        return Action::Toast("No changes to shelve".into());
    }
    if let Some(c) = entries.iter().find(|e| e.is_conflicted()) {
        return Action::Error(format!(
            "{} has unresolved conflicts and cannot be shelved",
            c.path
        ));
    }
    let files: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    let items: Vec<CheckItem> = entries
        .iter()
        .map(|e| CheckItem {
            label: match &e.orig_path {
                Some(o) => format!("{o} → {}", e.path),
                None => e.path.clone(),
            },
            badge: if e.orig_path.is_some() {
                'R'
            } else {
                e.display_state().letter()
            },
            checked: true,
        })
        .collect();
    let popup = FormPopup::new("Shelve Changes", "shelve", move |v, _ctx| {
        let selected: Vec<String> = files
            .iter()
            .zip(&v.checked)
            .filter(|(_, c)| **c)
            .map(|(p, _)| p.clone())
            .collect();
        let keep = v.options[0];
        let name = v.text.trim().to_string();
        Action::spawn("Shelving changes", move |repo| {
            let e = shelf::shelve(repo, &name, &selected, keep)?;
            Ok(format!(
                "Shelved {} as \"{}\"",
                plural(e.meta.files.len(), "file"),
                e.meta.name
            ))
        })
    })
    .text("Name", &shelf::default_name(), true)
    .checklist("Files", items)
    .option("Keep changes in the work tree", 'w', false);
    Action::popup(popup)
}

/// Shelve every local change (palette / `S`).
pub fn shelve_all(ctx: &Ctx) -> Action {
    match ctx.repo.status() {
        Ok(st) => {
            let paths: Vec<String> = st
                .entries
                .iter()
                .filter(|e| !e.is_ignored())
                .map(|e| e.path.clone())
                .collect();
            if paths.is_empty() {
                Action::Toast("No local changes to shelve".into())
            } else {
                shelve_dialog(ctx, paths)
            }
        }
        Err(e) => Action::error(format!("{e:#}")),
    }
}

/// Unshelve checklist for an entry (pending files preselected).
pub fn unshelve_dialog(entry: &ShelfEntry) -> Action {
    let pending: Vec<String> = entry
        .pending_files()
        .iter()
        .map(|f| f.path.clone())
        .collect();
    let files: Vec<String> = entry.meta.files.iter().map(|f| f.path.clone()).collect();
    let items: Vec<CheckItem> = entry
        .meta
        .files
        .iter()
        .map(|f| CheckItem {
            label: match (
                &f.orig_path,
                entry.is_unshelved(&f.path) && !entry.is_applied(),
            ) {
                (Some(o), false) => format!("{o} → {}", f.path),
                (Some(o), true) => format!("{o} → {}  (unshelved)", f.path),
                (None, true) => format!("{}  (unshelved)", f.path),
                (None, false) => f.path.clone(),
            },
            badge: f.status,
            checked: pending.contains(&f.path),
        })
        .collect();
    let id = entry.id.clone();
    let title = format!(
        "Unshelve \"{}\"",
        crate::util::truncate(&entry.meta.name, 50)
    );
    let popup = FormPopup::new(title, "unshelve", move |v, ctx| {
        let selected: Vec<String> = files
            .iter()
            .zip(&v.checked)
            .filter(|(_, c)| **c)
            .map(|(p, _)| p.clone())
            .collect();
        unshelve_files(ctx, &id, selected)
    })
    .checklist("Files to unshelve", items);
    Action::popup(popup)
}

/// Unshelve `paths` of entry `id`, asking first when untracked copies would overwrite files.
pub fn unshelve_files(ctx: &Ctx, id: &str, paths: Vec<String>) -> Action {
    let entry = match shelf::get(&ctx.repo, id) {
        Ok(e) => e,
        Err(e) => return Action::error(format!("{e:#}")),
    };
    let paths = if paths.is_empty() {
        entry
            .pending_files()
            .iter()
            .map(|f| f.path.clone())
            .collect()
    } else {
        paths
    };
    let collisions = shelf::untracked_collisions(&ctx.repo, &entry, &paths);
    if collisions.is_empty() {
        return run_unshelve(&ctx.repo, id, &paths, false);
    }
    let shown: Vec<&str> = collisions.iter().take(3).map(String::as_str).collect();
    let more = if collisions.len() > 3 {
        format!(" and {} more", collisions.len() - 3)
    } else {
        String::new()
    };
    let title = format!(
        "{} already exist: {}{more}",
        plural(collisions.len(), "file"),
        shown.join(", ")
    );
    let (id1, p1) = (id.to_string(), paths.clone());
    let (id2, p2) = (id.to_string(), paths);
    Action::popup(MenuPopup::new(
        crate::util::truncate(&title, 76),
        vec![
            MenuItem::new("Overwrite existing files", "o", move |ctx| {
                run_unshelve(&ctx.repo, &id1, &p1, true)
            }),
            MenuItem::new("Skip existing files", "s", move |ctx| {
                run_unshelve(&ctx.repo, &id2, &p2, false)
            }),
            MenuItem::new("Cancel", "c", |_| Action::None),
        ],
    ))
}

fn run_unshelve(repo: &Repo, id: &str, paths: &[String], overwrite: bool) -> Action {
    let (id, paths) = (id.to_string(), paths.to_vec());
    Action::run(
        repo,
        "Unshelving",
        move |repo| shelf::unshelve(repo, &id, &paths, overwrite),
        |r| match r {
            Ok(o) => unshelve_outcome_action(&o),
            Err(e) => Action::Batch(vec![Action::Error(format!("{e:#}")), Action::Refresh]),
        },
    )
}

fn unshelve_outcome_action(o: &UnshelveOutcome) -> Action {
    if !o.conflicts.is_empty() {
        return conflicts_action("Unshelved", &o.conflicts);
    }
    let mut msg = format!("Unshelved {}", plural(o.applied, "file"));
    if !o.skipped.is_empty() {
        msg.push_str(&format!(", skipped {} that already exist", o.skipped.len()));
    }
    if o.entry_applied {
        msg.push_str(" — entry moved to recently unshelved");
    }
    Action::Batch(vec![Action::Toast(msg), Action::Refresh])
}

/// Palette "Unshelve…": choose an entry, then the file checklist.
pub fn choose_unshelve(ctx: &Ctx) -> Action {
    let entries = match shelf::list(&ctx.repo) {
        Ok(l) => l,
        Err(e) => return Action::error(format!("{e:#}")),
    };
    if entries.is_empty() {
        return Action::Toast("The shelf is empty".into());
    }
    let items: Vec<MenuItem> = entries
        .into_iter()
        .take(40)
        .map(|e| {
            let label = format!(
                "{}{}  ({}, {})",
                e.meta.name,
                if e.is_applied() { " [unshelved]" } else { "" },
                plural(e.meta.files.len(), "file"),
                crate::util::relative_time(e.meta.created)
            );
            MenuItem::new(label, "", move |_| unshelve_dialog(&e))
        })
        .collect();
    Action::popup(MenuPopup::new("Unshelve", items).filterable())
}

/// Delete a shelf entry after confirmation.
pub fn delete_shelf(entry: &ShelfEntry) -> Action {
    let id = entry.id.clone();
    let msg = format!(
        "Delete shelved changes \"{}\" ({})?\nThis cannot be undone.",
        entry.meta.name,
        plural(entry.meta.files.len(), "file")
    );
    Action::popup(
        ConfirmPopup::new("Delete Shelved Changes", msg, move |ctx| {
            Action::run_op(
                &ctx.repo,
                "Deleting shelf",
                "Shelved changes deleted",
                move |repo| shelf::delete(repo, &id),
            )
        })
        .danger(),
    )
}

/// Delete every "recently unshelved" entry after confirmation.
pub fn delete_applied(ctx: &Ctx) -> Action {
    let n = shelf::list(&ctx.repo)
        .map(|l| l.iter().filter(|e| e.is_applied()).count())
        .unwrap_or(0);
    if n == 0 {
        return Action::Toast("No unshelved entries to clean up".into());
    }
    Action::popup(
        ConfirmPopup::new(
            "Clean Up Shelf",
            format!(
                "Delete {n} {} that were already unshelved?",
                if n == 1 { "entry" } else { "entries" }
            ),
            |_| {
                Action::spawn("Cleaning up shelf", |repo| {
                    let n = shelf::delete_applied(repo)?;
                    Ok(format!("Deleted {n} unshelved entries"))
                })
            },
        )
        .danger(),
    )
}

/// Rename a shelf entry.
pub fn rename_shelf(entry: &ShelfEntry) -> Action {
    let id = entry.id.clone();
    Action::popup(InputPopup::new(
        "Rename Shelved Changes",
        &entry.meta.name,
        move |name, ctx| {
            Action::run_op(&ctx.repo, "Renaming shelf", "Renamed", move |repo| {
                shelf::rename(repo, &id, &name)
            })
        },
    ))
}

/// Export a shelf entry (including untracked files) as a patch file; the patch is built and
/// written in the background.
pub fn export_shelf(ctx: &Ctx, entry: &ShelfEntry) -> Action {
    let default = format!("{}.patch", patch::slug(&entry.meta.name));
    let id = entry.id.clone();
    save_patch_prompt(
        ctx,
        "Export Shelved Changes as Patch",
        &default,
        move |repo| shelf::export_patch(repo, &id),
    )
}

// ----- patches -----------------------------------------------------------------------------

/// Default location for new patch files: the repository root.
fn default_patch_path(ctx: &Ctx, file_name: &str) -> String {
    ctx.repo.root.join(file_name).to_string_lossy().into_owned()
}

/// Produces the text of a patch file (runs on a background thread).
pub type PatchProducer = Box<dyn FnOnce(&Repo) -> anyhow::Result<String> + Send>;

/// Ask for a path, confirm overwriting, then build the patch with `produce` and write it, both in
/// the background.
pub fn save_patch_prompt(
    ctx: &Ctx,
    title: &str,
    file_name: &str,
    produce: impl FnOnce(&Repo) -> anyhow::Result<String> + Send + 'static,
) -> Action {
    let initial = default_patch_path(ctx, file_name);
    let produce: PatchProducer = Box::new(produce);
    Action::popup(
        InputPopup::new(title, &initial, move |input, ctx| {
            save_patch_to(patch::resolve_user_path(&ctx.repo, &input), produce)
        })
        .prompt("Patch file path (relative paths are inside the repository, ~ is your home):"),
    )
}

/// Write the patch from `produce` to `path`, asking before overwriting an existing file.
fn save_patch_to(path: PathBuf, produce: PatchProducer) -> Action {
    if path.is_dir() {
        return Action::Error(format!("{} is a directory", path.display()));
    }
    if path.exists() {
        let msg = format!("{} already exists. Overwrite it?", path.display());
        return Action::popup(
            ConfirmPopup::new("Overwrite File", msg, move |_| {
                write_patch_job(path, produce)
            })
            .danger(),
        );
    }
    write_patch_job(path, produce)
}

/// Background job: build the patch and write it to `path`.
fn write_patch_job(path: PathBuf, produce: PatchProducer) -> Action {
    Action::spawn("Creating patch", move |repo| {
        let text = produce(repo)?;
        patch::write_patch(&path, &text)?;
        Ok(format!("Patch saved to {}", path.display()))
    })
}

/// "Create patch from local changes…".
/// Only the status check runs here; the patch (temporary index plus `git add` of every changed
/// path) is built in the background after the path was chosen, from the changes at that time.
pub fn create_local_patch(ctx: &Ctx) -> Action {
    if let Err(e) = patch::local_change_paths(&ctx.repo) {
        return Action::error(format!("{e:#}"));
    }
    let name = format!(
        "{}.patch",
        patch::slug(
            &ctx.repo
                .head_branch()
                .unwrap_or_else(|| "local-changes".into())
        )
    );
    save_patch_prompt(
        ctx,
        "Create Patch from Local Changes",
        &name,
        patch::local_changes_patch,
    )
}

/// "Create patch from commit…": ask for a revision first.
pub fn create_commit_patch_prompt(ctx: &Ctx) -> Action {
    if ctx.repo.head_commit().is_none() {
        return Action::Error("There are no commits yet".into());
    }
    Action::popup(
        InputPopup::new("Create Patch from Commit", "HEAD", |rev, ctx| {
            create_commit_patch(ctx, rev.trim())
        })
        .prompt("Revision (hash, branch, tag, HEAD~2…):"),
    )
}

/// Save `git format-patch -1 <rev>` to a file (the revision is resolved now, the patch is
/// generated in the background).
pub fn create_commit_patch(ctx: &Ctx, rev: &str) -> Action {
    let hash = match ctx.repo.rev_parse(rev) {
        Ok(h) => h,
        Err(_) => return Action::Error(format!("'{rev}' is not a commit")),
    };
    let info = ctx
        .repo
        .try_run(&["log", "-1", "--format=%h%x1f%s", &hash])
        .unwrap_or_default();
    let (short, subject) = info.trim().split_once('\u{1f}').unwrap_or(("commit", ""));
    let name = format!("{short}-{}.patch", patch::slug(subject));
    save_patch_prompt(ctx, "Save Commit Patch", &name, move |repo| {
        patch::commit_patch(repo, &hash)
    })
}

/// "Apply patch…": ask for the file, then show the preview.
pub fn apply_patch_prompt(_ctx: &Ctx) -> Action {
    Action::popup(
        InputPopup::new("Apply Patch", "", |input, ctx| {
            preview_patch(&patch::resolve_user_path(&ctx.repo, &input))
        })
        .prompt("Patch file (.patch/.diff, or a mailbox from git format-patch):"),
    )
}

/// Open the preview for a patch file; confirming applies it.
pub fn preview_patch(path: &Path) -> Action {
    if !path.is_file() {
        return Action::Error(format!("{} does not exist", path.display()));
    }
    match PatchPreviewPopup::load(path.to_path_buf()) {
        Ok(p) if p.is_empty() => Action::Error(format!("{} contains no changes", path.display())),
        Ok(p) => Action::popup(p),
        Err(e) => Action::error(format!("{e:#}")),
    }
}

/// Apply a patch file and report the outcome.
pub fn apply_patch_file(repo: &Repo, path: &Path) -> Action {
    let path = path.to_path_buf();
    Action::run(
        repo,
        "Applying patch",
        move |repo| patch::apply_patch_file(repo, &path),
        patch_outcome_action,
    )
}

fn patch_outcome_action(r: anyhow::Result<PatchFileOutcome>) -> Action {
    match r {
        Ok(PatchFileOutcome::Applied(ApplyOutcome::Clean)) => Action::Batch(vec![
            Action::Toast("Patch applied to the work tree".into()),
            Action::Refresh,
        ]),
        Ok(PatchFileOutcome::Applied(ApplyOutcome::Conflicts(files))) => {
            conflicts_action("Patch applied", &files)
        }
        Ok(PatchFileOutcome::Committed(n)) => Action::Batch(vec![
            Action::Toast(format!("Applied {}", plural(n, "commit"))),
            Action::Refresh,
        ]),
        Ok(PatchFileOutcome::AmStopped { conflicts, message }) => {
            let hint =
                "git am stopped. Resolve the problem, then use \"Continue applying patches\" or \
\"Abort applying patches\" from the actions palette (:).";
            let mut v = vec![
                Action::Error(format!("{message}\n\n{hint}")),
                Action::Refresh,
            ];
            if !conflicts.is_empty() {
                v.push(Action::SwitchTab(Tab::Conflicts));
            }
            Action::Batch(v)
        }
        Err(e) => Action::Batch(vec![Action::Error(format!("{e:#}")), Action::Refresh]),
    }
}

fn require_am(ctx: &Ctx) -> Option<Action> {
    (ctx.repo.state() != crate::git::RepoState::ApplyingMailbox)
        .then(|| Action::Toast("No patch application (git am) is in progress".into()))
}

/// `git am --continue`.
pub fn am_continue(ctx: &Ctx) -> Action {
    require_am(ctx).unwrap_or_else(|| {
        Action::spawn("Continuing git am", |repo| {
            patch::am_continue(repo)?;
            Ok("Continued applying patches".into())
        })
    })
}

/// `git am --skip`.
pub fn am_skip(ctx: &Ctx) -> Action {
    require_am(ctx).unwrap_or_else(|| {
        Action::spawn("Skipping patch", |repo| {
            patch::am_skip(repo)?;
            Ok("Skipped the current patch".into())
        })
    })
}

/// `git am --abort` after confirmation.
pub fn am_abort(ctx: &Ctx) -> Action {
    require_am(ctx).unwrap_or_else(|| {
        Action::popup(
            ConfirmPopup::new(
                "Abort Applying Patches",
                "Abort git am and restore the branch to its state before the patches were applied?",
                |_| {
                    Action::spawn("Aborting git am", |repo| {
                        patch::am_abort(repo)?;
                        Ok("Aborted applying patches".into())
                    })
                },
            )
            .danger(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;

    fn has_switch_to_conflicts(a: &Action) -> bool {
        match a {
            Action::SwitchTab(Tab::Conflicts) => true,
            Action::Batch(v) => v.iter().any(has_switch_to_conflicts),
            _ => false,
        }
    }

    fn has_error(a: &Action) -> bool {
        match a {
            Action::Error(_) => true,
            Action::Batch(v) => v.iter().any(has_error),
            _ => false,
        }
    }

    #[test]
    fn unstash_conflicts_switch_tab() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "a.txt", "stash\n");
        stash::push(&repo, &StashOptions::default(), &[]).unwrap();
        commit_files(&repo, &[("a.txt", "other\n")], "c");
        let hash = stash::list(&repo).unwrap()[0].hash.clone();
        let a = run_unstash(&repo, 0, &hash, false, false);
        assert!(has_switch_to_conflicts(&a));
        assert!(has_error(&run_unstash(&repo, 5, &hash, false, false)));
    }

    #[test]
    fn dialogs_open_or_report() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        assert!(matches!(stash_dialog(&ctx), Action::Error(_)));
        assert!(matches!(shelve_all(&ctx), Action::Toast(_)));
        commit_files(&ctx.repo, &[("a.txt", "1\n")], "init");
        assert!(matches!(stash_dialog(&ctx), Action::Popup(_)));
        assert!(matches!(pop_latest(&ctx), Action::Toast(_)));
        assert!(matches!(clear_stashes(&ctx), Action::Toast(_)));
        assert!(matches!(choose_unshelve(&ctx), Action::Toast(_)));
        assert!(matches!(create_local_patch(&ctx), Action::Error(_)));
        write(&ctx.repo, "a.txt", "2\n");
        assert!(matches!(shelve_all(&ctx), Action::Popup(_)));
        assert!(matches!(
            shelve_dialog(&ctx, vec!["nope".into()]),
            Action::Toast(_)
        ));
        assert!(matches!(create_local_patch(&ctx), Action::Popup(_)));
        assert!(matches!(
            create_commit_patch(&ctx, "HEAD"),
            Action::Popup(_)
        ));
        assert!(matches!(
            create_commit_patch(&ctx, "nope"),
            Action::Error(_)
        ));
        assert!(matches!(am_continue(&ctx), Action::Toast(_)));
        assert!(matches!(
            preview_patch(&ctx.repo.root.join("missing.patch")),
            Action::Error(_)
        ));
    }

    #[test]
    fn unshelve_collision_menu_and_outcomes() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "u.txt", "shelved\n");
        let e = shelf::shelve(&repo, "x", &["u.txt".into()], false).unwrap();
        let ctx = Ctx::new(repo, Config::default());
        write(&ctx.repo, "u.txt", "different\n");
        assert!(matches!(
            unshelve_files(&ctx, &e.id, vec![]),
            Action::Popup(_)
        ));
        std::fs::remove_file(ctx.repo.path("u.txt")).unwrap();
        let a = unshelve_files(&ctx, &e.id, vec![]);
        assert!(matches!(a, Action::Batch(_)) && !has_error(&a));
        assert!(ctx.repo.path("u.txt").exists());
        let o = UnshelveOutcome {
            applied: 1,
            conflicts: vec!["a".into()],
            skipped: vec![],
            entry_applied: false,
        };
        assert!(has_switch_to_conflicts(&unshelve_outcome_action(&o)));
    }

    /// Run a background task's job like the app would; returns its message.
    fn run_job(a: Action, repo: &Repo) -> anyhow::Result<String> {
        match a {
            Action::Spawn(task) => (task.job)(repo),
            _ => panic!("expected a background task"),
        }
    }

    #[test]
    fn stash_ops_refuse_when_the_list_changed() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "a.txt", "first\n");
        stash::push(&repo, &StashOptions::default(), &[]).unwrap();
        let first = stash::list(&repo).unwrap().remove(0);
        // A new stash shifts the indexes before the user confirms.
        write(&repo, "a.txt", "second\n");
        stash::push(&repo, &StashOptions::default(), &[]).unwrap();
        assert!(run_job(drop_stash_job(first.index, first.hash.clone()), &repo).is_err());
        assert_eq!(stash::list(&repo).unwrap().len(), 2);
        assert!(has_error(&run_unstash(
            &repo,
            first.index,
            &first.hash,
            true,
            false
        )));
        assert_eq!(read_norm(&repo, "a.txt"), "1\n");
        // With the current index the stash shown is the one dropped.
        let now = stash::list(&repo).unwrap();
        let moved = now.iter().find(|s| s.hash == first.hash).unwrap();
        assert_eq!(moved.index, 1);
        run_job(drop_stash_job(moved.index, moved.hash.clone()), &repo).unwrap();
        let left = stash::list(&repo).unwrap();
        assert_eq!(left.len(), 1);
        assert_ne!(left[0].hash, first.hash);
    }

    fn read_norm(repo: &Repo, p: &str) -> String {
        std::fs::read_to_string(repo.path(p))
            .unwrap()
            .replace("\r\n", "\n")
    }

    #[test]
    fn patch_files_are_written_in_background_jobs() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "a.txt", "2\n");
        let file = repo.root.join("out dir").join("local.patch");
        let job = save_patch_to(file.clone(), Box::new(patch::local_changes_patch));
        assert!(!file.exists(), "nothing is written before the job runs");
        let msg = run_job(job, &repo).unwrap();
        assert!(msg.contains("local.patch"));
        assert!(std::fs::read_to_string(&file).unwrap().contains("+2"));
        // An existing file needs confirmation first; a directory is refused.
        assert!(matches!(
            save_patch_to(file.clone(), Box::new(patch::local_changes_patch)),
            Action::Popup(_)
        ));
        assert!(matches!(
            save_patch_to(repo.root.clone(), Box::new(patch::local_changes_patch)),
            Action::Error(_)
        ));
        // Errors of the producer surface as task errors and write nothing.
        let other = repo.root.join("none.patch");
        let failing = save_patch_to(other.clone(), Box::new(|_: &Repo| anyhow::bail!("boom")));
        assert!(run_job(failing, &repo).is_err());
        assert!(!other.exists());
    }

    #[test]
    fn patch_file_outcomes() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        write(&repo, "a.txt", "2\n");
        let text = patch::local_changes_patch(&repo).unwrap();
        let file = repo.git_dir.join("x.patch");
        patch::write_patch(&file, &text).unwrap();
        repo.run(&["checkout", "--", "a.txt"]).unwrap();
        assert!(!has_error(&apply_patch_file(&repo, &file)));
        assert!(has_error(&apply_patch_file(&repo, &file)));
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
    }
}
