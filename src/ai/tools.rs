//! The AI tool registry: every action the model may call, with typed arguments.
//!
//! This file is the single source of truth. `gitjet dump-tools` prints it for the training
//! pipeline; at runtime the same structs build the decoding grammar and parse the model's call.
//! Argument values are what the user *said* ("the login fix", "main"); `resolve` turns them into
//! real refs, paths and commits before anything runs.
//!
//! One call can stand for a whole flow: `merge` with `push` is fetch, checkout, merge, push. The
//! model only fills in the call; `plan` expands it into steps the user confirms. A release or
//! hotfix is a `new_branch` named `release/x.x.x` or `hotfix/x.x.x(.N)`; the plan then sets that
//! version in the project's version file and commits it.

use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How much a tool can change. Drives the confirmation step.
///
/// This is the tool's floor; a plan can raise it (`merge` with `push` is `Publishes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reads only, or only updates remote-tracking refs (log, fetch). Runs without asking.
    Read,
    /// Changes the repo but the reflog can undo it (checkout, merge, cherry-pick).
    Undoable,
    /// Sends work to the remote, which a local undo can't take back (push). Always confirms.
    Publishes,
    /// Can lose work or rewrite shared history (force push, hard reset, discard).
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeModeArg {
    #[default]
    Default,
    NoFf,
    Squash,
    FfOnly,
}

// What a merge does if it conflicts; absent means stop and open the Conflicts tab. Plain
// comments, not doc comments: doc comments turn the enum into a long `oneOf` in the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnConflictArg {
    // Undo the merge and leave the branch as it was.
    Abort,
    // Take our side where the branches conflict (`-X ours`); clean changes from both are kept.
    Ours,
    // Take their side where the branches conflict (`-X theirs`).
    Theirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SideArg {
    Ours,
    Theirs,
}

/// Create a branch and check it out.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewBranchArgs {
    /// Name as the user said it, with "feature/" in front when they call it a feature branch
    pub name: String,
    /// Branch to start from; omit for the current branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Merge a branch, tag or commit into a branch.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeArgs {
    /// What to merge, as the user said it: a branch name, a hash, or words from a commit message
    pub source: String,
    /// Branch to merge into; omit for the current branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into: Option<String>,
    /// Merge strategy; omit for git's default
    #[serde(default)]
    pub mode: MergeModeArg,
    /// If it conflicts: "abort" (cancel, reject), "theirs" or "ours" (take that side); omit to stop and resolve by hand
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_conflict: Option<OnConflictArg>,
    /// True when the user also asks to push the result
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub push: bool,
}

/// Copy commits onto a branch.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CherryPickArgs {
    /// Commit hashes exactly as the user typed them; omit to pick from a list
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    /// Branch the commits are on, to list the ones missing from the target
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Only commits by this author ("me" for the current user)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Words from the commit message, as the user quoted them
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Branch to put the commits on; omit for the current branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<String>,
    /// True when the user also asks to push the result
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub push: bool,
}

/// Resolve merge/rebase conflicts by taking one side for whole files.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveConflictsArgs {
    /// "ours" = the current branch's version (yours, mine); "theirs" = the incoming one
    pub side: SideArg,
    /// Files to resolve; omit for every conflicted file
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// Check out a branch.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwitchArgs {
    /// Branch as the user said it
    pub branch: String,
}

/// Fetch all remotes. Updates remote-tracking branches only.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchArgs {}

/// Pull a branch from its remote.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullArgs {
    /// Branch to update; omit for the current branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Push a branch to its remote. Never a force push.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushArgs {
    /// Branch to push; omit for the current branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Find commits by author, message, date, path or branch.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindCommitsArgs {
    /// Author name or e-mail as the user said it ("me" for the current user)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Words to search for in commit messages
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Start date as the user said it, e.g. "last monday", "2026-01-01"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// End date as the user said it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Only commits that touch this file or folder
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Branch to look on; omit for the current branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Only commits not yet on this branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_in: Option<String>,
}

/// Ask the user a question when the request is ambiguous or missing something.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClarifyArgs {
    /// One short question
    pub question: String,
}

/// The request is not something gitjet can do.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnsupportedArgs {
    /// Why, in a few words
    pub reason: String,
}

/// A parsed, typed model call.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    NewBranch(NewBranchArgs),
    Merge(MergeArgs),
    CherryPick(CherryPickArgs),
    ResolveConflicts(ResolveConflictsArgs),
    Switch(SwitchArgs),
    Fetch(FetchArgs),
    Pull(PullArgs),
    Push(PushArgs),
    FindCommits(FindCommitsArgs),
    Clarify(ClarifyArgs),
    Unsupported(UnsupportedArgs),
}

/// One entry of the registry.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    schema: fn() -> Value,
    parse: fn(Value) -> serde_json::Result<ToolCall>,
}

fn schema<T: JsonSchema>() -> Value {
    // Inline enums (no $defs/$ref) and plain types for optional fields (no ["string", "null"]):
    // shorter prompts, and simpler for a 270M model and for the decoding grammar.
    let generator = SchemaSettings::draft2020_12()
        .with(|s| {
            s.inline_subschemas = true;
        })
        .into_generator();
    let mut v = serde_json::to_value(generator.into_root_schema_for::<T>()).unwrap_or(Value::Null);
    drop_null_types(&mut v);
    // Keep only what a function schema needs; title/$schema just cost prompt tokens.
    if let Some(o) = v.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
        o.remove("description");
    }
    v
}

/// `Option<String>` comes out as `"type": ["string", "null"]`, and `Option<Enum>` as
/// `"anyOf": [{enum}, {"type": "null"}]`. Optional is already expressed by leaving the field out
/// of `required`, so keep just the real type.
fn drop_null_types(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if let Some(Value::Array(any)) = o.get("anyOf") {
                let real: Vec<Value> = any
                    .iter()
                    .filter(|s| s.get("type") != Some(&Value::from("null")))
                    .cloned()
                    .collect();
                if let [Value::Object(inner)] = real.as_slice() {
                    let inner = inner.clone();
                    o.remove("anyOf");
                    for (k, val) in inner {
                        o.entry(k).or_insert(val);
                    }
                }
            }
            if let Some(Value::Array(values)) = o.get_mut("enum") {
                values.retain(|e| !e.is_null());
            }
            if let Some(Value::Array(types)) = o.get("type") {
                let real: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
                if real.len() == 1 {
                    o.insert("type".into(), real[0].clone());
                }
            }
            o.values_mut().for_each(drop_null_types);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_null_types),
        _ => {}
    }
}

macro_rules! tool {
    ($name:literal, $desc:literal, $risk:expr, $args:ty, $variant:path) => {
        ToolDef {
            name: $name,
            description: $desc,
            risk: $risk,
            schema: schema::<$args>,
            parse: |v| serde_json::from_value::<$args>(v).map($variant),
        }
    };
}

/// Every tool, in a fixed order. Add new tools at the end: the order is part of the prompt.
pub const TOOLS: &[ToolDef] = &[
    tool!(
        "new_branch",
        "Create a branch from another branch and check it out.",
        Risk::Undoable,
        NewBranchArgs,
        ToolCall::NewBranch
    ),
    tool!(
        "merge",
        "Merge a branch, tag or commit into a branch (default: the current branch).",
        Risk::Undoable,
        MergeArgs,
        ToolCall::Merge
    ),
    tool!(
        "cherry_pick",
        "Copy commits onto a branch: by hash, by author and message words, or from a list.",
        Risk::Undoable,
        CherryPickArgs,
        ToolCall::CherryPick
    ),
    tool!(
        "resolve_conflicts",
        "Resolve conflicted files by taking our side or their side wholesale.",
        Risk::Undoable,
        ResolveConflictsArgs,
        ToolCall::ResolveConflicts
    ),
    tool!(
        "switch",
        "Check out a branch.",
        Risk::Undoable,
        SwitchArgs,
        ToolCall::Switch
    ),
    tool!(
        "fetch",
        "Fetch all remotes; your own branches don't change.",
        Risk::Read,
        FetchArgs,
        ToolCall::Fetch
    ),
    tool!(
        "pull",
        "Pull a branch from its remote (default: the current branch).",
        Risk::Undoable,
        PullArgs,
        ToolCall::Pull
    ),
    tool!(
        "push",
        "Push a branch to its remote (never a force push).",
        Risk::Publishes,
        PushArgs,
        ToolCall::Push
    ),
    tool!(
        "find_commits",
        "Find commits by author, message words, date range, path or branch.",
        Risk::Read,
        FindCommitsArgs,
        ToolCall::FindCommits
    ),
    tool!(
        "clarify",
        "Ask one question when the request is ambiguous or missing a branch, file or commit.",
        Risk::Read,
        ClarifyArgs,
        ToolCall::Clarify
    ),
    tool!(
        "unsupported",
        "The request is not a git task gitjet can do.",
        Risk::Read,
        UnsupportedArgs,
        ToolCall::Unsupported
    ),
];

pub fn find(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Parse `{"name": ..., "arguments": {...}}` into a typed call.
pub fn parse_call(name: &str, arguments: Value) -> anyhow::Result<ToolCall> {
    let def = find(name).ok_or_else(|| anyhow::anyhow!("unknown tool `{name}`"))?;
    Ok((def.parse)(arguments)?)
}

/// The tool list in the OpenAI / Hugging Face `tools=` format.
pub fn dump() -> Value {
    Value::Array(
        TOOLS
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": (t.schema)(),
                    },
                    "x-gitjet-risk": t.risk,
                })
            })
            .collect(),
    )
}

/// Short hash of the dump. A model is trained against one registry; refuse a model whose
/// hash differs from the running binary's.
pub fn registry_hash() -> String {
    let text = serde_json::to_string(&dump()).unwrap_or_default();
    // FNV-1a 64: stable across platforms and Rust versions, no extra dependency.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_snake_case() {
        let mut names: Vec<_> = TOOLS.iter().map(|t| t.name).collect();
        assert!(names
            .iter()
            .all(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '_')));
        names.sort();
        names.dedup();
        assert_eq!(names.len(), TOOLS.len());
    }

    #[test]
    fn every_argument_is_described() {
        // The model only sees descriptions; an undescribed argument is a guess.
        for t in TOOLS {
            let s = (t.schema)();
            for (arg, spec) in s["properties"].as_object().into_iter().flatten() {
                assert!(spec.get("description").is_some(), "{}.{arg}", t.name);
            }
        }
    }

    #[test]
    fn no_tool_can_force() {
        // Force push, hard reset and discard need typed confirmation; no v1 tool offers them.
        let text = serde_json::to_string(&dump()).unwrap();
        assert!(!text.contains("\"force"), "{text}");
        assert!(TOOLS.iter().all(|t| t.risk < Risk::Destructive));
    }

    #[test]
    fn calls_round_trip() {
        let call = parse_call(
            "merge",
            json!({"source": "fix login", "into": "main", "mode": "no_ff", "push": true}),
        )
        .unwrap();
        assert_eq!(
            call,
            ToolCall::Merge(MergeArgs {
                source: "fix login".into(),
                into: Some("main".into()),
                mode: MergeModeArg::NoFf,
                on_conflict: None,
                push: true,
            })
        );
        let call = parse_call(
            "merge",
            json!({"source": "origin/develop", "into": "deploy/uat", "on_conflict": "theirs"}),
        )
        .unwrap();
        assert!(matches!(
            call,
            ToolCall::Merge(MergeArgs {
                on_conflict: Some(OnConflictArg::Theirs),
                ..
            })
        ));
        assert!(parse_call("merge", json!({"source": "x", "on_conflict": "skip"})).is_err());
        let call = parse_call(
            "cherry_pick",
            json!({"from": "develop", "author": "me", "onto": "deploy/uat"}),
        )
        .unwrap();
        assert_eq!(
            call,
            ToolCall::CherryPick(CherryPickArgs {
                from: Some("develop".into()),
                author: Some("me".into()),
                onto: Some("deploy/uat".into()),
                ..Default::default()
            })
        );
        assert_eq!(
            parse_call(
                "cherry_pick",
                json!({"author": "me", "message": "fix login"})
            )
            .unwrap(),
            ToolCall::CherryPick(CherryPickArgs {
                author: Some("me".into()),
                message: Some("fix login".into()),
                ..Default::default()
            })
        );
        assert!(parse_call("create_release", json!({})).is_err());
        assert_eq!(
            parse_call("pull", json!({"branch": "deploy/prod"})).unwrap(),
            ToolCall::Pull(PullArgs {
                branch: Some("deploy/prod".into())
            })
        );
        assert!(parse_call("merge", json!({"into": "main"})).is_err());
        assert!(parse_call("merge", json!({"source": "x", "bogus": 1})).is_err());
        assert!(parse_call("push", json!({"force": true})).is_err());
        assert!(parse_call("rm_rf", json!({})).is_err());
    }

    #[test]
    fn defaults_are_left_out() {
        // Training rows omit defaulted arguments; serializing a call must do the same.
        let v = serde_json::to_value(CherryPickArgs::default()).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn schemas_are_flat() {
        let text = serde_json::to_string(&dump()).unwrap();
        assert!(!text.contains("$ref") && !text.contains("$defs"), "{text}");
        assert!(!text.contains("null"), "{text}");
    }

    #[test]
    fn dump_is_stable() {
        assert_eq!(dump(), dump());
        assert_eq!(registry_hash().len(), 16);
    }
}
