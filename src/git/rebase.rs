//! Interactive rebase: todo list building, reword/squash/fixup/drop/edit/reorder, continue/abort/skip.
//!
//! The plan the user builds in the editor ([`RebasePlan`]) is turned into a git todo file
//! ([`build_todo`]) which replaces the one git proposes via `GIT_SEQUENCE_EDITOR`. Rewording uses
//! `exec git commit --amend` lines so any number of messages can be changed in one run without an
//! interactive editor. Everything here is a free function to keep the `Repo` namespace small.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};

use super::log::{Commit, LogQuery};
use super::{Repo, RepoState};

/// Pseudo-base meaning "the whole history" (`git rebase -i --root`).
pub const ROOT: &str = "--root";

/// Longest range the editor accepts; beyond this a closer base must be chosen.
pub const MAX_COMMITS: usize = 2000;

/// What to do with one commit, in git's todo vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TodoAction {
    Pick,
    /// Pick, then replace the message (of the commit, or of the squash group it starts).
    Reword,
    /// Stop after applying so the commit can be amended.
    Edit,
    /// Meld into the previous commit, combining messages.
    Squash,
    /// Meld into the previous commit, discarding this message.
    Fixup,
    Drop,
}

impl TodoAction {
    /// Every action, in menu order.
    pub const ALL: [TodoAction; 6] = [
        TodoAction::Pick,
        TodoAction::Reword,
        TodoAction::Edit,
        TodoAction::Squash,
        TodoAction::Fixup,
        TodoAction::Drop,
    ];

    /// Lower-case name as shown in the editor and accepted by git.
    pub fn label(self) -> &'static str {
        match self {
            TodoAction::Pick => "pick",
            TodoAction::Reword => "reword",
            TodoAction::Edit => "edit",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
        }
    }

    /// The editor key that selects this action.
    pub fn key(self) -> char {
        match self {
            TodoAction::Pick => 'p',
            TodoAction::Reword => 'r',
            TodoAction::Edit => 'e',
            TodoAction::Squash => 's',
            TodoAction::Fixup => 'f',
            TodoAction::Drop => 'd',
        }
    }

    /// The verb written to the todo file (rewording is done with an `exec` line after a pick).
    pub fn verb(self) -> &'static str {
        match self {
            TodoAction::Reword => "pick",
            other => other.label(),
        }
    }

    /// Squash and fixup meld into the previous commit.
    pub fn folds(self) -> bool {
        matches!(self, TodoAction::Squash | TodoAction::Fixup)
    }
}

/// One line of the plan: a commit and what to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoEntry {
    pub commit: Commit,
    pub action: TodoAction,
    /// The replacement message; set exactly when `action` is [`TodoAction::Reword`]. When squash or
    /// fixup entries follow, it becomes the message of the whole melded commit.
    pub message: Option<String>,
}

impl TodoEntry {
    pub fn pick(commit: Commit) -> Self {
        Self {
            commit,
            action: TodoAction::Pick,
            message: None,
        }
    }
}

/// Commits that an interactive rebase onto `base` would replay, plus facts worth warning about.
#[derive(Debug, Clone)]
pub struct RebaseRange {
    /// The base as given (a revision, or [`ROOT`]).
    pub base: String,
    /// HEAD when the range was read; starting refuses to run if HEAD moved since.
    pub head: String,
    /// Non-merge commits, oldest first (git todo order).
    pub commits: Vec<Commit>,
    /// Merge commits in the range: they are not replayed, history becomes linear.
    pub merges: usize,
    /// Ids of listed commits already present on a remote-tracking branch.
    pub pushed: HashSet<String>,
}

/// What the user asked for: a base and the edited todo list.
#[derive(Debug, Clone)]
pub struct RebasePlan {
    pub base: String,
    pub head: String,
    pub entries: Vec<TodoEntry>,
}

/// How git's editors are run during a rebase. Injectable so tests can avoid the gitjet helper.
pub struct Editors {
    /// Builds `GIT_SEQUENCE_EDITOR`: a command that overwrites the file git opens with the given todo.
    pub sequence: Box<dyn Fn(&Path) -> String>,
    /// `GIT_EDITOR`: accepts whatever message git proposes (squash combinations, continue).
    pub message: String,
}

impl Editors {
    /// The gitjet binary acting as both editors.
    pub fn production() -> Self {
        Self {
            sequence: Box::new(crate::util::write_file_editor),
            message: crate::util::noop_editor().to_string(),
        }
    }
}

/// Where a rebase in progress stands (read from `.git/rebase-merge` or `.git/rebase-apply`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// 1-based number of the step being applied (0 when unknown).
    pub step: usize,
    pub total: usize,
    /// Branch being rebased (`None` when the rebase started from a detached HEAD).
    pub head_name: Option<String>,
    /// Short hash of the commit being rebased onto.
    pub onto: Option<String>,
    /// Full hash of the commit the rebase stopped at.
    pub stopped_sha: Option<String>,
    pub stopped_subject: Option<String>,
    /// Stopped at an `edit` line so the commit can be amended.
    pub editing: bool,
    /// Remaining todo lines (without comments).
    pub remaining: Vec<String>,
}

impl Progress {
    /// "step 3/7 at abc1234 subject" style one-liner.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        if self.total > 0 {
            s.push_str(&format!(
                "step {}/{}",
                self.step.min(self.total),
                self.total
            ));
        }
        if let Some(sha) = &self.stopped_sha {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&format!("at {}", short(sha)));
            if let Some(subj) = &self.stopped_subject {
                s.push_str(&format!(" \"{subj}\""));
            }
        }
        s
    }
}

/// Result of starting, continuing or skipping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The rebase completed.
    Done,
    /// Stopped at an `edit` line: amend the commit, then continue.
    Paused(Progress),
    /// Stopped on merge conflicts in these paths.
    Conflicts {
        paths: Vec<String>,
        progress: Progress,
    },
    /// Stopped for another reason (an `exec` failed, a commit became empty...), with git's message.
    Stopped { message: String, progress: Progress },
}

impl Outcome {
    /// A user-facing summary of what happened and what to do next.
    pub fn message(&self) -> String {
        match self {
            Outcome::Done => "Rebase finished successfully".into(),
            Outcome::Paused(p) => format!(
                "Rebase stopped for editing ({}). Amend the commit, then run \"Continue rebase\" (:)",
                p.describe()
            ),
            Outcome::Conflicts { paths, progress } => format!(
                "Rebase stopped ({}): {} conflicted file{} — resolve them in the Conflicts tab, then \"Continue rebase\"",
                progress.describe(),
                paths.len(),
                if paths.len() == 1 { "" } else { "s" }
            ),
            Outcome::Stopped { message, progress } => {
                format!("Rebase stopped ({}):\n{message}", progress.describe())
            }
        }
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// True while a rebase (interactive or not) is in progress.
pub fn is_rebasing(repo: &Repo) -> bool {
    repo.state() == RepoState::Rebasing
}

/// Read the commits `base..HEAD` (base exclusive, or all history for [`ROOT`]) the way git's own
/// todo would list them: no merges, no commits already applied upstream, oldest first.
pub fn load_range(repo: &Repo, base: &str) -> Result<RebaseRange> {
    let head = repo
        .head_commit()
        .ok_or_else(|| anyhow!("Nothing to rebase: the repository has no commits yet"))?;
    let base = base.trim();
    let range_revs: Vec<String> = if base == ROOT {
        vec!["HEAD".into()]
    } else {
        repo.rev_parse(base)
            .with_context(|| format!("Unknown base revision '{base}'"))?;
        vec![
            "--cherry-pick".into(),
            "--right-only".into(),
            format!("{base}...HEAD"),
        ]
    };
    let q = LogQuery {
        revs: range_revs,
        no_merges: true,
        max: MAX_COMMITS + 1,
        ..Default::default()
    };
    let mut commits = repo.log(&q)?;
    if commits.len() > MAX_COMMITS {
        bail!("More than {MAX_COMMITS} commits after '{base}' — choose a closer base revision");
    }
    if commits.is_empty() {
        bail!("Nothing to rebase: no commits between '{base}' and HEAD");
    }
    commits.reverse();
    let exclusive = if base == ROOT {
        "HEAD".to_string()
    } else {
        format!("{base}..HEAD")
    };
    let merges = repo
        .run(&["rev-list", "--count", "--merges", &exclusive])?
        .trim()
        .parse()
        .unwrap_or(0);
    let pushed = pushed_commits(repo, base, &commits)?;
    Ok(RebaseRange {
        base: base.to_string(),
        head,
        commits,
        merges,
        pushed,
    })
}

/// Listed commits that are reachable from some remote-tracking branch.
fn pushed_commits(repo: &Repo, base: &str, commits: &[Commit]) -> Result<HashSet<String>> {
    let has_remote_refs = !repo
        .run(&[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "refs/remotes",
        ])?
        .trim()
        .is_empty();
    if !has_remote_refs {
        return Ok(HashSet::new());
    }
    let not_base = format!("^{base}");
    let mut args = vec!["rev-list", "HEAD"];
    if base != ROOT {
        args.push(&not_base);
    }
    args.extend(["--not", "--remotes"]);
    let unpushed: HashSet<String> = repo
        .run(&args)?
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    Ok(commits
        .iter()
        .filter(|c| !unpushed.contains(&c.id))
        .map(|c| c.id.clone())
        .collect())
}

/// Base for "squash the last `n` commits": `HEAD~n`, or [`ROOT`] when that is the whole history.
pub fn squash_base(repo: &Repo, n: usize) -> Result<String> {
    if n < 2 {
        bail!("Choose at least 2 commits to squash");
    }
    let total: usize = repo
        .run(&["rev-list", "--first-parent", "--count", "HEAD"])
        .map_err(|_| anyhow!("Nothing to squash: the repository has no commits yet"))?
        .trim()
        .parse()
        .unwrap_or(0);
    if n > total {
        bail!(
            "The current branch has only {total} commit{}",
            if total == 1 { "" } else { "s" }
        );
    }
    if n == total {
        return Ok(ROOT.into());
    }
    repo.rev_parse(&format!("HEAD~{n}"))
}

/// Base that makes `rev` the first commit of the range, after checking it can be rewritten.
pub fn base_for_commit(repo: &Repo, rev: &str) -> Result<(String, String)> {
    let c = repo
        .commit_info(rev)
        .with_context(|| format!("Unknown commit '{rev}'"))?;
    if c.is_merge() {
        bail!(
            "{} is a merge commit; it cannot be rewritten with an interactive rebase",
            c.short
        );
    }
    let mut cmd = repo.command();
    cmd.args(["merge-base", "--is-ancestor", &c.id, "HEAD"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let ancestor = cmd.status().map(|s| s.success()).unwrap_or(false);
    if !ancestor {
        bail!("{} is not part of the current branch", c.short);
    }
    let base = match c.parents.first() {
        Some(p) => p.clone(),
        None => ROOT.to_string(),
    };
    Ok((base, c.id))
}

/// Check a plan can be executed. Returns the problem as a user-facing message.
pub fn validate(entries: &[TodoEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("The todo list is empty");
    }
    if let Some(first) = entries.iter().find(|e| e.action != TodoAction::Drop) {
        if first.action.folds() {
            bail!(
                "{} {}: the first commit cannot be squashed or fixed up — there is no earlier commit to meld into",
                first.commit.short,
                first.action.label()
            );
        }
    }
    if let Some(e) = entries.iter().find(|e| {
        e.action == TodoAction::Reword && e.message.as_deref().map_or(true, |m| m.trim().is_empty())
    }) {
        bail!("{}: reword needs a non-empty message", e.commit.short);
    }
    Ok(())
}

/// Index of the commit that entry `i` melds into (itself unless it is a squash/fixup).
/// `None` when a squash/fixup has nothing before it (invalid plan).
pub fn group_target(entries: &[TodoEntry], i: usize) -> Option<usize> {
    let e = entries.get(i)?;
    if !e.action.folds() {
        return Some(i);
    }
    (0..i)
        .rev()
        .find(|&j| entries[j].action != TodoAction::Drop && !entries[j].action.folds())
}

/// Indices of the squash/fixup entries melding into the target at `target`.
pub fn group_members(entries: &[TodoEntry], target: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for (j, e) in entries.iter().enumerate().skip(target + 1) {
        match e.action {
            TodoAction::Drop => continue,
            a if a.folds() => out.push(j),
            _ => break,
        }
    }
    out
}

/// Reword targets: (entry index, new message) for every entry that needs an amend after its group.
pub fn reword_messages(entries: &[TodoEntry]) -> Vec<(usize, String)> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.action == TodoAction::Reword)
        .filter_map(|(i, e)| e.message.clone().map(|m| (i, m)))
        .collect()
}

/// The todo file text. `msg_files` maps reword entry indices to their message file paths.
pub fn build_todo(entries: &[TodoEntry], msg_files: &HashMap<usize, String>) -> String {
    let mut out = String::new();
    // A pending amend: (message file) emitted once its squash group ends.
    let mut pending: Option<&str> = None;
    for (i, e) in entries.iter().enumerate() {
        if !e.action.folds() && e.action != TodoAction::Drop {
            flush_amend(&mut out, &mut pending);
        }
        out.push_str(&format!(
            "{} {} {}\n",
            e.action.verb(),
            e.commit.id,
            e.commit.subject
        ));
        if e.action == TodoAction::Reword {
            pending = msg_files.get(&i).map(String::as_str);
        }
    }
    flush_amend(&mut out, &mut pending);
    out
}

fn flush_amend(out: &mut String, pending: &mut Option<&str>) {
    if let Some(path) = pending.take() {
        out.push_str(&format!(
            "exec git commit --amend --only --no-verify -F {}\n",
            crate::util::sh_quote(path)
        ));
    }
}

/// Counts per action and whether the order changed, e.g. "3 pick, 1 reword · order changed".
pub fn summarize(entries: &[TodoEntry], original: &[String]) -> String {
    let mut parts = Vec::new();
    for a in TodoAction::ALL {
        let n = entries.iter().filter(|e| e.action == a).count();
        if n > 0 {
            parts.push(format!("{n} {}", a.label()));
        }
    }
    let ids: Vec<&str> = entries.iter().map(|e| e.commit.id.as_str()).collect();
    let orig: Vec<&str> = original.iter().map(String::as_str).collect();
    let mut s = parts.join(", ");
    if ids != orig {
        s.push_str(" · order changed");
    }
    s
}

/// Warnings to show before starting (pushed commits rewritten, merges linearized, everything dropped).
pub fn warnings(range: &RebaseRange, entries: &[TodoEntry]) -> Vec<String> {
    let mut w = Vec::new();
    if range.merges > 0 {
        w.push(format!(
            "{} merge commit{} in the range will not be kept — history becomes linear",
            range.merges,
            if range.merges == 1 { "" } else { "s" }
        ));
    }
    let pushed = entries
        .iter()
        .filter(|e| range.pushed.contains(&e.commit.id))
        .count();
    if pushed > 0 {
        w.push(format!(
            "{pushed} commit{} already pushed — rewriting {} needs a force push",
            if pushed == 1 { " is" } else { "s are" },
            if pushed == 1 { "it" } else { "them" }
        ));
    }
    if !entries.is_empty() && entries.iter().all(|e| e.action == TodoAction::Drop) {
        w.push("Every commit is dropped — the branch will be reset to the base".into());
    }
    w
}

/// Write the todo and message files into gitjet's data dir; returns the todo path.
fn prepare_files(repo: &Repo, entries: &[TodoEntry]) -> Result<PathBuf> {
    let dir = repo.data_dir()?.join("rebase");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut files = HashMap::new();
    for (i, msg) in reword_messages(entries) {
        let p = dir.join(format!("msg-{i}.txt"));
        let mut text = msg.replace("\r\n", "\n").trim_end().to_string();
        text.push('\n');
        std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
        files.insert(i, p.to_string_lossy().into_owned());
    }
    let todo = dir.join("git-rebase-todo");
    std::fs::write(&todo, build_todo(entries, &files))
        .with_context(|| format!("writing {}", todo.display()))?;
    Ok(todo)
}

/// Run the plan: `git rebase -i --autostash <base>` with our todo replacing git's.
pub fn start(repo: &Repo, plan: &RebasePlan, editors: &Editors) -> Result<Outcome> {
    if is_rebasing(repo) {
        bail!("A rebase is already in progress — continue, skip or abort it first");
    }
    validate(&plan.entries).map_err(|e| anyhow!("{e:#}"))?;
    if repo.head_commit().as_deref() != Some(plan.head.as_str()) {
        bail!(
            "HEAD moved since the rebase editor was opened — reopen it to see the current commits"
        );
    }
    let todo = prepare_files(repo, &plan.entries)?;
    let mut args = vec!["rebase", "-i", "--autostash"];
    if plan.base == ROOT {
        args.push("--root");
    } else {
        args.push(&plan.base);
    }
    let mut c = repo.command();
    c.args(&args)
        .env("GIT_SEQUENCE_EDITOR", (editors.sequence)(&todo))
        .env("GIT_EDITOR", &editors.message);
    let res = repo.run_cmd(c, &args);
    outcome_after(repo, res)
}

fn run_step(repo: &Repo, flag: &str, editors: &Editors) -> Result<Outcome> {
    if !is_rebasing(repo) {
        bail!("No rebase in progress");
    }
    let args = ["rebase", flag];
    let mut c = repo.command();
    c.args(args).env("GIT_EDITOR", &editors.message);
    let res = repo.run_cmd(c, &args);
    outcome_after(repo, res)
}

/// `git rebase --continue`, accepting git's proposed message.
pub fn continue_with(repo: &Repo, editors: &Editors) -> Result<Outcome> {
    run_step(repo, "--continue", editors)
}

/// `git rebase --skip`: drop the commit the rebase stopped at and go on.
pub fn skip_with(repo: &Repo, editors: &Editors) -> Result<Outcome> {
    run_step(repo, "--skip", editors)
}

/// [`continue_with`] using gitjet's own editor helper.
pub fn continue_rebase(repo: &Repo) -> Result<Outcome> {
    continue_with(repo, &Editors::production())
}

/// [`skip_with`] using gitjet's own editor helper.
pub fn skip(repo: &Repo) -> Result<Outcome> {
    skip_with(repo, &Editors::production())
}

/// `git rebase --abort`: restore the branch (and autostashed changes) as before the rebase.
pub fn abort(repo: &Repo) -> Result<()> {
    if !is_rebasing(repo) {
        bail!("No rebase in progress");
    }
    repo.run(&["rebase", "--abort"]).map(|_| ())
}

/// Classify the state after a rebase command ran with result `res`.
fn outcome_after(repo: &Repo, res: Result<String>) -> Result<Outcome> {
    if !is_rebasing(repo) {
        return res.map(|_| Outcome::Done);
    }
    let progress = progress(repo).unwrap_or_default();
    let paths: Vec<String> = repo
        .status()?
        .conflicted()
        .map(|e| e.path.clone())
        .collect();
    if !paths.is_empty() {
        return Ok(Outcome::Conflicts { paths, progress });
    }
    match res {
        Err(e) if !progress.editing => Ok(Outcome::Stopped {
            message: format!("{e:#}"),
            progress,
        }),
        _ => Ok(Outcome::Paused(progress)),
    }
}

fn read_trim(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_num(dir: &Path, name: &str) -> usize {
    read_trim(dir, name)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Current step information, or `None` when no rebase is in progress.
pub fn progress(repo: &Repo) -> Option<Progress> {
    let merge = repo.git_dir.join("rebase-merge");
    let apply = repo.git_dir.join("rebase-apply");
    let mut p = Progress::default();
    let dir = if merge.is_dir() {
        p.step = read_num(&merge, "msgnum");
        p.total = read_num(&merge, "end");
        p.editing = merge.join("amend").exists();
        p.stopped_sha = read_trim(&merge, "stopped-sha").or_else(|| last_done_commit(&merge));
        p.remaining = std::fs::read_to_string(merge.join("git-rebase-todo"))
            .map(|s| todo_lines(&s))
            .unwrap_or_default();
        merge
    } else if apply.is_dir() {
        p.step = read_num(&apply, "next");
        p.total = read_num(&apply, "last");
        p.stopped_sha = read_trim(&apply, "original-commit");
        apply
    } else {
        return None;
    };
    p.head_name = read_trim(&dir, "head-name")
        .filter(|h| h != "detached HEAD")
        .map(|h| h.trim_start_matches("refs/heads/").to_string());
    p.onto = read_trim(&dir, "onto").map(|o| short(&o).to_string());
    if let Some(sha) = p.stopped_sha.clone() {
        let full = repo.rev_parse(&sha).unwrap_or(sha);
        p.stopped_subject = repo
            .try_run(&["show", "-s", "--format=%s", &full, "--"])
            .map(|s| s.trim().to_string());
        p.stopped_sha = Some(full);
    }
    Some(p)
}

/// The commit of the last applied todo line (fallback when `stopped-sha` is absent).
fn last_done_commit(dir: &Path) -> Option<String> {
    let done = std::fs::read_to_string(dir.join("done")).ok()?;
    let lines = todo_lines(&done);
    lines.iter().rev().find_map(|l| {
        let mut it = l.split_whitespace();
        let verb = it.next()?;
        let is_commit_verb = matches!(
            verb,
            "pick" | "p" | "reword" | "r" | "edit" | "e" | "squash" | "s" | "fixup" | "f"
        );
        is_commit_verb
            .then(|| it.next().map(str::to_string))
            .flatten()
    })
}

/// Non-empty, non-comment todo lines (CRLF tolerant).
pub fn todo_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::*;

    fn fake_commit(id: &str, subject: &str) -> Commit {
        Commit {
            id: id.into(),
            short: id.chars().take(7).collect(),
            parents: vec![],
            author: "A".into(),
            email: "a@x".into(),
            time: 0,
            committer: "A".into(),
            committer_email: "a@x".into(),
            commit_time: 0,
            subject: subject.into(),
            refs: vec![],
        }
    }

    fn entry(id: &str, action: TodoAction, message: Option<&str>) -> TodoEntry {
        TodoEntry {
            commit: fake_commit(id, &format!("subject {id}")),
            action,
            message: message.map(str::to_string),
        }
    }

    #[test]
    fn todo_for_simple_actions() {
        let entries = vec![
            entry("aaa", TodoAction::Pick, None),
            entry("bbb", TodoAction::Edit, None),
            entry("ccc", TodoAction::Drop, None),
            entry("ddd", TodoAction::Fixup, None),
        ];
        let todo = build_todo(&entries, &HashMap::new());
        assert_eq!(
            todo,
            "pick aaa subject aaa\nedit bbb subject bbb\ndrop ccc subject ccc\nfixup ddd subject ddd\n"
        );
    }

    #[test]
    fn reword_exec_goes_after_its_squash_group() {
        let entries = vec![
            entry("aaa", TodoAction::Reword, Some("new a")),
            entry("bbb", TodoAction::Squash, None),
            entry("ccc", TodoAction::Drop, None),
            entry("ddd", TodoAction::Fixup, None),
            entry("eee", TodoAction::Reword, Some("new e")),
        ];
        let files: HashMap<usize, String> = [
            (0, "/m/a b.txt".to_string()),
            (4, "C:\\m\\e.txt".to_string()),
        ]
        .into_iter()
        .collect();
        let lines: Vec<String> = build_todo(&entries, &files)
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines[0], "pick aaa subject aaa");
        assert_eq!(lines[1], "squash bbb subject bbb");
        assert_eq!(lines[2], "drop ccc subject ccc");
        assert_eq!(lines[3], "fixup ddd subject ddd");
        assert_eq!(
            lines[4],
            "exec git commit --amend --only --no-verify -F '/m/a b.txt'"
        );
        assert_eq!(lines[5], "pick eee subject eee");
        assert_eq!(
            lines[6],
            "exec git commit --amend --only --no-verify -F 'C:/m/e.txt'"
        );
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn validation_rules() {
        assert!(validate(&[]).is_err());
        let bad = vec![
            entry("a", TodoAction::Drop, None),
            entry("b", TodoAction::Squash, None),
        ];
        let err = format!("{:#}", validate(&bad).unwrap_err());
        assert!(err.contains("first commit"), "{err}");
        let ok = vec![
            entry("a", TodoAction::Pick, None),
            entry("b", TodoAction::Fixup, None),
        ];
        assert!(validate(&ok).is_ok());
        let empty_msg = vec![entry("a", TodoAction::Reword, Some("  "))];
        assert!(validate(&empty_msg).is_err());
    }

    #[test]
    fn groups() {
        let e = vec![
            entry("a", TodoAction::Pick, None),
            entry("b", TodoAction::Squash, None),
            entry("c", TodoAction::Drop, None),
            entry("d", TodoAction::Fixup, None),
            entry("e", TodoAction::Pick, None),
        ];
        assert_eq!(group_target(&e, 3), Some(0));
        assert_eq!(group_target(&e, 4), Some(4));
        assert_eq!(group_members(&e, 0), vec![1, 3]);
        assert!(group_members(&e, 4).is_empty());
        let invalid = vec![entry("a", TodoAction::Squash, None)];
        assert_eq!(group_target(&invalid, 0), None);
    }

    #[test]
    fn summary_and_todo_lines() {
        let e = vec![
            entry("b", TodoAction::Pick, None),
            entry("a", TodoAction::Drop, None),
        ];
        let s = summarize(&e, &["a".into(), "b".into()]);
        assert_eq!(s, "1 pick, 1 drop · order changed");
        assert_eq!(
            todo_lines("pick a x\r\n# comment\r\n\r\nexec true\r\n"),
            vec!["pick a x".to_string(), "exec true".to_string()]
        );
        let p = Progress {
            step: 2,
            total: 5,
            stopped_sha: Some("0123456789".into()),
            stopped_subject: Some("hi".into()),
            ..Default::default()
        };
        assert_eq!(p.describe(), "step 2/5 at 0123456 \"hi\"");
    }

    // ----- real repositories ---------------------------------------------------------------

    /// Repo with commits c1..cN each adding file fK.txt; returns hashes oldest first.
    fn linear(repo: &Repo, n: usize) -> Vec<String> {
        (1..=n)
            .map(|k| {
                commit_files(
                    repo,
                    &[(&format!("f{k}.txt"), &format!("{k}\n"))],
                    &format!("c{k}"),
                )
            })
            .collect()
    }

    #[cfg(unix)]
    fn subjects(repo: &Repo) -> Vec<String> {
        repo.run(&["log", "--format=%s"])
            .unwrap()
            .lines()
            .map(|l| l.trim().to_string())
            .collect()
    }

    #[cfg(unix)]
    fn test_editors() -> Editors {
        Editors {
            sequence: Box::new(|p: &Path| {
                format!("cp {}", crate::util::sh_quote(&p.to_string_lossy()))
            }),
            message: "true".into(),
        }
    }

    fn plan_from(range: &RebaseRange) -> RebasePlan {
        RebasePlan {
            base: range.base.clone(),
            head: range.head.clone(),
            entries: range.commits.iter().cloned().map(TodoEntry::pick).collect(),
        }
    }

    #[test]
    fn range_listing_and_warnings() {
        let (_d, repo) = temp_repo();
        assert!(load_range(&repo, "HEAD").is_err(), "empty repo");
        let h = linear(&repo, 3);
        let r = load_range(&repo, &h[0]).unwrap();
        let subj: Vec<&str> = r.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subj, vec!["c2", "c3"], "oldest first, base exclusive");
        assert!(r.pushed.is_empty());
        assert_eq!(r.merges, 0);
        assert_eq!(load_range(&repo, ROOT).unwrap().commits.len(), 3);
        assert!(load_range(&repo, "HEAD").is_err(), "nothing to rebase");
        assert!(load_range(&repo, "no-such-rev").is_err());

        // Pushed detection through a remote-tracking ref.
        repo.run(&["update-ref", "refs/remotes/origin/main", &h[1]])
            .unwrap();
        let r = load_range(&repo, ROOT).unwrap();
        assert!(r.pushed.contains(&h[0]) && r.pushed.contains(&h[1]) && !r.pushed.contains(&h[2]));
        let plan = plan_from(&r);
        let w = warnings(&r, &plan.entries);
        assert!(
            w.iter().any(|w| w.contains("2 commits are already pushed")),
            "{w:?}"
        );

        // Merge commits are counted and left out of the list.
        repo.run(&["checkout", "-q", "-b", "side", &h[1]]).unwrap();
        commit_files(&repo, &[("side.txt", "s\n")], "side");
        repo.run(&["checkout", "-q", "main"]).unwrap();
        repo.run(&["merge", "-q", "--no-ff", "-m", "merge side", "side"])
            .unwrap();
        let r = load_range(&repo, &h[0]).unwrap();
        assert_eq!(r.merges, 1);
        assert!(r.commits.iter().all(|c| !c.is_merge()));
        assert!(warnings(&r, &plan_from(&r).entries)
            .iter()
            .any(|w| w.contains("merge")));
    }

    #[test]
    fn squash_and_reword_bases() {
        let (_d, repo) = temp_repo();
        assert!(squash_base(&repo, 2).is_err());
        let h = linear(&repo, 3);
        assert!(squash_base(&repo, 1).is_err());
        assert!(squash_base(&repo, 4).is_err());
        assert_eq!(squash_base(&repo, 2).unwrap(), h[0]);
        assert_eq!(squash_base(&repo, 3).unwrap(), ROOT);
        assert_eq!(
            base_for_commit(&repo, &h[1]).unwrap(),
            (h[0].clone(), h[1].clone())
        );
        assert_eq!(base_for_commit(&repo, &h[0]).unwrap().0, ROOT);
        repo.run(&["checkout", "-q", "-b", "other", &h[0]]).unwrap();
        let o = commit_files(&repo, &[("o.txt", "o\n")], "other");
        repo.run(&["checkout", "-q", "main"]).unwrap();
        assert!(base_for_commit(&repo, &o).is_err(), "not on branch");
    }

    #[cfg(unix)]
    #[test]
    fn reorder_drop_and_reword() {
        let (_d, repo) = temp_repo();
        let h = linear(&repo, 4);
        let range = load_range(&repo, &h[0]).unwrap();
        let mut plan = plan_from(&range);
        // c2 c3 c4 -> c4 (reworded), c2, drop c3
        let c4 = plan.entries.remove(2);
        plan.entries.insert(0, c4);
        plan.entries[0].action = TodoAction::Reword;
        plan.entries[0].message = Some("fourth\n\nwith body".into());
        plan.entries[2].action = TodoAction::Drop;
        let out = start(&repo, &plan, &test_editors()).unwrap();
        assert_eq!(out, Outcome::Done);
        assert_eq!(subjects(&repo), vec!["c2", "fourth", "c1"]);
        assert_eq!(
            repo.commit_message("HEAD~1").unwrap(),
            "fourth\n\nwith body"
        );
        assert!(!repo.path("f3.txt").exists());
        assert!(repo.path("f4.txt").exists());
        assert!(!is_rebasing(&repo));
    }

    #[cfg(unix)]
    #[test]
    fn squash_fixup_and_group_message() {
        let (_d, repo) = temp_repo();
        let h = linear(&repo, 5);
        let range = load_range(&repo, &h[0]).unwrap();
        let mut plan = plan_from(&range);
        // c2 + squash c3 ; c4 + fixup c5
        plan.entries[1].action = TodoAction::Squash;
        plan.entries[3].action = TodoAction::Fixup;
        assert_eq!(start(&repo, &plan, &test_editors()).unwrap(), Outcome::Done);
        assert_eq!(subjects(&repo), vec!["c4", "c2", "c1"]);
        let squashed = repo.commit_message("HEAD~1").unwrap();
        assert!(
            squashed.contains("c2") && squashed.contains("c3"),
            "{squashed}"
        );
        assert!(!squashed.contains('#'), "comments stripped: {squashed}");
        assert_eq!(repo.commit_message("HEAD").unwrap(), "c4");
        for k in 1..=5 {
            assert!(repo.path(&format!("f{k}.txt")).exists());
        }

        // Edited message of a squash group replaces the combined message.
        let range = load_range(&repo, ROOT).unwrap();
        let mut plan = plan_from(&range);
        plan.entries[1].action = TodoAction::Reword;
        plan.entries[1].message = Some("all in one".into());
        plan.entries[2].action = TodoAction::Squash;
        assert_eq!(start(&repo, &plan, &test_editors()).unwrap(), Outcome::Done);
        assert_eq!(subjects(&repo), vec!["all in one", "c1"]);
    }

    #[cfg(unix)]
    #[test]
    fn edit_stops_then_continue() {
        let (_d, repo) = temp_repo();
        let h = linear(&repo, 3);
        let range = load_range(&repo, &h[0]).unwrap();
        let mut plan = plan_from(&range);
        plan.entries[0].action = TodoAction::Edit;
        let out = start(&repo, &plan, &test_editors()).unwrap();
        let Outcome::Paused(p) = out else {
            panic!("expected pause, got {out:?}")
        };
        assert!(p.editing);
        assert_eq!(p.stopped_sha.as_deref(), Some(h[1].as_str()));
        assert_eq!(p.stopped_subject.as_deref(), Some("c2"));
        assert_eq!(p.head_name.as_deref(), Some("main"));
        assert_eq!(p.total, 2);
        assert!(is_rebasing(&repo));
        assert!(Outcome::Paused(p).message().contains("Continue rebase"));
        // Amend the stopped commit, then continue.
        write(&repo, "extra.txt", "x\n");
        repo.run(&["add", "extra.txt"]).unwrap();
        repo.run(&["commit", "-q", "--amend", "--no-edit"]).unwrap();
        assert_eq!(
            continue_with(&repo, &test_editors()).unwrap(),
            Outcome::Done
        );
        assert_eq!(subjects(&repo), vec!["c3", "c2", "c1"]);
        let files = repo.commit_files("HEAD~1").unwrap();
        assert!(files.iter().any(|(_, p, _)| p == "extra.txt"));
        assert!(continue_rebase(&repo).is_err(), "nothing in progress");
    }

    #[cfg(unix)]
    #[test]
    fn conflict_stop_skip_and_abort() {
        let (_d, repo) = temp_repo();
        let base = commit_files(&repo, &[("f.txt", "0\n")], "base");
        commit_files(&repo, &[("f.txt", "1\n")], "one");
        commit_files(&repo, &[("f.txt", "2\n")], "two");
        let head = repo.head_commit().unwrap();
        // Uncommitted change is autostashed and restored.
        write(&repo, "local.txt", "wip\n");
        repo.run(&["add", "local.txt"]).unwrap();

        let range = load_range(&repo, &base).unwrap();
        let mut plan = plan_from(&range);
        plan.entries.reverse();
        let out = start(&repo, &plan, &test_editors()).unwrap();
        let Outcome::Conflicts { paths, progress: p } = &out else {
            panic!("expected conflicts, got {out:?}")
        };
        assert_eq!(paths, &vec!["f.txt".to_string()]);
        assert_eq!(p.step, 1);
        assert!(out.message().contains("Conflicts tab"));
        assert!(
            start(&repo, &plan, &test_editors()).is_err(),
            "already rebasing"
        );

        abort(&repo).unwrap();
        assert!(!is_rebasing(&repo));
        assert_eq!(repo.head_commit().unwrap(), head);
        assert!(repo.path("local.txt").exists(), "autostash restored");
        assert!(abort(&repo).is_err());

        // Same plan again: "two" conflicts on top of "base"; skipping it lets "one" apply cleanly.
        let range = load_range(&repo, &base).unwrap();
        let mut plan = plan_from(&range);
        plan.entries.reverse();
        let out = start(&repo, &plan, &test_editors()).unwrap();
        assert!(matches!(out, Outcome::Conflicts { .. }), "{out:?}");
        assert_eq!(skip_with(&repo, &test_editors()).unwrap(), Outcome::Done);
        assert_eq!(subjects(&repo), vec!["one", "base"]);
        assert!(
            repo.path("local.txt").exists(),
            "autostash restored after skip"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_rebase_and_head_guard() {
        let (_d, repo) = temp_repo();
        linear(&repo, 2);
        let range = load_range(&repo, ROOT).unwrap();
        let mut plan = plan_from(&range);
        plan.entries.swap(0, 1);
        plan.entries[0].action = TodoAction::Reword;
        plan.entries[0].message = Some("second first".into());
        let stale = plan.clone();
        assert_eq!(start(&repo, &plan, &test_editors()).unwrap(), Outcome::Done);
        assert_eq!(subjects(&repo), vec!["c1", "second first"]);
        let err = start(&repo, &stale, &test_editors()).unwrap_err();
        assert!(format!("{err:#}").contains("HEAD moved"));
    }
}
