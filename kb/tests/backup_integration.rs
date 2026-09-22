//! integration tests for `kb::backup` and `kb::maintain::compact`: design.md §21.

use kb::backup::{self, BackupKind};
use kb::config::Config;
use kb::git;
use kb::maintain;

fn seed_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    git::run(root, &["config", "user.name", "test user"]).unwrap();
    git::run(root, &["config", "user.email", "test@example.com"]).unwrap();
    std::fs::write(
        root.join("docs").join("01K5A7Q2XW3ZRCM8T6YB4VJH0A.md"),
        "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0A\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# t\n\ncontent.\n",
    )
    .unwrap();
    git::commit_all(root, "seed one document").unwrap();
    dir
}

fn write_backup_toml(root: &std::path::Path, mirror: &std::path::Path, bundle_dir: &std::path::Path) {
    std::fs::write(
        root.join("backup.toml"),
        format!("mirror = \"{}\"\nbundle_dir = \"{}\"\n", mirror.display(), bundle_dir.display()),
    )
    .unwrap();
}

#[test]
fn run_all_mirrors_bundles_and_verifies() {
    let dir = seed_repo();
    let root = dir.path();
    let scratch = tempfile::tempdir().unwrap();
    let mirror_path = scratch.path().join("mirror.git");
    let bundle_dir = scratch.path().join("bundles");
    write_backup_toml(root, &mirror_path, &bundle_dir);

    let cfg = Config::load(root).unwrap();
    let head = git::rev_parse(root, "main").unwrap();

    let report = backup::run(&cfg, BackupKind::All).unwrap();
    assert_eq!(report.mirrored.as_deref(), Some(head.as_str()));
    let bundle_path = report.bundle_path.clone().expect("a bundle should have been created");
    assert!(bundle_path.is_file());
    // no `store_copy` is configured, so that (and only that) warning is expected.
    assert_eq!(report.warnings, vec!["no store_copy configured in backup.toml".to_string()]);

    // the mirror's main equals the repo's main.
    let mirror_main = git::rev_parse(&mirror_path, "main").unwrap();
    assert_eq!(mirror_main, head);

    // verify passes on the newest bundle.
    let lines = backup::verify(&cfg).unwrap();
    assert!(!lines.is_empty());

    // the manifest shows both commits.
    let manifest = backup::manifest(&cfg).unwrap();
    assert_eq!(manifest.last_mirrored.as_deref(), Some(head.as_str()));
    assert_eq!(manifest.last_bundled.as_deref(), Some(head.as_str()));

    // a restore drill into a fresh directory reports no differences.
    let restore_into = scratch.path().join("restore");
    let diffs = backup::restore_drill(&cfg, &restore_into).unwrap();
    assert!(diffs.is_empty(), "{diffs:?}");
    assert_eq!(git::rev_parse(&restore_into, "main").unwrap(), head);
}

#[test]
fn delete_allowed_by_backup_reflects_the_manifest() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();

    // no backup.toml at all: allowed, with a warning.
    let (ok, detail) = backup::delete_allowed_by_backup(&cfg, "irrelevant-sha").unwrap();
    assert!(ok);
    assert!(detail.contains("no backup configured"), "{detail}");

    let scratch = tempfile::tempdir().unwrap();
    let mirror_path = scratch.path().join("mirror.git");
    let bundle_dir = scratch.path().join("bundles");
    write_backup_toml(root, &mirror_path, &bundle_dir);
    let cfg = Config::load(root).unwrap();
    let head = git::rev_parse(root, "main").unwrap();
    backup::run(&cfg, BackupKind::All).unwrap();

    let (ok, _) = backup::delete_allowed_by_backup(&cfg, &head).unwrap();
    assert!(ok, "the head commit is exactly what was just mirrored and bundled");

    // a later, never-backed-up commit is not yet safe to delete a file last touched at.
    std::fs::write(root.join("docs").join("01K5A7Q2XW3ZRCM8T6YB4VJH0B.md"), "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0B\"\nlayer = \"reference\"\ntitle = \"t2\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# t2\n\nmore.\n").unwrap();
    let later = git::commit_all(root, "a later commit never backed up").unwrap();
    let (ok, detail) = backup::delete_allowed_by_backup(&cfg, &later).unwrap();
    assert!(!ok, "{detail}");
}

#[test]
fn compact_requires_a_backed_up_checkpoint_and_prunes_a_merged_session_branch() {
    let dir = seed_repo();
    let root = dir.path();

    // no checkpoint tag yet: compact refuses.
    let cfg = Config::load(root).unwrap();
    let report = maintain::compact(&cfg, maintain::CompactOpts::default()).unwrap();
    assert!(report.skipped.is_some(), "{report:?}");

    // tag a checkpoint, then back it up so compact's precondition is satisfied.
    let head = git::rev_parse(root, "main").unwrap();
    git::tag_annotated(root, "checkpoint/2026-01-01", &head, "test checkpoint").unwrap();
    let scratch = tempfile::tempdir().unwrap();
    write_backup_toml(root, &scratch.path().join("mirror.git"), &scratch.path().join("bundles"));
    let cfg = Config::load(root).unwrap();
    backup::run(&cfg, BackupKind::All).unwrap();

    // a "merged" session branch with no session file at all: just points at main, as a stray
    // branch an interrupted cleanup would have left behind.
    git::run(root, &["branch", "session/01K5A7Q2XW3ZRCM8T6YB4VJHFF", "main"]).unwrap();

    let history_before = git::rev_list_count(root, "main").unwrap();
    let main_before = git::rev_parse(root, "main").unwrap();

    let report = maintain::compact(&cfg, maintain::CompactOpts::default()).unwrap();
    assert!(report.skipped.is_none(), "{report:?}");
    assert!(report.deleted_session_branches.iter().any(|b| b == "session/01K5A7Q2XW3ZRCM8T6YB4VJHFF"), "{report:?}");
    assert!(git::list_refs(root, "refs/heads/session/*").unwrap().is_empty(), "the merged session branch should be gone");

    let history_after = git::rev_list_count(root, "main").unwrap();
    let main_after = git::rev_parse(root, "main").unwrap();
    assert_eq!(history_before, history_after, "compaction must never rewrite main's history");
    assert_eq!(main_before, main_after);
}
