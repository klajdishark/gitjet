//! Filterable chooser of branches and tags whose first row always accepts the typed text, so a
//! revision expression that matches no ref (`HEAD~3`, a hash) can be compared directly.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::compare::RefEntry;
use crate::ui::popups::fuzzy_match;
use crate::ui::widgets::{centered, dialog, move_sel, TextInput};
use crate::ui::{KeyHelp, Popup};

/// What a row of the chooser does when picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Ask for a revision in a separate input dialog (shown while the filter is empty).
    EnterRevision,
    /// Compare with this revision (a listed ref or the typed text).
    Rev(String),
}

/// The chooser popup. `on_pick` turns the choice into the next dialog.
pub struct RefChooser {
    title: String,
    refs: Vec<RefEntry>,
    filter: TextInput,
    sel: usize,
    on_pick: fn(&Ctx, Choice) -> Action,
}

impl RefChooser {
    pub fn new(
        title: impl Into<String>,
        refs: Vec<RefEntry>,
        on_pick: fn(&Ctx, Choice) -> Action,
    ) -> Self {
        Self {
            title: title.into(),
            refs,
            filter: TextInput::default(),
            sel: 0,
            on_pick,
        }
    }

    /// The text typed so far, trimmed.
    fn query(&self) -> String {
        self.filter.value().trim().to_string()
    }

    /// Indices of refs matching the filter.
    fn matches(&self) -> Vec<usize> {
        matching_refs(&self.refs, &self.query())
    }

    /// The choice behind visible row `row` (row 0 is the free-text row).
    fn choice(&self, row: usize, matches: &[usize]) -> Option<Choice> {
        if row == 0 {
            let q = self.query();
            return Some(if q.is_empty() {
                Choice::EnterRevision
            } else {
                Choice::Rev(q)
            });
        }
        matches
            .get(row - 1)
            .map(|&i| Choice::Rev(self.refs[i].name.clone()))
    }

    fn first_row_label(&self) -> String {
        let q = self.query();
        if q.is_empty() {
            "Enter branch, tag or revision…".into()
        } else {
            format!("Compare with \"{q}\"")
        }
    }
}

/// Indices of `refs` whose name fuzzy-matches `query` (all of them for an empty query).
pub fn matching_refs(refs: &[RefEntry], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    (0..refs.len())
        .filter(|&i| q.is_empty() || fuzzy_match(&refs[i].name.to_lowercase(), &q))
        .collect()
}

/// Row to select after the filter changes: the first matching ref, or the free-text row.
pub fn default_row(query: &str, match_count: usize) -> usize {
    usize::from(!query.trim().is_empty() && match_count > 0)
}

impl Popup for RefChooser {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let matches = self.matches();
        let rows = matches.len() + 1;
        self.sel = self.sel.min(rows - 1);
        let width = self
            .refs
            .iter()
            .map(|r| r.name.chars().count() + r.kind.label().len() + 6)
            .chain([self.first_row_label().chars().count() + 6])
            .max()
            .unwrap_or(40)
            .clamp(44, 80) as u16;
        let height = (rows as u16 + 4).clamp(6, 24);
        let r = centered(area, width, height);
        f.render_widget(Clear, r);
        let block = dialog(format!(" {} ", self.title), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [input_area, _, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(inner);
        let [p, inp] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(input_area);
        f.render_widget(Span::styled("> ", t.key()), p);
        self.filter.render(f, inp, t.text(), true);
        let w = list_area.width as usize;
        let row = |label: String, key: &str, style| {
            let pad = w.saturating_sub(label.chars().count() + key.chars().count() + 2);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {label}"), style),
                Span::raw(" ".repeat(pad)),
                Span::styled(format!("{key} "), t.key().remove_modifier(Modifier::BOLD)),
            ]))
        };
        let mut items = vec![row(self.first_row_label(), "type", t.text())];
        items.extend(matches.iter().map(|&i| {
            row(
                self.refs[i].name.clone(),
                self.refs[i].kind.label(),
                t.text(),
            )
        }));
        let mut state = ListState::default().with_selected(Some(self.sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(t.selected()),
            list_area,
            &mut state,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let matches = self.matches();
        let rows = matches.len() + 1;
        match key.code {
            KeyCode::Esc => return Action::ClosePopup,
            KeyCode::Up => self.sel = move_sel(self.sel, -1, rows),
            KeyCode::Down => self.sel = move_sel(self.sel, 1, rows),
            KeyCode::PageUp => self.sel = move_sel(self.sel, -10, rows),
            KeyCode::PageDown => self.sel = move_sel(self.sel, 10, rows),
            KeyCode::Enter => {
                if let Some(c) = self.choice(self.sel, &matches) {
                    return Action::ClosePopup.then((self.on_pick)(ctx, c));
                }
            }
            _ => {
                if self.filter.handle_key(&key) {
                    self.sel = default_row(&self.filter.value(), self.matches().len());
                }
            }
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("type", "filter / enter a revision"),
            ("↑↓", "move"),
            ("Enter", "compare"),
            ("Esc", "close"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::compare::RefKind;
    use crate::git::testutil::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn entry(name: &str, kind: RefKind) -> RefEntry {
        RefEntry {
            name: name.into(),
            full: format!("refs/x/{name}"),
            kind,
        }
    }

    fn picked(_: &Ctx, c: Choice) -> Action {
        match c {
            Choice::EnterRevision => Action::Toast("enter".into()),
            Choice::Rev(r) => Action::Toast(format!("rev {r}")),
        }
    }

    fn toast_of(a: Action) -> Option<String> {
        match a {
            Action::Toast(s) => Some(s),
            Action::Batch(v) => v.into_iter().find_map(toast_of),
            _ => None,
        }
    }

    fn typed(c: &mut RefChooser, s: &str, ctx: &Ctx) {
        for ch in s.chars() {
            c.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), ctx);
        }
    }

    #[test]
    fn matching_and_default_row() {
        let refs = vec![
            entry("main", RefKind::Local),
            entry("origin/feature", RefKind::Remote),
        ];
        assert_eq!(matching_refs(&refs, ""), vec![0, 1]);
        assert_eq!(matching_refs(&refs, "feat"), vec![1]);
        assert!(matching_refs(&refs, "HEAD~3").is_empty());
        assert_eq!(default_row("", 2), 0);
        assert_eq!(default_row("feat", 1), 1);
        assert_eq!(default_row("HEAD~3", 0), 0);
    }

    #[test]
    fn typed_revision_stays_selectable() {
        let (_d, repo) = temp_repo();
        let ctx = Ctx::new(repo, Config::default());
        let refs = vec![entry("main", RefKind::Local), entry("v1.0", RefKind::Tag)];
        let mut c = RefChooser::new("Compare", refs.clone(), picked);
        let enter = c.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx);
        assert_eq!(toast_of(enter).as_deref(), Some("enter"));
        // A filter that matches a ref selects it.
        let mut c = RefChooser::new("Compare", refs.clone(), picked);
        typed(&mut c, "v1", &ctx);
        let a = c.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx);
        assert_eq!(toast_of(a).as_deref(), Some("rev v1.0"));
        // An expression that matches nothing compares with the typed text.
        let mut c = RefChooser::new("Compare", refs, picked);
        typed(&mut c, "HEAD~3", &ctx);
        assert_eq!(c.first_row_label(), "Compare with \"HEAD~3\"");
        let a = c.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctx);
        assert_eq!(toast_of(a).as_deref(), Some("rev HEAD~3"));
    }
}
