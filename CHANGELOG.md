# Changelog

All notable changes to gitjet are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.4] - 2026-10-28

### Changed

- Checking out a branch takes it from the remote as-is (`[branches] checkout_from_remote`, on by
  default): gitjet fetches that branch, then creates or resets the local branch to the remote one
  and tracks it. A dialog shows each step (looking up, fetching, comparing, switching). When the
  local branch has commits that are not on the remote, gitjet asks whether to reset it or check
  out the local branch as it is. Offline, the last fetched state is used. Set the option to
  `false` for the previous behavior.

## [0.1.2] - 2026-10-28

### Changed

- Tabs load in the background: Changes, Branches, Conflicts, Shelf, Files and Repository no
  longer freeze the screen while git runs (50–150 ms per git command on Windows). The status bar
  shows a spinner ("Loading Changes") and panes show "Loading…" until data arrives.
- Diff previews (Changes, Shelf) and branch details load once the selection rests, so holding an
  arrow key no longer runs git for every row.
- Refresh requests are combined (one refresh per screen update), and returning to the terminal
  window refreshes at most once a second.
- Submodule init / sync / deinit, aborting a rebase and "Get revision" in file history run in the
  background with a spinner.
- Creating a branch runs in the background: the New branch dialog shows each step ("Creating
  feature and switching to it…") with a spinner and elapsed time, and errors stay inline.

### Fixed

- A branch created from a remote branch (for example from `origin/main`) no longer tracks it, so
  its first push publishes it under its own name instead of pushing to `origin/main`. The dialog
  says so. Overwriting an existing branch keeps that branch's upstream.

### Added

- `GITJET_TRACE=<file>` logs every git command with its duration and whether the screen waited
  for it.
- `GITJET_GIT=<path>` selects the git executable (skips package manager shims on Windows).

## [0.1.1] - 2026-10-27

### Changed

- The status bar (branch, ahead/behind, operation in progress) loads in the background instead
  of running git on every refresh, so refreshes no longer freeze the screen on Windows.
- Log queries start one git process fewer.
- Push errors now show why the remote rejected the push (for example "fetch first") instead of
  only "failed to push some refs".

### Added

- Large repositories (20,000+ objects or 300+ refs): gitjet writes git's commit-graph cache in
  the background when none exists, which makes the log, ahead/behind counts and "branches
  containing" several times faster. Turn it off with `write_commit_graph = false` under
  `[performance]`.
- Troubleshooting guide: tips for large repositories and Windows.

## [0.1.0] - 2026-10-27

Initial release.

### Added

#### General

- Terminal UI for Windows, macOS and Linux with nine tabs: Changes, Log, Branches, Stash & Shelf,
  Files, Conflicts, Pull Requests, Local History and Repository.
- Every operation runs the installed `git`, so hooks, credential helpers, signing,
  `.gitattributes` and Git config behave as on the command line. Background Git never prompts.
- Action palette (`:` or `` Alt+` ``) with fuzzy search over every action.
- Context menus (`.` or `m`) for the selection on every tab, help overlay (`?`) for every screen
  and a hint line with the most important keys.
- Status bar with branch, ahead/behind counts, repository state, messages and a spinner for
  background tasks.
- Mouse support: click tabs and rows, scroll lists.
- Dark and light themes with 24-bit colors.
- Optional TOML config file with `gitjet config-init` and `gitjet config-path`.
- Command line: `gitjet -C <dir>`, `gitjet clone <url> [dir]`, `gitjet init [dir]`.
- Per-repository data in `.git/gitjet/`, never visible in the work tree.
- Release binaries for Windows (x64, ARM64), macOS (Apple Silicon, Intel) and Linux (x64, ARM64,
  static), install scripts for PowerShell and `sh`, Scoop and Homebrew templates.

#### Changes and commit

- Changes tab: changed files grouped into Merge Conflicts, changelists and
  Unversioned Files, flat list or directory tree, path filter, staging indicators (fully,
  partially, not staged), diff preview in Local, Unstaged and Staged modes.
- Changelists (named groups of changed files): create, rename, describe, delete, set active, move files; stored per
  worktree.
- Stage and unstage files, directories or whole changelists; stage all, unstage all.
- Partial staging screen: stage, unstage or discard single lines or whole hunks, also for
  untracked files.
- Commit dialog: message editor with subject-length counter, message history (last 50, `Ctrl+P` /
  `Ctrl+N`, searchable list), drafts kept between openings, amend, sign-off, GPG/SSH signing,
  author override, commit of exactly the selected files or changelist.
- Commit checks: configurable commands that run before committing, with their output and
  "commit anyway".
- Commit and push: opens the push dialog after a successful commit.
- Rollback of selected files and "Rollback all".
- Add unversioned files to `.gitignore`, delete unversioned files.
- Shelve the selection or stash all changes from the Changes tab.

#### Diff viewer

- Fullscreen diff viewer with a file list, unified and side-by-side layouts, word-level and
  syntax highlighting, ignore whitespace, adjustable context, whole-file view, next/previous change
  across files (`n` / `N`, `F7` / `Shift+F7`), fuzzy file filter, sideways scrolling and mouse
  support.
- Open the file in your editor at the caret line; jump to annotate, file history and local
  history from any diff.
- Compare with a branch, tag or any revision (against the work tree, since the merge base in
  either direction, or directly), compare two files, show all local, staged or unstaged changes.

#### Log

- Commit graph with ref labels, relative or absolute dates, your own commits in bold, and paging
  for large histories.
- Filters: branch (all, `HEAD`, any branch or tag), user (including yourself), date presets and
  since/until, paths, text in messages or in diff content (`-S`), first-parent only, no merges.
- Details pane with the full message, parents, author and committer, refs and changed files, with
  per-file diff, annotate at revision, file history and compare with local.
- History operations: checkout revision, new branch or tag here, cherry-pick and revert (one or
  several marked commits), reset (soft, mixed, hard, keep), undo last commit, edit commit message,
  interactively rebase from here, compare with previous, with local, or two marked commits.
- Go to a commit by hash, branch, tag or revision expression; copy hashes and paths to the
  clipboard through OSC 52.

#### Branches and remotes

- Branches tab with Recent, Favorites, Local and Remote sections, upstream ahead/behind and
  "gone" markers, name filter, and the commits of the selected branch compared with the current
  one.
- Checkout of local and remote branches, with smart checkout (stash, checkout, unstash) and force
  checkout when local changes are in the way.
- New branch dialog with "Checkout branch" and "Overwrite existing branch", rename, safe delete
  with force-delete fallback, remote branch deletion, favorites.
- Merge into current (merge, `--no-ff`, squash, fast-forward only), rebase current onto selected,
  checkout and rebase onto current.
- Compare with current and show diff with the working tree; set and unset upstream.
- Background fetch, pull (merge, rebase, fast-forward only) and Update Project (fetch, then rebase
  or merge, configurable).
- Push dialog listing the commits and files to push, with force-with-lease, push tags, set
  upstream and change target.
- "(in terminal)" variants of Fetch, Pull and Push for credential and SSH prompts.

#### Interactive rebase

- Rebase editor: pick, reword, edit, squash, fixup, drop, reorder, reset, search, commit diff,
  warnings for pushed commits and merges, plan validation.
- Palette shortcuts for interactive rebase, squash last N commits and reword commit; continue,
  skip and abort; rebase status menu.

#### Conflicts

- Conflicts tab with a badge; gitjet switches to it automatically when an operation stops on
  conflicts.
- Per-file resolution: accept yours or theirs, mark resolved, edit externally, resolve by
  deleting, compare yours with theirs or with the base; accept yours or theirs for all files.
- Continue, skip and abort for merge, rebase, cherry-pick, revert and `git am`.
- 3-pane merge resolver (Yours / Result / Theirs) with per-change accept yours, theirs, both or
  base, undo, apply all non-conflicting changes, external editing of the result and save options.
- Whole-file "choose a version" screen for binary files and delete/modify conflicts.

#### Stash, shelf and patches

- Stash: create (message, include untracked, keep index), apply and pop (with restore index),
  drop, clear, new branch from stash.
- Shelf: shelve selected files (optionally keeping them in the work tree), unshelve
  all or selected files, unshelved state and clean-up, rename, delete, export as patch.
- Patches: create from local changes or from a commit, apply plain diffs or mailboxes after a
  preview; continue, skip and abort `git am`.

#### Files, blame and history

- Files tab: project tree of tracked and untracked files with status colors, fuzzy filter and
  content preview.
- Annotate (blame) with age colors, commit diff of a line, annotate previous revision, commit
  details, find, go to line and jumps between blocks of the same commit.
- File history that follows renames, directory history, and history for selected lines
  (`git log -L`).
- Show a file at any revision, compare a file with any revision, restore a file from a revision.

#### Local history

- Automatic background snapshots of changed files, independent of commits, with labels, per-file
  and timeline views, diff against the current file, revert to a snapshot and automatic pruning.

#### Pull requests

- Pull Requests tab. See [docs/pull-requests.md](docs/pull-requests.md).

#### Repository

- Tags (lightweight and annotated, push, delete local and remote, checkout), remotes (add, edit
  URL, push URL, rename, remove, fetch, prune), worktrees (add, open, remove, force remove,
  lock/unlock, prune), submodules (init, update, sync, add, deinit, open), ignore rules
  (`.gitignore` and `info/exclude`, check a path, list ignored files) and common Git config
  values (repository and global).
- Repository switcher for the parent repository, worktrees, submodules, nested and recent
  repositories; clone and init from inside the app.

[Unreleased]: https://github.com/kashari/gitjet/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/kashari/gitjet/compare/v0.1.2...v0.1.4
[0.1.2]: https://github.com/kashari/gitjet/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/klajdishark/gitjet/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/klajdishark/gitjet/releases/tag/v0.1.0
