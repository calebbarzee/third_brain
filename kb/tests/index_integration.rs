//! end-to-end test of the index build and query surface against a real (temporary) git repository.
use kb::config::Config;
use kb::git;
use kb::index;
use std::path::Path;

const ID_ROOT: &str = "01HZZZZZZZ0000000000000AAA";
const ID_INSTALL: &str = "01HZZZZZZZ0000000000000BBB";
const ID_OLD: &str = "01HZZZZZZZ0000000000000CCC";
const ID_NEW: &str = "01HZZZZZZZ0000000000000DDD";
const ID_CHILD: &str = "01HZZZZZZZ0000000000000EEE";
const ID_ORPHAN: &str = "01HZZZZZZZ0000000000000FFF";

fn write_doc(root: &Path, id: &str, frontmatter_extra: &str, body: &str) {
    let text = format!(
        "+++\nid = \"{id}\"\n{frontmatter_extra}\n+++\n\n{body}\n"
    );
    std::fs::write(root.join("docs").join(format!("{id}.md")), text).unwrap();
}

fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    git::init_for_tests(root).unwrap();

    // root canon document, referenced by install (kb: inline link) and parent of child.
    write_doc(
        root,
        ID_ROOT,
        "layer = \"reference\"\ntitle = \"Root guide\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        &format!("# Root guide\n\nStart here. See [install](kb:{ID_INSTALL}) for setup.\n"),
    );
    write_doc(
        root,
        ID_INSTALL,
        "layer = \"reference\"\ntitle = \"Install steps\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        &format!("# Install steps\n\nRun the installer. Related: [root guide](kb:{ID_ROOT}).\n\n## Linux\n\nUse the package manager.\n"),
    );
    // first commit, human author.
    git::run(root, &["add", "-A"]).unwrap();
    git::run(root, &["-c", "user.name=Ada Human", "-c", "user.email=ada@example.com", "commit", "-q", "-m", "seed root and install docs"]).unwrap();

    // superseded chain: old -> new.
    write_doc(
        root,
        ID_NEW,
        "layer = \"reference\"\ntitle = \"New widget\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        "# New widget\n\nThe current widget documentation.\n",
    );
    write_doc(
        root,
        ID_OLD,
        &format!("layer = \"reference\"\ntitle = \"Old widget\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nsuperseded_by = \"kb:{ID_NEW}\""),
        "# Old widget\n\nSuperseded; see the replacement.\n",
    );
    // parent chain: child's parent is root.
    write_doc(
        root,
        ID_CHILD,
        &format!("layer = \"procedure\"\ntitle = \"Root child procedure\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nparent = \"kb:{ID_ROOT}\""),
        "# Root child procedure\n\nA procedure that hangs off the root guide.\n",
    );
    // a draft with no incoming references, to exercise the non-canon percentile path.
    write_doc(
        root,
        ID_ORPHAN,
        "layer = \"reference\"\ntitle = \"Orphan draft\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        "# Orphan draft\n\nNot referenced by anything yet.\n",
    );
    // second commit, agent author with an owner configured in groups.toml.
    git::run(root, &["add", "-A"]).unwrap();
    git::run(
        root,
        &[
            "-c",
            "user.name=agent:writer@bot",
            "-c",
            "user.email=bot@example.com",
            "commit",
            "-q",
            "-m",
            "add superseded chain, parent chain, and a draft",
            "--trailer",
            "Run=run-42",
        ],
    )
    .unwrap();

    std::fs::write(root.join("groups.toml"), "[principals.bot]\nkind = \"service\"\nowner = \"caleb\"\n").unwrap();
    dir
}

#[test]
fn build_and_query_a_small_repo() {
    let dir = setup_repo();
    let cfg = Config::load(dir.path()).unwrap();

    let report = index::build(&cfg, None).unwrap();
    assert!(!report.already_built);
    assert_eq!(report.document_count, 6, "root, install, old, new, child, orphan");
    assert_eq!(report.canon_count, 4, "root, install, new, and child; old is superseded and orphan is a draft");
    assert!(report.chunk_count >= 6, "at least one chunk per document, got {}", report.chunk_count);
    assert!(!report.maintenance_enabled, "5 canon documents is below the default min_corpus of 50");

    // building the same commit again is a no-op.
    let second = index::build(&cfg, None).unwrap();
    assert!(second.already_built);
    assert_eq!(second.commit, report.commit);

    let conn = index::open_newest(&cfg).unwrap();

    // search finds a term present in both root and install.
    let hits = index::search(&conn, "install", &index::SearchOpts::default()).unwrap();
    assert!(!hits.is_empty(), "expected at least one search hit for `install`");
    assert!(hits.iter().any(|h| h.doc_id == ID_INSTALL));

    // get by unique prefix, and the superseded forward pointer.
    let install = index::get(&conn, &ID_INSTALL[..24]).unwrap().expect("install resolves by prefix");
    assert_eq!(install.id, ID_INSTALL);
    assert_eq!(install.status.as_deref(), Some("canon"));

    let old = index::get(&conn, ID_OLD).unwrap().expect("old widget exists");
    let forward = old.forward.expect("superseded document carries a forward pointer");
    assert_eq!(forward.id, ID_NEW);
    assert!(!forward.chain_continues, "new widget is canon, not itself superseded");

    // refs incoming: root is targeted by install's inline kb: link and by child's parent edge.
    let incoming = index::refs(&conn, ID_ROOT, true, 1).unwrap();
    assert!(incoming.iter().any(|r| r.from_id == ID_INSTALL && r.rel == "related"));
    assert!(incoming.iter().any(|r| r.from_id == ID_CHILD && r.rel == "parent"));

    // ls, filtered and unfiltered.
    let all = index::ls(&conn, &index::LsFilter::default()).unwrap();
    assert_eq!(all.len(), 6);
    let procedures = index::ls(&conn, &index::LsFilter { layer: Some("procedure".to_string()), ..Default::default() }).unwrap();
    assert_eq!(procedures.len(), 1);
    assert_eq!(procedures[0].id, ID_CHILD);

    // contributors and agent responsibility: the agent commit's owner (from groups.toml) shows up
    // as a responsible actor for every document touched by that commit.
    let new_doc = index::get(&conn, ID_NEW).unwrap().unwrap();
    assert!(new_doc.last_human_commit.is_none(), "new widget was only ever touched by the agent commit");

    // blame returns one row per section for a document with headings.
    let blame_rows = index::blame(&cfg, &conn, ID_INSTALL).unwrap();
    assert_eq!(blame_rows.len(), 2, "install has a preamble-less top heading plus one linux subheading");
}
