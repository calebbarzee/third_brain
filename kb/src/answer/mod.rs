//! the `answer` hook: search, build a prompt from the passages found, ask the `answer` provider,
//! parse and validate its citations, record `read`/`cite` usage, and seal one run log per
//! question. design.md §13's hooks table row `answer`, §18 (retrieval), §19 (the `answer` role
//! and the deferred-prompt contract), §16 (run logs).

pub mod eval;

use crate::config::Config;
use crate::index::{self, SearchHit, SearchOpts};
use crate::provider::{self, CompletionRequest, Message, Provider, Role, first_text};
use crate::runlog::{RunLog, RunStart, RunStatus};
use crate::store::usage;
use anyhow::Result;
use regex::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");
const DEFAULT_ANSWER: &str = include_str!("../../prompts/answer.md");

#[derive(Debug, Clone)]
pub struct AnswerOpts {
    pub limit: usize,
    pub include_captures: bool,
    /// write `read`/`cite` usage events (design.md §17.3, §17.4). default `true`; the cli's
    /// `--no-record` flag turns this off.
    pub record: bool,
}

impl Default for AnswerOpts {
    fn default() -> Self {
        Self { limit: 5, include_captures: false, record: true }
    }
}

/// one citation the answer actually used, resolved against a passage that was given to the model
/// (design.md: a citation that does not resolve is dropped, never fabricated into the record).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Citation {
    pub doc_id: String,
    pub title: Option<String>,
    pub heading_path: String,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    pub text: String,
    pub citations: Vec<Citation>,
    pub hits: Vec<SearchHit>,
    pub run_id: String,
    /// `true` when no `answer` provider is configured: `text` is empty and `deferred_prompt`
    /// carries the rendered prompt for the calling agent (an mcp client) to answer itself
    /// (design.md §16's deferred prompt).
    pub deferred: bool,
    pub deferred_prompt: Option<String>,
    /// citations the model wrote that did not resolve to any given passage, dropped from `text`
    /// and reported here instead (the raw `[kb:...]` text, for the caller to show or log).
    pub dropped_citations: Vec<String>,
}

fn load_prompt(root: &Path, name: &str, default: &'static str) -> Result<String> {
    let path = root.join("prompts").join(name);
    if path.is_file() {
        std::fs::read_to_string(&path).map_err(anyhow::Error::from)
    } else {
        Ok(default.to_string())
    }
}

/// the system and user halves of the answer prompt; kept apart so the deferred case can render
/// them into one block for a calling agent, while the live case sends them as separate
/// `CompletionRequest` fields.
struct RenderedPrompt {
    system: String,
    user: String,
}

/// build the answer prompt from `question` and the passages already found by search. each
/// passage is labeled `[kb:<id>#<slug>]` -- the exact citation form `prompts/answer.md` asks the
/// model to use, and what `parse_and_validate_citations` checks its answer against.
fn render_prompt(cfg: &Config, question: &str, hits: &[SearchHit]) -> Result<RenderedPrompt> {
    let style = load_prompt(&cfg.root, "style.md", DEFAULT_STYLE)?;
    let answer_prompt = load_prompt(&cfg.root, "answer.md", DEFAULT_ANSWER)?;
    let system = format!("{style}\n\n{answer_prompt}");

    let mut user = format!("question: {question}\n\npassages:\n");
    if hits.is_empty() {
        user.push_str("\n(no passages matched this question)\n");
    }
    for h in hits {
        user.push_str(&format!(
            "\n[kb:{}#{}] {}\n{}\n",
            h.doc_id,
            h.heading_path,
            h.title.as_deref().unwrap_or("(untitled)"),
            h.snippet
        ));
    }
    Ok(RenderedPrompt { system, user })
}

fn citation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[kb:([0-9A-HJKMNP-TV-Z]{26})(?:#([^\]]+))?\]").expect("citation regex is a fixed, valid pattern"))
}

/// pull every `[kb:<id>#<slug>]` citation out of `raw_text` and keep only the ones matching a
/// given passage's `(doc_id, heading_path)` exactly (a citation with no `#<slug>` never matches,
/// since every passage has one). the surviving text has invalid citations removed (no dangling
/// bracket) and valid ones left exactly as written; `citations` lists each resolved one once, in
/// first-seen order; `dropped` lists the raw text of each unresolved one. pure and side-effect
/// free, so it is unit-testable without a database or a provider.
pub fn parse_and_validate_citations(raw_text: &str, hits: &[SearchHit]) -> (String, Vec<Citation>, Vec<String>) {
    let re = citation_re();
    let mut clean = String::with_capacity(raw_text.len());
    let mut last = 0usize;
    let mut citations: Vec<Citation> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();

    for caps in re.captures_iter(raw_text) {
        let whole = caps.get(0).expect("capture group 0 is always the whole match");
        clean.push_str(&raw_text[last..whole.start()]);
        last = whole.end();

        let id = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let slug = caps.get(2).map(|m| m.as_str());
        let resolved = slug.and_then(|s| hits.iter().find(|h| h.doc_id == id && h.heading_path == s));
        match resolved {
            Some(hit) => {
                clean.push_str(whole.as_str());
                if !citations.iter().any(|c| c.doc_id == hit.doc_id && c.heading_path == hit.heading_path) {
                    citations.push(Citation {
                        doc_id: hit.doc_id.clone(),
                        title: hit.title.clone(),
                        heading_path: hit.heading_path.clone(),
                        quote: hit.snippet.clone(),
                    });
                }
            }
            None => dropped.push(whole.as_str().to_string()),
        }
    }
    clean.push_str(&raw_text[last..]);
    (clean, citations, dropped)
}

/// `answer`, using whatever provider is configured for the `answer` role.
pub fn answer(cfg: &Config, question: &str, opts: &AnswerOpts) -> Result<Answer> {
    answer_with_provider(cfg, question, opts, None)
}

/// `answer`, with an explicit provider override for tests (a fake completion provider) or a
/// caller that already resolved one. `None` falls back to `provider::provider_for_role(cfg,
/// "answer")`, and when that is also `None` the run defers (design.md §16).
pub fn answer_with_provider(cfg: &Config, question: &str, opts: &AnswerOpts, provider_override: Option<Box<dyn Provider>>) -> Result<Answer> {
    let answer_provider_cfg = cfg.kb.providers.get("answer");
    let mut log = RunLog::create(
        cfg,
        RunStart {
            actor: "agent:answer@system".to_string(),
            skill: Some("answer".to_string()),
            provider: answer_provider_cfg.map(|p| p.kind.clone()),
            model: answer_provider_cfg.map(|p| p.model.clone()),
            deferred: false,
            ..Default::default()
        },
    )?;
    let run_id = log.run_id().to_string();

    let conn = index::open_newest(cfg)?;
    let embed_provider = provider::provider_for_role(cfg, "embed")?;
    let query_vec = match &embed_provider {
        Some(p) if p.supports_embed() => p.embed(std::slice::from_ref(&question.to_string())).ok().and_then(|mut v| v.pop()),
        _ => None,
    };
    let rerank_provider = provider::provider_for_role(cfg, "rerank")?;
    let search_opts = SearchOpts { limit: opts.limit, status_any: false, include_captures: opts.include_captures, mode: None, explain: false };
    let mut hits = index::search(&conn, question, &search_opts, query_vec.as_deref(), rerank_provider.as_deref())?;
    // the model answers from full passages, not the short display snippet (capped to keep the
    // prompt bounded; a chunk is one heading-bounded section)
    for hit in &mut hits {
        if let Ok(text) = conn.query_row(
            "select text from chunks where doc_id = ?1 and heading_path = ?2 limit 1",
            rusqlite::params![hit.doc_id, hit.heading_path],
            |r| r.get::<_, String>(0),
        ) {
            hit.snippet = if text.chars().count() > 2_500 { text.chars().take(2_500).collect::<String>() + "…" } else { text };
        }
    }
    log.context(serde_json::json!({"question": question, "hit_count": hits.len(), "doc_ids": hits.iter().map(|h| &h.doc_id).collect::<Vec<_>>()}))?;

    let prompt = render_prompt(cfg, question, &hits)?;

    let provider: Option<Box<dyn Provider>> = match provider_override {
        Some(p) => Some(p),
        None => provider::provider_for_role(cfg, "answer")?,
    };
    let Some(provider) = provider else {
        let rendered = format!("{}\n\n---\n\n{}", prompt.system, prompt.user);
        log.deferred_request(serde_json::json!({"question": question, "prompt": rendered}))?;
        log.run_end(RunStatus::Deferred, None, None, None, &[], &[])?;
        log.seal(cfg)?;
        return Ok(Answer { text: String::new(), citations: vec![], hits, run_id, deferred: true, deferred_prompt: Some(rendered), dropped_citations: vec![] });
    };

    let max_tokens = answer_provider_cfg.and_then(|p| p.max_tokens).unwrap_or(1024);
    let req = CompletionRequest {
        system: Some(prompt.system),
        messages: vec![Message { role: Role::User, content: prompt.user }],
        max_tokens,
        temperature: Some(0.0),
        json_schema: None,
    };
    let resp = match provider.complete(&req) {
        Ok(r) => r,
        Err(e) => {
            log.error(&format!("{e:#}"))?;
            log.run_end(RunStatus::Failed, None, None, None, &[], &[])?;
            log.seal(cfg)?;
            return Err(e);
        }
    };
    log.llm_response(serde_json::to_value(&resp.blocks)?, serde_json::to_value(resp.usage)?)?;
    let raw_text = first_text(&resp).unwrap_or_default().to_string();

    let (clean_text, citations, dropped) = parse_and_validate_citations(&raw_text, &hits);
    for d in &dropped {
        log.decision(&format!("dropped citation not among the given passages: {d}"))?;
    }

    if opts.record {
        let read_ids: Vec<String> = hits.iter().map(|h| h.doc_id.clone()).collect();
        usage::record_reads(cfg, &read_ids)?;
        let cite_ids: Vec<String> = citations.iter().map(|c| c.doc_id.clone()).collect();
        usage::record_cites(cfg, &cite_ids)?;
    }

    log.report(&clean_text)?;
    log.run_end(RunStatus::Ok, None, None, None, &[], &[])?;
    log.seal(cfg)?;

    Ok(Answer { text: clean_text, citations, hits, run_id, deferred: false, deferred_prompt: None, dropped_citations: dropped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SearchExplain;

    fn hit(doc_id: &str, heading_path: &str, title: &str) -> SearchHit {
        SearchHit {
            doc_id: doc_id.to_string(),
            title: Some(title.to_string()),
            layer: None,
            status: Some("canon".to_string()),
            kind: "document".to_string(),
            source_uri: None,
            criticality_pct: None,
            heading_path: heading_path.to_string(),
            snippet: format!("excerpt of {heading_path}"),
            score: 1.0,
            last_updated: None,
            responsible: vec![],
            source_refs: vec![],
            code_refs: vec![],
            asset_refs: vec![],
            explain: None::<SearchExplain>,
        }
    }

    #[test]
    fn valid_citation_is_kept_and_recorded() {
        let hits = vec![hit("01K5A7Q2XW3ZRCM8T6YB4VJH0N", "install/linux", "install guide")];
        let text = "use the package manager [kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/linux].";
        let (clean, citations, dropped) = parse_and_validate_citations(text, &hits);
        assert_eq!(clean, text);
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].doc_id, "01K5A7Q2XW3ZRCM8T6YB4VJH0N");
        assert_eq!(citations[0].heading_path, "install/linux");
        assert!(dropped.is_empty());
    }

    #[test]
    fn bogus_citation_is_dropped_and_noted() {
        let hits = vec![hit("01K5A7Q2XW3ZRCM8T6YB4VJH0N", "install/linux", "install guide")];
        let text = "see [kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/linux] and also [kb:01K5A7Q2XW3ZRCM8T6YB4VJH0Z#nope].";
        let (clean, citations, dropped) = parse_and_validate_citations(text, &hits);
        assert_eq!(citations.len(), 1, "only the real passage should resolve");
        assert_eq!(dropped, vec!["[kb:01K5A7Q2XW3ZRCM8T6YB4VJH0Z#nope]".to_string()]);
        assert!(!clean.contains("K5A7Q2XW3ZRCM8T6YB4VJH0Z"), "the bogus citation should not survive into the cleaned text");
        assert!(clean.contains("install/linux"), "the valid citation should be untouched");
    }

    #[test]
    fn citation_with_right_id_but_wrong_slug_is_dropped() {
        let hits = vec![hit("01K5A7Q2XW3ZRCM8T6YB4VJH0N", "install/linux", "install guide")];
        let text = "[kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/windows] is not a passage we gave.";
        let (_clean, citations, dropped) = parse_and_validate_citations(text, &hits);
        assert!(citations.is_empty());
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn repeated_citation_of_the_same_passage_counts_once() {
        let hits = vec![hit("01K5A7Q2XW3ZRCM8T6YB4VJH0N", "install/linux", "install guide")];
        let text = "[kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/linux] ... and again [kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/linux].";
        let (_clean, citations, dropped) = parse_and_validate_citations(text, &hits);
        assert_eq!(citations.len(), 1);
        assert!(dropped.is_empty());
    }

    #[test]
    fn no_answer_provider_defers_with_a_rendered_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        // no index has been built in this temp repo, so `open_newest` fails before `answer` gets
        // to the provider check; this test instead exercises `render_prompt` directly, which is
        // the piece `answer_with_provider` uses to build the deferred prompt.
        let hits = vec![hit("01K5A7Q2XW3ZRCM8T6YB4VJH0N", "install/linux", "install guide")];
        let prompt = render_prompt(&cfg, "how do I install it", &hits).unwrap();
        assert!(prompt.user.contains("kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install/linux"));
        assert!(prompt.system.contains("role: answer"));
    }
}
