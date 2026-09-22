//! openai chat completions adapter. also serves any openai-compatible server (local or
//! hosted) through a `base_url` override, which is how many local servers are used.
//! design.md §19.

use super::{Block, CompletionRequest, CompletionResponse, Provider, Role, Usage, truncate};
use crate::config::ProviderConfig;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

pub struct OpenAiProvider {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let env_name = cfg.api_key_env.clone().unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
        let api_key = std::env::var(&env_name)
            .with_context(|| format!("openai provider: environment variable {env_name} is not set"))?;
        Ok(Self {
            client: super::http_client(),
            base_url: cfg.base_url.clone().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            api_key,
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

/// build the `/chat/completions` request body. pure so it can be unit-tested without network.
/// `system` becomes a leading `system` role message; `max_tokens` is sent as
/// `max_completion_tokens`, the current chat completions field name.
pub fn build_body(model: &str, req: &CompletionRequest) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for m in &req.messages {
        messages.push(json!({ "role": role_str(m.role), "content": m.content }));
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_completion_tokens": req.max_tokens,
    });
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(schema) = &req.json_schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": { "name": "output", "schema": schema, "strict": false },
        });
    }
    body
}

/// map a `/chat/completions` response body into a `CompletionResponse`. pure so it can be
/// unit-tested against fixture json without network. `choices[0].message.content` becomes a
/// `Text` block; `reasoning_content`, when present, becomes a `Reasoning` block.
pub fn map_response(raw: Value) -> CompletionResponse {
    let choice = raw["choices"].get(0).cloned().unwrap_or(Value::Null);
    let message = choice["message"].clone();
    let stop_reason = choice["finish_reason"].as_str().unwrap_or_default().to_string();
    let usage = Usage {
        input_tokens: raw["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: raw["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    };
    let mut blocks = Vec::new();
    if let Some(text) = message["content"].as_str() {
        blocks.push(Block::Text { text: text.to_string() });
    }
    if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str) {
        blocks.push(Block::Reasoning {
            text: Some(reasoning.to_string()),
            opaque: message["reasoning_content"].clone(),
            provider: "openai".to_string(),
        });
    }
    CompletionResponse { blocks, usage, stop_reason, raw }
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse> {
        let body = build_body(&self.model, req);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("openai request failed to send")?;
        let status = resp.status();
        let text = resp.text().context("reading openai response body")?;
        if !status.is_success() {
            bail!("openai request failed: {status} {}", truncate(&text, 500));
        }
        let raw: Value = serde_json::from_str(&text).context("openai response is not valid json")?;
        Ok(map_response(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn build_body_puts_system_first_and_uses_max_completion_tokens() {
        let req = CompletionRequest {
            system: Some("be terse".into()),
            messages: vec![Message { role: Role::User, content: "hi".into() }],
            max_tokens: 100,
            temperature: Some(0.5),
            json_schema: None,
        };
        let body = build_body("gpt-5", &req);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["max_completion_tokens"], 100);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be terse");
        assert_eq!(body["messages"][1]["role"], "user");
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn build_body_adds_json_schema_response_format() {
        let req = CompletionRequest {
            system: None,
            messages: vec![],
            max_tokens: 10,
            temperature: None,
            json_schema: Some(json!({ "type": "object" })),
        };
        let body = build_body("gpt-5", &req);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "output");
        assert_eq!(body["response_format"]["json_schema"]["strict"], false);
        assert_eq!(body["response_format"]["json_schema"]["schema"]["type"], "object");
    }

    #[test]
    fn map_response_reads_content_and_reasoning() {
        let raw = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": "the answer", "reasoning_content": "steps taken" }
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 7 }
        });
        let resp = map_response(raw);
        assert_eq!(resp.stop_reason, "stop");
        assert_eq!(resp.usage.input_tokens, 5);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            Block::Text { text } => assert_eq!(text, "the answer"),
            other => panic!("expected text block, got {other:?}"),
        }
        match &resp.blocks[1] {
            Block::Reasoning { text, provider, .. } => {
                assert_eq!(text.as_deref(), Some("steps taken"));
                assert_eq!(provider, "openai");
            }
            other => panic!("expected reasoning block, got {other:?}"),
        }
    }

    /// hits the live openai api (or an openai-compatible endpoint via base_url). requires
    /// KB_LIVE_PROVIDER_TESTS=1 and OPENAI_API_KEY.
    /// run with: KB_LIVE_PROVIDER_TESTS=1 cargo test -p kb --lib provider::openai::tests::live_completion -- --ignored
    #[test]
    #[ignore = "hits the live openai api; see the doc comment for how to run it"]
    fn live_completion() {
        if std::env::var("KB_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping: set KB_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }
        if std::env::var("OPENAI_API_KEY").is_err() {
            eprintln!("skipping: OPENAI_API_KEY is not set");
            return;
        }
        let cfg = ProviderConfig {
            kind: "openai".into(),
            model: std::env::var("KB_OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            base_url: std::env::var("OPENAI_BASE_URL").ok(),
            api_key_env: None,
            max_tokens: Some(16),
        };
        let provider = OpenAiProvider::new(&cfg).unwrap();
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
