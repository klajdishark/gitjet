//! Syntax highlighting for diff lines (syntect), computed lazily and incrementally.
//!
//! Each hunk is highlighted twice — once as the old file (context + removed lines) and once as the
//! new file (context + added lines) — so multi-line constructs are colored correctly on both sides.
//! Work is bounded per frame by a line budget; lines not reached yet render without colors and are
//! filled in on the following frames.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::git::diff::{Hunk, LineKind};

/// Lines longer than this are never highlighted (minified files would stall the parser).
pub const MAX_LINE_LEN: usize = 1000;

/// The bundled syntax definitions, loaded on first use.
pub fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// The syntect theme called `name`, falling back to a bundled dark theme.
pub fn theme(name: &str) -> Option<&'static Theme> {
    let ts = theme_set();
    ts.themes
        .get(name)
        .or_else(|| ts.themes.get("base16-ocean.dark"))
        .or_else(|| ts.themes.values().next())
}

/// Syntax for a path, by extension, then by whole file name (`Makefile`, `Dockerfile`...).
/// `None` for plain text or unknown types.
pub fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty());
    let found = ext
        .and_then(|e| {
            ss.find_syntax_by_extension(e)
                .or_else(|| ss.find_syntax_by_extension(&e.to_lowercase()))
        })
        .or_else(|| ss.find_syntax_by_extension(name));
    found.filter(|s| s.name != "Plain Text")
}

/// A colored byte range of one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRange {
    pub style: Style,
    pub range: Range<usize>,
}

/// Which version of the file a line is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old = 0,
    New = 1,
}

impl Side {
    fn includes(self, k: LineKind) -> bool {
        match self {
            Side::Old => matches!(k, LineKind::Context | LineKind::Removed),
            Side::New => matches!(k, LineKind::Context | LineKind::Added),
        }
    }

    /// The side a line of kind `k` is displayed from.
    pub fn of(k: LineKind) -> Side {
        if k == LineKind::Removed {
            Side::Old
        } else {
            Side::New
        }
    }
}

struct SideState {
    state: HighlightLines<'static>,
    /// Next hunk line index to feed
    next: usize,
    out: Vec<Option<Vec<StyledRange>>>,
}

/// Incremental highlighter for one file of a diff.
pub struct FileHighlighter {
    syntax: Option<&'static SyntaxReference>,
    theme: Option<&'static Theme>,
    hunks: HashMap<usize, [SideState; 2]>,
}

impl FileHighlighter {
    pub fn new(path: &str, theme_name: &str) -> Self {
        let syntax = syntax_for_path(path);
        Self {
            syntax,
            theme: syntax.and_then(|_| theme(theme_name)),
            hunks: HashMap::new(),
        }
    }

    /// Colors for line `line` of hunk `hi` read from `side`, highlighting up to it first while
    /// `budget` (lines) lasts. `None` when the file type is unknown or the line is not reached yet.
    pub fn line(
        &mut self,
        hunk: &Hunk,
        hi: usize,
        line: usize,
        side: Side,
        budget: &mut usize,
    ) -> Option<&[StyledRange]> {
        let (syntax, theme) = (self.syntax?, self.theme?);
        let sides = self.hunks.entry(hi).or_insert_with(|| {
            let mk = || SideState {
                state: HighlightLines::new(syntax, theme),
                next: 0,
                out: vec![None; hunk.lines.len()],
            };
            [mk(), mk()]
        });
        let s = &mut sides[side as usize];
        while s.next <= line && s.next < hunk.lines.len() && *budget > 0 {
            let dl = &hunk.lines[s.next];
            if side.includes(dl.kind) {
                *budget -= 1;
                if dl.content.len() <= MAX_LINE_LEN {
                    s.out[s.next] = highlight_one(&mut s.state, &dl.content);
                }
            }
            s.next += 1;
        }
        s.out.get(line)?.as_deref()
    }
}

fn highlight_one(state: &mut HighlightLines<'static>, content: &str) -> Option<Vec<StyledRange>> {
    let text = format!("{content}\n");
    let pieces = state.highlight_line(&text, syntax_set()).ok()?;
    let mut out = Vec::with_capacity(pieces.len());
    let mut pos = 0;
    for (st, piece) in pieces {
        let end = (pos + piece.len()).min(content.len());
        if end > pos {
            out.push(StyledRange {
                style: convert(st),
                range: pos..end,
            });
        }
        pos += piece.len();
    }
    Some(out)
}

pub fn convert(st: syntect::highlighting::Style) -> Style {
    let c = st.foreground;
    let mut s = Style::default().fg(Color::Rgb(c.r, c.g, c.b));
    if st.font_style.contains(FontStyle::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if st.font_style.contains(FontStyle::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if st.font_style.contains(FontStyle::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::parse_unified;

    #[test]
    fn finds_syntaxes() {
        assert!(syntax_for_path("src/main.rs").is_some());
        assert!(syntax_for_path("dir with space/x.py").is_some());
        assert!(syntax_for_path("Makefile").is_some());
        assert!(syntax_for_path("notes.unknownext").is_none());
        assert!(syntax_for_path("README").is_none());
        assert!(theme("no-such-theme").is_some());
    }

    #[test]
    fn highlights_both_sides_within_budget() {
        let text = "diff --git a/m.rs b/m.rs
--- a/m.rs
+++ b/m.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let a = 1;
+    let b = \"two\";
 }
";
        let d = parse_unified(text);
        let h = &d[0].hunks[0];
        let mut hl = FileHighlighter::new("m.rs", "base16-ocean.dark");
        let mut budget = 1;
        // Budget for one line only: line 2 on the new side is not reached yet.
        assert!(hl.line(h, 0, 2, Side::New, &mut budget).is_none());
        let mut budget = 100;
        let new = hl.line(h, 0, 2, Side::New, &mut budget).unwrap().to_vec();
        assert!(new.len() > 1, "several colored tokens");
        assert_eq!(new.last().unwrap().range.end, h.lines[2].content.len());
        let old = hl.line(h, 0, 1, Side::Old, &mut budget).unwrap();
        assert_eq!(old.first().unwrap().range.start, 0);
        // An added line is not part of the old side.
        assert!(hl.line(h, 0, 2, Side::Old, &mut budget).is_none());
    }

    #[test]
    fn plain_text_is_not_highlighted() {
        let d = parse_unified(
            "diff --git a/d.zzz b/d.zzz\n--- a/d.zzz\n+++ b/d.zzz\n@@ -1 +1 @@\n-a\n+b\n",
        );
        let mut hl = FileHighlighter::new("d.zzz", "base16-ocean.dark");
        let mut budget = 10;
        assert!(hl
            .line(&d[0].hunks[0], 0, 1, Side::New, &mut budget)
            .is_none());
        assert_eq!(budget, 10);
    }
}
