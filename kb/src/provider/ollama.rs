//! ollama chat adapter, for a local model. design.md §19. needs no api key.

use super::{Block, CompletionRequest, CompletionResponse, Provider, Role, Usage, truncate};
use crate::config::ProviderConfig;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

pub struct OllamaProvider {
    client: reqwest::blocking::Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            client: super::http_client(),
            base_url: cfg.base_url.clone().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            model: cfg.model.clone(),
        })
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// build the `/api/chat` request body. pure so it can be unit-tested without network.
/// `json_schema`, when given, is passed through as ollama's `format` field, which accepts a
/// json schema object directly.
pub fn build_body(model: &str, req: &CompletionRequest) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for m in &req.messages {
        messages.push(json!({ "role": role_str(m.role), "content": m.content }));
    }
    let mut options = json!({ "num_predict": req.max_tokens });
    if let Some(t) = req.temperature {
        options["temperature"] = json!(t);
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        "options": options,
    });
    if let Some(schema) = &req.json_schema {
        body["format"] = schema.clone();
    }
    body
}

/// map an `/api/chat` response body into a `CompletionResponse`. pure so it can be
/// unit-tested against fixture json without network. `message.thinking`, when present,
/// becomes a `Reasoning` block.
pub fn map_response(raw: Value) -> CompletionResponse {
    let message = raw["message"].clone();
    let stop_reason = raw["done_reason"].as_str().unwrap_or_default().to_string();
    let usage = Usage {
        input_tokens: raw["prompt_eval_count"].as_u64().unwrap_or(0),
        output_tokens: raw["eval_count"].as_u64().unwrap_or(0),
    };
    let mut blocks = Vec::new();
    if let Some(text) = message["content"].as_str() {
        blocks.push(Block::Text { text: text.to_string() });
    }
    if let Some(thinking) = message.get("thinking").and_then(Value::as_str) {
        blocks.push(Block::Reasoning {
            text: Some(thinking.to_string()),
            opaque: message["thinking"].clone(),
            provider: "ollama".to_string(),
        });
    }
    CompletionResponse { blocks, usage, stop_reason, raw }
}

impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse> {
        let body = build_body(&self.model, req);
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("ollama request failed to send")?;
        let status = resp.status();
        let text = resp.text().context("reading ollama response body")?;
        if !status.is_success() {
            bail!("ollama request failed: {status} {}", truncate(&text, 500));
        }
        let raw: Value = serde_json::from_str(&text).context("ollama response is not valid json")?;
        Ok(map_response(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn build_body_sets_options_and_stream_false() {
        let req = CompletionRequest {
            system: Some("be terse".into()),
            messages: vec![Message { role: Role::User, content: "hi".into() }],
            max_tokens: 64,
            temperature: Some(0.1),
            json_schema: None,
        };
        let body = build_body("llama3.2:3b", &req);
        assert_eq!(body["model"], "llama3.2:3b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["num_predict"], 64);
        assert_eq!(body["options"]["temperature"].as_f64().unwrap(), 0.1_f32 as f64);
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body.get("format").is_none());
    }

    #[test]
    fn build_body_passes_schema_as_format() {
        let req = CompletionRequest {
            system: None,
            messages: vec![],
            max_tokens: 10,
            temperature: None,
            json_schema: Some(json!({ "type": "object", "properties": {} })),
        };
        let body = build_body("llama3.2:3b", &req);
        assert_eq!(body["format"]["type"], "object");
    }

    #[test]
    fn map_response_reads_content_and_thinking() {
        let raw = json!({
            "done_reason": "stop",
            "prompt_eval_count": 3,
            "eval_count": 9,
            "message": { "role": "assistant", "content": "the answer", "thinking": "steps taken" }
        });
        let resp = map_response(raw);
        assert_eq!(resp.stop_reason, "stop");
        assert_eq!(resp.usage.input_tokens, 3);
        assert_eq!(resp.usage.output_tokens, 9);
        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[1] {
            Block::Reasoning { text, provider, .. } => {
                assert_eq!(text.as_deref(), Some("steps taken"));
                assert_eq!(provider, "ollama");
            }
            other => panic!("expected reasoning block, got {other:?}"),
        }
    }

    /// hits a local ollama server. requires KB_LIVE_PROVIDER_TESTS=1 and a running ollama with
    /// a pulled model (set KB_OLLAMA_MODEL to override the default).
    /// run with: KB_LIVE_PROVIDER_TESTS=1 cargo test -p kb --lib provider::ollama::tests::live_completion -- --ignored
    #[test]
    #[ignore = "hits a local ollama server; see the doc comment for how to run it"]
    fn live_completion() {
        if std::env::var("KB_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping: set KB_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }
        let cfg = ProviderConfig {
            kind: "ollama".into(),
            model: std::env::var("KB_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2:3b".into()),
            base_url: None,
            api_key_env: None,
            max_tokens: Some(16),
        };
        let provider = OllamaProvider::new(&cfg).unwrap();
        let resp = provider
            .complete(&CompletionRequest {
                system: None,
                messages: vec![Message { role: Role::User, content: "reply with the single word: ok".into() }],
                max_tokens: 16,
                temperature: Some(0.0),
                json_schema: None,
            })
            .unwrap();
        let text = super::super::first_text(&resp).unwrap_or_default();
        assert!(!text.is_empty(), "expected a non-empty text reply, got {resp:?}");
    }
}
