//! "Compare with current": commits only in the selected branch and commits only in the current branch.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::branch;
use crate::git::log::Commit;
use crate::ui::diff_view::{self, DiffSource};
use crate::ui::widgets::{centered_pct, move_sel, panel, scroll_to_keep_visible};
use crate::ui::{KeyHelp, Popup};

use super::commit_line;

const MAX: usize = 1000;

#[derive(Default)]
struct Pane {
    commits: Vec<Commit>,
    sel: usize,
    offset: usize,
}

pub struct ComparePopup {
    branch: String,
    current: String,
    /// [only in branch, only in current]
    panes: [Pane; 2],
    focus: usize,
    error: Option<String>,
    height: usize,
}

impl ComparePopup {
    pub fn new(ctx: &Ctx, branch: &str) -> Self {
        let mut p = Self {
            branch: branch.to_string(),
            current: ctx.repo.head_branch().unwrap_or_else(|| "HEAD".into()),
            panes: Default::default(),
            focus: 0,
            error: None,
            height: 10,
        };
        p.load(ctx);
        p
    }

    fn load(&mut self, ctx: &Ctx) {
        let repo = &ctx.repo;
        let r = branch::commits_between(repo, "HEAD", &self.branch, MAX)
            .and_then(|a| Ok((a, branch::commits_between(repo, &self.branch, "HEAD", MAX)?)));
        match r {
            Ok((a, b)) => {
                for (pane, commits) in self.panes.iter_mut().zip([a, b]) {
                    pane.sel = pane.sel.min(commits.len().saturating_sub(1));
                    pane.commits = commits;
                }
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn draw_pane(&mut self, f: &mut Frame, area: Rect, i: usize, ctx: &Ctx) {
        let t = &ctx.theme;
        let (title, empty) = if i == 0 {
            (
                format!(
                    " In {} but not in {} ({}) ",
                    self.branch,
                    self.current,
                    self.panes[0].commits.len()
                ),
                format!("{} has no commits that {} lacks", self.branch, self.current),
            )
        } else {
            (
                format!(
                    " In {} but not in {} ({}) ",
                    self.current,
                    self.branch,
                    self.panes[1].commits.len()
                ),
                format!("{} has no commits that {} lacks", self.current, self.branch),
            )
        };
        let focused = self.focus == i;
        let block = panel(title, focused, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let h = inner.height as usize;
        self.height = h.max(1);
        let pane = &mut self.panes[i];
        if pane.commits.is_empty() {
            f.render_widget(Paragraph::new(empty).style(t.dimmed()), inner);
            return;
        }
        pane.offset = scroll_to_keep_visible(pane.sel, pane.offset, h);
        let lines: Vec<Line> = pane
            .commits
            .iter()
            .enumerate()
            .skip(pane.offset)
            .take(h)
            .map(|(n, c)| {
                let line = commit_line(c, inner.width as usize, ctx);
                if n == pane.sel && focused {
                    line.style(t.selected())
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn selected(&self) -> Option<&Commit> {
        let p = &self.panes[self.focus];
        p.commits.get(p.sel)
    }
}

impl Popup for ComparePopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered_pct(area, 92, 88);
        f.render_widget(Clear, r);
        let block = crate::ui::widgets::dialog(
            format!(" Compare {} with {} ", self.branch, self.current),
            t,
        );
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [body, keys] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
        if let Some(e) = &self.error {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                body,
            );
        } else {
            let [a, b] = if body.width >= 120 {
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body)
            } else {
                Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body)
            };
            self.draw_pane(f, a, 0, ctx);
            self.draw_pane(f, b, 1, ctx);
        }
        let hint = |k: &'static str, d: &'static str| {
            [Span::styled(k, t.key()), Span::styled(d, t.dimmed())]
        };
        let spans: Vec<Span> = [
            hint("Enter", " show commit   "),
            hint("d", " diff (merge-base → branch)   "),
            hint("Tab", " switch list   "),
            hint("Esc", " close"),
        ]
        .into_iter()
        .flatten()
        .collect();
        f.render_widget(Line::from(spans), keys);
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let page = self.height as isize;
        let pane = &mut self.panes[self.focus];
        let len = pane.commits.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::ClosePopup,
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l') => self.focus = 1 - self.focus,
            KeyCode::Up | KeyCode::Char('k') => pane.sel = move_sel(pane.sel, -1, len),
            KeyCode::Down | KeyCode::Char('j') => pane.sel = move_sel(pane.sel, 1, len),
            KeyCode::PageUp => pane.sel = move_sel(pane.sel, -page, len),
            KeyCode::PageDown => pane.sel = move_sel(pane.sel, page, len),
            KeyCode::Home | KeyCode::Char('g') => pane.sel = 0,
            KeyCode::End | KeyCode::Char('G') => pane.sel = len.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(c) = self.selected() {
                    return diff_view::open(
                        format!("{} {}", c.short, c.subject),
                        DiffSource::Commit {
                            rev: c.id.clone(),
                            paths: Vec::new(),
                        },
                    );
                }
            }
            KeyCode::Char('d') => {
                return match branch::merge_base(&ctx.repo, "HEAD", &self.branch) {
                    Ok(base) => diff_view::open(
                        format!(
                            "Changes on {} since it diverged from {}",
                            self.branch, self.current
                        ),
                        DiffSource::Range {
                            from: base,
                            to: self.branch.clone(),
                            paths: Vec::new(),
                        },
                    ),
                    Err(e) => Action::Error(format!("{e:#}")),
                };
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => {}
        }
        Action::None
    }

    fn refresh(&mut self, ctx: &Ctx) {
        self.load(ctx);
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("↑↓/jk", "move"),
            ("Enter", "show commit"),
            ("d", "diff from merge-base"),
            ("Tab", "switch list"),
            ("r", "reload"),
            ("Esc", "close"),
        ]
    }
}
