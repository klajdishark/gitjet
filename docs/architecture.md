[← Back to docs index](README.md)

# Architecture

This page is for contributors. It explains how the code is organized, how a key press turns into
a Git command, and the rules that keep gitjet working the same on Windows, macOS and Linux. For
setup and the pull request checklist, see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Overview

gitjet is one Rust binary built on [ratatui](https://ratatui.rs) (with its crossterm backend).
It has no Git library: every operation runs the installed `git` executable. The code has two
halves:

- `src/git/`: talks to Git. No terminal or UI code.
- `src/ui/`: draws the screen and handles input. It calls into `src/git/` and never builds Git
  command lines itself.

```text
src/
├── main.rs        CLI (clap), hidden editor helpers, startup
├── app.rs         event loop, tabs, popup stack, background tasks, status bar
├── action.rs      Action enum: what input handlers ask the app to do
├── ctx.rs         Ctx: repo + config + theme, passed to every view and popup
├── config.rs      config.toml model and defaults
├── theme.rs       dark and light color themes
├── util.rs        shell, editor and formatting helpers (cross-platform)
├── git/           Git backend
└── ui/            UI framework, widgets, diff viewer, tabs
```

## Module map

### `src/git/`: the Git backend

`git/mod.rs` holds the core type [`Repo`](#how-git-is-run) and the shared modules; every other
module owns one feature area.

| Module | Responsibility |
|---|---|
| `mod.rs` | `Repo` (discover, run git, data dir, repository state), `testutil` |
| `status.rs` | Work tree status (`git status --porcelain=v2 -z`) |
| `diff.rs` | Unified diff model (`FileDiff`) and loaders; every diff in the app is parsed into it |
| `log.rs` | Commit history queries |
| `graph.rs` | Commit graph lane layout for the Log |
| `history.rs` | Checkout revision, cherry-pick, revert, reset, undo commit, branch or tag at a revision, reword `HEAD` |
| `commit.rs` | Creating commits (amend, sign-off, author, signing, message history, commit checks), stage, unstage, rollback |
| `partial.rs` | Staging, unstaging and discarding single lines or hunks by building patches for `git apply` |
| `changelist.rs` | Changelists (named groups of changed files) stored in the data dir |
| `branch.rs` | Branch listing with tracking info, create, checkout, rename, delete, merge, rebase onto, favorites, recents |
| `remote.rs` | Fetch, pull, update project, push, remote branch deletion, commits-to-push preview, "(in terminal)" commands |
| `rebase.rs` | Interactive rebase: todo list, plan validation, continue / abort / skip |
| `conflict.rs`, `conflict/merge.rs` | Conflicted files, base / ours / theirs content, the 3-way chunk model, resolve, continue / abort / skip of the operation in progress |
| `stash.rs` | Stash list, show, save, apply, pop, drop, clear, branch |
| `shelf.rs` | The shelf (named saved patches) stored in the data dir |
| `patch.rs` | Create patch files from changes or commits, apply patches (`git apply` / `git am`) |
| `blame.rs` | Annotate (`git blame --porcelain`) |
| `file_history.rs` | File history (`git log --follow`), line history (`git log -L`), file content at a revision |
| `compare.rs` | Compare with branch or revision, compare two files |
| `local_history.rs` | Local history snapshots, independent of commits |
| `admin.rs` | Tags, remotes, worktrees, submodules, ignore rules, config, nested and recent repositories |
| `hosting.rs` | Code hosting provider integration used by the Pull Requests tab |

### `src/ui/`: the UI

| Path | Responsibility |
|---|---|
| `ui/mod.rs` | The `View` and `Popup` traits, `KeyHelp`, `PaletteItem` |
| `ui/popups.rs` | Stock dialogs: `ConfirmPopup`, `InputPopup`, `TextAreaPopup`, `MenuPopup` (context menus and the palette), `MessagePopup` |
| `ui/widgets.rs` | `TextInput`, `TextArea`, `panel`, `centered`, selection and scrolling helpers |
| `ui/diff_view.rs`, `ui/diff_view/` | The diff viewer: `DiffSource`, `open` (fullscreen viewer), `draw_diff` (inline previews), comparison dialogs. `model.rs` is pure layout, `render.rs` paints, `highlight.rs` does syntax colors, `viewer.rs` is the popup |
| `ui/views/mod.rs` | The `Tab` enum, `build` (one view per tab, in order) and `palette` (every feature's palette entries) |
| `ui/views/<feature>.rs` | One tab each: `changes`, `log`, `branches`, `shelf`, `files`, `conflicts`, `prs`, `local_history`, `repository` |
| `ui/views/push.rs`, `ui/views/rebase.rs` | Features without a tab of their own: the push dialog and the interactive rebase editor |

A feature module exposes a few `pub fn` entry points that other features use, for example
`files::open_blame`, `files::open_file_history`, `local_history::open_for_path`,
`conflicts::open_resolver`, `shelf::shelve_paths`, `push::open` and `rebase::open`. Use these
instead of reaching into another feature's internals.

## The UI framework

### `View` and `Popup`

Every tab implements `View`:

```rust
pub trait View {
    fn title(&self) -> String;
    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()>;
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx);
    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action;
    fn handle_mouse(&mut self, ev: MouseEvent, ctx: &Ctx) -> Action { Action::None }
    fn help(&self) -> Vec<KeyHelp> { Vec::new() }
    fn captures_input(&self) -> bool { false }
    fn badge(&self) -> Option<String> { None }
    fn poll(&mut self, ctx: &Ctx) {}
    fn loading(&self) -> bool { false }
}
```

- `refresh` reloads from Git. It runs when the tab is first shown and after every
  `Action::Refresh`; hidden tabs are only marked dirty and refresh when they become visible.
  It must not wait on git: it starts a `ui::bg::Background` job and returns, and `poll` applies
  the result when it arrives. Starting a `git` process costs 50–150 ms on Windows, so a git call
  on the UI thread freezes the screen there.
- `poll` runs about ten times a second for every tab, visible or not, to pick up background
  results. `loading` returns `true` while a refresh is running; the status bar then shows a
  spinner ("Loading Changes").
- Panes that follow the selection (diff previews, branch details) load through `ui::bg::Preview`,
  which waits until the selection rests and caches results, so holding an arrow key does not start
  a git command per row.
- `help` returns `(key, description)` pairs. The first eight appear in the hint line; all of them
  appear in the `?` overlay.
- `captures_input` returns `true` while a text field in the view has focus, so the app does not
  treat `q`, `?` or digits as global keys.
- `badge` is shown in the tab title, for example the number of conflicts.

Everything drawn on top of the tabs (dialogs, menus, the diff viewer, the merge resolver, the
rebase editor) is a `Popup`. Popups form a stack; the top one receives all keys and mouse events.
A popup closes itself by returning `Action::ClosePopup`. Besides `draw`, `handle_key`,
`handle_mouse` and `help`, a popup can implement `refresh` (called after `Action::Refresh`) and
`poll` (called about ten times a second, for popups that wait on background work).

### `Action`

Input handlers never change app state directly. They return an `Action`, and `App::apply` carries
it out:

| Variant | Effect |
|---|---|
| `None` | Nothing |
| `Refresh` | Reload every view (lazily for hidden tabs) and every open popup |
| `SwitchTab(Tab)` | Show another tab |
| `Popup(Box<dyn Popup>)` | Push a popup (`Action::popup(p)` is a shorthand) |
| `ClosePopup` | Pop the top popup |
| `Toast(String)` | Short message in the status bar |
| `Error(String)` | Error toast, or a scrollable error dialog when the text is long or has several lines |
| `Spawn(Task)` | Run a job on a background thread with a spinner |
| `Suspend(Command)` | Leave the TUI, run an interactive command (editor, Git with prompts), come back and refresh |
| `SwitchRepo(PathBuf)` | Open another repository: rebuild all views, close popups |
| `Batch(Vec<Action>)` | Several actions in order |

Helpers:

- `Action::spawn(label, job)` creates a background task. `job` is `FnOnce(&Repo) -> Result<String>`
  and runs on its own thread with a clone of the `Repo`.
- `Action::spawn_then(label, job, then)` adds a follow-up that runs on the UI thread with the
  job's result and returns another `Action` (switch tab, open a popup, open another repository).
- `Action::from_result(result, ok_msg)` turns a `Result` into a toast or an error, plus a refresh.
- `action.then(next)` chains actions.

Popups use the same pattern: `ConfirmPopup::new(title, message, |ctx| ...)` and
`InputPopup::new(title, initial, |text, ctx| ...)` take callbacks that return the `Action` to apply
after the popup closes.

### Palette and context menus

`PaletteItem { name, run: fn(&Ctx) -> Action }` is one entry of the `:` action palette. Each
feature module has a `pub fn palette() -> Vec<PaletteItem>`, and `ui/views/mod.rs` concatenates
them. The app adds "Go to tab: …" entries.

Context menus are `MenuPopup`s of `MenuItem::new(label, key, run)`. A one-character `key` is the
accelerator. `.enabled(false)` shows an item dimmed. Several features build their direct key
bindings and their context menu from the same function, so a key and its menu entry cannot drift
apart (see `ui/views/repository/actions.rs`).

### Talking back to a view

Popup callbacks and palette entries only receive `&Ctx`, not the view. When they need to change a
view's state, the feature uses a small thread-local queue that the view drains on its next draw or
key press, for example `ui/views/log/mailbox.rs`. The UI is single-threaded, so this needs no
locking.

## Event loop and background tasks

`app::run` sets up the terminal (alternate screen, raw mode, mouse capture when `mouse = true`,
focus change events) and runs `App::event_loop`:

1. Draw: tab bar, the active view, status bar, hint line, then every popup from bottom to top.
2. Wait up to 100 ms for input, then drain all queued events before the next draw. Keys go to the
   top popup if there is one; otherwise global keys are checked (unless the view captures input),
   then the view gets the key. A paste is replayed as key presses. Focus gained requests a refresh
   (unless one ran in the last second).
3. `poll_tasks`: collect finished background tasks. A non-silent task shows its message (or
   "<label> finished") or its error, requests a refresh, then runs its `then` follow-up.
4. If a refresh was requested (`Action::Refresh`, any number of times since the last pass), run it
   once: mark every tab dirty and refresh the visible tab, the Conflicts tab and every popup.
5. `tick`: expire toasts, start the silent local history snapshot task when it is due, call `poll`
   on every tab and popup, and check the Conflicts badge (below).
6. If an `Action::Suspend` is pending: restore the terminal, run the command in the repository
   root and wait for it, set the terminal up again, report a non-zero exit, refresh.

Background tasks are plain `std::thread`s that send a `TaskDone` back over an `mpsc` channel. They
receive a clone of `Repo`, never the views. Silent tasks (local history snapshots) show no spinner
and trigger no refresh.

Whenever the Conflicts tab is not loading, the app checks its badge. When conflicts appear that
were not there before, it switches to the Conflicts tab (conflicts already present at startup set
the baseline and don't switch tabs).

## How Git is run

All Git calls go through `Repo` in `src/git/mod.rs`:

| Method | Use |
|---|---|
| `Repo::discover(path)` | Find the work tree root, the Git dir and the common Git dir with `git rev-parse --path-format=absolute` |
| `repo.command()` | A preconfigured `git` `Command` (see below) |
| `repo.run(args)` | Run and return stdout; a non-zero exit becomes an error that contains stderr |
| `repo.run_bytes(args)` | Raw stdout bytes (file contents) |
| `repo.run_allow(args, codes)` | Treat other exit codes as success (for example `diff --no-index` exits 1 on differences) |
| `repo.run_stdin(args, input)` | Feed stdin (patches for `git apply`, pathspecs); written from a separate thread so large inputs cannot deadlock |
| `repo.run_cmd(cmd, args)` | Run a prepared command with extra environment |
| `repo.try_run(args)` | Stdout on success, `None` on failure |
| `repo.data_dir()` | `<common git dir>/gitjet`, created on demand; shared by all worktrees |

`repo.command()` always:

- runs `git -C <root>` with `-c core.quotepath=false -c color.ui=false`, so paths come back as
  UTF-8 and output has no color codes;
- sets `GIT_TERMINAL_PROMPT=0`, so Git fails instead of asking for credentials;
- sets `GIT_EDITOR` and `GIT_SEQUENCE_EDITOR` to gitjet's no-op editor, so Git accepts the
  message it proposes instead of opening an editor;
- sets `GIT_MERGE_AUTOEDIT=no` and `GIT_PAGER=cat`;
- connects stdin to null.

Network operations add `GIT_SSH_COMMAND` with `-o BatchMode=yes`, so SSH fails instead of asking
for a passphrase or host key confirmation (`remote::net_command`). Authentication errors are
rewritten by `remote::auth_hint` to point at the "(in terminal)" actions, which use
`remote::terminal_command`: a plain `git` command with the user's environment, run through
`Action::Suspend` so Git owns the terminal.

### The editor helpers

When Git needs an editor, gitjet uses its own executable:

- `gitjet __noop-editor <file>` exits immediately, which accepts the file Git prepared.
- `gitjet __write-file <src> <dst>` copies `src` over `dst`, the file Git opened.

`main.rs` handles both arguments before clap parses the command line, so Git can append whatever
arguments it wants. `util::noop_editor()` and `util::write_file_editor(src)` build the command
strings, quoted for the `sh` that Git uses to run editors on every platform, including Git for
Windows.

Interactive rebase uses both: gitjet writes the todo list and the reworded messages to
`.git/gitjet/rebase/`, then runs `git rebase -i --autostash <base>` with
`GIT_SEQUENCE_EDITOR="gitjet __write-file <todo>"`, which replaces Git's todo with gitjet's, and
`GIT_EDITOR` set to the no-op editor. Tests inject other commands through `rebase::Editors`,
because the test binary is not gitjet.

### Interactive commands

Anything that needs the real terminal (the user's editor, Git with prompts) goes through
`Action::Suspend(Command)`. `util::editor_command` builds the editor command from the `editor`
setting, `$VISUAL` or `$EDITOR`, with a platform default.

## Testing

Tests live next to the code in `#[cfg(test)] mod tests` blocks (larger suites in their own file,
such as `ui/views/branches/tests.rs`). They run against real Git in temporary repositories:

```rust
use crate::git::testutil::*;

#[test]
fn commits_show_up() {
    let (_dir, repo) = temp_repo();                 // fresh repo on `main`
    commit_files(&repo, &[("a.txt", "hi\n")], "first");
    write(&repo, "a.txt", "changed\n");             // work tree change
    // ... call the function under test with &repo
}
```

`temp_repo()` creates a repository on branch `main` with a fixed user name and email, commit and
tag signing off, and `core.autocrlf=false`, so tests behave the same on every machine. The
`TempDir` it returns deletes the repository when dropped; keep it alive for the whole test.

UI tests render views and popups into ratatui's `TestBackend` and assert on the text of the
screen buffer, and send `KeyEvent`s to `handle_key` to check the resulting state or `Action`.
Pure logic (diff layout, tree building, the merge model, graph layout) is kept in modules without
terminal or Git access so it can be tested directly.

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` on
Windows, macOS and Linux.

## Cross-platform rules

- **Run Git, not a library.** Use `Repo` methods. Never call `Command::new("git")` directly,
  except for the few places that must not use `repo.command()` (CLI `clone`/`init` in `main.rs`,
  `Repo::discover`, and `remote::terminal_command` for interactive Git).
- **Never let background Git prompt.** Background commands come from `repo.command()` (and
  `remote::net_command` for network). Anything that may need input gets an "(in terminal)" variant
  through `Action::Suspend`.
- **Shell commands** (commit checks, editors) go through `util::shell`, which uses `cmd /C` on
  Windows and `sh -c` elsewhere.
- **Quote for Git's shell.** Values Git passes to `sh` (editor commands) use `util::sh_quote`,
  which also converts backslashes to forward slashes for Git for Windows.
- **Paths.** Git reports repository-relative paths with `/`. Keep them that way inside gitjet and
  join them to the root with `repo.path(rel)` only when touching the file system.
- **Line endings.** Do not normalize file content you read from Git or the work tree. Partial
  staging keeps the `\r` of CRLF lines so patches apply cleanly.
- **Text width.** Use `unicode-width` (see `util::truncate` and `util::fit`) for anything that is
  laid out in columns; never count bytes.
- **Colors** come from `Theme` (`ctx.theme`), so both the dark and the light theme work. Do not
  hard-code colors in views.
- **No blocking the UI.** Slow Git work (network, large logs, diffs, blame) runs in a background
  task or a loader thread; the UI thread only draws and handles input.
- **Storage.** Per-repository files go in `repo.data_dir()`; global files go next to the config
  file in `Config::dir()`. Never write into the work tree.
