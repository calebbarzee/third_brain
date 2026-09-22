//! integration tests for `kb import` (design.md §6, §9, §12): copying/moving markdown notes
//! into a knowledge base, filling frontmatter, rewriting wiki and relative markdown links to
//! `kb:` uris, and settling + merging the result. temp repos follow the same pattern as the
//! other integration suites: `kb::cli::init::init` plus a fixed local git identity.

use kb::config::Config;
use kb::git;
use kb::import::{self, ImportOptions};
use kb::model::is_ulid;

/// a kb repository skeleton with a fixed local identity and one seed commit on `main`.
fn seed_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    git::run(root, &["config", "user.name", "test user"]).unwrap();
    git::run(root, &["config", "user.email", "test@example.com"]).unwrap();
    git::commit_all(root, "seed").unwrap();
    dir
}

/// five source notes outside any git repository: two carry an obsidian-style `---` yaml header,
/// one wiki-links another (`[[meeting-notes]]`), one relative-markdown-links another
/// (`[Meeting Notes](meeting-notes.md)`, alongside a link nothing resolves), and one has no
/// links at all.
fn write_source_notes(dir: &std::path::Path) {
    std::fs::write(
        dir.join("project-overview.md"),
        "---\ntitle: Project Overview\ntags: [proj]\n---\n# Project Overview\n\nSee [[meeting-notes]] for the agenda.\n",
    )
    .unwrap();
    std::fs::write(dir.join("meeting-notes.md"), "---\ntitle: Meeting Notes\n---\n# Meeting Notes\n\nDiscussed the roadmap.\n").unwrap();
    std::fs::write(
        dir.join("related-doc.md"),
        "# Related Doc\n\nFollow up: [Meeting Notes](meeting-notes.md).\n\nAlso see [[Nonexistent Note]] which nobody wrote yet.\n",
    )
    .unwrap();
    std::fs::write(dir.join("standalone.md"), "# Standalone\n\nJust some plain notes with no links anywhere.\n").unwrap();
    std::fs::write(dir.join("old-ref.md"), "# Old Reference\n\nHistorical context, kept for the record.\n").unwrap();
}

#[test]
fn import_copies_renames_rewrites_links_and_merges() {
    let kb_dir = seed_repo();
    let root = kb_dir.path();
    let cfg = Config::load(root).unwrap();
    let main_before = git::head(root).unwrap();

    let source = tempfile::tempdir().unwrap();
    write_source_notes(source.path());
    let originals: Vec<(String, String)> = ["project-overview.md", "meeting-notes.md", "related-doc.md", "standalone.md", "old-ref.md"]
        .iter()
        .map(|name| (name.to_string(), std::fs::read_to_string(source.path().join(name)).unwrap()))
        .collect();

    let opts = ImportOptions { paths: vec![source.path().to_path_buf()], ..Default::default() };
    let report = import::import(&cfg, &opts).unwrap();

    assert_eq!(report.files_found, 5);
    assert_eq!(report.docs.len(), 5);
    assert_eq!(report.links_rewritten, 2, "wiki link + markdown link should both resolve: {report:#?}");
    assert_eq!(report.links_unresolved, 1, "the [[Nonexistent Note]] link should be left alone and counted: {report:#?}");

    // main gained exactly one commit, and it landed (auto class: fresh reference-layer documents).
    let main_after = git::head(root).unwrap();
    assert_ne!(main_after, main_before);
    assert_eq!(git::rev_list_count(root, &format!("{main_before}..{main_after}")).unwrap(), 1);
    assert!(report.merged_commit.is_some(), "{report:#?}");

    // docs/ holds exactly the 5 imported files, each named by its ulid.
    let doc_files: Vec<_> = std::fs::read_dir(root.join("docs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();
    assert_eq!(doc_files.len(), 5);
    for entry in &doc_files {
        let stem = entry.path().file_stem().unwrap().to_str().unwrap().to_string();
        assert!(is_ulid(&stem), "{stem} is not a ulid");
    }

    // originals are untouched: same path, same bytes.
    for (name, text) in &originals {
        assert_eq!(&std::fs::read_to_string(source.path().join(name)).unwrap(), text, "{name} was modified in place");
    }

    // the ulid map file exists and lists every document.
    let session_id = report.session_id.as_deref().unwrap();
    let map_path = cfg.state_dir().join("imports").join(format!("{session_id}.toml"));
    assert_eq!(report.map_path.as_deref(), Some(map_path.as_path()));
    let map_text = std::fs::read_to_string(&map_path).unwrap();
    let map_value: toml::Value = toml::from_str(&map_text).unwrap();
    assert_eq!(map_value["docs"].as_array().unwrap().len(), 5);

    // the wiki link and the markdown link both resolved to meeting-notes' new id.
    let overview = report.docs.iter().find(|d| d.source_path == "project-overview.md").unwrap();
    let meeting = report.docs.iter().find(|d| d.source_path == "meeting-notes.md").unwrap();
    let related = report.docs.iter().find(|d| d.source_path == "related-doc.md").unwrap();

    let overview_text = std::fs::read_to_string(root.join("docs").join(format!("{}.md", overview.id))).unwrap();
    assert!(overview_text.contains(&format!("(kb:{})", meeting.id)), "{overview_text}");
    let related_text = std::fs::read_to_string(root.join("docs").join(format!("{}.md", related.id))).unwrap();
    assert!(related_text.contains(&format!("(kb:{})", meeting.id)), "{related_text}");
    assert!(related_text.contains("[[Nonexistent Note]]"), "unresolved wiki link should be left as-is: {related_text}");

    // source_id, the source ref, and the aliases array are all present on the migrated document.
    let overview_doc = kb::parse::parse_document(&overview_text).unwrap();
    let fm = &overview_doc.frontmatter;
    assert_eq!(fm.extra.get("source_id").and_then(|v| v.as_str()), Some("project-overview.md"));
    assert!(fm.refs.iter().any(|r| r.rel == "source" && r.uri.starts_with("file:") && r.sha256.is_some()));
    let aliases = fm.extra.get("aliases").and_then(|v| v.as_array()).unwrap();
    assert!(aliases.iter().any(|v| v.as_str() == Some("project-overview")), "{aliases:?}");
}

#[test]
fn move_within_the_same_repository_uses_git_mv_and_keeps_history() {
    let kb_dir = seed_repo();
    let root = kb_dir.path();

    // a body long enough that git's default rename-similarity threshold (50%) is comfortably
    // cleared once `check::validate_file` prepends a `+++...+++` frontmatter block: `--follow`
    // below only finds the pre-import commit if git treats old and new paths as one rename.
    let body = "# A staged note\n\n".to_string()
        + &"written before it was imported, with enough body text that the frontmatter block \
           `validate --fix` adds stays a small fraction of the file, so git's rename detector \
           still treats this as the same file across the import.\n\n"
            .repeat(3);
    std::fs::create_dir_all(root.join("staging")).unwrap();
    std::fs::write(root.join("staging/note.md"), &body).unwrap();
    git::commit_all(root, "add a staging note").unwrap();

    let cfg = Config::load(root).unwrap();
    let opts = ImportOptions { paths: vec![root.join("staging")], move_files: true, ..Default::default() };
    let report = import::import(&cfg, &opts).unwrap();

    assert_eq!(report.docs.len(), 1);
    assert!(report.docs[0].moved, "the source was tracked in this same repo; --move should git mv it");
    let id = &report.docs[0].id;

    assert!(!root.join("staging/note.md").exists(), "the original should be gone after a same-repo move");
    let new_path = format!("docs/{id}.md");
    assert!(root.join(&new_path).is_file());

    let history = git::log_grep(root, "HEAD", "add a staging note").unwrap();
    assert!(!history.is_empty(), "the seeding commit should still be reachable from main");
    let subjects = git::run(root, &["log", "--follow", "--format=%s", "--", &new_path]).unwrap();
    assert!(subjects.contains("add a staging note"), "git log --follow should show the pre-import history:\n{subjects}");
}

#[test]
fn dry_run_writes_nothing() {
    let kb_dir = seed_repo();
    let root = kb_dir.path();
    let cfg = Config::load(root).unwrap();
    let main_before = git::head(root).unwrap();

    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a.md"), "# A\n\nbody\n").unwrap();
    std::fs::write(source.path().join("b.md"), "# B\n\nbody\n").unwrap();

    let opts = ImportOptions { paths: vec![source.path().to_path_buf()], dry_run: true, ..Default::default() };
    let report = import::import(&cfg, &opts).unwrap();

    assert!(report.dry_run);
    assert_eq!(report.files_found, 2);
    assert!(report.session_id.is_none());
    assert!(report.merged_commit.is_none());
    assert!(report.map_path.is_none());

    assert_eq!(git::head(root).unwrap(), main_before, "dry-run must not touch main");
    assert!(!cfg.state_dir().join("imports").is_dir(), "dry-run must not write the ulid map");
    assert!(!cfg.state_dir().join("sessions").is_dir(), "dry-run must not open a session");
    let doc_count = std::fs::read_dir(root.join("docs")).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|ext| ext == "md")).count();
    assert_eq!(doc_count, 0, "dry-run must not write into docs/");
}
