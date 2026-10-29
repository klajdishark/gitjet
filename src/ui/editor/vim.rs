//! Vim-style modal editing over a [`Buffer`]: normal, insert, replace, visual and command-line
//! modes. Keys arrive as chars: control keys as ASCII control codes, the others as [`keys`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use regex::Regex;

use super::buffer::{byte_of, char_len, Buffer, Pos};
use super::pattern;

pub mod keys {
    pub const ESC: char = '\x1b';
    pub const ENTER: char = '\r';
    pub const BS: char = '\x08';
    pub const TAB: char = '\t';
    pub const UP: char = '\u{E000}';
    pub const DOWN: char = '\u{E001}';
    pub const LEFT: char = '\u{E002}';
    pub const RIGHT: char = '\u{E003}';
    pub const HOME: char = '\u{E004}';
    pub const END: char = '\u{E005}';
    pub const PAGE_UP: char = '\u{E006}';
    pub const PAGE_DOWN: char = '\u{E007}';
    pub const DEL: char = '\u{E008}';

    /// The control code Ctrl+`c` sends.
    pub const fn ctrl(c: char) -> char {
        ((c as u8) & 0x1f) as char
    }
}
use keys::*;

/// A key that inserts itself (not a control code nor one of the special [`keys`]).
fn is_text(c: char) -> bool {
    !c.is_control() && !('\u{E000}'..='\u{E0FF}').contains(&c)
}

/// 0 for blanks (and line ends), 1 for keyword chars (any non-blank when `big`), 2 for the rest.
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// Screen cells `c` takes when drawn at cell `x`.
pub fn cell_width(c: char, x: usize, tabstop: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    if c == '\t' {
        tabstop - x % tabstop
    } else if c.is_control() {
        2
    } else {
        c.width().unwrap_or(0)
    }
}

/// The screen cell where char column `col` of `line` starts.
pub fn display_col(line: &str, col: usize, tabstop: usize) -> usize {
    line.chars()
        .take(col)
        .fold(0, |x, c| x + cell_width(c, x, tabstop))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    Command,
    Search { forward: bool },
}

#[derive(Clone, Debug, Default)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
}

pub struct Options {
    pub number: bool,
    pub relativenumber: bool,
    pub ignorecase: bool,
    pub smartcase: bool,
    pub hlsearch: bool,
    pub incsearch: bool,
    pub tabstop: usize,
    pub shiftwidth: usize,
    pub expandtab: bool,
    pub autoindent: bool,
    pub wrapscan: bool,
}

/// What the host should do after a key.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// A file was written.
    pub wrote: bool,
    /// The editor was closed (`:q`, `:wq`, `ZZ`...).
    pub quit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordFwd(bool),
    WordBack(bool),
    WordEnd(bool),
    WordEndBack(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    LastNonBlank,
    FileStart,
    FileEnd,
    ScreenTop,
    ScreenMid,
    ScreenBottom,
    ParaFwd,
    ParaBack,
    MatchPair,
    SearchNext,
    SearchPrev,
    StarFwd,
    StarBack,
    Find(char, char),
    FindRepeat,
    FindRepeatRev,
    NextLine,
    PrevLine,
    CurLine,
    /// `'x` (to the line) or `` `x `` (exact).
    Mark(char, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
    Upper,
    Lower,
    Toggle,
}

#[derive(Clone, Copy, Debug)]
enum Target {
    /// `dd`, `cc`, `yy`...
    Line,
    Motion(Motion),
    /// `iw`, `a"`... (inner, object char)
    Object(bool, char),
    /// The visual selection; `true` forces whole lines (`X`, `Y`, `C` in visual mode).
    Visual(bool),
}

#[derive(Debug)]
enum Act {
    Move(Motion),
    Op(Op, Target),
    Cmd(String),
}

#[derive(Debug)]
struct Cmd {
    reg: Option<char>,
    count: Option<usize>,
    act: Act,
}

enum Parse {
    More,
    Bad,
    Done(Cmd),
}

/// A range of text: chars from `a` to `b` (exclusive), or rows `a.row..=b.row` when `lines`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Range {
    pub a: Pos,
    pub b: Pos,
    pub lines: bool,
}

impl Range {
    fn chars(a: Pos, b: Pos) -> Self {
        Self { a, b, lines: false }
    }

    pub(super) fn lines(r1: usize, r2: usize) -> Self {
        Self {
            a: Pos::new(r1, 0),
            b: Pos::new(r2, 0),
            lines: true,
        }
    }
}

pub struct Vim {
    pub buf: Buffer,
    pub cur: Pos,
    pub mode: Mode,
    /// Command-line text while in `:` or `/` mode.
    pub cmdline: String,
    /// Message shown on the command line; `true` for errors.
    pub msg: Option<(String, bool)>,
    /// Normal-mode keys typed so far for an incomplete command (shown like 'showcmd').
    pub pending: String,
    pub opts: Options,
    /// First row on screen and the number of text rows, set by the renderer.
    pub top: usize,
    pub height: usize,
    /// Center the cursor line on the next draw.
    pub center_next: bool,
    pub path: PathBuf,
    /// Name shown in the status line.
    pub name: String,
    pub(super) crlf: bool,
    /// The file ends with a newline.
    pub(super) eol: bool,
    /// Cursor column to keep while moving up and down (`usize::MAX` after `$`).
    want_col: usize,
    pub(super) regs: HashMap<char, Register>,
    pub(super) last_search: Option<String>,
    last_forward: bool,
    /// Highlight matches of the last search (until `:noh`).
    pub(super) hl: bool,
    last_find: Option<(char, char)>,
    pub anchor: Pos,
    /// Last visual selection (anchor, cursor, linewise) for `'<,'>` and `gv`.
    pub(super) last_visual: Option<(Pos, Pos, bool)>,
    pub(super) marks: HashMap<char, Pos>,
    /// Keys of the last change, for `.`.
    dot: String,
    /// Keys of the change in progress while it is still in insert mode.
    rec: String,
    recording: bool,
    /// Replaying keys (`.`, `:normal`, `:g`): no recording, and the caller closes the undo group.
    pub(super) batch: bool,
    /// Pattern, replacement and flags of the last `:s`.
    pub(super) last_sub: Option<(String, String, String)>,
    cmd_history: Vec<String>,
    search_history: Vec<String>,
    history_pos: Option<usize>,
    pub(super) out: Outcome,
}

/// Lines of a text file, whether it uses CRLF, and whether it ends with a newline;
/// `None` when it does not exist.
pub(super) fn load(path: &Path) -> Result<Option<(Vec<String>, bool, bool)>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
    };
    if bytes.contains(&0) {
        bail!("{} looks like a binary file", path.display());
    }
    let Ok(text) = String::from_utf8(bytes) else {
        bail!("{} is not UTF-8 text", path.display());
    };
    let crlf = text.contains("\r\n");
    let eol = text.ends_with('\n');
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|l| match l.strip_suffix('\r').filter(|_| crlf) {
            Some(l) => l.to_string(),
            None => l.to_string(),
        })
        .collect();
    if eol {
        lines.pop();
    }
    Ok(Some((lines, crlf, eol)))
}

impl Vim {
    pub fn new(lines: Vec<String>, path: PathBuf, crlf: bool, eol: bool) -> Self {
        let expandtab = !lines.iter().any(|l| l.starts_with('\t'));
        Self {
            buf: Buffer::new(lines),
            cur: Pos::default(),
            mode: Mode::Normal,
            cmdline: String::new(),
            msg: None,
            pending: String::new(),
            opts: Options {
                number: true,
                relativenumber: false,
                ignorecase: true,
                smartcase: true,
                hlsearch: true,
                incsearch: true,
                tabstop: 4,
                shiftwidth: 4,
                expandtab,
                autoindent: true,
                wrapscan: true,
            },
            top: 0,
            height: 20,
            center_next: false,
            name: path.display().to_string(),
            path,
            crlf,
            eol,
            want_col: 0,
            regs: HashMap::new(),
            last_search: None,
            last_forward: true,
            hl: false,
            last_find: None,
            anchor: Pos::default(),
            last_visual: None,
            marks: HashMap::new(),
            dot: String::new(),
            rec: String::new(),
            recording: false,
            batch: false,
            last_sub: None,
            cmd_history: Vec::new(),
            search_history: Vec::new(),
            history_pos: None,
            out: Outcome::default(),
        }
    }

    /// Open `path` (a missing file starts empty and is created on `:w`), with the cursor on
    /// 1-based `line`.
    pub fn open(path: PathBuf, line: Option<usize>) -> Result<Self> {
        let loaded = load(&path)?;
        let new = loaded.is_none();
        let (lines, crlf, eol) = loaded.unwrap_or((Vec::new(), false, true));
        let mut v = Self::new(lines, path, crlf, eol);
        v.msg = Some((
            if new {
                "[New]".to_string()
            } else {
                format!("{}L", v.buf.line_count())
            },
            false,
        ));
        if let Some(l) = line {
            let row = l.saturating_sub(1).min(v.buf.last_row());
            v.cur = Pos::new(row, v.first_non_blank(row));
            v.want_col = v.cur.col;
            v.center_next = true;
        }
        Ok(v)
    }

    // ----- small helpers --------------------------------------------------------------------

    fn line(&self, row: usize) -> &str {
        self.buf.line(row)
    }

    fn len(&self, row: usize) -> usize {
        self.buf.line_len(row)
    }

    fn last(&self) -> usize {
        self.buf.last_row()
    }

    /// The char at `p`, `\n` at the end of a line.
    fn ch(&self, p: Pos) -> char {
        self.line(p.row).chars().nth(p.col).unwrap_or('\n')
    }

    fn cls(&self, p: Pos, big: bool) -> u8 {
        class(self.ch(p), big)
    }

    /// The next position, stepping over each line end as one position.
    fn next(&self, p: Pos) -> Option<Pos> {
        if p.col < self.len(p.row) {
            Some(Pos::new(p.row, p.col + 1))
        } else if p.row < self.last() {
            Some(Pos::new(p.row + 1, 0))
        } else {
            None
        }
    }

    fn prev(&self, p: Pos) -> Option<Pos> {
        if p.col > 0 {
            Some(Pos::new(p.row, p.col - 1))
        } else if p.row > 0 {
            Some(Pos::new(p.row - 1, self.len(p.row - 1)))
        } else {
            None
        }
    }

    fn is_empty_line(&self, p: Pos) -> bool {
        p.col == 0 && self.len(p.row) == 0
    }

    pub(super) fn first_non_blank(&self, row: usize) -> usize {
        let line = self.line(row);
        line.chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or_else(|| char_len(line).saturating_sub(1))
    }

    fn indent_of(&self, row: usize) -> String {
        let line = self.line(row);
        line[..line.len() - line.trim_start().len()].to_string()
    }

    /// Indentation `width` cells wide, with tabs unless 'expandtab'.
    pub(super) fn make_indent(&self, width: usize) -> String {
        if self.opts.expandtab {
            " ".repeat(width)
        } else {
            let ts = self.opts.tabstop.max(1);
            "\t".repeat(width / ts) + &" ".repeat(width % ts)
        }
    }

    pub fn is_visual(&self) -> bool {
        matches!(self.mode, Mode::Visual | Mode::VisualLine)
    }

    fn inserting(&self) -> bool {
        matches!(self.mode, Mode::Insert | Mode::Replace)
    }

    pub(super) fn info(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), false));
    }

    pub(super) fn err(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), true));
    }

    pub(super) fn regex(&self, pat: &str) -> Result<Regex, String> {
        pattern::compile(pat, self.opts.ignorecase, self.opts.smartcase)
    }

    /// The pattern whose matches are highlighted: the search being typed, else the last one.
    pub fn highlight_regex(&self) -> Option<Regex> {
        match self.mode {
            Mode::Search { .. } if self.opts.incsearch && !self.cmdline.is_empty() => {
                self.regex(&self.cmdline).ok()
            }
            _ if self.hl && self.opts.hlsearch => {
                self.last_search.as_ref().and_then(|p| self.regex(p).ok())
            }
            _ => None,
        }
    }

    /// The selection as a range, while in visual mode.
    pub(super) fn visual_range(&self) -> Range {
        let (a, b) = if self.anchor <= self.cur {
            (self.anchor, self.cur)
        } else {
            (self.cur, self.anchor)
        };
        if self.mode == Mode::VisualLine {
            Range::lines(a.row, b.row)
        } else {
            Range::chars(a, Pos::new(b.row, (b.col + 1).min(self.len(b.row))))
        }
    }

    /// Rows and char columns of the selection, for drawing: (first, last inclusive, linewise).
    pub fn selection(&self) -> Option<(Pos, Pos, bool)> {
        if !self.is_visual() {
            return None;
        }
        let (a, b) = if self.anchor <= self.cur {
            (self.anchor, self.cur)
        } else {
            (self.cur, self.anchor)
        };
        Some((a, b, self.mode == Mode::VisualLine))
    }

    fn exit_visual(&mut self) {
        if self.is_visual() {
            self.last_visual = Some((self.anchor, self.cur, self.mode == Mode::VisualLine));
            self.mode = Mode::Normal;
        }
    }

    pub(super) fn clamp_cursor(&mut self) {
        self.cur.row = self.cur.row.min(self.last());
        let len = self.len(self.cur.row);
        let max = if self.inserting() {
            len
        } else {
            len.saturating_sub(1)
        };
        self.cur.col = self.cur.col.min(max);
    }

    /// Column for `row` when moving vertically.
    fn want(&self, row: usize) -> usize {
        self.want_col.min(self.len(row))
    }

    pub(super) fn jump(&mut self, p: Pos) {
        self.marks.insert('\'', self.cur);
        self.cur = p;
    }

    /// Scroll so the cursor is on screen, with a few lines of context.
    pub fn ensure_visible(&mut self) {
        let h = self.height.max(1);
        let so = 3.min((h - 1) / 2);
        if std::mem::take(&mut self.center_next) {
            self.top = self.cur.row.saturating_sub(h / 2);
        }
        if self.cur.row < self.top + so {
            self.top = self.cur.row.saturating_sub(so);
        } else if self.cur.row + so >= self.top + h {
            self.top = self.cur.row + so + 1 - h;
        }
        self.top = self.top.min(self.buf.line_count().saturating_sub(h));
        if self.cur.row < self.top {
            self.top = self.cur.row;
        }
    }

    /// Scroll the view by `delta` rows, dragging the cursor along when it would leave it.
    pub fn scroll(&mut self, delta: isize) {
        let h = self.height.max(1);
        let max_top = self.buf.line_count().saturating_sub(h);
        self.top = self.top.saturating_add_signed(delta).min(max_top);
        let so = 3.min((h - 1) / 2);
        let lo = if self.top == 0 { 0 } else { self.top + so };
        let hi = if self.top + h >= self.buf.line_count() {
            self.last()
        } else {
            (self.top + h).saturating_sub(so + 1)
        };
        let row = self.cur.row.clamp(lo.min(hi), hi);
        if row != self.cur.row {
            self.cur.row = row;
            self.cur.col = self.want(row);
            self.clamp_cursor();
        }
    }

    /// Put the cursor at `p` (a mouse click); in insert mode this ends the change, like the
    /// arrow keys.
    pub fn move_to(&mut self, p: Pos) {
        let inserting = self.inserting();
        if inserting && !self.batch {
            if self.recording {
                self.rec.push(ESC);
                self.dot = std::mem::take(&mut self.rec);
                self.recording = false;
            }
            self.buf.end();
        }
        self.cur = p;
        self.clamp_cursor();
        self.want_col = self.cur.col;
        if inserting {
            self.buf.begin(self.cur);
        }
    }

    // ----- keys -----------------------------------------------------------------------------

    /// Handle one key.
    pub fn key(&mut self, k: char) -> Outcome {
        self.feed(k);
        std::mem::take(&mut self.out)
    }

    pub(super) fn feed(&mut self, k: char) {
        if k == ctrl('s') && !self.batch {
            if let Err(e) = self.write_to(None) {
                self.err(e);
            }
            return;
        }
        match self.mode {
            Mode::Insert | Mode::Replace => self.insert_key(k),
            Mode::Command | Mode::Search { .. } => self.cmdline_key(k),
            Mode::Normal | Mode::Visual | Mode::VisualLine => self.normal_key(k),
        }
    }

    /// Leave whatever `feed` left half done (for `.`, `:normal`, `:g`).
    pub(super) fn finish_keys(&mut self) {
        self.pending.clear();
        match self.mode {
            Mode::Insert | Mode::Replace => self.insert_key(ESC),
            Mode::Visual | Mode::VisualLine => self.exit_visual(),
            Mode::Command | Mode::Search { .. } => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
            }
            Mode::Normal => {}
        }
    }

    fn normal_key(&mut self, k: char) {
        if k == ESC || k == ctrl('c') {
            if self.pending.is_empty() {
                if self.is_visual() {
                    self.exit_visual();
                } else if k == ctrl('c') && !self.batch {
                    self.info("Type :q and press <Enter> to close the editor");
                }
            }
            self.pending.clear();
            return;
        }
        self.pending.push(k);
        let cmd = match parse(&self.pending, self.is_visual()) {
            Parse::More => return,
            Parse::Bad => {
                self.pending.clear();
                return;
            }
            Parse::Done(cmd) => cmd,
        };
        let keys = std::mem::take(&mut self.pending);
        if !self.batch {
            self.msg = None;
        }
        self.buf.begin(self.cur);
        let was_visual = self.is_visual();
        let change = self.exec(cmd);
        if change && !self.batch && !was_visual {
            if self.inserting() {
                self.rec = keys;
                self.recording = true;
            } else {
                self.dot = keys;
            }
        }
        if !self.inserting() && !self.batch {
            self.buf.end();
        }
        self.clamp_cursor();
    }

    /// Run a complete normal/visual command; true when it is a change `.` can repeat.
    fn exec(&mut self, cmd: Cmd) -> bool {
        match cmd.act {
            Act::Move(m) => {
                if let Some((p, _)) = self.motion_target(m, cmd.count) {
                    if matches!(
                        m,
                        Motion::FileStart
                            | Motion::FileEnd
                            | Motion::SearchNext
                            | Motion::SearchPrev
                            | Motion::StarFwd
                            | Motion::StarBack
                            | Motion::MatchPair
                            | Motion::ParaFwd
                            | Motion::ParaBack
                    ) {
                        self.jump(p);
                    } else {
                        self.cur = p;
                    }
                    match m {
                        Motion::Up | Motion::Down => {}
                        Motion::LineEnd => self.want_col = usize::MAX,
                        _ => self.want_col = p.col,
                    }
                }
                false
            }
            Act::Op(op, target) => {
                let visual = matches!(target, Target::Visual(_));
                let Some(range) = self.op_range(op, target, cmd.count) else {
                    return false;
                };
                if visual {
                    self.exit_visual();
                }
                self.apply_op(op, range, cmd.reg);
                self.want_col = self.cur.col;
                op != Op::Yank
            }
            Act::Cmd(s) => self.command(&s, cmd.count, cmd.reg),
        }
    }

    fn insert_key(&mut self, k: char) {
        if self.recording && !self.batch {
            self.rec.push(k);
        }
        let Pos { row, col } = self.cur;
        let len = self.len(row);
        match k {
            ESC | '\x03' => {
                if self.recording && !self.batch {
                    self.dot = std::mem::take(&mut self.rec);
                    self.recording = false;
                }
                self.mode = Mode::Normal;
                if !self.batch {
                    self.buf.end();
                }
                self.cur.col = col.saturating_sub(1);
                self.want_col = self.cur.col;
            }
            ENTER => {
                let indent = if self.opts.autoindent {
                    let i = self.indent_of(row);
                    let n = char_len(&i).min(col);
                    i.chars().take(n).collect()
                } else {
                    String::new()
                };
                self.cur = self.buf.insert(self.cur, &format!("\n{indent}"));
            }
            BS => {
                if col > 0 {
                    self.buf.delete(Pos::new(row, col - 1), self.cur);
                    self.cur.col -= 1;
                } else if row > 0 {
                    let l = self.len(row - 1);
                    self.buf.delete(Pos::new(row - 1, l), self.cur);
                    self.cur = Pos::new(row - 1, l);
                }
            }
            DEL => {
                if col < len {
                    self.buf.delete(self.cur, Pos::new(row, col + 1));
                } else if row < self.last() {
                    self.buf.delete(self.cur, Pos::new(row + 1, 0));
                }
            }
            TAB => {
                let text = if self.opts.expandtab {
                    let ts = self.opts.tabstop.max(1);
                    let x = display_col(self.line(row), col, ts);
                    " ".repeat(ts - x % ts)
                } else {
                    "\t".to_string()
                };
                self.cur = self.buf.insert(self.cur, &text);
            }
            '\x17' => {
                // Ctrl+W: delete the word before the cursor.
                if col == 0 {
                    return self.insert_key_raw(BS);
                }
                let chars: Vec<char> = self.line(row).chars().collect();
                let mut i = col;
                while i > 0 && chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                if i > 0 {
                    let c = class(chars[i - 1], false);
                    while i > 0 && class(chars[i - 1], false) == c {
                        i -= 1;
                    }
                }
                self.buf.delete(Pos::new(row, i), self.cur);
                self.cur.col = i;
            }
            '\x15' => {
                // Ctrl+U: delete back to the indent, or to the line start.
                let fnb = self.first_non_blank(row);
                let to = if col > fnb { fnb } else { 0 };
                self.buf.delete(Pos::new(row, to), self.cur);
                self.cur.col = to;
            }
            LEFT | RIGHT | UP | DOWN | HOME | END | PAGE_UP | PAGE_DOWN => {
                // Moving ends the change, for undo and `.`.
                if self.recording && !self.batch {
                    self.rec.pop();
                    self.rec.push(ESC);
                    self.dot = std::mem::take(&mut self.rec);
                    self.recording = false;
                }
                if !self.batch {
                    self.buf.end();
                }
                let h = self.height.max(2) - 1;
                match k {
                    LEFT => self.cur.col = col.saturating_sub(1),
                    RIGHT => self.cur.col = (col + 1).min(len),
                    HOME => self.cur.col = 0,
                    END => self.cur.col = len,
                    _ => {
                        let r = match k {
                            UP => row.saturating_sub(1),
                            DOWN => (row + 1).min(self.last()),
                            PAGE_UP => row.saturating_sub(h),
                            _ => (row + h).min(self.last()),
                        };
                        self.cur = Pos::new(r, self.want(r));
                    }
                }
                if matches!(k, LEFT | RIGHT | HOME | END) {
                    self.want_col = self.cur.col;
                }
                if !self.batch {
                    self.buf.begin(self.cur);
                }
            }
            c if is_text(c) => {
                if self.mode == Mode::Replace && col < len {
                    self.buf.delete(self.cur, Pos::new(row, col + 1));
                }
                self.cur = self.buf.insert(self.cur, c.encode_utf8(&mut [0; 4]));
                self.want_col = self.cur.col;
            }
            _ => {}
        }
    }

    /// `insert_key` without recording the key again.
    fn insert_key_raw(&mut self, k: char) {
        let rec = std::mem::replace(&mut self.recording, false);
        self.insert_key(k);
        self.recording = rec;
    }

    fn cmdline_key(&mut self, k: char) {
        let search = matches!(self.mode, Mode::Search { .. });
        match k {
            ESC | '\x03' => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
                self.history_pos = None;
            }
            ENTER => {
                let line = std::mem::take(&mut self.cmdline);
                let mode = std::mem::replace(&mut self.mode, Mode::Normal);
                self.history_pos = None;
                let hist = if search {
                    &mut self.search_history
                } else {
                    &mut self.cmd_history
                };
                if !line.is_empty() && hist.last() != Some(&line) {
                    hist.push(line.clone());
                }
                self.msg = None;
                match mode {
                    Mode::Search { forward } => self.search_cmd(&line, forward),
                    _ => {
                        self.buf.begin(self.cur);
                        self.ex(&line);
                        if !self.inserting() {
                            self.buf.end();
                        }
                    }
                }
                self.clamp_cursor();
            }
            BS => {
                if self.cmdline.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            '\x15' => self.cmdline.clear(),
            '\x17' => {
                let t = self.cmdline.trim_end().len();
                let keep = self.cmdline[..t]
                    .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                    .map_or(0, |i| i + 1);
                self.cmdline
                    .truncate(if keep == t { t.saturating_sub(1) } else { keep });
            }
            UP | DOWN => {
                let hist = if search {
                    &self.search_history
                } else {
                    &self.cmd_history
                };
                let pos = match (k, self.history_pos) {
                    (UP, None) => hist.len().checked_sub(1),
                    (UP, Some(p)) => Some(p.saturating_sub(1)),
                    (_, Some(p)) if p + 1 < hist.len() => Some(p + 1),
                    _ => None,
                };
                self.cmdline = pos.map(|p| hist[p].clone()).unwrap_or_default();
                self.history_pos = pos;
            }
            TAB => self.cmdline.push(' '),
            c if is_text(c) => self.cmdline.push(c),
            _ => {}
        }
    }

    // ----- motions --------------------------------------------------------------------------

    fn motion_target(&mut self, m: Motion, count: Option<usize>) -> Option<(Pos, Kind)> {
        use Kind::*;
        use Motion::*;
        let n = count.unwrap_or(1).max(1);
        let cur = self.cur;
        let last = self.last();
        let at_fnb = |v: &Self, r: usize| Pos::new(r, v.first_non_blank(r));
        let repeat = |v: &Self, f: &dyn Fn(&Self, Pos) -> Pos| (0..n).fold(cur, |p, _| f(v, p));
        Some(match m {
            Left => (Pos::new(cur.row, cur.col.saturating_sub(n)), Exclusive),
            Right => (
                Pos::new(cur.row, (cur.col + n).min(self.len(cur.row))),
                Exclusive,
            ),
            Up => {
                if cur.row == 0 {
                    return None;
                }
                let r = cur.row.saturating_sub(n);
                (Pos::new(r, self.want(r)), Linewise)
            }
            Down => {
                if cur.row == last {
                    return None;
                }
                let r = (cur.row + n).min(last);
                (Pos::new(r, self.want(r)), Linewise)
            }
            WordFwd(big) => (repeat(self, &|v, p| v.word_fwd(p, big)), Exclusive),
            WordBack(big) => (repeat(self, &|v, p| v.word_back(p, big)), Exclusive),
            WordEnd(big) => (repeat(self, &|v, p| v.word_end(p, big)), Inclusive),
            WordEndBack(big) => (repeat(self, &|v, p| v.word_end_back(p, big)), Inclusive),
            LineStart => (Pos::new(cur.row, 0), Exclusive),
            FirstNonBlank => (at_fnb(self, cur.row), Exclusive),
            LineEnd => {
                let r = (cur.row + n - 1).min(last);
                (Pos::new(r, self.len(r).saturating_sub(1)), Inclusive)
            }
            LastNonBlank => {
                let r = (cur.row + n - 1).min(last);
                let c = char_len(self.line(r).trim_end()).saturating_sub(1);
                (Pos::new(r, c), Inclusive)
            }
            FileStart => (at_fnb(self, count.map_or(0, |c| c - 1).min(last)), Linewise),
            FileEnd => (
                at_fnb(self, count.map_or(last, |c| c - 1).min(last)),
                Linewise,
            ),
            ScreenTop => (at_fnb(self, (self.top + n - 1).min(last)), Linewise),
            ScreenMid => {
                let visible = self.height.min(self.buf.line_count() - self.top);
                (
                    at_fnb(self, self.top + visible.saturating_sub(1) / 2),
                    Linewise,
                )
            }
            ScreenBottom => {
                let bottom = (self.top + self.height.max(1) - 1).min(last);
                (at_fnb(self, bottom.saturating_sub(n - 1)), Linewise)
            }
            ParaFwd => (repeat(self, &|v, p| v.para(p, true)), Exclusive),
            ParaBack => (repeat(self, &|v, p| v.para(p, false)), Exclusive),
            MatchPair => match count {
                Some(c) => {
                    let r = (c.min(100) * self.buf.line_count()).div_ceil(100);
                    (at_fnb(self, r.saturating_sub(1)), Linewise)
                }
                None => (self.match_pair(cur)?, Inclusive),
            },
            SearchNext | SearchPrev => {
                let fwd = self.last_forward == (m == SearchNext);
                match self.search(fwd, n) {
                    Ok(p) => (p, Exclusive),
                    Err(e) => {
                        self.err(e);
                        return None;
                    }
                }
            }
            StarFwd | StarBack => {
                let (start, word) = self.word_under_cursor()?;
                self.last_search = Some(format!(r"\<{}\>", pattern::escape_literal(&word)));
                self.last_forward = m == StarFwd;
                self.hl = true;
                let from = if m == StarBack { start } else { cur };
                let saved = std::mem::replace(&mut self.cur, from);
                let found = self.search(m == StarFwd, n);
                self.cur = saved;
                match found {
                    Ok(p) => (p, Exclusive),
                    Err(e) => {
                        self.err(e);
                        return None;
                    }
                }
            }
            Find(k, c) => {
                self.last_find = Some((k, c));
                self.find(k, c, n, false)?
            }
            FindRepeat | FindRepeatRev => {
                let (k, c) = self.last_find?;
                let k = if m == FindRepeat {
                    k
                } else {
                    match k {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                };
                self.find(k, c, n, true)?
            }
            NextLine => {
                if cur.row == last {
                    return None;
                }
                (at_fnb(self, (cur.row + n).min(last)), Linewise)
            }
            PrevLine => {
                if cur.row == 0 {
                    return None;
                }
                (at_fnb(self, cur.row.saturating_sub(n)), Linewise)
            }
            CurLine => (at_fnb(self, (cur.row + n - 1).min(last)), Linewise),
            Mark(c, exact) => {
                let Some(&p) = self.marks.get(&c) else {
                    self.err("E20: Mark not set");
                    return None;
                };
                let p = Pos::new(p.row.min(last), p.col);
                if exact {
                    (p, Exclusive)
                } else {
                    (at_fnb(self, p.row), Linewise)
                }
            }
        })
    }

    fn end_pos(&self) -> Pos {
        Pos::new(self.last(), self.len(self.last()))
    }

    fn word_fwd(&self, start: Pos, big: bool) -> Pos {
        let mut p = start;
        let c0 = self.cls(p, big);
        if c0 != 0 {
            loop {
                match self.next(p) {
                    Some(n) => {
                        p = n;
                        if self.cls(p, big) != c0 {
                            break;
                        }
                    }
                    None => return self.end_pos(),
                }
            }
        }
        loop {
            if self.cls(p, big) != 0 || (self.is_empty_line(p) && p != start) {
                return p;
            }
            match self.next(p) {
                Some(n) => p = n,
                None => return self.end_pos(),
            }
        }
    }

    fn word_end(&self, start: Pos, big: bool) -> Pos {
        let Some(mut p) = self.next(start) else {
            return start;
        };
        while self.cls(p, big) == 0 {
            match self.next(p) {
                Some(n) => p = n,
                None => return p,
            }
        }
        let c = self.cls(p, big);
        while let Some(n) = self.next(p).filter(|&n| self.cls(n, big) == c) {
            p = n;
        }
        p
    }

    fn word_back(&self, start: Pos, big: bool) -> Pos {
        let Some(mut p) = self.prev(start) else {
            return start;
        };
        while self.cls(p, big) == 0 {
            if self.is_empty_line(p) {
                return p;
            }
            match self.prev(p) {
                Some(n) => p = n,
                None => return p,
            }
        }
        let c = self.cls(p, big);
        while let Some(n) = self.prev(p).filter(|&n| self.cls(n, big) == c) {
            p = n;
        }
        p
    }

    fn word_end_back(&self, start: Pos, big: bool) -> Pos {
        let mut p = start;
        let c0 = self.cls(p, big);
        if c0 != 0 {
            loop {
                match self.prev(p) {
                    Some(n) => {
                        p = n;
                        if self.cls(p, big) != c0 {
                            break;
                        }
                    }
                    None => return Pos::default(),
                }
            }
        }
        loop {
            if self.cls(p, big) != 0 || (self.is_empty_line(p) && p != start) {
                return p;
            }
            match self.prev(p) {
                Some(n) => p = n,
                None => return p,
            }
        }
    }

    /// The end of the word at `p` (for `cw`, which changes to the end of the current word).
    fn word_end_here(&self, p: Pos, big: bool) -> Pos {
        let c = self.cls(p, big);
        let mut e = p;
        while let Some(n) = self
            .next(e)
            .filter(|&n| n.row == p.row && self.cls(n, big) == c)
        {
            e = n;
        }
        e
    }

    fn para(&self, p: Pos, forward: bool) -> Pos {
        let blank = |r: usize| self.line(r).is_empty();
        let mut r = p.row;
        if forward {
            let last = self.last();
            while r < last && blank(r) {
                r += 1;
            }
            while r < last && !blank(r) {
                r += 1;
            }
            if r == last && !blank(r) {
                return Pos::new(r, self.len(r));
            }
        } else {
            while r > 0 && blank(r) {
                r -= 1;
            }
            while r > 0 && !blank(r) {
                r -= 1;
            }
        }
        Pos::new(r, 0)
    }

    fn find(&self, kind: char, c: char, n: usize, repeat: bool) -> Option<(Pos, Kind)> {
        let chars: Vec<char> = self.line(self.cur.row).chars().collect();
        let col = self.cur.col;
        let till = matches!(kind, 't' | 'T');
        let skip = usize::from(repeat && till);
        let row = self.cur.row;
        if matches!(kind, 'f' | 't') {
            let i = (col + 1 + skip..chars.len())
                .filter(|&i| chars[i] == c)
                .nth(n - 1)?;
            Some((Pos::new(row, if till { i - 1 } else { i }), Kind::Inclusive))
        } else {
            let i = (0..col.saturating_sub(skip))
                .rev()
                .filter(|&i| chars[i] == c)
                .nth(n - 1)?;
            Some((Pos::new(row, if till { i + 1 } else { i }), Kind::Exclusive))
        }
    }

    fn match_pair(&self, from: Pos) -> Option<Pos> {
        let pair = |c: char| match c {
            '(' | ')' => Some(('(', ')')),
            '[' | ']' => Some(('[', ']')),
            '{' | '}' => Some(('{', '}')),
            _ => None,
        };
        let col = self
            .line(from.row)
            .chars()
            .enumerate()
            .skip(from.col)
            .find(|(_, c)| pair(*c).is_some())?
            .0;
        let mut p = Pos::new(from.row, col);
        let c = self.ch(p);
        let (open, close) = pair(c)?;
        let forward = c == open;
        let (me, other) = if forward {
            (open, close)
        } else {
            (close, open)
        };
        let mut depth = 0;
        loop {
            let c = self.ch(p);
            if c == me {
                depth += 1;
            } else if c == other {
                depth -= 1;
                if depth == 0 {
                    return Some(p);
                }
            }
            p = if forward {
                self.next(p)?
            } else {
                self.prev(p)?
            };
        }
    }

    /// The keyword under (or after) the cursor and where it starts.
    fn word_under_cursor(&self) -> Option<(Pos, String)> {
        let chars: Vec<char> = self.line(self.cur.row).chars().collect();
        let kw = |c: char| class(c, false) == 1;
        let mut s = (self.cur.col..chars.len()).find(|&i| kw(chars[i]))?;
        while s > 0 && kw(chars[s - 1]) {
            s -= 1;
        }
        let e = (s..chars.len())
            .find(|&i| !kw(chars[i]))
            .unwrap_or(chars.len());
        Some((Pos::new(self.cur.row, s), chars[s..e].iter().collect()))
    }

    // ----- operators and text objects -------------------------------------------------------

    fn op_range(&mut self, op: Op, target: Target, count: Option<usize>) -> Option<Range> {
        let n = count.unwrap_or(1).max(1);
        let cur = self.cur;
        match target {
            Target::Line => Some(Range::lines(cur.row, (cur.row + n - 1).min(self.last()))),
            Target::Visual(lines) => {
                let r = self.visual_range();
                Some(if lines {
                    Range::lines(r.a.row, r.b.row)
                } else {
                    r
                })
            }
            Target::Object(inner, o) => self.text_object(inner, o),
            Target::Motion(m) => {
                if let (Op::Change, Motion::WordFwd(big)) = (op, m) {
                    if self.cls(cur, big) != 0 {
                        let mut e = self.word_end_here(cur, big);
                        for _ in 1..n {
                            e = self.word_end(e, big);
                        }
                        return Some(Range::chars(cur, Pos::new(e.row, e.col + 1)));
                    }
                }
                let (t, kind) = self.motion_target(m, count)?;
                let (a, mut b) = if t < cur { (t, cur) } else { (cur, t) };
                Some(match kind {
                    Kind::Linewise => Range::lines(a.row, b.row),
                    Kind::Inclusive => Range::chars(a, Pos::new(b.row, b.col + 1)),
                    Kind::Exclusive => {
                        // `dw` on the last word of a line stops at the line end.
                        if matches!(m, Motion::WordFwd(_)) && b.row > a.row {
                            let at_end = b == self.end_pos();
                            if at_end || b.col <= self.first_non_blank(b.row) {
                                let mut r = if at_end { b.row } else { b.row - 1 };
                                while r > a.row && self.len(r) == 0 {
                                    r -= 1;
                                }
                                b = Pos::new(r, self.len(r));
                            }
                        }
                        if b.row > a.row && b.col == 0 {
                            if a.col <= self.first_non_blank(a.row) {
                                Range::lines(a.row, b.row - 1)
                            } else {
                                Range::chars(a, Pos::new(b.row - 1, self.len(b.row - 1)))
                            }
                        } else {
                            Range::chars(a, b)
                        }
                    }
                })
            }
        }
    }

    fn text_object(&self, inner: bool, o: char) -> Option<Range> {
        let row = self.cur.row;
        match o {
            'w' | 'W' => {
                let chars: Vec<char> = self.line(row).chars().collect();
                if chars.is_empty() {
                    return None;
                }
                let big = o == 'W';
                let col = self.cur.col.min(chars.len() - 1);
                let k = class(chars[col], big);
                let same = |i: usize| class(chars[i], big) == k;
                let blank = |i: usize| class(chars[i], big) == 0;
                let mut s = col;
                while s > 0 && same(s - 1) {
                    s -= 1;
                }
                let mut e = col;
                while e + 1 < chars.len() && same(e + 1) {
                    e += 1;
                }
                if !inner {
                    if k == 0 {
                        while e + 1 < chars.len() && !blank(e + 1) {
                            e += 1;
                        }
                    } else if e + 1 < chars.len() && blank(e + 1) {
                        while e + 1 < chars.len() && blank(e + 1) {
                            e += 1;
                        }
                    } else {
                        while s > 0 && blank(s - 1) {
                            s -= 1;
                        }
                    }
                }
                Some(Range::chars(Pos::new(row, s), Pos::new(row, e + 1)))
            }
            '"' | '\'' | '`' => {
                let chars: Vec<char> = self.line(row).chars().collect();
                let qs: Vec<usize> = (0..chars.len())
                    .filter(|&i| chars[i] == o && (i == 0 || chars[i - 1] != '\\'))
                    .collect();
                let col = self.cur.col;
                let pairs = || qs.chunks(2).filter(|p| p.len() == 2);
                let p = pairs()
                    .find(|p| p[0] <= col && col <= p[1])
                    .or_else(|| pairs().find(|p| p[0] > col))?;
                let (a, b) = if inner {
                    (p[0] + 1, p[1])
                } else {
                    (p[0], p[1] + 1)
                };
                Some(Range::chars(Pos::new(row, a), Pos::new(row, b)))
            }
            'b' | '(' | ')' => self.bracket_object(inner, '(', ')'),
            'B' | '{' | '}' => self.bracket_object(inner, '{', '}'),
            '[' | ']' => self.bracket_object(inner, '[', ']'),
            '<' | '>' => self.bracket_object(inner, '<', '>'),
            'p' => {
                let blank = |r: usize| self.line(r).trim().is_empty();
                let k = blank(row);
                let (mut s, mut e) = (row, row);
                while s > 0 && blank(s - 1) == k {
                    s -= 1;
                }
                while e < self.last() && blank(e + 1) == k {
                    e += 1;
                }
                if !inner {
                    while e < self.last() && blank(e + 1) != k {
                        e += 1;
                    }
                }
                Some(Range::lines(s, e))
            }
            _ => None,
        }
    }

    /// The innermost `open`...`close` pair around the cursor.
    fn enclosing(&self, open: char, close: char) -> Option<(Pos, Pos)> {
        let mut p = self.cur;
        let mut depth = 0;
        let start = loop {
            let c = self.ch(p);
            if c == close && p != self.cur {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    break p;
                }
                depth -= 1;
            }
            p = self.prev(p)?;
        };
        let mut p = start;
        let mut depth = 0;
        loop {
            let c = self.ch(p);
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some((start, p));
                }
            }
            p = self.next(p)?;
        }
    }

    fn bracket_object(&self, inner: bool, open: char, close: char) -> Option<Range> {
        let (a, b) = self.enclosing(open, close)?;
        if !inner {
            return Some(Range::chars(a, Pos::new(b.row, b.col + 1)));
        }
        let start = Pos::new(a.row, a.col + 1);
        let close_line = self.line(b.row);
        // `ci{` on a block: the lines in between.
        if start.col >= self.len(a.row)
            && b.row > a.row + 1
            && close_line[..byte_of(close_line, b.col)].trim().is_empty()
        {
            return Some(Range::lines(a.row + 1, b.row - 1));
        }
        Some(Range::chars(start, b))
    }

    fn set_reg(&mut self, reg: Option<char>, val: Register, yank: bool) {
        match reg {
            Some('_') => return,
            Some(c @ 'a'..='z') => {
                self.regs.insert(c, val.clone());
            }
            Some(c @ 'A'..='Z') => {
                let r = self.regs.entry(c.to_ascii_lowercase()).or_default();
                if r.linewise || val.linewise {
                    if !r.text.is_empty() {
                        r.text.push('\n');
                    }
                    r.linewise = true;
                }
                r.text.push_str(&val.text);
            }
            _ => {}
        }
        if yank {
            self.regs.insert('0', val.clone());
        }
        self.regs.insert('"', val);
    }

    pub(super) fn get_reg(&self, reg: Option<char>) -> Option<Register> {
        let name = match reg {
            Some(c @ ('a'..='z' | 'A'..='Z' | '0')) => c.to_ascii_lowercase(),
            _ => '"',
        };
        self.regs.get(&name).cloned()
    }

    pub(super) fn yank(&mut self, r: &Range, reg: Option<char>, yank: bool) {
        let text = if r.lines {
            self.buf.lines()[r.a.row..=r.b.row.min(self.last())].join("\n")
        } else {
            self.buf.text(r.a, r.b)
        };
        self.set_reg(
            reg,
            Register {
                text,
                linewise: r.lines,
            },
            yank,
        );
    }

    pub(super) fn apply_op(&mut self, op: Op, r: Range, reg: Option<char>) {
        let last = self.last();
        let (r1, r2) = (r.a.row.min(last), r.b.row.min(last));
        match op {
            Op::Yank => {
                self.yank(&r, reg, true);
                if !r.lines {
                    self.cur = r.a;
                } else if r1 < self.cur.row {
                    self.cur.row = r1;
                }
                let lines = r2 - r1 + 1;
                if r.lines && lines > 2 {
                    self.info(format!("{lines} lines yanked"));
                }
            }
            Op::Delete => {
                self.yank(&r, reg, false);
                if r.lines {
                    self.buf.delete_lines(r1, r2);
                    let row = r1.min(self.last());
                    self.cur = Pos::new(row, self.first_non_blank(row));
                    if r2 - r1 + 1 > 2 {
                        self.info(format!("{} fewer lines", r2 - r1 + 1));
                    }
                } else {
                    self.buf.delete(r.a, r.b);
                    self.cur = r.a;
                }
            }
            Op::Change => {
                self.yank(&r, reg, false);
                if r.lines {
                    let indent = if self.opts.autoindent {
                        self.indent_of(r1)
                    } else {
                        String::new()
                    };
                    self.cur = Pos::new(r1, char_len(&indent));
                    self.buf.replace_lines(r1, r2, vec![indent]);
                } else {
                    self.buf.delete(r.a, r.b);
                    self.cur = r.a;
                }
                self.mode = Mode::Insert;
            }
            Op::Indent | Op::Outdent => {
                for row in r1..=r2 {
                    self.shift(row, op == Op::Indent);
                }
                self.cur = Pos::new(r1, self.first_non_blank(r1));
            }
            Op::Upper | Op::Lower | Op::Toggle => {
                let (a, b) = if r.lines {
                    (Pos::new(r1, 0), Pos::new(r2, self.len(r2)))
                } else {
                    (r.a, r.b)
                };
                let text = self.buf.text(a, b);
                let mut new = String::with_capacity(text.len());
                for c in text.chars() {
                    let up = match op {
                        Op::Upper => true,
                        Op::Lower => false,
                        _ => !c.is_uppercase(),
                    };
                    if up {
                        new.extend(c.to_uppercase());
                    } else {
                        new.extend(c.to_lowercase());
                    }
                }
                if new != text {
                    self.buf.delete(a, b);
                    self.buf.insert(a, &new);
                }
                self.cur = a;
            }
        }
    }

    /// Shift a line one 'shiftwidth' right or left (empty lines stay empty).
    pub(super) fn shift(&mut self, row: usize, right: bool) {
        let line = self.line(row).to_string();
        if line.is_empty() {
            return;
        }
        let body = line.trim_start();
        let ts = self.opts.tabstop.max(1);
        let width = display_col(&line, char_len(&line) - char_len(body), ts);
        let sw = self.opts.shiftwidth.max(1);
        let new = if right {
            width + sw
        } else {
            width.saturating_sub(sw)
        };
        let s = self.make_indent(new) + body;
        self.buf.set_line(row, s);
    }

    // ----- simple commands ------------------------------------------------------------------

    /// Run a command that is neither a motion nor an operator; true for repeatable changes.
    fn command(&mut self, s: &str, count: Option<usize>, reg: Option<char>) -> bool {
        let n = count.unwrap_or(1).max(1);
        let Pos { row, col } = self.cur;
        let len = self.len(row);
        let visual = self.is_visual();
        let mut it = s.chars();
        let k = it.next().unwrap_or_default();
        let arg = it.next();
        let h = self.height.max(2);
        match k {
            'x' | DEL if len > 0 => {
                let end = Pos::new(row, (col + n).min(len));
                self.apply_op(Op::Delete, Range::chars(self.cur, end), reg);
            }
            'X' if col > 0 => {
                let start = Pos::new(row, col.saturating_sub(n));
                self.apply_op(Op::Delete, Range::chars(start, self.cur), reg);
            }
            'D' | 'C' => {
                let r2 = (row + n - 1).min(self.last());
                let r = Range::chars(self.cur, Pos::new(r2, self.len(r2)));
                let op = if k == 'D' { Op::Delete } else { Op::Change };
                self.apply_op(op, r, reg);
            }
            'Y' => {
                let r = Range::lines(row, (row + n - 1).min(self.last()));
                self.apply_op(Op::Yank, r, reg);
                return false;
            }
            's' => {
                let r = Range::chars(self.cur, Pos::new(row, (col + n).min(len)));
                self.apply_op(Op::Change, r, reg);
            }
            'S' => {
                let r = Range::lines(row, (row + n - 1).min(self.last()));
                self.apply_op(Op::Change, r, reg);
            }
            'p' | 'P' if visual => {
                self.visual_put(reg);
                return false;
            }
            'p' | 'P' => self.put(k == 'P', n, reg),
            'i' | 'a' if visual => {
                // `viw`, `va(`...: select a text object.
                if let Some(r) = arg.and_then(|o| self.text_object(k == 'i', o)) {
                    if r.lines {
                        self.anchor = r.a;
                        self.cur = Pos::new(r.b.row, 0);
                        self.mode = Mode::VisualLine;
                    } else if r.b > r.a {
                        self.anchor = r.a;
                        self.cur = self.prev(r.b).unwrap_or(r.a);
                    }
                }
                return false;
            }
            'i' => self.mode = Mode::Insert,
            'a' => {
                self.mode = Mode::Insert;
                self.cur.col = (col + 1).min(len);
            }
            'I' => {
                self.mode = Mode::Insert;
                self.cur.col = if len == 0 {
                    0
                } else {
                    self.first_non_blank(row)
                };
            }
            'A' => {
                self.mode = Mode::Insert;
                self.cur.col = len;
            }
            'o' if visual => {
                std::mem::swap(&mut self.anchor, &mut self.cur);
                return false;
            }
            'o' | 'O' => {
                let indent = if self.opts.autoindent {
                    self.indent_of(row)
                } else {
                    String::new()
                };
                let at = if k == 'o' { row + 1 } else { row };
                self.cur = Pos::new(at, char_len(&indent));
                self.buf.insert_lines(at, vec![indent]);
                self.mode = Mode::Insert;
            }
            'J' if visual => {
                let r = self.visual_range();
                self.exit_visual();
                self.join(r.a.row, (r.b.row - r.a.row + 1).max(2), true);
                return false;
            }
            'J' => self.join(row, n.max(2), true),
            'g' if arg == Some('J') => self.join(row, n.max(2), false),
            'g' if arg == Some('v') => {
                if let Some((a, c, lines)) = self.last_visual {
                    self.anchor = a;
                    self.cur = c;
                    self.mode = if lines {
                        Mode::VisualLine
                    } else {
                        Mode::Visual
                    };
                }
                return false;
            }
            'r' if visual => {
                let c = arg.unwrap_or(' ');
                let r = self.visual_range();
                self.exit_visual();
                let (a, b) = if r.lines {
                    (Pos::new(r.a.row, 0), Pos::new(r.b.row, self.len(r.b.row)))
                } else {
                    (r.a, r.b)
                };
                let text: String = self
                    .buf
                    .text(a, b)
                    .chars()
                    .map(|x| if x == '\n' { x } else { c })
                    .collect();
                self.buf.delete(a, b);
                self.buf.insert(a, &text);
                self.cur = a;
                return false;
            }
            'r' => {
                let c = arg.unwrap_or(' ');
                if col + n > len {
                    return false;
                }
                self.buf.delete(self.cur, Pos::new(row, col + n));
                if c == ENTER {
                    self.buf.insert(self.cur, "\n");
                    self.cur = Pos::new(row + 1, 0);
                } else {
                    self.buf.insert(self.cur, &c.to_string().repeat(n));
                    self.cur.col = col + n - 1;
                }
            }
            '~' if len > 0 => {
                let end = Pos::new(row, (col + n).min(len));
                self.apply_op(Op::Toggle, Range::chars(self.cur, end), None);
                self.cur.col = end.col;
            }
            'u' => {
                for _ in 0..n {
                    match self.buf.undo() {
                        Some(p) => self.cur = p,
                        None => {
                            self.info("Already at oldest change");
                            break;
                        }
                    }
                }
                return false;
            }
            '\x12' => {
                for _ in 0..n {
                    match self.buf.redo() {
                        Some(p) => self.cur = p,
                        None => {
                            self.info("Already at newest change");
                            break;
                        }
                    }
                }
                return false;
            }
            '.' => {
                let keys = self.dot.clone();
                let was = std::mem::replace(&mut self.batch, true);
                for _ in 0..n {
                    for k in keys.chars() {
                        self.feed(k);
                    }
                    self.finish_keys();
                }
                self.batch = was;
                return false;
            }
            'v' | 'V' => {
                let want = if k == 'v' {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
                if self.mode == want {
                    self.exit_visual();
                } else {
                    if !visual {
                        self.anchor = self.cur;
                    }
                    self.mode = want;
                }
                return false;
            }
            ':' => {
                self.cmdline = if visual {
                    self.exit_visual();
                    "'<,'>".into()
                } else if n > 1 {
                    format!(".,.+{}", n - 1)
                } else {
                    String::new()
                };
                self.mode = Mode::Command;
                return false;
            }
            '/' | '?' => {
                self.exit_visual();
                self.cmdline.clear();
                self.mode = Mode::Search { forward: k == '/' };
                return false;
            }
            'm' => {
                if let Some(c) = arg {
                    self.marks.insert(c, self.cur);
                }
                return false;
            }
            'Z' => {
                self.ex(if arg == Some('Z') { "x" } else { "q!" });
                return false;
            }
            'z' => {
                self.top = match arg {
                    Some('t' | ENTER) => row,
                    Some('b' | '-') => (row + 1).saturating_sub(h),
                    _ => row.saturating_sub(h / 2),
                };
                return false;
            }
            '\x04' | '\x15' => {
                let half = (h / 2).max(1) as isize;
                let d = if k == '\x04' { half } else { -half };
                self.top = self.top.saturating_add_signed(d);
                self.cur.row = self.cur.row.saturating_add_signed(d).min(self.last());
                self.cur.col = self.want(self.cur.row);
                return false;
            }
            '\x06' | '\x02' | PAGE_DOWN | PAGE_UP => {
                let page = (h.saturating_sub(2).max(1) * n) as isize;
                let d = if matches!(k, '\x06' | PAGE_DOWN) {
                    page
                } else {
                    -page
                };
                self.top = self.top.saturating_add_signed(d).min(self.last());
                self.cur.row = self.cur.row.saturating_add_signed(d).min(self.last());
                self.cur.col = self.want(self.cur.row);
                return false;
            }
            '\x05' => {
                self.scroll(n as isize);
                return false;
            }
            '\x19' => {
                self.scroll(-(n as isize));
                return false;
            }
            '\x07' => {
                let lines = self.buf.line_count();
                let modified = if self.buf.modified() {
                    " [Modified]"
                } else {
                    ""
                };
                let pct = (row + 1) * 100 / lines;
                self.info(format!(
                    "\"{}\"{modified} {lines} lines --{pct}%--",
                    self.name
                ));
                return false;
            }
            '\x01' | '\x18' => {
                let d = if k == '\x01' { n as i64 } else { -(n as i64) };
                return self.increment(d);
            }
            'R' => self.mode = Mode::Replace,
            '&' => self.ex("s"),
            _ => return false,
        }
        true
    }

    pub(super) fn put(&mut self, before: bool, n: usize, reg: Option<char>) {
        let Some(r) = self.get_reg(reg) else {
            self.err("E353: Nothing in register");
            return;
        };
        if r.linewise {
            let lines: Vec<String> = (0..n)
                .flat_map(|_| r.text.split('\n').map(String::from))
                .collect();
            let at = if before {
                self.cur.row
            } else {
                self.cur.row + 1
            };
            self.buf.insert_lines(at, lines);
            self.cur = Pos::new(at, self.first_non_blank(at));
        } else {
            let text = r.text.repeat(n);
            let at = if before || self.len(self.cur.row) == 0 {
                self.cur
            } else {
                Pos::new(self.cur.row, self.cur.col + 1)
            };
            let end = self.buf.insert(at, &text);
            self.cur = if text.contains('\n') {
                at
            } else {
                Pos::new(end.row, end.col.saturating_sub(1))
            };
        }
    }

    /// `p` in visual mode: replace the selection with a register.
    fn visual_put(&mut self, reg: Option<char>) {
        let Some(val) = self.get_reg(reg) else {
            self.err("E353: Nothing in register");
            return;
        };
        let r = self.visual_range();
        self.exit_visual();
        let whole = r.lines && r.a.row == 0 && r.b.row >= self.last();
        self.apply_op(Op::Delete, r, None);
        if r.lines {
            let lines: Vec<String> = val.text.split('\n').map(String::from).collect();
            self.buf.insert_lines(r.a.row, lines);
            if whole {
                self.buf.delete_lines(self.last(), self.last());
            }
            self.cur = Pos::new(r.a.row, self.first_non_blank(r.a.row));
        } else if val.linewise {
            self.buf.insert(r.a, &format!("\n{}\n", val.text));
            self.cur = Pos::new(r.a.row + 1, 0);
        } else {
            let end = self.buf.insert(r.a, &val.text);
            self.cur = Pos::new(end.row, end.col.saturating_sub(1));
        }
    }

    /// Join `count` lines starting at `r1`; with `spaces`, trims the joined lines' indent and
    /// puts one space between.
    pub(super) fn join(&mut self, r1: usize, count: usize, spaces: bool) {
        let r2 = (r1 + count - 1).min(self.last());
        if r2 <= r1 {
            return;
        }
        let mut s = self.line(r1).to_string();
        let mut col = 0;
        for r in r1 + 1..=r2 {
            let next = self.line(r);
            let next = if spaces { next.trim_start() } else { next };
            if spaces
                && !s.is_empty()
                && !next.is_empty()
                && !s.ends_with([' ', '\t'])
                && !next.starts_with(')')
            {
                s.push(' ');
                col = char_len(&s) - 1;
            } else {
                col = char_len(&s);
            }
            s.push_str(next);
        }
        self.buf.replace_lines(r1, r2, vec![s]);
        self.cur = Pos::new(r1, col);
    }

    /// Ctrl+A / Ctrl+X: add `delta` to the number at or after the cursor.
    fn increment(&mut self, delta: i64) -> bool {
        let row = self.cur.row;
        let chars: Vec<char> = self.line(row).chars().collect();
        let Some(mut s) = (self.cur.col..chars.len()).find(|&i| chars[i].is_ascii_digit()) else {
            return false;
        };
        while s > 0 && chars[s - 1].is_ascii_digit() {
            s -= 1;
        }
        let e = (s..chars.len())
            .find(|&i| !chars[i].is_ascii_digit())
            .unwrap_or(chars.len());
        let a = if s > 0 && chars[s - 1] == '-' {
            s - 1
        } else {
            s
        };
        let Ok(v) = chars[a..e].iter().collect::<String>().parse::<i64>() else {
            return false;
        };
        let new = v.saturating_add(delta).to_string();
        self.buf.delete(Pos::new(row, a), Pos::new(row, e));
        self.buf.insert(Pos::new(row, a), &new);
        self.cur.col = a + char_len(&new) - 1;
        true
    }

    // ----- search ---------------------------------------------------------------------------

    fn search_cmd(&mut self, pat: &str, forward: bool) {
        if !pat.is_empty() {
            self.last_search = Some(pat.to_string());
        }
        self.last_forward = forward;
        self.hl = true;
        match self.search(forward, 1) {
            Ok(p) => {
                self.jump(p);
                self.want_col = p.col;
            }
            Err(e) => self.err(e),
        }
    }

    /// The `n`-th match of the last search from the cursor.
    fn search(&mut self, forward: bool, n: usize) -> Result<Pos, String> {
        let pat = self
            .last_search
            .clone()
            .ok_or("E35: No previous regular expression")?;
        let re = self.regex(&pat)?;
        let mut p = self.cur;
        let mut wrapped = false;
        for _ in 0..n {
            let (q, w) = self
                .find_match(&re, p, forward)
                .ok_or_else(|| format!("E486: Pattern not found: {pat}"))?;
            p = q;
            wrapped |= w;
        }
        if wrapped {
            if !self.opts.wrapscan {
                return Err(if forward {
                    format!("E385: Search hit BOTTOM without match for: {pat}")
                } else {
                    format!("E384: Search hit TOP without match for: {pat}")
                });
            }
            self.info(if forward {
                "search hit BOTTOM, continuing at TOP"
            } else {
                "search hit TOP, continuing at BOTTOM"
            });
        } else if !self.batch {
            self.info(format!("{}{pat}", if forward { '/' } else { '?' }));
        }
        Ok(p)
    }

    /// Next match after (or before) `from`, and whether the search wrapped around the file.
    fn find_match(&self, re: &Regex, from: Pos, forward: bool) -> Option<(Pos, bool)> {
        let rows = self.buf.line_count();
        for i in 0..=rows {
            let row = if forward {
                (from.row + i) % rows
            } else {
                (from.row + rows * 2 - i) % rows
            };
            let line = self.line(row);
            let here = byte_of(line, from.col);
            let wrapped = if forward {
                row < from.row || i == rows
            } else {
                row > from.row || i == rows
            };
            let ok = |start: usize| match (i, forward) {
                (0, true) => start > here,
                (0, false) => start < here,
                (i, true) if i == rows => start <= here,
                (i, false) if i == rows => start >= here,
                _ => true,
            };
            let mut found = re.find_iter(line).map(|m| m.start()).filter(|&s| ok(s));
            let start = if forward { found.next() } else { found.last() };
            if let Some(s) = start {
                return Some((Pos::new(row, char_len(&line[..s])), wrapped));
            }
        }
        None
    }

    // ----- files ----------------------------------------------------------------------------

    /// A path typed in a command, relative to the edited file's directory.
    pub(super) fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.path.parent().unwrap_or(Path::new(".")).join(p)
        }
    }

    /// Write the buffer to `target` (the edited file when `None` or empty).
    pub(super) fn write_to(&mut self, target: Option<&str>) -> Result<(), String> {
        let path = match target.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => self.resolve(t),
            None => self.path.clone(),
        };
        let nl = if self.crlf { "\r\n" } else { "\n" };
        let lines = self.buf.lines();
        let mut text = lines.join(nl);
        if self.eol && !(lines.len() == 1 && lines[0].is_empty()) {
            text.push_str(nl);
        }
        std::fs::write(&path, &text)
            .map_err(|e| format!("E212: Can't open file for writing: {e}"))?;
        let n = lines.len();
        let name = if path == self.path {
            self.buf.mark_saved();
            self.name.clone()
        } else {
            path.display().to_string()
        };
        self.info(format!("\"{name}\" {n}L, {}B written", text.len()));
        self.out.wrote = true;
        Ok(())
    }

    /// Reload the file from disk, dropping changes (undoable).
    pub(super) fn reload(&mut self) -> Result<(), String> {
        let (mut lines, crlf, eol) = load(&self.path)
            .map_err(|e| format!("{e:#}"))?
            .ok_or_else(|| format!("E484: Can't open file {}", self.name))?;
        if lines.is_empty() {
            lines.push(String::new());
        }
        let last = self.last();
        self.buf.replace_lines(0, last, lines);
        self.buf.mark_saved();
        self.crlf = crlf;
        self.eol = eol;
        self.clamp_cursor();
        self.info(format!(
            "\"{}\" {}L reloaded",
            self.name,
            self.buf.line_count()
        ));
        Ok(())
    }
}

/// Parse normal-mode keys: `["x][count]` then an operator with a motion or text object, a
/// motion, or a command.
fn parse(s: &str, visual: bool) -> Parse {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut reg = None;
    if c.first() == Some(&'"') {
        match c.get(1) {
            None => return Parse::More,
            Some(&r) => {
                reg = Some(r);
                i = 2;
            }
        }
    }
    let (count, j) = count_at(&c, i);
    let Some(&k) = c.get(j) else {
        return Parse::More;
    };
    let rest = &c[j + 1..];
    let done = |count, act| Parse::Done(Cmd { reg, count, act });

    let op = match (k, rest.first()) {
        ('d', _) => Some((Op::Delete, 0)),
        ('c', _) => Some((Op::Change, 0)),
        ('y', _) => Some((Op::Yank, 0)),
        ('>', _) => Some((Op::Indent, 0)),
        ('<', _) => Some((Op::Outdent, 0)),
        ('g', Some('~')) => Some((Op::Toggle, 1)),
        ('g', Some('u')) => Some((Op::Lower, 1)),
        ('g', Some('U')) => Some((Op::Upper, 1)),
        _ if visual => match k {
            'x' | DEL => Some((Op::Delete, 0)),
            's' => Some((Op::Change, 0)),
            '~' => Some((Op::Toggle, 0)),
            'u' => Some((Op::Lower, 0)),
            'U' => Some((Op::Upper, 0)),
            'X' | 'D' => return done(count, Act::Op(Op::Delete, Target::Visual(true))),
            'Y' => return done(count, Act::Op(Op::Yank, Target::Visual(true))),
            'C' | 'S' | 'R' => return done(count, Act::Op(Op::Change, Target::Visual(true))),
            _ => None,
        },
        _ => None,
    };
    if let Some((op, extra)) = op {
        if visual {
            return done(count, Act::Op(op, Target::Visual(false)));
        }
        let rest = &rest[extra..];
        let (count2, j) = count_at(rest, 0);
        let count = match (count, count2) {
            (Some(a), Some(b)) => Some(a.saturating_mul(b)),
            (a, b) => a.or(b),
        };
        let rest = &rest[j..];
        let Some(&m) = rest.first() else {
            return Parse::More;
        };
        let line_key = match op {
            Op::Toggle => '~',
            Op::Lower => 'u',
            Op::Upper => 'U',
            _ => k,
        };
        if m == line_key {
            return done(count, Act::Op(op, Target::Line));
        }
        if m == 'i' || m == 'a' {
            return match rest.get(1) {
                None => Parse::More,
                Some(&o) => done(count, Act::Op(op, Target::Object(m == 'i', o))),
            };
        }
        return match motion(rest) {
            Err(()) => Parse::More,
            Ok(Some(mo)) => done(count, Act::Op(op, Target::Motion(mo))),
            Ok(None) => Parse::Bad,
        };
    }
    match motion(&c[j..]) {
        Err(()) => return Parse::More,
        Ok(Some(m)) => return done(count, Act::Move(m)),
        Ok(None) => {}
    }
    let two = |set: &str| match rest.first() {
        None => Parse::More,
        Some(&n) if set.is_empty() || set.contains(n) => done(count, Act::Cmd(format!("{k}{n}"))),
        _ => Parse::Bad,
    };
    match k {
        'r' | 'm' => two(""),
        'i' | 'a' if visual => two(""),
        'g' => two("Jv"),
        'z' => two("ztb.-\r"),
        'Z' => two("ZQ"),
        'x' | 'X' | 'D' | 'C' | 'Y' | 's' | 'S' | 'p' | 'P' | 'i' | 'a' | 'I' | 'A' | 'o' | 'O'
        | 'J' | 'u' | '.' | 'v' | 'V' | '~' | 'R' | ':' | '/' | '?' | '&' | DEL | PAGE_UP
        | PAGE_DOWN => done(count, Act::Cmd(k.to_string())),
        '\x01' | '\x02' | '\x04' | '\x05' | '\x06' | '\x07' | '\x12' | '\x15' | '\x18' | '\x19' => {
            done(count, Act::Cmd(k.to_string()))
        }
        _ => Parse::Bad,
    }
}

/// A count at `c[i..]` (not starting with 0, which is a motion) and the index after it.
fn count_at(c: &[char], i: usize) -> (Option<usize>, usize) {
    let mut j = i;
    while j < c.len() && c[j].is_ascii_digit() && !(j == i && c[j] == '0') {
        j += 1;
    }
    let n = (j > i).then(|| {
        c[i..j]
            .iter()
            .collect::<String>()
            .parse::<usize>()
            .unwrap_or(usize::MAX)
            .min(1_000_000)
    });
    (n, j)
}

/// A motion at the start of `c`: `Err` when more keys are needed, `None` when it is not one.
fn motion(c: &[char]) -> Result<Option<Motion>, ()> {
    use Motion::*;
    let Some(&k) = c.first() else {
        return Err(());
    };
    let next = c.get(1).copied();
    Ok(Some(match k {
        'h' | LEFT | BS => Left,
        'l' | RIGHT | ' ' => Right,
        'j' | DOWN | '\x0e' => Down,
        'k' | UP | '\x10' => Up,
        'w' => WordFwd(false),
        'W' => WordFwd(true),
        'b' => WordBack(false),
        'B' => WordBack(true),
        'e' => WordEnd(false),
        'E' => WordEnd(true),
        '0' | HOME => LineStart,
        '^' => FirstNonBlank,
        '$' | END => LineEnd,
        'G' => FileEnd,
        'H' => ScreenTop,
        'M' => ScreenMid,
        'L' => ScreenBottom,
        '}' => ParaFwd,
        '{' => ParaBack,
        '%' => MatchPair,
        'n' => SearchNext,
        'N' => SearchPrev,
        '*' => StarFwd,
        '#' => StarBack,
        ';' => FindRepeat,
        ',' => FindRepeatRev,
        '+' | ENTER => NextLine,
        '-' => PrevLine,
        '_' => CurLine,
        'f' | 'F' | 't' | 'T' => Find(k, next.ok_or(())?),
        '\'' | '`' => Mark(next.ok_or(())?, k == '`'),
        'g' => match next.ok_or(())? {
            'g' => FileStart,
            'e' => WordEndBack(false),
            'E' => WordEndBack(true),
            '_' => LastNonBlank,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub fn vim(text: &str) -> Vim {
        Vim::new(
            text.split('\n').map(String::from).collect(),
            PathBuf::from("test.txt"),
            false,
            true,
        )
    }

    /// Type `keys`; `<Esc>`, `<CR>`, `<BS>`, `<Tab>`, arrows and `<C-x>` name special keys.
    pub fn type_keys(v: &mut Vim, keys: &str) -> Outcome {
        let mut out = Outcome::default();
        let mut rest = keys;
        while let Some(c) = rest.chars().next() {
            let mut k = c;
            let mut used = c.len_utf8();
            if let Some(end) = rest.find('>').filter(|_| c == '<') {
                let special = match &rest[1..end] {
                    "Esc" => Some(ESC),
                    "CR" => Some(ENTER),
                    "BS" => Some(BS),
                    "Tab" => Some(TAB),
                    "Up" => Some(UP),
                    "Down" => Some(DOWN),
                    "Left" => Some(LEFT),
                    "Right" => Some(RIGHT),
                    n if n.len() == 3 && n.starts_with("C-") => n.chars().nth(2).map(ctrl),
                    _ => None,
                };
                if let Some(s) = special {
                    k = s;
                    used = end + 1;
                }
            }
            let o = v.key(k);
            out.wrote |= o.wrote;
            out.quit |= o.quit;
            rest = &rest[used..];
        }
        out
    }

    pub fn text(v: &Vim) -> String {
        v.buf.lines().join("\n")
    }

    pub fn check(start: &str, keys: &str, want: &str) {
        let mut v = vim(start);
        type_keys(&mut v, keys);
        assert_eq!(text(&v), want, "keys {keys:?} on {start:?}");
    }

    #[test]
    fn motions() {
        let mut v = vim("foo bar.baz\n\n  qux");
        type_keys(&mut v, "w");
        assert_eq!(v.cur, Pos::new(0, 4));
        type_keys(&mut v, "w");
        assert_eq!(v.cur, Pos::new(0, 7));
        type_keys(&mut v, "w");
        assert_eq!(v.cur, Pos::new(0, 8));
        type_keys(&mut v, "w");
        assert_eq!(v.cur, Pos::new(1, 0));
        type_keys(&mut v, "w");
        assert_eq!(v.cur, Pos::new(2, 2));
        type_keys(&mut v, "b");
        assert_eq!(v.cur, Pos::new(1, 0));
        type_keys(&mut v, "gg$");
        assert_eq!(v.cur, Pos::new(0, 10));
        type_keys(&mut v, "0e");
        assert_eq!(v.cur, Pos::new(0, 2));
        type_keys(&mut v, "W");
        assert_eq!(v.cur, Pos::new(0, 4));
        type_keys(&mut v, "G");
        assert_eq!(v.cur, Pos::new(2, 2));
        type_keys(&mut v, "2G");
        assert_eq!(v.cur, Pos::new(1, 0));
        type_keys(&mut v, "ggfb");
        assert_eq!(v.cur, Pos::new(0, 4));
        type_keys(&mut v, ";");
        assert_eq!(v.cur, Pos::new(0, 8));
        type_keys(&mut v, ",");
        assert_eq!(v.cur, Pos::new(0, 4));
        type_keys(&mut v, "tz");
        assert_eq!(v.cur, Pos::new(0, 9));
    }

    #[test]
    fn vertical_motion_keeps_column() {
        let mut v = vim("abcdef\nab\nabcdef");
        type_keys(&mut v, "4ljj");
        assert_eq!(v.cur, Pos::new(2, 4));
        type_keys(&mut v, "k");
        assert_eq!(v.cur, Pos::new(1, 1));
        type_keys(&mut v, "$j");
        assert_eq!(v.cur, Pos::new(2, 5));
    }

    #[test]
    fn operators() {
        check("one two three", "dw", "two three");
        check("one two three", "wcwX<Esc>", "one X three");
        check("one two three", "d2w", "three");
        check("one two three", "2dw", "three");
        check("one two three", "wD", "one ");
        check("one two three", "wC!<Esc>", "one !");
        check("one\ntwo", "dw", "\ntwo");
        check("a\nb\nc\nd", "jdd", "a\nc\nd");
        check("a\nb\nc\nd", "2dd", "c\nd");
        check("a\nb\nc\nd", "dj", "c\nd");
        check("a\nb\nc\nd", "Gdk", "a\nb");
        check("a\nb\nc", "dG", "");
        check("a\nb\nc", "jyyP", "a\nb\nb\nc");
        check("a\nb\nc", "yjGp", "a\nb\nc\na\nb");
        check("one two", "yiwP", "oneone two");
        check("foo(a, b)", "f(ci(x<Esc>", "foo(x)");
        check("foo(a, (b))", "fbdi(", "foo(a, ())");
        check("foo(a, b)", "fada(", "foo");
        check(r#"x = "hi there";"#, r#"ci"yo<Esc>"#, r#"x = "yo";"#);
        check(r#"x = "hi there";"#, r#"da""#, "x = ;");
        check(
            "fn f() {\n    a;\n    b;\n}",
            "jci{x<Esc>",
            "fn f() {\n    x\n}",
        );
        check("one two three", "wdaw", "one three");
        check("one two", "wdiw", "one ");
        check("abc", "x", "bc");
        check("abc", "$X", "ac");
        check("abc", "2x", "c");
        check("abc", "rx", "xbc");
        check("abc", "~~", "ABc");
        check("abc def", "gUiw", "ABC def");
        check("ABC", "guu", "abc");
        check("a\nb", "J", "a b");
        check("a\n    b", "J", "a b");
        check("a\nb", "gJ", "ab");
        check("x", ">>", "    x");
        check("        x", "<<", "    x");
        check("a 7 b", "<C-a>", "a 8 b");
        check("a 7 b", "5<C-x>", "a 2 b");
        check("a -1", "3<C-a>", "a 2");
        check("one two", "wdb", "two");
        check("one two", "de", " two");
        check("a b c", "d$", "");
        check("foo bar", "wd0", "bar");
        check("hello", "Rjel<Esc>", "jello");
    }

    #[test]
    fn insert_mode() {
        check("ab", "ix<Esc>", "xab");
        check("ab", "ax<Esc>", "axb");
        check("ab", "Ax<Esc>", "abx");
        check("  ab", "Ix<Esc>", "  xab");
        check("ab", "ox<Esc>", "ab\nx");
        check("ab", "Ox<Esc>", "x\nab");
        check("  ab", "ox<Esc>", "  ab\n  x");
        check("ab", "A<CR>c<Esc>", "ab\nc");
        check("ab", "A<BS><BS>x<Esc>", "x");
        check("a\nb", "jI<BS><Esc>", "ab");
        check("foo bar", "A<C-w>x<Esc>", "foo x");
        check("ab", "s<Esc>", "b");
        check("ab", "Sz<Esc>", "z");
        check("ab", "i<Tab><Esc>", "    ab");
    }

    #[test]
    fn undo_redo_and_repeat() {
        let mut v = vim("a b c");
        type_keys(&mut v, "dwdw");
        assert_eq!(text(&v), "c");
        type_keys(&mut v, "u");
        assert_eq!(text(&v), "b c");
        type_keys(&mut v, "u");
        assert_eq!(text(&v), "a b c");
        assert!(!v.buf.modified());
        type_keys(&mut v, "<C-r>");
        assert_eq!(text(&v), "b c");

        check("a b c d", "dw..", "d");
        check("x\nx\nx", "A;<Esc>j.j.", "x;\nx;\nx;");
        check("foo foo foo", "ciwbar<Esc>ww.", "bar foo bar");
        check("a\nb\nc", "ddu", "a\nb\nc");
        // An insert with typing undoes as one change.
        check("", "ione<CR>two<Esc>u", "");
        // `.` undoes as one change too.
        check("a\nb", "A!<Esc>j.u", "a!\nb");
    }

    #[test]
    fn visual_mode() {
        check("one two three", "vex", " two three");
        check("one two three", "wvey$p", "one two threetwo");
        check("a\nb\nc", "Vjd", "c");
        check("a\nb\nc", "Vj>", "    a\n    b\nc");
        check("abc", "vlU", "ABc");
        check("a\nb\nc", "VjJ", "a b\nc");
        check("abc", "vlrx", "xxc");
        check("ab\ncd", "vjc!<Esc>", "!d");
        check("one two", "yiwwviwp", "one one");
        check("a\nb", "yyjVp", "a\na");
    }

    #[test]
    fn registers() {
        check("a\nb", "\"ayyj\"ap", "a\nb\na");
        check("a\nb", "\"ayyjdd\"aP", "a\na");
        check("a\nb", "yyjdd\"0P", "a\na");
        check("a b", "\"_dwP", "b");
    }

    #[test]
    fn search() {
        let mut v = vim("foo\nbar foo\nbaz Foo");
        type_keys(&mut v, "/foo<CR>");
        assert_eq!(v.cur, Pos::new(1, 4));
        type_keys(&mut v, "n");
        assert_eq!(v.cur, Pos::new(2, 4), "ignorecase for a lower-case pattern");
        type_keys(&mut v, "n");
        assert_eq!(v.cur, Pos::new(0, 0));
        assert_eq!(
            v.msg.as_ref().unwrap().0,
            "search hit BOTTOM, continuing at TOP"
        );
        type_keys(&mut v, "N");
        assert_eq!(v.cur, Pos::new(2, 4));
        type_keys(&mut v, "?bar<CR>");
        assert_eq!(v.cur, Pos::new(1, 0));
        type_keys(&mut v, "/nope<CR>");
        assert!(v.msg.as_ref().unwrap().1);
        assert_eq!(v.cur, Pos::new(1, 0));
        type_keys(&mut v, "gg*");
        assert_eq!(v.cur, Pos::new(1, 4));
        type_keys(&mut v, "/Foo<CR>");
        assert_eq!(v.cur, Pos::new(2, 4), "smartcase for an upper-case pattern");
    }

    #[test]
    fn match_pairs_and_paragraphs() {
        let mut v = vim("f(a, [b])\n\nx\ny\n\nz");
        type_keys(&mut v, "%");
        assert_eq!(v.cur, Pos::new(0, 8));
        type_keys(&mut v, "%");
        assert_eq!(v.cur, Pos::new(0, 1));
        type_keys(&mut v, "}");
        assert_eq!(v.cur, Pos::new(1, 0));
        type_keys(&mut v, "}");
        assert_eq!(v.cur, Pos::new(4, 0));
        type_keys(&mut v, "{");
        assert_eq!(v.cur, Pos::new(1, 0));
        check("a\nb\n\nc", "dap", "c");
    }

    #[test]
    fn quitting_needs_a_write_or_bang() {
        let mut v = vim("a");
        assert!(type_keys(&mut v, ":q<CR>").quit);
        let mut v = vim("a");
        type_keys(&mut v, "x");
        assert!(!type_keys(&mut v, ":q<CR>").quit);
        assert!(v.msg.as_ref().unwrap().0.starts_with("E37"));
        assert!(type_keys(&mut v, ":q!<CR>").quit);
    }

    #[test]
    fn writes_preserving_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "one\r\ntwo\r\n").unwrap();
        let mut v = Vim::open(path.clone(), Some(2)).unwrap();
        assert_eq!(v.cur.row, 1);
        assert_eq!(v.buf.lines(), ["one", "two"]);
        type_keys(&mut v, "A!<Esc>");
        assert!(v.buf.modified());
        let out = type_keys(&mut v, ":w<CR>");
        assert!(out.wrote && !out.quit);
        assert!(!v.buf.modified());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\r\ntwo!\r\n");
        type_keys(&mut v, ":set ff=unix<CR>");
        assert!(v.buf.modified());
        let out = type_keys(&mut v, ":x<CR>");
        assert!(out.wrote && out.quit);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo!\n");

        let new = dir.path().join("new.txt");
        let mut v = Vim::open(new.clone(), None).unwrap();
        type_keys(&mut v, "ihi<Esc>:wq<CR>");
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "hi\n");

        std::fs::write(dir.path().join("bin"), b"a\0b").unwrap();
        assert!(Vim::open(dir.path().join("bin"), None).is_err());
    }
}
