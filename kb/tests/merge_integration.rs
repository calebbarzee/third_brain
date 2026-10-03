//! integration tests for the `merge` module: design.md §13 (merge), §14 (conflicts), §17.4
//! (delete guard), §20 (secrets), §21 (history). temp repos are built with
//! `kb::cli::init::init` and driven through `kb::session::*` and `kb::merge::*`, the same way a
//! real caller would.

use kb::config::Config;
use kb::git;
use kb::merge::{self, MergeOpts};
use kb::model::MergeClass;
use kb::session::{self, SessionState};

const ID_ROOT: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";

const ROOT_DOC: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0A\"\nlayer = \"reference\"\ntitle = \"root doc\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# root doc\n\noriginal text.\n";

/// a repo with the `kb init` skeleton plus one committed document on `main`, with a fixed local
/// git identity so sessions opened here don't pick up the machine's own global config.
fn seed_repo() -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path();
	kb::cli::init::init(root, false, false).unwrap();
	git::run(root, &["config", "user.name", "test user"]).unwrap();
	git::run(root, &["config", "user.email", "test@example.com"]).unwrap();
	std::fs::write(root.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC).unwrap();
	git::commit_all(root, "seed root doc").unwrap();
	dir
}

/// a minimal, valid `docs/*.md` document: reference layer, editable, canon.
fn doc_text(id: &str, title: &str, body: &str) -> String {
	format!(
		"+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"{title}\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# {title}\n\n{body}\n"
	)
}

fn write_doc(worktree: &std::path::Path, id: &str, title: &str, body: &str) {
	std::fs::write(worktree.join("docs").join(format!("{id}.md")), doc_text(id, title, body)).unwrap();
}

// (a) open, add a document, settle, merge -> landed; main advanced by exactly one commit with
// the Session trailer; worktree gone; `.kb/last_main` written.
#[test]
fn merge_lands_and_advances_main_by_one_commit() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();
	let main_before = git::head(root).unwrap();

	let mut s = session::open(&cfg, "add a new doc", None, &[]).unwrap();
	let worktree = s.worktree.clone();
	write_doc(&worktree, "01K5A7Q2XW3ZRCM8T6YB4VJH1A", "doc a", "content a");
	git::commit_all(&worktree, "wip: add doc a").unwrap();
	session::settle(&cfg, &mut s, None).unwrap();

	let outcome = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap();
	assert!(outcome.landed.is_some(), "{outcome:?}");
	assert_eq!(outcome.class, MergeClass::Auto);
	assert_eq!(outcome.state, SessionState::Merged);

	let main_after = git::head(root).unwrap();
	assert_ne!(main_after, main_before);
	assert_eq!(git::rev_list_count(root, &format!("{main_before}..{main_after}")).unwrap(), 1);
	let message = git::commit_message(root, &main_after).unwrap();
	assert!(message.contains(&format!("Session: {}", s.id)), "{message}");

	assert!(!worktree.is_dir(), "session worktree should be removed after landing");
	// the primary checkout has main checked out: its index and working tree must follow the land,
	// otherwise a later commit from it would drop the landed files.
	let status = git::run(root, &["status", "--porcelain"]).unwrap();
	assert!(status.trim().is_empty(), "primary checkout is stale after landing:\n{status}");
	let landed_docs = git::run(root, &["ls-tree", "-r", "--name-only", "HEAD", "--", "docs/"]).unwrap();
	for path in landed_docs.lines().filter(|l| l.ends_with(".md")) {
		assert!(root.join(path).is_file(), "{path} missing from the primary working tree");
	}

	let last_main = std::fs::read_to_string(cfg.state_dir().join("last_main")).unwrap();
	assert_eq!(last_main.trim(), main_after);
}

// (b) two sessions on different files: merge both; the second rebases cleanly and lands too.
#[test]
fn two_sessions_on_different_files_both_land() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let mut s1 = session::open(&cfg, "add doc b1", None, &[]).unwrap();
	write_doc(&s1.worktree, "01K5A7Q2XW3ZRCM8T6YB4VJH1B", "doc b1", "content");
	git::commit_all(&s1.worktree, "wip: add doc b1").unwrap();
	session::settle(&cfg, &mut s1, None).unwrap();

	let mut s2 = session::open(&cfg, "add doc b2", None, &[]).unwrap();
	write_doc(&s2.worktree, "01K5A7Q2XW3ZRCM8T6YB4VJH1C", "doc b2", "content");
	git::commit_all(&s2.worktree, "wip: add doc b2").unwrap();
	session::settle(&cfg, &mut s2, None).unwrap();

	let out1 = merge::merge(&cfg, &s1.id, MergeOpts::default()).unwrap();
	assert!(out1.landed.is_some(), "{out1:?}");

	// s2 was opened before s1 landed, so its merge must rebase onto the new main.
	let out2 = merge::merge(&cfg, &s2.id, MergeOpts::default()).unwrap();
	assert!(out2.landed.is_some(), "{out2:?}");

	assert_eq!(git::rev_list_count(root, "main").unwrap(), 3); // seed + b1 + b2
	assert!(git::show_file(root, "main", "docs/01K5A7Q2XW3ZRCM8T6YB4VJH1B.md").is_ok());
	assert!(git::show_file(root, "main", "docs/01K5A7Q2XW3ZRCM8T6YB4VJH1C.md").is_ok());
}

// (c) two sessions editing the same lines of one document: the second merge stops NeedsHuman,
// with the worktree left mid-rebase (human sessions never attempt automatic resolution).
#[test]
fn conflicting_edit_stops_needs_human_mid_rebase() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	// both sessions branch off the same base before either merges, so the second is guaranteed
	// to rebase onto the first's landed commit rather than starting from it already.
	let mut s1 = session::open(&cfg, "edit root doc first", None, &[]).unwrap();
	std::fs::write(s1.worktree.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC.replace("original text.", "first edit.")).unwrap();
	git::commit_all(&s1.worktree, "wip: first edit").unwrap();
	session::settle(&cfg, &mut s1, None).unwrap();

	let mut s2 = session::open(&cfg, "edit root doc second", None, &[]).unwrap();
	std::fs::write(s2.worktree.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC.replace("original text.", "second edit.")).unwrap();
	git::commit_all(&s2.worktree, "wip: second edit").unwrap();
	session::settle(&cfg, &mut s2, None).unwrap();

	let out1 = merge::merge(&cfg, &s1.id, MergeOpts::default()).unwrap();
	assert!(out1.landed.is_some(), "{out1:?}");

	let out2 = merge::merge(&cfg, &s2.id, MergeOpts::default()).unwrap();
	assert_eq!(out2.state, SessionState::NeedsHuman, "{out2:?}");
	assert!(out2.landed.is_none());
	assert!(s2.worktree.is_dir(), "the conflicted worktree must not be removed");
	assert!(git::rebase_in_progress(&s2.worktree), "the worktree must be left mid-rebase");

	let reloaded = session::load(&cfg, &s2.id).unwrap();
	assert_eq!(reloaded.state, SessionState::NeedsHuman);
}

// (d) an agent session with a secret planted after settle (settle's own gate is bypassed with a
// direct `git commit --no-verify`): merge fails at policy with NeedsAuthor, main unchanged.
#[test]
fn secret_planted_after_settle_blocks_merge_with_needs_author() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();
	let main_before = git::head(root).unwrap();

	let mut s = session::open(&cfg, "agent adds a doc", Some("agent:curate@svc-curate"), &[]).unwrap();
	write_doc(&s.worktree, "01K5A7Q2XW3ZRCM8T6YB4VJH1D", "doc d", "content");
	git::commit_all(&s.worktree, "wip: add doc d").unwrap();
	session::settle(&cfg, &mut s, None).unwrap();

	// bypass settle's own secrets gate: append a fake token and commit directly with --no-verify.
	let path = s.worktree.join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH1D.md");
	let mut content = std::fs::read_to_string(&path).unwrap();
	content.push_str(&format!("\n<!-- ghp_{} -->\n", "b".repeat(36)));
	std::fs::write(&path, content).unwrap();
	git::run(&s.worktree, &["add", "-A"]).unwrap();
	git::run(&s.worktree, &["commit", "--no-verify", "-q", "-m", "sneak in a secret"]).unwrap();

	let outcome = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap();
	assert_eq!(outcome.state, SessionState::NeedsAuthor, "{outcome:?}");
	assert!(outcome.landed.is_none());
	assert!(outcome.checks.iter().any(|c| c.name == "secrets" && !c.pass));
	assert_eq!(git::head(root).unwrap(), main_before, "main must not move");
}

// (e) an agent's change to a `principle` layer document is always review class (critical = "*");
// the note lands in `kb review list`, and `kb review approve` lands it.
#[test]
fn principle_layer_change_goes_to_review_then_approve_lands_it() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let id = "01K5A7Q2XW3ZRCM8T6YB4VJH1E";
	let principle_doc = format!(
		"+++\nid = \"{id}\"\nlayer = \"principle\"\ntitle = \"a principle\"\nstatus = \"canon\"\nagent_policy = \"locked\"\nowner = \"caleb\"\n+++\n\n# a principle\n\noriginal text.\n"
	);
	std::fs::write(root.join("docs").join(format!("{id}.md")), &principle_doc).unwrap();
	git::commit_all(root, "seed principle doc").unwrap();

	let mut s = session::open(&cfg, "revise a principle", Some("agent:curate@svc-curate"), &[]).unwrap();
	std::fs::write(s.worktree.join("docs").join(format!("{id}.md")), principle_doc.replace("original text.", "revised text.")).unwrap();
	git::commit_all(&s.worktree, "wip: revise principle").unwrap();
	session::settle(&cfg, &mut s, None).unwrap();

	let outcome = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap();
	assert_eq!(outcome.class, MergeClass::Review, "{outcome:?}");
	assert!(outcome.landed.is_none());
	assert_eq!(outcome.state, SessionState::Review);

	let items = merge::review_list(&cfg).unwrap();
	assert_eq!(items.len(), 1);
	assert_eq!(items[0].session, s.id);
	assert_eq!(items[0].note.class, "review");

	// groups.toml is empty in this fixture, so approval is allowed with a warning.
	let approved = merge::review_approve(&cfg, &s.id, Some("caleb".to_string())).unwrap();
	assert!(approved.landed.is_some(), "{approved:?}");
	assert!(git::show_file(root, "main", &format!("docs/{id}.md")).unwrap().starts_with(b"+++"));
}

// (f) a bare remote: `kb merge` lands by a plain push, and the bare repo's `main` equals the
// local `main`, even when a second writer pushes to the bare remote after this session opened
// (the fetch/rebase/push loop must catch up and still land, retrying if the push races).
#[test]
fn bare_remote_lands_via_push_despite_a_concurrent_writer() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let bare_parent = tempfile::tempdir().unwrap();
	let bare = bare_parent.path().join("origin.git");
	git::run(bare_parent.path(), &["init", "--bare", "-q", "-b", "main", "origin.git"]).unwrap();
	git::run(root, &["remote", "add", "origin", bare.to_str().unwrap()]).unwrap();
	git::run(root, &["push", "-q", "origin", "main"]).unwrap();

	let mut s = session::open(&cfg, "add doc via bare remote", None, &[]).unwrap();
	write_doc(&s.worktree, "01K5A7Q2XW3ZRCM8T6YB4VJH1F", "doc f", "content");
	git::commit_all(&s.worktree, "wip: add doc f").unwrap();
	session::settle(&cfg, &mut s, None).unwrap();

	// a second clone of the bare remote, pushing a competing commit on a short delay to race
	// against this test's own call to `merge::merge` below.
	let clone_parent = tempfile::tempdir().unwrap();
	let clone_path = clone_parent.path().join("clone");
	git::run(clone_parent.path(), &["clone", "-q", bare.to_str().unwrap(), "clone"]).unwrap();
	git::run(&clone_path, &["config", "user.name", "second writer"]).unwrap();
	git::run(&clone_path, &["config", "user.email", "second@example.com"]).unwrap();
	let other_id = "01K5A7Q2XW3ZRCM8T6YB4VJH1G";
	let handle = {
		let clone_path = clone_path.clone();
		std::thread::spawn(move || {
			std::thread::sleep(std::time::Duration::from_millis(30));
			write_doc(&clone_path, other_id, "doc g", "content");
			git::commit_all(&clone_path, "concurrent doc g").unwrap();
			git::run(&clone_path, &["push", "-q", "origin", "main"]).unwrap();
		})
	};

	let outcome = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap();
	handle.join().unwrap();

	assert!(outcome.landed.is_some(), "{outcome:?}");
	let local_main = git::head(root).unwrap();
	let bare_main = git::run(&bare, &["rev-parse", "main"]).unwrap().trim().to_string();
	assert_eq!(local_main, bare_main, "the bare remote's main must equal the local main");
	assert!(git::show_file(root, "main", "docs/01K5A7Q2XW3ZRCM8T6YB4VJH1F.md").is_ok());
	assert!(git::show_file(root, "main", &format!("docs/{other_id}.md")).is_ok());
}

// (g) `kb revert` of a merged session produces a new forward commit; main's history grows and
// never shrinks (no reset, no force-push).
#[test]
fn revert_produces_a_new_forward_commit() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let mut s = session::open(&cfg, "revise root doc", None, &[]).unwrap();
	std::fs::write(s.worktree.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC.replace("original text.", "revised text.")).unwrap();
	git::commit_all(&s.worktree, "wip: revise").unwrap();
	session::settle(&cfg, &mut s, None).unwrap();
	let outcome = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap();
	assert!(outcome.landed.is_some(), "{outcome:?}");
	assert!(String::from_utf8(git::show_file(root, "main", &format!("docs/{ID_ROOT}.md")).unwrap()).unwrap().contains("revised text."));

	let count_before = git::rev_list_count(root, "main").unwrap();

	let revert_outcome = merge::history::revert(&cfg, &s.id).unwrap();
	assert!(revert_outcome.landed.is_some(), "{revert_outcome:?}");

	let count_after = git::rev_list_count(root, "main").unwrap();
	assert!(count_after > count_before, "main's history must grow, never shrink");
	assert!(String::from_utf8(git::show_file(root, "main", &format!("docs/{ID_ROOT}.md")).unwrap()).unwrap().contains("original text."));
}

// (h) `kb doctor` reports clean after a normal land, and reports a non-fast-forward move when
// the test resets `main` by hand (simulating raw git bypassing `kb`).
#[test]
fn doctor_reports_clean_then_non_fast_forward() {
	let dir = seed_repo();
	let root = dir.path();
	let cfg = Config::load(root).unwrap();

	let mut s = session::open(&cfg, "add doc for doctor", None, &[]).unwrap();
	write_doc(&s.worktree, "01K5A7Q2XW3ZRCM8T6YB4VJH1H", "doc h", "content");
	git::commit_all(&s.worktree, "wip: add doc h").unwrap();
	session::settle(&cfg, &mut s, None).unwrap();
	let outcome = merge::merge(&cfg, &s.id, MergeOpts::default()).unwrap();
	let landed = outcome.landed.clone().unwrap();

	let report = merge::history::doctor(&cfg).unwrap();
	assert_eq!(report, vec!["clean".to_string()], "{report:?}");

	// reset main by hand to its grandparent-of-none, i.e. its own parent: not an ancestor
	// relationship in the forward direction, so this is a non-fast-forward move.
	let parent = git::run(root, &["rev-parse", &format!("{landed}~1")]).unwrap().trim().to_string();
	git::run(root, &["update-ref", "refs/heads/main", &parent]).unwrap();

	let report2 = merge::history::doctor(&cfg).unwrap();
	assert!(report2.iter().any(|l| l.contains("non-fast-forward")), "{report2:?}");
}
