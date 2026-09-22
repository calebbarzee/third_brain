//! the quick gate (`gate` role): a cheap binary-or-multi-label classifier meant for a small
//! local model or a hand-rolled script (a `command`-kind provider), used ahead of the full
//! `review` hook to skip it for changes an inexpensive classifier is confident are trivial.
//! `kb gate ask` (src/cli/gate.rs) is the manual entry point; `merge::merge` is the automated
//! one (design.md §15.2's per-merge review call, quick-gate addendum -- see the report).

use super::structured::complete_json;
use super::{CompletionRequest, Message, Provider, Role, provider_for_role};
use crate::config::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_GATE: &str = include_str!("../../prompts/gate.md");
const DEFAULT_MAX_TOKENS: u32 = 64;

/// one question for the gate: a closed label set, the text to classify, and optional context
/// (a one-sentence steer, e.g. what "trivial" means for this call).
#[derive(Debug, Clone)]
pub struct GateQuestion {
    pub labels: Vec<String>,
    pub text: String,
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateAnswer {
    pub label: String,
    pub p: f64,
    pub reason: String,
}

/// ask the provider configured for role `gate` to answer `q`. `Ok(None)` when no `gate`
/// provider is configured -- the same deferred-hook convention every other role follows
/// (design.md §16), not an error.
pub fn gate(cfg: &Config, q: &GateQuestion) -> Result<Option<GateAnswer>> {
    let Some(provider) = provider_for_role(cfg, "gate")? else {
        return Ok(None);
    };
    Ok(Some(gate_with(provider.as_ref(), cfg, q)?))
}

/// like `gate`, but against a caller-supplied provider instead of the one `kb.toml` names for
/// role `gate`. this is the seam `merge::merge_with_providers` uses to script a `FakeProvider`
/// gate answer in tests without a real `gate` role configured.
pub fn gate_with(provider: &dyn Provider, cfg: &Config, q: &GateQuestion) -> Result<GateAnswer> {
    let system = load_prompt(&cfg.root, "gate.md", DEFAULT_GATE)?;
    let user = build_user_message(q);
    let max_tokens = cfg.kb.providers.get("gate").and_then(|pc| pc.max_tokens).unwrap_or(DEFAULT_MAX_TOKENS);
    let req = CompletionRequest {
        system: Some(system),
        messages: vec![Message { role: Role::User, content: user }],
        max_tokens,
        temperature: Some(0.0),
        json_schema: Some(gate_schema(&q.labels)),
    };
    let labels = q.labels.clone();
    let structured = complete_json::<GateAnswer>(provider, req, move |v| validate_gate(v, &labels), 1)?;
    Ok(structured.value)
}

fn build_user_message(q: &GateQuestion) -> String {
    let mut user = format!("labels: {}\n", q.labels.join(", "));
    if let Some(c) = &q.context {
        user.push_str(&format!("context: {c}\n"));
    }
    user.push_str("\ntext:\n");
    user.push_str(&q.text);
    user.push_str("\n\noutput only the json object.");
    user
}

/// load `prompts/gate.md` from the repository root if present, else the crate's own default
/// (same convention as `classify::load_prompt` and `review::content::load_prompt`).
fn load_prompt(root: &Path, name: &str, default: &'static str) -> Result<String> {
    let path = root.join("prompts").join(name);
    if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    } else {
        Ok(default.to_string())
    }
}

fn gate_schema(labels: &[String]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "label": { "type": "string", "enum": labels },
            "p": { "type": "number", "minimum": 0, "maximum": 1 },
            "reason": { "type": "string" },
        },
        "required": ["label", "p", "reason"],
        "additionalProperties": false,
    })
}

fn validate_gate(value: &Value, labels: &[String]) -> Result<(), String> {
    let label = value.get("label").and_then(Value::as_str).ok_or("missing string field \"label\"")?;
    if !labels.iter().any(|l| l == label) {
        return Err(format!("label \"{label}\" is not one of {}", labels.join(", ")));
    }
    let p = value.get("p").and_then(Value::as_f64).ok_or("missing numeric field \"p\"")?;
    if !(0.0..=1.0).contains(&p) {
        return Err(format!("p {p} is not within 0..1"));
    }
    if value.get("reason").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err("missing or empty string field \"reason\"".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::fake::FakeProvider;

    fn q() -> GateQuestion {
        GateQuestion { labels: vec!["trivial".to_string(), "substantive".to_string()], text: "fixed a typo".to_string(), context: None }
    }

    #[test]
    fn validate_accepts_known_label_and_range() {
        let labels = vec!["trivial".to_string(), "substantive".to_string()];
        assert!(validate_gate(&json!({"label": "trivial", "p": 0.9, "reason": "r"}), &labels).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_label() {
        let labels = vec!["trivial".to_string(), "substantive".to_string()];
        assert!(validate_gate(&json!({"label": "bogus", "p": 0.9, "reason": "r"}), &labels).is_err());
    }

    #[test]
    fn validate_rejects_p_out_of_range() {
        let labels = vec!["trivial".to_string(), "substantive".to_string()];
        assert!(validate_gate(&json!({"label": "trivial", "p": 1.5, "reason": "r"}), &labels).is_err());
    }

    #[test]
    fn validate_rejects_missing_reason() {
        let labels = vec!["trivial".to_string(), "substantive".to_string()];
        assert!(validate_gate(&json!({"label": "trivial", "p": 0.5, "reason": ""}), &labels).is_err());
    }

    #[test]
    fn gate_with_parses_a_good_reply() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let provider = FakeProvider::new(vec![r#"{"label": "trivial", "p": 0.95, "reason": "just a typo fix"}"#.to_string()]);
        let answer = gate_with(&provider, &cfg, &q()).unwrap();
        assert_eq!(answer.label, "trivial");
        assert_eq!(answer.p, 0.95);
    }

    #[test]
    fn gate_with_retries_once_on_a_bad_label() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let provider = FakeProvider::new(vec![
            r#"{"label": "bogus", "p": 0.9, "reason": "r"}"#.to_string(),
            r#"{"label": "substantive", "p": 0.6, "reason": "changes meaning"}"#.to_string(),
        ]);
        let answer = gate_with(&provider, &cfg, &q()).unwrap();
        assert_eq!(answer.label, "substantive");
    }

    #[test]
    fn gate_returns_none_without_a_configured_provider() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(gate(&cfg, &q()).unwrap().is_none());
    }
}
