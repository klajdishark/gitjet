# gitjet

A keyboard-driven terminal Git client with the Git features of a full-featured IDE:
changelists, shelf, line-by-line staging, a 3-way merge resolver, interactive rebase, blame,
local history and pull requests. It runs in any terminal on **Windows, macOS and Linux**, ships
as a **single binary**, and works with **any Git hosting provider**.

![Changes](docs/screenshots/changes.png)

## Features

| Area | What you get |
|---|---|
| **Changes** (commit window) | Changelists, unversioned files, staged/partial indicators, diff preview, stage/unstage files, **partial staging by line or hunk**, discard selected lines, rollback, add to `.gitignore` |
| **Commit** | Message editor with subject-length hint, message history, amend, sign-off, GPG/SSH signing, author override, **commit checks** (your own commands run before committing), commit and push |
| **Diff viewer** | Unified and **side-by-side**, word-level highlighting, syntax highlighting, ignore whitespace, more/less context, jump between changes, open the file at the line in the built-in Vim-style editor |
| **Log** | Commit graph, refs and tags, filters (branch, user, date, path, text, content), details pane, cherry-pick, revert, reset (soft/mixed/hard/keep), undo commit, edit message, new branch/tag here, copy hash |
| **Branches** | Recent, favorites, local and remote branches with ahead/behind, checkout (smart checkout with stash), new, rename, delete, merge (no-ff/squash/ff-only), rebase onto, compare with current, set upstream |
| **Fetch / Pull / Push** | Background fetch, pull (merge/rebase/ff-only), Update Project, **push dialog with the commits to push**, force-with-lease, push tags, "in terminal" variants for password/SSH prompts |
| **Interactive rebase** | Reorder, pick, reword, edit, squash, fixup, drop; continue/abort/skip |
| **Conflicts** | Conflicts tab with badge, opens automatically after a merge/rebase/pull stops, **3-pane merge resolver** (yours / result / theirs), apply non-conflicting changes in one key, accept yours/theirs, continue/abort |
| **Stash & Shelf** | Stash with untracked/keep-index, apply/pop/drop/branch; **shelf** with partial unshelve; create and apply patch files |
| **Files** | Project tree, **annotate (blame)** with age colors, annotate previous revision, file history across renames, **history for selected lines**, file at revision, restore revision |
| **Pull requests** | List, review diff, checkout, create, comment, approve/request changes, merge, close/reopen — GitHub, GitLab, Bitbucket, Azure DevOps, Gitea/Forgejo and self-hosted servers |
| **Editor** | Built-in Vim-style editor that works the same on every platform: normal / insert / visual modes, motions, operators and text objects, registers, undo/redo, `.` repeat, `/` search, `:%s/pat/rep/g`, `:g/pat/d`, `:sort`, `:normal`, syntax highlighting (`:help` lists the commands) |
| **Local History** | Automatic snapshots of changed files, independent of commits; labels; diff against now; revert a file to any snapshot |
| **Repository** | Tags, remotes, worktrees, submodules, ignore rules, config, **repository switcher** (worktrees, submodules, nested and recent repos), clone, init |
| **Everywhere** | `:` action palette to search every action, `?` help for every screen, mouse support, light and dark themes |

<table>
<tr><td><img src="docs/screenshots/diff-side-by-side.png" alt="Side-by-side diff"></td><td><img src="docs/screenshots/log.png" alt="Log with graph"></td></tr>
<tr><td><img src="docs/screenshots/merge-resolver.png" alt="3-way merge resolver"></td><td><img src="docs/screenshots/commit.png" alt="Commit dialog"></td></tr>
<tr><td><img src="docs/screenshots/branches.png" alt="Branches"></td><td><img src="docs/screenshots/actions.png" alt="Action palette"></td></tr>
</table>

## Documentation

The full guide is in [docs/](docs/README.md):

- [Getting started](docs/getting-started.md) — install, first launch, how the screen works
- [Feature overview](docs/feature-overview.md) — common Git tasks and where to find them
- One page per area: [Changes and commit](docs/changes-and-commit.md), [Diff viewer](docs/diff-viewer.md),
  [Log](docs/log.md), [Branches and remotes](docs/branches-and-remotes.md),
  [Interactive rebase](docs/interactive-rebase.md), [Conflicts](docs/conflicts.md),
  [Stash, shelf and patches](docs/stash-shelf-patches.md), [Files, blame and history](docs/files-blame-history.md),
  [Pull requests](docs/pull-requests.md), [Local history](docs/local-history.md), [Repository](docs/repository.md)
- [Keyboard reference](docs/keyboard-reference.md), [Configuration](docs/configuration.md),
  [Troubleshooting](docs/troubleshooting.md)
- For contributors: [Architecture](docs/architecture.md), [Contributing](CONTRIBUTING.md), [Changelog](CHANGELOG.md)

## Install

gitjet is one executable. You need [Git](https://git-scm.com) installed.

**Windows** (PowerShell):

```powershell
irm https://github.com/kashari/gitjet/releases/latest/download/install.ps1 | iex
```

**macOS / Linux**:

```bash
curl -fsSL https://github.com/kashari/gitjet/releases/latest/download/install.sh | sh
```

**Manual**: download the archive for your platform from the
[releases page](https://github.com/kashari/gitjet/releases), unpack it, and put `gitjet`
(`gitjet.exe` on Windows) somewhere on your `PATH`.

**From source** (Rust 1.80+):

```bash
cargo install --git https://github.com/kashari/gitjet
```

| Platform | Archive |
|---|---|
| Windows x64 / ARM64 | `gitjet-x86_64-pc-windows-msvc.zip`, `gitjet-aarch64-pc-windows-msvc.zip` |
| macOS Apple Silicon / Intel | `gitjet-aarch64-apple-darwin.tar.gz`, `gitjet-x86_64-apple-darwin.tar.gz` |
| Linux x64 / ARM64 (static) | `gitjet-x86_64-unknown-linux-musl.tar.gz`, `gitjet-aarch64-unknown-linux-musl.tar.gz` |

The binaries are not code-signed yet: macOS may ask you to allow the app in
*System Settings → Privacy & Security*, and Windows SmartScreen may show "Unknown publisher".
The install scripts download with `curl`/PowerShell, which normally avoids both prompts.

## Usage

```bash
gitjet                 # open the repository in the current directory
gitjet -C path/to/repo # open another repository
gitjet clone <url>     # clone, then open
gitjet init [dir]      # create a repository, then open
gitjet config-init     # write a config file with all defaults
gitjet config-path     # show where the config file lives
```

### Keys

Every screen lists its keys at the bottom; press `?` for the full list.

| Key | Anywhere |
|---|---|
| `1` … `9` | Changes, Log, Branches, Stash & Shelf, Files, Conflicts, Pull Requests, Local History, Repository |
| `[` `]` | previous / next tab |
| `:` or <kbd>Alt</kbd>+<kbd>`</kbd> | action palette (type to search all actions) |
| `.` or `m` | context menu for the selection |
| `Enter` / `Esc` | open / back |
| `Tab` | next pane |
| `/` | filter |
| `F5` | refresh |
| `?` | help |
| `q`, `Ctrl+C` | quit |

## Hosting providers

All Git features work with any remote: GitHub, GitLab, Bitbucket, Azure DevOps, Gitea, Forgejo,
your own server, or no server at all. Pull requests talk to the provider's web API directly; no
provider CLI is needed.

Tokens are never stored in gitjet's config. gitjet looks for one in this order:

1. `GITJET_TOKEN` (or `GITJET_TOKEN_<HOST>`, e.g. `GITJET_TOKEN_GIT_EXAMPLE_COM`)
2. the provider's usual variable: `GITHUB_TOKEN`/`GH_TOKEN`, `GITLAB_TOKEN`,
   `BITBUCKET_TOKEN` (or `BITBUCKET_USERNAME` + `BITBUCKET_APP_PASSWORD`),
   `AZURE_DEVOPS_EXT_PAT`, `GITEA_TOKEN`
3. your Git credential helper (Git Credential Manager, macOS Keychain, libsecret…), the same
   place `git push` gets its password from. *Sign in* in the Pull Requests tab saves a token there.

Self-hosted servers are detected automatically. If one isn't, name its kind in the config:

```toml
[[hosting.hosts]]
host = "git.example.com"
kind = "gitlab"        # github, gitlab, bitbucket, bitbucket-server, azure, gitea, none
# api_url = "https://git.example.com/api/v4"   # only if it isn't the default
```

## Configuration

The config file is optional. `gitjet config-init` writes one with every setting. It lives at
`%APPDATA%\gitjet\config.toml` on Windows, `~/Library/Application Support/gitjet/config.toml`
on macOS, and `~/.config/gitjet/config.toml` on Linux.

```toml
theme = "dark"            # or "light"
mouse = true
builtin_editor = true     # open files in gitjet's built-in Vim-style editor
# editor = "code --wait"  # used when builtin_editor = false; defaults to $VISUAL, $EDITOR, then notepad / vi

[diff]
side_by_side = false
context_lines = 3
ignore_whitespace = false
word_diff = true
syntax_highlight = true

[commit]
sign_off = false
gpg_sign = false
subject_limit = 72

[[commit.checks]]         # run before every commit; a failure blocks it
name = "Tests"
command = "cargo test"

[log]
page_size = 500
all_branches = true

[local_history]
enabled = true
interval_secs = 60
keep_days = 14
max_file_kb = 1024

[update]
method = "rebase"         # how Pull and Update Project integrate: "rebase" or "merge"
prune = true
```

gitjet keeps its per-repository data (changelists, shelf, local history, favorites, commit
message history) in `.git/gitjet/`, so it never shows up as a change in your work tree.

## How it works

gitjet runs your installed `git` for every operation, so hooks, credential helpers, signing,
`.gitattributes` and your Git config behave exactly as on the command line. Background Git
never waits on a terminal prompt; when an operation needs a password or SSH passphrase, use the
*(in terminal)* version of the action.

## Development

```bash
cargo run -- -C /path/to/some/repo
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

CI runs the tests on Windows, macOS and Linux. Pushing a tag such as `v0.1.0` builds release
binaries for all six platforms and attaches them to a GitHub Release. Templates for a Scoop
bucket and a Homebrew tap are in `packaging/`.

## License

MIT
