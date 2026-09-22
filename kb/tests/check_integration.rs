//! integration test for the `check` module: a temp git repo, a couple of hand-written documents,
//! and two copies of real messy notes (one obsidian-style yaml header, one plain markdown with no
//! frontmatter at all), exercised through validate --fix, infer, gate, graph, and secrets.

use kb::check;
use kb::config::Config;
use kb::git;
use kb::model::{Layer, MergeClass, Operation, Severity};
use kb::parse::parse_document;
use std::path::Path;

const ID_ROOT: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";
const ID_DECISION: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0D";

const ROOT_DOC: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0A\"\nlayer = \"principle\"\ntitle = \"root principle\"\nstatus = \"canon\"\nagent_policy = \"locked\"\nowner = \"caleb\"\n+++\n\n# root principle\n\nfoundational stuff.\n";

const DECISION_DOC: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0D\"\nlayer = \"decision\"\ntitle = \"decision one\"\nstatus = \"canon\"\nagent_policy = \"append-only\"\nowner = \"caleb\"\nparent = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0A\"\n+++\n\n# decision\n\nwe decided x.\n\n# consequences\n\ny follows.\n";

const FIXTURES: &str = "/home/calebbarzee/1_projects/dev/third_brain/.tmp/fixtures/notes";

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(name)).expect("read fixture")
}

/// every `docs/*.md` file's id and frontmatter, for `check_graph`.
fn collect_docs(repo: &Path) -> Vec<(String, kb::model::Frontmatter)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(repo.join("docs")).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "md") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap();
        if let Ok(doc) = parse_document(&text)
            && let Some(id) = doc.frontmatter.id.clone() {
                out.push((id, doc.frontmatter));
            }
    }
    out
}

#[test]
fn validate_infer_gate_graph_and_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git::init_for_tests(repo).unwrap();
    std::fs::create_dir_all(repo.join("docs")).unwrap();

    std::fs::write(repo.join("docs").join(format!("{ID_ROOT}.md")), ROOT_DOC).unwrap();
    std::fs::write(repo.join("docs").join(format!("{ID_DECISION}.md")), DECISION_DOC).unwrap();
    std::fs::write(repo.join("docs/static_ip.md"), read_fixture("static_ip.md")).unwrap();
    std::fs::write(repo.join("docs/world_frame_transforms.md"), read_fixture("world_frame_transforms.md")).unwrap();
    git::commit_all(repo, "seed messy notes").unwrap();

    // --- validate --fix ---
    let cfg = Config::load(repo).unwrap();
    let outcome = check::validate_repo(&cfg, true).unwrap();
    assert_eq!(outcome.files.len(), 4);
    assert!(outcome.findings.is_empty(), "no duplicate ids or future timestamps expected: {:?}", outcome.findings);

    let static_ip_outcome = outcome.files.iter().find(|(p, _)| p.ends_with("static_ip.md")).map(|(_, o)| o).unwrap();
    assert!(static_ip_outcome.changed);
    assert!(static_ip_outcome.findings.iter().any(|f| f.code == "legacy-frontmatter"));
    assert!(static_ip_outcome.findings.iter().any(|f| f.code == "renamed"));
    let static_ip_fm = &static_ip_outcome.document.frontmatter;
    assert!(kb::model::is_ulid(static_ip_fm.id.as_deref().unwrap()));
    assert_eq!(static_ip_fm.extra.get("source_id").and_then(|v| v.as_str()), Some("static_ip"));
    assert!(!static_ip_outcome.document.body.contains("aliases:"));

    let wft_outcome = outcome.files.iter().find(|(p, _)| p.ends_with("world_frame_transforms.md")).map(|(_, o)| o).unwrap();
    assert!(wft_outcome.changed);
    assert!(wft_outcome.findings.iter().any(|f| f.code == "renamed"));
    let wft_fm = &wft_outcome.document.frontmatter;
    assert_eq!(wft_fm.title.as_deref(), Some("World-Frame Transforms"));
    assert_eq!(wft_fm.layer, Some(Layer::Reference));

    // the fixed files actually landed on disk under their new, ulid-named paths.
    assert!(!repo.join("docs/static_ip.md").is_file());
    assert!(!repo.join("docs/world_frame_transforms.md").is_file());
    assert!(repo.join("docs").join(format!("{}.md", static_ip_fm.id.as_deref().unwrap())).is_file());
    assert!(repo.join("docs").join(format!("{}.md", wft_fm.id.as_deref().unwrap())).is_file());

    let c1 = git::commit_all(repo, "validate --fix").unwrap();

    // --- infer: revise a critical section and append a new one in the same commit ---
    let decision_path = format!("docs/{ID_DECISION}.md");
    let edited = DECISION_DOC.replace("we decided x.", "we decided x, revised.").replace(
        "y follows.\n",
        "y follows.\n\n# notes\n\nsome afterthought.\n",
    );
    std::fs::write(repo.join(&decision_path), &edited).unwrap();
    let c2 = git::commit_all(repo, "revise decision, append notes").unwrap();

    let inferred = check::infer_range(&cfg, repo, &c1, &c2).unwrap();
    let (_, ops) = inferred.iter().find(|(p, _)| p == &decision_path).expect("decision doc in the diff");
    // both an existing section changing and the heading set growing land on the same row of
    // design.md §11's table ("more than one section changed, or the heading set changed"): rewrite.
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].op, Operation::Rewrite);

    // --- gate: a critical section (`decision`) changed, so this is review class regardless of actor ---
    let base_doc = {
        let bytes = git::show_file(repo, &c1, &decision_path).unwrap();
        parse_document(&String::from_utf8(bytes).unwrap()).unwrap()
    };
    let head_doc = parse_document(&edited).unwrap();
    let decision = check::classify_change(&cfg, Some(&base_doc), &head_doc, None, false);
    assert_eq!(decision.class, MergeClass::Review);
    assert!(decision.reasons.iter().any(|r| r.contains("decision")));

    // --- graph: introduce a hierarchy cycle (root now points back down at decision) ---
    let root_with_cycle = ROOT_DOC.replace(
        "agent_policy = \"locked\"",
        &format!("agent_policy = \"locked\"\nparent = \"kb:{ID_DECISION}\""),
    );
    std::fs::write(repo.join("docs").join(format!("{ID_ROOT}.md")), root_with_cycle).unwrap();
    let docs = collect_docs(repo);
    let graph_findings = check::check_graph(&docs);
    assert!(graph_findings.iter().any(|f| f.code == "cycle" && f.message.contains("hierarchy")));

    // --- secrets: plant a fake github token and catch it on the staged diff ---
    let planted = format!("{edited}\nleaked = \"ghp_{}\"\n", "a".repeat(36));
    std::fs::write(repo.join(&decision_path), planted).unwrap();
    git::run(repo, &["add", "-A"]).unwrap();
    let secret_findings = check::scan_staged(&cfg, repo).unwrap();
    assert!(secret_findings.iter().any(|f| f.code == "secret" && f.severity == Severity::Error && f.path.as_deref() == Some(&decision_path)));
    // the finding never carries the secret itself.
    assert!(!secret_findings.iter().any(|f| f.message.contains(&"a".repeat(36))));
}
