//! From the model's call to git: check every name against the repository, lay out the steps,
//! say how sure the user must be, then run them one by one.
//!
//! The model never runs anything itself. [`build`] turns its call into a [`Plan`] (or an answer
//! to show); the caller shows the plan, asks as [`Plan::confirm`] says, and only then calls
//! [`Plan::run`]. Any name that doesn't match exactly one branch, commit or file stops here
//! with a question instead.

use anyhow::{anyhow, bail, Result};

use super::resolve::{self, BranchRef, Names};
use super::tools::{
    CherryPickArgs, FindCommitsArgs, MergeArgs, MergeModeArg, NewBranchArgs, OnConflictArg,
    SideArg, ToolCall,
};
use crate::git::branch::{self, CheckoutTarget, Integration, MergeMode};
use crate::git::conflict::{self, Side};
use crate::git::log::{Commit, LogQuery};
use crate::git::remote::{self, PullMode, PushSpec};
use crate::git::{Repo, RepoState};

/// The file release and hotfix branches carry their version in.
pub const VERSION_FILE: &str = "devops/configuration.yaml";
/// More matching commits than this and a cherry-pick asks to narrow the search.
const MAX_PICKS: usize = 20;

/// How sure the user has to be before the plan runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Reads only: run straight away
    No,
    /// Answer y
    Yes,
    /// Type this branch name: the plan pushes to a production or main branch
    Type(String),
}

/// One thing the plan does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Fetch,
    Checkout(CheckoutTarget),
    /// Fast-forward the current branch to its remote copy (it has no commits of its own)
    FastForward {
        branch: String,
        upstream: String,
    },
    CreateBranch {
        name: String,
        from: String,
    },
    SetVersion {
        version: String,
    },
    Merge {
        source: String,
        into: String,
        mode: MergeModeArg,
        on_conflict: Option<OnConflictArg>,
    },
    CherryPick {
        onto: String,
        commits: Vec<Commit>,
    },
    Pull {
        branch: String,
        mode: PullMode,
    },
    Push {
        branch: String,
    },
    AcceptSide {
        side: SideArg,
        paths: Vec<String>,
    },
    /// Finish the merge once nothing is left in conflict
    Continue,
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::Fetch => "fetch all remotes".into(),
            Step::Checkout(t) => match t {
                CheckoutTarget::Tracking { local, remote_ref } => {
                    format!("check out {local} (new local branch from {remote_ref})")
                }
                other => format!("check out {}", other.label()),
            },
            Step::FastForward { branch, upstream } => {
                format!("update {branch} to {upstream} (fast-forward)")
            }
            Step::CreateBranch { name, from } if from.is_empty() => {
                format!("create branch {name} here and check it out")
            }
            Step::CreateBranch { name, from } => {
                format!("create branch {name} from {from} and check it out")
            }
            Step::SetVersion { version } => {
                format!("set version {version} in {VERSION_FILE} and commit")
            }
            Step::Merge {
                source,
                into,
                mode,
                on_conflict,
            } => {
                let how = match mode {
                    MergeModeArg::Default => "",
                    MergeModeArg::NoFf => " (always a merge commit)",
                    MergeModeArg::Squash => " (squash)",
                    MergeModeArg::FfOnly => " (fast-forward only)",
                };
                let conflicts = match on_conflict {
                    None => "; stop on conflicts",
                    Some(OnConflictArg::Abort) => "; cancel it on conflicts",
                    Some(OnConflictArg::Ours) => "; on conflicts keep ours",
                    Some(OnConflictArg::Theirs) => "; on conflicts take theirs",
                };
                format!("merge {source} into {into}{how}{conflicts}")
            }
            Step::CherryPick { onto, commits } => {
                let mut s = format!(
                    "cherry-pick {} onto {onto}:",
                    branch::commits(commits.len() as u32)
                );
                for c in commits {
                    s.push_str(&format!("\n      {} {} ({})", c.short, c.subject, c.author));
                }
                s
            }
            Step::Pull { branch, mode } => format!("pull {branch} ({})", mode.label()),
            Step::Push { branch } => format!("push {branch}"),
            Step::AcceptSide { side, paths } => format!(
                "take {} for {}",
                match side {
                    SideArg::Ours => "ours",
                    SideArg::Theirs => "theirs",
                },
                paths.join(", ")
            ),
            Step::Continue => "finish the merge (commit)".into(),
        }
    }

    fn run(&self, repo: &Repo, prune: bool) -> Result<String> {
        match self {
            Step::Fetch => remote::fetch_all(repo, prune),
            Step::Checkout(t) => {
                t.run(repo, false)?;
                Ok(format!("on {}", t.label()))
            }
            Step::FastForward { branch, upstream } => {
                repo.run(&["merge", "--ff-only", upstream]).map_err(|e| {
                    anyhow!("{branch} can't be fast-forwarded to {upstream}: {e:#}")
                })?;
                Ok(format!("{branch} is at {upstream}"))
            }
            Step::CreateBranch { name, from } => {
                branch::create_branch_with(repo, name, from, true, false)?;
                Ok(format!("on {name}"))
            }
            Step::SetVersion { version } => set_version(repo, version),
            Step::Merge {
                source,
                into,
                mode,
                on_conflict,
            } => {
                let x = match on_conflict {
                    Some(OnConflictArg::Ours) => Some("ours"),
                    Some(OnConflictArg::Theirs) => Some("theirs"),
                    _ => None,
                };
                match branch::merge_with(repo, source, merge_mode(*mode), x)? {
                    Integration::Done(msg) => Ok(msg),
                    Integration::Conflicts => {
                        let n = conflict::unmerged_count(repo).unwrap_or(0);
                        if *on_conflict == Some(OnConflictArg::Abort) {
                            conflict::abort_operation(repo)?;
                            bail!("merging {source} into {into} conflicts in {n} file(s); merge cancelled, {into} is unchanged")
                        }
                        bail!("merging {source} into {into} stopped with conflicts in {n} file(s). Resolve them in gitjet (Conflicts tab, 6), or ask \"take theirs\" / \"keep ours\"")
                    }
                }
            }
            Step::CherryPick { commits, .. } => {
                let revs: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
                crate::git::history::cherry_pick(repo, &revs)
            }
            Step::Pull { mode, .. } => remote::pull(repo, *mode),
            Step::Push { branch } => remote::push(repo, &PushSpec::for_branch(repo, branch)?),
            Step::AcceptSide { side, paths } => {
                let side = match side {
                    SideArg::Ours => Side::Ours,
                    SideArg::Theirs => Side::Theirs,
                };
                let n = conflict::accept_side(repo, paths, side)?;
                Ok(format!("resolved {n} file(s)"))
            }
            Step::Continue => conflict::continue_operation(repo),
        }
    }

    fn pushes(&self) -> Option<&str> {
        match self {
            Step::Push { branch } => Some(branch),
            _ => None,
        }
    }
}

/// What the model's call comes to.
#[derive(Debug)]
pub enum Answer {
    /// Something to tell the user; nothing to run (a question, a refusal, a commit list)
    Say(String),
    Plan(Plan),
}

#[derive(Debug)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Names that were corrected: "'uat' is deploy/uat"
    pub notes: Vec<String>,
    pub confirm: Confirm,
}

impl Plan {
    fn new(steps: Vec<Step>, notes: Vec<String>) -> Plan {
        let confirm = if steps.iter().all(|s| *s == Step::Fetch) {
            Confirm::No
        } else if let Some(b) = steps.iter().filter_map(Step::pushes).find(|b| protected(b)) {
            Confirm::Type(b.to_string())
        } else {
            Confirm::Yes
        };
        Plan {
            steps,
            notes,
            confirm,
        }
    }

    /// Run the steps in order, reporting each; stops at the first that fails.
    pub fn run(
        &self,
        repo: &Repo,
        settings: &Settings,
        mut report: impl FnMut(usize, &Step, &Result<String>),
    ) -> Result<()> {
        for (i, step) in self.steps.iter().enumerate() {
            let r = step.run(repo, settings.prune);
            report(i, step, &r);
            if let Err(e) = r {
                let left = self.steps.len() - i - 1;
                if left == 0 {
                    return Err(e);
                }
                return Err(e.context(format!(
                    "step {} of {} failed; the {left} after it did not run",
                    i + 1,
                    self.steps.len(),
                )));
            }
        }
        Ok(())
    }
}

/// Branches a push to which needs the name typed: production and main lines.
pub fn protected(branch: &str) -> bool {
    let last = branch.rsplit('/').next().unwrap_or(branch);
    ["prod", "production", "main", "master"].contains(&last)
}

/// The parts of gitjet's config a plan uses.
#[derive(Debug, Clone)]
pub struct Settings {
    pub pull: PullMode,
    pub prune: bool,
}

/// Whether to fetch before [`build`], so branches only on the remote are found and local
/// copies that are behind are noticed.
pub fn wants_fetch(call: &ToolCall) -> bool {
    matches!(
        call,
        ToolCall::NewBranch(_)
            | ToolCall::Merge(_)
            | ToolCall::CherryPick(_)
            | ToolCall::Switch(_)
            | ToolCall::FindCommits(_)
    )
}

/// Turn the model's call into a plan, checking every name against the repository.
pub fn build(repo: &Repo, call: &ToolCall, settings: &Settings) -> Result<Answer> {
    let state = repo.state();
    if state != RepoState::Clean && !matches!(call, ToolCall::ResolveConflicts(_)) {
        bail!(
            "a {} is in progress; finish or abort it first (Conflicts tab, 6)",
            state.label().to_lowercase()
        );
    }
    let mut notes = Vec::new();
    let mut b = Builder {
        repo,
        names: Names::load(repo)?,
        notes: &mut notes,
    };
    let steps = match call {
        ToolCall::Clarify(a) => return Ok(Answer::Say(a.question.clone())),
        ToolCall::Unsupported(a) => {
            return Ok(Answer::Say(format!("gitjet can't do that: {}", a.reason)))
        }
        ToolCall::FindCommits(a) => return find_commits(repo, &mut b, a).map(Answer::Say),
        ToolCall::Fetch(_) => vec![Step::Fetch],
        ToolCall::Switch(a) => {
            let target = b.branch(&a.branch)?;
            let mut steps = vec![Step::Checkout(target.checkout())];
            steps.extend(fast_forward(&target));
            steps
        }
        ToolCall::NewBranch(a) => b.new_branch(a)?,
        ToolCall::Merge(a) => b.merge(a)?,
        ToolCall::CherryPick(a) => b.cherry_pick(a)?,
        ToolCall::Pull(a) => {
            let mut steps = Vec::new();
            let name = match &a.branch {
                Some(s) => {
                    let target = b.branch(s)?;
                    if b.repo.head_branch().as_deref() != Some(target.name.as_str()) {
                        steps.push(Step::Checkout(target.checkout()));
                    }
                    target.name
                }
                None => current(repo)?,
            };
            steps.push(Step::Pull {
                branch: name,
                mode: settings.pull,
            });
            steps
        }
        ToolCall::Push(a) => {
            let name = match &a.branch {
                Some(s) => {
                    let target = b.branch(s)?;
                    if !target.local {
                        bail!("there is no local branch '{}' to push", target.name);
                    }
                    target.name
                }
                None => current(repo)?,
            };
            vec![Step::Push { branch: name }]
        }
        ToolCall::ResolveConflicts(a) => {
            let conflicted: Vec<String> =
                conflict::list(repo)?.into_iter().map(|f| f.path).collect();
            if conflicted.is_empty() {
                bail!("nothing is in conflict");
            }
            let paths = if a.paths.is_empty() {
                conflicted.clone()
            } else {
                resolve::conflicted_paths(&conflicted, &a.paths)?
            };
            let all = paths.len() == conflicted.len();
            let mut steps = vec![Step::AcceptSide {
                side: a.side,
                paths,
            }];
            if all && state == RepoState::Merging {
                steps.push(Step::Continue);
            }
            steps
        }
    };
    Ok(Answer::Plan(Plan::new(steps, notes)))
}

fn current(repo: &Repo) -> Result<String> {
    repo.head_branch()
        .ok_or_else(|| anyhow!("HEAD is detached; say which branch"))
}

fn merge_mode(m: MergeModeArg) -> MergeMode {
    match m {
        MergeModeArg::Default => MergeMode::Merge,
        MergeModeArg::NoFf => MergeMode::NoFf,
        MergeModeArg::Squash => MergeMode::Squash,
        MergeModeArg::FfOnly => MergeMode::FfOnly,
    }
}

/// Bring a local branch that is only behind its remote up to date, so the work lands on top of
/// what is already published and the push isn't rejected.
fn fast_forward(target: &BranchRef) -> Option<Step> {
    match (&target.remote_ref, target.local && target.stale) {
        (Some(upstream), true) => Some(Step::FastForward {
            branch: target.name.clone(),
            upstream: upstream.clone(),
        }),
        _ => None,
    }
}

struct Builder<'a> {
    repo: &'a Repo,
    names: Names,
    notes: &'a mut Vec<String>,
}

impl Builder<'_> {
    fn branch(&mut self, said: &str) -> Result<BranchRef> {
        let b = self.names.branch(said)?;
        if let Some(from) = &b.corrected_from {
            self.notes.push(format!("'{from}' is {}", b.name));
        }
        Ok(b)
    }

    /// The branch to work on, and the steps to be on it up to date with its remote copy: the
    /// one named, or the current one.
    fn target(&mut self, said: Option<&str>) -> Result<(String, Vec<Step>)> {
        let t = match said {
            Some(s) => self.branch(s)?,
            None => self.names.branch(&current(self.repo)?)?,
        };
        let mut steps = Vec::new();
        if self.repo.head_branch().as_deref() != Some(t.name.as_str()) {
            steps.push(Step::Checkout(t.checkout()));
        }
        steps.extend(fast_forward(&t));
        Ok((t.name, steps))
    }

    fn new_branch(&mut self, a: &NewBranchArgs) -> Result<Vec<Step>> {
        let name = a.name.trim().to_string();
        branch::validate_name(self.repo, &name)?;
        if branch::local_exists(self.repo, &name) {
            bail!("a branch named '{name}' already exists");
        }
        let from = match &a.from {
            Some(s) => self.branch(s)?.rev(),
            None => String::new(),
        };
        let mut steps = vec![Step::CreateBranch {
            name: name.clone(),
            from: from.clone(),
        }];
        if let Some(version) = release_version(&name) {
            let start = if from.is_empty() { "HEAD" } else { &from };
            let has_file = self
                .repo
                .try_run(&["cat-file", "-e", &format!("{start}:{VERSION_FILE}")])
                .is_some();
            if has_file {
                steps.push(Step::SetVersion { version });
            }
        }
        Ok(steps)
    }

    fn merge(&mut self, a: &MergeArgs) -> Result<Vec<Step>> {
        let (into, mut steps) = self.target(a.into.as_deref())?;
        let source = match self.names.branch(&a.source) {
            Ok(_) => {
                let s = self.branch(&a.source)?;
                if s.name == into && !s.said_remote {
                    bail!("can't merge {into} into itself");
                }
                s.rev()
            }
            Err(branch_err) => match resolve::commit_by_hash_or_message(self.repo, &a.source) {
                Ok(c) => {
                    self.notes.push(format!(
                        "'{}' is commit {} {}",
                        a.source, c.short, c.subject
                    ));
                    c.short
                }
                Err(_) => return Err(branch_err),
            },
        };
        steps.push(Step::Merge {
            source,
            into: into.clone(),
            mode: a.mode,
            on_conflict: a.on_conflict,
        });
        if a.push {
            steps.push(Step::Push { branch: into });
        }
        Ok(steps)
    }

    fn cherry_pick(&mut self, a: &CherryPickArgs) -> Result<Vec<Step>> {
        let (onto, mut steps) = self.target(a.onto.as_deref())?;
        let commits = if !a.commits.is_empty() {
            a.commits
                .iter()
                .map(|h| resolve::commit(self.repo, h))
                .collect::<Result<Vec<_>>>()?
        } else {
            if a.author.is_none() && a.message.is_none() && a.from.is_none() {
                bail!("which commits? Give the hashes, an author or words from the message");
            }
            let mut q = LogQuery {
                author: a
                    .author
                    .as_deref()
                    .map(|s| resolve::author(self.repo, s))
                    .transpose()?
                    .map(|s| regex::escape(&s)),
                grep: a.message.as_deref().map(|m| regex::escape(m.trim())),
                no_merges: true,
                max: MAX_PICKS + 1,
                ..Default::default()
            };
            match &a.from {
                Some(f) => q.revs.push(self.branch(f)?.rev()),
                None => q.all = true,
            }
            q.revs.push(format!("^{onto}"));
            let mut found = self.repo.log(&q)?;
            if found.is_empty() {
                bail!("no commits match (only commits not yet on {onto} count)");
            }
            if found.len() > MAX_PICKS {
                bail!("more than {MAX_PICKS} commits match; narrow it down (author, message words or hashes)");
            }
            found.reverse();
            found
        };
        steps.push(Step::CherryPick {
            onto: onto.clone(),
            commits,
        });
        if a.push {
            steps.push(Step::Push { branch: onto });
        }
        Ok(steps)
    }
}

fn find_commits(repo: &Repo, b: &mut Builder, a: &FindCommitsArgs) -> Result<String> {
    let mut q = LogQuery {
        author: a
            .author
            .as_deref()
            .map(|s| resolve::author(repo, s))
            .transpose()?
            .map(|s| regex::escape(&s)),
        grep: a.message.as_deref().map(|m| regex::escape(m.trim())),
        since: a.since.clone(),
        until: a.until.clone(),
        paths: a.path.iter().cloned().collect(),
        max: 50,
        topo: false,
        ..Default::default()
    };
    if let Some(s) = &a.branch {
        q.revs.push(b.branch(s)?.rev());
    }
    if let Some(n) = &a.not_in {
        if q.revs.is_empty() {
            q.revs.push("HEAD".into());
        }
        q.revs.push(format!("^{}", b.branch(n)?.rev()));
    }
    let found = repo.log(&q)?;
    if found.is_empty() {
        return Ok("no commits match".into());
    }
    let mut out = String::new();
    for c in &found {
        let date = chrono::DateTime::from_timestamp(c.time, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "{} {date} {:<16} {}\n",
            c.short, c.author, c.subject
        ));
    }
    if found.len() == q.max {
        out.push_str(&format!("(first {})\n", q.max));
    }
    Ok(out.trim_end().to_string())
}

/// `release/1.4.9` → `1.4.9`; `hotfix/1.4.9.1` → `1.4.9.1`.
pub fn release_version(branch: &str) -> Option<String> {
    let re = regex::Regex::new(r"^(?:release|hotfix)/v?(\d+\.\d+\.\d+(?:\.\d+)?)$").ok()?;
    re.captures(branch).map(|c| c[1].to_string())
}

/// Write `version` into the version file (`version:` and `versionSnapshot:`) and commit.
fn set_version(repo: &Repo, version: &str) -> Result<String> {
    let path = repo.path(VERSION_FILE);
    let text =
        std::fs::read_to_string(&path).map_err(|e| anyhow!("reading {VERSION_FILE}: {e}"))?;
    let new = with_version(&text, version);
    if new == text {
        return Ok(format!("{VERSION_FILE} already has {version}"));
    }
    std::fs::write(&path, &new).map_err(|e| anyhow!("writing {VERSION_FILE}: {e}"))?;
    let msg = format!("Set version to {version}");
    repo.run(&["commit", "-q", "-m", &msg, "--", VERSION_FILE])?;
    Ok(msg)
}

/// `text` with `version: x` set to `version` and `versionSnapshot: x-SNAPSHOT` to
/// `version-SNAPSHOT`; indentation, quotes and comments are kept.
pub fn with_version(text: &str, version: &str) -> String {
    let re =
        regex::Regex::new(r#"(?m)^(\s*(version|versionSnapshot):[ \t]*)(["']?)[^"'\s#]+(["']?)"#)
            .expect("valid regex");
    re.replace_all(text, |c: &regex::Captures| {
        let v = if &c[2] == "versionSnapshot" {
            format!("{version}-SNAPSHOT")
        } else {
            version.to_string()
        };
        format!("{}{}{v}{}", &c[1], &c[3], &c[4])
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::tools::*;
    use crate::git::testutil::{commit_files, temp_repo, TempDir};

    fn settings() -> Settings {
        Settings {
            pull: PullMode::Merge,
            prune: false,
        }
    }

    /// `origin` has develop (two commits ahead of main), deploy/uat and deploy/prod; the clone
    /// has develop and deploy/uat locally and is on develop.
    fn setup() -> (TempDir, TempDir, Repo, Repo) {
        let (od, origin) = temp_repo();
        commit_files(
            &origin,
            &[(
                VERSION_FILE,
                "app:\n  version: 1.4.7\n  versionSnapshot: 1.4.8-SNAPSHOT\n",
            )],
            "init",
        );
        origin.run(&["branch", "deploy/uat"]).unwrap();
        origin.run(&["branch", "deploy/prod"]).unwrap();
        origin.run(&["checkout", "-q", "-b", "develop"]).unwrap();
        commit_files(&origin, &[("feature.txt", "face id\n")], "add face id");
        commit_files(&origin, &[("login.txt", "fixed\n")], "fix login on safari");
        origin.run(&["checkout", "-q", "main"]).unwrap();
        let (cd, repo) = temp_repo();
        repo.run(&["remote", "add", "origin", od.path().to_str().unwrap()])
            .unwrap();
        repo.run(&["fetch", "-q", "origin"]).unwrap();
        repo.run(&[
            "checkout",
            "-q",
            "-b",
            "develop",
            "--track",
            "origin/develop",
        ])
        .unwrap();
        repo.run(&["branch", "--track", "deploy/uat", "origin/deploy/uat"])
            .unwrap();
        (od, cd, origin, repo)
    }

    fn plan(repo: &Repo, call: ToolCall) -> Plan {
        match build(repo, &call, &settings()).unwrap() {
            Answer::Plan(p) => p,
            Answer::Say(s) => panic!("expected a plan, got {s:?}"),
        }
    }

    fn run(repo: &Repo, p: &Plan) -> Result<()> {
        p.run(repo, &settings(), |_, _, _| {})
    }

    fn merge(source: &str, into: Option<&str>, on: Option<OnConflictArg>, push: bool) -> ToolCall {
        ToolCall::Merge(MergeArgs {
            source: source.into(),
            into: into.map(Into::into),
            mode: MergeModeArg::Default,
            on_conflict: on,
            push,
        })
    }

    #[test]
    fn merge_into_uat_and_push() {
        let (_od, _cd, origin, repo) = setup();
        let p = plan(&repo, merge("develop", Some("uat"), None, true));
        assert_eq!(
            p.steps,
            vec![
                Step::Checkout(CheckoutTarget::Existing("deploy/uat".into())),
                Step::Merge {
                    source: "develop".into(),
                    into: "deploy/uat".into(),
                    mode: MergeModeArg::Default,
                    on_conflict: None
                },
                Step::Push {
                    branch: "deploy/uat".into()
                },
            ]
        );
        assert_eq!(p.notes, vec!["'uat' is deploy/uat".to_string()]);
        assert_eq!(p.confirm, Confirm::Yes);
        run(&repo, &p).unwrap();
        assert_eq!(repo.head_branch().as_deref(), Some("deploy/uat"));
        assert_eq!(
            origin.rev_parse("refs/heads/deploy/uat").unwrap(),
            repo.rev_parse("develop").unwrap()
        );
    }

    #[test]
    fn pushing_to_prod_needs_the_name_typed() {
        let (_od, _cd, _o, repo) = setup();
        let p = plan(&repo, merge("develop", Some("deply/prod"), None, true));
        assert_eq!(p.confirm, Confirm::Type("deploy/prod".into()));
        assert!(matches!(
            &p.steps[0],
            Step::Checkout(CheckoutTarget::Tracking { local, .. }) if local == "deploy/prod"
        ));
    }

    #[test]
    fn unknown_names_stop_before_anything_runs() {
        let (_od, _cd, _o, repo) = setup();
        let e = build(
            &repo,
            &merge("hotfix branch", Some("uat"), None, false),
            &settings(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("no branch"), "{e}");
        assert_eq!(repo.head_branch().as_deref(), Some("develop"));
    }

    #[test]
    fn merge_a_commit_named_by_its_message() {
        let (_od, _cd, _o, repo) = setup();
        let p = plan(&repo, merge("face id", Some("uat"), None, false));
        let Step::Merge { source, .. } = &p.steps[1] else {
            panic!("{:?}", p.steps)
        };
        assert_eq!(
            repo.rev_parse(source).unwrap(),
            repo.rev_parse("develop~1").unwrap()
        );
    }

    #[test]
    fn cherry_pick_by_message_onto_uat() {
        let (_od, _cd, _o, repo) = setup();
        let p = plan(
            &repo,
            ToolCall::CherryPick(CherryPickArgs {
                author: Some("me".into()),
                message: Some("fix login".into()),
                onto: Some("deploy/uat".into()),
                ..Default::default()
            }),
        );
        let Step::CherryPick { commits, .. } = &p.steps[1] else {
            panic!("{:?}", p.steps)
        };
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "fix login on safari");
        run(&repo, &p).unwrap();
        assert!(repo.path("login.txt").exists());
        assert!(!repo.path("feature.txt").exists());
    }

    #[test]
    fn cherry_pick_by_hash_and_unknown_hash() {
        let (_od, _cd, _o, repo) = setup();
        let h = repo.rev_parse("develop~1").unwrap();
        let p = plan(
            &repo,
            ToolCall::CherryPick(CherryPickArgs {
                commits: vec![h[..7].to_string()],
                onto: Some("uat".into()),
                ..Default::default()
            }),
        );
        run(&repo, &p).unwrap();
        assert!(repo.path("feature.txt").exists());
        let e = build(
            &repo,
            &ToolCall::CherryPick(CherryPickArgs {
                commits: vec!["abcdef1".into()],
                ..Default::default()
            }),
            &settings(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("no commit"), "{e}");
    }

    #[test]
    fn release_branch_sets_the_version() {
        let (_od, _cd, _o, repo) = setup();
        let p = plan(
            &repo,
            ToolCall::NewBranch(NewBranchArgs {
                name: "release/1.4.9".into(),
                from: Some("origin/deploy/prod".into()),
            }),
        );
        assert_eq!(
            p.steps,
            vec![
                Step::CreateBranch {
                    name: "release/1.4.9".into(),
                    from: "origin/deploy/prod".into()
                },
                Step::SetVersion {
                    version: "1.4.9".into()
                },
            ]
        );
        run(&repo, &p).unwrap();
        let text = std::fs::read_to_string(repo.path(VERSION_FILE)).unwrap();
        assert!(
            text.contains("version: 1.4.9\n") && text.contains("versionSnapshot: 1.4.9-SNAPSHOT"),
            "{text}"
        );
        assert_eq!(
            repo.run(&["log", "-1", "--format=%s"]).unwrap().trim(),
            "Set version to 1.4.9"
        );
    }

    #[test]
    fn conflicts_cancel_or_take_a_side() {
        let (_od, _cd, _o, repo) = setup();
        repo.run(&["checkout", "-q", "deploy/uat"]).unwrap();
        commit_files(&repo, &[("login.txt", "uat version\n")], "uat change");
        // Cancel: the branch is left as it was.
        let before = repo.head_commit();
        let p = plan(
            &repo,
            merge("develop", None, Some(OnConflictArg::Abort), false),
        );
        let e = run(&repo, &p).unwrap_err();
        assert!(format!("{e:#}").contains("cancelled"), "{e:#}");
        assert_eq!(repo.head_commit(), before);
        assert_eq!(repo.state(), RepoState::Clean);
        // Theirs: done in one go.
        let p = plan(
            &repo,
            merge("develop", None, Some(OnConflictArg::Theirs), false),
        );
        run(&repo, &p).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.path("login.txt")).unwrap(),
            "fixed\n"
        );
    }

    #[test]
    fn resolve_conflicts_then_finish_the_merge() {
        let (_od, _cd, _o, repo) = setup();
        repo.run(&["checkout", "-q", "deploy/uat"]).unwrap();
        commit_files(&repo, &[("login.txt", "uat version\n")], "uat change");
        let p = plan(&repo, merge("develop", None, None, true));
        let e = run(&repo, &p).unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("Conflicts tab") && msg.contains("did not run"),
            "{msg}"
        );
        assert_eq!(repo.state(), RepoState::Merging);
        // Other requests wait for the merge.
        assert!(build(&repo, &ToolCall::Fetch(FetchArgs {}), &settings()).is_err());
        let p = plan(
            &repo,
            ToolCall::ResolveConflicts(ResolveConflictsArgs {
                side: SideArg::Ours,
                paths: vec!["login.txt".into()],
            }),
        );
        assert_eq!(p.steps.last(), Some(&Step::Continue));
        run(&repo, &p).unwrap();
        assert_eq!(repo.state(), RepoState::Clean);
        assert_eq!(
            std::fs::read_to_string(repo.path("login.txt")).unwrap(),
            "uat version\n"
        );
    }

    #[test]
    fn answers_without_running() {
        let (_od, _cd, _o, repo) = setup();
        let say = |call| match build(&repo, &call, &settings()).unwrap() {
            Answer::Say(s) => s,
            Answer::Plan(p) => panic!("{p:?}"),
        };
        assert_eq!(
            say(ToolCall::Clarify(ClarifyArgs {
                question: "Merge which branch?".into()
            })),
            "Merge which branch?"
        );
        let found = say(ToolCall::FindCommits(FindCommitsArgs {
            message: Some("face".into()),
            branch: Some("develop".into()),
            not_in: Some("uat".into()),
            ..Default::default()
        }));
        assert!(
            found.contains("add face id") && !found.contains("fix login"),
            "{found}"
        );
        assert_eq!(
            plan(&repo, ToolCall::Fetch(FetchArgs {})).confirm,
            Confirm::No
        );
    }

    #[test]
    fn versions() {
        assert_eq!(release_version("release/1.1.1").as_deref(), Some("1.1.1"));
        assert_eq!(
            release_version("hotfix/1.4.9.2").as_deref(),
            Some("1.4.9.2")
        );
        assert_eq!(release_version("feature/1.1.1"), None);
        assert_eq!(
            with_version(
                "a:\n  version: \"1.0.0\" # x\n  versionSnapshot: 1.0.1-SNAPSHOT\n",
                "2.0.0"
            ),
            "a:\n  version: \"2.0.0\" # x\n  versionSnapshot: 2.0.0-SNAPSHOT\n"
        );
        assert!(protected("deploy/prod") && protected("main") && !protected("deploy/uat"));
    }
}
