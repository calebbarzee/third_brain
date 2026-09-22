//! integration tests for `kb::maintain`: design.md §15.3 (nightly curation) end to end on a temp
//! kb repo, driven the way `kb maintain nightly` would drive it.

use kb::config::Config;
use kb::git;
use kb::maintain::{self, FindingKind, NightlyOpts};
use std::collections::HashSet;

const STALE_ID: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";
const SUPERSEDED_ID: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0B";
const OVER_BUDGET_ID: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0C";
const DANGLING_TARGET: &str = "01K5A7Q2XW3ZRCM8T6YB4VJHZZ";

fn write_doc(root: &std::path::Path, id: &str, text: &str) {
    std::fs::write(root.join("docs").join(format!("{id}.md")), text).unwrap();
}

fn stale_doc() -> String {
    format!(
        "+++\nid = \"{STALE_ID}\"\nlayer = \"procedure\"\ntitle = \"old runbook\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nreview_by = 2020-01-01\n+++\n\n# old runbook\n\n## prerequisites\n\nnone.\n\n## rollback\n\nnone.\n"
    )
}

fn superseded_dangling_doc() -> String {
    format!(
        "+++\nid = \"{SUPERSEDED_ID}\"\nlayer = \"reference\"\ntitle = \"old note\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nsuperseded_by = \"kb:{DANGLING_TARGET}\"\n+++\n\n> superseded 2020-01-01 by [gone](kb:{DANGLING_TARGET}). kept for history; not current.\n\n# old note\n\ncontent.\n"
    )
}

fn over_budget_doc() -> String {
    let filler = "filler ".repeat(6_200);
    format!("+++\nid = \"{OVER_BUDGET_ID}\"\nlayer = \"reference\"\ntitle = \"giant reference\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# giant reference\n\n{filler}\n")
}

/// a backdated ulid: `days_ago` encoded into the id's timestamp bits, everything else random.
fn backdated_ulid(days_ago: i64) -> (String, chrono::DateTime<chrono::Utc>) {
    let when = chrono::Utc::now() - chrono::Duration::days(days_ago);
    let base = ulid::Ulid::from_string(&kb::model::new_ulid()).unwrap();
    let dated = ulid::Ulid::from_parts(when.timestamp_millis() as u64, base.random());
    (dated.to_string(), when)
}

fn draft_doc(id: &str) -> String {
    format!("+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"abandoned draft\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# abandoned draft\n\nsome notes, never finished.\n")
}

/// a repo with `kb::cli::init::init`'s skeleton, a fixed local identity, and the four fixture
/// documents committed on `main`: an old draft (its own backdated commit), then the stale
/// procedure, the superseded document with a dangling pointer, and the over-budget reference
/// (one normal commit). returns the draft's id alongside the temp dir.
fn seed_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    git::run(root, &["config", "user.name", "test user"]).unwrap();
    git::run(root, &["config", "user.email", "test@example.com"]).unwrap();

    let (draft_id, when) = backdated_ulid(40);
    write_doc(root, &draft_id, &draft_doc(&draft_id));
    git::commit_all_dated(root, "add abandoned draft", &when.to_rfc3339()).unwrap();

    write_doc(root, STALE_ID, &stale_doc());
    write_doc(root, SUPERSEDED_ID, &superseded_dangling_doc());
    write_doc(root, OVER_BUDGET_ID, &over_budget_doc());
    git::commit_all(root, "seed stale, superseded, and over-budget documents").unwrap();

    (dir, draft_id)
}

fn kinds(findings: &[maintain::Finding]) -> HashSet<FindingKind> {
    findings.iter().map(|f| f.kind).collect()
}

fn doc_exists_on_main(root: &std::path::Path, id: &str) -> bool {
    git::ls_tree(root, "main", "docs/").unwrap_or_default().iter().any(|p| p.ends_with(&format!("{id}.md")))
}

#[test]
fn dry_run_then_real_runs_land_escalate_and_checkpoint_when_clean() {
    let (dir, draft_id) = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    kb::index::build(&cfg, None).unwrap();

    // 1. dry run: every finding kind the fixtures are built for, nothing written.
    let dry = maintain::nightly(&cfg, NightlyOpts { dry_run: true, since: None, only: None }).unwrap();
    let found = kinds(&dry.findings);
    assert!(found.contains(&FindingKind::Stale), "{found:?}");
    assert!(found.contains(&FindingKind::DraftPastTtl), "{found:?}");
    assert!(found.contains(&FindingKind::OverBudget), "{found:?}");
    assert!(found.contains(&FindingKind::MissingProvenance), "{found:?}");
    assert!(dry.sessions.is_empty());
    assert!(dry.checkpoint.is_none());

    // 2. lower min_corpus so auto-fixes are actually proposed, and rebuild the index (same
    // commit, so `refresh_captures` -- `index::build` would just reuse the existing artifact).
    let manifest_text = kb::cli::init::MANIFEST_TOML.replace("min_corpus = 50", "min_corpus = 1");
    std::fs::write(root.join("manifest.toml"), manifest_text).unwrap();
    let cfg = Config::load(root).unwrap();
    kb::index::refresh_captures(&cfg, None).unwrap();

    // 3. first real run: the stale procedure auto-retires (lands, with a banner and pointer);
    // the over-budget and dangling-pointer findings, and the draft delete, all become
    // escalations, and nothing about them lands.
    let report1 = maintain::nightly(&cfg, NightlyOpts::default()).unwrap();
    assert!(report1.checkpoint.is_none(), "first pass should not be clean: {report1:?}");
    assert!(!report1.sessions.is_empty());

    let stale_bytes = git::show_file(root, "main", &format!("docs/{STALE_ID}.md")).unwrap();
    let stale_doc = kb::parse::parse_document(&String::from_utf8(stale_bytes).unwrap()).unwrap();
    assert_eq!(stale_doc.frontmatter.status, Some(kb::model::Status::Archived), "stale procedure should have been auto-retired");
    assert!(kb::check::has_banner(&stale_doc.body), "auto-retire should add the forward-pointer banner");

    assert!(
        report1.escalations.iter().any(|e| e.starts_with(FindingKind::OverBudget.as_str())),
        "over budget should escalate: {:?}",
        report1.escalations
    );
    assert!(
        report1.escalations.iter().any(|e| e.starts_with(FindingKind::MissingProvenance.as_str()) && e.contains("does not exist")),
        "the dangling pointer should escalate: {:?}",
        report1.escalations
    );
    assert!(
        report1.escalations.iter().any(|e| e.starts_with(FindingKind::DraftPastTtl.as_str())),
        "the stale draft's delete should escalate, never land: {:?}",
        report1.escalations
    );
    assert!(doc_exists_on_main(root, &draft_id), "the draft delete must not have landed automatically");
    assert!(doc_exists_on_main(root, OVER_BUDGET_ID));
    assert!(doc_exists_on_main(root, SUPERSEDED_ID));

    // resolve every open escalation on file (a human reviewing the queue).
    for (id, rec) in maintain::list_escalations(&cfg).unwrap() {
        if !rec.resolved {
            maintain::resolve_escalation(&cfg, &id).unwrap();
        }
    }

    // 4. fix the three outstanding problems by hand, as a human would after reading the
    // escalations, then land a fresh, genuinely new commit (so the index rebuilds normally).
    std::fs::write(
        root.join("docs").join(format!("{OVER_BUDGET_ID}.md")),
        format!("+++\nid = \"{OVER_BUDGET_ID}\"\nlayer = \"reference\"\ntitle = \"giant reference\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# giant reference\n\nshort now.\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("docs").join(format!("{SUPERSEDED_ID}.md")),
        format!("+++\nid = \"{SUPERSEDED_ID}\"\nlayer = \"reference\"\ntitle = \"old note\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nsuperseded_by = \"none\"\n+++\n\n> superseded 2020-01-01. kept for history; not current.\n\n# old note\n\ncontent.\n"),
    )
    .unwrap();
    git::run(root, &["rm", "-q", &format!("docs/{draft_id}.md")]).unwrap();
    git::commit_all(root, "human: fix over-budget doc, fix dangling pointer, delete reviewed draft").unwrap();

    let cfg = Config::load(root).unwrap();
    kb::index::build(&cfg, None).unwrap();

    // 5. second real run: nothing left to find, so the pass is clean and gets a checkpoint tag.
    let report2 = maintain::nightly(&cfg, NightlyOpts::default()).unwrap();
    assert!(report2.checkpoint.is_some(), "a clean pass should tag a checkpoint: {report2:?}");
    let tag = report2.checkpoint.unwrap();
    assert!(tag.starts_with("checkpoint/"), "{tag}");
    assert!(git::tag_exists(root, &tag));
}
