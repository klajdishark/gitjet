[← Back to docs index](README.md)

# Troubleshooting

Answers to common problems. If your problem is not here, please
[open an issue](https://github.com/klajdishark/gitjet/issues) with your OS, terminal,
`gitjet --version` and `git --version`.

- [Git and repositories](#git-and-repositories)
- [Passwords, tokens and SSH](#passwords-tokens-and-ssh)
- [Terminal and display](#terminal-and-display)
- [Installation](#installation)
- [Line endings](#line-endings)
- [Large repositories](#large-repositories)
- [Editor, clipboard and mouse](#editor-clipboard-and-mouse)
- [Data, config and reset](#data-config-and-reset)

## Git and repositories

### "could not run git — is Git installed and on your PATH?"

gitjet runs the `git` executable for everything and does not include its own copy. Install Git
from [git-scm.com](https://git-scm.com) (on Windows, Git for Windows), then open a new terminal
and check:

```bash
git --version
```

If that works in your shell but gitjet still cannot find Git, the `PATH` that gitjet sees is
different, for example because gitjet was started from a different shell or a shortcut. Start
gitjet from the same terminal where `git --version` works.

### Which Git version do I need?

gitjet locates repositories with `git rev-parse --path-format=absolute`, which needs Git 2.31 or
newer. With an older Git, gitjet cannot open any repository. Update Git.

### "... is not inside a Git work tree"

gitjet opens the repository that contains the current directory (or the directory given with
`-C`). Run it from inside a repository, pass `-C path/to/repo`, or create one with
`gitjet init`. Bare repositories have no work tree and cannot be opened.

### The repository state shows MERGING, REBASING or similar

An operation stopped half way, for example on conflicts. gitjet shows the state in the status bar
until it is finished. Go to the Conflicts tab (`6`) to resolve the files, then continue (`C`) or
abort (`A`) the operation. See [Conflicts](conflicts.md).

## Passwords, tokens and SSH

### Fetch, pull or push fails with an authentication or SSH error

Background Git in gitjet never waits for input: it runs with `GIT_TERMINAL_PROMPT=0`, and SSH runs
in batch mode. If Git needs a username, password, token, SSH passphrase or a host key
confirmation, the operation fails instead of hanging. gitjet then adds a hint to the error.

Use the "(in terminal)" version of the action. It hands the terminal to Git, so you can type the
password or passphrase, and returns to gitjet when Git finishes:

- **Fetch (in terminal)**
- **Pull (in terminal)**
- **Push (in terminal)**

Find them in the action palette (`:`) or the Branches context menu (`.`). See
[Branches and remotes](branches-and-remotes.md#credential-and-ssh-prompts-the-in-terminal-actions).

To avoid prompts altogether, let Git remember your credentials. gitjet uses whatever Git uses.

### Set up a credential helper (HTTPS)

| OS | Usual setup |
|---|---|
| Windows | Git for Windows includes Git Credential Manager and normally configures it during installation. Sign in once from a terminal (for example with **Push (in terminal)**). |
| macOS | Git on macOS can store credentials in the keychain: `git config --global credential.helper osxkeychain`. |
| Linux | Use Git Credential Manager, the libsecret helper if your distribution packages it, or `git config --global credential.helper cache` to keep credentials in memory for a while. |

After the first successful sign-in in a terminal, background fetch and push work without prompts.

### SSH keys with a passphrase

Load the key into an SSH agent before starting gitjet, so SSH does not need to ask:

```bash
ssh-add ~/.ssh/id_ed25519
```

On Windows, the OpenSSH Authentication Agent service has to be running for `ssh-add` to work. On
macOS, `ssh-add --apple-use-keychain` stores the passphrase in the keychain.

### "Host key verification failed"

The first connection to a new SSH host asks you to confirm its key, which background Git cannot
do. Run **Fetch (in terminal)** once and answer the prompt, or connect once with `ssh` from your
shell.

### Cloning a private repository

**Clone repository…** inside gitjet runs in the background and cannot ask for credentials. Clone
from the command line instead, which runs `git clone` in your terminal:

```bash
gitjet clone https://example.com/team/project.git
```

### Does gitjet store my tokens?

No. gitjet never writes hosting tokens to its config file. See [Security](../SECURITY.md) and
[Pull requests](pull-requests.md).

## Terminal and display

### Which terminal should I use on Windows?

Use [Windows Terminal](https://aka.ms/terminal). It supports UTF-8, 24-bit color, mouse input and
focus events. The old console window (conhost) may draw lines and symbols incorrectly.

If you must use an older console, switch it to UTF-8 first:

```powershell
chcp 65001
```

### Lines, arrows or the spinner show as boxes or question marks

gitjet draws borders with box-drawing characters and uses symbols such as `↑ ↓ ← → ● ◐ ○ ▪` and a
Braille-pattern spinner. Pick a terminal font that contains these characters, and make sure the
terminal uses UTF-8. Most modern programming fonts cover box drawing and arrows; if only the
spinner looks wrong, the font lacks Braille patterns.

### Colors look wrong or washed out

gitjet uses 24-bit ("true color") RGB colors. Terminals without true color support show
approximate colors.

- Use a terminal with true color support, for example Windows Terminal, iTerm2, WezTerm, kitty,
  Alacritty or GNOME Terminal.
- In tmux, enable RGB for your terminal, for example `set -as terminal-features ",xterm-256color:RGB"`
  in `~/.tmux.conf` (tmux 3.2 or newer), and restart tmux.
- If your terminal has a light background, set `theme = "light"` in the
  [config](configuration.md).

### A key does nothing

- Some terminals and window managers use function keys or `Alt` combinations themselves. On macOS
  you may need to hold `fn` for `F4`, `F5` or `F7`.
- `` Alt+` `` may not reach the terminal on every keyboard layout. Use `:` to open the palette.
- Keys are case-sensitive: `D` (delete) is not `d`.
- When a text field has focus (a filter or input), letters go into the field. Press `Esc` or
  `Enter` to leave it.
- Press `?` to see the keys of the current tab, or check the hint line for the open dialog.

## Installation

### macOS says the app cannot be opened, or Windows shows "Unknown publisher"

The release binaries are not code-signed yet.

On **macOS**, a binary downloaded with a browser gets a quarantine flag, and Gatekeeper may block
it. Either allow it in *System Settings → Privacy & Security* ("Open Anyway"), or remove the flag:

```bash
xattr -d com.apple.quarantine /path/to/gitjet
```

On **Windows**, SmartScreen may show "Windows protected your PC" for a downloaded `gitjet.exe`.
Choose *More info → Run anyway*, or unblock the file in PowerShell:

```powershell
Unblock-File "$env:LOCALAPPDATA\Programs\gitjet\gitjet.exe"
```

The install scripts download with `curl` or PowerShell, which normally avoids both prompts.

### `gitjet` is not found after installing

- **macOS / Linux:** the script installs to `~/.local/bin` by default and prints a line to add it
  to your `PATH` if it is missing. Add that line to your shell profile and open a new terminal.
- **Windows:** the script adds `%LOCALAPPDATA%\Programs\gitjet` to your user `PATH`. Open a new
  terminal window so the change takes effect.

## Line endings

### Files show as modified, but the diff shows no changes

This is usually a line ending difference (CRLF versus LF). gitjet uses your Git settings, so
`core.autocrlf` and `.gitattributes` decide how line endings are handled, exactly as with `git` on
the command line.

- Check `core.autocrlf` in the Repository tab (`9`) > Config. On Windows, `true` is the common
  setting; on macOS and Linux, `input` or not set.
- For a whole team, a `.gitattributes` file with `* text=auto` is more reliable than per-user
  settings.
- In the diff viewer, `W` ignores whitespace changes, which also hides changes that only affect
  line endings within a line.

## Large repositories

gitjet loads tabs, previews and details off the screen thread, so keys and tab switches stay
instant even when git is slow; a spinner in the status bar ("Loading Changes") and "Loading…" in
the pane show that data is on its way. If loading itself takes long:

- **Windows is slower at starting programs**: each git command costs 50–150 ms there, more with
  real-time antivirus scanning. Excluding your repositories and Git's install folder from
  scanning (if your security policy allows it) helps a lot.
- **git through a shim** (Scoop, Chocolatey and similar): every git command then starts two
  programs. Point gitjet at the real executable with the `GITJET_GIT` environment variable, for
  example `GITJET_GIT=C:\Program Files\Git\cmd\git.exe`.
- **Finding what is slow**: set `GITJET_TRACE` to a file name and gitjet appends every git
  command it runs, with its duration and whether the screen waited for it (`[UI]`) or not
  (`[bg]`):

  ```text
     133.2 ms [bg] git --no-optional-locks status --porcelain=v2 -z --branch --untracked-files=all
     141.8 ms [UI] git --literal-pathspecs add -A --pathspec-from-file=- --pathspec-file-nul
  ```

  Please attach such a trace when reporting a performance problem.
- **Many branches or a long history**: gitjet writes git's commit-graph cache in the background
  for large repositories (see `[performance]` in [Configuration](configuration.md)). You can also
  let Git keep it up to date itself:

  ```bash
  git maintenance start
  ```

- **Many files** (slow `git status`): turn on Git's built-in file system monitor (Windows and macOS, Git 2.37+) and untracked cache in
  that repository:

  ```bash
  git config core.fsmonitor true
  git config core.untrackedCache true
  ```

- The Log loads commits in pages (`log.page_size`, default 500) and loads more as you scroll. Use
  filters (`f`), or `a` to show only `HEAD` instead of all branches, to reduce what is loaded.
  Set `all_branches = false` under `[log]` to make that the default.
- Network operations run in the background; you can keep working while they run.
- Local history only looks at changed files and files Git already tracks, at most 5000 per
  snapshot, and skips files larger than `local_history.max_file_kb`. Increase
  `local_history.interval_secs` to take snapshots less often, or set `local_history.enabled = false`
  to turn automatic snapshots off.
- Old local history snapshots are pruned after `local_history.keep_days` days. **Prune old local
  history** in the palette prunes now.

## Editor, clipboard and mouse

### The wrong editor opens, or it returns immediately

gitjet uses the `editor` setting, then `$VISUAL`, then `$EDITOR`, then `notepad` on Windows or
`vi` elsewhere. gitjet waits until the editor command exits. Graphical editors usually need a
"wait" flag to keep the command running until you close the file, for example:

```toml
editor = "your-editor --wait"
```

gitjet passes the line number (`+N`) only to `vi`, `vim`, `nvim`, `nano`, `emacs`, `hx`, `micro`
and `kak`.

### "Copied" appears, but nothing is in my clipboard

gitjet copies hashes and paths with the OSC 52 terminal escape sequence, which also works over
SSH. It does not call external clipboard tools. If your terminal does not support OSC 52, or has
it switched off, nothing is copied. Terminals such as Windows Terminal, iTerm2, kitty, WezTerm,
Alacritty and foot support it (some need it enabled in their settings). In tmux, add
`set -g set-clipboard on` to `~/.tmux.conf`.

### I cannot select text with the mouse

While mouse support is on, the terminal sends mouse events to gitjet. Many terminals let you
select text anyway while holding a modifier key (often `Shift`, or `Option` on macOS). To give
the mouse back to the terminal completely, set `mouse = false` in the config.

## Data, config and reset

### Where are the config and data?

| What | Where |
|---|---|
| Config file | `gitjet config-path` prints it. See [Configuration](configuration.md#where-the-config-file-lives). |
| Recent repositories | `recent.json` in the same folder as the config file |
| Changelists, shelf, local history, favorites, commit message history | `.git/gitjet/` in each repository. See [Configuration](configuration.md#per-repository-data-gitgitjet). |

### gitjet shows "Config error (using defaults)"

The config file exists but is not valid TOML, or a value has the wrong type. The message names the
file and the problem. Fix the file and restart gitjet. To start over, move the file away and run
`gitjet config-init`.

### How do I reset gitjet?

- **Settings:** delete or rename the config file (`gitjet config-path` shows where it is).
- **Recent repositories:** delete `recent.json` next to the config file.
- **One repository:** delete the folder `.git/gitjet/` in that repository while gitjet is not
  running. This removes changelists, favorites, the commit message history and draft, local
  history **and the shelf**. Export shelf entries you want to keep as patches first (Stash & Shelf
  tab, `x`).
- **Local history only:** delete `.git/gitjet/local-history/`, or prune it with **Prune old local
  history**.

Deleting these files never touches your commits, branches, stashes or work tree files.
