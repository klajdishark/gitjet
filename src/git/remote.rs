//! Network: fetch, pull (merge/rebase/ff-only), update project, push (force-with-lease, tags, upstream),
//! remote branch deletion and the commits-to-push preview.
//!
//! These functions block; the UI runs them through `Action::spawn`. Background git never prompts:
//! [`Repo::command`] disables git's own credential prompts and every network command goes through
//! [`net_command`], which also puts ssh into batch mode (no host-key or passphrase questions on the
//! terminal the TUI owns). Authentication problems therefore surface as errors; [`auth_hint`] turns them
//! into a pointer to the "(in terminal)" variants built with [`terminal_command`].

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{anyhow, bail, Result};

use super::branch::{commits, has_conflicts};
use super::log::{Commit, LogQuery};
use super::{Repo, RepoState};

/// Configured remote names.
pub fn remotes(repo: &Repo) -> Result<Vec<String>> {
    Ok(repo
        .run(&["remote"])?
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// True when a git error looks like a credential / SSH problem that needs an interactive terminal.
pub fn is_auth_error(msg: &str) -> bool {
    const PATTERNS: [&str; 14] = [
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "permission denied",
        "host key verification failed",
        "passphrase",
        "invalid username or password",
        "access denied",
        "authorization failed",
        "the requested url returned error: 401",
        "the requested url returned error: 403",
        "no such identity",
        "could not read from remote repository",
    ];
    let m = msg.to_lowercase();
    PATTERNS.iter().any(|p| m.contains(p))
}

/// Add a hint to network errors: use "`op` (in terminal)" for authentication problems, or pull first
/// for rejected pushes.
pub fn auth_hint(e: anyhow::Error, op: &str) -> anyhow::Error {
    let msg = format!("{e:#}");
    if is_auth_error(&msg) {
        anyhow!(
            "{msg}\n\nThis looks like an authentication or SSH host-key problem. Background git cannot ask for \
             credentials — run \"{op} (in terminal)\" from the Branches menu ('.') or the action palette (':')."
        )
    } else if msg.contains("rejected")
        || msg.contains("non-fast-forward")
        || msg.contains("fetch first")
    {
        anyhow!("{msg}\n\nThe remote has commits you don't have: pull or update the project first, or push with force-with-lease.")
    } else {
        e
    }
}

/// A plain interactive `git` command for `Action::Suspend` (real terminal, so prompts work).
pub fn terminal_command(repo: &Repo, args: &[&str]) -> Command {
    let mut c = Command::new(super::git_program());
    c.args(args).current_dir(&repo.root);
    c
}

/// ssh option that makes ssh fail instead of asking for a host key confirmation or a passphrase.
const SSH_BATCH_MODE: &str = "-o BatchMode=yes";

/// The ssh command to put in `GIT_SSH_COMMAND` so background ssh never prompts, or None to leave the
/// environment alone.
///
/// The effective command follows git's precedence: `GIT_SSH_COMMAND`, then `GIT_SSH` (a bare program
/// we cannot add options to, so it is left untouched), then `core.sshCommand`, then plain `ssh`.
/// Commands that are not OpenSSH-like (PuTTY's plink, TortoisePlink) or already set `BatchMode` are
/// left as they are.
pub fn batch_ssh_command(
    env_cmd: Option<&str>,
    git_ssh: Option<&str>,
    configured: Option<&str>,
) -> Option<String> {
    fn non_empty(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    let cmd = match (
        non_empty(env_cmd),
        non_empty(git_ssh),
        non_empty(configured),
    ) {
        (Some(c), _, _) => c.to_string(),
        (None, Some(_), _) => return None,
        (None, None, Some(c)) => c.to_string(),
        (None, None, None) => "ssh".to_string(),
    };
    if cmd.to_lowercase().contains("batchmode") {
        return None;
    }
    let base = program_name(&cmd).to_lowercase();
    if !base.contains("ssh") || base.contains("plink") {
        return None;
    }
    Some(format!("{cmd} {SSH_BATCH_MODE}"))
}

/// File name of the program a shell command line starts with (quoted paths may contain spaces).
fn program_name(cmd: &str) -> &str {
    let program = match cmd.chars().next() {
        Some(q @ ('"' | '\'')) => cmd[1..].split(q).next().unwrap_or_default(),
        _ => cmd.split_whitespace().next().unwrap_or_default(),
    };
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// A background `git` command for network operations: [`Repo::command`] plus ssh batch mode.
pub fn net_command(repo: &Repo, args: &[&str]) -> Command {
    let env = |k: &str| std::env::var(k).ok();
    let mut c = repo.command();
    c.args(args);
    let configured = repo.config_get("core.sshCommand");
    let git_ssh = env("GIT_SSH");
    if let Some(ssh) = batch_ssh_command(
        env("GIT_SSH_COMMAND").as_deref(),
        git_ssh.as_deref(),
        configured.as_deref(),
    ) {
        c.env("GIT_SSH_COMMAND", ssh);
    }
    c
}

/// Run a network git command in the background (never prompts; see [`net_command`]).
pub fn run_net(repo: &Repo, args: &[&str]) -> Result<String> {
    repo.run_cmd(net_command(repo, args), args)
}

/// `refname → object` for every remote-tracking branch.
fn remote_refs(repo: &Repo) -> BTreeMap<String, String> {
    repo.try_run(&[
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/remotes",
    ])
    .unwrap_or_default()
    .lines()
    .filter_map(|l| l.trim().split_once(' '))
    .filter(|(r, _)| !r.ends_with("/HEAD"))
    .map(|(r, o)| (r.to_string(), o.to_string()))
    .collect()
}

/// How remote-tracking refs changed during a fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefChanges {
    pub updated: usize,
    pub created: usize,
    pub pruned: usize,
}

impl RefChanges {
    pub fn between(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Self {
        let mut c = RefChanges::default();
        for (r, o) in after {
            match before.get(r) {
                None => c.created += 1,
                Some(old) if old != o => c.updated += 1,
                _ => {}
            }
        }
        c.pruned = before.keys().filter(|r| !after.contains_key(*r)).count();
        c
    }

    /// "2 updated, 1 new" or "everything up to date".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.updated > 0 {
            parts.push(format!("{} updated", self.updated));
        }
        if self.created > 0 {
            parts.push(format!("{} new", self.created));
        }
        if self.pruned > 0 {
            parts.push(format!("{} pruned", self.pruned));
        }
        if parts.is_empty() {
            "everything up to date".into()
        } else {
            format!(
                "{} branch{}",
                parts.join(", "),
                if self.updated + self.created + self.pruned == 1 {
                    ""
                } else {
                    "es"
                }
            )
        }
    }
}

/// `git fetch --all --tags [--prune]`; returns a summary like "Fetched origin: 2 updated branches".
pub fn fetch_all(repo: &Repo, prune: bool) -> Result<String> {
    let names = remotes(repo)?;
    if names.is_empty() {
        bail!("no remotes configured; add one in the Repository tab");
    }
    let before = remote_refs(repo);
    let mut args = vec!["fetch", "--all", "--tags"];
    if prune {
        args.push("--prune");
    }
    run_net(repo, &args).map_err(|e| auth_hint(e, "Fetch"))?;
    let changes = RefChanges::between(&before, &remote_refs(repo));
    let mut msg = format!("Fetched {}: {}", names.join(", "), changes.describe());
    if let Some((_, behind)) = head_ahead_behind(repo) {
        if behind > 0 {
            let branch = repo.head_branch().unwrap_or_default();
            msg.push_str(&format!("; {branch} is {} behind", commits(behind)));
        }
    }
    Ok(msg)
}

/// (ahead, behind) of HEAD relative to its upstream, if it has one.
pub fn head_ahead_behind(repo: &Repo) -> Option<(u32, u32)> {
    let out = repo.try_run(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let mut it = out
        .split_whitespace()
        .map(|s| s.parse::<u32>().unwrap_or(0));
    Some((it.next()?, it.next()?))
}

/// The upstream of a local branch as (remote, branch name on the remote).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub remote: String,
    pub branch: String,
}

impl Upstream {
    /// Remote-tracking ref in short form (`origin/main`).
    pub fn short(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// Configured upstream of `local` on a real remote (local upstreams, `remote = .`, are ignored).
pub fn upstream_of(repo: &Repo, local: &str) -> Option<Upstream> {
    let remote = repo.config_get(&format!("branch.{local}.remote"))?;
    let merge = repo.config_get(&format!("branch.{local}.merge"))?;
    if remote == "." {
        return None;
    }
    Some(Upstream {
        remote,
        branch: merge
            .strip_prefix("refs/heads/")
            .unwrap_or(&merge)
            .to_string(),
    })
}

/// How pull/update integrate upstream changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullMode {
    Merge,
    Rebase,
    FfOnly,
}

impl PullMode {
    /// From the config value `update.method` ("rebase" or "merge").
    pub fn from_config(method: &str) -> PullMode {
        if method.eq_ignore_ascii_case("merge") {
            PullMode::Merge
        } else {
            PullMode::Rebase
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PullMode::Merge => "merge",
            PullMode::Rebase => "rebase",
            PullMode::FfOnly => "fast-forward only",
        }
    }

    fn pull_flag(self) -> &'static str {
        match self {
            PullMode::Merge => "--no-rebase",
            PullMode::Rebase => "--rebase",
            PullMode::FfOnly => "--ff-only",
        }
    }
}

/// The current branch name; errors for detached HEAD / missing upstream.
fn current_with_upstream(repo: &Repo) -> Result<String> {
    let branch = repo
        .head_branch()
        .ok_or_else(|| anyhow!("HEAD is detached; check out a branch first"))?;
    if repo.head_commit().is_none() {
        bail!("'{branch}' has no commits yet");
    }
    if repo
        .try_run(&["rev-parse", "--verify", "-q", "@{upstream}"])
        .is_none()
    {
        bail!("'{branch}' has no upstream branch; set one with 'u' in the Branches tab (or push with set-upstream)");
    }
    Ok(branch)
}

/// Error describing a pull/update that stopped with conflicts.
fn conflicts_error(repo: &Repo) -> anyhow::Error {
    let n = repo
        .try_run(&["diff", "--name-only", "--diff-filter=U"])
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    anyhow!(
        "stopped with conflicts in {n} file{} — resolve them in the Conflicts tab (6)",
        if n == 1 { "" } else { "s" }
    )
}

fn integrated_message(verb: &str, n: u32, branch: &str) -> String {
    if n == 0 {
        format!("{branch} is already up to date")
    } else {
        format!("{verb} {} into {branch}", commits(n))
    }
}

/// `git pull` with the given mode and `--autostash`; returns "Pulled 3 commits into main".
pub fn pull(repo: &Repo, mode: PullMode) -> Result<String> {
    let branch = current_with_upstream(repo)?;
    let old_head = repo.head_commit().unwrap_or_default();
    let r = run_net(
        repo,
        &["pull", "--no-edit", "--autostash", mode.pull_flag()],
    );
    finish_integration(repo, r, "Pull")?;
    Ok(integrated_message(
        "Pulled",
        count_range(repo, &old_head, "@{upstream}"),
        &branch,
    ))
}

/// "Update project": fetch everything, then merge or rebase the current branch onto its upstream
/// (with autostash).
pub fn update_project(repo: &Repo, mode: PullMode, prune: bool) -> Result<String> {
    let fetched = fetch_all(repo, prune)?;
    let Ok(branch) = current_with_upstream(repo) else {
        return Ok(format!(
            "{fetched} (current branch has no upstream; nothing to integrate)"
        ));
    };
    let old_head = repo.head_commit().unwrap_or_default();
    let n = count_range(repo, &old_head, "@{upstream}");
    if n == 0 {
        return Ok(format!("Project is up to date ({branch})"));
    }
    let r = match mode {
        PullMode::Rebase => repo.run(&["rebase", "--autostash", "@{upstream}"]),
        PullMode::Merge => repo.run(&["merge", "--no-edit", "--autostash", "@{upstream}"]),
        PullMode::FfOnly => repo.run(&["merge", "--ff-only", "--autostash", "@{upstream}"]),
    };
    finish_integration(repo, r, "Update")?;
    Ok(format!(
        "Updated project: {}",
        integrated_message("integrated", n, &branch)
    ))
}

fn finish_integration(repo: &Repo, r: Result<String>, op: &str) -> Result<()> {
    match r {
        Ok(_) => Ok(()),
        Err(_)
            if has_conflicts(repo)
                || matches!(repo.state(), RepoState::Rebasing | RepoState::Merging) =>
        {
            Err(conflicts_error(repo))
        }
        Err(e) => Err(auth_hint(e, if op == "Pull" { "Pull" } else { "Fetch" })),
    }
}

/// Commits in `to` not in `from` (0 on errors).
fn count_range(repo: &Repo, from: &str, to: &str) -> u32 {
    if from.is_empty() {
        return 0;
    }
    repo.try_run(&["rev-list", "--count", &format!("{from}..{to}")])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Everything needed to run one `git push`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSpec {
    pub remote: String,
    /// Local branch to push
    pub local: String,
    /// Branch name on the remote
    pub target: String,
    pub force_with_lease: bool,
    /// Also push all tags
    pub tags: bool,
    pub set_upstream: bool,
}

impl PushSpec {
    /// Push `local` to its upstream, or to `<first remote>/<local>` as a new branch with set-upstream.
    pub fn for_branch(repo: &Repo, local: &str) -> Result<PushSpec> {
        if let Some(up) = upstream_of(repo, local) {
            return Ok(PushSpec {
                remote: up.remote,
                local: local.to_string(),
                target: up.branch,
                force_with_lease: false,
                tags: false,
                set_upstream: false,
            });
        }
        let names = remotes(repo)?;
        let remote = default_remote(&names)
            .ok_or_else(|| anyhow!("no remotes configured; add one in the Repository tab"))?;
        Ok(PushSpec {
            remote,
            local: local.to_string(),
            target: local.to_string(),
            force_with_lease: false,
            tags: false,
            set_upstream: true,
        })
    }

    /// `origin/main`.
    pub fn target_short(&self) -> String {
        format!("{}/{}", self.remote, self.target)
    }

    /// Arguments after `git`.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec!["push".to_string(), "--porcelain".to_string()];
        if self.force_with_lease {
            a.push("--force-with-lease".into());
        }
        if self.tags {
            a.push("--tags".into());
        }
        if self.set_upstream {
            a.push("--set-upstream".into());
        }
        a.push(self.remote.clone());
        a.push(format!(
            "refs/heads/{}:refs/heads/{}",
            self.local, self.target
        ));
        a
    }

    /// Arguments for the interactive "(in terminal)" variant (human-readable output).
    pub fn terminal_args(&self) -> Vec<String> {
        self.args()
            .into_iter()
            .filter(|a| a != "--porcelain")
            .collect()
    }
}

/// `origin` if present, else the first remote.
pub fn default_remote(names: &[String]) -> Option<String> {
    names
        .iter()
        .find(|r| *r == "origin")
        .or_else(|| names.first())
        .cloned()
}

/// True if the remote-tracking branch `remote/branch` exists locally.
pub fn remote_branch_exists(repo: &Repo, remote: &str, branch: &str) -> bool {
    repo.try_run(&[
        "rev-parse",
        "--verify",
        "-q",
        &format!("refs/remotes/{remote}/{branch}"),
    ])
    .is_some()
}

/// Commits that pushing `local` to `remote/target` would send, newest first (capped at `max`).
/// For a target that does not exist yet: commits not on any remote.
pub fn commits_to_push(
    repo: &Repo,
    local: &str,
    remote: &str,
    target: &str,
    max: usize,
) -> Result<Vec<Commit>> {
    let local_ref = format!("refs/heads/{local}");
    if repo
        .try_run(&["rev-parse", "--verify", "-q", &local_ref])
        .is_none()
    {
        return Ok(Vec::new());
    }
    let revs = if remote_branch_exists(repo, remote, target) {
        vec![local_ref, format!("^refs/remotes/{remote}/{target}")]
    } else {
        vec![local_ref, "--not".into(), "--remotes".into()]
    };
    repo.log(&LogQuery {
        revs,
        max,
        ..Default::default()
    })
}

/// Run a push; returns "Pushed 2 commits to origin/main".
pub fn push(repo: &Repo, spec: &PushSpec) -> Result<String> {
    let existed = remote_branch_exists(repo, &spec.remote, &spec.target);
    let n = commits_to_push(repo, &spec.local, &spec.remote, &spec.target, 100_000)?.len() as u32;
    let args = spec.args();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_push(repo, &refs).map_err(|e| auth_hint(e, "Push"))?;
    let target = spec.target_short();
    Ok(if !existed {
        format!("Pushed new branch {} to {target}", spec.local)
    } else if spec.force_with_lease && out.lines().any(|l| l.starts_with('+')) {
        format!("Force-pushed {} to {target}", spec.local)
    } else if n == 0 {
        if spec.tags {
            format!("Pushed tags to {}", spec.remote)
        } else {
            format!("Everything up to date ({target})")
        }
    } else {
        format!("Pushed {} to {target}", commits(n))
    })
}

/// Run `git push --porcelain`. On failure the per-ref reasons (printed on stdout in porcelain
/// mode, e.g. `[rejected] (fetch first)`) are put in front of git's stderr.
fn run_push(repo: &Repo, args: &[&str]) -> Result<String> {
    let out = net_command(repo, args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| anyhow!("could not run git push: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let reasons = rejected_refs(&stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut msg = String::from("git push failed");
    if !reasons.is_empty() {
        msg.push_str(": ");
        msg.push_str(&reasons.join("; "));
    }
    if !stderr.trim().is_empty() {
        msg.push('\n');
        msg.push_str(stderr.trim());
    }
    Err(anyhow!(msg))
}

/// `refs/heads/x: [rejected] (fetch first)` for each `!` line of `git push --porcelain` output.
fn rejected_refs(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("!\t"))
        .map(|l| {
            let mut parts = l.splitn(2, '\t');
            let refspec = parts.next().unwrap_or_default();
            let dst = refspec.rsplit(':').next().unwrap_or(refspec);
            format!("{dst}: {}", parts.next().unwrap_or("rejected").trim())
        })
        .collect()
}

/// `git push <remote> --delete <branch>`.
pub fn delete_remote_branch(repo: &Repo, remote: &str, branch: &str) -> Result<String> {
    run_net(repo, &["push", remote, "--delete", branch]).map_err(|e| auth_hint(e, "Push"))?;
    Ok(format!("Deleted {remote}/{branch}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::branch;
    use crate::git::testutil::*;

    /// A bare repo plus a local repo with `origin` pointing at it.
    fn with_remote() -> (TempDir, TempDir, Repo) {
        let bare = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(bare.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let (d, repo) = temp_repo();
        repo.run(&["remote", "add", "origin", &bare.path().to_string_lossy()])
            .unwrap();
        (bare, d, repo)
    }

    /// A second clone of the bare repo (another developer).
    fn clone_of(bare: &TempDir) -> (TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("clone");
        let ok = Command::new("git")
            .args(["clone", "-q"])
            .arg(bare.path())
            .arg(&dst)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let repo = Repo::discover(&dst).unwrap();
        for (k, v) in [
            ("user.name", "Other"),
            ("user.email", "o@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            repo.run(&["config", k, v]).unwrap();
        }
        (dir, repo)
    }

    #[test]
    fn auth_errors_are_detected() {
        assert!(is_auth_error(
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
        ));
        assert!(is_auth_error(
            "git@github.com: Permission denied (publickey)."
        ));
        assert!(is_auth_error("Host key verification failed."));
        assert!(!is_auth_error("fatal: couldn't find remote ref nope"));
        let e = auth_hint(anyhow!("Host key verification failed."), "Fetch");
        assert!(format!("{e}").contains("Fetch (in terminal)"));
        let e = auth_hint(anyhow!("! [rejected] main -> main (fetch first)"), "Push");
        assert!(format!("{e}").contains("pull or update"));
    }

    #[test]
    fn ssh_batch_mode_command() {
        let b = SSH_BATCH_MODE;
        assert_eq!(
            batch_ssh_command(None, None, None),
            Some(format!("ssh {b}"))
        );
        assert_eq!(
            batch_ssh_command(Some("  "), None, None),
            Some(format!("ssh {b}"))
        );
        assert_eq!(
            batch_ssh_command(None, None, Some("ssh -i ~/.ssh/work")),
            Some(format!("ssh -i ~/.ssh/work {b}"))
        );
        assert_eq!(
            batch_ssh_command(Some("ssh -p 2222"), None, Some("ssh -i k")),
            Some(format!("ssh -p 2222 {b}"))
        );
        assert_eq!(
            batch_ssh_command(None, None, Some("\"C:/Program Files/OpenSSH/ssh.exe\" -v")),
            Some(format!("\"C:/Program Files/OpenSSH/ssh.exe\" -v {b}"))
        );
        assert_eq!(
            batch_ssh_command(None, Some("/usr/bin/ssh"), Some("ssh -i k")),
            None
        );
        assert_eq!(
            batch_ssh_command(Some("ssh -o BatchMode=no"), None, None),
            None
        );
        assert_eq!(
            batch_ssh_command(None, None, Some("C:/PuTTY/plink.exe -batch")),
            None
        );
        assert_eq!(batch_ssh_command(None, None, Some("my-wrapper --x")), None);
        assert_eq!(program_name("'C:\\Tools\\ssh.exe' -v"), "ssh.exe");
        assert_eq!(program_name("/usr/bin/ssh -i k"), "ssh");
    }

    #[test]
    fn porcelain_rejections() {
        let out = "To /tmp/x\n!\trefs/heads/main:refs/heads/main\t[rejected] (fetch first)\nDone\n";
        assert_eq!(
            rejected_refs(out),
            vec!["refs/heads/main: [rejected] (fetch first)".to_string()]
        );
        assert!(rejected_refs("=\trefs/heads/a:refs/heads/a\t[up to date]\n").is_empty());
    }

    #[test]
    fn net_command_sets_batch_mode() {
        let (_d, repo) = temp_repo();
        let expected = batch_ssh_command(
            std::env::var("GIT_SSH_COMMAND").ok().as_deref(),
            std::env::var("GIT_SSH").ok().as_deref(),
            None,
        );
        let c = net_command(&repo, &["fetch"]);
        let get = |c: &Command| {
            c.get_envs()
                .find(|(k, _)| *k == "GIT_SSH_COMMAND")
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(get(&c), expected);
        if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
            assert_eq!(get(&c).as_deref(), Some("ssh -o BatchMode=yes"));
            repo.run(&["config", "core.sshCommand", "ssh -i key"])
                .unwrap();
            assert_eq!(
                get(&net_command(&repo, &["fetch"])).as_deref(),
                Some("ssh -i key -o BatchMode=yes")
            );
        }
        assert!(c.get_args().any(|a| a == "fetch"));
        // The interactive variant keeps prompts available.
        assert!(get(&terminal_command(&repo, &["fetch"])).is_none());
    }

    #[test]
    fn ref_change_summary() {
        let m = |v: &[(&str, &str)]| {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let before: BTreeMap<_, _> = m(&[("a", "1"), ("b", "2"), ("c", "3")]);
        let after: BTreeMap<_, _> = m(&[("a", "1"), ("b", "9"), ("d", "4")]);
        let c = RefChanges::between(&before, &after);
        assert_eq!(
            c,
            RefChanges {
                updated: 1,
                created: 1,
                pruned: 1
            }
        );
        assert_eq!(c.describe(), "1 updated, 1 new, 1 pruned branches");
        assert_eq!(RefChanges::default().describe(), "everything up to date");
    }

    #[test]
    fn push_spec_args() {
        let s = PushSpec {
            remote: "origin".into(),
            local: "feat".into(),
            target: "feat-remote".into(),
            force_with_lease: true,
            tags: true,
            set_upstream: true,
        };
        assert_eq!(
            s.terminal_args(),
            vec![
                "push",
                "--force-with-lease",
                "--tags",
                "--set-upstream",
                "origin",
                "refs/heads/feat:refs/heads/feat-remote"
            ]
        );
        assert_eq!(
            default_remote(&["up".into(), "origin".into()]).as_deref(),
            Some("origin")
        );
        assert_eq!(default_remote(&["up".into()]).as_deref(), Some("up"));
        assert_eq!(default_remote(&[]), None);
        assert_eq!(PullMode::from_config("Merge"), PullMode::Merge);
        assert_eq!(PullMode::from_config("rebase"), PullMode::Rebase);
    }

    #[test]
    fn push_fetch_pull_roundtrip() {
        let (bare, _d, repo) = with_remote();
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        commit_files(&repo, &[("b.txt", "2\n")], "second");

        // New branch: everything not on a remote is pushed with set-upstream.
        let spec = PushSpec::for_branch(&repo, "main").unwrap();
        assert!(spec.set_upstream);
        assert_eq!(spec.target_short(), "origin/main");
        assert_eq!(
            commits_to_push(&repo, "main", "origin", "main", 100)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            push(&repo, &spec).unwrap(),
            "Pushed new branch main to origin/main"
        );
        assert_eq!(
            upstream_of(&repo, "main"),
            Some(Upstream {
                remote: "origin".into(),
                branch: "main".into()
            })
        );
        assert!(commits_to_push(&repo, "main", "origin", "main", 100)
            .unwrap()
            .is_empty());

        commit_files(&repo, &[("c.txt", "3\n")], "third");
        let spec = PushSpec::for_branch(&repo, "main").unwrap();
        assert!(!spec.set_upstream);
        let to_push = commits_to_push(&repo, "main", "origin", "main", 100).unwrap();
        assert_eq!(
            to_push
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["third"]
        );
        assert_eq!(
            push(&repo, &spec).unwrap(),
            "Pushed 1 commit to origin/main"
        );
        assert!(push(&repo, &spec)
            .unwrap()
            .contains("Everything up to date"));

        // Someone else pushes two commits.
        let (_cd, other) = clone_of(&bare);
        commit_files(&other, &[("o1.txt", "o\n")], "other 1");
        commit_files(&other, &[("o2.txt", "o\n")], "other 2");
        other.run(&["push", "-q", "origin", "main"]).unwrap();
        other
            .run(&["push", "-q", "origin", "main:refs/heads/shared"])
            .unwrap();

        let msg = fetch_all(&repo, true).unwrap();
        assert!(
            msg.starts_with("Fetched origin: 1 updated, 1 new branches"),
            "{msg}"
        );
        assert!(msg.contains("main is 2 commits behind"), "{msg}");
        assert_eq!(head_ahead_behind(&repo), Some((0, 2)));

        // Our local change rides along thanks to autostash.
        write(&repo, "a.txt", "local edit\n");
        assert_eq!(
            pull(&repo, PullMode::Rebase).unwrap(),
            "Pulled 2 commits into main"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "local edit\n"
        );
        assert_eq!(
            pull(&repo, PullMode::FfOnly).unwrap(),
            "main is already up to date"
        );
        repo.run(&["checkout", "--", "a.txt"]).unwrap();

        // Diverged history: ff-only fails, update project rebases.
        commit_files(&other, &[("o3.txt", "o\n")], "other 3");
        other.run(&["push", "-q", "origin", "main"]).unwrap();
        commit_files(&repo, &[("mine.txt", "m\n")], "mine");
        repo.run(&["fetch", "-q"]).unwrap();
        assert!(pull(&repo, PullMode::FfOnly).is_err());
        let msg = update_project(&repo, PullMode::Rebase, true).unwrap();
        assert_eq!(msg, "Updated project: integrated 1 commit into main");
        assert_eq!(head_ahead_behind(&repo), Some((1, 0)));
        assert_eq!(
            update_project(&repo, PullMode::Merge, true).unwrap(),
            "Project is up to date (main)"
        );

        // Rejected push after the remote moved, force-with-lease after rewriting.
        repo.run(&["commit", "-q", "--amend", "-m", "mine amended"])
            .unwrap();
        let mut spec = PushSpec::for_branch(&repo, "main").unwrap();
        push(&repo, &spec).unwrap();
        repo.run(&["commit", "-q", "--amend", "-m", "mine amended twice"])
            .unwrap();
        let err = push(&repo, &spec).unwrap_err();
        assert!(format!("{err:#}").contains("force-with-lease"), "{err:#}");
        spec.force_with_lease = true;
        assert_eq!(
            push(&repo, &spec).unwrap(),
            "Force-pushed main to origin/main"
        );

        // Delete a remote branch, then prune it on fetch.
        assert!(remote_branch_exists(&repo, "origin", "shared"));
        assert_eq!(
            delete_remote_branch(&repo, "origin", "shared").unwrap(),
            "Deleted origin/shared"
        );
        assert!(!remote_branch_exists(&repo, "origin", "shared"));
        other
            .run(&["push", "-q", "origin", "main:refs/heads/gone-soon"])
            .unwrap();
        repo.run(&["fetch", "-q"]).unwrap();
        other
            .run(&["push", "-q", "origin", "--delete", "gone-soon"])
            .unwrap();
        assert!(fetch_all(&repo, true).unwrap().contains("1 pruned"));
    }

    #[test]
    fn pull_conflicts_and_missing_upstream() {
        let (bare, _d, repo) = with_remote();
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        let err = pull(&repo, PullMode::Merge).unwrap_err();
        assert!(format!("{err:#}").contains("no upstream"));
        push(&repo, &PushSpec::for_branch(&repo, "main").unwrap()).unwrap();
        let (_cd, other) = clone_of(&bare);
        commit_files(&other, &[("a.txt", "theirs\n")], "theirs");
        other.run(&["push", "-q", "origin", "main"]).unwrap();
        commit_files(&repo, &[("a.txt", "ours\n")], "ours");
        let err = pull(&repo, PullMode::Merge).unwrap_err();
        assert!(format!("{err:#}").contains("Conflicts tab"), "{err:#}");
        assert_eq!(repo.state(), RepoState::Merging);
        repo.run(&["merge", "--abort"]).unwrap();

        // A branch pushed to a differently named target.
        branch::create_branch_with(&repo, "local-name", "", true, false).unwrap();
        let spec = PushSpec {
            target: "remote-name".into(),
            ..PushSpec::for_branch(&repo, "local-name").unwrap()
        };
        push(&repo, &spec).unwrap();
        assert_eq!(
            upstream_of(&repo, "local-name").unwrap().short(),
            "origin/remote-name"
        );
    }

    #[test]
    fn no_remotes() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "first");
        assert!(fetch_all(&repo, false).is_err());
        assert!(PushSpec::for_branch(&repo, "main").is_err());
        assert!(commits_to_push(&repo, "nope", "origin", "nope", 10)
            .unwrap()
            .is_empty());
        let c = terminal_command(&repo, &["fetch"]);
        assert_eq!(c.get_program(), "git");
    }
}
