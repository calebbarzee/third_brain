//! the `classify` hook: suggest a document's `layer` and `tags` from its title and body.
//! fills only empty frontmatter fields; a malformed or out-of-schema answer is retried, and a
//! missing provider is not an error (design.md §6, §16, §19).

use super::structured::complete_json;
use super::{CompletionRequest, Message, Role, provider_for_role};
use crate::config::Config;
use crate::model::{Document, Frontmatter, Layer};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");
const DEFAULT_CLASSIFY: &str = include_str!("../../prompts/classify.md");

/// the tag shape required by the prompt and enforced by `validate_classify`.
const TAG_PATTERN: &str = r"^[a-z0-9][a-z0-9-]{0,31}$";
const MAX_TAGS: usize = 6;
const MAX_BODY_CHARS: usize = 6_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifyOutput {
    pub layer: Layer,
    pub tags: Vec<String>,
    pub confidence: f64,
    pub reason: String,
}

/// suggest `layer` and `tags` for `doc`. `neighbors` are titles of existing documents, used to
/// steer tag vocabulary; may be empty. returns `Ok(None)` when no provider is configured for
/// role `classify` -- the deferred case, not an error (design.md §16).
pub fn classify(cfg: &Config, doc: &Document, neighbors: &[String]) -> Result<Option<ClassifyOutput>> {
    let Some(provider) = provider_for_role(cfg, "classify")? else {
        return Ok(None);
    };

    let style = load_prompt(&cfg.root, "style.md", DEFAULT_STYLE)?;
    let classify_prompt = load_prompt(&cfg.root, "classify.md", DEFAULT_CLASSIFY)?;
    let system = format!("{style}\n\n{classify_prompt}");
    let user = build_user_message(doc, neighbors);

    let max_tokens = cfg.kb.providers.get("classify").and_then(|pc| pc.max_tokens).unwrap_or(1024);
    let req = CompletionRequest {
        system: Some(system),
        messages: vec![Message { role: Role::User, content: user }],
        max_tokens,
        temperature: Some(0.0),
        json_schema: Some(classify_schema()),
    };

    let retries = cfg.manifest.thresholds.classify_retries;
    let structured = complete_json::<ClassifyOutput>(provider.as_ref(), req, validate_classify, retries)?;
    Ok(Some(structured.value))
}

/// fill only empty `layer` and `tags` fields from `out`; returns which fields it filled, for
/// the `Suggested:` commit trailer (design.md §6).
pub fn apply_classification(fm: &mut Frontmatter, out: &ClassifyOutput) -> Vec<&'static str> {
    let mut filled = Vec::new();
    if fm.layer.is_none() {
        fm.layer = Some(out.layer);
        filled.push("layer");
    }
    if fm.tags.is_empty() {
        fm.tags = out.tags.clone();
        filled.push("tags");
    }
    filled
}

fn build_user_message(doc: &Document, neighbors: &[String]) -> String {
    let title = doc.frontmatter.title.as_deref().unwrap_or("(untitled)");
    let mut user = format!("title: {title}\n");
    if !neighbors.is_empty() {
        user.push_str("existing document titles (for tag vocabulary):\n");
        for n in neighbors {
            user.push_str(&format!("- {n}\n"));
        }
    }
    user.push_str("\nbody:\n");
    user.push_str(&super::truncate(&doc.body, MAX_BODY_CHARS));
    user.push_str("\n\noutput only the json object.");
    user
}

/// load `prompts/<name>` from the repository root if present, else fall back to the crate's
/// own default, embedded at compile time.
fn load_prompt(root: &Path, name: &str, default: &'static str) -> Result<String> {
    let path = root.join("prompts").join(name);
    if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    } else {
        Ok(default.to_string())
    }
}

fn classify_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "layer": { "type": "string", "enum": ["principle", "design", "decision", "procedure", "reference"] },
            "tags": {
                "type": "array",
                "items": { "type": "string", "pattern": TAG_PATTERN },
                "minItems": 1,
                "maxItems": MAX_TAGS,
            },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
            "reason": { "type": "string" },
        },
        "required": ["layer", "tags", "confidence", "reason"],
        "additionalProperties": false,
    })
}

fn validate_classify(value: &Value) -> Result<(), String> {
    let layer = value.get("layer").and_then(Value::as_str).ok_or("missing string field \"layer\"")?;
    layer.parse::<Layer>().map_err(|e| format!("invalid layer: {e}"))?;

    let tags = value.get("tags").and_then(Value::as_array).ok_or("missing array field \"tags\"")?;
    if tags.is_empty() {
        return Err("tags must have at least one entry".to_string());
    }
    if tags.len() > MAX_TAGS {
        return Err(format!("tags must have at most {MAX_TAGS} entries"));
    }
    let tag_re = Regex::new(TAG_PATTERN).expect("TAG_PATTERN is a valid regex");
    for t in tags {
        let s = t.as_str().ok_or("tags must be strings")?;
        if !tag_re.is_match(s) {
            return Err(format!("tag \"{s}\" is not lowercase kebab-case matching {TAG_PATTERN}"));
        }
    }

    let confidence = value.get("confidence").and_then(Value::as_f64).ok_or("missing numeric field \"confidence\"")?;
    if !(0.0..=1.0).contains(&confidence) {
        return Err(format!("confidence {confidence} is not within 0..1"));
    }

    if value.get("reason").and_then(Value::as_str).is_none() {
        return Err("missing string field \"reason\"".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    fn valid_value() -> Value {
        json!({ "layer": "procedure", "tags": ["auth", "platform"], "confidence": 0.8, "reason": "matches existing procedure docs" })
    }

    #[test]
    fn validate_accepts_good_output() {
        assert!(validate_classify(&valid_value()).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_layer() {
        let mut v = valid_value();
        v["layer"] = json!("unknown");
        assert!(validate_classify(&v).is_err());
    }

    #[test]
    fn validate_rejects_too_many_tags() {
        let mut v = valid_value();
        v["tags"] = json!(["a", "b", "c", "d", "e", "f", "g"]);
        let err = validate_classify(&v).unwrap_err();
        assert!(err.contains("at most 6"));
    }

    #[test]
    fn validate_rejects_empty_tags() {
        let mut v = valid_value();
        v["tags"] = json!([]);
        assert!(validate_classify(&v).is_err());
    }

    #[test]
    fn validate_rejects_bad_tag_shape() {
        let mut v = valid_value();
        v["tags"] = json!(["Not-Kebab"]);
        assert!(validate_classify(&v).is_err());
        let mut v2 = valid_value();
        v2["tags"] = json!(["-leading-dash"]);
        assert!(validate_classify(&v2).is_err());
    }

    #[test]
    fn validate_rejects_confidence_out_of_range() {
        let mut v = valid_value();
        v["confidence"] = json!(1.5);
        assert!(validate_classify(&v).is_err());
    }

    #[test]
    fn validate_rejects_missing_reason() {
        let mut v = valid_value();
        v.as_object_mut().unwrap().remove("reason");
        assert!(validate_classify(&v).is_err());
    }

    #[test]
    fn apply_classification_fills_only_empty_fields() {
        let out = ClassifyOutput {
            layer: Layer::Procedure,
            tags: vec!["auth".to_string()],
            confidence: 0.9,
            reason: "r".to_string(),
        };

        let mut empty = Frontmatter::default();
        let filled = apply_classification(&mut empty, &out);
        assert_eq!(filled, vec!["layer", "tags"]);
        assert_eq!(empty.layer, Some(Layer::Procedure));
        assert_eq!(empty.tags, vec!["auth".to_string()]);

        let mut partial = Frontmatter { layer: Some(Layer::Decision), status: Some(Status::Draft), ..Default::default() };
        let filled = apply_classification(&mut partial, &out);
        assert_eq!(filled, vec!["tags"]);
        assert_eq!(partial.layer, Some(Layer::Decision), "existing layer must not be overwritten");
        assert_eq!(partial.tags, vec!["auth".to_string()]);

        let mut full = Frontmatter { layer: Some(Layer::Decision), tags: vec!["x".to_string()], ..Default::default() };
        let filled = apply_classification(&mut full, &out);
        assert!(filled.is_empty());
        assert_eq!(full.tags, vec!["x".to_string()]);
    }

    #[test]
    fn classify_returns_none_without_a_configured_provider() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let doc = crate::parse::parse_document("# a title\n\nbody text\n").unwrap();
        assert!(classify(&cfg, &doc, &[]).unwrap().is_none());
    }
}
