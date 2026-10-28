//! Repository tab (9): tags, remotes, worktrees, submodules, ignore rules and config, plus the
//! repository switcher and Clone / Init.
//!
//! Left: section menu. Right: the section's list with a details pane. Every operation is in the `.`
//! menu; its single-key accelerators work directly in the list.

mod actions;
mod switcher;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::action::Action;
use crate::ctx::Ctx;
use crate::git::admin::{
    self, ConfigEntry, IgnoreRule, Remote, Submodule, SubmoduleState, Tag, Worktree,
};
use crate::theme::Theme;
use crate::ui::popups::MenuPopup;
use crate::ui::widgets::{move_sel, panel, scroll_to_keep_visible, TextInput};
use crate::ui::{KeyHelp, PaletteItem, View};
use crate::util::{fit, format_date, format_time, truncate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Tags,
    Remotes,
    Worktrees,
    Submodules,
    Ignore,
    Config,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::Tags,
        Section::Remotes,
        Section::Worktrees,
        Section::Submodules,
        Section::Ignore,
        Section::Config,
    ];

    fn title(self) -> &'static str {
        match self {
            Section::Tags => "Tags",
            Section::Remotes => "Remotes",
            Section::Worktrees => "Worktrees",
            Section::Submodules => "Submodules",
            Section::Ignore => "Ignore rules",
            Section::Config => "Config",
        }
    }

    fn index(self) -> usize {
        Section::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// Everything shown in the tab, loaded on refresh.
#[derive(Default)]
struct Data {
    tags: Vec<Tag>,
    remotes: Vec<Remote>,
    worktrees: Vec<Worktree>,
    submodules: Vec<Submodule>,
    ignore: Vec<IgnoreRule>,
    config: Vec<ConfigEntry>,
    /// Current branch, "detached HEAD" or "no commits" (computed on refresh, not per frame)
    head: String,
    /// Index into `worktrees` of the work tree this view is showing
    current_worktree: Option<usize>,
    /// Per-section load errors (shown instead of the list)
    errors: [Option<String>; 6],
}

impl Data {
    fn load(repo: &crate::git::Repo) -> Data {
        let mut errors: [Option<String>; 6] = Default::default();
        fn grab<T>(
            errors: &mut [Option<String>; 6],
            s: Section,
            r: anyhow::Result<Vec<T>>,
        ) -> Vec<T> {
            r.unwrap_or_else(|e| {
                errors[s.index()] = Some(format!("{e:#}"));
                Vec::new()
            })
        }
        let tags = grab(&mut errors, Section::Tags, admin::list_tags(repo));
        let remotes = grab(&mut errors, Section::Remotes, admin::list_remotes(repo));
        let worktrees = grab(&mut errors, Section::Worktrees, admin::list_worktrees(repo));
        let submodules = grab(
            &mut errors,
            Section::Submodules,
            admin::list_submodules(repo),
        );
        let current_worktree = worktrees
            .iter()
            .position(|w| admin::same_path(&w.path, &repo.root));
        Data {
            head: head_label(repo),
            current_worktree,
            tags,
            remotes,
            worktrees,
            submodules,
            ignore: admin::ignore_rules(repo),
            config: admin::config_entries(repo),
            errors,
        }
    }

    fn len(&self, s: Section) -> usize {
        match s {
            Section::Tags => self.tags.len(),
            Section::Remotes => self.remotes.len(),
            Section::Worktrees => self.worktrees.len(),
            Section::Submodules => self.submodules.len(),
            Section::Ignore => self.ignore.len(),
            Section::Config => self.config.len(),
        }
    }

    /// Text the filter matches against.
    fn search_text(&self, s: Section, i: usize) -> String {
        match s {
            Section::Tags => {
                let t = &self.tags[i];
                format!("{} {} {}", t.name, t.target, t.subject)
            }
            Section::Remotes => {
                let r = &self.remotes[i];
                format!("{} {} {}", r.name, r.fetch_url, r.push_urls.join(" "))
            }
            Section::Worktrees => {
                let w = &self.worktrees[i];
                format!("{} {}", w.path.display(), w.branch.as_deref().unwrap_or(""))
            }
            Section::Submodules => {
                let s = &self.submodules[i];
                format!("{} {}", s.path, s.url.as_deref().unwrap_or(""))
            }
            Section::Ignore => {
                let r = &self.ignore[i];
                format!("{} {}", r.pattern, r.file.label())
            }
            Section::Config => {
                let c = &self.config[i];
                format!("{} {}", c.key, c.effective.as_deref().unwrap_or(""))
            }
        }
    }
}

/// One-line explanation of a config key (dialog prompt and details pane).
fn config_hint(key: &str) -> &'static str {
    match key {
        "user.name" => "Author name recorded in commits",
        "user.email" => "Author e-mail recorded in commits",
        "core.autocrlf" => "Line ending conversion: true (Windows), input (macOS/Linux) or false",
        "pull.rebase" => "true: pull rebases local commits; false: pull merges",
        "commit.gpgsign" => "true: sign every commit (GPG/SSH)",
        "init.defaultBranch" => "Branch name used by git init",
        "core.editor" => "Editor git opens for messages",
        _ => "",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sections,
    Content,
}

pub struct RepositoryView {
    data: Data,
    loader: crate::ui::bg::Background<Data>,
    section: Section,
    focus: Focus,
    sel: [usize; 6],
    offset: [usize; 6],
    filter: TextInput,
    filtering: bool,
}

impl RepositoryView {
    /// Apply a finished refresh.
    fn apply_loaded(&mut self) {
        if let Some(data) = self.loader.poll() {
            self.data = data;
            self.clamp_selection();
        }
    }

    pub fn new(ctx: &Ctx) -> Self {
        // Runs at startup and after every repository switch: keeps the recent list current.
        let _ = admin::record_recent(&ctx.repo.root);
        Self {
            data: Data::default(),
            loader: crate::ui::bg::Background::new(),
            section: Section::Tags,
            focus: Focus::Sections,
            sel: [0; 6],
            offset: [0; 6],
            filter: TextInput::default(),
            filtering: false,
        }
    }

    fn visible(&self) -> Vec<usize> {
        let q = self.filter.value().to_lowercase();
        (0..self.data.len(self.section))
            .filter(|&i| {
                q.is_empty()
                    || self
                        .data
                        .search_text(self.section, i)
                        .to_lowercase()
                        .contains(&q)
            })
            .collect()
    }

    /// Index (into the section's data) of the selected row.
    fn selected(&self) -> Option<usize> {
        self.visible().get(self.sel[self.section.index()]).copied()
    }

    fn set_section(&mut self, s: Section) {
        if s != self.section {
            self.section = s;
            self.filter.set("");
            self.filtering = false;
        }
    }

    fn move_content(&mut self, delta: isize) {
        let n = self.visible().len();
        let i = self.section.index();
        self.sel[i] = move_sel(self.sel[i], delta, n);
    }

    fn clamp_selection(&mut self) {
        for s in Section::ALL {
            let n = self.data.len(s);
            self.sel[s.index()] = self.sel[s.index()].min(n.saturating_sub(1));
        }
    }

    /// Run the menu item bound to `key` ("Enter" or a single character).
    fn run_accelerator(&self, key: &str, ctx: &Ctx) -> Option<Action> {
        let items = actions::menu_items(self.section, &self.data, self.selected(), ctx);
        let item = items.into_iter().find(|it| it.key == key)?;
        if !item.enabled {
            return Some(Action::None);
        }
        Some((item.run)(ctx))
    }

    fn context_menu(&self, ctx: &Ctx) -> Action {
        let items = actions::menu_items(self.section, &self.data, self.selected(), ctx);
        Action::popup(MenuPopup::new(self.section.title(), items))
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.filter.set("");
                self.filtering = false;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.filtering = false,
            _ => {
                if self.filter.handle_key(&key) {
                    self.sel[self.section.index()] = 0;
                }
            }
        }
        Action::None
    }

    fn handle_sections_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        let i = self.section.index();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.set_section(Section::ALL[move_sel(i, 1, 6)]),
            KeyCode::Up | KeyCode::Char('k') => self.set_section(Section::ALL[move_sel(i, -1, 6)]),
            KeyCode::Home | KeyCode::Char('g') => self.set_section(Section::ALL[0]),
            KeyCode::End | KeyCode::Char('G') => self.set_section(Section::ALL[5]),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Content,
            KeyCode::Char('/') => {
                self.focus = Focus::Content;
                self.filtering = true;
            }
            // Section accelerators (n, D, ...) work from the section menu too.
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                return self
                    .run_accelerator(&c.to_string(), ctx)
                    .unwrap_or(Action::None);
            }
            _ => {}
        }
        Action::None
    }

    fn handle_content_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.move_content(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_content(-1),
            KeyCode::PageDown => self.move_content(10),
            KeyCode::PageUp => self.move_content(-10),
            KeyCode::Home => self.sel[self.section.index()] = 0,
            KeyCode::End => self.move_content(isize::MAX / 2),
            KeyCode::Left => self.focus = Focus::Sections,
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    self.focus = Focus::Sections;
                } else {
                    self.filter.set("");
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Enter => {
                let a = self
                    .run_accelerator("Enter", ctx)
                    .or_else(|| self.run_accelerator("e", ctx));
                return a.unwrap_or(Action::None);
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                return self
                    .run_accelerator(&c.to_string(), ctx)
                    .unwrap_or(Action::None);
            }
            _ => {}
        }
        Action::None
    }

    // ----- drawing -----------------------------------------------------------------------------

    fn draw_sections(&self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let block = panel(" Repository ", self.focus == Focus::Sections, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let w = inner.width as usize;
        let mut lines: Vec<Line> = Section::ALL
            .iter()
            .map(|s| {
                let count = self.data.len(*s);
                let label = format!(" {}", s.title());
                let n = if *s == Section::Config {
                    String::new()
                } else {
                    count.to_string()
                };
                let pad = w.saturating_sub(label.width() + n.width() + 1);
                let line = Line::from(vec![
                    Span::styled(label, t.text()),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(n, t.dimmed()),
                ]);
                if *s == self.section {
                    line.patch_style(t.selected())
                } else {
                    line
                }
            })
            .collect();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(" {}", truncate(&ctx.repo.name(), w.saturating_sub(1))),
            t.title(),
        ));
        lines.push(Line::styled(
            format!(" {}", truncate(&self.data.head, w.saturating_sub(1))),
            Style::default().fg(t.local_branch),
        ));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" O", t.key()),
            Span::styled(" switch repo", t.dimmed()),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_content(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = &ctx.theme;
        let vis = self.visible();
        let title = format!(" {} ({}) ", self.section.title(), vis.len());
        let block = panel(title, self.focus == Focus::Content, t);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let show_filter = self.filtering || !self.filter.is_empty();
        let [filter_row, list] = Layout::vertical([
            Constraint::Length(u16::from(show_filter)),
            Constraint::Min(1),
        ])
        .areas(inner);
        if show_filter {
            let [p, inp] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(filter_row);
            f.render_widget(Span::styled("/ ", t.key()), p);
            self.filter.render(f, inp, t.text(), self.filtering);
        }
        if let Some(e) = &self.data.errors[self.section.index()] {
            f.render_widget(
                Paragraph::new(e.as_str()).style(Style::default().fg(t.error)),
                list,
            );
            return;
        }
        if vis.is_empty() {
            let msg = if self.data.len(self.section) == 0 {
                empty_message(self.section)
            } else {
                "Nothing matches the filter"
            };
            f.render_widget(Paragraph::new(msg).style(t.dimmed()), list);
            return;
        }
        let si = self.section.index();
        let h = list.height as usize;
        self.offset[si] = scroll_to_keep_visible(self.sel[si], self.offset[si], h);
        let w = list.width as usize;
        let widths = self.column_widths();
        let lines: Vec<Line> = vis
            .iter()
            .enumerate()
            .skip(self.offset[si])
            .take(h)
            .map(|(row, &i)| {
                let line = self.row_line(i, w, widths, ctx);
                if row == self.sel[si] {
                    line.patch_style(t.selected())
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }

    /// Width of the first column of the current section (names), capped.
    fn column_widths(&self) -> usize {
        let names: Vec<usize> = match self.section {
            Section::Tags => self.data.tags.iter().map(|t| t.name.width()).collect(),
            Section::Remotes => self.data.remotes.iter().map(|r| r.name.width()).collect(),
            Section::Submodules => self
                .data
                .submodules
                .iter()
                .map(|s| s.path.width() + 2 * s.depth)
                .collect(),
            Section::Config => self.data.config.iter().map(|c| c.key.width()).collect(),
            Section::Ignore => vec![17],
            Section::Worktrees => self
                .data
                .worktrees
                .iter()
                .map(|w| w.path.display().to_string().width())
                .collect(),
        };
        names.into_iter().max().unwrap_or(8).clamp(6, 48)
    }

    fn row_line(&self, i: usize, w: usize, c1: usize, ctx: &Ctx) -> Line<'static> {
        let t = &ctx.theme;
        let rest = |used: usize| w.saturating_sub(used);
        match self.section {
            Section::Tags => {
                let tag = &self.data.tags[i];
                let kind = if tag.annotated { "A" } else { " " };
                let used = c1 + 2 + 10 + 12 + 3;
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", fit(&tag.name, c1)),
                        Style::default().fg(t.tag),
                    ),
                    Span::styled(
                        format!("{:<10}", truncate(&tag.target, 9)),
                        Style::default().fg(t.hash),
                    ),
                    Span::styled(
                        format!("{:<12}", format_date(tag.date)),
                        Style::default().fg(t.date),
                    ),
                    Span::styled(format!("{kind}  "), t.key()),
                    Span::styled(
                        truncate(&tag.subject, rest(used)),
                        if tag.annotated { t.text() } else { t.dimmed() },
                    ),
                ])
            }
            Section::Remotes => {
                let r = &self.data.remotes[i];
                let push = r
                    .push_urls
                    .iter()
                    .filter(|u| **u != r.fetch_url)
                    .map(|u| format!("  push: {u}"))
                    .collect::<String>();
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", fit(&r.name, c1)),
                        Style::default().fg(t.remote_branch),
                    ),
                    Span::styled(
                        truncate(&format!("{}{push}", r.fetch_url), rest(c1 + 2)),
                        t.text(),
                    ),
                ])
            }
            Section::Worktrees => worktree_line(
                &self.data.worktrees[i],
                self.data.current_worktree == Some(i),
                w,
                ctx,
            ),
            Section::Submodules => submodule_line(&self.data.submodules[i], c1, w, t),
            Section::Ignore => {
                let r = &self.data.ignore[i];
                let src = format!("{}:{}", r.file.label(), r.line);
                let style = if r.pattern.starts_with('!') {
                    Style::default().fg(t.added)
                } else {
                    t.text()
                };
                Line::from(vec![
                    Span::styled(format!(" {} ", fit(&src, 22)), t.dimmed()),
                    Span::styled(truncate(&r.pattern, rest(24)), style),
                ])
            }
            Section::Config => config_line(&self.data.config[i], c1, w, t),
        }
    }

    fn details(&self, ctx: &Ctx) -> Vec<Line<'static>> {
        let t = &ctx.theme;
        let Some(i) = self.selected() else {
            return Vec::new();
        };
        let kv = |k: &str, v: String| {
            Line::from(vec![
                Span::styled(format!("{k:<10}"), t.dimmed()),
                Span::styled(v, t.text()),
            ])
        };
        match self.section {
            Section::Tags => {
                let x = &self.data.tags[i];
                vec![
                    kv(
                        "Tag",
                        format!(
                            "{} ({})",
                            x.name,
                            if x.annotated {
                                "annotated"
                            } else {
                                "lightweight"
                            }
                        ),
                    ),
                    kv("Commit", x.target.clone()),
                    kv("Date", format_time(x.date)),
                    kv(
                        if x.annotated { "Message" } else { "Subject" },
                        x.subject.clone(),
                    ),
                ]
            }
            Section::Remotes => {
                let r = &self.data.remotes[i];
                let mut v = vec![
                    kv("Remote", r.name.clone()),
                    kv("Fetch", r.fetch_url.clone()),
                ];
                for u in &r.push_urls {
                    v.push(kv("Push", u.clone()));
                }
                v
            }
            Section::Worktrees => {
                let w = &self.data.worktrees[i];
                let mut v = vec![
                    kv("Path", w.path.display().to_string()),
                    kv(
                        "Checkout",
                        match (&w.branch, w.bare) {
                            (_, true) => "bare repository".into(),
                            (Some(b), _) => format!("branch {b}"),
                            (None, _) => "detached HEAD".into(),
                        },
                    ),
                    kv("HEAD", w.head.clone().unwrap_or_else(|| "(none)".into())),
                ];
                if let Some(l) = &w.locked {
                    v.push(kv(
                        "Locked",
                        if l.is_empty() {
                            "yes".into()
                        } else {
                            l.clone()
                        },
                    ));
                }
                if let Some(p) = &w.prunable {
                    v.push(kv(
                        "Prunable",
                        if p.is_empty() {
                            "directory missing".into()
                        } else {
                            p.clone()
                        },
                    ));
                }
                v
            }
            Section::Submodules => {
                let s = &self.data.submodules[i];
                vec![
                    kv("Path", s.path.clone()),
                    kv(
                        "URL",
                        s.url
                            .clone()
                            .unwrap_or_else(|| "(nested; see its .gitmodules)".into()),
                    ),
                    kv(
                        "Commit",
                        format!("{} {}", s.commit, s.describe.clone().unwrap_or_default()),
                    ),
                    kv("State", s.state.label().to_string()),
                    kv(
                        "Branch",
                        s.branch.clone().unwrap_or_else(|| "(default)".into()),
                    ),
                ]
            }
            Section::Ignore => {
                let r = &self.data.ignore[i];
                vec![
                    kv("Pattern", r.pattern.clone()),
                    kv(
                        "File",
                        format!("{} line {}", r.file.path(&ctx.repo).display(), r.line),
                    ),
                    kv(
                        "Scope",
                        match r.file {
                            admin::IgnoreFile::GitIgnore => {
                                "committed and shared with everyone".into()
                            }
                            admin::IgnoreFile::Exclude => "this clone only (not committed)".into(),
                        },
                    ),
                ]
            }
            Section::Config => {
                let c = &self.data.config[i];
                let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "(not set)".into());
                vec![
                    kv("Key", c.key.clone()),
                    kv("About", config_hint(&c.key).to_string()),
                    kv("Repo", show(&c.local)),
                    kv("Global", show(&c.global)),
                    kv("Effective", show(&c.effective)),
                ]
            }
        }
    }
}

fn empty_message(s: Section) -> &'static str {
    match s {
        Section::Tags => "No tags. Press n to create one.",
        Section::Remotes => "No remotes. Press n to add one.",
        Section::Worktrees => "No worktrees.",
        Section::Submodules => "No submodules. Press n to add one.",
        Section::Ignore => {
            "No ignore rules. Press n to add a pattern to .gitignore, x for .git/info/exclude."
        }
        Section::Config => "",
    }
}

/// Label of HEAD for the section pane: the branch name, "detached HEAD" or "no commits".
fn head_label(repo: &crate::git::Repo) -> String {
    repo.head_branch().unwrap_or_else(|| {
        if repo.head_commit().is_some() {
            "detached HEAD".into()
        } else {
            "no commits".into()
        }
    })
}

fn worktree_line(w: &Worktree, current: bool, width: usize, ctx: &Ctx) -> Line<'static> {
    let t = &ctx.theme;
    let marker = if current { "● " } else { "  " };
    let what = match (&w.branch, w.bare) {
        (_, true) => "(bare)".to_string(),
        (Some(b), _) => format!("[{b}]"),
        (None, _) => format!(
            "(detached {})",
            w.head
                .as_deref()
                .map(|h| truncate(h, 8))
                .unwrap_or_default()
        ),
    };
    let mut flags = String::new();
    if w.is_main {
        flags.push_str(" (main worktree)");
    }
    if w.locked.is_some() {
        flags.push_str(" locked");
    }
    if w.prunable.is_some() {
        flags.push_str(" prunable");
    }
    let path = switcher::display_path(&w.path);
    let room = width.saturating_sub(what.width() + flags.width() + 5);
    Line::from(vec![
        Span::styled(format!(" {marker}"), Style::default().fg(t.head)),
        Span::styled(format!("{} ", truncate(&path, room)), t.text()),
        Span::styled(what, Style::default().fg(t.local_branch)),
        Span::styled(flags, Style::default().fg(t.warning)),
    ])
}

fn submodule_line(s: &Submodule, c1: usize, width: usize, t: &Theme) -> Line<'static> {
    let (sym, color) = match s.state {
        SubmoduleState::Uninitialized => ("-", t.dim),
        SubmoduleState::UpToDate => (" ", t.success),
        SubmoduleState::Modified => ("+", t.modified),
        SubmoduleState::Conflict => ("U", t.conflict),
    };
    let name = format!("{}{}", "  ".repeat(s.depth), s.path);
    let used = c1 + 4 + 10;
    Line::from(vec![
        Span::styled(format!(" {sym} "), Style::default().fg(color)),
        Span::styled(format!("{} ", fit(&name, c1)), t.text()),
        Span::styled(
            format!("{:<10}", truncate(&s.commit, 9)),
            Style::default().fg(t.hash),
        ),
        Span::styled(
            truncate(
                s.describe.as_deref().unwrap_or(s.state.label()),
                width.saturating_sub(used),
            ),
            t.dimmed(),
        ),
    ])
}

fn config_line(c: &ConfigEntry, c1: usize, width: usize, t: &Theme) -> Line<'static> {
    let val = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
    let col = width.saturating_sub(c1 + 3) / 3;
    Line::from(vec![
        Span::styled(format!(" {} ", fit(&c.key, c1)), t.key()),
        Span::styled(
            fit(&format!("repo: {}", val(&c.local)), col.saturating_sub(1)) + " ",
            if c.local.is_some() {
                t.text()
            } else {
                t.dimmed()
            },
        ),
        Span::styled(
            fit(
                &format!("global: {}", val(&c.global)),
                col.saturating_sub(1),
            ) + " ",
            t.dimmed(),
        ),
        Span::styled(
            fit(&format!("= {}", val(&c.effective)), col),
            Style::default().fg(t.success),
        ),
    ])
}

impl View for RepositoryView {
    fn title(&self) -> String {
        "Repository".into()
    }

    fn refresh(&mut self, ctx: &Ctx) -> anyhow::Result<()> {
        self.loader.start(&ctx.repo, Data::load);
        self.apply_loaded();
        Ok(())
    }

    fn poll(&mut self, _ctx: &Ctx) {
        self.apply_loaded();
    }

    fn loading(&self) -> bool {
        self.loader.busy()
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let [left, right] =
            Layout::horizontal([Constraint::Length(22), Constraint::Min(20)]).areas(area);
        self.draw_sections(f, left, ctx);
        let details = self.details(ctx);
        let dh = if details.is_empty() {
            0
        } else {
            details.len() as u16 + 2
        };
        let [list, info] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(dh.min(right.height / 2)),
        ])
        .areas(right);
        self.draw_content(f, list, ctx);
        if dh > 0 {
            let block = panel(" Details ", false, &ctx.theme);
            let inner = block.inner(info);
            f.render_widget(block, info);
            f.render_widget(Paragraph::new(details), inner);
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Action {
        if self.filtering {
            return self.handle_filter_key(key);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sections => Focus::Content,
                    Focus::Content => Focus::Sections,
                };
                return Action::None;
            }
            KeyCode::Char('.') | KeyCode::Char('m') => return self.context_menu(ctx),
            KeyCode::Char('O') => return switcher::open(ctx),
            _ => {}
        }
        match self.focus {
            Focus::Sections => self.handle_sections_key(key, ctx),
            Focus::Content => self.handle_content_key(key, ctx),
        }
    }

    fn help(&self) -> Vec<KeyHelp> {
        let mut v: Vec<KeyHelp> = vec![
            ("j/k", "move"),
            ("Tab", "sections/list"),
            (". / m", "all actions"),
            ("Enter", "open / edit"),
            ("/", "filter"),
            ("O", "switch repository"),
        ];
        v.extend(match self.section {
            Section::Tags => vec![
                ("n", "new tag"),
                ("D", "delete tag"),
                ("p", "push tag"),
                ("P", "push all tags"),
                ("X", "delete remote tag"),
                ("c", "checkout tag"),
                ("w", "compare with work tree"),
            ],
            Section::Remotes => vec![
                ("n", "add remote"),
                ("e", "edit URL"),
                ("u", "set push URL"),
                ("R", "rename"),
                ("D", "remove"),
                ("f", "fetch"),
                ("F", "fetch all"),
                ("P", "prune"),
            ],
            Section::Worktrees => vec![
                ("n", "add worktree"),
                ("D", "remove"),
                ("X", "force remove (discard changes)"),
                ("l", "lock / unlock"),
                ("P", "prune"),
            ],
            Section::Submodules => vec![
                ("i", "init"),
                ("u", "update"),
                ("s", "sync URLs"),
                ("n", "add submodule"),
                ("d", "deinit"),
            ],
            Section::Ignore => vec![
                ("n", "add to .gitignore"),
                ("x", "add to info/exclude"),
                ("D", "remove pattern"),
                ("c", "check path"),
                ("I", "show ignored files"),
                ("e/E", "edit file"),
            ],
            Section::Config => vec![
                ("e", "edit repo value"),
                ("g", "edit global value"),
                ("x", "unset repo value"),
                ("o", "open config file"),
            ],
        });
        v.push(("Esc", "back / clear filter"));
        v
    }

    fn captures_input(&self) -> bool {
        self.filtering
    }
}

/// Palette entries for this feature.
pub fn palette() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            name: "Switch repository…",
            run: open_repo_switcher,
        },
        PaletteItem {
            name: "Clone repository…",
            run: switcher::clone_dialog,
        },
        PaletteItem {
            name: "Init repository…",
            run: switcher::init_dialog,
        },
        PaletteItem {
            name: "New tag…",
            run: |_| actions::new_tag("HEAD"),
        },
        PaletteItem {
            name: "Add remote…",
            run: actions::add_remote,
        },
        PaletteItem {
            name: "Add worktree…",
            run: actions::add_worktree,
        },
        PaletteItem {
            name: "Add submodule…",
            run: |_| actions::add_submodule(),
        },
        PaletteItem {
            name: "Update submodules",
            run: |_| actions::update_submodules(),
        },
        PaletteItem {
            name: "Fetch all remotes",
            run: |ctx| {
                let prune = ctx.config.update.prune;
                Action::spawn("Fetch all", move |repo| admin::fetch_all(repo, prune))
            },
        },
        PaletteItem {
            name: "Push all tags",
            run: |ctx| {
                actions::with_remote(
                    ctx,
                    "Push all tags to",
                    std::rc::Rc::new(|remote, _| {
                        Action::spawn("Push tags", move |repo| admin::push_all_tags(repo, &remote))
                    }),
                )
            },
        },
        PaletteItem {
            name: "Show repository settings (tags, remotes, worktrees…)",
            run: |_| Action::SwitchTab(crate::ui::views::Tab::Repository),
        },
    ]
}

/// Open the repository switcher (nested repos, worktrees, submodules, recent repos).
pub fn open_repo_switcher(ctx: &Ctx) -> Action {
    switcher::open(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git::testutil::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn blank_view() -> RepositoryView {
        RepositoryView {
            data: Data::default(),
            loader: crate::ui::bg::Background::new(),
            section: Section::Tags,
            focus: Focus::Sections,
            sel: [0; 6],
            offset: [0; 6],
            filter: TextInput::default(),
            filtering: false,
        }
    }

    fn render(view: &mut RepositoryView, ctx: &Ctx, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| view.draw(f, f.area(), ctx)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn renders_every_section_of_a_real_repo() {
        let (_d, repo) = temp_repo();
        commit_files(
            &repo,
            &[("a.txt", "1\n"), (".gitignore", "*.log\n")],
            "init",
        );
        admin::create_tag(&repo, "v1.0", "", "First release").unwrap();
        admin::add_remote(&repo, "origin", "https://example.com/x.git").unwrap();
        let ctx = Ctx::new(repo, Config::default());
        let mut v = blank_view();
        v.refresh(&ctx).unwrap();
        let expect = [
            "v1.0",
            "origin",
            "[main]",
            "No submodules",
            "*.log",
            "user.name",
        ];
        for (i, s) in Section::ALL.iter().enumerate() {
            v.set_section(*s);
            let screen = render(&mut v, &ctx, 120, 30);
            assert!(screen.contains(expect[i]), "{s:?}:\n{screen}");
            // Tiny terminals must not panic.
            render(&mut v, &ctx, 20, 5);
        }
    }

    #[test]
    fn keys_navigate_and_open_dialogs() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a.txt", "1\n")], "init");
        admin::create_tag(&repo, "v1", "", "").unwrap();
        let ctx = Ctx::new(repo, Config::default());
        let mut v = blank_view();
        v.refresh(&ctx).unwrap();
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('n')), &ctx),
            Action::Popup(_)
        ));
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('.')), &ctx),
            Action::Popup(_)
        ));
        v.handle_key(key(KeyCode::Tab), &ctx);
        assert_eq!(v.focus, Focus::Content);
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('D')), &ctx),
            Action::Popup(_)
        ));
        assert!(matches!(
            v.handle_key(key(KeyCode::Enter), &ctx),
            Action::Popup(_)
        ));
        v.handle_key(key(KeyCode::Char('/')), &ctx);
        assert!(v.captures_input());
        v.handle_key(key(KeyCode::Char('z')), &ctx);
        assert!(v.visible().is_empty());
        v.handle_key(key(KeyCode::Esc), &ctx);
        assert!(!v.captures_input() && v.visible().len() == 1);
        v.handle_key(key(KeyCode::Esc), &ctx);
        assert_eq!(v.focus, Focus::Sections);
        v.handle_key(key(KeyCode::Char('j')), &ctx);
        assert_eq!(v.section, Section::Remotes);
        // Push all tags without remotes is an error, not a panic.
        v.set_section(Section::Tags);
        assert!(matches!(
            v.handle_key(key(KeyCode::Char('P')), &ctx),
            Action::Error(_)
        ));
    }

    #[test]
    fn sections_are_indexed_in_order() {
        for (i, s) in Section::ALL.iter().enumerate() {
            assert_eq!(s.index(), i);
            assert!(!s.title().is_empty());
        }
        for k in admin::CONFIG_KEYS {
            assert!(!config_hint(k).is_empty(), "{k}");
        }
    }

    #[test]
    fn filter_matches_section_rows() {
        let data = Data {
            tags: vec![
                Tag {
                    name: "v1.0".into(),
                    target: "abc".into(),
                    date: 0,
                    subject: "First release".into(),
                    annotated: true,
                },
                Tag {
                    name: "nightly".into(),
                    target: "def".into(),
                    date: 0,
                    subject: "build".into(),
                    annotated: false,
                },
            ],
            ..Data::default()
        };
        let mut v = RepositoryView {
            data,
            loader: crate::ui::bg::Background::new(),
            section: Section::Tags,
            focus: Focus::Content,
            sel: [0; 6],
            offset: [0; 6],
            filter: TextInput::new("release"),
            filtering: false,
        };
        assert_eq!(v.visible(), vec![0]);
        v.filter.set("");
        v.move_content(5);
        assert_eq!(v.selected(), Some(1));
        v.set_section(Section::Remotes);
        assert_eq!(v.selected(), None);
    }
}
