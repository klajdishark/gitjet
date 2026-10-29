//! A small dialog with an optional text field, an optional file checklist and checkbox options.
//! Used for "Stash changes", "Shelve changes", "Unshelve" and "Apply stash".

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::ui::widgets::{centered, dialog, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, Popup};

/// Maximum checklist rows shown at once.
const MAX_LIST_ROWS: usize = 12;

/// What the user entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormValues {
    pub text: String,
    /// One flag per checklist item
    pub checked: Vec<bool>,
    /// One flag per option
    pub options: Vec<bool>,
}

/// A checklist row.
#[derive(Debug, Clone)]
pub struct CheckItem {
    pub label: String,
    /// Status letter shown before the label
    pub badge: char,
    pub checked: bool,
}

/// A checkbox option with an accelerator (Alt+key anywhere, plain key outside the text field).
/// Accelerators must not be `j`, `k` or `a`, which navigate / toggle all files.
#[derive(Debug, Clone)]
pub struct FormOption {
    pub label: String,
    pub key: char,
    pub on: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Text,
    Item(usize),
    Opt(usize),
}

type OnSubmit = Box<dyn FnOnce(FormValues, &Ctx) -> Action>;

/// The dialog. Enter submits, Esc cancels, ↑↓ move, Tab jumps between sections, Space toggles.
pub struct FormPopup {
    title: String,
    text: Option<(String, TextInput)>,
    text_required: bool,
    list_label: String,
    items: Vec<CheckItem>,
    options: Vec<FormOption>,
    submit_label: String,
    row: usize,
    list_offset: usize,
    error: Option<String>,
    on_submit: Option<OnSubmit>,
}

impl FormPopup {
    pub fn new(
        title: impl Into<String>,
        submit_label: impl Into<String>,
        on_submit: impl FnOnce(FormValues, &Ctx) -> Action + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            text: None,
            text_required: false,
            list_label: String::new(),
            items: Vec::new(),
            options: Vec::new(),
            submit_label: submit_label.into(),
            row: 0,
            list_offset: 0,
            error: None,
            on_submit: Some(Box::new(on_submit)),
        }
    }

    /// Add the text field (`required`: submitting with an empty value is refused).
    pub fn text(mut self, label: impl Into<String>, initial: &str, required: bool) -> Self {
        self.text = Some((label.into(), TextInput::new(initial)));
        self.text_required = required;
        self
    }

    /// Add the checklist; at least one item must be checked to submit.
    pub fn checklist(mut self, label: impl Into<String>, items: Vec<CheckItem>) -> Self {
        self.list_label = label.into();
        self.items = items;
        self
    }

    pub fn option(mut self, label: impl Into<String>, key: char, on: bool) -> Self {
        self.options.push(FormOption {
            label: label.into(),
            key,
            on,
        });
        self
    }

    fn rows(&self) -> Vec<Row> {
        let mut v = Vec::new();
        if self.text.is_some() {
            v.push(Row::Text);
        }
        v.extend((0..self.items.len()).map(Row::Item));
        v.extend((0..self.options.len()).map(Row::Opt));
        v
    }

    fn current(&self) -> Option<Row> {
        self.rows().get(self.row).copied()
    }

    fn move_row(&mut self, delta: isize) {
        let n = self.rows().len();
        if n > 0 {
            self.row = (self.row as isize + delta).rem_euclid(n as isize) as usize;
        }
    }

    /// Jump to the first row of the next (or previous) section.
    fn jump_section(&mut self, forward: bool) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let section = |r: Row| match r {
            Row::Text => 0,
            Row::Item(_) => 1,
            Row::Opt(_) => 2,
        };
        let cur = section(rows[self.row.min(rows.len() - 1)]);
        let mut starts: Vec<(usize, usize)> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let s = section(*r);
            if starts.last().map(|(ls, _)| *ls) != Some(s) {
                starts.push((s, i));
            }
        }
        let pos = starts.iter().position(|(s, _)| *s == cur).unwrap_or(0);
        let next = if forward {
            (pos + 1) % starts.len()
        } else {
            (pos + starts.len() - 1) % starts.len()
        };
        self.row = starts[next].1;
    }

    fn toggle(&mut self, r: Row) {
        match r {
            Row::Item(i) => self.items[i].checked = !self.items[i].checked,
            Row::Opt(i) => self.options[i].on = !self.options[i].on,
            Row::Text => {}
        }
    }

    fn toggle_all(&mut self) {
        let all = self.items.iter().all(|i| i.checked);
        for it in &mut self.items {
            it.checked = !all;
        }
    }

    fn toggle_option_key(&mut self, c: char) -> bool {
        let c = c.to_ascii_lowercase();
        match self.options.iter_mut().find(|o| o.key == c) {
            Some(o) => {
                o.on = !o.on;
                true
            }
            None => false,
        }
    }

    fn values(&self) -> FormValues {
        FormValues {
            text: self
                .text
                .as_ref()
                .map(|(_, t)| t.value())
                .unwrap_or_default(),
            checked: self.items.iter().map(|i| i.checked).collect(),
            options: self.options.iter().map(|o| o.on).collect(),
        }
    }

    fn submit(&mut self, ctx: &Ctx) -> Action {
        let v = self.values();
        if self.text_required && v.text.trim().is_empty() {
            self.error = Some("Please enter a value".into());
            return Action::None;
        }
        if !self.items.is_empty() && !v.checked.iter().any(|c| *c) {
            self.error = Some("Select at least one file (Space)".into());
            return Action::None;
        }
        match self.on_submit.take() {
            Some(cb) => Action::ClosePopup.then(cb(v, ctx)),
            None => Action::ClosePopup,
        }
    }

    fn list_rows(&self) -> usize {
        self.items.len().min(MAX_LIST_ROWS)
    }

    fn height(&self) -> u16 {
        let text = if self.text.is_some() { 3 } else { 0 };
        let list = if self.items.is_empty() {
            0
        } else {
            self.list_rows() + 2
        };
        let opts = self.options.len() + usize::from(!self.options.is_empty());
        (text + list + opts + 2 + 2) as u16
    }

    fn checkbox(on: bool) -> &'static str {
        if on {
            "[x] "
        } else {
            "[ ] "
        }
    }
}

impl Popup for FormPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered(area, 76, self.height().min(area.height));
        f.render_widget(Clear, r);
        let block = dialog(format!(" {} ", self.title), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let cur = self.current();
        let text_h = if self.text.is_some() { 3 } else { 0 };
        let list_h = if self.items.is_empty() {
            0
        } else {
            self.list_rows() as u16 + 2
        };
        let opt_h = (self.options.len() + usize::from(!self.options.is_empty())) as u16;
        let [text_a, list_a, opt_a, _, foot_a] = Layout::vertical([
            Constraint::Length(text_h),
            Constraint::Length(list_h),
            Constraint::Length(opt_h),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .areas(inner);
        if let Some((label, input)) = &self.text {
            let [l, field] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(text_a);
            f.render_widget(Span::styled(label.as_str(), t.dimmed()), l);
            let focused = cur == Some(Row::Text);
            let [mark, inp] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(field);
            f.render_widget(
                Span::styled(if focused { "› " } else { "  " }, t.key()),
                mark,
            );
            input.render(f, inp, t.text(), focused);
        }
        if !self.items.is_empty() {
            let [l, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(list_a);
            let checked = self.items.iter().filter(|i| i.checked).count();
            f.render_widget(
                Line::from(vec![
                    Span::styled(self.list_label.clone(), t.dimmed()),
                    Span::styled(format!("  {checked}/{}", self.items.len()), t.dimmed()),
                ]),
                l,
            );
            let sel_item = match cur {
                Some(Row::Item(i)) => Some(i),
                _ => None,
            };
            if let Some(i) = sel_item {
                self.list_offset = scroll_to_keep_visible(i, self.list_offset, self.list_rows());
            }
            let w = list.width as usize;
            let lines: Vec<Line> = self
                .items
                .iter()
                .enumerate()
                .skip(self.list_offset)
                .take(self.list_rows())
                .map(|(i, it)| {
                    let base = if sel_item == Some(i) {
                        t.selected()
                    } else {
                        Style::default()
                    };
                    let text = crate::util::truncate(&it.label, w.saturating_sub(8));
                    Line::from(vec![
                        Span::styled("  ", base),
                        Span::styled(Self::checkbox(it.checked), base.patch(t.key())),
                        Span::styled(
                            format!("{} ", it.badge),
                            base.patch(badge_style(it.badge, ctx)),
                        ),
                        Span::styled(text, base.patch(t.text())),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(lines), list);
        }
        if !self.options.is_empty() {
            let rows =
                Layout::vertical(vec![Constraint::Length(1); self.options.len() + 1]).split(opt_a);
            for (i, o) in self.options.iter().enumerate() {
                let focused = cur == Some(Row::Opt(i));
                let base = if focused {
                    t.selected()
                } else {
                    Style::default()
                };
                f.render_widget(
                    Line::from(vec![
                        Span::styled("  ", base),
                        Span::styled(Self::checkbox(o.on), base.patch(t.key())),
                        Span::styled(o.label.clone(), base.patch(t.text())),
                        Span::styled(format!("  Alt+{}", o.key), base.patch(t.dimmed())),
                    ]),
                    rows[i + 1],
                );
            }
        }
        let [err, keys] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(foot_a);
        if let Some(e) = &self.error {
            f.render_widget(Span::styled(e.as_str(), Style::default().fg(t.error)), err);
        }
        let mut spans = vec![
            Span::styled("Enter", t.key()),
            Span::styled(format!(" {}   ", self.submit_label), t.dimmed()),
            Span::styled("Space", t.key()),
            Span::styled(" toggle   ", t.dimmed()),
        ];
        if !self.items.is_empty() {
            spans.push(Span::styled("a", t.key()));
            spans.push(Span::styled(" all   ", t.dimmed()));
        }
        spans.extend([
            Span::styled("Tab", t.key()),
            Span::styled(" section   ", t.dimmed()),
            Span::styled("Esc", t.key()),
            Span::styled(" cancel", t.dimmed()),
        ]);
        f.render_widget(Line::from(spans), keys);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        self.error = None;
        let alt = key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::CONTROL);
        if let (true, KeyCode::Char(c)) = (alt, key.code) {
            self.toggle_option_key(c);
            return Action::None;
        }
        match key.code {
            KeyCode::Esc => return Action::ClosePopup,
            KeyCode::Enter => return self.submit(ctx),
            KeyCode::Up => self.move_row(-1),
            KeyCode::Down => self.move_row(1),
            KeyCode::Tab => self.jump_section(true),
            KeyCode::BackTab => self.jump_section(false),
            _ => {
                let cur = self.current();
                if cur == Some(Row::Text) {
                    if let Some((_, input)) = self.text.as_mut() {
                        input.handle_key(&key);
                    }
                    return Action::None;
                }
                match key.code {
                    KeyCode::Char(' ') => {
                        if let Some(r) = cur {
                            self.toggle(r);
                        }
                    }
                    KeyCode::Char('j') => self.move_row(1),
                    KeyCode::Char('k') => self.move_row(-1),
                    KeyCode::Char('a') if matches!(cur, Some(Row::Item(_))) => self.toggle_all(),
                    KeyCode::Char(c) => {
                        self.toggle_option_key(c);
                    }
                    KeyCode::PageDown => self.move_row(MAX_LIST_ROWS as isize),
                    KeyCode::PageUp => self.move_row(-(MAX_LIST_ROWS as isize)),
                    _ => {}
                }
            }
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        let mut v = vec![
            ("Enter", "confirm"),
            ("↑↓", "move"),
            ("Space", "toggle"),
            ("Tab", "next section"),
        ];
        if !self.items.is_empty() {
            v.push(("a", "toggle all files"));
        }
        if !self.options.is_empty() {
            v.push(("Alt+key", "toggle option"));
        }
        v.push(("Esc", "cancel"));
        v
    }
}

/// Color for a status letter.
pub fn badge_style(c: char, ctx: &Ctx) -> Style {
    let t = &ctx.theme;
    let color = match c {
        'A' => t.added,
        'D' => t.removed,
        'R' | 'C' => t.renamed,
        '?' => t.untracked,
        'U' => t.conflict,
        _ => t.modified,
    };
    Style::default().fg(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::temp_repo;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn items(n: usize) -> Vec<CheckItem> {
        (0..n)
            .map(|i| CheckItem {
                label: format!("f{i}"),
                badge: 'M',
                checked: true,
            })
            .collect()
    }

    #[test]
    fn navigation_toggles_and_submit() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let got: Rc<RefCell<Option<FormValues>>> = Rc::default();
        let sink = got.clone();
        let mut p = FormPopup::new("T", "Go", move |v, _| {
            *sink.borrow_mut() = Some(v);
            Action::None
        })
        .text("Name", "x", true)
        .checklist("Files", items(3))
        .option("Keep", 'w', false);
        // Typing goes into the text field, including letters that are accelerators elsewhere.
        p.handle_key(key(KeyCode::Char('w')), &ctx);
        assert_eq!(p.values().text, "xw");
        assert!(!p.values().options[0]);
        // Tab → checklist, Space unchecks the first file.
        p.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(p.current(), Some(Row::Item(0)));
        p.handle_key(key(KeyCode::Char(' ')), &ctx);
        p.handle_key(key(KeyCode::Char('j')), &ctx);
        assert_eq!(p.current(), Some(Row::Item(1)));
        // Plain accelerator outside the text field, Alt accelerator anywhere.
        p.handle_key(key(KeyCode::Char('w')), &ctx);
        assert!(p.values().options[0]);
        p.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT), &ctx);
        assert!(!p.values().options[0]);
        // Tab cycles sections: options, then back to text.
        p.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(p.current(), Some(Row::Opt(0)));
        p.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(p.current(), Some(Row::Text));
        p.handle_key(key(KeyCode::BackTab), &ctx);
        assert_eq!(p.current(), Some(Row::Opt(0)));
        p.handle_key(key(KeyCode::Down), &ctx);
        assert_eq!(p.current(), Some(Row::Text));
        let a = p.handle_key(key(KeyCode::Enter), &ctx);
        assert!(!a.is_none());
        let v = got.borrow().clone().unwrap();
        assert_eq!(v.text, "xw");
        assert_eq!(v.checked, vec![false, true, true]);
        assert_eq!(v.options, vec![false]);
    }

    #[test]
    fn validation() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let mut p = FormPopup::new("T", "Go", |_, _| Action::None)
            .text("Name", "", true)
            .checklist("Files", items(2));
        assert!(p.handle_key(key(KeyCode::Enter), &ctx).is_none());
        assert!(p.error.is_some());
        p.handle_key(key(KeyCode::Char('n')), &ctx);
        p.handle_key(key(KeyCode::Tab), &ctx);
        p.handle_key(key(KeyCode::Char('a')), &ctx);
        assert_eq!(p.values().checked, vec![false, false]);
        assert!(p.handle_key(key(KeyCode::Enter), &ctx).is_none());
        p.handle_key(key(KeyCode::Char('a')), &ctx);
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter), &ctx),
            Action::Batch(_)
        ));
    }

    #[test]
    fn options_only_form() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let mut p = FormPopup::new("T", "Go", |_, _| Action::None)
            .option("A", 'a', false)
            .option("B", 'b', true);
        p.handle_key(key(KeyCode::Char(' ')), &ctx);
        p.handle_key(key(KeyCode::Char('b')), &ctx);
        assert_eq!(p.values().options, vec![true, false]);
        p.handle_key(key(KeyCode::Up), &ctx);
        assert_eq!(p.current(), Some(Row::Opt(1)));
        assert!(p.height() >= 6);
    }
}
