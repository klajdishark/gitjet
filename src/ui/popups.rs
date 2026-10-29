//! Stock dialogs: confirm, text input, multi-line text, menu (filterable), scrollable message.
//!
//! Callbacks receive `&Ctx` and return the [`Action`] to apply after the popup closes.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::widgets::{centered, dialog, move_sel, TextArea, TextInput};
use super::{KeyHelp, Popup};
use crate::action::Action;
use crate::ctx::Ctx;

type OnYes = Box<dyn FnOnce(&Ctx) -> Action>;
type OnText = Box<dyn FnOnce(String, &Ctx) -> Action>;

fn close_then(a: Action) -> Action {
    Action::ClosePopup.then(a)
}

/// Yes/No question. `y`/Enter confirms, `n`/Esc cancels.
pub struct ConfirmPopup {
    title: String,
    message: String,
    on_yes: Option<OnYes>,
    danger: bool,
}

impl ConfirmPopup {
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        on_yes: impl FnOnce(&Ctx) -> Action + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            on_yes: Some(Box::new(on_yes)),
            danger: false,
        }
    }

    /// Red styling for destructive operations (hard reset, delete, force push).
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

impl Popup for ConfirmPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let width = (self
            .message
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(20) as u16
            + 4)
        .clamp(40, 90);
        let lines = self.message.lines().count() as u16 + (self.message.len() as u16 / width) + 4;
        let r = centered(area, width, lines.min(area.height));
        f.render_widget(Clear, r);
        let mut block = dialog(format!(" {} ", self.title), t);
        if self.danger {
            block = block.border_style(Style::default().fg(t.error));
        }
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [msg, keys] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        f.render_widget(
            Paragraph::new(self.message.as_str())
                .style(t.text())
                .wrap(Wrap { trim: false }),
            msg,
        );
        f.render_widget(
            Line::from(vec![
                Span::styled("y/Enter", t.key()),
                Span::styled(" yes   ", t.dimmed()),
                Span::styled("n/Esc", t.key()),
                Span::styled(" no", t.dimmed()),
            ]),
            keys,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => match self.on_yes.take() {
                Some(cb) => close_then(cb(ctx)),
                None => Action::ClosePopup,
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                Action::ClosePopup
            }
            _ => Action::None,
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("y/Enter", "confirm"), ("n/Esc", "cancel")]
    }
}

/// Single-line text prompt. Enter submits, Esc cancels.
pub struct InputPopup {
    title: String,
    prompt: String,
    input: TextInput,
    on_submit: Option<OnText>,
    allow_empty: bool,
}

impl InputPopup {
    pub fn new(
        title: impl Into<String>,
        initial: &str,
        on_submit: impl FnOnce(String, &Ctx) -> Action + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            prompt: String::new(),
            input: TextInput::new(initial),
            on_submit: Some(Box::new(on_submit)),
            allow_empty: false,
        }
    }

    /// Explanatory text shown above the field.
    pub fn prompt(mut self, p: impl Into<String>) -> Self {
        self.prompt = p.into();
        self
    }

    pub fn allow_empty(mut self) -> Self {
        self.allow_empty = true;
        self
    }
}

impl Popup for InputPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let prompt_lines = if self.prompt.is_empty() {
            0
        } else {
            self.prompt.lines().count() as u16 + 1
        };
        let r = centered(area, 70, 4 + prompt_lines);
        f.render_widget(Clear, r);
        let block = dialog(format!(" {} ", self.title), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [p, field, keys] = Layout::vertical([
            Constraint::Length(prompt_lines),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        if prompt_lines > 0 {
            f.render_widget(Paragraph::new(self.prompt.as_str()).style(t.dimmed()), p);
        }
        self.input.render(f, field, t.text(), true);
        f.render_widget(
            Line::from(vec![
                Span::styled("Enter", t.key()),
                Span::styled(" ok   ", t.dimmed()),
                Span::styled("Esc", t.key()),
                Span::styled(" cancel", t.dimmed()),
            ]),
            keys,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        match key.code {
            KeyCode::Esc => Action::ClosePopup,
            KeyCode::Enter => {
                let v = self.input.value();
                if v.trim().is_empty() && !self.allow_empty {
                    return Action::None;
                }
                match self.on_submit.take() {
                    Some(cb) => close_then(cb(v, ctx)),
                    None => Action::ClosePopup,
                }
            }
            _ => {
                self.input.handle_key(&key);
                Action::None
            }
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("Enter", "ok"), ("Esc", "cancel")]
    }
}

/// Multi-line text editor dialog. Ctrl+S (or Alt+Enter) submits, Esc cancels.
pub struct TextAreaPopup {
    title: String,
    area: TextArea,
    on_submit: Option<OnText>,
    subject_limit: Option<usize>,
}

impl TextAreaPopup {
    pub fn new(
        title: impl Into<String>,
        initial: &str,
        on_submit: impl FnOnce(String, &Ctx) -> Action + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            area: TextArea::new(initial),
            on_submit: Some(Box::new(on_submit)),
            subject_limit: None,
        }
    }

    /// Highlight the first line in warning color when longer than `n` (commit subject convention).
    pub fn subject_limit(mut self, n: usize) -> Self {
        self.subject_limit = Some(n);
        self
    }
}

impl Popup for TextAreaPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered(area, 90, 20);
        f.render_widget(Clear, r);
        let block = dialog(format!(" {} ", self.title), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [text, keys] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        let limit = self.subject_limit;
        let warn = Style::default().fg(t.warning);
        let normal = t.text();
        let styler = move |i: usize, s: &str| {
            if i == 0 && limit.is_some_and(|n| s.chars().count() > n) {
                warn
            } else {
                normal
            }
        };
        self.area.render(f, text, normal, true, Some(&styler));
        f.render_widget(
            Line::from(vec![
                Span::styled("Ctrl+S", t.key()),
                Span::styled(" save   ", t.dimmed()),
                Span::styled("Esc", t.key()),
                Span::styled(" cancel", t.dimmed()),
            ]),
            keys,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let submit = (key.code == KeyCode::Char('s')
            && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT));
        if submit {
            let v = self.area.value();
            if v.trim().is_empty() {
                return Action::None;
            }
            return match self.on_submit.take() {
                Some(cb) => close_then(cb(v, ctx)),
                None => Action::ClosePopup,
            };
        }
        if key.code == KeyCode::Esc {
            return Action::ClosePopup;
        }
        self.area.handle_key(&key);
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("Ctrl+S", "save"), ("Esc", "cancel")]
    }
}

pub struct MenuItem {
    pub label: String,
    /// Shortcut shown on the right; a single character also works as an accelerator.
    pub key: String,
    pub run: Box<dyn FnOnce(&Ctx) -> Action>,
    pub enabled: bool,
}

impl MenuItem {
    pub fn new(
        label: impl Into<String>,
        key: impl Into<String>,
        run: impl FnOnce(&Ctx) -> Action + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            key: key.into(),
            run: Box::new(run),
            enabled: true,
        }
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
}

/// Context menu / chooser. Up/Down + Enter, single-char accelerators, or type to filter (when filterable).
pub struct MenuPopup {
    title: String,
    items: Vec<Option<MenuItem>>,
    sel: usize,
    filter: Option<TextInput>,
}

impl MenuPopup {
    pub fn new(title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            title: title.into(),
            items: items.into_iter().map(Some).collect(),
            sel: 0,
            filter: None,
        }
    }

    /// Type-to-filter mode (used by the action palette). Accelerators are disabled.
    pub fn filterable(mut self) -> Self {
        self.filter = Some(TextInput::default());
        self
    }

    fn visible(&self) -> Vec<usize> {
        let q = self
            .filter
            .as_ref()
            .map(|f| f.value().to_lowercase())
            .unwrap_or_default();
        let mut scored: Vec<(u8, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let it = it.as_ref()?;
                if q.is_empty() {
                    return Some((0, i));
                }
                match_score(&it.label.to_lowercase(), &q).map(|s| (s, i))
            })
            .collect();
        // Loose subsequence hits only show when nothing matches properly.
        if scored.iter().any(|(s, _)| *s < FUZZY_SCORE) {
            scored.retain(|(s, _)| *s < FUZZY_SCORE);
        }
        scored.sort_by_key(|(s, _)| *s);
        scored.into_iter().map(|(_, i)| i).collect()
    }

    fn run(&mut self, idx: usize, ctx: &Ctx) -> Action {
        match self.items.get_mut(idx).and_then(Option::take) {
            Some(item) if item.enabled => close_then((item.run)(ctx)),
            Some(item) => {
                self.items[idx] = Some(item);
                Action::None
            }
            None => Action::None,
        }
    }
}

const FUZZY_SCORE: u8 = 4;

/// How well lowercase `label` matches lowercase query `q` (lower is better), or None.
/// Prefix < word prefix < substring < all words present < subsequence.
pub fn match_score(label: &str, q: &str) -> Option<u8> {
    let q = q.trim();
    if label.starts_with(q) {
        Some(0)
    } else if label
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with(q))
    {
        Some(1)
    } else if label.contains(q) {
        Some(2)
    } else if q.split_whitespace().all(|w| label.contains(w)) {
        Some(3)
    } else if fuzzy_match(label, q) {
        Some(FUZZY_SCORE)
    } else {
        None
    }
}

/// Subsequence match: every char of `q` appears in order in `s`.
pub fn fuzzy_match(s: &str, q: &str) -> bool {
    let mut it = s.chars();
    q.chars()
        .filter(|c| *c != ' ')
        .all(|qc| it.any(|c| c == qc))
}

impl Popup for MenuPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.visible();
        self.sel = self.sel.min(vis.len().saturating_sub(1));
        let width = self
            .items
            .iter()
            .flatten()
            .map(|i| i.label.chars().count() + i.key.chars().count() + 6)
            .max()
            .unwrap_or(30)
            .clamp(36, 80) as u16;
        let extra = if self.filter.is_some() { 2 } else { 0 };
        let height = (vis.len() as u16 + 2 + extra).clamp(5, 24);
        let r = centered(area, width, height);
        f.render_widget(Clear, r);
        let block = dialog(format!(" {} ", self.title), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [filter_area, list_area] =
            Layout::vertical([Constraint::Length(extra), Constraint::Min(1)]).areas(inner);
        if let Some(fi) = &self.filter {
            let [a, _] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(filter_area);
            let [p, inp] = Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(a);
            f.render_widget(Span::styled("> ", t.key()), p);
            fi.render(f, inp, t.text(), true);
        }
        let w = list_area.width as usize;
        let items: Vec<ListItem> = vis
            .iter()
            .map(|&i| {
                let it = self.items[i].as_ref().expect("visible");
                let style = if it.enabled { t.text() } else { t.dimmed() };
                let pad = w.saturating_sub(it.label.chars().count() + it.key.chars().count() + 2);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {}", it.label), style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(
                        format!("{} ", it.key),
                        t.key().remove_modifier(Modifier::BOLD),
                    ),
                ]))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(t.selected()),
            list_area,
            &mut state,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let vis = self.visible();
        match key.code {
            KeyCode::Esc => return Action::ClosePopup,
            KeyCode::Up => self.sel = move_sel(self.sel, -1, vis.len()),
            KeyCode::Down => self.sel = move_sel(self.sel, 1, vis.len()),
            KeyCode::PageUp => self.sel = move_sel(self.sel, -10, vis.len()),
            KeyCode::PageDown => self.sel = move_sel(self.sel, 10, vis.len()),
            KeyCode::Enter => {
                if let Some(&idx) = vis.get(self.sel) {
                    return self.run(idx, ctx);
                }
            }
            _ => {
                if let Some(fi) = self.filter.as_mut() {
                    if fi.handle_key(&key) {
                        self.sel = 0;
                    }
                    return Action::None;
                }
                match key.code {
                    KeyCode::Char('k') => self.sel = move_sel(self.sel, -1, vis.len()),
                    KeyCode::Char('j') => self.sel = move_sel(self.sel, 1, vis.len()),
                    KeyCode::Char('q') => return Action::ClosePopup,
                    KeyCode::Char(c) => {
                        let hit = self.items.iter().position(|it| {
                            it.as_ref().is_some_and(|it| {
                                it.key.chars().count() == 1 && it.key.starts_with(c)
                            })
                        });
                        if let Some(idx) = hit {
                            return self.run(idx, ctx);
                        }
                    }
                    _ => {}
                }
            }
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("↑↓", "move"), ("Enter", "run"), ("Esc", "close")]
    }
}

/// Scrollable read-only text (errors, command output, help).
pub struct MessagePopup {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
    is_error: bool,
}

impl MessagePopup {
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        let text: String = text.into();
        Self {
            title: title.into(),
            lines: text.lines().map(|l| Line::raw(l.to_string())).collect(),
            scroll: 0,
            is_error: false,
        }
    }

    pub fn from_lines(title: impl Into<String>, lines: Vec<Line<'static>>) -> Self {
        Self {
            title: title.into(),
            lines,
            scroll: 0,
            is_error: false,
        }
    }

    pub fn error(mut self) -> Self {
        self.is_error = true;
        self
    }
}

impl Popup for MessagePopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let width = (self.lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16 + 4)
            .clamp(40, area.width.saturating_sub(4).max(40));
        let height = (self.lines.len() as u16 + 3).clamp(5, area.height.saturating_sub(2).max(5));
        let r = centered(area, width, height);
        f.render_widget(Clear, r);
        let mut block = dialog(format!(" {} ", self.title), t);
        if self.is_error {
            block = block
                .border_style(Style::default().fg(t.error))
                .title_style(Style::default().fg(t.error));
        }
        let inner = block.inner(r);
        f.render_widget(block, r);
        let max = (self.lines.len() as u16).saturating_sub(inner.height);
        self.scroll = self.scroll.min(max);
        f.render_widget(
            Paragraph::new(self.lines.clone())
                .style(t.text())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            inner,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, _ctx: &Ctx) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Action::ClosePopup,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Action::None
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll = self.scroll.saturating_add(10);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![("↑↓", "scroll"), ("Esc", "close")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking() {
        assert_eq!(match_score("push…", "pu"), Some(0));
        assert_eq!(match_score("commit and push…", "pu"), Some(1));
        assert_eq!(
            match_score("continue applying patches (git am --continue)", "pu"),
            Some(FUZZY_SCORE)
        );
        assert_eq!(match_score("new branch", "branch new"), Some(3));
        assert_eq!(match_score("fetch", "zz"), None);
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_match("push commits", "psh"));
        assert!(fuzzy_match("new branch", "new br"));
        assert!(!fuzzy_match("fetch", "fx"));
    }
}
