//! llm provider trait and adapters. design.md §19: hooks never call a vendor sdk directly;
//! they build a request, hand it to the provider configured for their role, and process the
//! response. blocks are text, tool use, tool result, and reasoning with an opaque provider
//! payload; this build implements text and reasoning. the cli is synchronous, so this trait
//! is blocking (`reqwest::blocking`).

pub mod anthropic;
pub mod classify;
pub mod claude_code;
pub mod command;
pub mod gate;
pub mod ollama;
pub mod openai;
pub mod structured;
mod subprocess;

use crate::config::Config;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// who wrote one message in a completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// one request to a provider. `json_schema` is a hint for native structured output where the
/// provider supports it; every adapter also tolerates a plain text reply and structured.rs
/// extracts and validates the json from it either way.
#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub json_schema: Option<serde_json::Value>,
}

/// one block of a completion response. reasoning's `opaque` carries the provider's block
/// verbatim (json) so it can round-trip byte for byte on a later turn, per appendix b of
/// design.md: modified reasoning blocks are rejected by at least one provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Block {
    Text { text: String },
    Reasoning { text: Option<String>, opaque: serde_json::Value, provider: String },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// a completion response. `raw` is the whole provider response body, stored always (design.md
/// §19: "store the raw response always").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub blocks: Vec<Block>,
    pub usage: Usage,
    pub stop_reason: String,
    pub raw: serde_json::Value,
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse>;

    /// embed a batch of texts, one vector per input, in order. design.md §19's `embed` role.
    /// the default is "not supported"; `supports_embed` lets a caller check first instead of
    /// paying for a request that will only fail.
    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        bail!("{} provider does not support embed", self.name())
    }

    /// whether `embed` is implemented for this provider.
    fn supports_embed(&self) -> bool {
        false
    }

    /// cross-encoder rerank: a relevance score per candidate, same order as `candidates`, given
    /// `query`. design.md §19's `rerank` role; not implemented by any adapter yet (design.md
    /// §32: "rerank model, hosted or local | | after step 5"), so the default always errs and
    /// retrieval checks `supports_rerank` first and skips the step rather than calling this.
    fn rerank(&self, _query: &str, _candidates: &[String]) -> Result<Vec<f64>> {
        bail!("{} provider does not support rerank", self.name())
    }

    /// whether `rerank` is implemented for this provider.
    fn supports_rerank(&self) -> bool {
        false
    }
}

/// build the provider configured for `role` in `cfg.kb.providers`. `Ok(None)` means no
/// provider is configured for the role; callers treat that as the deferred-prompt case
/// (design.md §16) rather than an error.
pub fn provider_for_role(cfg: &Config, role: &str) -> Result<Option<Box<dyn Provider>>> {
    let Some(pc) = cfg.kb.providers.get(role) else {
        return Ok(None);
    };
    let inner = build_provider(role, pc)?;
    Ok(Some(Box::new(Traced {
        inner,
        role: role.to_string(),
        model: pc.model.clone(),
        state_dir: cfg.state_dir(),
        actor: format!("human:{}", crate::server::auth::local_principal(cfg)),
    })))
}

fn build_provider(role: &str, pc: &crate::config::ProviderConfig) -> Result<Box<dyn Provider>> {
    let provider: Box<dyn Provider> = match pc.kind.as_str() {
        "anthropic" => Box::new(anthropic::AnthropicProvider::new(pc)?),
        "openai" => Box::new(openai::OpenAiProvider::new(pc)?),
        "ollama" => Box::new(ollama::OllamaProvider::new(pc)?),
        "claude-code" => Box::new(claude_code::ClaudeCodeProvider::new(pc)?),
        "command" => Box::new(command::CommandProvider::new(pc)?),
        other => bail!(
            "role \"{role}\": unknown provider kind \"{other}\" (expected anthropic, openai, ollama, claude-code, or command)"
        ),
    };
    Ok(provider)
}

/// a provider that writes every completion, request and response in full, to the transcripts
/// table (`crate::transcript`). embed and rerank pass through unrecorded: they carry vectors and
/// scores, not conversation.
struct Traced {
    inner: Box<dyn Provider>,
    role: String,
    model: String,
    state_dir: std::path::PathBuf,
    actor: String,
}

impl Provider for Traced {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse> {
        let started = crate::clock::now();
        let clock = std::time::Instant::now();
        let result = self.inner.complete(req);
        let request = serde_json::json!({
            "system": req.system, "messages": req.messages, "max_tokens": req.max_tokens,
            "temperature": req.temperature, "json_schema": req.json_schema,
        });
        let (response, error, usage) = match &result {
            Ok(resp) => (serde_json::to_value(resp).ok(), None, Some(resp.usage)),
            Err(e) => (None, Some(format!("{e:#}")), None),
        };
        let entry = crate::transcript::Entry {
            kind: "llm",
            role: &self.role,
            provider: Some(self.inner.name()),
            model: (!self.model.is_empty()).then_some(self.model.as_str()),
            actor: &self.actor,
            request,
            response,
            error,
            started,
            duration_ms: clock.elapsed().as_millis() as u64,
            input_tokens: usage.map(|u| u.input_tokens),
            output_tokens: usage.map(|u| u.output_tokens),
        };
        if let Err(e) = crate::transcript::record(&self.state_dir, entry) {
            eprintln!("warning: could not record the {} transcript: {e:#}", self.role);
        }
        result
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.inner.embed(texts)
    }

    fn supports_embed(&self) -> bool {
        self.inner.supports_embed()
    }

    fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<f64>> {
        self.inner.rerank(query, candidates)
    }

    fn supports_rerank(&self) -> bool {
        self.inner.supports_rerank()
    }
}

/// the first text block in a response, if any.
pub fn first_text(resp: &CompletionResponse) -> Option<&str> {
    resp.blocks.iter().find_map(|b| match b {
        Block::Text { text } => Some(text.as_str()),
        Block::Reasoning { .. } => None,
    })
}

/// truncate `s` to at most `max_chars` characters, for embedding a response body in an error
/// without growing error messages unboundedly. never used on request headers or api keys.
pub(crate) fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}... [truncated]")
    }
}

/// http client with a generous timeout for slow local models. `KB_PROVIDER_TIMEOUT_SECS` overrides (default 600).
pub fn http_client() -> reqwest::blocking::Client {
    let secs = std::env::var("KB_PROVIDER_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600u64);
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(secs))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// a scripted provider for tests: returns each response in order, never touches the network.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    pub struct FakeProvider {
        responses: Mutex<std::vec::IntoIter<String>>,
    }

    impl FakeProvider {
        pub fn new(responses: Vec<String>) -> Self {
            Self { responses: Mutex::new(responses.into_iter()) }
        }
    }

    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse> {
            let text = self
                .responses
                .lock()
                .expect("fake provider mutex poisoned")
                .next()
                .ok_or_else(|| anyhow::anyhow!("fake provider script exhausted"))?;
            Ok(CompletionResponse {
                blocks: vec![Block::Text { text }],
                usage: Usage::default(),
                stop_reason: "end_turn".to_string(),
                raw: serde_json::json!({}),
            })
        }
    }
}
