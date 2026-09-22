//! the `resolve` hook (design.md §14): given a rebase conflict on one path, ask a model for a
//! merged file, validate it, and decide whether it still needs a human.

use super::udiff::render_unified;
use crate::config::Config;
use crate::git;
use crate::parse::parse_document;
use crate::provider::structured::complete_json;
use crate::provider::{CompletionRequest, Message, Role, provider_for_role};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");
const DEFAULT_RESOLVE: &str = include_str!("../../prompts/resolve.md");

/// everything a resolver sub-run needs for one conflicted path (design.md §14).
#[derive(Debug, Clone)]
pub struct ConflictInput {
    pub path: String,
    pub base_text: String,
    pub ours_text: String,
    pub theirs_text: String,
    /// the session's settled message.
    pub ours_message: String,
    /// messages of `main` commits touching `path` since base, oldest first.
    pub theirs_messages: Vec<String>,
}

/// gather a conflict packet for `path` from git: the file at `base_sha`/`ours_sha`/`theirs_sha`,
/// `ours_sha`'s full commit message, and the messages of every `theirs` commit since `base_sha`
/// that touched `path`. a side missing the path (created on the other side) reads as empty text.
pub fn gather_conflict(repo: &Path, path: &str, base_sha: &str, ours_sha: &str, theirs_sha: &str) -> Result<ConflictInput> {
    let base_text = String::from_utf8_lossy(&git::show_file(repo, base_sha, path).unwrap_or_default()).into_owned();
    let ours_text = String::from_utf8_lossy(&git::show_file(repo, ours_sha, path).unwrap_or_default()).into_owned();
    let theirs_text = String::from_utf8_lossy(&git::show_file(repo, theirs_sha, path).unwrap_or_default()).into_owned();
    let ours_message = git::commit_message(repo, ours_sha)?;
    let theirs_messages = git::log_messages_for_path(repo, base_sha, theirs_sha, path)?;
    Ok(ConflictInput { path: path.to_string(), base_text, ours_text, theirs_text, ours_message, theirs_messages })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolveOutput {
    /// the full resolved file (frontmatter plus body), ready to write and `git add`.
    pub content: String,
    pub confidence: f64,
    /// true when the two edits cannot both be true (design.md §14): forces escalation.
    pub contradiction: bool,
    pub summary: String,
}

/// run the resolver over `input`. `Ok(None)` when no provider is configured for role `resolve`
/// -- the deferred case (design.md §16), not an error.
pub fn resolve(cfg: &Config, input: &ConflictInput) -> Result<Option<ResolveOutput>> {
    let Some(provider) = provider_for_role(cfg, "resolve")? else {
        return Ok(None);
    };

    let style = load_prompt(&cfg.root, "style.md", DEFAULT_STYLE)?;
    let resolve_prompt = load_prompt(&cfg.root, "resolve.md", DEFAULT_RESOLVE)?;
    let system = format!("{style}\n\n{resolve_prompt}");
    let user = build_user_message(input);

    let max_tokens = cfg.kb.providers.get("resolve").and_then(|pc| pc.max_tokens).unwrap_or(4096);
    let req = CompletionRequest {
        system: Some(system),
        messages: vec![Message { role: Role::User, content: user }],
        max_tokens,
        temperature: Some(0.0),
        json_schema: Some(resolve_schema()),
    };

    // no dedicated `resolve_retries` threshold exists in manifest.toml; `resolve` and `review`
    // are both content-review-adjacent hooks with a two-retry default, so this reuses
    // `review_retries` rather than inventing a new config knob (see this crate's report).
    let retries = cfg.manifest.thresholds.review_retries;
    let ours_id = parse_document(&input.ours_text).ok().and_then(|d| d.frontmatter.id);
    let structured = complete_json::<ResolveOutput>(provider.as_ref(), req, |v| validate_resolve(v, ours_id.as_deref()), retries)?;
    Ok(Some(structured.value))
}

/// per design.md §14 and §30's "conflict escalation" defaults: contradiction, confidence under
/// 0.6, more than 5 conflicting paths, a repeat on this path, or a `locked` layer all force
/// escalation to a human, keeping the worktree mid-rebase. returns the first reason that
/// applies, in the order design.md lists them, or `None` when the resolution may proceed.
pub fn should_escalate(out: &ResolveOutput, paths_conflicting: usize, is_locked_layer: bool, repeat: bool) -> Option<String> {
    if out.contradiction {
        return Some("the two edits cannot both be true".to_string());
    }
    if out.confidence < 0.6 {
        return Some(format!("confidence {:.2} is below 0.6", out.confidence));
    }
    if paths_conflicting > 5 {
        return Some(format!("{paths_conflicting} paths conflict, more than 5"));
    }
    if repeat {
        return Some("this path conflicted again after a retry".to_string());
    }
    if is_locked_layer {
        return Some("the document's layer is locked".to_string());
    }
    None
}

fn build_user_message(input: &ConflictInput) -> String {
    let base_ours = render_unified(&format!("{} (base)", input.path), &format!("{} (ours)", input.path), &input.base_text, &input.ours_text);
    let base_theirs = render_unified(&format!("{} (base)", input.path), &format!("{} (theirs)", input.path), &input.base_text, &input.theirs_text);
    let ours_theirs = render_unified(&format!("{} (ours)", input.path), &format!("{} (theirs)", input.path), &input.ours_text, &input.theirs_text);
    let theirs_messages = if input.theirs_messages.is_empty() { "(none)".to_string() } else { input.theirs_messages.join("\n---\n") };
    format!(
        "path: {}\n\nours message:\n{}\n\ntheirs messages (main commits touching this path since base):\n{}\n\ndiff base -> ours:\n{}\n\ndiff base -> theirs:\n{}\n\ndiff ours -> theirs:\n{}\n\noutput only the json object.",
        input.path, input.ours_message, theirs_messages, base_ours, base_theirs, ours_theirs
    )
}

fn load_prompt(root: &Path, name: &str, default: &'static str) -> Result<String> {
    let path = root.join("prompts").join(name);
    if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    } else {
        Ok(default.to_string())
    }
}

fn resolve_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "content": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
            "contradiction": { "type": "boolean" },
            "summary": { "type": "string" },
        },
        "required": ["content", "confidence", "contradiction", "summary"],
        "additionalProperties": false,
    })
}

/// content must parse as a document and keep `ours`'s frontmatter id (a resolver may never
/// rename a document); confidence must be a proportion.
fn validate_resolve(value: &Value, ours_id: Option<&str>) -> Result<(), String> {
    let content = value.get("content").and_then(Value::as_str).ok_or("missing string field \"content\"")?;
    let doc = parse_document(content).map_err(|e| format!("\"content\" does not parse as a document: {e}"))?;
    if let Some(expected) = ours_id {
        match doc.frontmatter.id.as_deref() {
            Some(id) if id == expected => {}
            Some(other) => return Err(format!("\"content\"'s frontmatter id `{other}` does not match ours's id `{expected}`")),
            None => return Err(format!("\"content\"'s frontmatter is missing `id` (expected `{expected}`)")),
        }
    }

    let confidence = value.get("confidence").and_then(Value::as_f64).ok_or("missing numeric field \"confidence\"")?;
    if !(0.0..=1.0).contains(&confidence) {
        return Err(format!("confidence {confidence} is not within 0..1"));
    }

    if value.get("contradiction").and_then(Value::as_bool).is_none() {
        return Err("missing boolean field \"contradiction\"".to_string());
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

    const ID: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0N";

    fn doc_text(id: &str, body: &str) -> String {
        format!("+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# t\n\n{body}\n")
    }

    fn setup_conflict_repo() -> (tempfile::TempDir, String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        gitmod::init_for_tests(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        let path = format!("docs/{ID}.md");
        std::fs::write(dir.path().join(&path), doc_text(ID, "base text")).unwrap();
        let base = gitmod::commit_all(dir.path(), "base").unwrap();

        std::fs::write(dir.path().join(&path), doc_text(ID, "ours text")).unwrap();
        let ours = gitmod::commit_all(dir.path(), "ours: rewrite for clarity").unwrap();

        // simulate "theirs" as a second branch from base without leaving the ours history,
        // since gather_conflict only reads blobs at named revisions.
        gitmod::run(dir.path(), &["checkout", "-q", &base]).unwrap();
        std::fs::write(dir.path().join(&path), doc_text(ID, "theirs text")).unwrap();
        gitmod::run(dir.path(), &["checkout", "-q", "-b", "theirs-branch"]).unwrap();
        let theirs = gitmod::commit_all(dir.path(), "theirs: update the number").unwrap();
        gitmod::run(dir.path(), &["checkout", "-q", &ours]).unwrap();

        (dir, base, ours, theirs)
    }

    #[test]
    fn gather_conflict_reads_both_sides_and_messages() {
        let (dir, base, ours, theirs) = setup_conflict_repo();
        let path = format!("docs/{ID}.md");
        let input = gather_conflict(dir.path(), &path, &base, &ours, &theirs).unwrap();
        assert!(input.base_text.contains("base text"));
        assert!(input.ours_text.contains("ours text"));
        assert!(input.theirs_text.contains("theirs text"));
        assert!(input.ours_message.contains("rewrite for clarity"));
        assert_eq!(input.theirs_messages.len(), 1);
        assert!(input.theirs_messages[0].contains("update the number"));
    }

    #[test]
    fn resolve_validation_rejects_a_changed_id() {
        let (dir, base, ours, theirs) = setup_conflict_repo();
        let path = format!("docs/{ID}.md");
        let input = gather_conflict(dir.path(), &path, &base, &ours, &theirs).unwrap();
        std::fs::write(dir.path().join("kb.toml"), "[providers.resolve]\nkind = \"ollama\"\nmodel = \"test\"\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();

        let other_id = "01K5A7Q2XW3ZRCM8T6YB4VJH0P";
        let bad = format!(
            r#"{{"content": {:?}, "confidence": 0.9, "contradiction": false, "summary": "merged"}}"#,
            doc_text(other_id, "merged text")
        );
        let good = format!(
            r#"{{"content": {:?}, "confidence": 0.9, "contradiction": false, "summary": "merged"}}"#,
            doc_text(ID, "merged text")
        );
        let provider = FakeProvider::new(vec![bad, good]);
        let user = build_user_message(&input);
        let ours_id = parse_document(&input.ours_text).ok().and_then(|d| d.frontmatter.id);
        let req = CompletionRequest {
            system: Some("system".to_string()),
            messages: vec![Message { role: Role::User, content: user }],
            max_tokens: 4096,
            temperature: Some(0.0),
            json_schema: Some(resolve_schema()),
        };
        let out = complete_json::<ResolveOutput>(&provider, req, |v| validate_resolve(v, ours_id.as_deref()), cfg.manifest.thresholds.review_retries).unwrap();
        assert_eq!(out.attempts, 2, "the id-changed answer must be rejected and retried");
        assert!(out.value.content.contains(ID));
    }

    #[test]
    fn resolve_returns_none_without_a_configured_provider() {
        let (dir, base, ours, theirs) = setup_conflict_repo();
        let path = format!("docs/{ID}.md");
        let input = gather_conflict(dir.path(), &path, &base, &ours, &theirs).unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(resolve(&cfg, &input).unwrap().is_none());
    }

    fn out(confidence: f64, contradiction: bool) -> ResolveOutput {
        ResolveOutput { content: doc_text(ID, "x"), confidence, contradiction, summary: "s".into() }
    }

    #[test]
    fn should_escalate_covers_every_documented_case() {
        assert!(should_escalate(&out(0.9, true), 1, false, false).is_some(), "contradiction escalates");
        assert!(should_escalate(&out(0.5, false), 1, false, false).is_some(), "confidence under 0.6 escalates");
        assert!(should_escalate(&out(0.9, false), 6, false, false).is_some(), "more than 5 paths escalates");
        assert!(should_escalate(&out(0.9, false), 1, false, true).is_some(), "a repeat on one path escalates");
        assert!(should_escalate(&out(0.9, false), 1, true, false).is_some(), "a locked layer escalates");
        assert!(should_escalate(&out(0.9, false), 5, false, false).is_none(), "5 paths exactly does not escalate");
        assert!(should_escalate(&out(0.9, false), 1, false, false).is_none(), "a clean resolution proceeds");
    }
}
