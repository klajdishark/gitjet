//! The commit details pane: metadata, full message and changed files.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::git::log::Commit;
use crate::git::Repo;
use crate::theme::Theme;
use crate::util::{format_time, relative_time};

use super::rows::{label_style, labels, RefIndex};

/// A file changed by a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// `A`, `M`, `D`, `R`, `C`, `T`...
    pub status: char,
    pub path: String,
    /// Source path of renames and copies
    pub old_path: Option<String>,
}

impl ChangedFile {
    /// Paths to pass to a diff so renames are detected (new and old path).
    pub fn diff_paths(&self) -> Vec<String> {
        let mut v = vec![self.path.clone()];
        v.extend(self.old_path.clone());
        v
    }
}

/// Lazily loaded details of the selected commit.
#[derive(Debug, Clone)]
pub struct Details {
    pub id: String,
    pub message: String,
    pub files: Vec<ChangedFile>,
    pub error: Option<String>,
    /// Placeholder shown while the real details load in the background
    pub loading: bool,
}

/// More files than this are not listed (huge imports, vendoring commits).
pub const MAX_FILES: usize = 5000;
/// Loaded details kept for quick back-and-forth navigation.
const CACHE_SIZE: usize = 128;

impl Details {
    /// Shown until the background load finishes: the subject only.
    pub fn placeholder(c: &Commit) -> Self {
        Details {
            id: c.id.clone(),
            message: c.subject.clone(),
            files: Vec::new(),
            error: None,
            loading: true,
        }
    }

    /// Read the full message and changed files (two `git show` calls; run off the UI thread).
    pub fn load(repo: &Repo, id: &str) -> Self {
        let mut d = Details {
            id: id.to_string(),
            message: String::new(),
            files: Vec::new(),
            error: None,
            loading: false,
        };
        match repo.commit_message(id) {
            Ok(m) => d.message = m.replace("\r\n", "\n"),
            Err(e) => d.error = Some(format!("{e:#}")),
        }
        match repo.commit_files(id) {
            Ok(files) => {
                d.files = files
                    .into_iter()
                    .take(MAX_FILES)
                    .map(|(status, path, old_path)| ChangedFile {
                        status,
                        path,
                        old_path,
                    })
                    .collect()
            }
            Err(e) => d.error = Some(format!("{e:#}")),
        }
        d
    }
}

/// Loads [`Details`] on a background thread, one commit at a time. While the user scrolls quickly
/// only the commit selected when the running load finishes is loaded next; intermediate selections
/// are skipped. Results are cached by commit id (a commit's content never changes).
pub struct DetailsLoader {
    tx: Sender<Details>,
    rx: Receiver<Details>,
    /// Commit whose details are being loaded
    running: Option<String>,
    cache: HashMap<String, Details>,
}

impl DetailsLoader {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            running: None,
            cache: HashMap::new(),
        }
    }

    /// Collect finished loads into the cache.
    pub fn poll(&mut self) {
        while let Ok(d) = self.rx.try_recv() {
            if self.running.as_deref() == Some(d.id.as_str()) {
                self.running = None;
            }
            if self.cache.len() >= CACHE_SIZE {
                self.cache.clear();
            }
            self.cache.insert(d.id.clone(), d);
        }
    }

    /// Cached details of `id`.
    pub fn get(&self, id: &str) -> Option<Details> {
        self.cache.get(id).cloned()
    }

    /// Start loading `id` unless it is cached or another load is still running (the caller asks
    /// again on the next draw).
    pub fn request(&mut self, repo: &Repo, id: &str) {
        if self.running.is_some() || self.cache.contains_key(id) {
            return;
        }
        self.running = Some(id.to_string());
        let (repo, id, tx) = (repo.clone(), id.to_string(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Details::load(&repo, &id));
        });
    }

    /// True while a background load runs.
    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Drop failed loads so they are retried (after a refresh, e.g. once a fetch brought the commit).
    pub fn forget_errors(&mut self) {
        self.cache.retain(|_, d| d.error.is_none());
    }
}

fn field(name: &str, t: &Theme) -> Span<'static> {
    Span::styled(format!("{name:<10}"), t.dimmed())
}

fn when(ts: i64) -> String {
    format!("{} ({})", format_time(ts), relative_time(ts))
}

/// Metadata and message lines for the top of the details pane.
pub fn info_lines(c: &Commit, d: &Details, refs: &RefIndex, t: &Theme) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        field("Commit", t),
        Span::styled(c.id.clone(), Style::default().fg(t.hash)),
    ])];
    let n = c.short.len().max(7);
    let parents: Vec<Span<'static>> = if c.parents.is_empty() {
        vec![Span::styled("(root commit)", t.dimmed())]
    } else {
        c.parents
            .iter()
            .map(|p| {
                Span::styled(
                    format!("{} ", &p[..n.min(p.len())]),
                    Style::default().fg(t.hash),
                )
            })
            .collect()
    };
    let mut line = vec![field(
        if c.parents.len() > 1 {
            "Parents"
        } else {
            "Parent"
        },
        t,
    )];
    line.extend(parents);
    out.push(Line::from(line));
    out.push(Line::from(vec![
        field("Author", t),
        Span::styled(
            format!("{} <{}>", c.author, c.email),
            Style::default().fg(t.author),
        ),
    ]));
    out.push(Line::from(vec![
        field("", t),
        Span::styled(when(c.time), Style::default().fg(t.date)),
    ]));
    if c.committer != c.author || c.committer_email != c.email || c.commit_time != c.time {
        out.push(Line::from(vec![
            field("Committer", t),
            Span::styled(
                format!("{} <{}>", c.committer, c.committer_email),
                Style::default().fg(t.author),
            ),
        ]));
        out.push(Line::from(vec![
            field("", t),
            Span::styled(when(c.commit_time), Style::default().fg(t.date)),
        ]));
    }
    let labels = labels(&c.refs, refs);
    if !labels.is_empty() {
        let mut line = vec![field("Refs", t)];
        for l in labels {
            line.push(Span::styled(
                format!("[{}] ", l.text),
                label_style(l.kind, t),
            ));
        }
        out.push(Line::from(line));
    }
    out.push(Line::raw(""));
    if let Some(e) = &d.error {
        out.push(Line::styled(e.clone(), Style::default().fg(t.error)));
    }
    let mut lines = d.message.lines();
    if let Some(subject) = lines.next() {
        out.push(Line::styled(subject.to_string(), t.text()));
    }
    out.extend(lines.map(|l| Line::styled(l.to_string(), t.text())));
    if d.loading {
        out.push(Line::styled("Loading…", t.dimmed()));
    }
    out
}

/// Color for a status letter.
pub fn status_style(status: char, t: &Theme) -> Style {
    Style::default().fg(match status {
        'A' => t.added,
        'D' => t.removed,
        'R' | 'C' => t.renamed,
        'U' => t.conflict,
        _ => t.modified,
    })
}

/// One row of the changed-files list.
pub fn file_line(f: &ChangedFile, t: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{} ", f.status), status_style(f.status, t)),
        Span::styled(f.path.clone(), t.text()),
    ];
    if let Some(old) = &f.old_path {
        spans.push(Span::styled(format!("  ← {old}"), t.dimmed()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::*;

    #[test]
    fn loads_message_and_files_with_renames() {
        let (_d, repo) = temp_repo();
        commit_files(
            &repo,
            &[("old name.txt", "same content here\nline two\nline three\n")],
            "first",
        );
        repo.run(&["mv", "old name.txt", "new name.txt"]).unwrap();
        let id = commit_files(
            &repo,
            &[("added.bin", "\0\x01binary")],
            "rename\r\n\r\nbody",
        );
        let d = Details::load(&repo, &id);
        assert!(d.error.is_none());
        assert_eq!(d.message, "rename\n\nbody");
        let renamed = d
            .files
            .iter()
            .find(|f| f.status == 'R')
            .expect("rename detected");
        assert_eq!(renamed.path, "new name.txt");
        assert_eq!(renamed.old_path.as_deref(), Some("old name.txt"));
        assert_eq!(renamed.diff_paths(), vec!["new name.txt", "old name.txt"]);
        assert!(d
            .files
            .iter()
            .any(|f| f.status == 'A' && f.path == "added.bin"));

        let c = repo.commit_info(&id).unwrap();
        let t = Theme::dark();
        let lines = info_lines(&c, &d, &RefIndex::load(&repo), &t);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text[0].contains(&id));
        assert!(text
            .iter()
            .any(|l| l.contains("Test User <test@example.com>")));
        assert!(text.iter().any(|l| l.contains("[HEAD→main]")));
        assert!(text.iter().any(|l| l == "body"));
        assert!(file_line(renamed, &t)
            .to_string()
            .contains("← old name.txt"));
    }

    #[test]
    fn background_loader_caches_results() {
        let (_d, repo) = temp_repo();
        let c1 = commit_files(&repo, &[("a", "1\n")], "one\n\nbody");
        let c2 = commit_files(&repo, &[("b", "2\n")], "two");
        let mut l = DetailsLoader::new();
        l.request(&repo, &c1);
        l.request(&repo, &c2); // ignored: one load at a time
        for _ in 0..500 {
            l.poll();
            if !l.is_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let d = l.get(&c1).expect("loaded");
        assert!(!d.loading && d.message == "one\n\nbody");
        assert!(l.get(&c2).is_none());
        let c = repo.commit_info(&c2).unwrap();
        let p = Details::placeholder(&c);
        assert!(p.loading && p.message == "two" && p.files.is_empty());
        let lines = info_lines(&c, &p, &RefIndex::default(), &Theme::dark());
        assert_eq!(
            lines.last().map(|l| l.to_string()).as_deref(),
            Some("Loading…")
        );
        l.forget_errors();
        assert!(l.get(&c1).is_some());
    }

    #[test]
    fn missing_commit_reports_error() {
        let (_d, repo) = temp_repo();
        commit_files(&repo, &[("a", "1\n")], "x");
        let d = Details::load(&repo, "0000000000000000000000000000000000000000");
        assert!(d.error.is_some());
    }
}
