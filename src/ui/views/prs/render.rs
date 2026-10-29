//! Text layout for the Pull Requests tab: header, list rows, the details pane and the setup /
//! limited-mode screens.

use std::time::Instant;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::git::hosting::{
    auth, refs, web, CheckOutcome, DetectedBy, Hosting, PrDetails, PrState, ProviderKind,
    PullRequest, ReviewDecision,
};
use crate::theme::Theme;
use crate::util::{fit, relative_time, truncate};

use super::state::Setup;

/// Spinner frame for an operation started at `since`.
pub fn spinner(since: Instant) -> &'static str {
    crate::app::spinner_frame(since)
}

/// Greedy word wrap to `width` display columns. Input lines (LF or CRLF) are kept; words longer
/// than `width` are hard-broken. Always returns at least one line.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r').replace('\t', "    ");
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0;
        for word in line.split(' ') {
            let ww = word.width();
            if cur_w > 0 && cur_w + 1 + ww <= width {
                cur.push(' ');
                cur.push_str(word);
                cur_w += 1 + ww;
                continue;
            }
            if cur_w > 0 {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if ww <= width {
                cur.push_str(word);
                cur_w = ww;
                continue;
            }
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if cur_w + cw > width && cur_w > 0 {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(c);
                cur_w += cw;
            }
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Glyph and color of a request's state.
pub fn state_marker(pr: &PullRequest, t: &Theme) -> (&'static str, Style) {
    if pr.draft && pr.state == PrState::Open {
        return ("◌", t.dimmed());
    }
    match pr.state {
        PrState::Open => ("●", Style::default().fg(t.success)),
        PrState::Merged => ("◆", Style::default().fg(t.renamed)),
        PrState::Closed => ("✕", Style::default().fg(t.error)),
        PrState::Locked => ("■", Style::default().fg(t.warning)),
    }
}

fn review_cell(r: Option<ReviewDecision>, t: &Theme) -> (&'static str, Style) {
    match r {
        Some(ReviewDecision::Approved) => ("✓ approved", Style::default().fg(t.success)),
        Some(ReviewDecision::ChangesRequested) => ("✗ changes", Style::default().fg(t.error)),
        Some(ReviewDecision::ReviewRequired) => ("● review", Style::default().fg(t.warning)),
        None => ("", t.dimmed()),
    }
}

/// Column widths for the list at a given total width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Columns {
    pub number: usize,
    pub title: usize,
    pub review: usize,
    pub author: usize,
    pub branches: usize,
    pub updated: usize,
}

/// Lay out columns: optional ones are dropped (branches, then author, review, updated) as space runs out.
pub fn columns(width: usize, number_width: usize) -> Columns {
    let mut c = Columns {
        number: number_width + 1,
        title: 0,
        review: 11,
        author: 14,
        branches: 30,
        updated: 15,
    };
    let fixed = |c: &Columns| 2 + c.number + 1 + c.review + c.author + c.branches + c.updated;
    let min_title = 24;
    for drop in 0..4 {
        if width >= fixed(&c) + min_title {
            break;
        }
        match drop {
            0 => c.branches = 0,
            1 => c.author = 0,
            2 => c.review = 0,
            _ => c.updated = 0,
        }
    }
    c.title = width.saturating_sub(fixed(&c)).max(1);
    c
}

/// One list row.
pub fn list_row(pr: &PullRequest, h: &Hosting, cols: Columns, t: &Theme) -> Line<'static> {
    let (glyph, gstyle) = state_marker(pr, t);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), gstyle),
        Span::styled(
            fit(&h.reference(pr.number), cols.number),
            Style::default().fg(t.hash),
        ),
        Span::raw(" "),
    ];
    let mut title = String::new();
    if pr.draft {
        title.push_str("[draft] ");
    }
    title.push_str(&pr.title);
    let title_style = if pr.draft || pr.state != PrState::Open {
        t.dimmed()
    } else {
        t.text()
    };
    spans.push(Span::styled(fit(&title, cols.title), title_style));
    if cols.review > 0 {
        let (text, style) = review_cell(pr.review, t);
        spans.push(Span::styled(fit(&format!(" {text}"), cols.review), style));
    }
    if cols.author > 0 {
        spans.push(Span::styled(
            fit(&format!(" {}", pr.author), cols.author),
            Style::default().fg(t.author),
        ));
    }
    if cols.branches > 0 {
        spans.push(Span::styled(
            fit(&format!(" {} → {}", pr.head, pr.base), cols.branches),
            Style::default().fg(t.local_branch),
        ));
    }
    if cols.updated > 0 {
        let when = pr.updated.map(relative_time).unwrap_or_default();
        spans.push(Span::styled(
            fit(&format!(" {when}"), cols.updated),
            Style::default().fg(t.date),
        ));
    }
    Line::from(spans)
}

fn heading(text: impl Into<String>, t: &Theme) -> Line<'static> {
    Line::styled(text.into(), t.title())
}

fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    text: &str,
    indent: usize,
    width: usize,
    style: Style,
) {
    let pad = " ".repeat(indent);
    for l in wrap_text(text, width.saturating_sub(indent).max(10)) {
        out.push(Line::styled(format!("{pad}{l}"), style));
    }
}

fn review_state_style(state: &str, t: &Theme) -> Style {
    match state {
        "APPROVED" => Style::default().fg(t.success),
        "CHANGES_REQUESTED" => Style::default().fg(t.error),
        "REQUESTED" | "PENDING" => Style::default().fg(t.warning),
        _ => t.dimmed(),
    }
}

/// Header lines that only need the list row (shown while details load).
fn summary_lines(
    pr: &PullRequest,
    h: &Hosting,
    width: usize,
    t: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let title = format!("{} {}", h.reference(pr.number), pr.title);
    for l in wrap_text(&title, width) {
        out.push(Line::styled(l, t.text()));
    }
    let (glyph, gstyle) = state_marker(pr, t);
    let mut meta = vec![Span::styled(
        format!("{glyph} {}", pr.state.label()),
        gstyle,
    )];
    if pr.draft {
        meta.push(Span::styled(" · draft", t.dimmed()));
    }
    if !pr.author.is_empty() {
        meta.push(Span::styled(" · by ", t.dimmed()));
        meta.push(Span::styled(
            pr.author.clone(),
            Style::default().fg(t.author),
        ));
    }
    if let Some(ts) = pr.updated {
        meta.push(Span::styled(
            format!(" · updated {}", relative_time(ts)),
            Style::default().fg(t.date),
        ));
    }
    out.push(Line::from(meta));
    if !pr.head.is_empty() || !pr.base.is_empty() {
        out.push(Line::from(vec![
            Span::styled(pr.head.clone(), Style::default().fg(t.local_branch)),
            Span::styled(" → ", t.dimmed()),
            Span::styled(pr.base.clone(), Style::default().fg(t.remote_branch)),
        ]));
    }
    if let Some(r) = pr.review {
        let (text, style) = review_cell(Some(r), t);
        out.push(Line::styled(
            format!("Review: {} ({})", text, r.label()),
            style,
        ));
    }
    if !pr.labels.is_empty() {
        out.push(Line::styled(
            format!("Labels: {}", pr.labels.join(", ")),
            Style::default().fg(t.tag),
        ));
    }
}

/// Everything in the details pane, pre-wrapped to `width`.
pub fn detail_lines(
    pr: &PullRequest,
    details: Option<&PrDetails>,
    h: &Hosting,
    width: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // Prefer the fresher list row when details came from an older load, but fill gaps from details.
    let merged;
    let pr = match details {
        Some(d) if d.pr.number == pr.number && !d.pr.title.is_empty() => {
            merged = PullRequest {
                review: pr.review.or(d.pr.review),
                labels: if pr.labels.is_empty() {
                    d.pr.labels.clone()
                } else {
                    pr.labels.clone()
                },
                ..pr.clone()
            };
            &merged
        }
        _ => pr,
    };
    summary_lines(pr, h, width, t, &mut out);
    let Some(d) = details else { return out };

    let mut stats = Vec::new();
    if let (Some(a), Some(r)) = (d.additions, d.deletions) {
        stats.push(Span::styled(format!("+{a}"), Style::default().fg(t.added)));
        stats.push(Span::styled(
            format!(" −{r}"),
            Style::default().fg(t.removed),
        ));
    }
    if let Some(n) = d.changed_files {
        let lead = if stats.is_empty() { "" } else { " in " };
        stats.push(Span::styled(
            format!("{lead}{n} file{}", if n == 1 { "" } else { "s" }),
            t.dimmed(),
        ));
    }
    if let Some(m) = &d.mergeable {
        let lower = m.to_ascii_lowercase();
        let style = if lower.contains("conflict") || lower.contains("cannot") {
            Style::default().fg(t.error)
        } else if lower.starts_with("mergeable") {
            Style::default().fg(t.success)
        } else {
            t.dimmed()
        };
        stats.push(Span::styled(
            format!(" · {}", lower.replace('_', " ")),
            style,
        ));
    }
    if !stats.is_empty() {
        out.push(Line::from(stats));
    }

    if !d.checks.is_empty() {
        out.push(Line::raw(""));
        let c = &d.checks;
        let mut spans = vec![Span::styled("Checks: ", t.title())];
        let parts = [
            (CheckOutcome::Success, "✓", "passed", t.success),
            (CheckOutcome::Failure, "✗", "failed", t.error),
            (CheckOutcome::Pending, "●", "pending", t.warning),
            (CheckOutcome::Skipped, "-", "skipped", t.dim),
        ];
        for (outcome, glyph, word, color) in parts {
            let n = c.count(outcome);
            if n > 0 {
                spans.push(Span::styled(
                    format!("{glyph} {n} {word}  "),
                    Style::default().fg(color),
                ));
            }
        }
        out.push(Line::from(spans));
        for check in c
            .items
            .iter()
            .filter(|c| matches!(c.outcome, CheckOutcome::Failure | CheckOutcome::Pending))
            .take(12)
        {
            let (glyph, color) = if check.outcome == CheckOutcome::Failure {
                ("✗", t.error)
            } else {
                ("●", t.warning)
            };
            out.push(Line::styled(
                format!(
                    "  {glyph} {}",
                    truncate(&check.name, width.saturating_sub(4))
                ),
                Style::default().fg(color),
            ));
        }
    }

    if !d.reviews.is_empty() {
        out.push(Line::raw(""));
        out.push(heading(format!("Reviews ({})", d.reviews.len()), t));
        for r in &d.reviews {
            let mut spans = vec![
                Span::styled(format!("  {}", r.author), Style::default().fg(t.author)),
                Span::styled(
                    format!(" {}", r.state.to_lowercase().replace('_', " ")),
                    review_state_style(&r.state, t),
                ),
            ];
            if let Some(ts) = r.submitted {
                spans.push(Span::styled(
                    format!(" · {}", relative_time(ts)),
                    Style::default().fg(t.date),
                ));
            }
            out.push(Line::from(spans));
            if !r.body.trim().is_empty() {
                push_wrapped(&mut out, r.body.trim(), 4, width, t.text());
            }
        }
    }

    out.push(Line::raw(""));
    out.push(heading("Description", t));
    if d.body.trim().is_empty() {
        out.push(Line::styled("  No description provided.", t.dimmed()));
    } else {
        push_wrapped(&mut out, d.body.trim_end(), 2, width, t.text());
    }

    if !d.files.is_empty() {
        out.push(Line::raw(""));
        out.push(heading(format!("Files ({})", d.files.len()), t));
        for f in d.files.iter().take(300) {
            match (f.additions, f.deletions) {
                (Some(a), Some(r)) => {
                    let stat = format!("+{a} −{r}");
                    let path_w = width.saturating_sub(stat.width() + 4);
                    out.push(Line::from(vec![
                        Span::styled(format!("  {} ", fit(&f.path, path_w)), t.text()),
                        Span::styled(format!("+{a}"), Style::default().fg(t.added)),
                        Span::styled(format!(" −{r}"), Style::default().fg(t.removed)),
                    ]));
                }
                _ => out.push(Line::styled(
                    format!("  {}", truncate(&f.path, width.saturating_sub(2))),
                    t.text(),
                )),
            }
        }
        if d.files.len() > 300 {
            out.push(Line::styled(
                format!("  … and {} more", d.files.len() - 300),
                t.dimmed(),
            ));
        }
    }

    out.push(Line::raw(""));
    out.push(heading(format!("Comments ({})", d.comments.len()), t));
    if d.comments.is_empty() {
        out.push(Line::styled(
            "  No comments yet. Press C to add one.",
            t.dimmed(),
        ));
    }
    for c in &d.comments {
        let mut spans = vec![Span::styled(
            format!("  {}", c.author),
            Style::default().fg(t.author),
        )];
        if let Some(ts) = c.created {
            spans.push(Span::styled(
                format!(" · {}", relative_time(ts)),
                Style::default().fg(t.date),
            ));
        }
        out.push(Line::from(spans));
        push_wrapped(&mut out, c.body.trim(), 4, width, t.text());
        out.push(Line::raw(""));
    }
    out
}

fn key_line(keys: &[(&str, &str)], t: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (k, d) in keys {
        spans.push(Span::styled(format!(" {k}"), t.key()));
        spans.push(Span::styled(format!(" {d}  "), t.dimmed()));
    }
    Line::from(spans)
}

/// One-line header above the list: provider, location and sign-in state.
pub fn header_line(h: &Hosting, auth_source: Option<&str>, t: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(h.kind.name().to_string(), t.title()),
        Span::styled(format!(" · {}", h.location()), t.text()),
    ];
    match auth_source {
        Some(src) => spans.push(Span::styled(
            format!("  · signed in ({src})"),
            Style::default().fg(t.success),
        )),
        None => spans.push(Span::styled(
            "  · not signed in (read-only) — L to sign in",
            Style::default().fg(t.warning),
        )),
    }
    Line::from(spans)
}

/// `[[hosting.hosts]]` snippet for the config file.
fn config_lines(h: &Hosting, t: &Theme, out: &mut Vec<Line<'static>>) {
    let code = Style::default().fg(t.accent);
    out.push(heading("Configure the provider", t));
    out.push(Line::styled(
        format!(
            "If {} runs GitHub Enterprise, GitLab, Bitbucket Server/Data Center, Azure DevOps \
             Server or Gitea/Forgejo, add to {}:",
            h.url.host,
            crate::config::Config::path().display()
        ),
        t.dimmed(),
    ));
    out.push(Line::styled("  [[hosting.hosts]]", code));
    out.push(Line::styled(
        format!("  host = \"{}\"", h.url.credential_host()),
        code,
    ));
    out.push(Line::styled(
        "  kind = \"gitlab\"   # github, gitlab, bitbucket, bitbucket-server, azure, gitea, none",
        code,
    ));
    out.push(Line::styled(
        format!(
            "  # api_url = \"{}/api/v4\"   # only when not the default",
            h.url.web_base
        ),
        code,
    ));
}

fn token_lines(h: &Hosting, t: &Theme, out: &mut Vec<Line<'static>>) {
    let host = h.url.credential_host();
    out.push(heading("Sign in", t));
    out.push(Line::styled(
        format!(
            "Press L to enter a token; your git credential helper stores it (never gitjet's \
             config). Create one under: {}.",
            h.kind.token_help()
        ),
        t.text(),
    ));
    let mut env = vec![
        format!("GITJET_TOKEN_{}", auth::host_env_suffix(&host)),
        "GITJET_TOKEN".to_string(),
    ];
    env.extend(
        auth::provider_env_vars(h.kind, &host)
            .iter()
            .map(|v| v.to_string()),
    );
    out.push(Line::styled(
        format!(
            "Tokens are also read from the environment: {}.",
            env.join(", ")
        ),
        t.dimmed(),
    ));
}

/// Lines for every state without a request list. `branch` / `head` are the current branch and
/// HEAD commit, for the browser shortcuts.
pub fn setup_lines(
    setup: &Setup,
    branch: Option<&str>,
    head: Option<&str>,
    started: Instant,
    t: &Theme,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    match setup {
        Setup::Ready(_) => {}
        Setup::Detecting => {
            out.push(Line::styled(
                format!(
                    "{} Looking at the remotes and their hosting provider…",
                    spinner(started)
                ),
                t.dimmed(),
            ));
        }
        Setup::NoRemotes => {
            out.push(heading("This repository has no remotes", t));
            out.push(Line::raw(""));
            out.push(Line::styled(
                "Pull requests need a remote on a hosting server, for example:",
                t.text(),
            ));
            out.push(Line::styled(
                "  git remote add origin https://example.com/OWNER/REPO.git",
                Style::default().fg(t.accent),
            ));
            out.push(Line::styled(
                "Remotes can also be added in the Repository tab.",
                t.dimmed(),
            ));
            out.push(Line::raw(""));
            out.push(key_line(&[("R", "check again")], t));
        }
        Setup::NotHosted { remote, url } => {
            out.push(heading("No hosted remote", t));
            out.push(Line::raw(""));
            out.push(Line::styled(
                format!(
                    "Remote '{remote}' is a local path ({url}); there is no server to ask for \
                     pull requests."
                ),
                t.text(),
            ));
            out.push(Line::raw(""));
            out.push(key_line(&[("R", "check again")], t));
        }
        Setup::Limited { hosting: h, reason } => {
            out.push(Line::from(vec![
                Span::styled(h.kind.name().to_string(), t.title()),
                Span::styled(format!(" · {}", h.location()), t.text()),
            ]));
            out.push(Line::styled(
                format!("Remote '{}' · {}", h.remote, h.detected_by.label()),
                t.dimmed(),
            ));
            out.push(Line::raw(""));
            let style = if h.kind.has_api() {
                Style::default().fg(t.error)
            } else {
                t.text()
            };
            for l in reason.lines().take(8) {
                out.push(Line::styled(l.to_string(), style));
            }
            out.push(Line::raw(""));
            out.push(heading("Available", t));
            out.push(key_line(&[("W", "open the repository in the browser")], t));
            if let Some(b) = branch {
                out.push(key_line(
                    &[("B", "open the current branch in the browser")],
                    t,
                ));
                out.push(Line::styled(
                    format!("     {}", web::branch_url(h, b)),
                    t.dimmed(),
                ));
            }
            if head.is_some() {
                out.push(key_line(&[("H", "open the HEAD commit in the browser")], t));
            }
            if web::create_url(h, "x", "y", None).is_some() {
                out.push(key_line(
                    &[(
                        "N",
                        "push the branch and create a pull request in the browser",
                    )],
                    t,
                ));
            }
            if refs::pr_ref(h.kind, 1).is_some() {
                out.push(key_line(
                    &[
                        ("c", "check out a request by number (git fetch)"),
                        ("d", "diff a request by number"),
                    ],
                    t,
                ));
            }
            if h.kind.has_api() {
                out.push(key_line(&[("L", "sign in")], t));
            }
            out.push(key_line(&[("R", "check again")], t));
            out.push(Line::raw(""));
            if h.kind.has_api() {
                token_lines(h, t, &mut out);
                out.push(Line::raw(""));
            }
            if h.kind == ProviderKind::Generic || h.detected_by != DetectedBy::KnownHost {
                config_lines(h, t, &mut out);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::hosting::{parse_remote_url, ChangedFile, Checks, Comment, Review};

    fn hosting(kind: ProviderKind, url: &str) -> Hosting {
        Hosting::new(
            kind,
            "origin",
            url,
            parse_remote_url(url).expect("url"),
            None,
            DetectedBy::KnownHost,
        )
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wraps_words_and_long_tokens() {
        assert_eq!(wrap_text("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_text("a\r\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap_text("", 10), vec![""]);
        assert_eq!(
            wrap_text("日本語テキスト", 6),
            vec!["日本語", "テキス", "ト"]
        );
        for l in wrap_text(
            "some fairly long sentence with words that need wrapping in a pane",
            13,
        ) {
            assert!(l.width() <= 13, "{l:?} too wide");
        }
    }

    #[test]
    fn columns_degrade_with_width() {
        let wide = columns(160, 4);
        assert!(wide.branches > 0 && wide.author > 0 && wide.title >= 24);
        let narrow = columns(50, 4);
        assert_eq!(narrow.branches, 0);
        assert_eq!(narrow.author, 0);
        let tiny = columns(20, 4);
        assert_eq!((tiny.review, tiny.updated), (0, 0));
        assert!(tiny.title >= 1);
    }

    #[test]
    fn details_render_all_sections() {
        let mut checks = Checks::default();
        checks.push("ci", "failure");
        let d = PrDetails {
            pr: PullRequest {
                number: 5,
                title: "Add thing".into(),
                author: "al".into(),
                head: "feat".into(),
                base: "main".into(),
                ..Default::default()
            },
            body: "Why this matters".into(),
            comments: vec![Comment {
                author: "bo".into(),
                body: "Nice work".into(),
                created: None,
            }],
            reviews: vec![Review {
                author: "cy".into(),
                state: "APPROVED".into(),
                body: String::new(),
                submitted: None,
            }],
            checks,
            files: vec![
                ChangedFile {
                    path: "a.rs".into(),
                    additions: Some(3),
                    deletions: Some(1),
                },
                ChangedFile {
                    path: "b.rs".into(),
                    additions: None,
                    deletions: None,
                },
            ],
            additions: Some(3),
            deletions: Some(1),
            mergeable: Some("conflicting".into()),
            changed_files: Some(2),
        };
        let t = Theme::dark();
        let h = hosting(ProviderKind::GitHub, "https://github.com/o/r");
        let all = text(&detail_lines(&d.pr, Some(&d), &h, 60, &t));
        for needle in [
            "#5 Add thing",
            "feat → main",
            "+3",
            "in 2 files",
            "conflicting",
            "1 failed",
            "✗ ci",
            "Reviews (1)",
            "cy approved",
            "Why this matters",
            "Files (2)",
            "a.rs",
            "b.rs",
            "Comments (1)",
            "Nice work",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in\n{all}");
        }
        let summary = text(&detail_lines(&d.pr, None, &h, 60, &t));
        assert!(summary.contains("feat → main"));
        assert!(!summary.contains("Description"));
    }

    #[test]
    fn header_and_limited_screens() {
        let t = Theme::dark();
        let az = hosting(
            ProviderKind::Azure,
            "https://dev.azure.com/org/proj/_git/repo",
        );
        let hdr = text(&[header_line(&az, None, &t)]);
        assert!(
            hdr.contains("Azure DevOps · dev.azure.com/org/proj/repo"),
            "{hdr}"
        );
        assert!(hdr.contains("not signed in"));
        assert!(text(&[header_line(&az, Some("AZURE_DEVOPS_EXT_PAT"), &t)])
            .contains("signed in (AZURE_DEVOPS_EXT_PAT)"));

        let generic = hosting(
            ProviderKind::Generic,
            "https://git.example.org/team/app.git",
        );
        let s = text(&setup_lines(
            &Setup::Limited {
                hosting: generic,
                reason: "No pull request API is known for git.example.org.".into(),
            },
            Some("main"),
            Some("abc"),
            Instant::now(),
            &t,
        ));
        for needle in [
            "Git server · git.example.org/team/app",
            "W open the repository",
            "B open the current branch",
            "H open the HEAD",
            "[[hosting.hosts]]",
            "host = \"git.example.org\"",
            "R check again",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in\n{s}");
        }
        assert!(!s.contains("L sign in"));
        assert!(!s.contains("N push the branch"));

        let s = text(&setup_lines(
            &Setup::Limited {
                hosting: az,
                reason: "Azure DevOps requires signing in (HTTP 401)".into(),
            },
            None,
            None,
            Instant::now(),
            &t,
        ));
        for needle in [
            "L sign in",
            "N push the branch",
            "c check out a request",
            "AZURE_DEVOPS_EXT_PAT",
            "HTTP 401",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in\n{s}");
        }
        let none = text(&setup_lines(
            &Setup::NoRemotes,
            None,
            None,
            Instant::now(),
            &t,
        ));
        assert!(none.contains("git remote add origin"));
        let local = text(&setup_lines(
            &Setup::NotHosted {
                remote: "origin".into(),
                url: "/srv/r.git".into(),
            },
            None,
            None,
            Instant::now(),
            &t,
        ));
        assert!(local.contains("local path"));
    }
}
