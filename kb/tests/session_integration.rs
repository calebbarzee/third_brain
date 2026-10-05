//! integration test for the `session` module: a temp repo with one committed document, a
//! session opened, edited, and settled, plus the secrets-gate refusal and `kb eod` paths.

use kb::config::Config;
use kb::git;
use kb::session::{self, SessionState};

const ID_ROOT: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";

const ROOT_DOC: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0A\"\nlayer = \"reference\"\ntitle = \"root doc\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# root doc\n\noriginal text.\n";

const NEW_DOC: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0B\"\nlayer = \"reference\"\ntitle = \"new doc\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# new doc\n\nfresh content.\n";

/// a repo with `kb::cli::init::init` skeleton plus one committed document on `main`.
fn seed_repo() -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path();
	kb::cli::init::init(root, false, false).unwrap();
	// `init` only runs `git init`; pin a repo-local identity so sessions opened here don't pick
	// up whatever the machine's global git config happens to hold.
	git::run(root, &["config", "user.name", "test user"]).unwrap();
	git::run(root, &["config", "user.email", "test@example.com"]).unwrap();
	std::fs::write(root.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC).unwrap();
	git::commit_all(root, "seed root doc").unwrap();
	dir
}

#[test]
fn open_edit_settle_cleanup() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();
	let main_head_before = git::head(root).unwrap();

	let mut s = session::open(&cfg, "add a new doc and touch the root doc", None, &[]).unwrap();
	assert_eq!(s.state, SessionState::Open);
	assert_eq!(s.branch, format!("session/{}", s.id));
	assert!(s.worktree.starts_with(cfg.state_dir()), "worktree {} should live under .kb", s.worktree.display());
	assert!(s.worktree.is_dir());
	assert_eq!(s.actor, "human:test@example.com");

	// two wip commits inside the worktree, as an agent would make one per operation.
	std::fs::write(s.worktree.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC.replace("original text.", "revised text.")).unwrap();
	git::commit_all(&s.worktree, "wip: revise root doc").unwrap();

	std::fs::write(s.worktree.join("docs").join("01K5A7Q2XW3ZRCM8T6YB4VJH0B.md"), NEW_DOC).unwrap();
	git::commit_all(&s.worktree, "wip: add new doc").unwrap();

	let commits = session::settle(&cfg, &mut s, None).unwrap();
	assert_eq!(commits.len(), 1);
	assert_eq!(s.state, SessionState::Settled);
	assert_eq!(s.ops, 2);

	// exactly one commit landed on the branch since base.
	let count = git::rev_list_count(&s.worktree, &format!("{}..HEAD", s.base_commit)).unwrap();
	assert_eq!(count, 1);

	let message = git::run(&s.worktree, &["log", "-1", "--format=%B", "HEAD"]).unwrap();
	assert!(message.starts_with("add a new doc and touch the root doc"));
	assert!(message.contains("01K5A7Q2XW3ZRCM8T6YB4VJH0A  root doc:"));
	assert!(message.contains("01K5A7Q2XW3ZRCM8T6YB4VJH0B  new doc: created"));
	assert!(message.contains("why:"));
	assert!(message.contains(&format!("Session: {}", s.id)));
	assert!(message.contains("Actor: human:test@example.com"));
	assert!(message.contains(&format!("Base: {}", s.base_commit)));
	assert!(message.contains("Ops: 2"));

	// main never moved.
	assert_eq!(git::head(root).unwrap(), main_head_before);

	session::cleanup(&cfg, &mut s, true).unwrap();
	assert_eq!(s.state, SessionState::Merged);
	assert!(!s.worktree.is_dir());
	let list = git::run(root, &["worktree", "list"]).unwrap();
	assert_eq!(list.lines().count(), 1, "only the main worktree should remain:\n{list}");
	assert!(git::rev_parse(root, &s.branch).is_err() || git::rev_parse(root, &s.branch).unwrap().is_empty());
}

#[test]
fn secret_in_staged_change_blocks_settle_and_makes_no_commit() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let mut s = session::open(&cfg, "leak a token by accident", None, &[]).unwrap();
	// left uncommitted, the way a human's editor leaves work before `settle` stages it: `settle`
	// itself must be the thing that catches it, via its own `git add -A` plus `scan_staged`.
	let leaked = format!("{}\nleaked = \"ghp_{}\"\n", ROOT_DOC.replace("original text.", "revised text."), "a".repeat(36));
	std::fs::write(s.worktree.join("docs").join(format!("{ID_ROOT}.md")), leaked).unwrap();

	let head_before = git::head(&s.worktree).unwrap();
	let err = session::settle(&cfg, &mut s, None).unwrap_err();
	assert!(format!("{err:#}").contains("secret"), "{err:#}");
	assert_eq!(s.state, SessionState::NeedsAuthor);
	assert_eq!(git::head(&s.worktree).unwrap(), head_before, "settle must not commit when secrets are found");

	// the session file on disk also reflects needs-author.
	let reloaded = session::load(&cfg, &s.id).unwrap();
	assert_eq!(reloaded.state, SessionState::NeedsAuthor);
}

#[test]
fn eod_settles_open_session_with_uncommitted_work() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let s = session::open(&cfg, "end of day still open", None, &[]).unwrap();
	// uncommitted change, never committed by hand.
	std::fs::write(s.worktree.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC.replace("original text.", "eod text.")).unwrap();

	let report = session::eod(&cfg).unwrap();
	assert_eq!(report.settled, 1);
	assert_eq!(report.abandoned, 0);

	let after = session::load(&cfg, &s.id).unwrap();
	assert_eq!(after.state, SessionState::Settled);
	assert_eq!(after.eod, Some(true));
	let count = git::rev_list_count(&after.worktree, &format!("{}..HEAD", after.base_commit)).unwrap();
	assert_eq!(count, 1);
}

#[test]
fn resume_recreates_a_missing_worktree() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let s = session::open(&cfg, "will be evicted", None, &[]).unwrap();
	let worktree = s.worktree.clone();
	git::worktree_remove(root, &worktree).unwrap();
	assert!(!worktree.is_dir());

	let resumed = session::resume(&cfg, &s.id, "").unwrap();
	assert_eq!(resumed.worktree, worktree);
	assert!(worktree.is_dir());
	assert_eq!(git::rev_parse(root, &resumed.branch).unwrap(), git::head(&worktree).unwrap());
}

#[test]
fn sparse_session_checks_out_only_candidate_and_shared_paths() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let s = session::open(&cfg, "sparse candidate set", None, &[ID_ROOT.to_string()]).unwrap();
	assert!(s.worktree.join("docs").join(format!("{ID_ROOT}.md")).is_file());
	assert!(s.worktree.join("kb.toml").is_file());
	// the other worktree's file that was never in the candidate set is not checked out here
	// (the flat file for a different id was never part of `sparse`).
	let unrelated = s.worktree.join("docs").join("01K5A7Q2XW3ZRCM8T6YB4VJH0B.md");
	assert!(!unrelated.is_file());
}
