[← Back to docs index](README.md)

# Configuration

gitjet works without any configuration. A config file lets you change the theme, the editor, diff
defaults, commit options, commit checks, log paging, local history and how Pull and Update Project
integrate changes.

## Where the config file lives

The file is `gitjet/config.toml` inside your user configuration directory:

| OS | Path |
|---|---|
| Windows | `%APPDATA%\gitjet\config.toml` (for example `C:\Users\you\AppData\Roaming\gitjet\config.toml`) |
| macOS | `~/Library/Application Support/gitjet/config.toml` |
| Linux | `$XDG_CONFIG_HOME/gitjet/config.toml`, which is `~/.config/gitjet/config.toml` when `XDG_CONFIG_HOME` is not set |

Two commands help:

```bash
gitjet config-path   # print the path (the file does not have to exist)
gitjet config-init   # write a file with every setting at its default, then print the path
```

`config-init` never overwrites an existing file. The `?` help screen inside gitjet also shows the
path at the bottom.

gitjet reads the file once at startup. Restart gitjet after editing it.

Every setting is optional. A file that only contains `theme = "light"` is valid; everything else
keeps its default. If the file cannot be parsed, gitjet starts with all defaults and shows the
error message, which includes the file path and the problem.

## Full example

This is the default configuration, plus a commit check and a host entry as examples:

```toml
theme = "dark"            # "dark" or "light"
mouse = true
# editor = "nano"       # default: $VISUAL, then $EDITOR, then notepad (Windows) / vi

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

[[commit.checks]]
name = "Tests"
command = "cargo test"
enabled = true

[log]
page_size = 500
all_branches = true

[local_history]
enabled = true
interval_secs = 60
keep_days = 14
max_file_kb = 1024

[update]
method = "rebase"         # "rebase" or "merge"
prune = true

[branches]
checkout_from_remote = true

[hosting]
use_git_credentials = true

[[hosting.hosts]]
host = "git.example.com"
kind = "gitlab"
# api_url = "https://git.example.com/api/v4"
```

## Settings

### Top level

| Key | Type | Default | Description |
|---|---|---|---|
| `theme` | string | `"dark"` | Color theme: `"dark"` or `"light"` (case does not matter). Any other value uses the dark theme. The theme also picks the syntax highlighting colors. |
| `mouse` | bool | `true` | Mouse support: click tabs, scroll lists, click rows. Set to `false` to leave the mouse to your terminal (for example for selecting text). |
| `editor` | string | not set | Command used to open files ("open in editor" actions, editing a conflicted file). When not set, gitjet uses `$VISUAL`, then `$EDITOR`, then `notepad` on Windows and `vi` elsewhere. The command runs through the system shell (`cmd /C` on Windows, `sh -c` elsewhere), so it can include arguments, for example `"your-editor --wait"`. gitjet suspends its screen while the editor runs. |

When gitjet opens a file at a specific line, it passes `+<line>` before the file name if the editor
is `vi`, `vim`, `nvim`, `nano`, `emacs`, `hx`, `micro` or `kak`. Other editors open the file at
the top.

### `[diff]`

These set the starting state of every diff. Most of them can be toggled per diff with a key in the
[diff viewer](diff-viewer.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `side_by_side` | bool | `false` | Start the diff viewer in side-by-side layout instead of unified. Toggle with `s`. |
| `context_lines` | integer | `3` | Unchanged lines shown around each change (passed to Git as `-U<n>`). Change with `+` / `-`. |
| `ignore_whitespace` | bool | `false` | Ignore whitespace changes (`--ignore-all-space`). Toggle with `W`. |
| `word_diff` | bool | `true` | Highlight the changed words inside changed lines. Toggle with `w`. |
| `syntax_highlight` | bool | `true` | Syntax colors in diffs. Toggle with `h`. |

The inline diff previews (for example in the Changes tab) use the same settings.

### `[commit]`

| Key | Type | Default | Description |
|---|---|---|---|
| `sign_off` | bool | `false` | The "Sign-off" option in the commit dialog starts switched on (adds a `Signed-off-by` trailer). |
| `gpg_sign` | bool | `false` | The "GPG sign" option in the commit dialog starts switched on (`git commit -S`, using your Git signing setup). The option also starts on when Git's own `commit.gpgsign` is true. When you switch the option off, gitjet passes `--no-gpg-sign`. |
| `subject_limit` | integer | `72` | The commit dialog counts the characters of the first line and shows it in the warning color when it is longer than this. Also used by the Log's "Edit commit message" editor. `0` turns the warning off. |
| `checks` | array of tables | empty | Commit checks, see below. |

#### Commit checks

Commit checks are commands that run before each commit, for example tests or a linter. A
check that exits with a non-zero code blocks the commit. gitjet then shows the output of the
checks and lets you go back or commit anyway. See
[Changes and commit](changes-and-commit.md#commit-checks).

Each check is a `[[commit.checks]]` table:

| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string | empty | Name shown in the commit dialog and in the output. When empty, the command is shown instead. |
| `command` | string | required | Command line to run. It runs through the system shell (`cmd /C` on Windows, `sh -c` elsewhere) in the repository root, with no input. Checks with an empty command are ignored. |
| `enabled` | bool | `true` | Set to `false` to switch a check off without deleting it. Disabled checks are not shown and never run. |

In the commit dialog, a single "Run checks" option (switched on whenever at least one check is
configured) decides whether the checks run for this commit. All enabled checks run in order, even
after one of them fails, so you see every failure at once.

Examples for different ecosystems:

```toml
# Rust
[[commit.checks]]
name = "Format"
command = "cargo fmt --all -- --check"

[[commit.checks]]
name = "Clippy"
command = "cargo clippy --all-targets -- -D warnings"

[[commit.checks]]
name = "Tests"
command = "cargo test"
```

```toml
# JavaScript / TypeScript
[[commit.checks]]
name = "Lint"
command = "npm run lint"

[[commit.checks]]
name = "Unit tests"
command = "npm test"
```

```toml
# Python
[[commit.checks]]
name = "pytest"
command = "python -m pytest -q"

[[commit.checks]]
name = "Ruff"
command = "ruff check ."
enabled = false        # kept in the file, not run
```

```toml
# .NET
[[commit.checks]]
name = "Build"
command = "dotnet build --nologo"

[[commit.checks]]
name = "Tests"
command = "dotnet test --nologo"
```

The commands run in the repository root of whatever repository gitjet has open, and the config
file is shared by all repositories. If a check only makes sense for some repositories, make the
command handle that itself, for example with a script in the repository.

### `[log]`

| Key | Type | Default | Description |
|---|---|---|---|
| `page_size` | integer | `500` | Commits loaded at a time in the Log. The next page loads automatically as you scroll near the end. Values are limited to the range 50 to 10000. |
| `all_branches` | bool | `true` | Show the commits of all branches, tags and remotes (`git log --all`, without the stash) instead of only `HEAD`. Toggle in the Log with `a`; clearing the filters with `F` returns to this setting. |

### `[local_history]`

See [Local history](local-history.md) for how snapshots work.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `true` | Take automatic snapshots of changed files in the background. When `false`, the Local History tab says automatic snapshots are off; manual snapshots still work. |
| `interval_secs` | integer | `60` | Seconds between automatic snapshots. Values below 5 are treated as 5. |
| `keep_days` | integer | `14` | Snapshots older than this many days are pruned. |
| `max_file_kb` | integer | `1024` | Files larger than this (in KiB) are not snapshotted. |

### `[update]`

| Key | Type | Default | Description |
|---|---|---|---|
| `method` | string | `"rebase"` | How **Update Project** and **Pull (in terminal)** integrate the upstream changes: `"rebase"` or `"merge"`. Any value other than `"merge"` means rebase. The normal Pull menu lets you choose each time. See [Branches and remotes](branches-and-remotes.md#how-to-fetch-pull-and-update-the-project). |
| `prune` | bool | `true` | Fetch with `--prune`, which removes remote-tracking branches that no longer exist on the remote. Used by Fetch, Update Project, "Fetch (in terminal)" and the Repository tab's fetch actions. |

### `[hosting]`

Settings for the pull request integration. See [Pull requests](pull-requests.md).

| Key | Type | Default | Description |
|---|---|---|---|
| `use_git_credentials` | bool | `true` | Allow looking up API tokens through Git's credential helper (`git credential fill`). |
| `hosts` | array of tables | empty | Self-hosted servers whose provider cannot be guessed from the host name. |

gitjet never stores hosting tokens in its config file.

#### `[[hosting.hosts]]`

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | required | Host name as it appears in the remote URL, for example `"git.example.com"`. |
| `kind` | string | required | `"github"`, `"gitlab"`, `"bitbucket"`, `"bitbucket-server"`, `"azure"`, `"gitea"` (also for Forgejo) or `"none"` (no pull request integration for this host). |
| `api_url` | string | not set | API base URL, only needed when it is not the provider's default for that host. |

Examples:

```toml
[[hosting.hosts]]
host = "git.example.com"
kind = "gitlab"

[[hosting.hosts]]
host = "code.example.org"
kind = "gitea"

[[hosting.hosts]]
host = "bitbucket.example.net"
kind = "bitbucket-server"
api_url = "https://bitbucket.example.net/rest/api/1.0"

[[hosting.hosts]]
host = "mirror.example.com"
kind = "none"
```

### `[branches]`

| Key | Type | Default | Description |
|---|---|---|---|
| `checkout_from_remote` | bool | `true` | Checking out a branch takes it from the remote as-is. gitjet fetches that one branch, then creates or resets the local branch to it and sets it as the upstream. The remote branch is the local branch's upstream, else the branch of the same name on a remote (`origin` first). When the local branch has commits that are not on the remote, gitjet asks first: reset (the commits stay reachable through the reflog) or check out the local branch as it is. When the fetch fails (offline, authentication), the last fetched state is used and the toast says so. Branches that exist only locally are checked out as usual. Set to `false` to always check out local branches as they are. |

### `[performance]`

| Key | Type | Default | Description |
|---|---|---|---|
| `write_commit_graph` | bool | `true` | When a large repository (20,000+ objects or 300+ refs) has no commit-graph file, write one in the background with `git commit-graph write --reachable`. It is the same cache `git gc` and `git maintenance` write, and it makes the Log, ahead/behind counts and "branches containing" several times faster. Set to `false` if gitjet should never write to `.git/objects`. |

## Per-repository data: `.git/gitjet/`

gitjet keeps the data that belongs to one repository inside that repository's Git directory, in a
folder named `gitjet`. Because it is inside `.git`, it never shows up as a change in your work
tree and is never committed or pushed. For a linked worktree the folder is in the main
repository's Git directory, so all worktrees of a repository share it.

| Path | Contents |
|---|---|
| `changelists.json` | Changelists and which files belong to them (main work tree). |
| `changelists.<worktree>.json` | Changelists of a linked worktree. |
| `changelists.json.bak` | A damaged changelists file is renamed to this (or `.N.bak`) and gitjet starts with fresh changelists. |
| `commit_history.json` | The last 50 commit messages, for the commit dialog's message history. |
| `commit_draft.txt` | The unfinished commit message, kept when you close the commit dialog without committing. |
| `COMMIT_MSG` | Temporary message file passed to `git commit`. Deleted after the commit. |
| `favorites.json` | Favorite branches. |
| `shelf/<id>/` | One folder per shelf entry: `meta.json`, `changes.patch` and copies of untracked files in `untracked/`. |
| `local-history/` | Local history snapshots: `objects/`, `index.jsonl`, `state.json` and a `lock` file. Linked worktrees use `local-history/worktrees/<id>/`. |
| `rebase/` | The todo list and commit messages written for the last interactive rebase. |
| `conflict-compare/` | Temporary copies of the base, yours and theirs versions used to compare conflicting versions. |

Recent branches are not stored by gitjet; they come from Git's reflog.

## Global data next to the config file

| Path | Contents |
|---|---|
| `<config dir>/gitjet/config.toml` | The configuration described on this page. |
| `<config dir>/gitjet/recent.json` | Recently opened repositories, for the repository switcher (up to 20). |

To reset gitjet, see [Troubleshooting](troubleshooting.md#how-do-i-reset-gitjet).
