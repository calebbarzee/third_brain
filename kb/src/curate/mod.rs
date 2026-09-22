//! the `curate` hook: turn captures (or a free-text proposal) into operations, applied inside a
//! session, with a run log. design.md §11 (operations), §16 (runs, context packet), §19
//! (hooks, prompts, guards), §25 (capture to canon).

mod capture_adapter;
pub mod ops;
pub mod packet;

pub use ops::{Applied, ProposedOp};
pub use packet::{Candidate, Excerpt, Packet, build as build_packet};

use crate::config::Config;
use crate::git;
use crate::model::{MergeClass, Operation, Ref};
use crate::parse::parse_document;
use crate::parse::body::parse_body;
use crate::policy::{self, Actor, DocPolicyView, OpRequest};
use crate::provider::structured::complete_json;
use crate::provider::{self, CompletionRequest, Message, Provider, Role};
use crate::runlog::{RunLog, RunStart, RunStatus};
use crate::session;
use anyhow::{Context, Result, anyhow};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");
const DEFAULT_CURATE: &str = include_str!("../../prompts/curate.md");

/// one proposed discard: the capture id or excerpt uri, and a one-line reason.
#[derive(Debug, Clone, Deserialize)]
struct DiscardWire {
    id: String,
    reason: String,
}

/// the wire shape `complete_json` deserializes; `CurateOutput` is the friendlier public shape
/// (`discards` as tuples, matching the plan's api) built from this.
#[derive(Debug, Clone, Deserialize)]
struct CurateWire {
    ops: Vec<ProposedOp>,
    discards: Vec<DiscardWire>,
    summary: String,
}

/// the model's answer for one packet: operations to apply, material to discard, and a summary
/// used in the settled commit message and the run report.
#[derive(Debug, Clone, Serialize)]
pub struct CurateOutput {
    pub ops: Vec<ProposedOp>,
    /// (capture id or excerpt uri, reason).
    pub discards: Vec<(String, String)>,
    pub summary: String,
}

/// `kb curate captures` / `kb curate propose` input.
#[derive(Debug, Clone)]
pub struct CurateInput {
    pub actor: String,
    pub task: String,
    pub capture_ids: Vec<String>,
    pub proposal_text: Option<String>,
    pub dry_run: bool,
}

/// the run's outcome, whether it deferred, dry-ran, or landed a settled session.
#[derive(Debug, Clone, Serialize)]
pub struct CurateReport {
    pub run_id: String,
    pub session_id: Option<String>,
    pub applied: Vec<Applied>,
    pub escalations: Vec<String>,
    pub discards: usize,
    pub deferred: bool,
    pub settled_commits: Vec<String>,
}

fn load_prompt(root: &Path, name: &str, default: &'static str) -> Result<String> {
    let path = root.join("prompts").join(name);
    if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    } else {
        Ok(default.to_string())
    }
}

fn curate_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ops": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["create", "append", "revise", "rewrite", "supersede", "link", "retire", "escalate"] },
                        "target_id": { "type": ["string", "null"] },
                        "section": { "type": ["string", "null"] },
                        "field": { "type": ["string", "null"] },
                        "content": { "type": ["string", "null"] },
                        "title": { "type": ["string", "null"] },
                        "layer": { "type": ["string", "null"] },
                        "refs": { "type": "array" },
                        "new_id": { "type": ["string", "null"] },
                        "justification": { "type": "string" },
                        "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                    },
                    "required": ["op", "justification", "confidence"],
                },
            },
            "discards": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "id": { "type": "string" }, "reason": { "type": "string" } },
                    "required": ["id", "reason"],
                },
            },
            "summary": { "type": "string" },
        },
        "required": ["ops", "discards", "summary"],
    })
}

/// validate one raw `ops[i]` json value against its target candidate's `allowed_ops` and
/// `headings`; `create` and `escalate` need no candidate (design.md §6, prompts/curate.md).
fn validate_op(i: usize, opv: &Value, candidates: &[Candidate]) -> Result<(), String> {
    let op_str = opv.get("op").and_then(Value::as_str).ok_or_else(|| format!("ops[{i}]: missing string field \"op\""))?;
    let op: Operation = op_str.parse().map_err(|e| format!("ops[{i}]: {e}"))?;

    let justification = opv.get("justification").and_then(Value::as_str).unwrap_or("");
    if justification.trim().is_empty() {
        return Err(format!("ops[{i}]: justification must not be empty"));
    }
    let confidence = opv.get("confidence").and_then(Value::as_f64).ok_or_else(|| format!("ops[{i}]: missing numeric field \"confidence\""))?;
    if !(0.0..=1.0).contains(&confidence) {
        return Err(format!("ops[{i}]: confidence {confidence} is not within 0..1"));
    }

    match op {
        Operation::Split | Operation::Merge | Operation::Delete => {
            return Err(format!("ops[{i}]: {op} is a review-class operation; propose escalate instead"));
        }
        Operation::Create | Operation::Escalate => return Ok(()),
        _ => {}
    }

    let target_id = opv.get("target_id").and_then(Value::as_str).ok_or_else(|| format!("ops[{i}]: {op} requires target_id"))?;
    let candidate = candidates.iter().find(|c| c.id == target_id).ok_or_else(|| {
        let ids = candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ");
        format!("ops[{i}]: target_id `{target_id}` is not one of the packet's candidates; choose target_id from the candidates list: {ids}, or use create/discard")
    })?;
    if !candidate.allowed_ops.iter().any(|a| a.op == op) {
        return Err(format!("ops[{i}]: {op} is not in `{target_id}`'s allowed_ops"));
    }
    if let Some(section) = opv.get("section").and_then(Value::as_str)
        && !candidate.headings.iter().any(|h| h == section)
    {
        return Err(format!("ops[{i}]: section `{section}` does not exist on `{target_id}`"));
    }
    Ok(())
}

fn validate_curate_output(value: &Value, candidates: &[Candidate], discard_ids: &[String]) -> Result<(), String> {
    let ops = value.get("ops").and_then(Value::as_array).ok_or("missing array field \"ops\"")?;
    for (i, opv) in ops.iter().enumerate() {
        validate_op(i, opv, candidates)?;
    }
    let discards = value.get("discards").and_then(Value::as_array).ok_or("missing array field \"discards\"")?;
    for (i, d) in discards.iter().enumerate() {
        if d.get("id").and_then(Value::as_str).is_none() {
            return Err(format!("discards[{i}]: missing string field \"id\""));
        }
        if d.get("reason").and_then(Value::as_str).is_none() {
            return Err(format!("discards[{i}]: missing string field \"reason\""));
        }
        // a discard names one of the excerpts given in the packet, never a placeholder or a guess
        let id = d.get("id").and_then(Value::as_str).unwrap_or_default();
        if !discard_ids.is_empty() && !discard_ids.iter().any(|x| x == id) {
            return Err(format!("discards[{i}]: id `{id}` is not one of the excerpts: {}", discard_ids.join(", ")));
        }
    }
    if value.get("summary").and_then(Value::as_str).is_none() {
        return Err("missing string field \"summary\"".to_string());
    }
    Ok(())
}

/// call the `curate` role's provider with `packet`, validating every proposed op against the
/// packet's own `allowed_ops` and `headings`; an out-of-set answer retries once (per
/// `manifest.toml [thresholds] classify_retries`) with the validation error, then fails.
pub fn propose(cfg: &Config, packet: &Packet, provider: &dyn Provider) -> Result<CurateOutput> {
    let style = load_prompt(&cfg.root, "style.md", DEFAULT_STYLE)?;
    let curate_prompt = load_prompt(&cfg.root, "curate.md", DEFAULT_CURATE)?;
    let system = format!("{style}\n\n{curate_prompt}");
    let user = packet::render(packet);

    let max_tokens = cfg.kb.providers.get("curate").and_then(|pc| pc.max_tokens).unwrap_or(4096);
    let req = CompletionRequest {
        system: Some(system),
        messages: vec![Message { role: Role::User, content: user }],
        max_tokens,
        temperature: Some(0.0),
        json_schema: Some(curate_schema()),
    };

    let retries = cfg.manifest.thresholds.classify_retries;
    let candidates = packet.candidates.clone();
    let discard_ids: Vec<String> = packet.excerpts.iter().map(|e| e.source_uri.clone()).collect();
    let structured =
        complete_json::<CurateWire>(provider, req, move |v| validate_curate_output(v, &candidates, &discard_ids), retries)?;
    let wire = structured.value;

    let excerpt_source = packet.excerpts.first().map(|e| e.source_uri.clone());
    let mut ops = wire.ops;
    for op in ops.iter_mut() {
        if op.op == Operation::Create && !op.refs.iter().any(|r| r.rel == "source")
            && let Some(uri) = &excerpt_source
        {
            op.refs.push(Ref { rel: "source".to_string(), uri: uri.clone(), sha256: None, media_type: None });
        }
    }
    let discards = wire.discards.into_iter().map(|d| (d.id, d.reason)).collect();
    Ok(CurateOutput { ops, discards, summary: wire.summary })
}

/// `DocPolicyView` for an op's `target_id`, read from the session worktree; `None` for `create`
/// (no document exists yet) and for any op missing a `target_id`.
fn load_doc_view(cfg: &Config, worktree: &Path, op: &ProposedOp) -> Result<Option<DocPolicyView>> {
    if op.op == Operation::Create {
        return Ok(None);
    }
    let Some(target_id) = &op.target_id else { return Ok(None) };
    let Ok(path) = ops::locate_doc(worktree, target_id) else { return Ok(None) };
    let text = std::fs::read_to_string(&path)?;
    let doc = parse_document(&text)?;
    let word_count = parse_body(&doc.body).word_count;

    let (band, in_canon, in_code) = match crate::index::open_newest(cfg) {
        Ok(conn) => {
            let crit_band: Option<String> =
                conn.query_row("select crit_band from documents where id = ?1", params![target_id], |r| r.get(0)).optional().ok().flatten();
            let band = crit_band.map(|b| if b == "low" { MergeClass::Auto } else { MergeClass::Review });
            let (ic, icd): (i64, i64) = conn
                .query_row("select in_canon, in_code from graph_metrics where doc_id = ?1", params![target_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap_or((0, 0));
            (band, ic as u32, icd as u32)
        }
        Err(_) => (None, 0, 0),
    };
    Ok(Some(DocPolicyView::from_frontmatter(cfg, &doc.frontmatter, word_count, band, in_canon, in_code)?))
}

fn added_words(op: &ProposedOp) -> usize {
    op.content.as_deref().map(|c| c.split_whitespace().count()).unwrap_or(0)
}

/// every capture id known to the catalog with status `"new"`. best-effort: an error from the
/// (possibly still-stubbed) capture store surfaces as an empty list plus the error, not a panic.
pub fn list_new_captures(cfg: &Config) -> Result<Vec<String>> {
    let caps = capture_adapter::list(cfg, Some("new"), None, 1000)?;
    Ok(caps.into_iter().map(|c| c.capture_id).collect())
}

/// `kb curate`: gather source texts, build the packet, propose or defer, apply the result inside
/// a session, and settle. equivalent to `run_with_provider(cfg, input, None)`.
pub fn run(cfg: &Config, input: CurateInput) -> Result<CurateReport> {
    run_with_provider(cfg, input, None)
}

/// `run`, with an explicit provider override for tests (a `FakeProvider`) or callers that
/// already resolved one. `None` falls back to `provider::provider_for_role(cfg, "curate")`, and
/// when that is also `None` the run defers (design.md §16).
pub fn run_with_provider(cfg: &Config, input: CurateInput, provider_override: Option<Box<dyn Provider>>) -> Result<CurateReport> {
    let actor = Actor::parse(&input.actor).map_err(|e| anyhow!(e))?;

    let mut texts: Vec<(String, String)> = Vec::new();
    let mut discards: Vec<(String, String)> = Vec::new();
    for id in &input.capture_ids {
        match capture_adapter::get(cfg, id) {
            Ok(Some(capture)) => match capture_adapter::text(cfg, &capture) {
                Ok(text) => texts.push((capture.source_uri.clone(), text)),
                Err(e) => discards.push((id.clone(), format!("could not read capture text: {e:#}"))),
            },
            Ok(None) => discards.push((id.clone(), "capture not found".to_string())),
            Err(e) => discards.push((id.clone(), format!("capture store error: {e:#}"))),
        }
    }
    if let Some(proposal) = &input.proposal_text {
        texts.push(("proposal:free-text".to_string(), proposal.clone()));
    }

    let mut log = RunLog::create(
        cfg,
        RunStart { actor: actor.to_string(), principal: Some(actor.principal.clone()), skill: Some("curate".to_string()), deferred: false, ..Default::default() },
    )?;
    let run_id = log.run_id().to_string();

    let packet = packet::build(cfg, &actor, &input.task, &texts)?;
    log.context(serde_json::to_value(&packet)?)?;

    let provider: Option<Box<dyn Provider>> = match provider_override {
        Some(p) => Some(p),
        None => provider::provider_for_role(cfg, "curate")?,
    };
    let Some(provider) = provider else {
        log.deferred_request(serde_json::to_value(&packet)?)?;
        println!("{}", packet::render(&packet));
        log.run_end(RunStatus::Deferred, None, None, None, &[], &[])?;
        log.seal(cfg)?;
        return Ok(CurateReport { run_id, session_id: None, applied: vec![], escalations: vec![], discards: discards.len(), deferred: true, settled_commits: vec![] });
    };

    let output = match propose(cfg, &packet, provider.as_ref()) {
        Ok(o) => o,
        Err(e) => {
            log.error(&format!("{e:#}"))?;
            log.run_end(RunStatus::Failed, None, None, None, &[], &[])?;
            log.seal(cfg)?;
            return Err(e);
        }
    };
    discards.extend(output.discards.clone());

    if input.dry_run {
        for op in &output.ops {
            println!("{} target={:?} section={:?} confidence={:.2}: {}", op.op, op.target_id, op.section, op.confidence, op.justification);
        }
        for (id, reason) in &discards {
            println!("discard {id}: {reason}");
        }
        log.report(&output.summary)?;
        log.run_end(RunStatus::Ok, None, None, Some(&output.summary), &[], &[])?;
        log.seal(cfg)?;
        return Ok(CurateReport { run_id, session_id: None, applied: vec![], escalations: vec![], discards: discards.len(), deferred: false, settled_commits: vec![] });
    }

    let candidate_ids: Vec<String> = packet.candidates.iter().map(|c| c.id.clone()).collect();
    let mut session = session::open(cfg, &input.task, Some(&actor.to_string()), &candidate_ids)?;
    session.run_id = Some(run_id.clone());
    session::save(cfg, &session)?;

    let owner = policy::responsible_party(cfg, &actor);
    let mut applied_list = Vec::new();
    let mut escalations = Vec::new();
    let mut justifications = Vec::new();

    for op in &output.ops {
        if op.op == Operation::Escalate {
            log.decision(&format!("escalate: {}", op.justification))?;
            escalations.push(op.justification.clone());
            continue;
        }

        let doc_view = load_doc_view(cfg, &session.worktree, op)?;
        let req = OpRequest { op: op.op, section: op.section.clone(), field: op.field.clone(), added_words: added_words(op), target_layer: op.layer };
        let decision = policy::decide(cfg, &actor, doc_view.as_ref(), &req);
        log.policy_decision(op.target_id.as_deref().unwrap_or("(new)"), op.op, decision.allow, decision.class, &decision.reasons)?;
        if !decision.allow {
            discards.push((op.target_id.clone().unwrap_or_default(), format!("policy denied: {}", decision.reasons.join("; "))));
            continue;
        }

        match ops::apply(cfg, &session.worktree, op, &owner) {
            Ok(applied) => {
                log.op(op.op, &applied.doc_id, op.section.as_deref(), applied.before_hash.as_deref(), Some(&applied.after_hash), Some(applied.diff.as_bytes()))?;
                if let Some(secondary) = &applied.secondary {
                    log.op(Operation::Create, &secondary.doc_id, None, secondary.before_hash.as_deref(), Some(&secondary.after_hash), Some(secondary.diff.as_bytes()))?;
                }
                git::run(&session.worktree, &["add", "-A"])?;
                git::commit_all(&session.worktree, &format!("wip: {} {}", op.op, applied.doc_id))?;
                justifications.push(op.justification.clone());
                applied_list.push(applied);
            }
            Err(e) => {
                discards.push((op.target_id.clone().unwrap_or_default(), format!("apply failed: {e:#}")));
            }
        }
    }

    for id in &input.capture_ids {
        let status = if discards.iter().any(|(d_id, _)| d_id == id) { "discarded" } else { "curated" };
        let _ = capture_adapter::set_status(cfg, id, status, Some(&run_id));
    }

    let why = justifications.iter().map(|j| format!("- {j}")).collect::<Vec<_>>().join("\n");
    let message = if why.is_empty() { output.summary.clone() } else { format!("{}\n\n{why}", output.summary) };
    let settled_commits = if applied_list.is_empty() { vec![] } else { session::settle(cfg, &mut session, Some(&message))? };

    log.settle(&settled_commits)?;
    log.report(&output.summary)?;
    log.run_end(RunStatus::Ok, None, None, Some(&output.summary), &settled_commits, &[])?;
    log.seal(cfg)?;

    if !settled_commits.is_empty() {
        println!("next: kb merge {}", session.id);
    }

    Ok(CurateReport { run_id, session_id: Some(session.id.clone()), applied: applied_list, escalations, discards: discards.len(), deferred: false, settled_commits })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Layer;
    use crate::provider::fake::FakeProvider;

    fn candidate() -> Candidate {
        Candidate {
            id: "01K5A7Q2XW3ZRCM8T6YB4VJH0A".to_string(),
            title: "install steps".to_string(),
            layer: Layer::Procedure.as_str().to_string(),
            status: "canon".to_string(),
            agent_policy: "editable".to_string(),
            criticality_pct: Some(10.0),
            headings: vec!["install".to_string(), "install/linux".to_string()],
            matched_sections: vec![("install/linux".to_string(), "use the package manager".to_string())],
            allowed_ops: vec![policy::AllowedOp { op: Operation::Append, class: MergeClass::Auto, reasons: vec![] }],
            budget_headroom_words: 100,
        }
    }

    fn packet_with(candidates: Vec<Candidate>) -> Packet {
        Packet {
            task: "curate one capture".to_string(),
            objective: "curate one capture".to_string(),
            candidates,
            excerpts: vec![Excerpt { source_uri: "capture:01ABC".to_string(), text: "some new fact".to_string(), untrusted: true }],
            neighbors: vec![],
            sessions_hint: vec![],
            style: "write plainly".to_string(),
        }
    }

    #[test]
    fn propose_accepts_a_valid_op() {
        let packet = packet_with(vec![candidate()]);
        let response = serde_json::json!({
            "ops": [{
                "op": "append", "target_id": "01K5A7Q2XW3ZRCM8T6YB4VJH0A", "section": null, "field": null,
                "content": "## new note\n\nsome new fact.", "title": null, "layer": null, "refs": [], "new_id": null,
                "justification": "the capture adds a fact not covered anywhere in the candidate.", "confidence": 0.85
            }],
            "discards": [],
            "summary": "appended one note",
        })
        .to_string();
        let provider = FakeProvider::new(vec![response]);
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let out = propose(&cfg, &packet, &provider).unwrap();
        assert_eq!(out.ops.len(), 1);
        assert_eq!(out.ops[0].op, Operation::Append);
        assert_eq!(out.summary, "appended one note");
    }

    #[test]
    fn propose_retries_after_an_out_of_set_op() {
        let packet = packet_with(vec![candidate()]);
        let bad = serde_json::json!({
            "ops": [{
                "op": "rewrite", "target_id": "01K5A7Q2XW3ZRCM8T6YB4VJH0A", "justification": "rewriting everything.", "confidence": 0.5
            }],
            "discards": [],
            "summary": "x",
        })
        .to_string();
        let good = serde_json::json!({
            "ops": [{
                "op": "append", "target_id": "01K5A7Q2XW3ZRCM8T6YB4VJH0A", "content": "## note\n\nfact.",
                "justification": "corrected to an allowed operation.", "confidence": 0.7
            }],
            "discards": [],
            "summary": "appended after retry",
        })
        .to_string();
        let provider = FakeProvider::new(vec![bad, good]);
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let out = propose(&cfg, &packet, &provider).unwrap();
        assert_eq!(out.summary, "appended after retry");
    }

    #[test]
    fn propose_injects_source_ref_for_create() {
        let packet = packet_with(vec![]);
        let response = serde_json::json!({
            "ops": [{
                "op": "create", "title": "new doc", "layer": "reference", "content": "# new doc\n\nfresh.",
                "justification": "nothing in the corpus covers this new fact from the capture.", "confidence": 0.9
            }],
            "discards": [],
            "summary": "created one doc",
        })
        .to_string();
        let provider = FakeProvider::new(vec![response]);
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let out = propose(&cfg, &packet, &provider).unwrap();
        assert!(out.ops[0].refs.iter().any(|r| r.rel == "source" && r.uri == "capture:01ABC"));
    }
}
