//! `:` commands of the built-in editor: ranges (`%`, `.`, `$`, `N`, `'<,'>`, `'a`, `/pat/`,
//! with `+N` / `-N`), `:w` `:q` `:x` `:e!`, `:s`, `:g` / `:v`, `:d` `:y` `:m` `:t` `:j` `:>` `:<`,
//! `:sort`, `:normal`, `:set`...

use regex::Captures;

use super::buffer::Pos;
use super::pattern;
use super::vim::{display_col, load, Op, Range, Vim};

/// Command names and the shortest abbreviation Vim accepts for each; the first match wins.
const NAMES: &[(&str, usize)] = &[
    ("substitute", 1),
    ("set", 2),
    ("sort", 3),
    ("write", 1),
    ("wq", 2),
    ("wall", 2),
    ("wqall", 3),
    ("xit", 1),
    ("xall", 2),
    ("exit", 3),
    ("edit", 1),
    ("quit", 1),
    ("qall", 2),
    ("quitall", 5),
    ("update", 2),
    ("global", 1),
    ("vglobal", 1),
    ("delete", 1),
    ("yank", 1),
    ("join", 1),
    ("move", 1),
    ("copy", 2),
    ("t", 1),
    ("put", 2),
    ("normal", 4),
    ("nohlsearch", 3),
    ("undo", 1),
    ("redo", 3),
    ("read", 1),
    ("retab", 3),
    ("help", 1),
];

fn full_name(name: &str) -> Option<&'static str> {
    NAMES
        .iter()
        .find(|(full, min)| name.len() >= *min && full.starts_with(name))
        .map(|(full, _)| *full)
}

const HELP: &str = ":w [file] :q[!] :wq :x :e! | :%s/pat/rep/gi :g/pat/cmd :v/pat/d | \
:d :y :m N :t N :j :> :< :sort[!] [iun] :norm keys :r file | :noh :set nu rnu ic et ts=4 ff=unix";

/// A cursor over a command line.
struct P {
    c: Vec<char>,
    i: usize,
}

impl P {
    fn new(s: &str) -> Self {
        Self {
            c: s.chars().collect(),
            i: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn eat(&mut self, ch: char) -> bool {
        let yes = self.peek() == Some(ch);
        self.i += usize::from(yes);
        yes
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.i += 1;
        }
    }

    fn number(&mut self) -> Option<i64> {
        let s = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let digits: String = self.c[s..self.i].iter().collect();
        (self.i > s).then(|| digits.parse().unwrap_or(i64::MAX / 4))
    }

    fn rest(&self) -> String {
        self.c[self.i..].iter().collect()
    }

    /// Text up to an unescaped `delim` (consumed); `\delim` becomes `delim`.
    fn until(&mut self, delim: char) -> String {
        let mut out = String::new();
        while let Some(c) = self.bump() {
            if c == delim {
                break;
            }
            if c == '\\' && self.peek() == Some(delim) {
                out.push(delim);
                self.i += 1;
            } else if c == '\\' {
                out.push(c);
                if let Some(n) = self.bump() {
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}

/// A delimiter for `:s` and `:g`: any ASCII punctuation but `\`, `"` and `|`.
fn is_delim(c: char) -> bool {
    c.is_ascii_punctuation() && !matches!(c, '\\' | '"' | '|')
}

type Rows = (usize, usize);

impl Vim {
    /// Run a command line, showing any error.
    pub(super) fn ex(&mut self, line: &str) {
        if let Err(e) = self.ex_run(line) {
            self.err(e);
        }
    }

    pub(super) fn ex_run(&mut self, line: &str) -> Result<(), String> {
        let line = line.trim_start_matches([':', ' ', '\t']);
        let mut p = P::new(line);
        if p.peek().is_none() {
            return Ok(());
        }
        let range = self.range(&mut p)?;
        p.ws();
        let name: String = match p.peek() {
            Some(c) if c.is_ascii_alphabetic() => {
                let s = p.i;
                while p.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    p.i += 1;
                }
                p.c[s..p.i].iter().collect()
            }
            Some(c @ ('&' | '<' | '>')) => {
                let s = p.i;
                while p.peek() == Some(c) {
                    p.i += 1;
                }
                p.c[s..p.i].iter().collect()
            }
            _ => String::new(),
        };
        let bang = p.eat('!');
        let raw = p.rest();
        let arg = raw.trim();
        let cur = self.cur.row;
        let here = range.unwrap_or((cur, cur));
        let all = range.unwrap_or((0, self.buf.last_row()));
        let not_cmd = || format!("E492: Not an editor command: {line}");

        if name.is_empty() {
            if bang || !arg.is_empty() {
                return Err(not_cmd());
            }
            if let Some((_, b)) = range {
                self.jump(Pos::new(b, self.first_non_blank(b)));
            }
            return Ok(());
        }
        let full = match name.chars().next() {
            Some('&' | '<' | '>') => name.as_str(),
            _ => full_name(&name).ok_or_else(not_cmd)?,
        };
        let modified_err = || "E37: No write since last change (add ! to override)".to_string();
        match full {
            "write" | "wall" => self.write_to(Some(arg)),
            "update" => {
                if self.buf.modified() {
                    self.write_to(Some(arg))?;
                }
                Ok(())
            }
            "wq" | "wqall" | "xit" | "xall" | "exit" => {
                if matches!(full, "wq" | "wqall") || self.buf.modified() || !arg.is_empty() {
                    self.write_to(Some(arg))?;
                }
                self.out.quit = true;
                Ok(())
            }
            "quit" | "qall" | "quitall" => {
                if self.buf.modified() && !bang {
                    return Err(modified_err());
                }
                self.out.quit = true;
                Ok(())
            }
            "edit" => {
                if !arg.is_empty() {
                    return Err(
                        "Only the current file can be edited here; use :e! to reload it".into(),
                    );
                }
                if self.buf.modified() && !bang {
                    return Err(modified_err());
                }
                self.reload()
            }
            "substitute" => self.substitute(here, &raw),
            "&" => self.substitute(here, arg),
            "&&" => {
                let flags = self
                    .last_sub
                    .as_ref()
                    .map(|s| s.2.clone())
                    .unwrap_or_default();
                self.substitute(here, &format!("{flags}{arg}"))
            }
            "global" | "vglobal" => self.global(all, &raw, full == "vglobal" || bang),
            "delete" | "yank" => {
                let (reg, count) = reg_and_count(arg);
                let (a, b) = self.with_count(here, count);
                let op = if full == "delete" {
                    Op::Delete
                } else {
                    Op::Yank
                };
                self.apply_op(op, Range::lines(a, b), reg);
                Ok(())
            }
            "join" => {
                let (_, count) = reg_and_count(arg);
                let (a, b) = match (range, count) {
                    (_, Some(n)) => (here.1, here.1 + n.max(2) - 1),
                    (Some((a, b)), None) if a != b => (a, b),
                    _ => (here.0, here.0 + 1),
                };
                self.join(a, b - a + 1, !bang);
                Ok(())
            }
            s if s.starts_with(['>', '<']) => {
                let (_, count) = reg_and_count(arg);
                let (a, b) = self.with_count(here, count);
                for row in a..=b {
                    for _ in 0..name.len() {
                        self.shift(row, full.starts_with('>'));
                    }
                }
                self.cur = Pos::new(b, self.first_non_blank(b));
                Ok(())
            }
            "move" => self.move_lines(here, arg),
            "copy" | "t" => self.copy_lines(here, arg),
            "put" => {
                let reg = arg.chars().next();
                let Some(r) = self.get_reg(reg) else {
                    return Err("E353: Nothing in register".into());
                };
                let lines: Vec<String> = r.text.split('\n').map(String::from).collect();
                let at = if bang { here.1 } else { here.1 + 1 };
                let n = lines.len();
                self.buf.insert_lines(at, lines);
                let row = (at + n - 1).min(self.buf.last_row());
                self.cur = Pos::new(row, self.first_non_blank(row));
                Ok(())
            }
            "sort" => self.sort(all, bang, arg),
            "nohlsearch" => {
                self.hl = false;
                Ok(())
            }
            "set" => self.set(arg),
            "undo" => {
                match self.buf.undo() {
                    Some(p) => self.cur = p,
                    None => self.info("Already at oldest change"),
                }
                Ok(())
            }
            "redo" => {
                match self.buf.redo() {
                    Some(p) => self.cur = p,
                    None => self.info("Already at newest change"),
                }
                Ok(())
            }
            "normal" => self.normal_cmd(here, raw.trim_start()),
            "read" => {
                if arg.is_empty() {
                    return Err("E32: No file name".into());
                }
                let path = self.resolve(arg);
                let (lines, _, _) = load(&path)
                    .map_err(|e| format!("{e:#}"))?
                    .ok_or_else(|| format!("E484: Can't open file {arg}"))?;
                let at = here.1 + 1;
                self.buf.insert_lines(at, lines);
                self.cur = Pos::new(at.min(self.buf.last_row()), 0);
                Ok(())
            }
            "retab" => {
                let ts = self.opts.tabstop.max(1);
                for row in all.0..=all.1 {
                    let line = self.buf.line(row).to_string();
                    let body = line.trim_start();
                    let indent = &line[..line.len() - body.len()];
                    let width = display_col(indent, indent.chars().count(), ts);
                    let new = self.make_indent(width) + body;
                    self.buf.set_line(row, new);
                }
                Ok(())
            }
            "help" => {
                self.info(HELP);
                Ok(())
            }
            _ => Err(not_cmd()),
        }
    }

    /// Rows `a..=b`, or `count` lines from `b` when a count is given (`:d 3`).
    fn with_count(&self, (a, b): Rows, count: Option<usize>) -> Rows {
        match count {
            Some(n) => (b, (b + n.max(1) - 1).min(self.buf.last_row())),
            None => (a, b),
        }
    }

    fn clamp_row(&self, r: i64) -> usize {
        r.clamp(0, self.buf.last_row() as i64) as usize
    }

    fn range(&self, p: &mut P) -> Result<Option<Rows>, String> {
        p.ws();
        if p.eat('%') {
            return Ok(Some((0, self.buf.last_row())));
        }
        let a = self.address(p)?;
        p.ws();
        if p.eat(',') || p.eat(';') {
            p.ws();
            let cur = self.cur.row as i64;
            let b = self.address(p)?.unwrap_or(cur);
            let (a, b) = (self.clamp_row(a.unwrap_or(cur)), self.clamp_row(b));
            return Ok(Some((a.min(b), a.max(b))));
        }
        Ok(a.map(|a| {
            let r = self.clamp_row(a);
            (r, r)
        }))
    }

    /// One address as a 0-based row (`0`, above the first line, is -1).
    fn address(&self, p: &mut P) -> Result<Option<i64>, String> {
        let cur = self.cur.row as i64;
        let last = self.buf.last_row() as i64;
        let base = match p.peek() {
            Some(c) if c.is_ascii_digit() => p.number().map(|n| n - 1),
            Some('.') => {
                p.i += 1;
                Some(cur)
            }
            Some('$') => {
                p.i += 1;
                Some(last)
            }
            Some('\'') => {
                p.i += 1;
                let m = p.bump().ok_or("E20: Mark not set")?;
                Some(self.mark_row(m)? as i64)
            }
            Some(d @ ('/' | '?')) => {
                p.i += 1;
                let pat = p.until(d);
                let pat = if pat.is_empty() {
                    self.last_search
                        .clone()
                        .ok_or("E35: No previous regular expression")?
                } else {
                    pat
                };
                let re = self.regex(&pat)?;
                let rows = self.buf.line_count();
                let found = (1..=rows)
                    .map(|i| {
                        if d == '/' {
                            (self.cur.row + i) % rows
                        } else {
                            (self.cur.row + rows - i) % rows
                        }
                    })
                    .find(|&r| re.is_match(self.buf.line(r)))
                    .ok_or_else(|| format!("E486: Pattern not found: {pat}"))?;
                Some(found as i64)
            }
            _ => None,
        };
        let mut off = 0i64;
        let mut any = false;
        loop {
            if p.eat('+') {
                off += p.number().unwrap_or(1);
            } else if p.eat('-') {
                off -= p.number().unwrap_or(1);
            } else {
                break;
            }
            any = true;
        }
        Ok(match (base, any) {
            (None, false) => None,
            (b, _) => Some(b.unwrap_or(cur) + off),
        })
    }

    fn mark_row(&self, m: char) -> Result<usize, String> {
        let row = match m {
            '<' | '>' => {
                let (a, b, _) = self.last_visual.ok_or("E20: Mark not set")?;
                if m == '<' {
                    a.row.min(b.row)
                } else {
                    a.row.max(b.row)
                }
            }
            m => self.marks.get(&m).ok_or("E20: Mark not set")?.row,
        };
        Ok(row.min(self.buf.last_row()))
    }

    /// `:s/pat/rep/flags [count]`; without `/pat/`, repeats the last substitution.
    fn substitute(&mut self, rows: Rows, raw: &str) -> Result<(), String> {
        let mut p = P::new(raw.trim_start());
        let (pat, rep) = match p.peek() {
            Some(d) if is_delim(d) => {
                p.i += 1;
                let pat = p.until(d);
                let rep = p.until(d);
                (pat, Some(rep))
            }
            _ => (String::new(), None),
        };
        let flags = p.rest();
        let pat = if pat.is_empty() {
            self.last_sub
                .as_ref()
                .map(|s| s.0.clone())
                .filter(|_| rep.is_none())
                .or_else(|| self.last_search.clone())
                .ok_or("E35: No previous regular expression")?
        } else {
            pat
        };
        let rep = match rep {
            Some(r) => r,
            None => self
                .last_sub
                .as_ref()
                .map(|s| s.1.clone())
                .ok_or("E35: No previous substitute regular expression")?,
        };
        let (mut global, mut count_only, mut no_err) = (false, false, false);
        let mut case = None;
        let mut fp = P::new(flags.trim());
        let mut kept = String::new();
        while let Some(c) = fp.peek() {
            match c {
                'g' => global = !global,
                'i' => case = Some(true),
                'I' => case = Some(false),
                'n' => count_only = true,
                'e' => no_err = true,
                '&' | ' ' => {}
                'c' => {
                    return Err("Confirming each substitution (the c flag) is not supported".into())
                }
                c if c.is_ascii_digit() => break,
                _ => return Err(format!("E488: Trailing characters: {flags}")),
            }
            if c != ' ' && c != '&' {
                kept.push(c);
            }
            fp.i += 1;
        }
        let rows = match fp.number() {
            Some(n) => self.with_count(rows, Some(n.max(1) as usize)),
            None => rows,
        };
        let src = match case {
            Some(true) => format!(r"{pat}\c"),
            Some(false) => format!(r"{pat}\C"),
            None => pat.clone(),
        };
        let re = self.regex(&src)?;
        let (mut total, mut lines, mut last_line) = (0, 0, None::<usize>);
        for row in (rows.0..=rows.1).rev() {
            let line = self.buf.line(row).to_string();
            let n = if global {
                re.find_iter(&line).count()
            } else {
                usize::from(re.is_match(&line))
            };
            if n == 0 {
                continue;
            }
            total += n;
            lines += 1;
            if count_only {
                continue;
            }
            let expand = |c: &Captures| pattern::expand(&rep, c);
            let new = if global {
                re.replace_all(&line, expand)
            } else {
                re.replacen(&line, 1, expand)
            };
            let new: Vec<String> = new.split('\n').map(String::from).collect();
            let added = new.len() - 1;
            self.buf.replace_lines(row, row, new);
            // Rows are rewritten bottom-up: the first one rewritten is the last line changed.
            last_line = Some(last_line.unwrap_or(row) + added);
        }
        self.last_search = Some(pat.clone());
        self.hl = true;
        self.last_sub = Some((pat.clone(), rep, kept));
        if total == 0 {
            return if no_err {
                Ok(())
            } else {
                Err(format!("E486: Pattern not found: {pat}"))
            };
        }
        if let Some(l) = last_line {
            self.cur = Pos::new(l, self.first_non_blank(l));
        }
        let n =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let on = n(lines, "line", "lines");
        if count_only {
            self.info(format!("{} on {on}", n(total, "match", "matches")));
        } else if total > 1 {
            let subs = n(total, "substitution", "substitutions");
            self.info(format!("{subs} on {on}"));
        }
        Ok(())
    }

    /// `:g/pat/cmd` runs `cmd` on each matching line (`:v` and `:g!` on the others).
    fn global(&mut self, rows: Rows, raw: &str, invert: bool) -> Result<(), String> {
        let mut p = P::new(raw.trim_start());
        let d = p
            .bump()
            .filter(|&d| is_delim(d))
            .ok_or("E: :g needs a pattern, like :g/pattern/d")?;
        let pat = p.until(d);
        let pat = if pat.is_empty() {
            self.last_search
                .clone()
                .ok_or("E35: No previous regular expression")?
        } else {
            pat
        };
        let cmd = p.rest();
        let re = self.regex(&pat)?;
        let marked: Vec<usize> = (rows.0..=rows.1)
            .filter(|&r| re.is_match(self.buf.line(r)) != invert)
            .collect();
        self.last_search = Some(pat.clone());
        self.hl = true;
        if marked.is_empty() {
            return Err(format!("E486: Pattern not found: {pat}"));
        }
        if cmd.trim().is_empty() {
            self.info(format!("{} matching lines", marked.len()));
            return Ok(());
        }
        let was = std::mem::replace(&mut self.batch, true);
        // Earlier commands may add or remove lines; shift the remaining rows to follow.
        let mut delta = 0i64;
        let mut result = Ok(());
        for r in marked {
            let row = r as i64 + delta;
            if row < 0 || row > self.buf.last_row() as i64 {
                continue;
            }
            let before = self.buf.line_count() as i64;
            self.cur = Pos::new(row as usize, 0);
            result = self.ex_run(&cmd);
            if result.is_err() {
                break;
            }
            delta += self.buf.line_count() as i64 - before;
        }
        self.batch = was;
        self.clamp_cursor();
        result
    }

    /// `:normal keys`: type `keys` in normal mode on each line of the range.
    fn normal_cmd(&mut self, (a, b): Rows, keys: &str) -> Result<(), String> {
        if keys.is_empty() {
            return Err("E471: Argument required".into());
        }
        let was = std::mem::replace(&mut self.batch, true);
        let mut delta = 0i64;
        for r in a..=b {
            let row = r as i64 + delta;
            if row < 0 || row > self.buf.last_row() as i64 {
                break;
            }
            let before = self.buf.line_count() as i64;
            self.cur = Pos::new(row as usize, 0);
            for k in keys.chars() {
                self.feed(k);
            }
            self.finish_keys();
            delta += self.buf.line_count() as i64 - before;
        }
        self.batch = was;
        Ok(())
    }

    /// Destination of `:m` / `:t`: the row to insert below (-1 for above the first line).
    fn dest(&self, arg: &str) -> Result<i64, String> {
        let mut p = P::new(arg);
        let d = self.address(&mut p)?.ok_or("E14: Invalid address")?;
        Ok(d.clamp(-1, self.buf.last_row() as i64))
    }

    fn move_lines(&mut self, (a, b): Rows, arg: &str) -> Result<(), String> {
        let dest = self.dest(arg)?;
        if dest >= a as i64 && dest < b as i64 {
            return Err("E134: Cannot move a range of lines into itself".into());
        }
        if dest == b as i64 || dest == a as i64 - 1 {
            return Ok(());
        }
        let lines = self.buf.delete_lines(a, b);
        let n = lines.len();
        let at = if dest > b as i64 {
            (dest + 1) as usize - n
        } else {
            (dest + 1) as usize
        };
        self.buf.insert_lines(at, lines);
        let row = at + n - 1;
        self.cur = Pos::new(row, self.first_non_blank(row));
        Ok(())
    }

    fn copy_lines(&mut self, (a, b): Rows, arg: &str) -> Result<(), String> {
        let at = (self.dest(arg)? + 1) as usize;
        let lines = self.buf.lines()[a..=b].to_vec();
        let n = lines.len();
        self.buf.insert_lines(at, lines);
        let row = at + n - 1;
        self.cur = Pos::new(row, self.first_non_blank(row));
        Ok(())
    }

    /// `:sort[!] [i] [u] [n]`: `!` reverses, `i` ignores case, `u` drops duplicates, `n` sorts
    /// by the first number in each line.
    fn sort(&mut self, (a, b): Rows, reverse: bool, arg: &str) -> Result<(), String> {
        let (mut icase, mut uniq, mut numeric) = (false, false, false);
        for c in arg.chars() {
            match c {
                'i' => icase = true,
                'u' => uniq = true,
                'n' => numeric = true,
                ' ' => {}
                _ => return Err(format!("E474: Invalid argument: {arg}")),
            }
        }
        let mut lines = self.buf.lines()[a..=b].to_vec();
        if numeric {
            let num = regex::Regex::new(r"-?\d+").expect("valid regex");
            lines.sort_by_key(|l| num.find(l).and_then(|m| m.as_str().parse::<i64>().ok()));
        } else if icase {
            lines.sort_by_key(|l| l.to_lowercase());
        } else {
            lines.sort();
        }
        if reverse {
            lines.reverse();
        }
        if uniq {
            lines.dedup_by(|x, y| {
                if icase {
                    x.to_lowercase() == y.to_lowercase()
                } else {
                    x == y
                }
            });
        }
        self.buf.replace_lines(a, b, lines);
        Ok(())
    }

    fn set(&mut self, arg: &str) -> Result<(), String> {
        if arg.is_empty() {
            let o = &self.opts;
            let flag = |on: bool, name: &str| format!("{}{name}", if on { "" } else { "no" });
            let msg = format!(
                "{} {} {} {} ts={} sw={} ff={}",
                flag(o.number, "number"),
                flag(o.ignorecase, "ignorecase"),
                flag(o.smartcase, "smartcase"),
                flag(o.expandtab, "expandtab"),
                o.tabstop,
                o.shiftwidth,
                if self.crlf { "dos" } else { "unix" },
            );
            self.info(msg);
            return Ok(());
        }
        for a in arg.split_whitespace() {
            let invalid = || format!("E518: Unknown option: {a}");
            if let Some((name, val)) = a.split_once('=') {
                match name {
                    "tabstop" | "ts" | "shiftwidth" | "sw" => {
                        let n: usize = val
                            .parse()
                            .ok()
                            .filter(|n| (1..=32).contains(n))
                            .ok_or_else(|| format!("E487: Argument must be 1 to 32: {a}"))?;
                        if name.starts_with('t') {
                            self.opts.tabstop = n;
                        } else {
                            self.opts.shiftwidth = n;
                        }
                    }
                    "fileformat" | "ff" => {
                        let crlf = match val {
                            "dos" => true,
                            "unix" => false,
                            _ => return Err(format!("E474: Invalid argument: {a}")),
                        };
                        if crlf != self.crlf {
                            self.crlf = crlf;
                            self.buf.set_modified();
                        }
                    }
                    _ => return Err(invalid()),
                }
                continue;
            }
            if let Some(name) = a.strip_suffix('?') {
                let shown = match name {
                    "tabstop" | "ts" => format!("tabstop={}", self.opts.tabstop),
                    "shiftwidth" | "sw" => format!("shiftwidth={}", self.opts.shiftwidth),
                    "fileformat" | "ff" => {
                        format!("fileformat={}", if self.crlf { "dos" } else { "unix" })
                    }
                    n => {
                        let on = *self.bool_opt(n).ok_or_else(invalid)?;
                        format!("{}{n}", if on { "" } else { "no" })
                    }
                };
                self.info(shown);
                continue;
            }
            let (name, value) = if let Some(n) = a.strip_suffix('!') {
                (n, None)
            } else if let Some(n) = a.strip_prefix("no").filter(|n| self.bool_opt(n).is_some()) {
                (n, Some(false))
            } else if let Some(n) = a.strip_prefix("inv") {
                (n, None)
            } else {
                (a, Some(true))
            };
            let opt = self.bool_opt(name).ok_or_else(invalid)?;
            *opt = value.unwrap_or(!*opt);
        }
        Ok(())
    }

    fn bool_opt(&mut self, name: &str) -> Option<&mut bool> {
        let o = &mut self.opts;
        Some(match name {
            "number" | "nu" => &mut o.number,
            "relativenumber" | "rnu" => &mut o.relativenumber,
            "ignorecase" | "ic" => &mut o.ignorecase,
            "smartcase" | "scs" => &mut o.smartcase,
            "hlsearch" | "hls" => &mut o.hlsearch,
            "incsearch" | "is" => &mut o.incsearch,
            "expandtab" | "et" => &mut o.expandtab,
            "autoindent" | "ai" => &mut o.autoindent,
            "wrapscan" | "ws" => &mut o.wrapscan,
            _ => return None,
        })
    }
}

/// An optional register letter and count after `:d` / `:y` / `:j` / `:>`.
fn reg_and_count(arg: &str) -> (Option<char>, Option<usize>) {
    let arg = arg.trim();
    let mut chars = arg.chars();
    let reg = chars
        .next()
        .filter(|c| c.is_ascii_alphabetic() || *c == '_');
    let rest = if reg.is_some() { chars.as_str() } else { arg };
    (reg, rest.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::super::vim::tests::{check, text, type_keys, vim};
    use super::*;

    #[test]
    fn substitute() {
        check("a a\na a", ":%s/a/b/g<CR>", "b b\nb b");
        check("a a\na a", ":%s/a/b/<CR>", "b a\nb a");
        check("a a\na a", ":s/a/b/g<CR>", "b b\na a");
        check("a\nb\nc\nd", ":2,3s/$/!/<CR>", "a\nb!\nc!\nd");
        check("foo bar", r":s/\(\w\+\) \(\w\+\)/\2 \1/<CR>", "bar foo");
        check("Foo foo", ":s/foo/x/gi<CR>", "x x");
        check("Foo foo", ":s/foo/x/gI<CR>", "Foo x");
        check("a,b,c", r":s/,/\r/g<CR>", "a\nb\nc");
        check("path", ":s#path#/usr/bin#<CR>", "/usr/bin");
        check("a/b", r":s/\//-/<CR>", "a-b");
        check("x", ":s/x/a&b/<CR>", "axb");
        check("one two", r":s/\w\+/\u&/g<CR>", "One Two");
        check("a\na", ":s/a/b/<CR>j:&&<CR>", "b\nb");
        check("ab\nab", ":s/a/x/<CR>j&", "xb\nxb");
        check("a b\nc", r":%s/ /\n/g<CR>", "a\nb\nc");
        // The last search is the default pattern.
        check("foo bar", "/bar<CR>:s//baz/<CR>", "foo baz");
    }

    #[test]
    fn substitute_reports_and_counts() {
        let mut v = vim("a a\na");
        type_keys(&mut v, ":%s/a/b/gn<CR>");
        assert_eq!(text(&v), "a a\na");
        assert_eq!(v.msg.as_ref().unwrap().0, "3 matches on 2 lines");
        type_keys(&mut v, ":%s/zzz/b/<CR>");
        assert!(v.msg.as_ref().unwrap().1);
        type_keys(&mut v, ":%s/zzz/b/e<CR>");
        assert_eq!(v.msg, None);
        // A whole :s undoes in one step.
        type_keys(&mut v, ":%s/a/b/g<CR>u");
        assert_eq!(text(&v), "a a\na");
    }

    #[test]
    fn visual_range() {
        check("a\na\na\na", "jVj:s/a/b/<CR>", "a\nb\nb\na");
        check("x\ny\nz", "Vj:d<CR>", "z");
    }

    #[test]
    fn global() {
        check("keep\ndrop 1\nkeep\ndrop 2", ":g/drop/d<CR>", "keep\nkeep");
        check(
            "keep\ndrop 1\nkeep\ndrop 2",
            ":v/drop/d<CR>",
            "drop 1\ndrop 2",
        );
        check("keep\ndrop 1\nkeep", ":g!/drop/d<CR>", "drop 1");
        check("a\n\n\nb", ":g/^$/d<CR>", "a\nb");
        check("1\n2\n3", ":g/^/m0<CR>", "3\n2\n1");
        check("a x\nb\nc x", ":g/x/s/x/y/<CR>", "a y\nb\nc y");
        check("a\nb", ":g/./normal A;<CR>", "a;\nb;");
        let mut v = vim("a\nb\na");
        type_keys(&mut v, ":g/a/d<CR>u");
        assert_eq!(text(&v), "a\nb\na", "one undo step");
    }

    #[test]
    fn line_commands() {
        check("a\nb\nc", ":2d<CR>", "a\nc");
        check("a\nb\nc", ":1,2d<CR>", "c");
        check("a\nb\nc", ":d 2<CR>", "c");
        check("a\nb\nc", ":$d<CR>", "a\nb");
        check("a\nb\nc", ":.,$d<CR>", "");
        check("a\nb\nc", ":.+1d<CR>", "a\nc");
        check("a\nb\nc", ":/c/d<CR>", "a\nb");
        check("a\nb\nc", ":1m$<CR>", "b\nc\na");
        check("a\nb\nc", ":3m0<CR>", "c\na\nb");
        check("a\nb\nc", ":1t.<CR>", "a\na\nb\nc");
        check("a\nb\nc", ":1,2co$<CR>", "a\nb\nc\na\nb");
        check("a\nb\nc", ":1,3j<CR>", "a b c");
        check("a\nb", ":j!<CR>", "ab");
        check("a\nb", ":%><CR>", "    a\n    b");
        check("a", ":>><CR>", "        a");
        check("c\na\nb\na", ":sort<CR>", "a\na\nb\nc");
        check("c\na\nb\na", ":sort u<CR>", "a\nb\nc");
        check("c\na\nb", ":sort!<CR>", "c\nb\na");
        check("x10\nx9\nx100", ":sort n<CR>", "x9\nx10\nx100");
        check("b\nA\nc", ":sort i<CR>", "A\nb\nc");
        check("a\nb\nc", ":2,3normal Ax<CR>", "a\nbx\ncx");
        check("a\nb", "yy:2put<CR>", "a\nb\na");
        check("a\nb", "ma:2<CR>:'ad<CR>", "b");
        check("\ta", ":set et<CR>:retab<CR>", "    a");
    }

    #[test]
    fn goto_line_and_settings() {
        let mut v = vim("a\nb\nc\nd");
        type_keys(&mut v, ":3<CR>");
        assert_eq!(v.cur.row, 2);
        type_keys(&mut v, ":$<CR>");
        assert_eq!(v.cur.row, 3);
        type_keys(&mut v, ":99<CR>");
        assert_eq!(v.cur.row, 3);
        type_keys(&mut v, ":set nonu rnu ts=8<CR>");
        assert!(!v.opts.number && v.opts.relativenumber);
        assert_eq!(v.opts.tabstop, 8);
        type_keys(&mut v, ":set nu!<CR>");
        assert!(v.opts.number);
        type_keys(&mut v, ":set ts?<CR>");
        assert_eq!(v.msg.as_ref().unwrap().0, "tabstop=8");
        type_keys(&mut v, ":set bogus<CR>");
        assert!(v.msg.as_ref().unwrap().1);
        type_keys(&mut v, ":frobnicate<CR>");
        assert!(v.msg.as_ref().unwrap().0.starts_with("E492"));
        type_keys(&mut v, ":noh<CR>");
        assert!(!v.hl);
    }

    #[test]
    fn write_to_another_file_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "ab\n").unwrap();
        let mut v = Vim::open(path.clone(), None).unwrap();
        type_keys(&mut v, "x:w copy.txt<CR>");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("copy.txt")).unwrap(),
            "b\n"
        );
        assert!(
            v.buf.modified(),
            "writing elsewhere keeps the buffer modified"
        );
        type_keys(&mut v, ":e<CR>");
        assert!(v.msg.as_ref().unwrap().0.starts_with("E37"));
        type_keys(&mut v, ":e!<CR>");
        assert_eq!(text(&v), "ab");
        assert!(!v.buf.modified());
        type_keys(&mut v, ":r copy.txt<CR>");
        assert_eq!(text(&v), "ab\nb");
    }
}
