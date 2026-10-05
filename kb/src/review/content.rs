//! the `review` hook (design.md §15.2): validate a settled change against its justification,
//! the diffs, and the graph context, and decide whether it may merge.

use super::packet::{ReviewInput, ReviewPacket, gather};
use crate::config::Config;
use crate::model::MergeClass;
use crate::provider::structured::complete_json;
use crate::provider::{CompletionRequest, Message, Role, provider_for_role};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");
const DEFAULT_REVIEW: &str = include_str!("../../prompts/review.md");

/// the reviewer's verdict (design.md §15.2's output schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
	Approve,
	RequestChanges,
	Escalate,
}

/// what kind of problem one finding names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingKind {
	Factual,
	Contradiction,
	Provenance,
	Scope,
	Injection,
	Style,
	Completeness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFinding {
	pub doc_id: String,
	#[serde(default)]
	pub section: Option<String>,
	pub kind: FindingKind,
	pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewOutput {
	pub verdict: Verdict,
	pub confidence: f64,
	#[serde(default)]
	pub findings: Vec<ReviewFinding>,
	pub summary: String,
}

/// the merge-time consequence of a review verdict (design.md §15.2's effect table).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewDecision {
	pub class: MergeClass,
	/// false only for `request_changes`: findings go back to the writer instead of landing.
	pub merge_allowed: bool,
	pub reason: String,
}

/// run content review over `input`. `Ok(None)` when no provider is configured for role
/// `review` -- the "self_review = true" deferred case (design.md §15.2), not an error.
pub fn review(cfg: &Config, input: &ReviewInput) -> Result<Option<ReviewOutput>> {
	let Some(provider) = provider_for_role(cfg, "review")? else {
		return Ok(None);
	};

	let packet = gather(cfg, input)?;
	let style = load_prompt(&cfg.root, "style.md", DEFAULT_STYLE)?;
	let review_prompt = load_prompt(&cfg.root, "review.md", DEFAULT_REVIEW)?;
	let system = format!("{style}\n\n{review_prompt}");
	let user = packet.render();

	let max_tokens = cfg.kb.providers.get("review").and_then(|pc| pc.max_tokens).unwrap_or(2048);
	let req = CompletionRequest {
		system: Some(system),
		messages: vec![Message { role: Role::User, content: user }],
		max_tokens,
		temperature: Some(0.0),
		json_schema: Some(review_schema()),
	};

	let retries = cfg.manifest.thresholds.review_retries;
	let structured = complete_json::<ReviewOutput>(provider.as_ref(), req, validate_review, retries)?;
	Ok(Some(structured.value))
}

/// print the packet without calling a model; used by `kb review packet`.
pub fn packet_only(cfg: &Config, input: &ReviewInput) -> Result<ReviewPacket> {
	gather(cfg, input)
}

/// merge-time class and permission from a verdict, per design.md §15.2's effect table:
/// `approve` at or above `review_confidence` auto-merges; `approve` below it becomes review
/// class; `request_changes` blocks the merge outright; `escalate` is always review class.
pub fn decide(cfg: &Config, out: &ReviewOutput) -> ReviewDecision {
	let threshold = cfg.manifest.thresholds.review_confidence;
	// an approval that also reports a possible injection is not an approval to merge unseen
	if out.verdict == Verdict::Approve && out.findings.iter().any(|f| f.kind == FindingKind::Injection) {
		return ReviewDecision {
			class: MergeClass::Review,
			merge_allowed: true,
			reason: format!("approve at confidence {:.2}, but the reviewer reported a possible prompt injection", out.confidence),
		};
	}
	match out.verdict {
		Verdict::Approve if out.confidence >= threshold => {
			ReviewDecision { class: MergeClass::Auto, merge_allowed: true, reason: format!("approve, confidence {:.2} >= {:.2}", out.confidence, threshold) }
		}
		Verdict::Approve => ReviewDecision {
			class: MergeClass::Review,
			merge_allowed: true,
			reason: format!("approve, confidence {:.2} below {:.2}", out.confidence, threshold),
		},
		Verdict::RequestChanges => ReviewDecision {
			class: MergeClass::Review,
			merge_allowed: false,
			reason: "request_changes: findings return to the writer for a forward commit".to_string(),
		},
		Verdict::Escalate => ReviewDecision { class: MergeClass::Review, merge_allowed: true, reason: "escalate: needs human review".to_string() },
	}
}

fn load_prompt(root: &Path, name: &str, default: &'static str) -> Result<String> {
	let path = root.join("prompts").join(name);
	if path.is_file() { std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display())) } else { Ok(default.to_string()) }
}

fn review_schema() -> Value {
	json!({
		"type": "object",
		"properties": {
			"verdict": { "type": "string", "enum": ["approve", "request_changes", "escalate"] },
			"confidence": { "type": "number", "minimum": 0, "maximum": 1 },
			"findings": {
				"type": "array",
				"items": {
					"type": "object",
					"properties": {
						"doc_id": { "type": "string" },
						"section": { "type": ["string", "null"] },
						"kind": { "type": "string", "enum": ["factual", "contradiction", "provenance", "scope", "injection", "style", "completeness"] },
						"reason": { "type": "string" },
					},
					"required": ["doc_id", "kind", "reason"],
				},
			},
			"summary": { "type": "string" },
		},
		"required": ["verdict", "confidence", "findings", "summary"],
		"additionalProperties": false,
	})
}

fn validate_review(value: &Value) -> Result<(), String> {
	let verdict = value.get("verdict").and_then(Value::as_str).ok_or("missing string field \"verdict\"")?;
	if !matches!(verdict, "approve" | "request_changes" | "escalate") {
		return Err(format!("verdict \"{verdict}\" is not one of approve, request_changes, escalate"));
	}

	let confidence = value.get("confidence").and_then(Value::as_f64).ok_or("missing numeric field \"confidence\"")?;
	if !(0.0..=1.0).contains(&confidence) {
		return Err(format!("confidence {confidence} is not within 0..1"));
	}

	let findings = value.get("findings").and_then(Value::as_array).ok_or("missing array field \"findings\"")?;
	for (i, f) in findings.iter().enumerate() {
		let doc_id = f.get("doc_id").and_then(Value::as_str).ok_or_else(|| format!("finding {i}: missing string field \"doc_id\""))?;
		if doc_id.is_empty() {
			return Err(format!("finding {i}: \"doc_id\" must not be empty"));
		}
		let kind = f.get("kind").and_then(Value::as_str).ok_or_else(|| format!("finding {i}: missing string field \"kind\""))?;
		if !matches!(kind, "factual" | "contradiction" | "provenance" | "scope" | "injection" | "style" | "completeness") {
			return Err(format!("finding {i}: kind \"{kind}\" is not a known finding kind"));
		}
		if f.get("reason").and_then(Value::as_str).is_none_or(str::is_empty) {
			return Err(format!("finding {i}: missing or empty string field \"reason\""));
		}
	}

	if value.get("summary").and_then(Value::as_str).is_none_or(str::is_empty) {
		return Err("missing or empty string field \"summary\"".to_string());
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::Config;
	use crate::git as gitmod;
	use crate::provider::fake::FakeProvider;

	#[test]
	fn approve_with_an_injection_finding_is_review_class() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let out = ReviewOutput {
			verdict: Verdict::Approve,
			confidence: 0.95,
			findings: vec![ReviewFinding { doc_id: "x".into(), section: None, kind: FindingKind::Injection, reason: "text addressed to the reviewer".into() }],
			summary: "ok".into(),
		};
		let d = decide(&cfg, &out);
		assert_eq!(d.class, MergeClass::Review, "{d:?}");
		assert!(d.merge_allowed);
		let clean = ReviewOutput { findings: vec![], ..out };
		assert_eq!(decide(&cfg, &clean).class, MergeClass::Auto);
	}

	fn approve_json(confidence: f64) -> String {
		format!(r#"{{"verdict": "approve", "confidence": {confidence}, "findings": [], "summary": "looks fine"}}"#)
	}

	fn request_changes_json() -> String {
		r#"{"verdict": "request_changes", "confidence": 0.5, "findings": [{"doc_id": "01K5A7Q2XW3ZRCM8T6YB4VJH0N", "section": "install", "kind": "factual", "reason": "wrong port number"}], "summary": "fix the port"}"#.to_string()
	}

	fn escalate_json() -> String {
		r#"{"verdict": "escalate", "confidence": 0.4, "findings": [{"doc_id": "01K5A7Q2XW3ZRCM8T6YB4VJH0N", "section": null, "kind": "contradiction", "reason": "conflicts with another doc"}], "summary": "needs a human"}"#.to_string()
	}

	/// a temp repo with a two-commit history on one document and a `review` role configured
	/// (`kb.toml`'s provider kind is never actually dialed: tests call `review_with_script`,
	/// which substitutes a `FakeProvider` for the pipeline's completion step).
	fn repo_with_provider() -> (tempfile::TempDir, Config) {
		let dir = tempfile::tempdir().unwrap();
		gitmod::init_for_tests(dir.path()).unwrap();
		std::fs::create_dir_all(dir.path().join("docs")).unwrap();
		let id = "01K5A7Q2XW3ZRCM8T6YB4VJH0N";
		std::fs::write(
			dir.path().join(format!("docs/{id}.md")),
			format!(
				"+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# t\n\none\n"
			),
		)
		.unwrap();
		gitmod::commit_all(dir.path(), "base").unwrap();
		std::fs::write(
			dir.path().join(format!("docs/{id}.md")),
			format!(
				"+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# t\n\ntwo\n"
			),
		)
		.unwrap();
		gitmod::commit_all(dir.path(), "head").unwrap();
		std::fs::write(dir.path().join("kb.toml"), "[providers.review]\nkind = \"ollama\"\nmodel = \"test\"\n").unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		(dir, cfg)
	}

	fn base_head(dir: &tempfile::TempDir) -> (String, String) {
		let subjects = gitmod::log_subjects(dir.path(), "HEAD").unwrap();
		assert_eq!(subjects, vec!["base", "head"]);
		let head = gitmod::head(dir.path()).unwrap();
		let base = gitmod::run(dir.path(), &["rev-parse", "HEAD~1"]).unwrap().trim().to_string();
		(base, head)
	}

	/// runs `review()`'s full pipeline (gather + prompt + complete_json + decide) against a
	/// `FakeProvider` script, by calling the same building blocks `review()` uses internally
	/// with a provider substituted in place of `provider_for_role`'s real adapters.
	fn review_with_script(cfg: &Config, input: &ReviewInput, script: Vec<String>) -> ReviewOutput {
		let provider = FakeProvider::new(script);
		let packet = gather(cfg, input).unwrap();
		let req = CompletionRequest {
			system: Some("system".to_string()),
			messages: vec![Message { role: Role::User, content: packet.render() }],
			max_tokens: 2048,
			temperature: Some(0.0),
			json_schema: Some(review_schema()),
		};
		complete_json::<ReviewOutput>(&provider, req, validate_review, cfg.manifest.thresholds.review_retries).unwrap().value
	}

	#[test]
	fn approve_above_threshold_is_auto_and_allowed() {
		let (dir, cfg) = repo_with_provider();
		let (base, head) = base_head(&dir);
		let input = ReviewInput {
			session_id: None,
			base,
			head,
			main: None,
			justification: "fix typo".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};
		let out = review_with_script(&cfg, &input, vec![approve_json(0.9)]);
		let decision = decide(&cfg, &out);
		assert_eq!(decision.class, MergeClass::Auto);
		assert!(decision.merge_allowed);
	}

	#[test]
	fn approve_below_threshold_is_review_but_allowed() {
		let (dir, cfg) = repo_with_provider();
		let (base, head) = base_head(&dir);
		let input = ReviewInput {
			session_id: None,
			base,
			head,
			main: None,
			justification: "fix typo".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};
		let out = review_with_script(&cfg, &input, vec![approve_json(0.5)]);
		let decision = decide(&cfg, &out);
		assert_eq!(decision.class, MergeClass::Review);
		assert!(decision.merge_allowed);
	}

	#[test]
	fn request_changes_blocks_the_merge() {
		let (dir, cfg) = repo_with_provider();
		let (base, head) = base_head(&dir);
		let input = ReviewInput {
			session_id: None,
			base,
			head,
			main: None,
			justification: "fix typo".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};
		let out = review_with_script(&cfg, &input, vec![request_changes_json()]);
		let decision = decide(&cfg, &out);
		assert_eq!(decision.class, MergeClass::Review);
		assert!(!decision.merge_allowed);
		assert_eq!(out.findings.len(), 1);
		assert_eq!(out.findings[0].kind, FindingKind::Factual);
	}

	#[test]
	fn escalate_is_review_and_allowed() {
		let (dir, cfg) = repo_with_provider();
		let (base, head) = base_head(&dir);
		let input = ReviewInput {
			session_id: None,
			base,
			head,
			main: None,
			justification: "fix typo".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};
		let out = review_with_script(&cfg, &input, vec![escalate_json()]);
		let decision = decide(&cfg, &out);
		assert_eq!(decision.class, MergeClass::Review);
		assert!(decision.merge_allowed);
	}

	#[test]
	fn validation_retries_then_succeeds() {
		let (dir, cfg) = repo_with_provider();
		let (base, head) = base_head(&dir);
		let input = ReviewInput {
			session_id: None,
			base,
			head,
			main: None,
			justification: "fix typo".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};
		let out = review_with_script(&cfg, &input, vec!["not json".to_string(), r#"{"verdict": "bogus"}"#.to_string(), approve_json(0.95)]);
		assert_eq!(out.verdict, Verdict::Approve);
	}

	#[test]
	fn review_returns_none_without_a_configured_provider() {
		let dir = tempfile::tempdir().unwrap();
		gitmod::init_for_tests(dir.path()).unwrap();
		gitmod::commit_all(dir.path(), "empty").unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let head = gitmod::head(dir.path()).unwrap();
		let input = ReviewInput {
			session_id: None,
			base: head.clone(),
			head,
			main: None,
			justification: "x".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};
		assert!(review(&cfg, &input).unwrap().is_none());
	}

	#[test]
	fn validate_review_rejects_bad_shapes() {
		assert!(validate_review(&json!({"verdict": "approve", "confidence": 0.5, "findings": [], "summary": "ok"})).is_ok());
		assert!(validate_review(&json!({"verdict": "nope", "confidence": 0.5, "findings": [], "summary": "ok"})).is_err());
		assert!(validate_review(&json!({"verdict": "approve", "confidence": 1.5, "findings": [], "summary": "ok"})).is_err());
		assert!(validate_review(&json!({"verdict": "approve", "confidence": 0.5, "findings": [{"kind": "factual", "reason": "r"}], "summary": "ok"})).is_err());
		assert!(validate_review(&json!({"verdict": "approve", "confidence": 0.5, "findings": [], "summary": ""})).is_err());
	}

	/// hits a local ollama server with a small two-commit repo: the head commit's justification
	/// says "fix typo" but the diff also deletes a whole section. reports what the model
	/// returned rather than asserting a specific verdict, since a live model's exact judgment
	/// is not something a unit test should pin down.
	/// run with: KB_LIVE_PROVIDER_TESTS=1 cargo test -p kb --lib review::content::tests::live_review_flags_diff_justification_mismatch -- --ignored --nocapture
	#[test]
	#[ignore = "hits a local ollama server; see the doc comment for how to run it"]
	fn live_review_flags_diff_justification_mismatch() {
		if std::env::var("KB_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
			eprintln!("skipping: set KB_LIVE_PROVIDER_TESTS=1 to run");
			return;
		}
		let model = std::env::var("KB_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2:3b".to_string());
		let tags = std::process::Command::new("curl").args(["-s", "http://localhost:11434/api/tags"]).output();
		let available = match &tags {
			Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).contains(&model),
			_ => false,
		};
		if !available {
			eprintln!("skipping: ollama not reachable at localhost:11434, or model \"{model}\" is not pulled");
			return;
		}

		let dir = tempfile::tempdir().unwrap();
		gitmod::init_for_tests(dir.path()).unwrap();
		std::fs::create_dir_all(dir.path().join("docs")).unwrap();
		let id = "01K5A7Q2XW3ZRCM8T6YB4VJH0N";
		let fm =
			format!("+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n");
		let base_body = "# usage\n\nrun `kb sesion open` to start.\n\n# rollback\n\nif the merge fails, run `kb revert <session>` to undo it and restore the previous state.\n";
		std::fs::write(dir.path().join(format!("docs/{id}.md")), format!("{fm}{base_body}")).unwrap();
		gitmod::commit_all(dir.path(), "base").unwrap();
		let head_body = "# usage\n\nrun `kb session open` to start.\n";
		std::fs::write(dir.path().join(format!("docs/{id}.md")), format!("{fm}{head_body}")).unwrap();
		gitmod::commit_all(dir.path(), "fix typo").unwrap();

		std::fs::write(dir.path().join("kb.toml"), format!("[providers.review]\nkind = \"ollama\"\nmodel = \"{model}\"\n")).unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let head = gitmod::head(dir.path()).unwrap();
		let base = gitmod::run(dir.path(), &["rev-parse", "HEAD~1"]).unwrap().trim().to_string();
		let input = ReviewInput {
			session_id: None,
			base,
			head,
			main: None,
			justification: "fix typo".into(),
			actor: "human:caleb".into(),
			context_excerpts: vec![],
			session_facts: None,
		};

		let out = review(&cfg, &input).unwrap().expect("a provider is configured for role \"review\"");
		eprintln!("live review verdict: {:?}  confidence: {:.2}", out.verdict, out.confidence);
		eprintln!("summary: {}", out.summary);
		for f in &out.findings {
			eprintln!("  finding [{:?}] {}: {}", f.kind, f.doc_id, f.reason);
		}
		assert!(!out.summary.is_empty(), "expected a non-empty summary, got {out:?}");
	}
}
