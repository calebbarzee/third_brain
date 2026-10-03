//! end-to-end test of the `curate` hook: a temp kb repo with 3 committed documents and an
//! index, then `curate::run_with_provider` through both the deferred path (no provider
//! configured) and the applied path (a scripted provider), landing in a session. design.md
//! §16 (deferred prompt), §25 (capture to canon).

use kb::config::Config;
use kb::curate::{self, CurateInput};
use kb::git;
use kb::provider::{Block, CompletionRequest, CompletionResponse, Provider, Usage};
use std::path::Path;
use std::sync::Mutex;

const ID_TARGET: &str = "01HZZZZZZZ0000000000000AB1";
const ID_OTHER_A: &str = "01HZZZZZZZ0000000000000AB2";
const ID_OTHER_B: &str = "01HZZZZZZZ0000000000000AB3";

fn write_doc(root: &Path, id: &str, extra: &str, body: &str) {
    std::fs::write(root.join("docs").join(format!("{id}.md")), format!("+++\nid = \"{id}\"\n{extra}\n+++\n\n{body}")).unwrap();
}

/// a repo with `kb::cli::init::init`'s skeleton, write access for `svc-curate`, 3 committed
/// documents, and a built index.
fn seed_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    git::run(root, &["config", "user.name", "test user"]).unwrap();
    git::run(root, &["config", "user.email", "test@example.com"]).unwrap();

    // svc-curate needs write access to touch any document: grant it via the "agents" group.
    std::fs::write(root.join("kb.toml"), "[defaults]\nwrite_groups = [\"agents\"]\n").unwrap();
    std::fs::write(
        root.join("groups.toml"),
        "[principals.svc-curate]\nkind = \"service\"\nowner = \"caleb\"\ngroups = [\"agents\"]\nlayer_ceiling = \"principle\"\n\n[groups.agents]\nmembers = [\"svc-curate\"]\n",
    )
    .unwrap();

    write_doc(
        root,
        ID_TARGET,
        "layer = \"procedure\"\ntitle = \"Static IP setup\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        "# Static IP setup\n\n## Steps\n\nUse nmcli to configure the interface on the launcher.\n",
    );
    write_doc(
        root,
        ID_OTHER_A,
        "layer = \"reference\"\ntitle = \"Unrelated topic\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        "# Unrelated topic\n\nSomething about kayaking equipment.\n",
    );
    write_doc(
        root,
        ID_OTHER_B,
        "layer = \"reference\"\ntitle = \"Another topic\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
        "# Another topic\n\nNotes on office furniture.\n",
    );
    git::commit_all(root, "seed 3 documents").unwrap();

    let cfg = Config::load(root).unwrap();
    kb::index::build(&cfg, None).unwrap();
    dir
}

/// a provider that returns each scripted response text in order; never touches the network.
struct ScriptedProvider {
    responses: Mutex<std::vec::IntoIter<String>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<String>) -> Self {
        Self { responses: Mutex::new(responses.into_iter()) }
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let text = self.responses.lock().unwrap().next().ok_or_else(|| anyhow::anyhow!("scripted provider exhausted"))?;
        Ok(CompletionResponse { blocks: vec![Block::Text { text }], usage: Usage::default(), stop_reason: "end_turn".to_string(), raw: serde_json::json!({}) })
    }
}

#[test]
fn no_provider_configured_defers_and_opens_no_session() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let main_before = git::rev_parse(root, "main").unwrap();

    let input = CurateInput {
        actor: "agent:curate@svc-curate".to_string(),
        task: "curate a free-text proposal".to_string(),
        capture_ids: vec![],
        proposal_text: Some("use nmcli to configure a static ip on the launcher".to_string()),
        dry_run: false,
    };
    let report = curate::run_with_provider(&cfg, input, None).unwrap();

    assert!(report.deferred, "{report:?}");
    assert!(report.session_id.is_none(), "{report:?}");
    assert!(report.settled_commits.is_empty());
    assert!(kb::session::list(&cfg).unwrap().is_empty(), "no session should have opened");

    let main_after = git::rev_parse(root, "main").unwrap();
    assert_eq!(main_before, main_after, "main must be untouched by a deferred run");
}

#[test]
fn scripted_provider_lands_an_append_in_a_settled_session() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let main_before = git::rev_parse(root, "main").unwrap();

    let response = serde_json::json!({
        "ops": [{
            "op": "append",
            "target_id": ID_TARGET,
            "content": "## Verification\n\nRun `nmcli connection show` to confirm the static ip took effect.",
            "justification": "the proposal adds a verification step for the static ip procedure that the document does not yet cover.",
            "confidence": 0.88
        }],
        "discards": [],
        "summary": "appended a verification step to the static ip procedure",
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![response]);

    let input = CurateInput {
        actor: "agent:curate@svc-curate".to_string(),
        task: "curate a free-text proposal about static ip verification".to_string(),
        capture_ids: vec![],
        proposal_text: Some("after setting a static ip with nmcli on the launcher, run nmcli connection show to verify it took effect".to_string()),
        dry_run: false,
    };
    let report = curate::run_with_provider(&cfg, input, Some(Box::new(provider))).unwrap();

    assert!(!report.deferred, "{report:?}");
    assert_eq!(report.applied.len(), 1, "{report:?}");
    assert_eq!(report.applied[0].doc_id, ID_TARGET);
    assert_eq!(report.escalations.len(), 0);
    assert_eq!(report.settled_commits.len(), 1, "{report:?}");

    let session_id = report.session_id.clone().expect("a session should have opened");
    let session = kb::session::load(&cfg, &session_id).unwrap();
    assert_eq!(session.state, kb::session::SessionState::Settled);

    // the settled commit's message carries the op's justification verbatim (design.md §12).
    let message = git::commit_message(root, &report.settled_commits[0]).unwrap();
    assert!(
        message.contains("the proposal adds a verification step for the static ip procedure that the document does not yet cover."),
        "{message}"
    );
    assert!(message.contains("appended a verification step"), "{message}");

    // the document itself was actually changed inside the session worktree.
    let doc_text = std::fs::read_to_string(session.worktree.join("docs").join(format!("{ID_TARGET}.md"))).unwrap();
    assert!(doc_text.contains("## Verification"));
    assert!(doc_text.contains("nmcli connection show"));

    // settling a session never touches `main`; only `kb merge` does.
    let main_after = git::rev_parse(root, "main").unwrap();
    assert_eq!(main_before, main_after, "main must be unchanged by a settled (unmerged) session");
}

/// the simulation's pattern: an agent escalated the same open question every day, nowhere a
/// person would look, and left an empty session behind each time. an escalate-only run must file
/// one queue item, leave no session, and see that item in its next packet instead of filing again.
#[test]
fn an_escalation_reaches_the_queue_once_and_leaves_no_session() {
    let dir = seed_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();
    let escalate = || {
        serde_json::json!({
            "ops": [{
                "op": "escalate",
                "target_id": ID_TARGET,
                "justification": "the launcher address in the material contradicts the document; a person must choose.",
                "confidence": 0.9
            }],
            "discards": [],
            "summary": "escalated a contradiction",
        })
        .to_string()
    };
    let input = || CurateInput {
        actor: "agent:curate@svc-curate".to_string(),
        task: "curate a free-text proposal".to_string(),
        capture_ids: vec![],
        proposal_text: Some("use nmcli to configure a static ip on the launcher at a different address".to_string()),
        dry_run: false,
    };

    let report = curate::run_with_provider(&cfg, input(), Some(Box::new(ScriptedProvider::new(vec![escalate()])))).unwrap();
    assert_eq!(report.escalations.len(), 1);
    curate::run_with_provider(&cfg, input(), Some(Box::new(ScriptedProvider::new(vec![escalate()])))).unwrap();

    let queued = kb::maintain::list_escalations(&cfg).unwrap();
    assert_eq!(queued.len(), 1, "one open question, filed once: {queued:?}");
    assert_eq!(queued[0].1.doc_ids, vec![ID_TARGET.to_string()]);
    let open: Vec<_> = kb::session::list(&cfg).unwrap().into_iter().filter(|s| s.state == kb::session::SessionState::Open).collect();
    assert!(open.is_empty(), "an escalate-only run left an open session: {open:?}");
    let texts = vec![("proposal".to_string(), "static ip on the launcher with nmcli".to_string())];
    let actor = kb::policy::Actor::parse("agent:curate@svc-curate").unwrap();
    let packet = curate::build_packet(&cfg, &actor, "next run", &texts).unwrap();
    let target = packet.candidates.iter().find(|c| c.id == ID_TARGET).expect("target is a candidate");
    assert_eq!(target.open_escalations.len(), 1, "the next packet shows the open question");
}
