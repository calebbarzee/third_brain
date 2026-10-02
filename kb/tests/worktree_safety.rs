//! regression tests for the 2026-09-28 audit's work-loss findings: tick removing worktrees kb
//! does not own (P0-1), landing and abandon deleting uncommitted files (P0-2), a stale session
//! base after rebase undoing a revert (P0-6), and `kb eod` stopping at the first bad session.

use kb::config::Config;
use kb::git;
use kb::merge::{self, MergeOpts, history, tick};
use kb::session::{self, SessionState};

const ID_A: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";
const ID_C: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0C";

fn doc_text(id: &str, title: &str, body: &str) -> String {
    format!("+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"{title}\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# {title}\n\n{body}\n")
}

fn doc_path(root: &std::path::Path, id: &str) -> std::path::PathBuf {
    root.join("docs").join(format!("{id}.md"))
}

/// `kb init` skeleton with documents A and C on main and a fixed git identity.
fn seed_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    git::run(root, &["config", "user.name", "test user"]).unwrap();
    git::run(root, &["config", "user.email", "test@example.com"]).unwrap();
    std::fs::write(doc_path(root, ID_A), doc_text(ID_A, "doc a", "alpha")).unwrap();
    std::fs::write(doc_path(root, ID_C), doc_text(ID_C, "doc c", "gamma")).unwrap();
    git::commit_all(root, "seed a and c").unwrap();
    dir
}

fn edit_in_session(cfg: &Config, objective: &str, id: &str, title: &str, body: &str) -> session::Session {
    let mut s = session::open(cfg, objective, None, &[]).unwrap();
    std::fs::write(doc_path(&s.worktree, id), doc_text(id, title, body)).unwrap();
    git::commit_all(&s.worktree, &format!("wip: {objective}")).unwrap();
    session::settle(cfg, &mut s, None).unwrap();
    s
}

#[test]
fn tick_leaves_a_foreign_worktree_and_its_uncommitted_files_alone() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let foreign = root.join(".claude").join("worktrees").join("myfeature");
    git::run(root, &["worktree", "add", "-b", "myfeature", &foreign.display().to_string()]).unwrap();
    std::fs::write(foreign.join("draft.txt"), "unsaved").unwrap();

    let report = tick::tick(&cfg).unwrap();

    assert_eq!(report.removed_worktrees, 0, "{report:?}");
    assert!(foreign.join("draft.txt").is_file(), "tick deleted a worktree it does not own");
}

#[test]
fn tick_keeps_an_orphan_kb_worktree_that_holds_uncommitted_work() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    // an orphan: a worktree under kb's own directory with no session file and no session branch.
    let orphan_id = "01K5A7Q2XW3ZRCM8T6YB4VJH9Z";
    let orphan = cfg.state_dir().join("worktrees").join(orphan_id);
    git::run(root, &["worktree", "add", "--detach", &orphan.display().to_string()]).unwrap();
    std::fs::write(orphan.join("notes.txt"), "unsaved").unwrap();

    let report = tick::tick(&cfg).unwrap();

    assert_eq!(report.kept_dirty_worktrees, 1, "{report:?}");
    assert!(orphan.join("notes.txt").is_file());

    std::fs::remove_file(orphan.join("notes.txt")).unwrap();
    let report = tick::tick(&cfg).unwrap();
    assert_eq!(report.removed_worktrees, 1, "a clean orphan kb worktree is still removed: {report:?}");
    assert!(!orphan.exists());
}

#[test]
fn merge_refuses_a_session_with_uncommitted_work_and_keeps_it() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let s = edit_in_session(&cfg, "edit a", ID_A, "doc a", "alpha 2");
    let late = s.worktree.join("docs").join("late.md");
    std::fs::write(&late, "# written after settle\n").unwrap();
    let main_before = git::head(root).unwrap();

    let err = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap_err().to_string();

    assert!(err.contains("docs/late.md"), "{err}");
    assert!(late.is_file(), "merge deleted a file that exists on no branch");
    assert_eq!(git::head(root).unwrap(), main_before);
    assert_eq!(session::load(&cfg, &s.id).unwrap().state, SessionState::Settled);
}

#[test]
fn merge_refuses_an_unsettled_session() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let s = session::open(&cfg, "never settled", None, &[]).unwrap();
    std::fs::write(doc_path(&s.worktree, ID_A), doc_text(ID_A, "doc a", "raw wip")).unwrap();
    git::commit_all(&s.worktree, "wip: raw").unwrap();

    let err = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap_err().to_string();

    assert!(err.contains("settle"), "{err}");
}

#[test]
fn abandon_refuses_a_worktree_with_uncommitted_work() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let s = session::open(&cfg, "half done", None, &[]).unwrap();
    std::fs::write(s.worktree.join("docs").join("half.md"), "# half\n").unwrap();

    assert!(session::abandon(&cfg, &s.id).is_err());
    assert!(s.worktree.join("docs").join("half.md").is_file());
}

/// the audit's repro: S1 edits A; S2 edits C and lands; S1 runs a check-only merge (which
/// rebases it) and settles again; S2 is reverted; S1 lands. C must stay reverted, and S1's
/// commit must touch only A.
#[test]
fn resettling_after_a_rebase_does_not_carry_other_sessions_changes() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();

    let mut s1 = edit_in_session(&cfg, "edit a", ID_A, "doc a", "alpha 2");
    let s2 = edit_in_session(&cfg, "edit c", ID_C, "doc c", "gamma 2");
    assert!(merge::merge(&cfg, &s2.id, MergeOpts::default()).unwrap().landed.is_some());

    let checked = merge::merge(&cfg, &s1.id, MergeOpts { check_only: true, ..MergeOpts::default() }).unwrap();
    assert!(checked.landed.is_none());
    s1 = session::load(&cfg, &s1.id).unwrap();
    session::settle(&cfg, &mut s1, None).unwrap();

    let reverted = history::revert(&cfg, &s2.id).unwrap();
    assert!(reverted.landed.is_some(), "{reverted:?}");
    assert!(std::fs::read_to_string(doc_path(root, ID_C)).unwrap().contains("gamma\n"));

    let landed = merge::merge(&cfg, &s1.id, MergeOpts::default()).unwrap();
    let head = landed.landed.expect("s1 lands");

    let c_now = std::fs::read_to_string(doc_path(root, ID_C)).unwrap();
    assert!(c_now.contains("gamma\n") && !c_now.contains("gamma 2"), "s1 undid the revert of s2:\n{c_now}");
    let touched = git::run(root, &["diff-tree", "--no-commit-id", "--name-only", "-r", &head]).unwrap();
    assert_eq!(touched.trim(), format!("docs/{ID_A}.md"), "s1's commit touched more than its own document");
}

#[test]
fn eod_skips_an_empty_session_and_still_settles_the_next_one() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let empty = session::open(&cfg, "opened, nothing written", None, &[]).unwrap();
    let busy = session::open(&cfg, "uncommitted edit", None, &[]).unwrap();
    std::fs::write(doc_path(&busy.worktree, ID_A), doc_text(ID_A, "doc a", "alpha eod")).unwrap();

    let report = session::eod(&cfg).unwrap();

    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.settled, 1, "{report:?}");
    assert_eq!(session::load(&cfg, &busy.id).unwrap().state, SessionState::Settled);
    assert_eq!(session::load(&cfg, &empty.id).unwrap().state, SessionState::Open);
}
