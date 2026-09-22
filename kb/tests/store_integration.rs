//! end-to-end test of the content store: init a repo, configure a `files` source, capture from
//! it, dedup and quarantine behavior across two runs, and export. contract: design.md §9, §20.

use kb::cli::init;
use kb::config::Config;
use kb::store::{catalog, export, sources};
use std::time::{Duration, SystemTime};

/// a syntactically plausible github token; long enough to trip the `github-pat` secret rule.
fn fake_github_token() -> String {
    format!("ghp_{}", "a".repeat(36))
}

fn setup() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    init::init(dir.path(), false, true).unwrap();

    let inbox = dir.path().join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(inbox.join("note.md"), "# A note\n\nsome captured text.\n").unwrap();
    std::fs::write(inbox.join("data.json"), "{\"b\": 2, \"a\": 1}").unwrap();
    std::fs::write(inbox.join("secret.txt"), format!("token = \"{}\"\n", fake_github_token())).unwrap();

    let kb_toml = dir.path().join("kb.toml");
    let mut existing = std::fs::read_to_string(&kb_toml).unwrap();
    existing.push_str("\n[sources.inbox]\nkind = \"files\"\npath = \"inbox\"\nglob = \"**/*\"\n");
    std::fs::write(&kb_toml, existing).unwrap();

    let cfg = Config::load(dir.path()).unwrap();
    (dir, cfg)
}

#[test]
fn capture_dedup_quarantine_and_export() {
    let (dir, cfg) = setup();

    // --- first run: three files pulled, two land as new captures, one is quarantined ---
    let report = sources::capture_all(&cfg, None).unwrap();
    assert_eq!(report.quarantined, 1, "the token file should be quarantined: {:?}", report.per_source);
    let inbox_new: usize = report.per_source.iter().filter(|(name, _)| name == "inbox").map(|(_, n)| *n).sum();
    assert_eq!(inbox_new, 2, "note.md and data.json should be new, non-quarantined captures");

    assert!(catalog::get_cursor(&cfg, "inbox").unwrap().is_some(), "cursor must advance after a pull");

    let quarantined_rows = catalog::list(&cfg, Some("quarantined"), None, 10).unwrap();
    assert_eq!(quarantined_rows.len(), 1);
    assert!(quarantined_rows[0].source_uri.contains("secret.txt"));

    // --- second run from the same directory: nothing new, nothing quarantined ---
    let report2 = sources::capture_all(&cfg, None).unwrap();
    let inbox_new2: usize = report2.per_source.iter().filter(|(name, _)| name == "inbox").map(|(_, n)| *n).sum();
    assert_eq!(inbox_new2, 0, "an unchanged directory must not produce new captures");
    assert_eq!(report2.quarantined, 0);

    // --- touch (modify content of) one file: exactly one new capture ---
    let note_path = dir.path().join("inbox/note.md");
    std::fs::write(&note_path, "# A note\n\nsome captured text, now revised.\n").unwrap();
    // force the mtime comfortably past the cursor recorded by the first run, so this update is
    // never lost to filesystem mtime resolution or same-second test execution.
    let f = std::fs::OpenOptions::new().write(true).open(&note_path).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();

    let report3 = sources::capture_all(&cfg, None).unwrap();
    let inbox_new3: usize = report3.per_source.iter().filter(|(name, _)| name == "inbox").map(|(_, n)| *n).sum();
    assert_eq!(inbox_new3, 1, "the modified file should be exactly one new capture");
    assert_eq!(report3.quarantined, 0);

    // --- export one of the new captures and check the header parses as toml ---
    let rows = catalog::list(&cfg, Some("new"), Some("inbox"), 10).unwrap();
    assert!(rows.len() >= 2, "expected at least the two original new captures, got {}", rows.len());
    let json_capture = rows.iter().find(|c| c.media_type == "application/json").expect("data.json capture");

    let out_path = dir.path().join("export.md");
    let written = export::export(&cfg, &json_capture.capture_id, false, Some(&out_path)).unwrap();
    assert_eq!(written, out_path);

    let text = std::fs::read_to_string(&out_path).unwrap();
    let doc = kb::parse::parse_document(&text).expect("exported header must parse as toml frontmatter");
    assert_eq!(doc.frontmatter.extra.get("source_id").and_then(|v| v.as_str()), Some(json_capture.capture_id.as_str()));
    assert_eq!(doc.frontmatter.extra.get("sha256_text").and_then(|v| v.as_str()), Some(json_capture.sha256_text.as_str()));
    // normalize()'s json pretty-print landed in the body, sorted by key (no `preserve_order`).
    assert!(doc.body.contains("\"a\": 1"));
    assert!(doc.body.contains("\"b\": 2"));
}
