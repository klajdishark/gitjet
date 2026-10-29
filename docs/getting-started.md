[← Back to docs index](README.md)

# Getting started

This page covers installing gitjet, opening a repository, finding your way around the screen and
the command-line options.

## Requirements

- **Git** must be installed and on your `PATH`. gitjet runs your `git` executable for every
  operation, so your hooks, credential helpers, signing setup, `.gitattributes` and Git config work
  the same as they do on the command line. gitjet finds repositories with
  `git rev-parse --path-format=absolute`, which needs Git 2.31 or newer.
- A terminal. Any modern terminal on Windows, macOS or Linux works. See
  [Troubleshooting](troubleshooting.md#terminal-and-display) for terminal and font tips.

## Install

gitjet is a single executable with no runtime dependencies besides Git.

### Windows

In PowerShell:

```powershell
irm https://github.com/kashari/gitjet/releases/latest/download/install.ps1 | iex
```

The script downloads `gitjet.exe` for your architecture (x64 or ARM64), checks its SHA-256
checksum, copies it to `%LOCALAPPDATA%\Programs\gitjet` and adds that folder to your user `PATH`.
It needs no administrator rights. Open a new terminal afterwards so the new `PATH` takes effect.

### macOS and Linux

```bash
curl -fsSL https://github.com/kashari/gitjet/releases/latest/download/install.sh | sh
```

The script picks the archive for your OS and CPU (x86_64 or arm64/aarch64), checks its SHA-256
checksum, and installs the binary to `~/.local/bin/gitjet`. If that directory is not on your
`PATH`, the script prints the line to add to your shell profile. It uses `curl`, or `wget` when
`curl` is not available.

### Install script options

Both scripts read these environment variables:

| Variable | Meaning | Default |
|---|---|---|
| `GITJET_INSTALL_DIR` | Where to put the binary | `~/.local/bin` (macOS/Linux), `%LOCALAPPDATA%\Programs\gitjet` (Windows) |
| `GITJET_VERSION` | Release tag to install, for example `v0.1.0` | latest release |

Example:

```bash
curl -fsSL https://github.com/kashari/gitjet/releases/latest/download/install.sh | GITJET_INSTALL_DIR=/usr/local/bin sh
```

```powershell
$env:GITJET_VERSION = "v0.1.0"; irm https://github.com/kashari/gitjet/releases/latest/download/install.ps1 | iex
```

### Manual download

Download the archive for your platform from the
[releases page](https://github.com/kashari/gitjet/releases), unpack it, and put `gitjet`
(`gitjet.exe` on Windows) somewhere on your `PATH`. Each archive has a matching `.sha256` file.

| Platform | Archive |
|---|---|
| Windows x64 | `gitjet-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `gitjet-aarch64-pc-windows-msvc.zip` |
| macOS Apple Silicon | `gitjet-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `gitjet-x86_64-apple-darwin.tar.gz` |
| Linux x64 (static) | `gitjet-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 (static) | `gitjet-aarch64-unknown-linux-musl.tar.gz` |

The binaries are not code-signed yet. See
[Troubleshooting](troubleshooting.md#macos-says-the-app-cannot-be-opened-or-windows-shows-unknown-publisher)
if macOS or Windows blocks them.

### From source

With Rust 1.80 or newer:

```bash
cargo install --git https://github.com/kashari/gitjet
```

### Check the installation

```bash
gitjet --version
```

## First launch

Open a terminal in any folder inside a Git work tree and run:

```bash
gitjet
```

gitjet opens the repository that contains the current directory, even from a subfolder. If the
folder is not inside a Git work tree, gitjet prints an error and exits. To open another repository
without changing directory, use `gitjet -C path/to/repo`.

gitjet starts on the **Changes** tab. If the repository already has merge conflicts, the
**Conflicts** tab shows a badge with their number.

A config file is optional. Without one, gitjet uses the defaults listed in
[Configuration](configuration.md). If the config file exists but cannot be parsed, gitjet starts
with the defaults and shows the parse error.

## The screen

```text
 gitjet   1 Changes (3)  2 Log  3 Branches  4 Stash & Shelf  5 Files  6 Conflicts ...   <- tab bar
┌ Local Changes · 3 files ───┐┌ Local (HEAD) · src/main.rs ───────────────────────┐
│                            ││                                                   │  <- panes
│                            ││                                                   │
└────────────────────────────┘└───────────────────────────────────────────────────┘
 myrepo  main  ↑1 ↓0                                               ⠙ Fetch         <- status bar
 Space stage/unstage  c commit  Enter diff/expand ...  : actions  ? help  q quit     <- hint line
```

From top to bottom:

| Part | What it shows |
|---|---|
| Tab bar | The nine tabs with their number keys. A tab can carry a badge in parentheses, for example the number of changed files or conflicts. The Conflicts tab is highlighted while there are conflicts. |
| Panes | The active tab. Most tabs have a list on the left and a preview or details pane on the right. The focused pane has a highlighted border. |
| Status bar | Repository name, current branch (or `detached at <hash>`), commits ahead (`↑`) and behind (`↓`) the upstream, the repository state when an operation is in progress (`MERGING`, `REBASING`, `CHERRY-PICKING`, `REVERTING`, `APPLYING PATCHES`, `BISECTING`), and the latest message. On the right, a spinner lists the background tasks that are running. |
| Hint line | The most important keys of the current tab or the open dialog. |

Messages in the status bar disappear after 4 seconds, errors after 10 seconds. Long or multi-line
errors open in a scrollable dialog instead.

The tabs are:

| Key | Tab | Page |
|---|---|---|
| `1` | Changes | [Changes and commit](changes-and-commit.md) |
| `2` | Log | [Log](log.md) |
| `3` | Branches | [Branches and remotes](branches-and-remotes.md) |
| `4` | Stash & Shelf | [Stash, shelf and patches](stash-shelf-patches.md) |
| `5` | Files | [Files, blame and history](files-blame-history.md) |
| `6` | Conflicts | [Conflicts and merge resolver](conflicts.md) |
| `7` | Pull Requests | [Pull requests](pull-requests.md) |
| `8` | Local History | [Local history](local-history.md) |
| `9` | Repository | [Repository](repository.md) |

The fullscreen [diff viewer](diff-viewer.md), the [interactive rebase editor](interactive-rebase.md),
the commit dialog, the push dialog and the merge resolver open on top of the tabs.

## How gitjet is operated

gitjet is built for the keyboard. You only need a handful of keys to discover everything else.

| Key | What it does |
|---|---|
| `1` to `9` | Switch to a tab. |
| `[` / `]` | Previous / next tab. |
| `:` or `` Alt+` `` | Open the **action palette**: a searchable list of every action in gitjet. Type a few letters, pick an entry with the arrow keys, press `Enter`. The palette also has "Go to tab" entries. |
| `.` or `m` | Open the **context menu** for the selected item. It lists every action that applies, with its shortcut key on the right. Press the shortcut or move to an entry and press `Enter`. |
| `?` | Show all keys of the current tab, plus the global keys and the path of the config file. |
| `Tab` | Move focus to the next pane (for example from a file list to its diff). |
| `/` | Filter the list in the current tab. Type to narrow it down, `Esc` clears the filter. |
| `Enter` | Open the selected item (a diff, a dialog, a folder). |
| `Esc` | Go back: clear a filter or marks, close a dialog or fullscreen view. |
| `j` / `k` or `↑` / `↓` | Move the selection. |
| `F5` | Reload everything from Git. |
| `q` | Quit (on a tab; in many dialogs `q` closes the dialog). |
| `Ctrl+C` | Quit immediately, from anywhere. |

While you type into a text field inside a tab (for example a filter), the single-letter global keys
such as `q`, `?` and the number keys go into the field instead.

When a dialog or fullscreen view is open, it receives all keys. The hint line at the bottom shows
its keys.

### Action palette

The palette matches what you type against action names. Names that start with your text come
first, then names with a word starting with it, then names that contain it, then names that
contain all the words you typed. Loose matches (letters in order) show only when nothing else
matches. For example, `pu` finds "Push…" and "Pull…", and `branch new` finds "New branch…".

![Action palette](screenshots/actions.png)

### Context menus

In a context menu, `↑` / `↓` (or `j` / `k`) move, `Enter` runs the entry, and a single-character
shortcut shown on the right runs that entry directly. Entries that do not apply right now are shown
dimmed and do nothing. `Esc` or `q` closes the menu.

### Dialogs

| Dialog | Keys |
|---|---|
| Yes/no question | `y` or `Enter` confirms, `n`, `Esc` or `q` cancels. Dangerous operations (hard reset, delete, force push) have a red border. |
| Single-line input | `Enter` accepts, `Esc` cancels. |
| Multi-line text (for example a commit message) | `Ctrl+S` or `Alt+Enter` saves, `Esc` cancels. `Enter` starts a new line. |
| Message or error | `↑` / `↓` or `j` / `k` scroll, `PgUp` / `PgDn` or `Space` scroll a page, `Esc`, `Enter` or `q` close. |

Text fields support `←` / `→`, `Home` / `End`, `Ctrl+A` / `Ctrl+E` (start / end), `Ctrl+U`
(delete to the start), `Ctrl+W` or `Alt+Backspace` (delete the previous word), `Backspace` and
`Delete`. Pasted text is inserted as if typed.

### Mouse

Mouse support is on by default (`mouse = true` in the [config](configuration.md)).

- Click a tab title in the tab bar to switch to it.
- The scroll wheel scrolls the list or view under focus. Where a view has no special scroll
  handling, the wheel acts like `↑` / `↓`.
- Many views also react to clicks, for example selecting a row or focusing a pane. The pages for
  each tab list the details.

When mouse support is on, your terminal's own text selection may need a modifier key. Set
`mouse = false` to turn it off.

### Refreshing

gitjet reloads its data after every operation, when you press `F5`, and when the terminal window
regains focus (in terminals that report focus changes). Tabs that are not visible reload when you
switch to them. The Conflicts tab is always kept current; when a refresh finds new conflicts,
gitjet switches to it and shows "Conflicts need resolving".

### Background tasks

Network operations such as fetch, pull and push run in the background. The status bar shows a
spinner and the task name while they run, and a message when they finish. You can keep working in
the meantime.

Background Git never waits for a password or SSH passphrase. If an operation needs one, use the
"(in terminal)" variant of the action, which hands the terminal to Git so you can type it. See
[Branches and remotes](branches-and-remotes.md#credential-and-ssh-prompts-the-in-terminal-actions).

## Command line

```text
gitjet [OPTIONS] [COMMAND]
```

| Command or option | What it does |
|---|---|
| `gitjet` | Open the repository that contains the current directory. |
| `gitjet -C <DIR>`, `gitjet --path <DIR>` | Open the repository that contains `<DIR>`. |
| `gitjet clone <URL> [DIR]` | Run `git clone --progress` in your terminal, then open the new repository. Without `DIR`, the folder is named after the repository in the URL (for example `https://example.com/a/b.git` clones into `b`). Because the clone runs in your terminal, password and passphrase prompts work. |
| `gitjet init [DIR]` | Create `DIR` (default: the current directory) if needed, run `git init` in it, then open it. |
| `gitjet config-path` | Print the path of the config file. The file does not have to exist. |
| `gitjet config-init` | Write a config file with every setting at its default value, then print its path. An existing file is left unchanged. |
| `gitjet --version`, `gitjet -V` | Print the version. |
| `gitjet --help`, `gitjet -h` | Print help. `gitjet help <command>` prints help for one command. |

gitjet also accepts two hidden arguments, `__noop-editor` and `__write-file`. Git calls them when
gitjet sets itself as Git's editor (see [Architecture](architecture.md#how-git-is-run)). You do not
need to use them.

## Next steps

- [Changes and commit](changes-and-commit.md): stage, commit and work with changelists.
- [Keyboard reference](keyboard-reference.md): every key on one page.
- [Feature overview](feature-overview.md): common Git tasks and where to find them.
- [Configuration](configuration.md): themes, diff defaults, commit checks.
