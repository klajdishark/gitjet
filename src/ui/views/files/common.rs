//! Helpers shared by the Files tab and its popups: background loading, line navigation, inline
//! search / go-to-line prompts, colors and small cross-popup actions.

use std::sync::mpsc::{channel, Receiver, TryRecvError};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::status::FileState;
use crate::git::Repo;
use crate::theme::Theme;
use crate::ui::popups::MessagePopup;
use crate::ui::widgets::{scroll_to_keep_visible, TextInput};

/// Result of a background load, delivered once to the popup that started it.
pub struct Loader<T> {
    rx: Option<Receiver<anyhow::Result<T>>>,
}

impl<T: Send + 'static> Loader<T> {
    /// Run `job` on a background thread with its own copy of the repository handle.
    pub fn start(
        repo: &Repo,
        job: impl FnOnce(&Repo) -> anyhow::Result<T> + Send + 'static,
    ) -> Self {
        let (tx, rx) = channel();
        let repo = repo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(job(&repo));
        });
        Self { rx: Some(rx) }
    }

    /// A loader that has nothing in flight.
    pub fn idle() -> Self {
        Self { rx: None }
    }

    /// True while the job has not delivered its result yet.
    pub fn is_loading(&self) -> bool {
        self.rx.is_some()
    }

    /// The job's result, once; `None` while still running or when idle.
    pub fn poll(&mut self) -> Option<anyhow::Result<T>> {
        let rx = self.rx.as_ref()?;
        match rx.try_recv() {
            Ok(r) => {
                self.rx = None;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                Some(Err(anyhow::anyhow!("background job stopped unexpectedly")))
            }
        }
    }
}

/// Cursor + scroll offset over `len` lines shown in `height` rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineNav {
    pub cursor: usize,
    pub offset: usize,
    pub height: usize,
}

impl LineNav {
    /// Move the cursor by `delta` lines (clamped).
    pub fn move_by(&mut self, delta: isize, len: usize) {
        self.cursor = crate::ui::widgets::move_sel(self.cursor, delta, len);
    }

    /// One screen up/down.
    pub fn page(&mut self, down: bool, len: usize) {
        let h = self.height.max(1) as isize;
        self.move_by(if down { h } else { -h }, len);
    }

    /// Jump to `idx`, centering it when it is off screen.
    pub fn jump(&mut self, idx: usize, len: usize) {
        self.cursor = idx.min(len.saturating_sub(1));
        if self.cursor < self.offset || self.cursor >= self.offset + self.height.max(1) {
            self.offset = self.cursor.saturating_sub(self.height / 2);
        }
    }

    /// Fix the offset for a viewport of `height` rows before drawing.
    pub fn fit(&mut self, height: usize, len: usize) {
        self.height = height;
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.offset = scroll_to_keep_visible(self.cursor, self.offset, height);
        self.offset = self.offset.min(len.saturating_sub(height));
    }

    /// Standard navigation keys; returns true when handled.
    pub fn handle_key(&mut self, key: &KeyEvent, len: usize) -> bool {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1, len),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1, len),
            KeyCode::PageDown | KeyCode::Char(' ') => self.page(true, len),
            KeyCode::PageUp => self.page(false, len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = len.saturating_sub(1),
            _ => return false,
        }
        true
    }
}

/// Index of the next line (after `from`, wrapping) containing `query`, case-insensitively.
pub fn find_line<S: AsRef<str>>(
    lines: &[S],
    query: &str,
    from: usize,
    forward: bool,
) -> Option<usize> {
    let q = query.to_lowercase();
    let n = lines.len();
    if q.is_empty() || n == 0 {
        return None;
    }
    (1..=n)
        .map(|step| {
            if forward {
                (from + step) % n
            } else {
                (from + n * 2 - step) % n
            }
        })
        .find(|&i| lines[i].as_ref().to_lowercase().contains(&q))
}

/// Parse a 1-based line number for "go to line"; clamps to the file length.
pub fn parse_line_number(s: &str, len: usize) -> Option<usize> {
    let n: usize = s.trim().trim_start_matches(':').parse().ok()?;
    (n > 0 && len > 0).then(|| n.min(len) - 1)
}

/// Parse a line range like `10`, `10-20`, `10,20` or `10:20` into 1-based inclusive bounds.
pub fn parse_line_range(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let (a, b) = match s.split_once(['-', ',', ':']) {
        Some((a, b)) => (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?),
        None => {
            let n = s.parse::<u32>().ok()?;
            (n, n)
        }
    };
    let (lo, hi) = (a.min(b), a.max(b));
    (lo > 0).then_some((lo, hi))
}

/// What the inline prompt at the bottom of a line viewer is asking for.
pub enum Prompt {
    None,
    Search(TextInput),
    Goto(TextInput),
}

/// Outcome of a key typed into a [`Prompt`].
pub enum PromptOutcome {
    /// Key consumed, prompt still open
    Editing,
    Cancelled,
    Search(String),
    Goto(String),
}

impl Prompt {
    pub fn is_active(&self) -> bool {
        !matches!(self, Prompt::None)
    }

    /// Feed a key to the open prompt. Closes it on Enter/Esc.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PromptOutcome {
        let outcome = match (&mut *self, key.code) {
            (Prompt::None, _) => return PromptOutcome::Cancelled,
            (_, KeyCode::Esc) => PromptOutcome::Cancelled,
            (Prompt::Search(t), KeyCode::Enter) => PromptOutcome::Search(t.value()),
            (Prompt::Goto(t), KeyCode::Enter) => PromptOutcome::Goto(t.value()),
            (Prompt::Search(t) | Prompt::Goto(t), _) => {
                t.handle_key(key);
                return PromptOutcome::Editing;
            }
        };
        *self = Prompt::None;
        outcome
    }

    /// Draw the prompt in a one-line footer.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let (label, input) = match self {
            Prompt::None => return,
            Prompt::Search(t) => ("Find: ", t),
            Prompt::Goto(t) => ("Go to line: ", t),
        };
        let w = label.chars().count() as u16;
        f.render_widget(Span::styled(label, theme.key()), area);
        let field = Rect {
            x: area.x + w.min(area.width),
            width: area.width.saturating_sub(w),
            ..area
        };
        input.render(f, field, theme.text(), true);
    }
}

/// Color for a file state marker.
pub fn state_color(theme: &Theme, s: FileState) -> Color {
    match s {
        FileState::Modified | FileState::TypeChanged => theme.modified,
        FileState::Added | FileState::Copied => theme.added,
        FileState::Deleted => theme.removed,
        FileState::Renamed => theme.renamed,
        FileState::Untracked => theme.untracked,
        FileState::Ignored => theme.ignored,
        FileState::Conflicted => theme.conflict,
        FileState::Unmodified => theme.fg,
    }
}

/// Color for a name-status letter in history lists.
pub fn status_letter_color(theme: &Theme, c: char) -> Color {
    match c {
        'A' | 'C' => theme.added,
        'D' => theme.removed,
        'R' => theme.renamed,
        _ => theme.modified,
    }
}

fn blend(a: Color, b: Color, t: f32) -> Option<Color> {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |x: u8, y: u8| {
                (x as f32 + (y as f32 - x as f32) * t)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            Some(Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2)))
        }
        _ => None,
    }
}

/// Annotation color by age: the newest commit gets the accent color, the oldest the dim color.
pub fn age_color(theme: &Theme, time: i64, oldest: i64, newest: i64) -> Color {
    let t = if newest <= oldest {
        1.0
    } else {
        ((time - oldest) as f32 / (newest - oldest) as f32).clamp(0.0, 1.0)
    };
    blend(theme.dim, theme.accent, t).unwrap_or(if t > 0.66 {
        theme.accent
    } else if t > 0.33 {
        theme.fg
    } else {
        theme.dim
    })
}

/// Replace tabs so content columns line up in the terminal.
pub fn expand_tabs(s: &str) -> String {
    if s.contains('\t') {
        s.replace('\t', "    ")
    } else {
        s.to_string()
    }
}

/// Open `path` (repo-relative) in the user's editor, optionally at a 1-based line.
pub fn open_in_editor(ctx: &Ctx, path: &str, line: Option<usize>) -> Action {
    let abs = ctx.repo.path(path);
    if !abs.exists() {
        return Action::Error(format!("{path} does not exist in the work tree"));
    }
    Action::edit(abs, line)
}

/// Popup with the full details of one commit (hash, author, dates, refs, message, changed files).
pub fn commit_details(ctx: &Ctx, rev: &str) -> Action {
    if rev == crate::git::blame::UNCOMMITTED {
        return Action::Toast("Not committed yet — local changes".into());
    }
    let repo = &ctx.repo;
    let info = match repo.commit_info(rev) {
        Ok(i) => i,
        Err(e) => return Action::Error(format!("{e:#}")),
    };
    let message = repo
        .commit_message(rev)
        .unwrap_or_else(|_| info.subject.clone());
    let t = &ctx.theme;
    let field = |k: &str, v: String, style: Style| {
        Line::from(vec![
            Span::styled(format!("{k:<11}"), t.dimmed()),
            Span::styled(v, style),
        ])
    };
    let mut lines = vec![
        field("Commit", info.id.clone(), Style::default().fg(t.hash)),
        field(
            "Author",
            format!("{} <{}>", info.author, info.email),
            Style::default().fg(t.author),
        ),
        field(
            "Date",
            crate::util::format_time(info.time),
            Style::default().fg(t.date),
        ),
    ];
    if info.committer != info.author || info.commit_time != info.time {
        lines.push(field(
            "Committer",
            format!(
                "{} <{}> {}",
                info.committer,
                info.committer_email,
                crate::util::format_time(info.commit_time)
            ),
            t.text(),
        ));
    }
    if !info.parents.is_empty() {
        let short: Vec<&str> = info.parents.iter().map(|p| &p[..p.len().min(10)]).collect();
        lines.push(field(
            "Parents",
            short.join(" "),
            Style::default().fg(t.hash),
        ));
    }
    if !info.refs.is_empty() {
        lines.push(field(
            "Refs",
            info.refs.join(", "),
            Style::default().fg(t.local_branch),
        ));
    }
    lines.push(Line::raw(""));
    lines.extend(
        message
            .lines()
            .map(|l| Line::styled(format!("  {l}"), t.text())),
    );
    if let Ok(files) = repo.commit_files(rev) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("{} changed file(s)", files.len()),
            t.title(),
        ));
        for (c, p, old) in files.iter().take(200) {
            let text = match old {
                Some(o) => format!("{o} → {p}"),
                None => p.clone(),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {c} "),
                    Style::default().fg(status_letter_color(t, *c)),
                ),
                Span::styled(text, t.text()),
            ]));
        }
        if files.len() > 200 {
            lines.push(Line::styled(
                format!("  … and {} more", files.len() - 200),
                t.dimmed(),
            ));
        }
    }
    Action::popup(MessagePopup::from_lines(
        format!("Commit {}", info.short),
        lines,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    #[test]
    fn line_nav_keeps_cursor_visible() {
        let mut n = LineNav::default();
        n.fit(10, 100);
        n.move_by(15, 100);
        n.fit(10, 100);
        assert_eq!((n.cursor, n.offset), (15, 6));
        n.page(true, 100);
        assert_eq!(n.cursor, 25);
        n.jump(80, 100);
        assert_eq!(n.offset, 75);
        n.fit(10, 100);
        assert!(n.cursor >= n.offset && n.cursor < n.offset + 10);
        n.handle_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 100);
        assert_eq!(n.cursor, 99);
        n.fit(10, 5);
        assert_eq!((n.cursor, n.offset), (4, 0));
    }

    #[test]
    fn find_wraps_both_ways() {
        let lines = ["alpha", "Beta", "gamma", "beta 2"];
        assert_eq!(find_line(&lines, "beta", 0, true), Some(1));
        assert_eq!(find_line(&lines, "beta", 1, true), Some(3));
        assert_eq!(find_line(&lines, "beta", 3, true), Some(1));
        assert_eq!(find_line(&lines, "beta", 1, false), Some(3));
        assert_eq!(find_line(&lines, "zzz", 0, true), None);
        assert_eq!(find_line(&lines, "", 0, true), None);
    }

    #[test]
    fn parses_numbers_and_ranges() {
        assert_eq!(parse_line_number("12", 100), Some(11));
        assert_eq!(parse_line_number("500", 100), Some(99));
        assert_eq!(parse_line_number("0", 100), None);
        assert_eq!(parse_line_number("x", 100), None);
        assert_eq!(parse_line_range("10-20"), Some((10, 20)));
        assert_eq!(parse_line_range(" 20 , 10 "), Some((10, 20)));
        assert_eq!(parse_line_range("7"), Some((7, 7)));
        assert_eq!(parse_line_range("3:4"), Some((3, 4)));
        assert_eq!(parse_line_range("0-4"), None);
        assert_eq!(parse_line_range("a-b"), None);
    }

    #[test]
    fn age_colors_are_ordered() {
        let t = Theme::dark();
        assert_eq!(age_color(&t, 10, 0, 10), t.accent);
        assert_eq!(age_color(&t, 0, 0, 10), t.dim);
        assert_eq!(age_color(&t, 5, 5, 5), t.accent);
        let Color::Rgb(r, _, _) = age_color(&t, 5, 0, 10) else {
            panic!("rgb")
        };
        let (Color::Rgb(a, _, _), Color::Rgb(b, _, _)) = (t.dim, t.accent) else {
            panic!("rgb")
        };
        assert!(r >= a.min(b) && r <= a.max(b));
    }

    #[test]
    fn prompt_flow() {
        let mut p = Prompt::Search(TextInput::default());
        assert!(matches!(
            p.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            PromptOutcome::Editing
        ));
        match p.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            PromptOutcome::Search(q) => assert_eq!(q, "x"),
            _ => panic!("expected search"),
        }
        assert!(!p.is_active());
        assert_eq!(expand_tabs("\ta"), "    a");
    }

    #[test]
    fn loader_delivers_once() {
        let (_d, repo) = crate::git::testutil::temp_repo();
        let mut l = Loader::start(&repo, |_| Ok(42));
        let mut got = None;
        for _ in 0..200 {
            if let Some(r) = l.poll() {
                got = Some(r.unwrap());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, Some(42));
        assert!(!l.is_loading());
        assert!(l.poll().is_none());
        assert!(Loader::<u8>::idle().poll().is_none());
    }
}
