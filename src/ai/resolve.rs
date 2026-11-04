//! Turn the names the model copied from the request into real branches, commits and files.
//!
//! The model only repeats what the user said ("uat", "deply/prod", "the login fix"). Nothing
//! runs on those words: each one is matched against the repository here, and anything that
//! doesn't match exactly one thing stops the plan with a question.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};

use crate::git::branch::{self, Branch, CheckoutTarget};
use crate::git::log::{Commit, LogQuery};
use crate::git::Repo;

/// A branch the user named, as found in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRef {
    /// Local name (`deploy/uat`), also for a branch that so far exists only on the remote
    pub name: String,
    /// A local branch of that name exists
    pub local: bool,
    /// Its remote-tracking branch (`origin/deploy/uat`), if any
    pub remote_ref: Option<String>,
    /// The user named the remote side ("origin/x", "x on the remote")
    pub said_remote: bool,
    /// The local branch is behind its remote and has nothing of its own
    pub stale: bool,
    /// What the user said, when it differs from `name` (shown in the plan)
    pub corrected_from: Option<String>,
}

impl BranchRef {
    /// The revision to merge, cherry-pick from or start a branch from: the remote side when the
    /// user named it, or when the local branch is only an older copy of it.
    pub fn rev(&self) -> String {
        match &self.remote_ref {
            Some(r) if self.said_remote || !self.local || self.stale => r.clone(),
            _ => self.name.clone(),
        }
    }

    /// How to check it out: the local branch, or a new one tracking the remote branch.
    pub fn checkout(&self) -> CheckoutTarget {
        match (&self.remote_ref, self.local) {
            (Some(r), false) => CheckoutTarget::Tracking {
                local: self.name.clone(),
                remote_ref: r.clone(),
            },
            _ => CheckoutTarget::Existing(self.name.clone()),
        }
    }
}

/// The repository's branches, for matching names.
pub struct Names {
    local: Vec<Branch>,
    remote: Vec<Branch>,
    remotes: Vec<String>,
}

impl Names {
    pub fn load(repo: &Repo) -> Result<Self> {
        let (remote, local) = branch::list_branches(repo)?
            .into_iter()
            .partition(Branch::is_remote);
        Ok(Self {
            local,
            remote,
            remotes: crate::git::remote::remotes(repo).unwrap_or_default(),
        })
    }

    fn local(&self, name: &str) -> Option<&Branch> {
        self.local.iter().find(|b| b.name == name)
    }

    fn remote_ref(&self, remote: &str, name: &str) -> Option<String> {
        self.remote
            .iter()
            .find(|b| b.remote.as_deref() == Some(remote) && b.short_name() == name)
            .map(|b| b.name.clone())
    }

    /// The default remote's copy of `name`, or any remote's.
    fn any_remote_ref(&self, name: &str) -> Option<String> {
        let mut found: Vec<&Branch> = self
            .remote
            .iter()
            .filter(|b| b.short_name() == name)
            .collect();
        found.sort_by_key(|b| b.remote.as_deref() != Some("origin"));
        found.first().map(|b| b.name.clone())
    }

    /// Every branch name, local or remote (without the remote prefix).
    fn all(&self) -> BTreeSet<&str> {
        self.local
            .iter()
            .map(|b| b.name.as_str())
            .chain(self.remote.iter().map(Branch::short_name))
            .filter(|n| *n != "HEAD")
            .collect()
    }

    /// The branch the user meant by `said`: exact name, then a unique branch ending in
    /// `/said` ("uat" → "deploy/uat"), then a unique close spelling ("deply/prod").
    pub fn branch(&self, said: &str) -> Result<BranchRef> {
        let said = said.trim().trim_matches(['"', '\'', '`']);
        if said.is_empty() {
            bail!("the branch name is empty");
        }
        // "origin/x": the remote side of x.
        let (remote, bare) = match self
            .remotes
            .iter()
            .filter(|r| said.starts_with(&format!("{r}/")) && self.local(said).is_none())
            .max_by_key(|r| r.len())
        {
            Some(r) => (Some(r.as_str()), &said[r.len() + 1..]),
            None => (None, said),
        };
        let all = self.all();
        let lower = bare.to_lowercase();
        let name = if let Some(n) = all.iter().find(|n| n.to_lowercase() == lower) {
            n.to_string()
        } else {
            let suffix: Vec<&str> = all
                .iter()
                .copied()
                .filter(|n| n.to_lowercase().ends_with(&format!("/{lower}")))
                .collect();
            match suffix.as_slice() {
                [one] => one.to_string(),
                [] => self.close_spelling(bare, &all)?,
                many => bail!("'{said}' could be {}. Which one?", list(many)),
            }
        };
        let local = self.local(&name);
        let remote_ref = match remote {
            Some(r) => Some(
                self.remote_ref(r, &name)
                    .ok_or_else(|| anyhow!("{r} has no branch '{name}' (fetched copies only)"))?,
            ),
            None => local
                .and_then(|b| b.upstream.clone())
                .filter(|u| self.remote.iter().any(|b| &b.name == u))
                .or_else(|| self.any_remote_ref(&name)),
        };
        let stale = local.is_some_and(|b| b.upstream.is_some() && b.ahead == 0 && b.behind > 0);
        let shown = remote.map_or(name.clone(), |r| format!("{r}/{name}"));
        Ok(BranchRef {
            corrected_from: (shown != said).then(|| said.to_string()),
            local: local.is_some(),
            remote_ref,
            said_remote: remote.is_some(),
            stale,
            name,
        })
    }

    fn close_spelling(&self, said: &str, all: &BTreeSet<&str>) -> Result<String> {
        let lower = said.to_lowercase();
        let max = if lower.chars().count() <= 4 { 1 } else { 2 };
        let score = |n: &str| {
            let n = n.to_lowercase();
            let whole = distance(&lower, &n);
            if lower.contains('/') {
                whole
            } else {
                whole.min(distance(&lower, n.rsplit('/').next().unwrap_or(&n)))
            }
        };
        let mut scored: Vec<(usize, &str)> = all.iter().map(|n| (score(n), *n)).collect();
        scored.sort();
        let best = scored.first().map_or(usize::MAX, |s| s.0);
        let at_best: Vec<&str> = scored.iter().filter(|s| s.0 == best).map(|s| s.1).collect();
        match at_best.as_slice() {
            [one] if best <= max => Ok(one.to_string()),
            many if best <= max => bail!("'{said}' could be {}. Which one?", list(many)),
            _ => {
                let near: Vec<&str> = scored
                    .iter()
                    .filter(|s| s.0 <= max + 2)
                    .take(3)
                    .map(|s| s.1)
                    .collect();
                if near.is_empty() {
                    bail!("there is no branch '{said}'")
                }
                bail!("there is no branch '{said}'. Did you mean {}?", list(&near))
            }
        }
    }
}

/// `a, b or c`, each quoted.
fn list(names: &[&str]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    match q.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => q.join(""),
    }
}

/// Edit distance (insert, delete, replace one character).
pub fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if ca == *cb {
                prev
            } else {
                1 + prev.min(row[j]).min(cur)
            };
            prev = cur;
        }
    }
    row[b.len()]
}

/// The commit for a hash the user typed (abbreviated is fine).
pub fn commit(repo: &Repo, said: &str) -> Result<Commit> {
    let said = said.trim();
    if said.len() < 4 || !said.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("'{said}' is not a commit hash");
    }
    let full = repo
        .run(&["rev-parse", "--verify", "-q", &format!("{said}^{{commit}}")])
        .map_err(|_| anyhow!("there is no commit {said} (fetch first if it's someone else's)"))?;
    repo.commit_info(full.trim())
}

/// The author filter for "me": the configured e-mail, else the name.
pub fn author(repo: &Repo, said: &str) -> Result<String> {
    let said = said.trim();
    if ["me", "myself", "mine", "i"].contains(&said.to_lowercase().as_str()) {
        return repo
            .config_get("user.email")
            .or_else(|| repo.config_get("user.name"))
            .ok_or_else(|| anyhow!("git doesn't know who you are (user.email is not set)"));
    }
    Ok(said.to_string())
}

/// The commit a merge source names when it isn't a branch: a hash, or words from one commit's
/// message.
pub fn commit_by_hash_or_message(repo: &Repo, said: &str) -> Result<Commit> {
    if let Ok(c) = commit(repo, said) {
        return Ok(c);
    }
    let found = repo.log(&LogQuery {
        all: true,
        grep: Some(regex::escape(said.trim())),
        max: 2,
        ..Default::default()
    })?;
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no branch or commit matches '{said}'"),
        _ => bail!("more than one commit mentions '{said}'; give the hash"),
    }
}

/// The conflicted files the user named: exact path, then file name, then a close spelling.
pub fn conflicted_paths(conflicted: &[String], said: &[String]) -> Result<Vec<String>> {
    let name_of = |p: &str| p.rsplit('/').next().unwrap_or(p).to_lowercase();
    let mut out = Vec::new();
    for s in said {
        let s = s.trim().trim_start_matches("./");
        let lower = s.to_lowercase();
        let mut pick: Vec<&String> = conflicted.iter().filter(|p| p.as_str() == s).collect();
        if pick.is_empty() {
            pick = conflicted
                .iter()
                .filter(|p| p.to_lowercase().ends_with(&format!("/{lower}")) || name_of(p) == lower)
                .collect();
        }
        if pick.is_empty() {
            pick = conflicted
                .iter()
                .filter(|p| distance(&name_of(s), &name_of(p)) <= 2)
                .collect();
        }
        match pick.as_slice() {
            [one] => out.push((*one).clone()),
            [] => {
                let all: Vec<&str> = conflicted.iter().map(String::as_str).collect();
                bail!(
                    "no conflicted file matches '{s}'. The conflicted files are {}",
                    list(&all)
                )
            }
            many => {
                let m: Vec<&str> = many.iter().map(|p| p.as_str()).collect();
                bail!("'{s}' could be {}. Which one?", list(&m))
            }
        }
    }
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::{commit_files, temp_repo, TempDir};

    /// A repo with an `origin` holding develop, deploy/uat, deploy/prod and feature/uat-merge;
    /// develop and deploy/uat are also local.
    fn repo_with_remote() -> (TempDir, TempDir, Repo) {
        let (origin_dir, origin) = temp_repo();
        commit_files(&origin, &[("a.txt", "1\n")], "init");
        for b in ["develop", "deploy/uat", "deploy/prod", "feature/uat-merge"] {
            origin.run(&["branch", b]).unwrap();
        }
        let (dir, repo) = temp_repo();
        repo.run(&[
            "remote",
            "add",
            "origin",
            origin_dir.path().to_str().unwrap(),
        ])
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
        (origin_dir, dir, repo)
    }

    #[test]
    fn finds_branches_by_name_end_and_spelling() {
        let (_o, _d, repo) = repo_with_remote();
        let names = Names::load(&repo).unwrap();
        let uat = names.branch("uat").unwrap();
        assert_eq!(uat.name, "deploy/uat");
        assert!(uat.local);
        assert_eq!(uat.corrected_from.as_deref(), Some("uat"));
        assert_eq!(uat.rev(), "deploy/uat");

        let prod = names.branch("deply/prod").unwrap();
        assert_eq!(prod.name, "deploy/prod");
        assert!(!prod.local);
        assert_eq!(prod.rev(), "origin/deploy/prod");
        assert!(matches!(prod.checkout(), CheckoutTarget::Tracking { .. }));

        assert_eq!(names.branch("Develop").unwrap().name, "develop");

        let remote = names.branch("origin/deploy/uat").unwrap();
        assert!(remote.said_remote);
        assert_eq!(remote.rev(), "origin/deploy/uat");
        assert_eq!(remote.corrected_from, None);
    }

    #[test]
    fn refuses_unknown_and_ambiguous_branches() {
        let (_o, _d, repo) = repo_with_remote();
        repo.run(&["branch", "feature/prod"]).unwrap();
        let names = Names::load(&repo).unwrap();
        let e = names.branch("prod").unwrap_err().to_string();
        assert!(e.contains("could be"), "{e}");
        let e = names.branch("hotfix branch").unwrap_err().to_string();
        assert!(e.contains("no branch"), "{e}");
        assert!(names.branch("staging").is_err());
    }

    #[test]
    fn a_local_branch_behind_its_remote_merges_the_remote_side() {
        let (_o, _d, repo) = repo_with_remote();
        commit_files(&repo, &[("b.txt", "2\n")], "ahead");
        repo.run(&["update-ref", "refs/remotes/origin/deploy/uat", "HEAD"])
            .unwrap();
        let names = Names::load(&repo).unwrap();
        let uat = names.branch("deploy/uat").unwrap();
        assert!(uat.stale);
        assert_eq!(uat.rev(), "origin/deploy/uat");
    }

    #[test]
    fn commits_by_hash_and_message() {
        let (_d, repo) = temp_repo();
        let a = commit_files(&repo, &[("a", "1")], "fix login on safari");
        commit_files(&repo, &[("b", "1")], "add face id");
        assert_eq!(commit(&repo, &a[..7]).unwrap().id, a);
        assert!(commit(&repo, "deadbeef").is_err());
        assert!(commit(&repo, "main").is_err());
        assert_eq!(commit_by_hash_or_message(&repo, "fix login").unwrap().id, a);
        assert!(commit_by_hash_or_message(&repo, "nothing like this").is_err());
        assert_eq!(author(&repo, "me").unwrap(), "test@example.com");
        assert_eq!(author(&repo, "arben").unwrap(), "arben");
    }

    #[test]
    fn conflicted_files_by_path_name_and_spelling() {
        let c = vec!["src/app/Login.java".to_string(), "pom.xml".to_string()];
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            conflicted_paths(&c, &s(&["Login.java", "pom.xml"])).unwrap(),
            c
        );
        assert_eq!(
            conflicted_paths(&c, &s(&["app/login.java"])).unwrap(),
            s(&["src/app/Login.java"])
        );
        assert_eq!(
            conflicted_paths(&c, &s(&["pom.xm"])).unwrap(),
            s(&["pom.xml"])
        );
        assert!(conflicted_paths(&c, &s(&[" conflicted files"])).is_err());
    }

    #[test]
    fn edit_distance() {
        assert_eq!(distance("deply/prod", "deploy/prod"), 1);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("uat", "uat"), 0);
    }
}
