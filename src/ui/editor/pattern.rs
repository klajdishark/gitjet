//! Vim search patterns and `:s` replacement strings, translated for the `regex` crate.
//!
//! Patterns are "magic" like in Vim: `\(` `\)` `\|` `\+` `\?` `\{n,m}` are special and their
//! bare forms are literal, `\<` `\>` are word boundaries. `\v` switches to very magic, where the
//! syntax is plain regex. `\c` / `\C` force case-insensitive / sensitive matching.

use std::iter::Peekable;
use std::str::Chars;

use regex::{Captures, Regex, RegexBuilder};

/// Compile a Vim pattern. Without `\c` / `\C`, `ignorecase` applies unless `smartcase` is on and
/// the pattern has an upper-case letter.
pub fn compile(pat: &str, ignorecase: bool, smartcase: bool) -> Result<Regex, String> {
    let (src, case) = translate(pat);
    let icase = case.unwrap_or_else(|| ignorecase && !(smartcase && has_upper(pat)));
    RegexBuilder::new(&src)
        .case_insensitive(icase)
        .build()
        .map_err(|e| {
            let e = e.to_string();
            let why = e.lines().last().unwrap_or_default().trim().to_string();
            format!("E383: Invalid pattern: {pat} ({why})")
        })
}

/// Upper-case letters that are not part of an escape like `\S`.
fn has_upper(pat: &str) -> bool {
    let mut esc = false;
    pat.chars().any(|c| {
        let upper = !esc && c.is_uppercase();
        esc = !esc && c == '\\';
        upper
    })
}

/// Escape a literal string as a Vim pattern (for `*` / `#`).
pub fn escape_literal(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '\\' | '/' | '.' | '*' | '$' | '^' | '~' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn translate(pat: &str) -> (String, Option<bool>) {
    let mut out = String::new();
    let mut very = false;
    let mut case = None;
    let mut it = pat.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                let Some(n) = it.next() else {
                    out.push_str(r"\\");
                    break;
                };
                match n {
                    'v' => very = true,
                    'm' => very = false,
                    'c' => case = Some(true),
                    'C' => case = Some(false),
                    '<' | '>' => out.push_str(r"\b"),
                    '{' if !very => out.push_str(&quantifier(&mut it)),
                    '(' | ')' | '|' | '+' | '?' if !very => out.push(n),
                    '=' if !very => out.push('?'),
                    'a' => out.push_str("[A-Za-z]"),
                    'A' => out.push_str("[^A-Za-z]"),
                    'l' => out.push_str("[a-z]"),
                    'L' => out.push_str("[^a-z]"),
                    'u' => out.push_str("[A-Z]"),
                    'U' => out.push_str("[^A-Z]"),
                    'x' => out.push_str("[0-9A-Fa-f]"),
                    'X' => out.push_str("[^0-9A-Fa-f]"),
                    'o' => out.push_str("[0-7]"),
                    'h' => out.push_str("[A-Za-z0-9_]"),
                    'e' => out.push_str(r"\x1b"),
                    's' | 'S' | 'd' | 'D' | 'w' | 'W' | 'n' | 't' | 'r' | '0'..='9' => {
                        out.push('\\');
                        out.push(n);
                    }
                    c => out.push_str(&regex::escape(&c.to_string())),
                }
            }
            '[' => class(&mut it, &mut out),
            '{' if very => out.push_str(&quantifier(&mut it)),
            '<' | '>' if very => out.push_str(r"\b"),
            '=' if very => out.push('?'),
            '(' | ')' | '|' | '+' | '?' | '{' | '}' if !very => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    (out, case)
}

/// The rest of a `\{...}` quantifier: `\{n,m}`, `\{n}`, `\{,m}`, `\{}` (= `*`) and the lazy
/// `\{-...}` forms.
fn quantifier(it: &mut Peekable<Chars>) -> String {
    let mut body = String::new();
    for c in it.by_ref() {
        if c == '}' {
            break;
        }
        if c != '\\' {
            body.push(c);
        }
    }
    let (lazy, body) = match body.strip_prefix('-') {
        Some(b) => (true, b.to_string()),
        None => (false, body),
    };
    let mut q = if body.is_empty() || body == "," {
        "*".to_string()
    } else if let Some(m) = body.strip_prefix(',') {
        format!("{{0,{m}}}")
    } else {
        format!("{{{body}}}")
    };
    if lazy {
        q.push('?');
    }
    q
}

/// A `[...]` collection: mostly the same syntax, but characters the regex crate treats as class
/// operators are escaped.
fn class(it: &mut Peekable<Chars>, out: &mut String) {
    let start = out.len();
    out.push('[');
    if it.peek() == Some(&'^') {
        out.push('^');
        it.next();
    }
    if it.peek() == Some(&']') {
        out.push_str(r"\]");
        it.next();
    }
    while let Some(c) = it.next() {
        match c {
            ']' => {
                out.push(']');
                return;
            }
            '\\' => match it.next() {
                Some(n @ ('n' | 't' | 'r' | 'd' | 's' | 'w' | '\\' | ']' | '^' | '-')) => {
                    out.push('\\');
                    out.push(n);
                }
                Some('e') => out.push_str(r"\x1b"),
                Some(n) => out.push_str(&regex::escape(&n.to_string())),
                None => out.push_str(r"\\"),
            },
            '[' if it.peek() == Some(&':') => {
                out.push('[');
                for c in it.by_ref() {
                    out.push(c);
                    if c == ']' {
                        break;
                    }
                }
            }
            '[' | '&' | '~' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    // Unterminated: Vim matches the `[` literally.
    out.insert(start, '\\');
}

#[derive(Clone, Copy)]
enum Case {
    Upper,
    Lower,
}

/// Expand a `:s` replacement for one match: `&` and `\0`-`\9` insert groups, `\n` / `\r` break
/// the line, `\t` is a tab, `\u` `\l` change the next char's case and `\U` `\L` ... `\E` a run.
pub fn expand(rep: &str, caps: &Captures) -> String {
    let mut out = String::new();
    let mut once: Option<Case> = None;
    let mut run: Option<Case> = None;
    let push = |out: &mut String, s: &str, once: &mut Option<Case>, run: Option<Case>| {
        for c in s.chars() {
            match once.take().or(run) {
                Some(Case::Upper) => out.extend(c.to_uppercase()),
                Some(Case::Lower) => out.extend(c.to_lowercase()),
                None => out.push(c),
            }
        }
    };
    let group = |g: usize| caps.get(g).map_or("", |m| m.as_str());
    let mut it = rep.chars();
    while let Some(c) = it.next() {
        match c {
            '&' => push(&mut out, group(0), &mut once, run),
            '\\' => match it.next() {
                Some(d @ '0'..='9') => {
                    push(&mut out, group(d as usize - '0' as usize), &mut once, run)
                }
                Some('n' | 'r') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => once = Some(Case::Upper),
                Some('l') => once = Some(Case::Lower),
                Some('U') => run = Some(Case::Upper),
                Some('L') => run = Some(Case::Lower),
                Some('e' | 'E') => run = None,
                Some(o) => push(&mut out, &o.to_string(), &mut once, run),
                None => out.push('\\'),
            },
            c => push(&mut out, &c.to_string(), &mut once, run),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(pat: &str, rep: &str, text: &str) -> String {
        let re = compile(pat, false, false).unwrap();
        re.replace_all(text, |c: &Captures| expand(rep, c))
            .into_owned()
    }

    #[test]
    fn magic_syntax() {
        assert_eq!(
            sub(r"\(\w\+\) \(\w\+\)", r"\2 \1", "hello world"),
            "world hello"
        );
        assert_eq!(sub(r"a\|b", "x", "abc"), "xxc");
        assert_eq!(sub(r"(a)", "x", "(a)a"), "xa");
        assert_eq!(sub(r"\<is\>", "IS", "this is"), "this IS");
        assert_eq!(sub(r"o\{2}", "0", "foo fo"), "f0 fo");
        assert_eq!(sub(r"a.\{-}b", "X", "aXbYb"), "XYb");
        assert_eq!(sub(r"\v(\w+)\@", r"<\1>", "me@"), "<me>");
        assert_eq!(sub(r"[a[]", "-", "a[b"), "--b");
        assert_eq!(sub(r"1.5", "x", "1.5 105"), "x x");
        assert_eq!(sub(r"1\.5", "x", "1.5 105"), "x 105");
        assert_eq!(sub(r"^\s\+", "", "   x"), "x");
    }

    #[test]
    fn replacement_specials() {
        assert_eq!(sub("o", "[&]", "foo"), "f[o][o]");
        assert_eq!(sub(r"\w\+", r"\u&", "foo bar"), "Foo Bar");
        assert_eq!(sub(r"\w\+", r"\U&\E!", "foo"), "FOO!");
        assert_eq!(sub(",", r"\n", "a,b"), "a\nb");
        assert_eq!(sub("a", r"\&", "a"), "&");
    }

    #[test]
    fn case_rules() {
        assert!(compile("abc", true, true).unwrap().is_match("ABC"));
        assert!(!compile("Abc", true, true).unwrap().is_match("ABC"));
        assert!(compile(r"\Sbc", true, true).unwrap().is_match("ABC"));
        assert!(compile(r"abc\c", false, false).unwrap().is_match("ABC"));
        assert!(compile(r"\(", false, false).is_err());
    }
}
