//! Rendering and key-handling tests of the Branches tab against real temporary repositories.

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;

use super::tree::{Row, Section};
use super::*;
use crate::config::Config;
use crate::git::testutil::*;
use crate::ui::views::Tab;
use crate::ui::Popup;

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn screen(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let w = buf.area.width as usize;
    buf.content()
        .chunks(w)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(view: &mut BranchesView, ctx: &Ctx, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| view.draw(f, f.area(), ctx)).unwrap();
    screen(&term)
}

fn render_popup(p: &mut dyn Popup, ctx: &Ctx, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| p.draw(f, f.area(), ctx)).unwrap();
    screen(&term)
}

/// Repo with main (pushed to a bare origin, one commit ahead), feature/ü and a remote-only branch.
fn setup() -> (TempDir, TempDir, Ctx) {
    let bare = tempfile::tempdir().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(bare.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let (d, repo) = temp_repo();
    repo.run(&["remote", "add", "origin", &bare.path().to_string_lossy()])
        .unwrap();
    commit_files(&repo, &[("a.txt", "1\n")], "first commit");
    repo.run(&["push", "-q", "-u", "origin", "main"]).unwrap();
    repo.run(&["push", "-q", "origin", "main:refs/heads/remote-only"])
        .unwrap();
    repo.run(&["fetch", "-q"]).unwrap();
    repo.run(&["branch", "feature/ü"]).unwrap();
    commit_files(&repo, &[("b.txt", "2\n")], "second commit");
    (bare, d, Ctx::new(repo, Config::default()))
}

fn popup_of(a: Action) -> Option<Box<dyn Popup>> {
    match a {
        Action::Popup(p) => Some(p),
        Action::Batch(v) => v.into_iter().find_map(popup_of),
        _ => None,
    }
}

#[test]
fn renders_tree_and_details() {
    let (_b, _d, ctx) = setup();
    let mut v = BranchesView::new(&ctx);
    v.refresh(&ctx).unwrap();
    let s = render(&mut v, &ctx, 140, 30);
    for want in [
        "On main",
        "origin/main ↑1",
        "▾ Favorites",
        "▾ Local",
        "▾ Remote",
        "▾ origin",
        "feature/ü",
        "origin/remote-only",
        "second commit",
    ] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
    assert!(v.selected().is_some_and(|b| b.is_head && b.name == "main"));
    // Narrow terminal hides the details pane but still renders.
    let s = render(&mut v, &ctx, 50, 12);
    assert!(s.contains("Local"));
    assert!(!v.detail_visible);
    // Selecting another branch shows how it relates to the current one.
    // The count runs in the background once the selection rests; the app redraws every 100 ms.
    v.handle_key(key(KeyCode::Up), &ctx);
    let s = render(&mut v, &ctx, 140, 30);
    assert!(s.contains("counting…"), "{s}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut s = render(&mut v, &ctx, 140, 30);
    while !s.contains("ahead,") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
        s = render(&mut v, &ctx, 140, 30);
    }
    assert!(s.contains("ahead,"), "{s}");
}

#[test]
fn detached_head_header_and_push() {
    let (_b, _d, ctx) = setup();
    ctx.repo.run(&["checkout", "-q", "--detach"]).unwrap();
    let mut v = BranchesView::new(&ctx);
    v.refresh(&ctx).unwrap();
    assert!(render(&mut v, &ctx, 140, 20).contains("HEAD detached at"));
    assert!(matches!(ops::push_branch(&ctx, None), Action::Error(m) if m.contains("detached")));
    assert!(matches!(ops::push_in_terminal(&ctx), Action::Error(_)));
}

#[test]
fn empty_repo() {
    let (_e, empty) = temp_repo();
    let ctx = Ctx::new(empty, Config::default());
    let mut v = BranchesView::new(&ctx);
    v.refresh(&ctx).unwrap();
    assert!(render(&mut v, &ctx, 120, 10).contains("No branches yet"));
    assert!(matches!(
        v.handle_key(key(KeyCode::Char('n')), &ctx),
        Action::Popup(_)
    ));
    assert!(matches!(
        v.handle_key(key(KeyCode::Char('.')), &ctx),
        Action::Popup(_)
    ));
}

#[test]
fn filter_collapse_favorite_checkout() {
    let (_b, _d, ctx) = setup();
    let mut v = BranchesView::new(&ctx);
    v.refresh(&ctx).unwrap();
    v.handle_key(key(KeyCode::Char('/')), &ctx);
    assert!(v.captures_input());
    v.handle_key(key(KeyCode::Char('ü')), &ctx);
    v.handle_key(key(KeyCode::Enter), &ctx);
    assert!(!v.captures_input());
    assert_eq!(v.selected().map(|b| b.name.as_str()), Some("feature/ü"));
    assert!(render(&mut v, &ctx, 140, 20).contains("/ ü"));

    // Favorite toggling persists.
    v.handle_key(key(KeyCode::Char('*')), &ctx);
    assert!(Favorites::load(&ctx.repo).contains("refs/heads/feature/ü"));

    // Esc clears the filter; the selection is kept.
    v.handle_key(key(KeyCode::Esc), &ctx);
    assert!(v.filter.is_empty());
    assert_eq!(v.selected().map(|b| b.name.as_str()), Some("feature/ü"));

    // Context menu, then checkout with Enter.
    assert!(matches!(
        v.handle_key(key(KeyCode::Char('.')), &ctx),
        Action::Popup(_)
    ));
    assert!(matches!(
        v.handle_key(key(KeyCode::Enter), &ctx),
        Action::Batch(_)
    ));
    assert_eq!(ctx.repo.head_branch().as_deref(), Some("feature/ü"));

    // Collapse the Local section from its header, expand with →.
    v.refresh(&ctx).unwrap();
    while !matches!(
        v.selected_row(),
        Some(Row::Section {
            section: Section::Local,
            ..
        })
    ) {
        v.handle_key(key(KeyCode::Up), &ctx);
    }
    let before = v.rows.len();
    v.handle_key(key(KeyCode::Enter), &ctx);
    assert!(v.rows.len() < before);
    assert!(matches!(
        v.selected_row(),
        Some(Row::Section {
            collapsed: true,
            ..
        })
    ));
    v.handle_key(key(KeyCode::Right), &ctx);
    assert_eq!(v.rows.len(), before);
}

#[test]
fn checkout_with_conflicting_changes_offers_smart_and_force() {
    let (_b, _d, ctx) = setup();
    let repo = &ctx.repo;
    repo.run(&["checkout", "-q", "feature/ü"]).unwrap();
    commit_files(repo, &[("a.txt", "feature\n")], "feature edit");
    repo.run(&["checkout", "-q", "main"]).unwrap();
    write(repo, "a.txt", "dirty\n");
    let mut menu = popup_of(ops::run_checkout(
        &ctx,
        branch::CheckoutTarget::Existing("feature/ü".into()),
    ))
    .expect("smart checkout menu");
    let s = render_popup(menu.as_mut(), &ctx, 100, 20);
    assert!(s.contains("Smart checkout"), "{s}");
    assert!(s.contains("Force checkout"));
    let mut confirm =
        popup_of(menu.handle_key(key(KeyCode::Char('f')), &ctx)).expect("danger confirm");
    assert!(render_popup(confirm.as_mut(), &ctx, 100, 20).contains("Discard your local changes"));
    confirm.handle_key(key(KeyCode::Char('y')), &ctx);
    assert_eq!(repo.head_branch().as_deref(), Some("feature/ü"));
}

#[test]
fn compare_and_new_branch_popups() {
    let (_b, _d, ctx) = setup();
    let mut p = compare::ComparePopup::new(&ctx, "feature/ü");
    let s = render_popup(&mut p, &ctx, 140, 30);
    assert!(s.contains("In main but not in feature/ü (1)"), "{s}");
    assert!(s.contains("second commit"));
    assert!(matches!(
        p.handle_key(key(KeyCode::Char('d')), &ctx),
        Action::Popup(_)
    ));
    // The branch has nothing main lacks; the commit only in main is in the second list.
    assert!(matches!(
        p.handle_key(key(KeyCode::Enter), &ctx),
        Action::None
    ));
    p.handle_key(key(KeyCode::Tab), &ctx);
    assert!(matches!(
        p.handle_key(key(KeyCode::Enter), &ctx),
        Action::Popup(_)
    ));

    let mut nb = new_branch::NewBranchPopup::new(None);
    for c in "topic".chars() {
        nb.handle_key(key(KeyCode::Char(c)), &ctx);
    }
    nb.handle_key(key(KeyCode::Tab), &ctx);
    nb.handle_key(key(KeyCode::Char(' ')), &ctx); // untick "Checkout branch"
    assert!(render_popup(&mut nb, &ctx, 100, 20).contains("[ ] Checkout branch"));
    assert!(matches!(
        nb.handle_key(key(KeyCode::Enter), &ctx),
        Action::Batch(_)
    ));
    assert!(branch::local_exists(&ctx.repo, "topic"));
    assert_eq!(ctx.repo.head_branch().as_deref(), Some("main"));
    // A duplicate name shows an inline error and keeps the dialog open.
    let mut nb = new_branch::NewBranchPopup::new(None);
    for c in "topic".chars() {
        nb.handle_key(key(KeyCode::Char(c)), &ctx);
    }
    assert!(matches!(
        nb.handle_key(key(KeyCode::Enter), &ctx),
        Action::None
    ));
    assert!(render_popup(&mut nb, &ctx, 100, 20).contains("already exists"));
}

#[test]
fn remote_checkout_creates_tracking_branch() {
    let (_b, _d, ctx) = setup();
    let all = branch::list_branches(&ctx.repo).unwrap();
    let remote_only = all.iter().find(|b| b.name == "origin/remote-only").unwrap();
    let mut input = popup_of(ops::checkout(&ctx, remote_only)).expect("name prompt");
    assert!(render_popup(input.as_mut(), &ctx, 100, 20).contains("remote-only"));
    input.handle_key(key(KeyCode::Enter), &ctx);
    assert_eq!(ctx.repo.head_branch().as_deref(), Some("remote-only"));
    let up = crate::git::remote::upstream_of(&ctx.repo, "remote-only").unwrap();
    assert_eq!(up.short(), "origin/remote-only");
}

#[test]
fn merge_conflicts_switch_to_conflicts_tab() {
    let (_b, _d, ctx) = setup();
    let repo = &ctx.repo;
    repo.run(&["checkout", "-q", "feature/ü"]).unwrap();
    commit_files(repo, &[("a.txt", "feature\n")], "feature edit");
    repo.run(&["checkout", "-q", "main"]).unwrap();
    commit_files(repo, &[("a.txt", "main\n")], "main edit");
    let a = ops::integration(
        branch::merge(repo, "feature/ü", branch::MergeMode::Merge),
        "Merge",
    );
    let Action::Batch(v) = a else {
        panic!("batch expected")
    };
    assert!(v
        .iter()
        .any(|a| matches!(a, Action::SwitchTab(Tab::Conflicts))));
}

#[test]
fn delete_unmerged_asks_for_force() {
    let (_b, _d, ctx) = setup();
    let repo = &ctx.repo;
    repo.run(&["checkout", "-q", "-b", "wip"]).unwrap();
    commit_files(repo, &[("w.txt", "w\n")], "wip");
    repo.run(&["checkout", "-q", "main"]).unwrap();
    let all = branch::list_branches(repo).unwrap();
    let wip = all.iter().find(|b| b.name == "wip").unwrap();
    let mut confirm = popup_of(ops::delete(&ctx, wip)).expect("force delete confirm");
    assert!(render_popup(confirm.as_mut(), &ctx, 100, 20).contains("not fully merged"));
    confirm.handle_key(key(KeyCode::Char('y')), &ctx);
    assert!(!branch::local_exists(repo, "wip"));
    let main = all.iter().find(|b| b.name == "main").unwrap();
    assert!(matches!(ops::delete(&ctx, main), Action::Error(_)));
}

#[test]
fn palette_has_all_entries() {
    let names: Vec<&str> = palette().iter().map(|p| p.name).collect();
    for want in [
        "Fetch",
        "Pull…",
        "Update Project",
        "New branch…",
        "Checkout branch…",
        "Merge branch…",
        "Rebase onto…",
        "Delete branch…",
        "Fetch (in terminal)",
        "Pull (in terminal)",
        "Push (in terminal)",
    ] {
        assert!(names.contains(&want), "{want}");
    }
}

#[test]
fn checkout_blocked_by_untracked_files_names_them() {
    let (_b, _d, ctx) = setup();
    let repo = &ctx.repo;
    repo.run(&["checkout", "-q", "feature/ü"]).unwrap();
    commit_files(repo, &[("new.txt", "tracked on feature\n")], "add new.txt");
    repo.run(&["checkout", "-q", "main"]).unwrap();
    write(repo, "new.txt", "my untracked work\n");
    let mut menu = popup_of(ops::run_checkout(
        &ctx,
        branch::CheckoutTarget::Existing("feature/ü".into()),
    ))
    .expect("blocked checkout menu");
    let s = render_popup(menu.as_mut(), &ctx, 120, 20);
    assert!(!s.contains("Smart checkout"), "{s}");
    assert!(s.contains("Untracked files block checkout"), "{s}");
    assert!(s.contains("overwrite 1 untracked file"), "{s}");
    let mut info = popup_of(menu.handle_key(key(KeyCode::Char('v')), &ctx)).expect("file list");
    assert!(render_popup(info.as_mut(), &ctx, 120, 20).contains("new.txt"));
    let mut confirm =
        popup_of(menu.handle_key(key(KeyCode::Char('f')), &ctx)).expect("danger confirm");
    let s = render_popup(confirm.as_mut(), &ctx, 120, 20);
    assert!(
        s.contains("Untracked files that will be overwritten"),
        "{s}"
    );
    assert!(s.contains("new.txt"), "{s}");
    confirm.handle_key(key(KeyCode::Char('y')), &ctx);
    assert_eq!(repo.head_branch().as_deref(), Some("feature/ü"));
}

#[test]
fn new_branch_from_remote_uses_local_name_without_upstream() {
    let (_b, _d, ctx) = setup();
    let all = branch::list_branches(&ctx.repo).unwrap();
    let remote_only = all.iter().find(|b| b.name == "origin/remote-only").unwrap();
    let mut nb = new_branch::NewBranchPopup::new(Some(remote_only));
    let s = render_popup(&mut nb, &ctx, 100, 20);
    assert!(s.contains("remote-only"), "{s}");
    assert!(s.contains("Won't track origin/remote-only"), "{s}");
    assert!(matches!(
        nb.handle_key(key(KeyCode::Enter), &ctx),
        Action::Batch(_)
    ));
    assert_eq!(ctx.repo.head_branch().as_deref(), Some("remote-only"));
    assert!(crate::git::remote::upstream_of(&ctx.repo, "remote-only").is_none());
}

#[test]
fn overwriting_a_branch_with_unique_commits_asks_first() {
    let (_b, _d, ctx) = setup();
    let repo = &ctx.repo;
    repo.run(&["checkout", "-q", "-b", "topic"]).unwrap();
    commit_files(repo, &[("t.txt", "t\n")], "topic only");
    repo.run(&["checkout", "-q", "main"]).unwrap();
    let old_tip = repo.rev_parse("topic").unwrap();
    let mut nb = new_branch::NewBranchPopup::new(None);
    for c in "topic".chars() {
        nb.handle_key(key(KeyCode::Char(c)), &ctx);
    }
    nb.handle_key(key(KeyCode::Tab), &ctx);
    nb.handle_key(key(KeyCode::Tab), &ctx);
    nb.handle_key(key(KeyCode::Char(' ')), &ctx); // tick "Overwrite existing branch"
    let mut confirm = popup_of(nb.handle_key(key(KeyCode::Enter), &ctx)).expect("danger confirm");
    let s = render_popup(confirm.as_mut(), &ctx, 120, 20);
    assert!(s.contains("1 commit that is not in HEAD"), "{s}");
    assert!(s.contains(&old_tip[..8]), "{s}");
    // Declining leaves the branch alone.
    assert!(matches!(
        confirm.handle_key(key(KeyCode::Char('n')), &ctx),
        Action::ClosePopup
    ));
    assert_eq!(repo.rev_parse("topic").unwrap(), old_tip);
    // Declining left the dialog usable.
    let mut confirm = popup_of(nb.handle_key(key(KeyCode::Enter), &ctx)).expect("danger confirm");
    // Confirming closes the confirmation; the dialog then creates the branch and closes itself.
    confirm.handle_key(key(KeyCode::Char('y')), &ctx);
    assert!(matches!(nb.poll(&ctx), Action::Batch(_)));
    assert_eq!(
        repo.rev_parse("topic").unwrap(),
        repo.rev_parse("main").unwrap()
    );
    assert_eq!(repo.head_branch().as_deref(), Some("topic"));

    // Nothing to lose: no confirmation.
    let mut nb = new_branch::NewBranchPopup::new(None);
    for c in "topic".chars() {
        nb.handle_key(key(KeyCode::Char(c)), &ctx);
    }
    nb.handle_key(key(KeyCode::BackTab), &ctx);
    nb.handle_key(key(KeyCode::Char(' ')), &ctx);
    assert!(matches!(
        nb.handle_key(key(KeyCode::Enter), &ctx),
        Action::Batch(_)
    ));
}
