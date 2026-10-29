//! File-list helpers shared by the tab and the "Apply patch" preview popup.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::diff::{parse_unified, ChangeKind, FileDiff};
use crate::git::patch::{self, PatchFormat};
use crate::ui::diff_view::{diff_height, draw_diff};
use crate::ui::widgets::{centered_pct, move_sel, panel, scroll_to_keep_visible};
use crate::ui::{KeyHelp, Popup};

use super::form::badge_style;

/// True for diffs of untracked files (stash third parent or shelved copies).
pub fn is_untracked(d: &FileDiff) -> bool {
    d.header
        .first()
        .is_some_and(|h| h == "untracked file" || h == "new file (untracked)")
}

/// Status letter for a diff.
pub fn file_letter(d: &FileDiff) -> char {
    if is_untracked(d) {
        return '?';
    }
    match d.kind {
        ChangeKind::Modified => 'M',
        ChangeKind::Added => 'A',
        ChangeKind::Deleted => 'D',
        ChangeKind::Renamed => 'R',
        ChangeKind::Copied => 'C',
        ChangeKind::ModeChanged => 'T',
    }
}

/// Display name: `old → new` for renames/copies.
pub fn file_label(d: &FileDiff) -> String {
    match (&d.old_path, &d.new_path, d.kind) {
        (Some(o), Some(n), ChangeKind::Renamed | ChangeKind::Copied) if o != n => {
            format!("{o} → {n}")
        }
        _ => d.path().to_string(),
    }
}

/// One file row: optional checkbox, status letter, path, +/- counts.
pub fn file_line(
    d: &FileDiff,
    width: usize,
    mark: Option<bool>,
    dim: bool,
    ctx: &Ctx,
) -> Line<'static> {
    let t = &ctx.theme;
    let letter = file_letter(d);
    let counts = if d.binary {
        " bin".to_string()
    } else {
        format!(" +{} -{}", d.added_count(), d.removed_count())
    };
    let prefix = match mark {
        Some(true) => "[x] ",
        Some(false) => "[ ] ",
        None => "",
    };
    let fixed = prefix.chars().count() + 2 + counts.chars().count();
    let name = crate::util::fit(&file_label(d), width.saturating_sub(fixed));
    let text_style = if dim { t.dimmed() } else { t.text() };
    Line::from(vec![
        Span::styled(prefix.to_string(), t.key()),
        Span::styled(
            format!("{letter} "),
            if dim {
                t.dimmed()
            } else {
                badge_style(letter, ctx)
            },
        ),
        Span::styled(name, text_style),
        Span::styled(counts, t.dimmed()),
    ])
}

/// "Apply patch" preview: file list on the left, selected file's diff on the right.
/// Enter applies the patch, Esc cancels.
pub struct PatchPreviewPopup {
    file: PathBuf,
    format: PatchFormat,
    subjects: Vec<String>,
    diffs: Vec<FileDiff>,
    sel: usize,
    offset: usize,
    scroll: usize,
    page: usize,
}

impl PatchPreviewPopup {
    /// Read and parse a patch file.
    pub fn load(file: PathBuf) -> Result<Self> {
        let text = patch::read_patch(&file)?;
        let format = patch::detect_format(&text);
        Ok(Self {
            subjects: patch::mailbox_subjects(&text),
            diffs: parse_unified(&text),
            format,
            file,
            sel: 0,
            offset: 0,
            scroll: 0,
            page: 10,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.diffs.is_empty()
    }

    fn select(&mut self, delta: isize) {
        let n = move_sel(self.sel, delta, self.diffs.len());
        if n != self.sel {
            self.sel = n;
            self.scroll = 0;
        }
    }

    fn summary(&self) -> String {
        let files = self.diffs.len();
        let f = format!("{files} file{}", if files == 1 { "" } else { "s" });
        match self.format {
            PatchFormat::Mailbox => {
                let n = self.subjects.len().max(1);
                format!(
                    "Mailbox with {n} commit{} ({f}) — applied with git am --3way",
                    if n == 1 { "" } else { "s" }
                )
            }
            PatchFormat::Diff => {
                format!("Diff of {f} — applied to the work tree (3-way merge fallback)")
            }
        }
    }
}

impl Popup for PatchPreviewPopup {
    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let r = centered_pct(area, 92, 90);
        f.render_widget(Clear, r);
        let block =
            crate::ui::widgets::dialog(format!(" Apply Patch: {} ", self.file.display()), t);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let subj_h = self.subjects.len().min(3) as u16;
        let [head, subjects, body, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(subj_h),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(Span::styled(self.summary(), t.title()), head);
        let subj: Vec<Line> = self
            .subjects
            .iter()
            .take(3)
            .map(|s| Line::styled(format!("  {s}"), t.text()))
            .collect();
        f.render_widget(Paragraph::new(subj), subjects);
        let [list_a, diff_a] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
                .areas(body);
        let lb = panel(" Files ", true, t);
        let li = lb.inner(list_a);
        f.render_widget(lb, list_a);
        self.offset = scroll_to_keep_visible(self.sel, self.offset, li.height as usize);
        let lines: Vec<Line> = self
            .diffs
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(li.height as usize)
            .map(|(i, d)| {
                let l = file_line(d, li.width as usize, None, false, ctx);
                if i == self.sel {
                    l.style(t.selected())
                } else {
                    l
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), li);
        let db = panel(" Diff ", false, t);
        let di = db.inner(diff_a);
        f.render_widget(db, diff_a);
        self.page = di.height.max(1) as usize;
        if let Some(d) = self.diffs.get(self.sel) {
            let one = std::slice::from_ref(d);
            self.scroll = self.scroll.min(diff_height(one).saturating_sub(1));
            draw_diff(f, di, one, self.scroll, ctx);
        }
        f.render_widget(
            Line::from(vec![
                Span::styled("Enter", t.key()),
                Span::styled(" apply   ", t.dimmed()),
                Span::styled("j/k", t.key()),
                Span::styled(" file   ", t.dimmed()),
                Span::styled("PgUp/PgDn J/K", t.key()),
                Span::styled(" scroll diff   ", t.dimmed()),
                Span::styled("Esc", t.key()),
                Span::styled(" cancel", Style::default().fg(t.dim)),
            ]),
            keys,
        );
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::ClosePopup,
            KeyCode::Enter | KeyCode::Char('y') => {
                return Action::ClosePopup
                    .then(super::actions::apply_patch_file(&ctx.repo, &self.file));
            }
            KeyCode::Up | KeyCode::Char('k') => self.select(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select(1),
            KeyCode::Home | KeyCode::Char('g') => self.select(-(self.diffs.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.select(self.diffs.len() as isize),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll += self.page,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page),
            KeyCode::Char('J') => self.scroll += 1,
            KeyCode::Char('K') => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
        Action::None
    }

    fn help(&self) -> Vec<KeyHelp> {
        vec![
            ("Enter", "apply"),
            ("j/k", "select file"),
            ("PgUp/PgDn", "scroll diff"),
            ("J/K", "scroll line"),
            ("Esc", "cancel"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::parse_unified;

    #[test]
    fn letters_and_labels() {
        let d = parse_unified(
            "diff --git a/o b/n\nsimilarity index 90%\nrename from o\nrename to n\n--- a/o\n+++ b/n\n@@ -1 +1 @@\n-a\n+b\n\
diff --git a/x b/x\ndeleted file mode 100644\n--- a/x\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
        );
        assert_eq!(file_letter(&d[0]), 'R');
        assert_eq!(file_label(&d[0]), "o → n");
        assert_eq!(file_letter(&d[1]), 'D');
        assert_eq!(file_label(&d[1]), "x");
        let u = patch::synthetic_added_diff("u", b"x\n");
        assert!(is_untracked(&u));
        assert_eq!(file_letter(&u), '?');
    }

    #[test]
    fn preview_loads_and_navigates() {
        let (_d, repo) = crate::git::testutil::temp_repo();
        let file = repo.git_dir.join("p.patch");
        let text = "From: A <a@b>\nSubject: [PATCH] one\n\n---\ndiff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-1\n+2\n\
diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-1\n+2\n";
        patch::write_patch(&file, text).unwrap();
        let mut p = PatchPreviewPopup::load(file).unwrap();
        assert!(!p.is_empty());
        assert_eq!(p.format, PatchFormat::Mailbox);
        assert!(p.summary().contains("2 files"));
        p.scroll = 4;
        p.select(1);
        assert_eq!((p.sel, p.scroll), (1, 0));
        p.select(5);
        assert_eq!(p.sel, 1);
    }
}
