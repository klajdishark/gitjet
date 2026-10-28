//! Main loop: owns the tabs, popup stack, background tasks and status bar.

use std::io::stdout;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::action::{Action, Task};
use crate::ctx::Ctx;
use crate::git::{Repo, RepoState};
use crate::ui::popups::{MenuItem, MenuPopup, MessagePopup};
use crate::ui::views::{self, Tab};
use crate::ui::{KeyHelp, Popup, View};

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Terminals report focus changes often (every Alt+Tab); a refresh this recent makes another redundant.
const FOCUS_REFRESH_MIN_GAP: Duration = Duration::from_secs(1);

const GLOBAL_HELP: [KeyHelp; 7] = [
    ("1-9", "switch tab"),
    ("[ ]", "prev/next tab"),
    (":", "actions"),
    ("?", "help"),
    ("F5", "refresh"),
    ("q", "quit"),
    ("Ctrl+C", "quit"),
];

struct Toast {
    text: String,
    error: bool,
    at: Instant,
}

struct TaskDone {
    id: u64,
    label: String,
    silent: bool,
    result: Result<String>,
}

fn load_head(repo: &Repo) -> HeadInfo {
    let branch = repo.head_branch().unwrap_or_else(|| {
        repo.try_run(&["rev-parse", "--short", "HEAD"])
            .map(|s| format!("detached at {}", s.trim()))
            .unwrap_or_else(|| "no commits yet".into())
    });
    let state = repo.state();
    let (mut ahead, mut behind, mut has_upstream) = (0, 0, false);
    if let Some(out) = repo.try_run(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
    {
        let mut it = out.split_whitespace();
        ahead = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        behind = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        has_upstream = true;
    }
    HeadInfo {
        branch,
        state: (state != RepoState::Clean).then_some(state),
        ahead,
        behind,
        has_upstream,
    }
}

#[derive(Default)]
struct HeadInfo {
    branch: String,
    state: Option<RepoState>,
    ahead: u32,
    behind: u32,
    has_upstream: bool,
}

pub struct App {
    ctx: Ctx,
    views: Vec<Box<dyn View>>,
    dirty: Vec<bool>,
    active: usize,
    popups: Vec<Box<dyn Popup>>,
    toast: Option<Toast>,
    running: Vec<String>,
    tx: Sender<TaskDone>,
    rx: Receiver<TaskDone>,
    next_task: u64,
    followups: std::collections::HashMap<u64, crate::action::Then>,
    /// Whether conflicts were seen last time (None until the first load), to switch to the
    /// Conflicts tab when new ones appear.
    had_conflicts: Option<bool>,
    /// Set by `Action::Refresh`; the refresh runs once per loop pass, however often it was asked for.
    refresh_pending: bool,
    last_refresh: Instant,
    pending_suspend: Option<Command>,
    quit: bool,
    head: HeadInfo,
    head_loader: crate::ui::bg::Background<HeadInfo>,
    tab_hits: Vec<(u16, u16, usize)>,
    last_snapshot: Instant,
    ticks: usize,
}

pub fn run(ctx: Ctx) -> Result<()> {
    let mouse = ctx.config.mouse;
    let mut terminal = enter(mouse)?;
    let mut app = App::new(ctx);
    let res = app.event_loop(&mut terminal, mouse);
    leave(mouse);
    res
}

fn enter(mouse: bool) -> Result<DefaultTerminal> {
    let terminal = ratatui::init();
    if mouse {
        let _ = execute!(stdout(), EnableMouseCapture);
    }
    let _ = execute!(stdout(), EnableFocusChange);
    Ok(terminal)
}

fn leave(mouse: bool) {
    if mouse {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
    let _ = execute!(stdout(), DisableFocusChange);
    ratatui::restore();
}

impl App {
    fn new(ctx: Ctx) -> Self {
        let views = views::build(&ctx);
        let n = views.len();
        let (tx, rx) = channel();
        let mut app = Self {
            ctx,
            views,
            dirty: vec![true; n],
            active: 0,
            popups: Vec::new(),
            toast: None,
            running: Vec::new(),
            tx,
            rx,
            next_task: 0,
            followups: std::collections::HashMap::new(),
            had_conflicts: None,
            refresh_pending: false,
            last_refresh: Instant::now(),
            pending_suspend: None,
            quit: false,
            head: HeadInfo::default(),
            head_loader: crate::ui::bg::Background::new(),
            tab_hits: Vec::new(),
            last_snapshot: Instant::now(),
            ticks: 0,
        };
        if let Some(e) = app.ctx.config.load_error.clone() {
            app.apply(Action::Error(format!("Config error (using defaults): {e}")));
        }
        app.refresh_head();
        app.ensure_fresh(0);
        app.ensure_fresh(Tab::Conflicts.index());
        app.maintain_repo();
        app
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal, mouse: bool) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                // Drain all queued events before redrawing.
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Mouse(m) => self.on_mouse(m),
                        Event::FocusGained => {
                            if self.last_refresh.elapsed() >= FOCUS_REFRESH_MIN_GAP {
                                self.apply(Action::Refresh);
                            }
                        }
                        Event::Paste(s) => {
                            for c in s.chars() {
                                self.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
                            }
                        }
                        _ => {}
                    }
                    if self.quit || self.pending_suspend.is_some() || !event::poll(Duration::ZERO)?
                    {
                        break;
                    }
                }
            }
            self.poll_tasks();
            if std::mem::take(&mut self.refresh_pending) {
                self.refresh_all();
            }
            self.tick();
            if let Some(mut cmd) = self.pending_suspend.take() {
                leave(mouse);
                let status = cmd.current_dir(&self.ctx.repo.root).status();
                *terminal = enter(mouse)?;
                terminal.clear()?;
                match status {
                    Ok(s) if !s.success() => {
                        self.apply(Action::Toast(format!("Command exited with {s}")))
                    }
                    Err(e) => self.apply(Action::Error(format!("Could not run command: {e}"))),
                    _ => {}
                }
                self.apply(Action::Refresh);
            }
            if self.quit {
                return Ok(());
            }
        }
    }

    // ----- actions -------------------------------------------------------------------------

    fn apply(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Refresh => self.refresh_pending = true,
            Action::SwitchTab(t) => self.switch_tab(t.index()),
            Action::Popup(p) => self.popups.push(p),
            Action::ClosePopup => {
                self.popups.pop();
            }
            Action::Toast(text) => {
                if !text.is_empty() {
                    self.toast = Some(Toast {
                        text,
                        error: false,
                        at: Instant::now(),
                    })
                }
            }
            Action::Error(text) => {
                let long = text.contains('\n') || text.chars().count() > 110;
                if long {
                    self.popups
                        .push(Box::new(MessagePopup::new("Error", text).error()));
                } else {
                    self.toast = Some(Toast {
                        text,
                        error: true,
                        at: Instant::now(),
                    });
                }
            }
            Action::Spawn(task) => self.spawn(task),
            Action::Suspend(cmd) => self.pending_suspend = Some(cmd),
            Action::SwitchRepo(path) => match Repo::discover(&path) {
                Ok(repo) => {
                    self.ctx.repo = repo;
                    self.views = views::build(&self.ctx);
                    self.dirty = vec![true; self.views.len()];
                    self.popups.clear();
                    self.had_conflicts = None;
                    self.refresh_head();
                    self.ensure_fresh(self.active);
                    self.ensure_fresh(Tab::Conflicts.index());
                    self.maintain_repo();
                    let name = self.ctx.repo.name();
                    self.apply(Action::Toast(format!("Opened {name}")));
                }
                Err(e) => self.apply(Action::Error(format!("{e:#}"))),
            },
            Action::Batch(v) => {
                for a in v {
                    self.apply(a);
                }
            }
        }
    }

    fn spawn(&mut self, task: Task) {
        let Task {
            label,
            job,
            silent,
            then,
        } = task;
        if !silent {
            self.running.push(label.clone());
        }
        let id = self.next_task;
        self.next_task += 1;
        if let Some(then) = then {
            self.followups.insert(id, then);
        }
        let repo = self.ctx.repo.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = job(&repo);
            let _ = tx.send(TaskDone {
                id,
                label,
                silent,
                result,
            });
        });
    }

    fn poll_tasks(&mut self) {
        while let Ok(done) = self.rx.try_recv() {
            let then = self.followups.remove(&done.id);
            if done.silent {
                continue;
            }
            if let Some(i) = self.running.iter().position(|l| *l == done.label) {
                self.running.remove(i);
            }
            match &done.result {
                Ok(msg) => {
                    let msg = if msg.trim().is_empty() {
                        format!("{} finished", done.label)
                    } else {
                        msg.clone()
                    };
                    self.apply(Action::Toast(msg));
                }
                Err(e) => self.apply(Action::Error(format!("{}: {e:#}", done.label))),
            }
            self.apply(Action::Refresh);
            if let Some(then) = then {
                let a = then(&done.result);
                self.apply(a);
            }
        }
    }

    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if let Some(head) = self.head_loader.poll() {
            self.head = head;
        }
        if let Some(t) = &self.toast {
            let ttl = if t.error { 10 } else { 4 };
            if t.at.elapsed() > Duration::from_secs(ttl) {
                self.toast = None;
            }
        }
        let lh = &self.ctx.config.local_history;
        if lh.enabled
            && self.last_snapshot.elapsed() >= Duration::from_secs(lh.interval_secs.max(5))
        {
            self.last_snapshot = Instant::now();
            let cfg = lh.clone();
            self.spawn(Task {
                label: "Local history snapshot".into(),
                job: Box::new(move |repo| {
                    crate::git::local_history::snapshot_changed(repo, &cfg).map(|n| n.to_string())
                }),
                silent: true,
                then: None,
            });
        }
        for v in self.views.iter_mut() {
            v.poll(&self.ctx);
        }
        self.watch_conflicts();
        let mut polled = Vec::new();
        for p in self.popups.iter_mut() {
            polled.push(p.poll(&self.ctx));
        }
        for a in polled {
            self.apply(a);
        }
    }

    /// Silent background upkeep for large repositories (commit-graph), when enabled.
    fn maintain_repo(&mut self) {
        if self.ctx.config.performance.write_commit_graph {
            self.spawn(Task {
                label: "Write commit-graph".into(),
                job: Box::new(|repo| repo.ensure_commit_graph().map(|w| w.to_string())),
                silent: true,
                then: None,
            });
        }
    }

    /// Branch, upstream and operation state for the status bar, loaded in the background.
    fn refresh_head(&mut self) {
        self.head_loader.start(&self.ctx.repo, load_head);
    }

    fn refresh_all(&mut self) {
        self.last_refresh = Instant::now();
        self.dirty.iter_mut().for_each(|d| *d = true);
        self.refresh_head();
        self.ensure_fresh(self.active);
        // Views with badges (conflicts) stay current even when hidden.
        let conflicts = Tab::Conflicts.index();
        if conflicts != self.active {
            self.ensure_fresh(conflicts);
        }
        for p in self.popups.iter_mut() {
            p.refresh(&self.ctx);
        }
    }

    /// Bring up the conflicts as soon as an operation stops on them. The Conflicts tab loads in the
    /// background, so this checks its badge whenever it is not loading; the first load only sets
    /// the baseline (conflicts that were already there when gitjet started don't switch tabs).
    fn watch_conflicts(&mut self) {
        let conflicts = Tab::Conflicts.index();
        let view = &self.views[conflicts];
        if view.loading() {
            return;
        }
        let has_conflicts = view.badge().is_some();
        if has_conflicts && self.had_conflicts == Some(false) && self.active != conflicts {
            self.switch_tab(conflicts);
            self.apply(Action::Toast("Conflicts need resolving".into()));
        }
        self.had_conflicts = Some(has_conflicts);
    }

    fn ensure_fresh(&mut self, i: usize) {
        if i < self.views.len() && self.dirty[i] {
            self.dirty[i] = false;
            if let Err(e) = self.views[i].refresh(&self.ctx) {
                self.apply(Action::Error(format!("{e:#}")));
            }
        }
    }

    fn switch_tab(&mut self, i: usize) {
        if i < self.views.len() {
            self.active = i;
            self.ensure_fresh(i);
        }
    }

    // ----- input ---------------------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if let Some(p) = self.popups.last_mut() {
            let a = p.handle_key(key, &self.ctx);
            self.apply(a);
            return;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let palette_key = (key.code == KeyCode::Char('`')
            && key.modifiers.contains(KeyModifiers::ALT))
            || (key.code == KeyCode::Char(':') && plain);
        if !self.views[self.active].captures_input() {
            match key.code {
                KeyCode::Char('q') if plain => {
                    self.quit = true;
                    return;
                }
                KeyCode::Char('?') if plain => {
                    self.show_help();
                    return;
                }
                KeyCode::Char(c @ '1'..='9') if plain => {
                    self.switch_tab(c as usize - '1' as usize);
                    return;
                }
                KeyCode::Char('[') if plain => {
                    let n = self.views.len();
                    self.switch_tab((self.active + n - 1) % n);
                    return;
                }
                KeyCode::Char(']') if plain => {
                    self.switch_tab((self.active + 1) % self.views.len());
                    return;
                }
                KeyCode::F(5) => {
                    self.apply(Action::Refresh);
                    return;
                }
                _ if palette_key => {
                    self.show_palette();
                    return;
                }
                _ => {}
            }
        }
        let a = self.views[self.active].handle_key(key, &self.ctx);
        self.apply(a);
    }

    fn on_mouse(&mut self, ev: MouseEvent) {
        let scroll_key = match ev.kind {
            MouseEventKind::ScrollUp => Some(KeyCode::Up),
            MouseEventKind::ScrollDown => Some(KeyCode::Down),
            _ => None,
        };
        if let Some(p) = self.popups.last_mut() {
            let a = p.handle_mouse(ev, &self.ctx);
            if a.is_none() {
                if let Some(code) = scroll_key {
                    let a = p.handle_key(KeyEvent::new(code, KeyModifiers::NONE), &self.ctx);
                    self.apply(a);
                }
            } else {
                self.apply(a);
            }
            return;
        }
        if ev.row == 0 && matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(&(_, _, i)) = self
                .tab_hits
                .iter()
                .find(|(x0, x1, _)| ev.column >= *x0 && ev.column < *x1)
            {
                self.switch_tab(i);
            }
            return;
        }
        let a = self.views[self.active].handle_mouse(ev, &self.ctx);
        if a.is_none() {
            if let Some(code) = scroll_key {
                let a = self.views[self.active]
                    .handle_key(KeyEvent::new(code, KeyModifiers::NONE), &self.ctx);
                self.apply(a);
            }
        } else {
            self.apply(a);
        }
    }

    fn show_help(&mut self) {
        let t = &self.ctx.theme;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let section = |title: String, keys: Vec<KeyHelp>, lines: &mut Vec<Line<'static>>| {
            if keys.is_empty() {
                return;
            }
            lines.push(Line::styled(title, t.title()));
            for (k, d) in keys {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {k:<14}"), t.key()),
                    Span::styled(d.to_string(), t.text()),
                ]));
            }
            lines.push(Line::raw(""));
        };
        section(
            self.views[self.active].title(),
            self.views[self.active].help(),
            &mut lines,
        );
        section("Global".into(), GLOBAL_HELP.to_vec(), &mut lines);
        let tabs: Vec<KeyHelp> = Vec::new();
        section("".into(), tabs, &mut lines);
        lines.push(Line::styled(
            format!("Config file: {}", crate::config::Config::path().display()),
            t.dimmed(),
        ));
        self.popups.push(Box::new(MessagePopup::from_lines(
            "Keyboard shortcuts",
            lines,
        )));
    }

    fn show_palette(&mut self) {
        let items: Vec<MenuItem> = views::palette()
            .into_iter()
            .map(|p| MenuItem::new(p.name, "", p.run))
            .chain(Tab::ALL.iter().enumerate().map(|(i, t)| {
                let t = *t;
                MenuItem::new(
                    format!("Go to tab: {}", self.views[i].title()),
                    (i + 1).to_string(),
                    move |_| Action::SwitchTab(t),
                )
            }))
            .collect();
        self.popups
            .push(Box::new(MenuPopup::new("Actions", items).filterable()));
    }

    // ----- drawing -------------------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let [tabs, main, status, hints] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_tabs(f, tabs);
        self.views[self.active].draw(f, main, &self.ctx);
        self.draw_status(f, status);
        self.draw_hints(f, hints);
        for p in self.popups.iter_mut() {
            p.draw(f, area, &self.ctx);
        }
    }

    fn draw_tabs(&mut self, f: &mut Frame, area: Rect) {
        let t = &self.ctx.theme;
        let mut spans = vec![
            Span::styled(
                " gitjet ",
                Style::default()
                    .fg(t.accent)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            Span::raw(" "),
        ];
        let mut x = 9u16;
        self.tab_hits.clear();
        for (i, v) in self.views.iter().enumerate() {
            let badge = v.badge().map(|b| format!(" ({b})")).unwrap_or_default();
            let label = format!(" {} {}{} ", i + 1, v.title(), badge);
            let w = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
            let style = if i == self.active {
                t.selected().fg(t.accent)
            } else if !badge.is_empty() && i == Tab::Conflicts.index() {
                Style::default().fg(t.conflict)
            } else {
                t.dimmed()
            };
            self.tab_hits.push((x, x + w, i));
            x += w;
            spans.push(Span::styled(label, style));
        }
        f.render_widget(Line::from(spans), area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let t = &self.ctx.theme;
        let mut left = vec![
            Span::styled(
                format!(" {} ", self.ctx.repo.name()),
                Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", self.head.branch),
                Style::default().fg(t.local_branch),
            ),
        ];
        if self.head.has_upstream && (self.head.ahead > 0 || self.head.behind > 0) {
            left.push(Span::styled(
                format!("↑{} ↓{} ", self.head.ahead, self.head.behind),
                Style::default().fg(t.warning),
            ));
        }
        if let Some(s) = self.head.state {
            left.push(Span::styled(
                format!(" {} ", s.label()),
                Style::default()
                    .fg(t.conflict)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ));
            left.push(Span::raw(" "));
        }
        if let Some(toast) = &self.toast {
            let style = if toast.error {
                Style::default().fg(t.error)
            } else {
                Style::default().fg(t.success)
            };
            let first = toast.text.lines().next().unwrap_or("");
            left.push(Span::styled(format!(" {first}"), style));
        }
        let mut busy = self.running.clone();
        if self.views[self.active].loading() {
            busy.push(format!("Loading {}", self.views[self.active].title()));
        }
        let right = if busy.is_empty() {
            String::new()
        } else {
            format!(
                "{} {} ",
                SPINNER[self.ticks % SPINNER.len()],
                busy.join(", ")
            )
        };
        let rw = unicode_width::UnicodeWidthStr::width(right.as_str()) as u16;
        let [l, r] = Layout::horizontal([Constraint::Min(1), Constraint::Length(rw)]).areas(area);
        f.render_widget(
            Paragraph::new(Line::from(left)).style(Style::default().bg(t.selection_bg)),
            l,
        );
        f.render_widget(
            Paragraph::new(right).style(Style::default().fg(t.accent).bg(t.selection_bg)),
            r,
        );
    }

    fn draw_hints(&self, f: &mut Frame, area: Rect) {
        let t = &self.ctx.theme;
        let keys: Vec<KeyHelp> = match self.popups.last() {
            Some(p) => p.help(),
            None => {
                let mut v = self.views[self.active].help();
                v.truncate(8);
                v.extend([(":", "actions"), ("?", "help"), ("q", "quit")]);
                v
            }
        };
        let mut spans = Vec::new();
        for (k, d) in keys {
            spans.push(Span::styled(format!(" {k}"), t.key()));
            spans.push(Span::styled(format!(" {d} "), t.dimmed()));
        }
        f.render_widget(Line::from(spans), area);
    }
}
