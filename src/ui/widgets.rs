//! Reusable widgets and layout helpers.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use crate::theme::Theme;

/// A rectangle of `width` x `height` cells (clamped to `area`) centered in `area`.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(h);
    v
}

/// A rectangle taking `pct_x`% x `pct_y`% of `area`, centered.
pub fn centered_pct(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    centered(area, area.width * pct_x / 100, area.height * pct_y / 100)
}

/// A pane inside a view or a fullscreen tool: a thin rule along the top carrying the title (lit
/// when focused) and a column of padding on each side, so neighbouring panes read as one quiet
/// line instead of a grid of boxes.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(theme.border(focused))
        .padding(Padding::horizontal(1))
        .title(title.into())
        .title_style(if focused {
            theme.title()
        } else {
            theme.dimmed()
        })
}

/// A floating dialog: a full rounded frame so it stands off the content behind it.
pub fn dialog<'a>(title: impl Into<Line<'a>>, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border(true))
        .padding(Padding::horizontal(1))
        .title(title.into())
        .title_style(theme.title())
}

/// True if this key event is a printable character to insert (handles AltGr on Windows, which
/// reports Ctrl+Alt).
pub fn is_text_input(key: &KeyEvent) -> Option<char> {
    if let KeyCode::Char(c) = key.code {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl == alt {
            return Some(c);
        }
    }
    None
}

fn ctrl(key: &KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn width_of(chars: &[char]) -> usize {
    chars.iter().map(|c| c.width().unwrap_or(0)).sum()
}

/// Single-line text field.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    pub fn new(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn set(&mut self, s: &str) {
        *self = Self::new(s);
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Apply an editing key. Returns true if the key was consumed.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if let Some(c) = is_text_input(key) {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
            return true;
        }
        if ctrl(key, 'a') {
            self.cursor = 0;
            return true;
        }
        if ctrl(key, 'e') {
            self.cursor = self.chars.len();
            return true;
        }
        if ctrl(key, 'u') {
            self.chars.drain(..self.cursor);
            self.cursor = 0;
            return true;
        }
        if ctrl(key, 'w')
            || (key.code == KeyCode::Backspace && key.modifiers.contains(KeyModifiers::ALT))
        {
            let mut i = self.cursor;
            while i > 0 && self.chars[i - 1] == ' ' {
                i -= 1;
            }
            while i > 0 && self.chars[i - 1] != ' ' {
                i -= 1;
            }
            self.chars.drain(i..self.cursor);
            self.cursor = i;
            return true;
        }
        match key.code {
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Draw the text (scrolled horizontally to keep the cursor visible); places the terminal cursor when focused.
    pub fn render(&self, f: &mut Frame, area: Rect, style: Style, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let w = area.width as usize;
        let mut start = 0;
        while width_of(&self.chars[start..self.cursor]) >= w {
            start += 1;
        }
        let visible: String = self.chars[start..].iter().collect();
        f.render_widget(Paragraph::new(Span::styled(visible, style)), area);
        if focused {
            let x = area.x + width_of(&self.chars[start..self.cursor]) as u16;
            f.set_cursor_position((x, area.y));
        }
    }
}

/// Styles one line of a [`TextArea`] given its index and text.
pub type LineStyler<'a> = &'a dyn Fn(usize, &str) -> Style;

/// Multi-line text editor (commit messages, PR descriptions).
#[derive(Debug, Clone)]
pub struct TextArea {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    scroll: usize,
    hscroll: usize,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new("")
    }
}

impl TextArea {
    pub fn new(s: &str) -> Self {
        let mut lines: Vec<Vec<char>> = s
            .split('\n')
            .map(|l| l.trim_end_matches('\r').chars().collect())
            .collect();
        if lines.is_empty() {
            lines.push(Vec::new());
        }
        let row = lines.len() - 1;
        let col = lines[row].len();
        Self {
            lines,
            row,
            col,
            scroll: 0,
            hscroll: 0,
        }
    }

    pub fn value(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn set(&mut self, s: &str) {
        *self = Self::new(s);
    }

    /// Apply an editing key. Returns true if consumed. Enter inserts a newline.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        if let Some(c) = is_text_input(key) {
            self.lines[self.row].insert(self.col, c);
            self.col += 1;
            return true;
        }
        match key.code {
            KeyCode::Enter => {
                let rest = self.lines[self.row].split_off(self.col);
                self.lines.insert(self.row + 1, rest);
                self.row += 1;
                self.col = 0;
            }
            KeyCode::Backspace => {
                if self.col > 0 {
                    self.col -= 1;
                    self.lines[self.row].remove(self.col);
                } else if self.row > 0 {
                    let cur = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                    self.lines[self.row].extend(cur);
                }
            }
            KeyCode::Delete => {
                if self.col < self.lines[self.row].len() {
                    self.lines[self.row].remove(self.col);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(next);
                }
            }
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            KeyCode::Right => {
                if self.col < self.lines[self.row].len() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.col = self.col.min(self.lines[self.row].len());
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = self.col.min(self.lines[self.row].len());
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.lines[self.row].len(),
            KeyCode::PageUp => {
                self.row = self.row.saturating_sub(10);
                self.col = self.col.min(self.lines[self.row].len());
            }
            KeyCode::PageDown => {
                self.row = (self.row + 10).min(self.lines.len() - 1);
                self.col = self.col.min(self.lines[self.row].len());
            }
            KeyCode::Up | KeyCode::Down => {}
            _ => return false,
        }
        true
    }

    /// Draw inside `area` (no border). `style_line` can style individual lines (e.g. long subject warning).
    pub fn render(
        &mut self,
        f: &mut Frame,
        area: Rect,
        style: Style,
        focused: bool,
        style_line: Option<LineStyler<'_>>,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let h = area.height as usize;
        let w = area.width as usize;
        if self.row < self.scroll {
            self.scroll = self.row;
        } else if self.row >= self.scroll + h {
            self.scroll = self.row + 1 - h;
        }
        let cur_w = width_of(&self.lines[self.row][..self.col]);
        if cur_w < self.hscroll {
            self.hscroll = cur_w;
        } else if cur_w >= self.hscroll + w {
            self.hscroll = cur_w + 1 - w;
        }
        let lines: Vec<Line> = self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(h)
            .map(|(i, l)| {
                let s: String = l.iter().collect();
                let st = style_line.map(|f| f(i, &s)).unwrap_or(style);
                Line::styled(s, st)
            })
            .collect();
        f.render_widget(Paragraph::new(lines).scroll((0, self.hscroll as u16)), area);
        if focused {
            let x = area.x + (cur_w - self.hscroll) as u16;
            let y = area.y + (self.row - self.scroll) as u16;
            f.set_cursor_position((x, y));
        }
    }
}

/// Keep `selected` visible in a list of `len` items shown in `height` rows; returns the new offset.
pub fn scroll_to_keep_visible(selected: usize, offset: usize, height: usize) -> usize {
    if height == 0 {
        return offset;
    }
    if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

/// Move a selection index by `delta` within `len` items (clamped).
pub fn move_sel(sel: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (sel as isize + delta).clamp(0, len as isize - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn text_input_edits() {
        let mut t = TextInput::new("ab");
        t.handle_key(&k(KeyCode::Left));
        t.handle_key(&k(KeyCode::Char('X')));
        assert_eq!(t.value(), "aXb");
        t.handle_key(&k(KeyCode::Backspace));
        assert_eq!(t.value(), "ab");
        let altgr = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(t.handle_key(&altgr));
        assert_eq!(t.value(), "a@b");
    }

    #[test]
    fn text_area_newlines() {
        let mut t = TextArea::new("subject");
        t.handle_key(&k(KeyCode::Enter));
        t.handle_key(&k(KeyCode::Enter));
        t.handle_key(&k(KeyCode::Char('b')));
        assert_eq!(t.value(), "subject\n\nb");
        t.handle_key(&k(KeyCode::Home));
        t.handle_key(&k(KeyCode::Backspace));
        assert_eq!(t.value(), "subject\nb");
    }
}
