//! Text of the built-in editor: a list of lines addressed by (row, char column), with
//! snapshot undo grouped by editing command.

/// A position in the buffer: `col` counts chars, not bytes. `col == line length` is the end of
/// the line (where its newline would be).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// Byte offset of char column `col` in `s` (the length when past the end).
pub fn byte_of(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(i, _)| i)
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
    rev: u64,
}

const UNDO_LEVELS: usize = 500;

pub struct Buffer {
    lines: Vec<String>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Cursor at the start of the open change group; all edits until `end` undo as one.
    group: Option<Pos>,
    /// The open group already saved its snapshot.
    snapped: bool,
    rev: u64,
    next_rev: u64,
    saved_rev: u64,
    /// First row changed since the last `take_changed` (for the syntax highlighter).
    changed_from: usize,
}

impl Buffer {
    pub fn new(mut lines: Vec<String>) -> Self {
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            undo: Vec::new(),
            redo: Vec::new(),
            group: None,
            snapped: false,
            rev: 0,
            next_rev: 0,
            saved_rev: 0,
            changed_from: 0,
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line(&self, row: usize) -> &str {
        self.lines.get(row).map_or("", String::as_str)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn last_row(&self) -> usize {
        self.lines.len() - 1
    }

    pub fn line_len(&self, row: usize) -> usize {
        char_len(self.line(row))
    }

    pub fn modified(&self) -> bool {
        self.rev != self.saved_rev
    }

    pub fn mark_saved(&mut self) {
        self.saved_rev = self.rev;
    }

    /// Flag a change made outside the text (line endings), so writing is needed.
    pub fn set_modified(&mut self) {
        self.next_rev += 1;
        self.rev = self.next_rev;
    }

    pub fn take_changed(&mut self) -> Option<usize> {
        let from = std::mem::replace(&mut self.changed_from, usize::MAX);
        (from != usize::MAX).then_some(from)
    }

    /// Start a change group (no-op when one is open): edits until [`end`](Self::end) undo together.
    pub fn begin(&mut self, cursor: Pos) {
        if self.group.is_none() {
            self.group = Some(cursor);
            self.snapped = false;
        }
    }

    pub fn end(&mut self) {
        self.group = None;
        self.snapped = false;
    }

    /// Called before every edit touching `row` and below.
    fn touch(&mut self, row: usize) {
        self.changed_from = self.changed_from.min(row);
        if self.snapped {
            return;
        }
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.group.unwrap_or_default(),
            rev: self.rev,
        });
        if self.undo.len() > UNDO_LEVELS {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.next_rev += 1;
        self.rev = self.next_rev;
        self.snapped = self.group.is_some();
    }

    /// Restore the state before the last change group; returns the cursor it started at.
    pub fn undo(&mut self) -> Option<Pos> {
        self.end();
        let s = self.undo.pop()?;
        let lines = std::mem::replace(&mut self.lines, s.lines);
        self.redo.push(Snapshot {
            lines,
            cursor: s.cursor,
            rev: self.rev,
        });
        self.rev = s.rev;
        self.changed_from = 0;
        Some(s.cursor)
    }

    pub fn redo(&mut self) -> Option<Pos> {
        self.end();
        let s = self.redo.pop()?;
        let lines = std::mem::replace(&mut self.lines, s.lines);
        self.undo.push(Snapshot {
            lines,
            cursor: s.cursor,
            rev: self.rev,
        });
        self.rev = s.rev;
        self.changed_from = 0;
        Some(s.cursor)
    }

    fn clamp(&self, p: Pos) -> Pos {
        if p.row > self.last_row() {
            let r = self.last_row();
            return Pos::new(r, self.line_len(r));
        }
        Pos::new(p.row, p.col.min(self.line_len(p.row)))
    }

    /// Text between `a` and `b` (exclusive); rows are joined by `\n`.
    pub fn text(&self, a: Pos, b: Pos) -> String {
        let (a, b) = (self.clamp(a), self.clamp(b));
        if a >= b {
            return String::new();
        }
        let first = self.line(a.row);
        if a.row == b.row {
            return first[byte_of(first, a.col)..byte_of(first, b.col)].to_string();
        }
        let mut out = first[byte_of(first, a.col)..].to_string();
        for r in a.row + 1..b.row {
            out.push('\n');
            out.push_str(self.line(r));
        }
        let last = self.line(b.row);
        out.push('\n');
        out.push_str(&last[..byte_of(last, b.col)]);
        out
    }

    /// Insert `text` (may contain newlines) at `at`; returns the position just after it.
    pub fn insert(&mut self, at: Pos, text: &str) -> Pos {
        let at = self.clamp(at);
        if text.is_empty() {
            return at;
        }
        self.touch(at.row);
        let line = &self.lines[at.row];
        let b = byte_of(line, at.col);
        let tail = line[b..].to_string();
        let mut head = line[..b].to_string();
        let mut parts = text.split('\n');
        let first = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();
        let Some((last, middle)) = rest.split_last() else {
            head.push_str(first);
            head.push_str(&tail);
            self.lines[at.row] = head;
            return Pos::new(at.row, at.col + char_len(first));
        };
        head.push_str(first);
        self.lines[at.row] = head;
        let mut new: Vec<String> = middle.iter().map(|s| s.to_string()).collect();
        new.push(format!("{last}{tail}"));
        let n = new.len();
        self.lines.splice(at.row + 1..at.row + 1, new);
        Pos::new(at.row + n, char_len(last))
    }

    /// Delete from `a` to `b` (exclusive), returning the removed text.
    pub fn delete(&mut self, a: Pos, b: Pos) -> String {
        let (a, b) = (self.clamp(a), self.clamp(b));
        let text = self.text(a, b);
        if text.is_empty() {
            return text;
        }
        self.touch(a.row);
        let first = self.line(a.row);
        let head = &first[..byte_of(first, a.col)];
        let last = self.line(b.row);
        let joined = format!("{head}{}", &last[byte_of(last, b.col)..]);
        self.lines.splice(a.row..=b.row, [joined]);
        text
    }

    /// Remove rows `r1..=r2`; the buffer keeps at least one (empty) line.
    pub fn delete_lines(&mut self, r1: usize, r2: usize) -> Vec<String> {
        let r2 = r2.min(self.last_row());
        if r1 > r2 {
            return Vec::new();
        }
        self.touch(r1);
        let removed: Vec<String> = self.lines.drain(r1..=r2).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        removed
    }

    /// Insert whole lines before row `at` (`at == line_count` appends).
    pub fn insert_lines(&mut self, at: usize, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let at = at.min(self.lines.len());
        self.touch(at.min(self.last_row()));
        self.lines.splice(at..at, lines);
    }

    pub fn set_line(&mut self, row: usize, s: String) {
        if self.lines[row] != s {
            self.touch(row);
            self.lines[row] = s;
        }
    }

    /// Replace rows `r1..=r2` with `new` (at least one line is kept).
    pub fn replace_lines(&mut self, r1: usize, r2: usize, new: Vec<String>) {
        if self.lines[r1..=r2] == new[..] {
            return;
        }
        self.touch(r1);
        self.lines.splice(r1..=r2, new);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Buffer {
        Buffer::new(s.split('\n').map(String::from).collect())
    }

    #[test]
    fn insert_and_delete_across_lines() {
        let mut b = buf("héllo\nworld");
        let end = b.insert(Pos::new(0, 2), "XY\nZ");
        assert_eq!(b.lines(), ["héXY", "Zllo", "world"]);
        assert_eq!(end, Pos::new(1, 1));
        assert_eq!(b.delete(Pos::new(0, 4), Pos::new(2, 1)), "\nZllo\nw");
        assert_eq!(b.lines(), ["héXYorld"]);
    }

    #[test]
    fn undo_groups_and_saved_state() {
        let mut b = buf("a");
        assert!(!b.modified());
        b.begin(Pos::new(0, 0));
        b.insert(Pos::new(0, 1), "b");
        b.insert(Pos::new(0, 2), "c");
        b.end();
        b.insert(Pos::new(0, 3), "d");
        assert_eq!(b.line(0), "abcd");
        b.undo();
        assert_eq!(b.line(0), "abc");
        b.undo();
        assert_eq!(b.line(0), "a");
        assert!(!b.modified());
        b.redo();
        assert_eq!(b.line(0), "abc");
        b.mark_saved();
        b.undo();
        assert!(b.modified());
        b.redo();
        assert!(!b.modified());
    }

    #[test]
    fn deleting_every_line_leaves_one() {
        let mut b = buf("a\nb");
        assert_eq!(b.delete_lines(0, 9), ["a", "b"]);
        assert_eq!(b.lines(), [""]);
    }
}
