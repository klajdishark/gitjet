//! Branches: list local/remote with tracking info, create, checkout, rename, delete, merge, rebase onto,
//! favorites, recents.
//!
//! Everything here is local (no network); see [`super::remote`] for fetch/pull/push.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::log::{Commit, LogQuery};
use super::{Repo, RepoState};

/// A local or remote-tracking branch with its tip commit and tracking state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Display name: `main` for local branches, `origin/main` for remote-tracking ones
    pub name: String,
    /// Full ref, e.g. `refs/heads/main` or `refs/remotes/origin/main`
    pub full_ref: String,
    /// Remote name for remote-tracking branches (`origin`)
    pub remote: Option<String>,
    /// True for the branch HEAD points to
    pub is_head: bool,
    /// Upstream in short form (`origin/main`), local branches only
    pub upstream: Option<String>,
    /// The configured upstream no longer exists (`[gone]`)
    pub upstream_gone: bool,
    /// Commits on this branch not in its upstream
    pub ahead: u32,
    /// Commits in the upstream not on this branch
    pub behind: u32,
    /// Abbreviated hash of the tip commit
    pub short: String,
    /// Committer time of the tip (unix seconds)
    pub time: i64,
    /// Subject of the tip commit
    pub subject: String,
}

impl Branch {
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Branch name without the remote prefix (`origin/feature/x` → `feature/x`).
    pub fn short_name(&self) -> &str {
        match &self.remote {
            Some(r) => self
                .name
                .strip_prefix(r.as_str())
                .and_then(|s| s.strip_prefix('/'))
                .unwrap_or(&self.name),
            None => &self.name,
        }
    }
}

const FIELDS: usize = 9;
const BRANCH_FORMAT: &str = "--format=%(refname)%00%(HEAD)%00%(upstream:short)%00%(upstream:track)%00%(objectname:short)%00%(committerdate:unix)%00%(symref)%00%(upstream)%00%(contents:subject)";

/// All local branches, then remote-tracking branches (symbolic refs like `origin/HEAD` are skipped).
pub fn list_branches(repo: &Repo) -> Result<Vec<Branch>> {
    let remotes = super::remote::remotes(repo).unwrap_or_default();
    let out = repo.run(&["for-each-ref", BRANCH_FORMAT, "refs/heads", "refs/remotes"])?;
    Ok(parse_branches(&out, &remotes))
}

/// Parse `for-each-ref` output produced with [`BRANCH_FORMAT`]. `remotes` resolves remote names that
/// contain slashes.
pub fn parse_branches(out: &str, remotes: &[String]) -> Vec<Branch> {
    let mut v: Vec<Branch> = out
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            let f: Vec<&str> = line.splitn(FIELDS, '\0').collect();
            if f.len() < FIELDS || !f[6].is_empty() {
                return None;
            }
            let full_ref = f[0].to_string();
            let (name, remote) = match full_ref.strip_prefix("refs/heads/") {
                Some(n) => (n.to_string(), None),
                None => {
                    let n = full_ref.strip_prefix("refs/remotes/")?;
                    (n.to_string(), Some(remote_of(n, remotes)))
                }
            };
            let (ahead, behind, gone) = parse_track(f[3]);
            let upstream = if remote.is_none() && !f[7].is_empty() {
                Some(short_upstream(f[7], f[2]))
            } else {
                None
            };
            Some(Branch {
                name,
                full_ref,
                remote,
                is_head: f[1].trim() == "*",
                upstream,
                upstream_gone: gone,
                ahead,
                behind,
                short: f[4].to_string(),
                time: f[5].trim().parse().unwrap_or(0),
                subject: f[8].to_string(),
            })
        })
        .collect();
    v.sort_by(|a, b| {
        a.is_remote()
            .cmp(&b.is_remote())
            .then_with(|| a.name.cmp(&b.name))
    });
    v
}

/// `refs/remotes/origin/main` → `origin/main`; `refs/heads/x` (local upstream) → `x`.
fn short_upstream(full: &str, short: &str) -> String {
    full.strip_prefix("refs/remotes/")
        .or_else(|| full.strip_prefix("refs/heads/"))
        .map(str::to_string)
        .unwrap_or_else(|| short.to_string())
}

/// The remote a remote-tracking short name belongs to; longest matching remote wins.
fn remote_of(short: &str, remotes: &[String]) -> String {
    remotes
        .iter()
        .filter(|r| short.starts_with(&format!("{r}/")))
        .max_by_key(|r| r.len())
        .cloned()
        .unwrap_or_else(|| short.split('/').next().unwrap_or(short).to_string())
}

/// Parse `%(upstream:track)`: `[ahead 2, behind 1]`, `[ahead 2]`, `[behind 1]`, `[gone]` or empty.
pub fn parse_track(s: &str) -> (u32, u32, bool) {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    if s == "gone" {
        return (0, 0, true);
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in s.split(',') {
        let mut it = part.split_whitespace();
        match (it.next(), it.next().and_then(|n| n.parse().ok())) {
            (Some("ahead"), Some(n)) => ahead = n,
            (Some("behind"), Some(n)) => behind = n,
            _ => {}
        }
    }
    (ahead, behind, false)
}

/// Recently checked-out branch names (most recent first, unique), from HEAD's reflog.
pub fn recent_branches(repo: &Repo, limit: usize) -> Vec<String> {
    repo.try_run(&["reflog", "show", "-n", "1000", "--format=%gs", "HEAD", "--"])
        .map(|out| parse_recent_checkouts(&out, limit))
        .unwrap_or_default()
}

/// Extract targets of `checkout: moving from A to B` reflog subjects, unique, newest first.
pub fn parse_recent_checkouts(out: &str, limit: usize) -> Vec<String> {
    let mut res: Vec<String> = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.trim().strip_prefix("checkout: moving from ") else {
            continue;
        };
        let Some((_, to)) = rest.rsplit_once(" to ") else {
            continue;
        };
        let to = to.trim();
        if !to.is_empty() && !res.iter().any(|r| r == to) {
            res.push(to.to_string());
            if res.len() >= limit {
                break;
            }
        }
    }
    res
}

/// Favorite branches (full refs), persisted in `<data dir>/favorites.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorites {
    pub refs: BTreeSet<String>,
}

impl Default for Favorites {
    /// `main` and `master` are favorites until the user changes the list.
    fn default() -> Self {
        Self {
            refs: ["refs/heads/main", "refs/heads/master"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl Favorites {
    fn path(repo: &Repo) -> Result<PathBuf> {
        Ok(repo.data_dir()?.join("favorites.json"))
    }

    /// Load favorites; a missing or unreadable file yields the defaults.
    pub fn load(repo: &Repo) -> Favorites {
        Self::path(repo)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, repo: &Repo) -> Result<()> {
        let p = Self::path(repo)?;
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))
    }

    pub fn contains(&self, full_ref: &str) -> bool {
        self.refs.contains(full_ref)
    }

    /// Toggle and return the new state (true = now a favorite).
    pub fn toggle(&mut self, full_ref: &str) -> bool {
        if self.refs.remove(full_ref) {
            false
        } else {
            self.refs.insert(full_ref.to_string());
            true
        }
    }
}

/// Validate a new branch name with `git check-ref-format --branch`.
pub fn validate_name(repo: &Repo, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("branch name is empty");
    }
    repo.run(&["check-ref-format", "--branch", name])
        .map(|_| ())
        .map_err(|_| anyhow!("'{name}' is not a valid branch name"))
}

/// True if a local branch with this name exists.
pub fn local_exists(repo: &Repo, name: &str) -> bool {
    repo.try_run(&["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")])
        .is_some()
}

/// Check out an existing branch (or any revision).
pub fn checkout(repo: &Repo, name: &str, force: bool) -> Result<()> {
    let mut args = vec!["checkout"];
    if force {
        args.push("-f");
    }
    args.extend([name, "--"]);
    repo.run(&args).map(|_| ())
}

/// Create local branch `local` tracking remote-tracking branch `remote_ref` (`origin/x`) and check it out.
pub fn checkout_tracking(repo: &Repo, local: &str, remote_ref: &str, force: bool) -> Result<()> {
    validate_name(repo, local)?;
    let mut args = vec!["checkout"];
    if force {
        args.push("-f");
    }
    args.extend(["-b", local, "--track", remote_ref, "--"]);
    repo.run(&args).map(|_| ())
}

/// True when a checkout/merge failed because local changes would be overwritten.
pub fn is_local_changes_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("would be overwritten by")
        || m.contains("would be removed by")
        || m.contains("please commit your changes or stash them")
}

/// Files that made a checkout fail, parsed from git's error message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckoutBlockers {
    /// Tracked files with local changes ("Your local changes … would be overwritten")
    pub changed: Vec<String>,
    /// Untracked files the target branch tracks ("untracked working tree files would be overwritten")
    pub untracked: Vec<String>,
}

impl CheckoutBlockers {
    /// Parse git's checkout error. Files are the indented lines after each "would be overwritten" line.
    pub fn parse(msg: &str) -> CheckoutBlockers {
        let mut out = CheckoutBlockers::default();
        let mut into: Option<bool> = None; // Some(true) = untracked list
        for line in msg.lines() {
            let lower = line.to_lowercase();
            if lower.contains("would be overwritten by") || lower.contains("would be removed by") {
                into = Some(lower.contains("untracked"));
                continue;
            }
            let indented = line.starts_with('\t') || line.starts_with("    ");
            match into {
                Some(untracked) if indented && !line.trim().is_empty() => {
                    let path = line.trim().to_string();
                    if untracked {
                        out.untracked.push(path);
                    } else {
                        out.changed.push(path);
                    }
                }
                _ => into = None,
            }
        }
        out
    }

    /// True when only tracked changes are in the way, so stashing them lets the checkout proceed.
    pub fn smart_checkout_possible(&self) -> bool {
        self.untracked.is_empty()
    }
}

/// What to check out in [`smart_checkout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
    /// An existing local branch or revision
    Existing(String),
    /// A new local branch tracking a remote-tracking branch
    Tracking { local: String, remote_ref: String },
    /// Local branch `local` (created or reset) exactly at remote-tracking branch `remote_ref`,
    /// tracking it: "check out from the remote"
    FromRemote { local: String, remote_ref: String },
}

impl CheckoutTarget {
    pub fn label(&self) -> &str {
        match self {
            CheckoutTarget::Existing(n) => n,
            CheckoutTarget::Tracking { local, .. } | CheckoutTarget::FromRemote { local, .. } => {
                local
            }
        }
    }

    pub fn run(&self, repo: &Repo, force: bool) -> Result<()> {
        match self {
            CheckoutTarget::Existing(n) => checkout(repo, n, force),
            CheckoutTarget::Tracking { local, remote_ref } => {
                checkout_tracking(repo, local, remote_ref, force)
            }
            CheckoutTarget::FromRemote { local, remote_ref } => {
                validate_name(repo, local)?;
                let mut args = vec!["checkout"];
                if force {
                    args.push("-f");
                }
                args.extend(["-B", local, "--track", remote_ref, "--"]);
                repo.run(&args).map(|_| ())
            }
        }
    }
}

/// The remote branch a checkout takes a branch from (see `[branches] checkout_from_remote`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSource {
    pub remote: String,
    /// Branch name on the remote
    pub branch: String,
    /// Local branch name
    pub local: String,
}

impl RemoteSource {
    /// Remote-tracking branch in short form (`origin/main`).
    pub fn remote_ref(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// Where to check out `b` from: a remote branch is its own source (local name: its short name);
/// a local branch uses its upstream, else the same name on a remote (`origin` first). None for a
/// branch that exists only locally.
pub fn remote_source(repo: &Repo, b: &Branch, all: &[Branch]) -> Option<RemoteSource> {
    if let Some(remote) = &b.remote {
        return Some(RemoteSource {
            remote: remote.clone(),
            branch: b.short_name().to_string(),
            local: b.short_name().to_string(),
        });
    }
    if let Some(up) = crate::git::remote::upstream_of(repo, &b.name) {
        return Some(RemoteSource {
            remote: up.remote,
            branch: up.branch,
            local: b.name.clone(),
        });
    }
    let mut same_name: Vec<&Branch> = all
        .iter()
        .filter(|r| r.is_remote() && r.short_name() == b.name)
        .collect();
    same_name.sort_by_key(|r| r.remote.as_deref() != Some("origin"));
    same_name.first().map(|r| RemoteSource {
        remote: r.remote.clone().unwrap_or_default(),
        branch: b.name.clone(),
        local: b.name.clone(),
    })
}

/// Fetch only `src`'s branch, updating its remote-tracking branch (no tags).
pub fn fetch_one(repo: &Repo, src: &RemoteSource) -> Result<()> {
    let refspec = format!(
        "+refs/heads/{}:refs/remotes/{}/{}",
        src.branch, src.remote, src.branch
    );
    crate::git::remote::run_net(repo, &["fetch", "--no-tags", &src.remote, &refspec])
        .map(|_| ())
        .map_err(|e| crate::git::remote::auth_hint(e, "Fetch"))
}

/// Commits on local branch `local` that are not in `remote_ref`, with the local tip. None when
/// there is no such local branch.
pub fn commits_only_local(repo: &Repo, local: &str, remote_ref: &str) -> Option<(u32, String)> {
    let tip = repo.rev_parse(&format!("refs/heads/{local}")).ok()?;
    let n = repo
        .try_run(&["rev-list", "--count", &format!("{remote_ref}..{tip}")])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    Some((n, tip))
}

/// Result of a smart checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartOutcome {
    /// Checked out and local changes restored cleanly
    Clean,
    /// Checked out; restoring local changes conflicted (the stash entry is kept)
    Conflicts,
}

/// Smart checkout: stash local changes, check out, re-apply the changes.
pub fn smart_checkout(repo: &Repo, target: &CheckoutTarget) -> Result<SmartOutcome> {
    let msg = format!("gitjet smart checkout of {}", target.label());
    let stash = stash_push(repo, &msg)?;
    if let Err(e) = target.run(repo, false) {
        if let Some(sha) = &stash {
            let _ = repo
                .run(&["stash", "apply", "--index", sha])
                .map(|_| drop_stash(repo, sha));
        }
        return Err(
            e.context("checkout failed even after stashing local changes (they were restored)")
        );
    }
    let Some(sha) = stash else {
        return Ok(SmartOutcome::Clean);
    };
    match repo.run(&["stash", "apply", sha.as_str()]) {
        Ok(_) => {
            drop_stash(repo, &sha)?;
            Ok(SmartOutcome::Clean)
        }
        Err(_) if has_conflicts(repo) => Ok(SmartOutcome::Conflicts),
        Err(e) => Err(e.context(format!(
            "checked out {}, but restoring local changes failed; they are kept in the stash",
            target.label()
        ))),
    }
}

/// Stash tracked changes; returns the stash commit, or None when there was nothing to stash.
fn stash_push(repo: &Repo, message: &str) -> Result<Option<String>> {
    let before = repo
        .try_run(&["rev-parse", "-q", "--verify", "refs/stash"])
        .map(|s| s.trim().to_string());
    repo.run(&["stash", "push", "-m", message])?;
    let after = repo
        .try_run(&["rev-parse", "-q", "--verify", "refs/stash"])
        .map(|s| s.trim().to_string());
    Ok(if after != before { after } else { None })
}

/// Drop the stash entry whose commit is `sha` (found by hash, so concurrent stashes are safe).
fn drop_stash(repo: &Repo, sha: &str) -> Result<()> {
    let list = repo.run(&["stash", "list", "--format=%H"])?;
    if let Some(i) = list.lines().position(|l| l.trim() == sha) {
        repo.run(&["stash", "drop", "-q", &format!("stash@{{{i}}}")])?;
    }
    Ok(())
}

/// True when the index has unmerged entries.
pub fn has_conflicts(repo: &Repo) -> bool {
    repo.try_run(&["diff", "--name-only", "--diff-filter=U"])
        .is_some_and(|s| !s.trim().is_empty())
}

/// Create branch `name` at `start` (empty = HEAD), optionally checking it out; `overwrite` resets an
/// existing branch of that name (`checkout -B` / `branch -f`).
///
/// The new branch never tracks its start point (`--no-track`): by default git makes a remote start
/// such as `origin/main` the upstream, so the first push would go to `origin/main` instead of
/// publishing the branch under its own name. Without an upstream, push creates
/// `<remote>/<name>` and sets that as upstream. An overwritten branch keeps its own upstream.
pub fn create_branch_with(
    repo: &Repo,
    name: &str,
    start: &str,
    checkout_after: bool,
    overwrite: bool,
) -> Result<()> {
    validate_name(repo, name)?;
    if !overwrite && local_exists(repo, name) {
        bail!("a branch named '{name}' already exists");
    }
    let mut args: Vec<&str> = match (checkout_after, overwrite) {
        (true, false) => vec!["checkout", "--no-track", "-b", name],
        (true, true) => vec!["checkout", "--no-track", "-B", name],
        (false, false) => vec!["branch", "--no-track", name],
        (false, true) => vec!["branch", "--no-track", "-f", name],
    };
    if !start.is_empty() {
        args.push(start);
    } else if !checkout_after && repo.head_commit().is_none() {
        bail!("cannot create a branch before the first commit; use \"checkout after create\"");
    }
    if checkout_after {
        args.push("--");
    }
    repo.run(&args).map(|_| ())
}

/// Commits on the existing local branch `name` that resetting it to `start` (empty = HEAD) would leave
/// reachable only through the reflog, with the branch's current tip. None when the branch does not exist.
pub fn commits_lost_by_reset(repo: &Repo, name: &str, start: &str) -> Option<(u32, String)> {
    let full = format!("refs/heads/{name}");
    let tip = repo.rev_parse(&full).ok()?;
    let start = if start.is_empty() { "HEAD" } else { start };
    let range = match repo.rev_parse(start) {
        Ok(s) => format!("{s}..{tip}"),
        Err(_) => tip.clone(),
    };
    let lost = repo
        .try_run(&["rev-list", "--count", &range])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    Some((lost, tip))
}

/// Rename a local branch.
pub fn rename_branch(repo: &Repo, old: &str, new: &str) -> Result<()> {
    validate_name(repo, new)?;
    repo.run(&["branch", "-m", old, new]).map(|_| ())
}

/// Delete a local branch; `force` deletes even when unmerged.
pub fn delete_branch(repo: &Repo, name: &str, force: bool) -> Result<()> {
    if repo.head_branch().as_deref() == Some(name) {
        bail!("cannot delete the current branch '{name}'; check out another branch first");
    }
    repo.run(&["branch", if force { "-D" } else { "-d" }, name])
        .map(|_| ())
}

/// True when `git branch -d` refused because the branch is not merged.
pub fn is_not_merged_error(msg: &str) -> bool {
    msg.contains("not fully merged")
}

/// Merge flavors offered by "Merge into current".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMode {
    Merge,
    NoFf,
    Squash,
    FfOnly,
}

impl MergeMode {
    pub const ALL: [MergeMode; 4] = [
        MergeMode::Merge,
        MergeMode::NoFf,
        MergeMode::Squash,
        MergeMode::FfOnly,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MergeMode::Merge => "Merge",
            MergeMode::NoFf => "Merge (--no-ff, always create a merge commit)",
            MergeMode::Squash => "Squash merge (stage changes, commit yourself)",
            MergeMode::FfOnly => "Fast-forward only",
        }
    }

    fn flag(self) -> Option<&'static str> {
        match self {
            MergeMode::Merge => None,
            MergeMode::NoFf => Some("--no-ff"),
            MergeMode::Squash => Some("--squash"),
            MergeMode::FfOnly => Some("--ff-only"),
        }
    }
}

/// How a merge or rebase ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integration {
    /// Done; message for the user
    Done(String),
    /// Stopped with conflicts that must be resolved
    Conflicts,
}

/// Merge `branch` into the current branch.
pub fn merge(repo: &Repo, branch: &str, mode: MergeMode) -> Result<Integration> {
    let current = repo.head_branch().unwrap_or_else(|| "HEAD".into());
    let before = repo.head_commit();
    let mut args = vec!["merge", "--no-edit", "--autostash"];
    args.extend(mode.flag());
    args.push(branch);
    match repo.run(&args) {
        Ok(out) => {
            if out.contains("Already up to date") || out.contains("Already up-to-date") {
                return Ok(Integration::Done(format!(
                    "{current} is already up to date with {branch}"
                )));
            }
            if mode == MergeMode::Squash {
                return Ok(Integration::Done(format!(
                    "Squashed {branch} into the index; commit to finish"
                )));
            }
            let n = before
                .and_then(|b| repo.try_run(&["rev-list", "--count", &format!("{b}..HEAD")]))
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            Ok(Integration::Done(format!(
                "Merged {branch} into {current} ({})",
                commits(n)
            )))
        }
        Err(_) if has_conflicts(repo) => Ok(Integration::Conflicts),
        Err(e) => Err(e),
    }
}

/// Rebase the current branch onto `upstream`.
pub fn rebase_onto(repo: &Repo, upstream: &str) -> Result<Integration> {
    rebase(repo, &["rebase", "--autostash", upstream], upstream)
}

/// Check out `branch` and rebase it onto `onto`.
pub fn checkout_and_rebase(repo: &Repo, branch: &str, onto: &str) -> Result<Integration> {
    rebase(repo, &["rebase", "--autostash", onto, branch], onto)
}

fn rebase(repo: &Repo, args: &[&str], onto: &str) -> Result<Integration> {
    match repo.run(args) {
        Ok(out) => {
            let current = repo.head_branch().unwrap_or_else(|| "HEAD".into());
            if out.contains("is up to date") {
                Ok(Integration::Done(format!(
                    "{current} is already up to date with {onto}"
                )))
            } else {
                Ok(Integration::Done(format!("Rebased {current} onto {onto}")))
            }
        }
        Err(_) if repo.state() == RepoState::Rebasing || has_conflicts(repo) => {
            Ok(Integration::Conflicts)
        }
        Err(e) => Err(e),
    }
}

/// "1 commit" / "3 commits".
pub fn commits(n: u32) -> String {
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// Commits reachable from `include` but not from `exclude`, newest first (capped).
pub fn commits_between(
    repo: &Repo,
    exclude: &str,
    include: &str,
    max: usize,
) -> Result<Vec<Commit>> {
    repo.log(&LogQuery {
        revs: vec![include.to_string(), format!("^{exclude}")],
        max,
        ..Default::default()
    })
}

/// Best common ancestor of two revisions.
pub fn merge_base(repo: &Repo, a: &str, b: &str) -> Result<String> {
    Ok(repo.run(&["merge-base", a, b])?.trim().to_string())
}

/// (commits only in `a`, commits only in `b`).
pub fn ahead_behind(repo: &Repo, a: &str, b: &str) -> Result<(u32, u32)> {
    let out = repo.run(&["rev-list", "--left-right", "--count", &format!("{a}...{b}")])?;
    let mut it = out
        .split_whitespace()
        .map(|s| s.parse::<u32>().unwrap_or(0));
    Ok((it.next().unwrap_or(0), it.next().unwrap_or(0)))
}

/// Set `local`'s upstream to the remote-tracking branch `upstream` (`origin/x`).
pub fn set_upstream(repo: &Repo, local: &str, upstream: &str) -> Result<()> {
    repo.run(&["branch", &format!("--set-upstream-to={upstream}"), local])
        .map(|_| ())
}

/// Remove `local`'s upstream configuration.
pub fn unset_upstream(repo: &Repo, local: &str) -> Result<()> {
    repo.run(&["branch", "--unset-upstream", local]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::*;

    fn create_branch(repo: &Repo, name: &str, start: &str, checkout_after: bool) -> Result<()> {
        create_branch_with(repo, name, start, checkout_after, false)
    }

    fn names(v: &[Branch]) -> Vec<&str> {
        v.iter().map(|b| b.name.as_str()).collect()
    }

    #[test]
    fn track_parsing() {
        assert_eq!(parse_track("[ahead 2, behind 1]"), (2, 1, false));
        assert_eq!(parse_track("[ahead 3]"), (3, 0, false));
        assert_eq!(parse_track("[behind 7]"), (0, 7, false));
        assert_eq!(parse_track("[gone]"), (0, 0, true));
        assert_eq!(parse_track(""), (0, 0, false));
    }

    #[test]
    fn parse_for_each_ref_output() {
        let out = "refs/heads/main\0*\0origin/main\0[ahead 1]\0abc1234\x001700000000\0\0refs/remotes/origin/main\0Subject with \0 nul?\r\n\
refs/remotes/origin/HEAD\0 \0\0\0abc1234\x001700000000\0refs/remotes/origin/main\0\0s\n\
refs/remotes/my/fork/dev\0 \0\0\0def5678\x001700000001\0\0\0dev work\n\
refs/heads/feature/x y\0 \0\0\0def5678\x001700000001\0\0\0wip\n";
        let remotes = vec!["origin".to_string(), "my/fork".to_string()];
        let b = parse_branches(out, &remotes);
        assert_eq!(names(&b), vec!["feature/x y", "main", "my/fork/dev"]);
        let main = &b[1];
        assert!(main.is_head);
        assert_eq!(main.upstream.as_deref(), Some("origin/main"));
        assert_eq!(main.ahead, 1);
        assert_eq!(main.time, 1_700_000_000);
        assert_eq!(main.subject, "Subject with \0 nul?");
        let dev = &b[2];
        assert_eq!(dev.remote.as_deref(), Some("my/fork"));
        assert_eq!(dev.short_name(), "dev");
        assert!(dev.upstream.is_none());
    }

    #[test]
    fn recent_checkouts() {
        let out = "commit: x\ncheckout: moving from main to feature\ncheckout: moving from dev to main\ncheckout: moving from main to dev\ncheckout: moving from a to feature\n";
        assert_eq!(
            parse_recent_checkouts(out, 10),
            vec!["feature", "main", "dev"]
        );
        assert_eq!(parse_recent_checkouts(out, 2), vec!["feature", "main"]);
    }

    #[test]
    fn list_create_rename_delete() {
        let (_d, repo) = temp_repo();
        assert!(list_branches(&repo).unwrap().is_empty());
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        create_branch(&repo, "feature/ü x", "", false).unwrap_err();
        create_branch(&repo, "feature/ü", "", false).unwrap();
        assert!(create_branch(&repo, "feature/ü", "", false).is_err());
        create_branch(&repo, "dev", "HEAD", true).unwrap();
        assert_eq!(repo.head_branch().as_deref(), Some("dev"));
        let b = list_branches(&repo).unwrap();
        assert_eq!(names(&b), vec!["dev", "feature/ü", "main"]);
        assert!(b[0].is_head);
        assert_eq!(b[0].subject, "first");
        rename_branch(&repo, "feature/ü", "renamed").unwrap();
        assert!(local_exists(&repo, "renamed"));
        assert!(
            delete_branch(&repo, "dev", false).is_err(),
            "current branch"
        );
        checkout(&repo, "main", false).unwrap();
        commit_files(&repo, &[("b.txt", "2\n")], "main only");
        checkout(&repo, "renamed", false).unwrap();
        commit_files(&repo, &[("c.txt", "3\n")], "unmerged");
        checkout(&repo, "main", false).unwrap();
        let err = delete_branch(&repo, "renamed", false).unwrap_err();
        assert!(is_not_merged_error(&format!("{err:#}")));
        delete_branch(&repo, "renamed", true).unwrap();
        delete_branch(&repo, "dev", false).unwrap();
        assert_eq!(names(&list_branches(&repo).unwrap()), vec!["main"]);
        let recent = recent_branches(&repo, 5);
        assert_eq!(recent.first().map(String::as_str), Some("main"));
        assert!(recent.iter().any(|r| r == "renamed"));
    }

    #[test]
    fn checkout_with_local_changes_and_smart_checkout() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("f.txt", "base\n")], "base");
        create_branch(&repo, "other", "", false).unwrap();
        checkout(&repo, "other", false).unwrap();
        commit_files(
            &repo,
            &[("f.txt", "other\n"), ("g.txt", "g\n")],
            "other change",
        );
        checkout(&repo, "main", false).unwrap();
        write(&repo, "f.txt", "local edit\n");
        let err = checkout(&repo, "other", false).unwrap_err();
        assert!(is_local_changes_error(&format!("{err:#}")));
        // Restoring conflicts: changes stay in the stash, conflicts in the work tree.
        let out = smart_checkout(&repo, &CheckoutTarget::Existing("other".into())).unwrap();
        assert_eq!(out, SmartOutcome::Conflicts);
        assert_eq!(repo.head_branch().as_deref(), Some("other"));
        assert!(has_conflicts(&repo));
        repo.run(&["checkout", "-f", "main"]).unwrap();
        repo.run(&["stash", "drop"]).unwrap();
        // Non-conflicting change is carried over and the stash is dropped.
        write(&repo, "a-new-tracked.txt", "x\n");
        repo.run(&["add", "a-new-tracked.txt"]).unwrap();
        let out = smart_checkout(&repo, &CheckoutTarget::Existing("other".into())).unwrap();
        assert_eq!(out, SmartOutcome::Clean);
        assert!(repo.path("a-new-tracked.txt").exists());
        assert!(repo
            .try_run(&["rev-parse", "-q", "--verify", "refs/stash"])
            .is_none());
        // Force checkout discards local changes.
        write(&repo, "f.txt", "discard me\n");
        checkout(&repo, "main", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path("f.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "base\n"
        );
    }

    #[test]
    fn checkout_blocked_by_untracked_files() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("f.txt", "base\n")], "base");
        create_branch(&repo, "other", "", true).unwrap();
        commit_files(
            &repo,
            &[
                ("f.txt", "other\n"),
                ("dir/new file.txt", "tracked there\n"),
            ],
            "other",
        );
        checkout(&repo, "main", false).unwrap();
        write(&repo, "dir/new file.txt", "mine\n");
        let err = format!("{:#}", checkout(&repo, "other", false).unwrap_err());
        assert!(is_local_changes_error(&err));
        let b = CheckoutBlockers::parse(&err);
        assert_eq!(b.untracked, vec!["dir/new file.txt"]);
        assert!(b.changed.is_empty());
        assert!(!b.smart_checkout_possible());

        // Both kinds at once.
        write(&repo, "f.txt", "edit\n");
        let err = format!("{:#}", checkout(&repo, "other", false).unwrap_err());
        let b = CheckoutBlockers::parse(&err);
        assert_eq!(b.changed, vec!["f.txt"]);
        assert!(!b.smart_checkout_possible());

        // Tracked changes only: smart checkout is possible.
        std::fs::remove_file(repo.path("dir/new file.txt")).unwrap();
        let err = format!("{:#}", checkout(&repo, "other", false).unwrap_err());
        let b = CheckoutBlockers::parse(&err);
        assert_eq!(
            b,
            CheckoutBlockers {
                changed: vec!["f.txt".into()],
                untracked: vec![]
            }
        );
        assert!(b.smart_checkout_possible());
    }

    #[test]
    fn checkout_blockers_parsing() {
        let msg = "git checkout other -- failed: error: Your local changes to the following files would be overwritten by checkout:\r\n\ta.txt\r\n\tb c.txt\r\nPlease commit your changes or stash them before you switch branches.\r\nerror: The following untracked working tree files would be overwritten by checkout:\r\n\tu.txt\r\nPlease move or remove them before you switch branches.\r\nAborting";
        let b = CheckoutBlockers::parse(msg);
        assert_eq!(b.changed, vec!["a.txt", "b c.txt"]);
        assert_eq!(b.untracked, vec!["u.txt"]);
        assert_eq!(
            CheckoutBlockers::parse("fatal: nope"),
            CheckoutBlockers::default()
        );
    }

    #[test]
    fn merge_modes_and_conflicts() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "base");
        create_branch(&repo, "ff", "", false).unwrap();
        checkout(&repo, "ff", false).unwrap();
        commit_files(&repo, &[("b.txt", "2\n")], "on ff");
        checkout(&repo, "main", false).unwrap();
        let r = merge(&repo, "ff", MergeMode::FfOnly).unwrap();
        assert_eq!(
            r,
            Integration::Done("Merged ff into main (1 commit)".into())
        );
        let r = merge(&repo, "ff", MergeMode::Merge).unwrap();
        assert!(matches!(r, Integration::Done(m) if m.contains("already up to date")));

        create_branch(&repo, "side", "", false).unwrap();
        checkout(&repo, "side", false).unwrap();
        commit_files(&repo, &[("s.txt", "s\n")], "side");
        checkout(&repo, "main", false).unwrap();
        commit_files(&repo, &[("m.txt", "m\n")], "main");
        assert!(merge(&repo, "side", MergeMode::FfOnly).is_err());
        let r = merge(&repo, "side", MergeMode::NoFf).unwrap();
        assert!(matches!(r, Integration::Done(_)));
        assert_eq!(repo.commit_info("HEAD").unwrap().parents.len(), 2);

        create_branch(&repo, "sq", "", false).unwrap();
        checkout(&repo, "sq", false).unwrap();
        commit_files(&repo, &[("q.txt", "q\n")], "sq");
        checkout(&repo, "main", false).unwrap();
        let r = merge(&repo, "sq", MergeMode::Squash).unwrap();
        assert!(matches!(r, Integration::Done(m) if m.contains("Squashed")));
        assert!(repo
            .run(&["diff", "--cached", "--name-only"])
            .unwrap()
            .contains("q.txt"));
        repo.run(&["commit", "-q", "-m", "squashed"]).unwrap();

        create_branch(&repo, "c", "HEAD~1", false).unwrap();
        commit_files(&repo, &[("a.txt", "main\n")], "main edit");
        checkout(&repo, "c", false).unwrap();
        commit_files(&repo, &[("a.txt", "c\n")], "c edit");
        checkout(&repo, "main", false).unwrap();
        assert_eq!(
            merge(&repo, "c", MergeMode::Merge).unwrap(),
            Integration::Conflicts
        );
        assert_eq!(repo.state(), RepoState::Merging);
    }

    #[test]
    fn rebase_compare_and_ahead_behind() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "base");
        create_branch(&repo, "topic", "", false).unwrap();
        commit_files(&repo, &[("m.txt", "m\n")], "main 1");
        commit_files(&repo, &[("m2.txt", "m\n")], "main 2");
        checkout(&repo, "topic", false).unwrap();
        commit_files(&repo, &[("t.txt", "t\n")], "topic 1");
        assert_eq!(ahead_behind(&repo, "topic", "main").unwrap(), (1, 2));
        let only_main = commits_between(&repo, "topic", "main", 100).unwrap();
        assert_eq!(
            only_main
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["main 2", "main 1"]
        );
        let base = merge_base(&repo, "topic", "main").unwrap();
        assert_eq!(base, repo.rev_parse("main~2").unwrap());
        let r = rebase_onto(&repo, "main").unwrap();
        assert_eq!(r, Integration::Done("Rebased topic onto main".into()));
        assert_eq!(ahead_behind(&repo, "topic", "main").unwrap(), (1, 0));
        assert!(
            matches!(rebase_onto(&repo, "main").unwrap(), Integration::Done(m) if m.contains("up to date"))
        );

        // Checkout and rebase onto current.
        checkout(&repo, "main", false).unwrap();
        commit_files(&repo, &[("m3.txt", "m\n")], "main 3");
        let r = checkout_and_rebase(&repo, "topic", "main").unwrap();
        assert!(matches!(r, Integration::Done(_)));
        assert_eq!(repo.head_branch().as_deref(), Some("topic"));
        assert_eq!(ahead_behind(&repo, "topic", "main").unwrap(), (1, 0));

        // Conflicting rebase stops in the rebasing state.
        commit_files(&repo, &[("a.txt", "topic\n")], "topic edit");
        checkout(&repo, "main", false).unwrap();
        commit_files(&repo, &[("a.txt", "main\n")], "main edit");
        checkout(&repo, "topic", false).unwrap();
        assert_eq!(rebase_onto(&repo, "main").unwrap(), Integration::Conflicts);
        assert_eq!(repo.state(), RepoState::Rebasing);
        repo.run(&["rebase", "--abort"]).unwrap();
    }

    #[test]
    fn upstream_tracking_and_favorites() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "base");
        create_branch(&repo, "base", "", false).unwrap();
        commit_files(&repo, &[("b.txt", "1\n")], "ahead");
        set_upstream(&repo, "main", "base").unwrap();
        let b = list_branches(&repo).unwrap();
        let main = b.iter().find(|b| b.name == "main").unwrap();
        assert_eq!(main.upstream.as_deref(), Some("base"));
        assert_eq!((main.ahead, main.behind), (1, 0));
        unset_upstream(&repo, "main").unwrap();
        let b = list_branches(&repo).unwrap();
        assert!(b
            .iter()
            .find(|b| b.name == "main")
            .unwrap()
            .upstream
            .is_none());

        let mut fav = Favorites::load(&repo);
        assert!(fav.contains("refs/heads/main"));
        assert!(fav.toggle("refs/heads/base"));
        assert!(!fav.toggle("refs/heads/main"));
        fav.save(&repo).unwrap();
        let back = Favorites::load(&repo);
        assert!(back.contains("refs/heads/base"));
        assert!(!back.contains("refs/heads/main"));
    }

    #[test]
    fn overwrite_existing_branch() {
        let (_d, repo) = temp_repo();
        let first = commit_files(&repo, &[("a.txt", "1\n")], "first");
        create_branch(&repo, "x", "", false).unwrap();
        let second = commit_files(&repo, &[("a.txt", "2\n")], "second");
        assert_eq!(repo.rev_parse("x").unwrap(), first);
        create_branch_with(&repo, "x", "", false, true).unwrap();
        assert_eq!(repo.rev_parse("x").unwrap(), second);
        assert_eq!(
            commits_lost_by_reset(&repo, "x", ""),
            Some((0, second.clone()))
        );
        assert_eq!(
            commits_lost_by_reset(&repo, "x", &first),
            Some((1, second.clone()))
        );
        assert_eq!(commits_lost_by_reset(&repo, "nope", ""), None);
        create_branch_with(&repo, "x", &first, true, true).unwrap();
        assert_eq!(repo.head_branch().as_deref(), Some("x"));
        assert_eq!(repo.head_commit().unwrap(), first);
    }

    #[test]
    fn new_branch_from_remote_branch_does_not_track_it() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        repo.run(&["remote", "add", "origin", "."]).unwrap();
        repo.run(&["update-ref", "refs/remotes/origin/main", "HEAD"])
            .unwrap();
        let upstream =
            |b: &str| repo.try_run(&["rev-parse", "--abbrev-ref", &format!("{b}@{{upstream}}")]);
        // Plain git would set origin/main as upstream, so the first push would go there.
        create_branch_with(&repo, "feature", "origin/main", true, false).unwrap();
        assert_eq!(repo.head_branch().as_deref(), Some("feature"));
        assert_eq!(upstream("feature"), None);
        create_branch_with(&repo, "other", "origin/main", false, false).unwrap();
        assert_eq!(upstream("other"), None);
        // Overwriting keeps the branch's own upstream.
        repo.run(&["branch", "--set-upstream-to=origin/main", "other"])
            .unwrap();
        create_branch_with(&repo, "other", "HEAD", false, true).unwrap();
        assert_eq!(
            upstream("other").as_deref().map(str::trim),
            Some("origin/main")
        );
    }

    #[test]
    fn rebase_and_merge_autostash_local_changes() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "base");
        create_branch(&repo, "topic", "", false).unwrap();
        commit_files(&repo, &[("m.txt", "m\n")], "main");
        checkout(&repo, "topic", false).unwrap();
        commit_files(&repo, &[("t.txt", "t\n")], "topic");
        write(&repo, "a.txt", "dirty\n");
        assert!(matches!(
            rebase_onto(&repo, "main").unwrap(),
            Integration::Done(_)
        ));
        assert_eq!(
            std::fs::read_to_string(repo.path("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "dirty\n"
        );
        checkout(&repo, "main", false).unwrap();
        assert!(matches!(
            merge(&repo, "topic", MergeMode::Merge).unwrap(),
            Integration::Done(_)
        ));
        assert_eq!(
            std::fs::read_to_string(repo.path("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "dirty\n"
        );
    }

    #[test]
    fn create_in_empty_repo() {
        let (_d, repo) = temp_repo();
        assert!(create_branch(&repo, "x", "", false).is_err());
        create_branch(&repo, "x", "", true).unwrap();
        assert_eq!(repo.head_branch().as_deref(), Some("x"));
    }
}
