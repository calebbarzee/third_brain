//! captures in the index: build, default-search exclusion, include_captures, get, refs incoming,
//! capture: ref drift, canon_count, and --refresh-captures. design.md §9, §8, §17, §18.
use kb::config::Config;
use kb::git;
use kb::index;
use kb::store::catalog::{self, PutCapture};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

const ID_OK: &str = "01HZZZZZZZ0000000000001AAA";
const ID_BAD: &str = "01HZZZZZZZ0000000000001BBB";
const ID_MISSING_REF: &str = "01HZZZZZZZ0000000000001CCC";
const FAKE_CAPTURE_ID: &str = "01HZZZZZZZ0000000000009ZZZ";
const UNIQUE_WORD: &str = "wobblefrobnicator";

fn write_doc(root: &Path, id: &str, frontmatter_extra: &str, body: &str) {
    let text = format!("+++\nid = \"{id}\"\n{frontmatter_extra}\n+++\n\n{body}\n");
    std::fs::write(root.join("docs").join(format!("{id}.md")), text).unwrap();
}

/// a temp kb repository (via `kb::cli::init::init`) with two captures in the catalog and three
/// canon documents: one with a correct `source` ref to capture a, one with a stale-hash `source`
/// ref to capture b, and one with a `source` ref to a capture id that does not exist.
fn setup() -> (tempfile::TempDir, Config, catalog::Capture, catalog::Capture) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    git::run(root, &["config", "user.name", "Test User"]).unwrap();
    git::run(root, &["config", "user.email", "test@example.com"]).unwrap();

    let cfg = Config::load(root).unwrap();

    let capture_a = catalog::put_capture(
        &cfg,
        PutCapture {
            source: "manual".into(),
            source_uri: "file:/a.txt".into(),
            source_cursor: None,
            media_type: "text/plain".into(),
            title: Some("Capture A".into()),
            original: format!("original bytes mentioning {UNIQUE_WORD}").into_bytes(),
            text: format!("normalized text mentioning {UNIQUE_WORD} only here"),
            captured_at: None,
        },
    )
    .unwrap();
    let capture_b = catalog::put_capture(
        &cfg,
        PutCapture {
            source: "manual".into(),
            source_uri: "file:/b.txt".into(),
            source_cursor: None,
            media_type: "text/plain".into(),
            title: None,
            original: b"other original bytes".to_vec(),
            text: "other normalized text, unrelated content".into(),
            captured_at: None,
        },
    )
    .unwrap();

    write_doc(
        root,
        ID_OK,
        &format!(
            "layer = \"reference\"\ntitle = \"Doc OK\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n\n\
             [[refs]]\nrel = \"source\"\nuri = \"capture:{}\"\nsha256 = \"{}\"",
            capture_a.capture_id, capture_a.sha256_text
        ),
        "# Doc OK\n\nThis document cites capture a correctly.\n",
    );
    write_doc(
        root,
        ID_BAD,
        &format!(
            "layer = \"reference\"\ntitle = \"Doc Bad Hash\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n\n\
             [[refs]]\nrel = \"source\"\nuri = \"capture:{}\"\nsha256 = \"deadbeef\"",
            capture_b.capture_id
        ),
        "# Doc Bad Hash\n\nThis document's recorded hash for capture b is stale.\n",
    );
    write_doc(
        root,
        ID_MISSING_REF,
        &format!(
            "layer = \"reference\"\ntitle = \"Doc Missing Ref\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n\n\
             [[refs]]\nrel = \"source\"\nuri = \"capture:{FAKE_CAPTURE_ID}\""
        ),
        "# Doc Missing Ref\n\nThis document points at a capture that was never captured.\n",
    );

    git::commit_all(root, "seed three canon documents, two with capture source refs").unwrap();
    (dir, cfg, capture_a, capture_b)
}

#[test]
fn captures_are_indexed_but_excluded_from_default_search() {
    let (dir, cfg, capture_a, capture_b) = setup();
    let _ = dir;

    let report = index::build(&cfg, None).unwrap();
    assert_eq!(report.document_count, 3, "the three real documents, not the captures");
    assert_eq!(report.canon_count, 3, "canon_count counts only real documents; captures are never canon");
    assert_eq!(report.capture_count, 2, "both non-quarantined captures were indexed");

    let conn = index::open_newest(&cfg).unwrap();

    // default search never returns a capture, even for a word that exists only in capture text.
    let default_hits = index::search(&conn, UNIQUE_WORD, &index::SearchOpts::default()).unwrap();
    assert!(default_hits.is_empty(), "default search must not surface captures: {default_hits:?}");

    // include_captures widens the candidate set and finds it.
    let opts = index::SearchOpts { include_captures: true, ..Default::default() };
    let wide_hits = index::search(&conn, UNIQUE_WORD, &opts).unwrap();
    assert!(wide_hits.iter().any(|h| h.doc_id == capture_a.capture_id && h.kind == "capture"), "expected capture a in {wide_hits:?}");
    let hit = wide_hits.iter().find(|h| h.doc_id == capture_a.capture_id).unwrap();
    assert_eq!(hit.source_uri.as_deref(), Some("file:/a.txt"));

    // get() on a capture id returns the capture row with body and source fields.
    let rec = index::get(&conn, &capture_a.capture_id).unwrap().expect("capture resolves through get()");
    assert_eq!(rec.kind, "capture");
    assert!(rec.body.contains(UNIQUE_WORD));
    let cap_fields = rec.capture.expect("capture-specific fields present");
    assert_eq!(cap_fields.source_uri.as_deref(), Some("file:/a.txt"));
    assert_eq!(cap_fields.adapter.as_deref(), Some("manual"));

    // a document is not mistaken for a capture: no `capture` fields.
    let doc_rec = index::get(&conn, ID_OK).unwrap().unwrap();
    assert_eq!(doc_rec.kind, "document");
    assert!(doc_rec.capture.is_none());

    // refs incoming on a capture lists documents whose source refs point at it.
    let incoming = index::refs(&conn, &capture_a.capture_id, true, 1).unwrap();
    assert!(incoming.iter().any(|r| r.from_id == ID_OK && r.rel == "source"), "expected {ID_OK} incoming, got {incoming:?}");

    // drift: a correct hash is ok, a stale one is hash_changed, a nonexistent capture is missing.
    let ok_refs = index::refs(&conn, ID_OK, false, 1).unwrap();
    let ok_ref = ok_refs.iter().find(|r| r.uri == format!("capture:{}", capture_a.capture_id)).unwrap();
    assert_eq!(ok_ref.drift_state.as_deref(), Some("ok"));

    let bad_refs = index::refs(&conn, ID_BAD, false, 1).unwrap();
    let bad_ref = bad_refs.iter().find(|r| r.uri == format!("capture:{}", capture_b.capture_id)).unwrap();
    assert_eq!(bad_ref.drift_state.as_deref(), Some("hash_changed"));

    let missing_refs = index::refs(&conn, ID_MISSING_REF, false, 1).unwrap();
    let missing_ref = missing_refs.iter().find(|r| r.uri == format!("capture:{FAKE_CAPTURE_ID}")).unwrap();
    assert_eq!(missing_ref.drift_state.as_deref(), Some("missing"));
    assert!(missing_ref.resolved_id.is_none());

    // ls defaults to documents only; --kind capture opts in.
    let default_ls = index::ls(&conn, &index::LsFilter::default()).unwrap();
    assert_eq!(default_ls.len(), 3, "ls defaults to kind = document");
    let capture_ls = index::ls(&conn, &index::LsFilter { kind: Some("capture".to_string()), ..Default::default() }).unwrap();
    assert_eq!(capture_ls.len(), 2);
}

#[test]
fn refresh_captures_picks_up_a_capture_without_a_new_commit() {
    let (dir, cfg, _capture_a, _capture_b) = setup();
    let _ = dir;

    let first = index::build(&cfg, None).unwrap();
    assert_eq!(first.capture_count, 2);

    // a third capture lands with no new main commit.
    catalog::put_capture(
        &cfg,
        PutCapture {
            source: "manual".into(),
            source_uri: "file:/c.txt".into(),
            source_cursor: None,
            media_type: "text/plain".into(),
            title: Some("Capture C".into()),
            original: b"third capture original".to_vec(),
            text: "third capture normalized text".into(),
            captured_at: None,
        },
    )
    .unwrap();

    // a plain rebuild of the same commit is a no-op: the capture is not picked up.
    let unchanged = index::build(&cfg, None).unwrap();
    assert!(unchanged.already_built);
    assert_eq!(unchanged.capture_count, 2, "plain `kb index build` does not rebuild an existing commit's artifact");

    // --refresh-captures forces a rebuild into a new `<commit>-<n>.sqlite` file.
    let refreshed = index::refresh_captures(&cfg, None).unwrap();
    assert!(!refreshed.already_built);
    assert_eq!(refreshed.commit, first.commit);
    assert_eq!(refreshed.capture_count, 3, "the refreshed artifact contains all three captures");
    assert_ne!(refreshed.artifact_path, first.artifact_path);
    assert_eq!(refreshed.artifact_path.file_name().unwrap().to_str().unwrap(), format!("{}-1.sqlite", first.commit));

    // the original artifact is untouched (index artifacts are immutable per commit).
    let old_conn = Connection::open_with_flags(&first.artifact_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let old_captures: i64 = old_conn.query_row("select count(*) from documents where kind = 'capture'", [], |r| r.get(0)).unwrap();
    assert_eq!(old_captures, 2);

    // newest_artifact sorts by mtime, so the refreshed build is now current.
    let newest = index::newest_artifact(&cfg).unwrap();
    assert_eq!(newest, refreshed.artifact_path);
    let conn = index::open_newest(&cfg).unwrap();
    let capture_ls = index::ls(&conn, &index::LsFilter { kind: Some("capture".to_string()), ..Default::default() }).unwrap();
    assert_eq!(capture_ls.len(), 3);
}
