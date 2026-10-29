//! Small cross-platform helpers that are not Git-specific.

use std::path::Path;
use std::process::Command;

use chrono::{Local, TimeZone};

pub const NOOP_EDITOR_ARG: &str = "__noop-editor";
pub const WRITE_FILE_ARG: &str = "__write-file";

/// A command that runs `cmd` through the platform shell (`cmd /C` on Windows, `sh -c` elsewhere).
#[cfg(windows)]
pub fn shell(cmd: &str) -> Command {
    use std::os::windows::process::CommandExt;
    // `arg` would escape inner quotes as `\"`, which cmd.exe does not understand. Pass the line
    // verbatim; with /S, cmd strips only the outer pair of quotes and keeps the rest as written.
    let mut c = Command::new("cmd");
    c.args(["/S", "/C"]).raw_arg(format!("\"{cmd}\""));
    c
}

/// A command that runs `cmd` through the platform shell (`cmd /C` on Windows, `sh -c` elsewhere).
#[cfg(not(windows))]
pub fn shell(cmd: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    c
}

/// Quote a path so that git's shell (sh on all platforms, incl. Git for Windows) sees one argument.
pub fn sh_quote(s: &str) -> String {
    let s = s.replace('\\', "/");
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn self_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "gitjet".to_string())
}

/// Value for GIT_EDITOR / GIT_SEQUENCE_EDITOR that accepts whatever git proposes (no terminal needed).
/// Set on every git command, so it is computed once.
pub fn noop_editor() -> &'static str {
    static EDITOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    EDITOR.get_or_init(|| format!("{} {}", sh_quote(&self_exe()), NOOP_EDITOR_ARG))
}

/// Value for GIT_EDITOR / GIT_SEQUENCE_EDITOR that overwrites the file git opens with `src`.
pub fn write_file_editor(src: &Path) -> String {
    format!(
        "{} {} {}",
        sh_quote(&self_exe()),
        WRITE_FILE_ARG,
        sh_quote(&src.to_string_lossy())
    )
}

/// The user's editor: config `editor`, then $VISUAL, $EDITOR, then a platform default (on
/// Windows the vim of Git for Windows when installed, else notepad). A bare path with spaces,
/// like `C:\Program Files\Git\usr\bin\vim.exe`, is quoted so the shell keeps it whole.
pub fn editor(configured: Option<&str>) -> String {
    let ed = configured
        .map(str::to_string)
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(default_editor);
    quote_editor_path(&ed)
}

fn default_editor() -> String {
    if !cfg!(windows) {
        return "vi".into();
    }
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|dir| Path::new(&dir).join(r"Git\usr\bin\vim.exe"))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notepad".into())
}

/// Quote `ed` when it is an unquoted path to an existing file containing spaces; anything else
/// (a command with arguments, an already quoted path) is left as written.
fn quote_editor_path(ed: &str) -> String {
    let t = ed.trim();
    if !t.starts_with('"') && t.contains(' ') && Path::new(t).is_file() {
        format!("\"{t}\"")
    } else {
        t.to_string()
    }
}

/// Whether `ed` (as returned by [`editor`]) runs in the terminal and takes `+line`.
fn is_terminal_editor(ed: &str) -> bool {
    let base = ed
        .trim_start_matches('"')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(ed)
        .trim_end_matches('"')
        .to_lowercase();
    ["vi", "vim", "nvim", "nano", "emacs", "hx", "micro", "kak"]
        .iter()
        .any(|e| {
            base == *e || base.starts_with(&format!("{e}.")) || base.starts_with(&format!("{e} "))
        })
}

fn line_arg(ed: &str, line: Option<usize>) -> Option<String> {
    line.filter(|_| is_terminal_editor(ed))
        .map(|n| format!("+{n}"))
}

/// A command opening `path` (optionally at `line`) in the user's editor. Run it with `Action::Suspend`.
pub fn editor_command(configured: Option<&str>, path: &Path, line: Option<usize>) -> Command {
    let ed = editor(configured);
    let p = path.to_string_lossy();
    let line_arg = line_arg(&ed, line)
        .map(|a| format!("{a} "))
        .unwrap_or_default();
    if cfg!(windows) {
        shell(&format!("{ed} {line_arg}\"{p}\""))
    } else {
        shell(&format!("{ed} {line_arg}{}", sh_quote(&p)))
    }
}

/// "3 minutes ago" style relative time for a unix timestamp.
pub fn relative_time(ts: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let d = (now - ts).max(0);
    let (n, unit) = match d {
        0..=59 => return "just now".into(),
        60..=3599 => (d / 60, "minute"),
        3600..=86_399 => (d / 3600, "hour"),
        86_400..=2_591_999 => (d / 86_400, "day"),
        2_592_000..=31_535_999 => (d / 2_592_000, "month"),
        _ => (d / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Local date-time like `2026-10-27 14:03`.
pub fn format_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Local date like `2026-10-27`.
pub fn format_date(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Truncate to at most `width` display columns, adding `…` when cut.
pub fn truncate(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut w = 0;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Pad or truncate to exactly `width` display columns.
pub fn fit(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let t = truncate(s, width);
    let pad = width.saturating_sub(t.width());
    format!("{t}{}", " ".repeat(pad))
}

pub fn repo_name_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    trimmed
        .rsplit(['/', ':', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("repo")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_paths_with_spaces_are_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("my editors").join("vim.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();
        let raw = exe.to_string_lossy().into_owned();
        assert_eq!(editor(Some(&raw)), format!("\"{raw}\""));
        // Already quoted, or a command with arguments: left alone.
        assert_eq!(editor(Some(&format!("\"{raw}\""))), format!("\"{raw}\""));
        assert_eq!(editor(Some("code --wait")), "code --wait");
        // The quoted vim still gets the +line argument.
        let cmd = format!(
            "{:?}",
            editor_command(Some(&raw), Path::new("a.txt"), Some(7))
        );
        assert!(cmd.contains("+7"), "{cmd}");
    }

    #[test]
    fn repo_names() {
        assert_eq!(repo_name_from_url("https://github.com/a/b.git"), "b");
        assert_eq!(repo_name_from_url("git@github.com:a/b"), "b");
        assert_eq!(repo_name_from_url("C:\\code\\proj"), "proj");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(fit("ab", 4), "ab  ");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("C:\\a b\\x"), "'C:/a b/x'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
