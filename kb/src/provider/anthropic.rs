//! anthropic messages api adapter. design.md §19.
//!
//! thinking is off by default; set `KB_ANTHROPIC_THINKING_BUDGET` (a token count) to enable
//! it, since `ProviderConfig` (owned by config.rs) has no field for it yet.

use super::{Block, CompletionRequest, CompletionResponse, Provider, Role, Usage, truncate};
use crate::config::ProviderConfig;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let env_name = cfg.api_key_env.clone().unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
        let api_key = std::env::var(&env_name)
            .with_context(|| format!("anthropic provider: environment variable {env_name} is not set"))?;
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

/// build the `/v1/messages` request body. pure so it can be unit-tested without network.
pub fn build_body(model: &str, req: &CompletionRequest) -> Value {
    let messages: Vec<Value> =
        req.messages.iter().map(|m| json!({ "role": role_str(m.role), "content": m.content })).collect();
    let mut body = json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "messages": messages,
    });
    if let Some(system) = &req.system {
        body["system"] = json!(system);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Ok(budget) = std::env::var("KB_ANTHROPIC_THINKING_BUDGET")
        && let Ok(budget_tokens) = budget.parse::<u32>() {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget_tokens });
        }
    body
}

/// map a `/v1/messages` response body into a `CompletionResponse`. pure so it can be
/// unit-tested against fixture json without network. `content[].type == "thinking"` becomes a
/// `Reasoning` block whose `opaque` is the whole block (including `signature`), so it
/// round-trips byte for byte on a later turn.
pub fn map_response(raw: Value) -> CompletionResponse {
    let stop_reason = raw["stop_reason"].as_str().unwrap_or_default().to_string();
    let usage = Usage {
        input_tokens: raw["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: raw["usage"]["output_tokens"].as_u64().unwrap_or(0),
    };
    let blocks = raw["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| match b["type"].as_str() {
            Some("text") => Some(Block::Text { text: b["text"].as_str().unwrap_or_default().to_string() }),
            Some("thinking") => Some(Block::Reasoning {
                text: b["thinking"].as_str().map(str::to_string),
                opaque: b.clone(),
                provider: "anthropic".to_string(),
            }),
            _ => None,
        })
        .collect();
    CompletionResponse { blocks, usage, stop_reason, raw }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse> {
        let body = build_body(&self.model, req);
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("anthropic request failed to send")?;
        let status = resp.status();
        let text = resp.text().context("reading anthropic response body")?;
        if !status.is_success() {
            bail!("anthropic request failed: {status} {}", truncate(&text, 500));
        }
        let raw: Value = serde_json::from_str(&text).context("anthropic response is not valid json")?;
        Ok(map_response(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn build_body_includes_system_and_temperature() {
        let req = CompletionRequest {
            system: Some("be terse".into()),
            messages: vec![Message { role: Role::User, content: "hi".into() }],
            max_tokens: 100,
            temperature: Some(0.2),
            json_schema: None,
        };
        let body = build_body("claude-haiku-4-5", &req);
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["temperature"].as_f64().unwrap(), 0.2_f32 as f64);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        // no assertion on the "thinking" key here: it is process-wide env-gated and owned by
        // build_body_thinking_budget_from_env, which runs concurrently under cargo test.
    }

    // kept in one test, not two, because both branches read the process-wide
    // KB_ANTHROPIC_THINKING_BUDGET env var and cargo runs tests on multiple threads by default.
    #[test]
    fn build_body_thinking_budget_from_env() {
        let req = CompletionRequest { system: None, messages: vec![], max_tokens: 100, temperature: None, json_schema: None };

        assert!(std::env::var("KB_ANTHROPIC_THINKING_BUDGET").is_err(), "no other test may set this env var");
        let body = build_body("claude-haiku-4-5", &req);
        assert!(body.get("thinking").is_none());

        unsafe { std::env::set_var("KB_ANTHROPIC_THINKING_BUDGET", "2048") };
        let body = build_body("claude-haiku-4-5", &req);
        unsafe { std::env::remove_var("KB_ANTHROPIC_THINKING_BUDGET") };
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn map_response_splits_text_and_thinking() {
        let raw = json!({
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 12, "output_tokens": 34 },
            "content": [
                { "type": "thinking", "thinking": "let me see", "signature": "abc123" },
                { "type": "text", "text": "the answer" },
                { "type": "tool_use", "id": "t1", "name": "search", "input": {} }
            ]
        });
        let resp = map_response(raw);
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 34);
        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            Block::Reasoning { text, opaque, provider } => {
                assert_eq!(text.as_deref(), Some("let me see"));
                assert_eq!(opaque["signature"], "abc123");
                assert_eq!(provider, "anthropic");
            }
            other => panic!("expected reasoning block, got {other:?}"),
        }
        match &resp.blocks[1] {
            Block::Text { text } => assert_eq!(text, "the answer"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn new_fails_without_api_key_env() {
        let cfg = ProviderConfig {
            kind: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
            base_url: None,
            api_key_env: Some("KB_TEST_MISSING_ANTHROPIC_KEY_XYZ".into()),
            max_tokens: None,
        };
        assert!(AnthropicProvider::new(&cfg).is_err());
    }

    /// hits the live anthropic api. requires KB_LIVE_PROVIDER_TESTS=1 and ANTHROPIC_API_KEY.
    /// run with: KB_LIVE_PROVIDER_TESTS=1 cargo test -p kb --lib provider::anthropic::tests::live_completion -- --ignored
    #[test]
    #[ignore = "hits the live anthropic api; see the doc comment for how to run it"]
    fn live_completion() {
        if std::env::var("KB_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping: set KB_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            eprintln!("skipping: ANTHROPIC_API_KEY is not set");
            return;
        }
        let cfg = ProviderConfig {
            kind: "anthropic".into(),
            model: std::env::var("KB_ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into()),
            base_url: None,
            api_key_env: None,
            max_tokens: Some(16),
        };
        let provider = AnthropicProvider::new(&cfg).unwrap();
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
