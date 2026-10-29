//! Every operation of the Repository tab, as menu items with single-key accelerators.
//!
//! The view's key handling and its `.` context menu both come from [`menu_items`], so a key always does
//! exactly what the menu says.

use std::path::PathBuf;
use std::rc::Rc;

use ratatui::text::Line;

use super::{Data, Section};
use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::admin::{self, ConfigScope, IgnoreFile, WorktreeCheckout};
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::popups::{ConfirmPopup, InputPopup, MenuItem, MenuPopup, MessagePopup};

/// Items for `section` with the row `idx` selected (None when the list is empty).
pub(super) fn menu_items(
    section: Section,
    data: &Data,
    idx: Option<usize>,
    ctx: &Ctx,
) -> Vec<MenuItem> {
    match section {
        Section::Tags => tag_items(data, idx),
        Section::Remotes => remote_items(data, idx),
        Section::Worktrees => worktree_items(data, idx),
        Section::Submodules => submodule_items(data, idx, ctx),
        Section::Ignore => ignore_items(data, idx),
        Section::Config => config_items(data, idx),
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

// ----- remote picker -----------------------------------------------------------------------------

type RemoteFn = Rc<dyn Fn(String, &Ctx) -> Action>;

/// Run `f` with a remote: the only one directly, otherwise let the user pick (default first).
pub(super) fn with_remote(ctx: &Ctx, title: &str, f: RemoteFn) -> Action {
    let remotes = match admin::list_remotes(&ctx.repo) {
        Ok(r) => r,
        Err(e) => return Action::error(format!("{e:#}")),
    };
    if remotes.is_empty() {
        return Action::Error("No remotes configured — add one in Repository → Remotes (n)".into());
    }
    if remotes.len() == 1 {
        return f(remotes[0].name.clone(), ctx);
    }
    let default = admin::default_remote(&ctx.repo, &remotes);
    let mut names: Vec<String> = remotes.into_iter().map(|r| r.name).collect();
    if let Some(d) = default {
        names.retain(|n| *n != d);
        names.insert(0, d);
    }
    let items = names
        .into_iter()
        .map(|n| {
            let f = f.clone();
            MenuItem::new(n.clone(), "", move |ctx| f(n, ctx))
        })
        .collect();
    Action::popup(MenuPopup::new(title, items))
}

// ----- tags --------------------------------------------------------------------------------------

/// "New tag…": name → revision → optional message (annotated when given).
pub(super) fn new_tag(rev: &str) -> Action {
    let rev = rev.to_string();
    Action::popup(
        InputPopup::new("New tag", "", move |name, _| {
            let name = name.trim().to_string();
            Action::popup(
                InputPopup::new(format!("Tag {name} at revision"), &rev, move |rev, _| {
                    Action::popup(
                        InputPopup::new(format!("Message for {name}"), "", move |msg, ctx| {
                            let kind = if msg.trim().is_empty() {
                                "lightweight"
                            } else {
                                "annotated"
                            };
                            let ok = format!("Created {kind} tag {name}");
                            let name = name.clone();
                            Action::run_op(&ctx.repo, "Creating tag", ok, move |repo| {
                                admin::create_tag(repo, &name, &rev, &msg)
                            })
                        })
                        .allow_empty()
                        .prompt(
                            "Leave empty for a lightweight tag; a message creates an annotated tag",
                        ),
                    )
                })
                .prompt("Branch, tag or commit hash"),
            )
        })
        .prompt("Tag name, e.g. v1.2.0"),
    )
}

fn tag_items(data: &Data, idx: Option<usize>) -> Vec<MenuItem> {
    let tag = idx.and_then(|i| data.tags.get(i)).map(|t| t.name.clone());
    let has = tag.is_some();
    let t = move || tag.clone().unwrap_or_default();
    let (t1, t2, t3, t4, t5, t6) = (t(), t(), t(), t(), t(), t());
    vec![
        MenuItem::new("Show tagged commit", "Enter", move |_| {
            diff_view::open(
                format!("Tag {t1}"),
                DiffSource::Commit {
                    rev: format!("refs/tags/{t1}"),
                    paths: Vec::new(),
                },
            )
        })
        .enabled(has),
        MenuItem::new("Compare with work tree", "w", move |_| {
            diff_view::open(
                format!("Tag {t2} vs work tree"),
                DiffSource::RevToWorktree {
                    rev: format!("refs/tags/{t2}"),
                    paths: Vec::new(),
                },
            )
        })
        .enabled(has),
        MenuItem::new("New tag…", "n", |_| new_tag("HEAD")),
        MenuItem::new("Checkout tag (detached HEAD)…", "c", move |_| {
            Action::popup(ConfirmPopup::new(
                "Checkout tag",
                format!("Check out tag {t3}? HEAD will be detached; create a branch to keep new commits."),
                move |ctx| {
                    Action::run_op(&ctx.repo, "Checking out tag", format!("Checked out {t3} (detached HEAD)"), move |repo| admin::checkout_tag(repo, &t3))
                },
            ))
        })
        .enabled(has),
        MenuItem::new("Push tag", "p", move |ctx| {
            let name = t4.clone();
            with_remote(
                ctx,
                "Push tag to",
                Rc::new(move |remote, _| {
                    let name = name.clone();
                    Action::spawn(format!("Push tag {name}"), move |repo| {
                        admin::push_tag(repo, &remote, &name)
                    })
                }),
            )
        })
        .enabled(has),
        MenuItem::new("Push all tags", "P", |ctx| {
            with_remote(
                ctx,
                "Push all tags to",
                Rc::new(|remote, _| Action::spawn("Push tags", move |repo| admin::push_all_tags(repo, &remote))),
            )
        }),
        MenuItem::new("Delete local tag…", "D", move |_| {
            Action::popup(
                ConfirmPopup::new("Delete tag", format!("Delete local tag {t5}?"), move |ctx| {
                    Action::run_op(&ctx.repo, "Deleting tag", format!("Deleted tag {t5}"), move |repo| admin::delete_tag(repo, &t5))
                })
                .danger(),
            )
        })
        .enabled(has),
        MenuItem::new("Delete tag on remote…", "X", move |ctx| {
            let name = t6.clone();
            with_remote(
                ctx,
                "Delete tag on",
                Rc::new(move |remote, _| {
                    let name = name.clone();
                    Action::popup(
                        ConfirmPopup::new(
                            "Delete remote tag",
                            format!("Delete tag {name} on {remote}? Others who fetched it keep their copy."),
                            move |_| {
                                Action::spawn(format!("Delete remote tag {name}"), move |repo| {
                                    admin::delete_remote_tag(repo, &remote, &name)
                                })
                            },
                        )
                        .danger(),
                    )
                }),
            )
        })
        .enabled(has),
    ]
}

// ----- remotes -----------------------------------------------------------------------------------

/// "Add remote…": name → URL.
pub(super) fn add_remote(ctx: &Ctx) -> Action {
    let has_origin = admin::list_remotes(&ctx.repo)
        .map(|r| r.iter().any(|r| r.name == "origin"))
        .unwrap_or(false);
    let initial = if has_origin { "" } else { "origin" };
    Action::popup(
        InputPopup::new("Add remote", initial, |name, _| {
            let name = name.trim().to_string();
            Action::popup(
                InputPopup::new(format!("URL of {name}"), "", move |url, ctx| {
                    Action::run_op(
                        &ctx.repo,
                        "Adding remote",
                        format!("Added remote {name}"),
                        move |repo| admin::add_remote(repo, &name, &url),
                    )
                })
                .prompt("https://host/owner/repo.git, git@host:owner/repo.git or a local path"),
            )
        })
        .prompt("Remote name"),
    )
}

fn remote_items(data: &Data, idx: Option<usize>) -> Vec<MenuItem> {
    let remote = idx.and_then(|i| data.remotes.get(i)).cloned();
    let has = remote.is_some();
    let name = remote.as_ref().map(|r| r.name.clone()).unwrap_or_default();
    let fetch_url = remote
        .as_ref()
        .map(|r| r.fetch_url.clone())
        .unwrap_or_default();
    let push_url = remote
        .as_ref()
        .and_then(|r| r.push_urls.first().cloned())
        .unwrap_or_else(|| fetch_url.clone());
    let n = move || name.clone();
    let (n2, n3, n4, n5, n6, n7) = (n(), n(), n(), n(), n(), n());
    vec![
        MenuItem::new("Add remote…", "n", add_remote),
        MenuItem::new("Edit URL…", "e", move |_| edit_url(n2, fetch_url, false)).enabled(has),
        MenuItem::new("Set push URL…", "u", move |_| edit_url(n3, push_url, true)).enabled(has),
        MenuItem::new("Rename…", "R", move |_| {
            let old = n4.clone();
            Action::popup(InputPopup::new(format!("Rename remote {old}"), &n4, move |new, ctx| {
                let new = new.trim().to_string();
                Action::run_op(&ctx.repo, "Renaming remote", format!("Renamed {old} to {new}"), move |repo| admin::rename_remote(repo, &old, &new))
            }))
        })
        .enabled(has),
        MenuItem::new("Remove…", "D", move |_| {
            Action::popup(
                ConfirmPopup::new(
                    "Remove remote",
                    format!("Remove remote {n5}? Its remote-tracking branches and settings are deleted."),
                    move |ctx| {
                        Action::run_op(&ctx.repo, "Removing remote", format!("Removed remote {n5}"), move |repo| admin::remove_remote(repo, &n5))
                    },
                )
                .danger(),
            )
        })
        .enabled(has),
        MenuItem::new("Fetch", "f", move |ctx| {
            let prune = ctx.config.update.prune;
            Action::spawn(format!("Fetch {n6}"), move |repo| admin::fetch_remote(repo, &n6, prune))
        })
        .enabled(has),
        MenuItem::new("Fetch all remotes", "F", |ctx| {
            let prune = ctx.config.update.prune;
            Action::spawn("Fetch all", move |repo| admin::fetch_all(repo, prune))
        })
        .enabled(!data.remotes.is_empty()),
        MenuItem::new("Prune stale remote branches", "P", move |_| {
            Action::spawn(format!("Prune {n7}"), move |repo| admin::prune_remote(repo, &n7))
        })
        .enabled(has),
    ]
}

fn edit_url(name: String, current: String, push: bool) -> Action {
    let title = if push {
        format!("Push URL of {name}")
    } else {
        format!("URL of {name}")
    };
    Action::popup(InputPopup::new(title, &current, move |url, ctx| {
        let what = if push { "push URL" } else { "URL" };
        Action::run_op(
            &ctx.repo,
            "Updating remote URL",
            format!("Updated {what} of {name}"),
            move |repo| admin::set_remote_url(repo, &name, &url, push),
        )
    }))
}

// ----- worktrees ---------------------------------------------------------------------------------

/// "Add worktree…": path → checkout kind → branch/revision → background `git worktree add`.
pub(super) fn add_worktree(ctx: &Ctx) -> Action {
    let parent = ctx
        .repo
        .root
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.repo.root.clone());
    let default = parent.join(format!("{}-worktree", ctx.repo.name()));
    Action::popup(
        InputPopup::new(
            "Add worktree",
            &default.to_string_lossy(),
            move |input, _| {
                let path = admin::resolve_dir(&input, &parent)
                    .to_string_lossy()
                    .into_owned();
                let base_name = admin::resolve_dir(&input, &parent)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (p1, p2, p3) = (path.clone(), path.clone(), path);
                Action::popup(MenuPopup::new(
                    "Check out in the new worktree",
                    vec![
                        MenuItem::new("New branch…", "b", move |_| {
                            Action::popup(InputPopup::new(
                                "New branch name",
                                &base_name,
                                move |branch, _| {
                                    let branch = branch.trim().to_string();
                                    Action::popup(
                                        InputPopup::new(
                                            format!("Start {branch} at"),
                                            "HEAD",
                                            move |base, _| {
                                                spawn_add(
                                                    p1,
                                                    WorktreeCheckout::NewBranch { branch, base },
                                                )
                                            },
                                        )
                                        .prompt("Branch, tag or commit"),
                                    )
                                },
                            ))
                        }),
                        MenuItem::new("Existing branch…", "e", move |ctx| pick_branch(ctx, p2)),
                        MenuItem::new("Detached HEAD at revision…", "d", move |_| {
                            Action::popup(InputPopup::new(
                                "Detached at revision",
                                "HEAD",
                                move |rev, _| spawn_add(p3, WorktreeCheckout::Detached(rev)),
                            ))
                        }),
                    ],
                ))
            },
        )
        .prompt("Directory of the new worktree (relative paths are next to this repository)"),
    )
}

fn pick_branch(ctx: &Ctx, path: String) -> Action {
    let branches = match admin::local_branches(&ctx.repo) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return Action::Error("There are no local branches yet".into()),
        Err(e) => return Action::error(format!("{e:#}")),
    };
    let busy: Vec<String> = admin::list_worktrees(&ctx.repo)
        .map(|w| w.into_iter().filter_map(|w| w.branch).collect())
        .unwrap_or_default();
    let items = branches
        .into_iter()
        .map(|b| {
            let checked_out = busy.contains(&b);
            let p = path.clone();
            let label = if checked_out {
                format!("{b} (checked out elsewhere)")
            } else {
                b.clone()
            };
            MenuItem::new(label, "", move |_| {
                spawn_add(p, WorktreeCheckout::Existing(b))
            })
            .enabled(!checked_out)
        })
        .collect();
    Action::popup(MenuPopup::new("Existing branch", items).filterable())
}

fn spawn_add(path: String, how: WorktreeCheckout) -> Action {
    Action::spawn("Add worktree", move |repo| {
        admin::add_worktree(repo, &path, &how)?;
        Ok(format!(
            "Created worktree {path} — select it and press Enter to open"
        ))
    })
}

fn worktree_items(data: &Data, idx: Option<usize>) -> Vec<MenuItem> {
    let wt = idx.and_then(|i| data.worktrees.get(i)).cloned();
    let is_current = idx.is_some() && idx == data.current_worktree;
    let removable = wt.as_ref().is_some_and(|w| !w.is_main) && !is_current;
    let openable = wt.as_ref().is_some_and(|w| w.prunable.is_none() && !w.bare) && !is_current;
    let locked = wt.as_ref().is_some_and(|w| w.locked.is_some());
    let path = wt.as_ref().map(|w| w.path.clone()).unwrap_or_default();
    let (p1, p2, p3, p4) = (path.clone(), path.clone(), path.clone(), path);
    let lock_label = if locked { "Unlock" } else { "Lock…" };
    vec![
        MenuItem::new("Open worktree", "Enter", move |_| Action::SwitchRepo(p1)).enabled(openable),
        MenuItem::new("Add worktree…", "n", add_worktree),
        MenuItem::new("Remove worktree…", "D", move |_| remove_worktree(p2, locked, false)).enabled(removable),
        MenuItem::new("Force remove worktree (discard its changes)…", "X", move |_| {
            remove_worktree(p4, locked, true)
        })
        .enabled(removable),
        MenuItem::new(lock_label, "l", move |ctx| {
            if locked {
                return Action::run_op(&ctx.repo, "Unlocking worktree", "Worktree unlocked", move |repo| admin::unlock_worktree(repo, &p3));
            }
            Action::popup(
                InputPopup::new("Lock worktree", "", move |reason, ctx| {
                    Action::run_op(&ctx.repo, "Locking worktree", "Worktree locked", move |repo| admin::lock_worktree(repo, &p3, &reason))
                })
                .allow_empty()
                .prompt("Optional reason (e.g. \"on a removable drive\"); locked worktrees are never pruned"),
            )
        })
        .enabled(wt.as_ref().is_some_and(|w| !w.is_main)),
        MenuItem::new("Prune stale worktrees", "P", |_| {
            Action::spawn("Prune worktrees", admin::prune_worktrees)
        }),
    ]
}

/// Confirm, then remove the worktree in the background (deleting a big directory and checking a
/// big worktree for changes must not freeze the UI). Without `force`, a worktree with changes is
/// left alone and the error points to the force variant.
fn remove_worktree(path: PathBuf, locked: bool, force: bool) -> Action {
    if locked {
        return Action::Toast("The worktree is locked — unlock it first (l)".into());
    }
    let shown = path.display().to_string();
    let (title, msg) = if force {
        (
            "Force remove worktree",
            format!("Force-remove worktree {shown}?\nIts directory is deleted with all uncommitted changes and untracked files."),
        )
    } else {
        (
            "Remove worktree",
            format!(
                "Remove worktree {shown}? Its directory is deleted; the branch is kept.\nIf it has uncommitted changes nothing is removed (use X to force)."
            ),
        )
    };
    Action::popup(
        ConfirmPopup::new(title, msg, move |_| {
            Action::spawn("Remove worktree", move |repo| {
                if !force && admin::worktree_is_dirty(&path) {
                    anyhow::bail!(
                        "{shown} has uncommitted changes or untracked files; nothing was removed. \
                         Use \"Force remove worktree\" (X) to discard them."
                    );
                }
                admin::remove_worktree(repo, &path, force)?;
                Ok(format!("Removed worktree {shown}"))
            })
        })
        .danger(),
    )
}

// ----- submodules --------------------------------------------------------------------------------

/// "Update submodules" (`update --init --recursive`) in the background.
pub(super) fn update_submodules() -> Action {
    Action::spawn("Update submodules", admin::submodule_update)
}

/// "Add submodule…": URL → path → background `git submodule add`.
pub(super) fn add_submodule() -> Action {
    Action::popup(
        InputPopup::new("Add submodule", "", |url, _| {
            let url = url.trim().to_string();
            let default = crate::util::repo_name_from_url(&url);
            Action::popup(
                InputPopup::new("Submodule path", &default, move |path, _| {
                    let path = path.trim().replace('\\', "/");
                    Action::spawn("Add submodule", move |repo| {
                        admin::submodule_add(repo, &url, &path)
                    })
                })
                .prompt("Directory inside this repository"),
            )
        })
        .prompt("Repository URL"),
    )
}

fn submodule_items(data: &Data, idx: Option<usize>, ctx: &Ctx) -> Vec<MenuItem> {
    let sub = idx.and_then(|i| data.submodules.get(i)).cloned();
    let path = sub.as_ref().map(|s| s.path.clone()).unwrap_or_default();
    let abs = ctx.repo.path(&path);
    let initialized = sub.is_some() && abs.join(".git").exists();
    let any = !data.submodules.is_empty();
    let p1 = path.clone();
    vec![
        MenuItem::new("Open submodule", "Enter", move |_| Action::SwitchRepo(abs)).enabled(initialized),
        MenuItem::new("Init", "i", |_| {
            Action::spawn("Init submodules", |repo| {
                admin::submodule_init(repo).map(|_| "Submodules initialized".into())
            })
        })
        .enabled(any),
        MenuItem::new("Update (init, recursive)", "u", |_| update_submodules()).enabled(any),
        MenuItem::new("Sync URLs", "s", |_| {
            Action::spawn("Sync submodule URLs", |repo| {
                admin::submodule_sync(repo).map(|_| "Submodule URLs synchronized".into())
            })
        })
        .enabled(any),
        MenuItem::new("Add submodule…", "n", |_| add_submodule()),
        MenuItem::new("Deinit…", "d", move |_| {
            Action::popup(
                ConfirmPopup::new(
                    "Deinit submodule",
                    format!("Unregister {p1} and remove its work tree? Local changes inside it are lost."),
                    move |_| {
                        Action::spawn("Deinit submodule", move |repo| {
                            admin::submodule_deinit(repo, &p1).map(|_| format!("Deinitialized {p1}"))
                        })
                    },
                )
                .danger(),
            )
        })
        .enabled(initialized),
    ]
}

// ----- ignore ------------------------------------------------------------------------------------

fn add_pattern(file: IgnoreFile) -> Action {
    Action::popup(
        InputPopup::new(
            format!("Add pattern to {}", file.label()),
            "",
            move |pattern, ctx| {
                let p = pattern.trim().to_string();
                Action::run_op(
                    &ctx.repo,
                    "Updating ignore file",
                    format!("Added {p} to {}", file.label()),
                    move |repo| admin::add_ignore_pattern(repo, file, &p),
                )
            },
        )
        .prompt("e.g. *.log, build/, /config.local — prefix ! to re-include"),
    )
}

/// "Check path": which rule ignores (or re-includes) a path.
fn check_path() -> Action {
    Action::popup(
        InputPopup::new("Check ignore rules", "", |path, ctx| {
            let path = path.trim().replace('\\', "/");
            match admin::check_ignore(&ctx.repo, &path) {
                Ok(None) => Action::Toast(format!("{path} is not ignored (no rule matches)")),
                Ok(Some(m)) => {
                    let verdict = if m.ignored() {
                        "IGNORED"
                    } else {
                        "NOT ignored (re-included)"
                    };
                    let text = format!(
                        "{path}\n\n{verdict}\n\nRule:   {}\nSource: {}\nLine:   {}",
                        m.pattern, m.source, m.line
                    );
                    Action::popup(MessagePopup::new("Ignore check", text))
                }
                Err(e) => Action::error(format!("{e:#}")),
            }
        })
        .prompt("Path relative to the repository root (need not exist)"),
    )
}

/// Maximum number of ignored files listed in the dialog.
const MAX_IGNORED_SHOWN: usize = 2000;

fn show_ignored(ctx: &Ctx) -> Action {
    let theme = ctx.theme.clone();
    Action::run(
        &ctx.repo,
        "Listing ignored files",
        |repo| repo.ignored_files(),
        move |r| ignored_popup(r, &theme),
    )
}

fn ignored_popup(r: anyhow::Result<Vec<String>>, theme: &crate::theme::Theme) -> Action {
    match r {
        Ok(files) if files.is_empty() => Action::Toast("No ignored files in the work tree".into()),
        Ok(files) => {
            let mut lines: Vec<Line<'static>> = vec![Line::styled(
                format!("{} ignored:", plural(files.len(), "file")),
                theme.title(),
            )];
            lines.extend(
                files
                    .iter()
                    .take(MAX_IGNORED_SHOWN)
                    .map(|f| Line::raw(f.clone())),
            );
            if files.len() > MAX_IGNORED_SHOWN {
                lines.push(Line::styled(
                    format!("… and {} more", files.len() - MAX_IGNORED_SHOWN),
                    theme.dimmed(),
                ));
            }
            Action::popup(MessagePopup::from_lines("Ignored files", lines))
        }
        Err(e) => Action::error(format!("{e:#}")),
    }
}

fn edit_file(ctx: &Ctx, file: IgnoreFile, line: Option<usize>) -> Action {
    let path = file.path(&ctx.repo);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, "") {
            return Action::error(format!("creating {}: {e}", path.display()));
        }
    }
    Action::edit(path, line)
}

fn ignore_items(data: &Data, idx: Option<usize>) -> Vec<MenuItem> {
    let rule = idx.and_then(|i| data.ignore.get(i)).cloned();
    let has = rule.is_some();
    let r1 = rule.clone();
    vec![
        MenuItem::new("Edit file at this rule", "Enter", move |ctx| match r1 {
            Some(r) => edit_file(ctx, r.file, Some(r.line)),
            None => Action::None,
        })
        .enabled(has),
        MenuItem::new("Add pattern to .gitignore…", "n", |_| {
            add_pattern(IgnoreFile::GitIgnore)
        }),
        MenuItem::new("Add pattern to .git/info/exclude…", "x", |_| {
            add_pattern(IgnoreFile::Exclude)
        }),
        MenuItem::new("Remove pattern…", "D", move |_| match rule {
            Some(r) => Action::popup(
                ConfirmPopup::new(
                    "Remove pattern",
                    format!(
                        "Remove \"{}\" (line {}) from {}?",
                        r.pattern,
                        r.line,
                        r.file.label()
                    ),
                    move |ctx| {
                        Action::run_op(
                            &ctx.repo,
                            "Updating ignore file",
                            format!("Removed {}", r.pattern),
                            move |repo| admin::remove_ignore_rule(repo, &r),
                        )
                    },
                )
                .danger(),
            ),
            None => Action::None,
        })
        .enabled(has),
        MenuItem::new("Check path…", "c", |_| check_path()),
        MenuItem::new("Show ignored files", "I", show_ignored),
        MenuItem::new("Edit .gitignore in editor", "e", |ctx| {
            edit_file(ctx, IgnoreFile::GitIgnore, None)
        }),
        MenuItem::new("Edit .git/info/exclude in editor", "E", |ctx| {
            edit_file(ctx, IgnoreFile::Exclude, None)
        }),
    ]
}

// ----- config ------------------------------------------------------------------------------------

fn edit_config(key: String, current: String, scope: ConfigScope) -> Action {
    let scope_name = match scope {
        ConfigScope::Local => "repository",
        ConfigScope::Global => "global",
    };
    let prompt = format!(
        "{}\nEmpty value removes the {scope_name} setting",
        super::config_hint(&key)
    );
    Action::popup(
        InputPopup::new(
            format!("{key} ({scope_name})"),
            &current,
            move |value, ctx| {
                let msg = if value.trim().is_empty() {
                    format!("Unset {scope_name} {key}")
                } else {
                    format!("Set {scope_name} {key} = {}", value.trim())
                };
                Action::run_op(&ctx.repo, "Updating config", msg, move |repo| {
                    admin::set_config(repo, scope, &key, &value)
                })
            },
        )
        .allow_empty()
        .prompt(prompt),
    )
}

fn config_items(data: &Data, idx: Option<usize>) -> Vec<MenuItem> {
    let entry = idx.and_then(|i| data.config.get(i)).cloned();
    let has = entry.is_some();
    let key = entry.as_ref().map(|e| e.key.clone()).unwrap_or_default();
    let local = entry.as_ref().and_then(|e| e.local.clone());
    let global = entry
        .as_ref()
        .and_then(|e| e.global.clone())
        .unwrap_or_default();
    let has_local = local.is_some();
    let local = local
        .or_else(|| entry.as_ref().and_then(|e| e.effective.clone()))
        .unwrap_or_default();
    let (k2, k3, k4) = (key.clone(), key.clone(), key);
    vec![
        MenuItem::new("Edit repository value…", "e", move |_| {
            edit_config(k2, local, ConfigScope::Local)
        })
        .enabled(has),
        MenuItem::new("Edit global value…", "g", move |_| {
            edit_config(k3, global, ConfigScope::Global)
        })
        .enabled(has),
        MenuItem::new("Unset repository value", "x", move |ctx| {
            Action::run_op(
                &ctx.repo,
                "Updating config",
                format!("Unset repository {k4}"),
                move |repo| admin::set_config(repo, ConfigScope::Local, &k4, ""),
            )
        })
        .enabled(has_local),
        MenuItem::new("Open repository config in editor", "o", |ctx| {
            let path = ctx.repo.common_dir.join("config");
            Action::edit(path, None)
        }),
    ]
}
