//! Git backend. Everything runs the real `git` executable so hooks, credentials and config behave
//! exactly as on the command line, on Windows, macOS and Linux alike.
//!
//! Core (shared by all features): [`Repo`], [`status`], [`diff`], [`log`].
//! Feature modules each own one area; see the module docs.

pub mod diff;
pub mod log;
pub mod status;

pub mod admin;
pub mod blame;
pub mod branch;
pub mod changelist;
pub mod commit;
pub mod compare;
pub mod conflict;
pub mod file_history;
pub mod graph;
pub mod history;
pub mod hosting;
pub mod local_history;
pub mod partial;
pub mod patch;
pub mod rebase;
pub mod remote;
pub mod shelf;
pub mod stash;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Work tree root (absolute)
    pub root: PathBuf,
    /// This work tree's git dir (absolute; differs from `common_dir` for linked worktrees)
    pub git_dir: PathBuf,
    /// Shared git dir (absolute)
    pub common_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Merging,
    Rebasing,
    /// `git am` in progress
    ApplyingMailbox,
    CherryPicking,
    Reverting,
    Bisecting,
}

impl RepoState {
    pub fn label(self) -> &'static str {
        match self {
            RepoState::Clean => "",
            RepoState::Merging => "MERGING",
            RepoState::Rebasing => "REBASING",
            RepoState::ApplyingMailbox => "APPLYING PATCHES",
            RepoState::CherryPicking => "CHERRY-PICKING",
            RepoState::Reverting => "REVERTING",
            RepoState::Bisecting => "BISECTING",
        }
    }
}

impl Repo {
    /// Find the repository containing `path`.
    pub fn discover(path: &Path) -> Result<Repo> {
        let out = Command::new(git_program())
            .arg("-C")
            .arg(path)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
            ])
            .stdin(Stdio::null())
            .output()
            .context("could not run git — is Git installed and on your PATH?")?;
        if !out.status.success() {
            bail!(
                "{} is not inside a Git work tree ({})",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let mut next = || {
            lines
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("unexpected git rev-parse output"))
        };
        Ok(Repo {
            root: next()?,
            git_dir: next()?,
            common_dir: next()?,
        })
    }

    /// A `git` command preconfigured for this repo: runs in the root, never prompts on the terminal,
    /// never opens an interactive editor (git's proposed message is accepted), no colors, UTF-8 paths.
    pub fn command(&self) -> Command {
        let mut c = Command::new(git_program());
        c.arg("-C")
            .arg(&self.root)
            .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", crate::util::noop_editor())
            .env("GIT_SEQUENCE_EDITOR", crate::util::noop_editor())
            .env("GIT_MERGE_AUTOEDIT", "no")
            .env("GIT_PAGER", "cat")
            .stdin(Stdio::null());
        c
    }

    /// Run git and return stdout; non-zero exit becomes an error containing stderr.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let mut c = self.command();
        c.args(args);
        self.run_cmd(c, args)
    }

    /// Like [`run`](Self::run) but returns raw stdout bytes (file contents, binary-safe output).
    pub fn run_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut c = self.command();
        c.args(args);
        let out = exec(c, args)?;
        check(&out, args, &[0])?;
        Ok(out.stdout)
    }

    /// Run git; exit codes in `ok_codes` are success (e.g. `diff --no-index` exits 1 on differences).
    pub fn run_allow(&self, args: &[&str], ok_codes: &[i32]) -> Result<String> {
        let mut c = self.command();
        c.args(args);
        let out = exec(c, args)?;
        check(&out, args, ok_codes)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run git feeding `input` on stdin (patches for `apply`, pathspecs for `--pathspec-from-file=-`).
    pub fn run_stdin(&self, args: &[&str], input: &[u8]) -> Result<String> {
        let mut c = self.command();
        c.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = c.spawn().map_err(|e| spawn_error(e, args))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // Write on a thread so large inputs can't deadlock against a full stdout pipe.
        let data = input.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&data));
        let out = child.wait_with_output()?;
        let _ = writer.join();
        trace(args, started);
        check(&out, args, &[0])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a prepared command (from [`command`](Self::command), maybe with extra env) and return stdout.
    pub fn run_cmd(&self, c: Command, args_for_errors: &[&str]) -> Result<String> {
        let out = exec(c, args_for_errors)?;
        check(&out, args_for_errors, &[0])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run git, returning stdout only on success.
    pub fn try_run(&self, args: &[&str]) -> Option<String> {
        self.run(args).ok()
    }

    /// Absolute path of a repo-relative path.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Directory name of the work tree, for display.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Private storage for gitjet (changelists, shelf, local history, recents), shared by all worktrees.
    pub fn data_dir(&self) -> Result<PathBuf> {
        let d = self.common_dir.join("gitjet");
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        Ok(d)
    }

    pub fn state(&self) -> RepoState {
        let g = &self.git_dir;
        if g.join("rebase-merge").exists() {
            RepoState::Rebasing
        } else if g.join("rebase-apply").exists() {
            if g.join("rebase-apply").join("applying").exists() {
                RepoState::ApplyingMailbox
            } else {
                RepoState::Rebasing
            }
        } else if g.join("MERGE_HEAD").exists() {
            RepoState::Merging
        } else if g.join("CHERRY_PICK_HEAD").exists() {
            RepoState::CherryPicking
        } else if g.join("REVERT_HEAD").exists() {
            RepoState::Reverting
        } else if g.join("BISECT_LOG").exists() {
            RepoState::Bisecting
        } else {
            RepoState::Clean
        }
    }

    /// Current branch name, or None when HEAD is detached.
    pub fn head_branch(&self) -> Option<String> {
        self.try_run(&["symbolic-ref", "--short", "-q", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Full hash of HEAD, or None in an empty repository.
    pub fn head_commit(&self) -> Option<String> {
        self.try_run(&["rev-parse", "--verify", "-q", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn config_get(&self, key: &str) -> Option<String> {
        self.try_run(&["config", "--get", key])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// True when git's commit-graph file exists (single file or split chain).
    pub fn has_commit_graph(&self) -> bool {
        let info = self.common_dir.join("objects").join("info");
        info.join("commit-graph").exists()
            || info
                .join("commit-graphs")
                .join("commit-graph-chain")
                .exists()
    }

    /// Write the commit-graph when the repository is large and has none. Returns whether it wrote one.
    /// Small repositories are skipped: there the graph doesn't matter and we don't touch `.git`.
    pub fn ensure_commit_graph(&self) -> Result<bool> {
        if self.has_commit_graph() {
            return Ok(false);
        }
        let objects: u64 = self
            .run(&["count-objects", "-v"])?
            .lines()
            .filter_map(|l| l.split_once(": "))
            .filter(|(k, _)| *k == "count" || *k == "in-pack")
            .filter_map(|(_, v)| v.trim().parse::<u64>().ok())
            .sum();
        let refs = self.run(&["for-each-ref", "--format=x"])?.lines().count();
        if objects < LARGE_REPO_OBJECTS && refs < LARGE_REPO_REFS {
            return Ok(false);
        }
        self.run(&["commit-graph", "write", "--reachable"])?;
        Ok(true)
    }

    /// Resolve any revision to a full hash.
    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        Ok(self
            .run(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?
            .trim()
            .to_string())
    }
}

/// Object and ref counts from which a repository counts as large (see [`Repo::ensure_commit_graph`]).
const LARGE_REPO_OBJECTS: u64 = 20_000;
const LARGE_REPO_REFS: usize = 300;

fn spawn_error(e: std::io::Error, args: &[&str]) -> anyhow::Error {
    anyhow!(
        "could not run `git {}`: {e} — is Git installed and on your PATH?",
        args.join(" ")
    )
}

fn exec(mut c: Command, args: &[&str]) -> Result<Output> {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let started = Instant::now();
    let out = c.output().map_err(|e| spawn_error(e, args));
    trace(args, started);
    out
}

/// The git executable: `GITJET_GIT` when set (e.g. the real `git.exe` instead of a package
/// manager shim, which costs an extra process per call on Windows), else `git` from PATH.
pub fn git_program() -> &'static OsString {
    static PROGRAM: OnceLock<OsString> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        std::env::var_os("GITJET_GIT")
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "git".into())
    })
}

/// With `GITJET_TRACE=<file>`, append each git command and how long it took to `<file>`
/// (to find what makes the UI slow; the terminal itself is busy drawing).
fn trace(args: &[&str], started: Instant) {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("GITJET_TRACE").filter(|p| !p.is_empty())?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    });
    if let Some(Ok(mut f)) = file.as_ref().map(Mutex::lock) {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let thread = std::thread::current();
        let on_ui = thread.name() == Some("main");
        let _ = writeln!(
            f,
            "{ms:8.1} ms {} git {}",
            if on_ui { "[UI]" } else { "[bg]" },
            args.join(" ")
        );
    }
}

fn check(out: &Output, args: &[&str], ok_codes: &[i32]) -> Result<()> {
    let code = out.status.code().unwrap_or(-1);
    if ok_codes.contains(&code) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let msg = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let shown: Vec<&str> = args.iter().take(4).copied().collect();
    bail!(
        "git {}{} failed: {}",
        shown.join(" "),
        if args.len() > 4 { " …" } else { "" },
        if msg.is_empty() {
            format!("exit code {code}")
        } else {
            msg.to_string()
        }
    )
}

/// Test helpers: temporary repositories with deterministic identity, usable from any module's tests.
#[cfg(test)]
pub mod testutil {
    use super::*;
    pub use tempfile::TempDir;

    /// A fresh repository on branch `main` with user identity configured and signing disabled.
    pub fn temp_repo() -> (TempDir, Repo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ok = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(dir.path())
            .status()
            .expect("git init")
            .success();
        assert!(ok, "git init failed");
        let repo = Repo::discover(dir.path()).expect("discover");
        for (k, v) in [
            ("user.name", "Test User"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.autocrlf", "false"),
            ("init.defaultBranch", "main"),
        ] {
            repo.run(&["config", k, v]).unwrap();
        }
        (dir, repo)
    }

    /// Write `content` to `rel` (creating parent dirs).
    pub fn write(repo: &Repo, rel: &str, content: &str) {
        let p = repo.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    /// Write files, stage everything and commit; returns the new commit hash.
    pub fn commit_files(repo: &Repo, files: &[(&str, &str)], message: &str) -> String {
        for (rel, content) in files {
            write(repo, rel, content);
        }
        repo.run(&["add", "-A"]).unwrap();
        repo.run(&["commit", "-q", "-m", message]).unwrap();
        repo.head_commit().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn discover_and_state() {
        let (_d, repo) = temp_repo();
        assert_eq!(repo.state(), RepoState::Clean);
        assert!(repo.head_commit().is_none());
        commit_files(&repo, &[("a.txt", "hi\n")], "first");
        assert_eq!(repo.head_branch().as_deref(), Some("main"));
        assert!(repo.data_dir().unwrap().exists());
        let sub = repo.root.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(Repo::discover(&sub).unwrap().root, repo.root);
    }

    #[test]
    fn commit_graph_only_for_large_repos() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "one");
        assert!(
            !repo.ensure_commit_graph().unwrap(),
            "small repo is left alone"
        );
        assert!(!repo.has_commit_graph());
        for i in 0..LARGE_REPO_REFS {
            repo.run(&["branch", &format!("b{i}")]).unwrap();
        }
        assert!(repo.ensure_commit_graph().unwrap());
        assert!(repo.has_commit_graph());
        assert!(!repo.ensure_commit_graph().unwrap(), "written only once");
    }

    #[test]
    fn errors_carry_stderr() {
        let (_d, repo) = temp_repo();
        let err = repo.run(&["rev-parse", "--verify", "nope"]).unwrap_err();
        assert!(format!("{err:#}").contains("failed"));
    }

    #[test]
    fn stdin_and_allowed_codes() {
        let (_d, repo) = temp_repo();
        let h = repo
            .run_stdin(&["hash-object", "--stdin"], b"hello\n")
            .unwrap();
        assert_eq!(h.trim().len(), 40);
        write(&repo, "x", "1\n");
        write(&repo, "y", "2\n");
        let d = repo
            .run_allow(&["diff", "--no-index", "--", "x", "y"], &[0, 1])
            .unwrap();
        assert!(d.contains("-1"));
    }
}
