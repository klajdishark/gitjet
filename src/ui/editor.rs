//! The built-in editor: a small Vim (normal / insert / visual modes, operators and text objects,
//! registers, `.`, search, and the common `:` commands like `:%s/a/b/g`) drawn in the main area,
//! so files open inside gitjet on every platform without an external program.

mod buffer;
mod ex;
mod pattern;
mod vim;

use std::ops::Range;
use std::path::PathBuf;

use anyhow::Result;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Clear;
use ratatui::Frame;
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference};

use crate::action::Action;
use crate::ctx::Ctx;
use crate::ui::diff_view::highlight;
use crate::ui::{KeyHelp, Popup};
use buffer::Pos;
use vim::{cell_width, display_col, keys, Mode, Vim};

pub struct Editor {
    vim: Vim,
    syntax: Syntax,
    /// First display column shown (horizontal scroll).
    left: usize,
    /// Text area and gutter width at the last draw, for mouse clicks.
    text_area: Rect,
    gutter: u16,
    wrote: bool,
}

impl Editor {
    /// Open `path` at 1-based `line`; `name` is shown in the status line.
    pub fn open(path: PathBuf, line: Option<usize>, name: String, ctx: &Ctx) -> Result<Self> {
        let syntax = Syntax::new(&name, ctx);
        let mut vim = Vim::open(path, line)?;
        vim.name = name;
        Ok(Self {
            vim,
            syntax,
            left: 0,
            text_area: Rect::default(),
            gutter: 0,
            wrote: false,
        })
    }
}

/// The key as the editor sees it: control keys as control codes, others as [`keys`] chars.
fn to_key(key: &KeyEvent) -> Option<char> {
    if let Some(c) = crate::ui::widgets::is_text_input(key) {
        return Some(c);
    }
    Some(match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match c.to_ascii_lowercase() {
                c @ 'a'..='z' => keys::ctrl(c),
                '[' => keys::ESC,
                _ => return None,
            }
        }
        KeyCode::Enter => keys::ENTER,
        KeyCode::Esc => keys::ESC,
        KeyCode::Backspace => keys::BS,
        KeyCode::Tab => keys::TAB,
        KeyCode::Up => keys::UP,
        KeyCode::Down => keys::DOWN,
        KeyCode::Left => keys::LEFT,
        KeyCode::Right => keys::RIGHT,
        KeyCode::Home => keys::HOME,
        KeyCode::End => keys::END,
        KeyCode::PageUp => keys::PAGE_UP,
        KeyCode::PageDown => keys::PAGE_DOWN,
        KeyCode::Delete => keys::DEL,
        _ => return None,
    })
}

impl Popup for Editor {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let r = crate::app::layout(area)[1];
        if r.width < 4 || r.height < 3 {
            return;
        }
        f.render_widget(Clear, r);
        let [text, status, cmd] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(r);
        let v = &mut self.vim;
        v.height = text.height as usize;
        v.ensure_visible();
        if let Some(from) = v.buf.take_changed() {
            self.syntax.invalidate(from);
        }
        let lines = v.buf.lines();
        let bottom = (v.top + text.height as usize).min(lines.len());
        self.syntax.update(lines, bottom);

        let th = &ctx.theme;
        let ts = v.opts.tabstop.max(1);
        let numbers = v.opts.number || v.opts.relativenumber;
        let gutter = if numbers {
            lines.len().to_string().len().max(3) as u16 + 1
        } else {
            0
        };
        let width = text.width.saturating_sub(gutter) as usize;
        self.gutter = gutter;
        self.text_area = text;

        // Keep the cursor column on screen.
        let cx = display_col(&lines[v.cur.row], v.cur.col, ts);
        if cx < self.left {
            self.left = cx;
        } else if width > 0 && cx >= self.left + width {
            self.left = cx + 1 - width;
        }
        let left = self.left;

        let search = v.highlight_regex();
        let sel = v.selection();
        let match_style = Style::default().bg(th.warning).fg(Color::Black);
        let base = th.text();
        let buf = f.buffer_mut();
        for y in 0..text.height {
            let row = v.top + y as usize;
            let sy = text.y + y;
            if row >= lines.len() {
                buf.set_string(text.x, sy, "~", th.dimmed());
                continue;
            }
            if numbers {
                let n = if v.opts.relativenumber && row != v.cur.row {
                    row.abs_diff(v.cur.row)
                } else {
                    row + 1
                };
                let style = if row == v.cur.row {
                    Style::default().fg(th.accent)
                } else {
                    th.dimmed()
                };
                let w = gutter as usize - 1;
                buf.set_string(text.x, sy, format!("{n:>w$} "), style);
            }
            let line = lines[row].as_str();
            let spans = self.syntax.line(row);
            let matches: Vec<Range<usize>> = search
                .as_ref()
                .map(|re| {
                    re.find_iter(line)
                        .map(|m| m.range())
                        .filter(|m| !m.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let selected = |col: usize| match sel {
                Some((a, b, true)) => (a.row..=b.row).contains(&row),
                Some((a, b, false)) => (a..=b).contains(&Pos::new(row, col)),
                None => false,
            };
            let x0 = text.x + gutter;
            let mut x = 0usize;
            let mut si = 0;
            let mut col = 0;
            for (bi, c) in line.char_indices() {
                if x >= left + width {
                    break;
                }
                let w = cell_width(c, x, ts);
                let mut style = base;
                while si < spans.len() && spans[si].1.end <= bi {
                    si += 1;
                }
                if let Some((s, r)) = spans.get(si) {
                    if r.contains(&bi) {
                        style = style.patch(*s);
                    }
                }
                if matches.iter().any(|m| m.contains(&bi)) {
                    style = match_style;
                }
                if selected(col) {
                    style = style.patch(th.selected());
                }
                if x >= left && x + w <= left + width {
                    let sx = x0 + (x - left) as u16;
                    match c {
                        '\t' => {
                            buf.set_string(sx, sy, " ".repeat(w), style);
                        }
                        c if c.is_control() => {
                            let shown = format!("^{}", ((c as u8) ^ 0x40) as char);
                            buf.set_string(sx, sy, shown, th.dimmed().patch(style));
                        }
                        c => {
                            buf.set_stringn(sx, sy, c.to_string(), w, style);
                        }
                    }
                } else {
                    // A wide char or tab cut by an edge: blank its visible part.
                    for cell in x.max(left)..(x + w).min(left + width) {
                        buf.set_string(x0 + (cell - left) as u16, sy, " ", style);
                    }
                }
                x += w;
                col += 1;
            }
            // A selected line end shows as one cell.
            if selected(col) && x >= left && x < left + width {
                buf.set_string(x0 + (x - left) as u16, sy, " ", th.selected());
            }
        }

        // Status line: mode, name, flags, position.
        let (label, color) = match v.mode {
            Mode::Normal => ("NORMAL", th.accent),
            Mode::Insert => ("INSERT", th.success),
            Mode::Replace => ("REPLACE", th.error),
            Mode::Visual => ("VISUAL", th.warning),
            Mode::VisualLine => ("V-LINE", th.warning),
            Mode::Command | Mode::Search { .. } => ("COMMAND", th.accent),
        };
        let bar = Style::default().bg(th.selection_bg).fg(th.fg);
        buf.set_style(status, bar);
        let label = format!(" {label} ");
        let label_style = Style::default()
            .bg(color)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        buf.set_string(status.x, status.y, &label, label_style);
        let mut flags = String::new();
        if v.buf.modified() {
            flags.push_str(" [+]");
        }
        if v.crlf {
            flags.push_str(" [dos]");
        }
        let pos = format!(
            "{}:{}  {}% ",
            v.cur.row + 1,
            v.cur.col + 1,
            (v.cur.row + 1) * 100 / lines.len()
        );
        let room = status
            .width
            .saturating_sub(label.len() as u16 + pos.len() as u16 + 2);
        let name_x = status.x + label.len() as u16 + 1;
        let name = format!("{}{flags}", v.name);
        buf.set_stringn(name_x, status.y, name, room as usize, bar);
        let pos_x = status.right().saturating_sub(pos.len() as u16);
        buf.set_string(pos_x, status.y, &pos, bar);

        // Command line: what is being typed, else the last message and pending keys.
        let prompt = match v.mode {
            Mode::Command => Some(':'),
            Mode::Search { forward } => Some(if forward { '/' } else { '?' }),
            _ => None,
        };
        if let Some(prompt) = prompt {
            let s = format!("{prompt}{}", v.cmdline);
            let w = s.chars().fold(0, |x, c| x + cell_width(c, x, ts).max(1));
            let skip = w.saturating_sub(cmd.width as usize - 1);
            let shown: String = s.chars().skip(skip).collect();
            buf.set_stringn(cmd.x, cmd.y, &shown, cmd.width as usize, th.text());
            let cx = cmd.x + (w - skip).min(cmd.width as usize - 1) as u16;
            f.set_cursor_position((cx, cmd.y));
            return;
        }
        let pending_w = v.pending.chars().count() as u16;
        if let Some((m, err)) = &v.msg {
            let style = if *err {
                Style::default().fg(th.error)
            } else {
                th.text()
            };
            let room = cmd.width.saturating_sub(pending_w + 1) as usize;
            buf.set_stringn(cmd.x, cmd.y, m, room, style);
        }
        if !v.pending.is_empty() {
            let px = cmd.right().saturating_sub(pending_w + 1);
            buf.set_string(px, cmd.y, &v.pending, th.dimmed());
        }
        if v.cur.row >= v.top && v.cur.row < v.top + text.height as usize && cx >= left {
            let x = text.x + gutter + (cx - left).min(width.saturating_sub(1)) as u16;
            f.set_cursor_position((x, text.y + (v.cur.row - v.top) as u16));
        }
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> Action {
        let Some(k) = to_key(&key) else {
            return Action::None;
        };
        let out = self.vim.key(k);
        self.wrote |= out.wrote;
        if out.quit {
            return if self.wrote {
                Action::Batch(vec![Action::ClosePopup, Action::Refresh])
            } else {
                Action::ClosePopup
            };
        }
        if out.wrote {
            Action::Refresh
        } else {
            Action::None
        }
    }

    fn handle_mouse(&mut self, ev: MouseEvent, _ctx: &Ctx) -> Action {
        let v = &mut self.vim;
        match ev.kind {
            MouseEventKind::ScrollDown => v.scroll(3),
            MouseEventKind::ScrollUp => v.scroll(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let a = self.text_area;
                let inside = ev.column >= a.x + self.gutter
                    && ev.column < a.right()
                    && ev.row >= a.y
                    && ev.row < a.bottom();
                if !inside || !matches!(v.mode, Mode::Normal | Mode::Insert) {
                    return Action::None;
                }
                let row = (v.top + (ev.row - a.y) as usize).min(v.buf.last_row());
                let target = self.left + (ev.column - a.x - self.gutter) as usize;
                let ts = v.opts.tabstop.max(1);
                let mut x = 0;
                let mut col = 0;
                for c in v.buf.line(row).chars() {
                    let w = cell_width(c, x, ts);
                    if x + w > target {
                        break;
                    }
                    x += w;
                    col += 1;
                }
                v.move_to(Pos::new(row, col));
            }
            _ => {}
        }
        Action::None
    }

    fn captures_everything(&self) -> bool {
        true
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("i/a/o", "insert"),
            ("Esc", "normal mode"),
            (":w", "save"),
            (":q", "close"),
            (":%s/a/b/g", "replace"),
            (":help", "commands"),
        ]
    }
}

/// Syntax colors per line, computed incrementally from the first changed line down to the
/// bottom of the screen.
struct Syntax {
    syntax: Option<&'static SyntaxReference>,
    highlighter: Option<Highlighter<'static>>,
    /// Parser state at the start of each line computed so far (one more than `lines`).
    states: Vec<(ParseState, HighlightState)>,
    lines: Vec<Vec<(Style, Range<usize>)>>,
}

/// Lines highlighted per frame at most; the rest fill in on the next frames.
const SYNTAX_BUDGET: usize = 2000;

impl Syntax {
    fn new(name: &str, ctx: &Ctx) -> Self {
        let syntax = ctx
            .config
            .diff
            .syntax_highlight
            .then(|| highlight::syntax_for_path(name))
            .flatten();
        let highlighter = syntax
            .and_then(|_| highlight::theme(ctx.theme.syntax_theme))
            .map(Highlighter::new);
        Self {
            syntax,
            highlighter,
            states: Vec::new(),
            lines: Vec::new(),
        }
    }

    fn invalidate(&mut self, from: usize) {
        self.lines.truncate(from);
        self.states.truncate(from + 1);
    }

    fn update(&mut self, text: &[String], upto: usize) {
        let (Some(syntax), Some(h)) = (self.syntax, self.highlighter.as_ref()) else {
            return;
        };
        if self.states.is_empty() {
            let start = (
                ParseState::new(syntax),
                HighlightState::new(h, ScopeStack::new()),
            );
            self.states.push(start);
        }
        let ss = highlight::syntax_set();
        let mut budget = SYNTAX_BUDGET;
        while self.lines.len() < upto.min(text.len()) && budget > 0 {
            budget -= 1;
            let i = self.lines.len();
            let (mut ps, mut hs) = self.states[i].clone();
            let line = &text[i];
            let mut spans = Vec::new();
            if line.len() <= highlight::MAX_LINE_LEN {
                let l = format!("{line}\n");
                if let Ok(ops) = ps.parse_line(&l, ss) {
                    let mut pos = 0;
                    for (st, piece) in HighlightIterator::new(&mut hs, &ops, &l, h) {
                        let end = (pos + piece.len()).min(line.len());
                        if end > pos {
                            spans.push((highlight::convert(st), pos..end));
                        }
                        pos += piece.len();
                    }
                }
            }
            self.lines.push(spans);
            self.states.push((ps, hs));
        }
    }

    fn line(&self, row: usize) -> &[(Style, Range<usize>)] {
        self.lines.get(row).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::temp_repo;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn press(e: &mut Editor, ctx: &Ctx, keys: &str) -> Action {
        let mut last = Action::None;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            last = e.handle_key(KeyEvent::new(code, KeyModifiers::NONE), ctx);
        }
        last
    }

    fn screen(term: &Terminal<TestBackend>, y: u16) -> String {
        let b = term.backend().buffer();
        (0..b.area.width).map(|x| b[(x, y)].symbol()).collect()
    }

    #[test]
    fn edits_a_file_in_the_main_area() {
        let (dir, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "hello world\nsecond\n").unwrap();
        let mut e = Editor::open(path.clone(), Some(2), "notes.md".into(), &ctx).unwrap();
        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        term.draw(|f| e.draw(f, f.area(), &ctx)).unwrap();
        // Row 0 is gitjet's tab bar; the text starts on row 1 after the line numbers.
        let row1 = screen(&term, 1);
        assert!(row1.starts_with("  1 hello world"), "{row1:?}");
        assert!(screen(&term, 2).starts_with("  2 second"));
        assert!(screen(&term, 3).starts_with('~'));
        let status = screen(&term, 8);
        assert!(
            status.contains("NORMAL") && status.contains("notes.md"),
            "{status:?}"
        );

        press(&mut e, &ctx, "ggcwbye\x1b");
        term.draw(|f| e.draw(f, f.area(), &ctx)).unwrap();
        assert!(screen(&term, 1).starts_with("  1 bye world"));
        assert!(screen(&term, 8).contains("[+]"));

        assert!(matches!(press(&mut e, &ctx, ":%s/o/0/g\n"), Action::None));
        assert!(matches!(press(&mut e, &ctx, ":w\n"), Action::Refresh));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "bye w0rld\nsec0nd\n"
        );
        match press(&mut e, &ctx, ":q\n") {
            Action::Batch(v) => assert!(matches!(v[0], Action::ClosePopup)),
            _ => panic!("expected the editor to close"),
        }
    }

    #[test]
    fn ctrl_keys_reach_the_editor() {
        let k = |c, m| to_key(&KeyEvent::new(KeyCode::Char(c), m));
        assert_eq!(k('r', KeyModifiers::CONTROL), Some('\x12'));
        assert_eq!(k('[', KeyModifiers::CONTROL), Some(keys::ESC));
        assert_eq!(k('x', KeyModifiers::NONE), Some('x'));
        // AltGr on Windows reports Ctrl+Alt: still text.
        assert_eq!(k('{', KeyModifiers::CONTROL | KeyModifiers::ALT), Some('{'));
    }
}
