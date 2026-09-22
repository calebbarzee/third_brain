//! claude-code adapter (`kind = "claude-code"`): runs the owner's locally installed `claude`
//! cli in print mode, so requests ride the owner's subscription with no api key. design.md §19.
//!
//! `base_url` is ignored -- there is no endpoint to point at. `model` is passed through to the
//! cli's `--model` verbatim, so it accepts either an alias ("sonnet", "opus", "haiku") or a
//! full model name; an empty `model` omits the flag and the cli's own default model runs.
//!
//! the cli is asked for `--tools ""` (no tools) rather than a turn limit: this build's `claude`
//! (2.1.273) has no `--max-turns` flag (`claude -p --help` was checked first, per the task),
//! so turn count is capped indirectly by giving the model nothing to call. `--strict-mcp-config`
//! with no `--mcp-config` similarly drops any project/user mcp servers, so the reply comes from
//! the prompt alone. the prompt goes over stdin (`claude -p` reads stdin when no positional
//! prompt is given, confirmed against this build), not as an argument, to avoid argv length and
//! quoting limits.

use super::subprocess::{run_with_timeout, timeout_from_env};
use super::{Block, CompletionRequest, CompletionResponse, Provider, Role, Usage, truncate};
use crate::config::ProviderConfig;
use anyhow::{Result, bail};
use serde_json::Value;

/// the claude cli binary to run; overridable so a test can point at a stub instead of the real
/// `claude` (which would otherwise spend real subscription usage every test run).
const DEFAULT_BIN: &str = "claude";

pub struct ClaudeCodeProvider {
    bin: String,
    /// `None` when `cfg.model` was empty: the cli's own default model runs.
    model: Option<String>,
}

impl ClaudeCodeProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        Ok(Self {
            bin: std::env::var("KB_CLAUDE_CODE_BIN").unwrap_or_else(|_| DEFAULT_BIN.to_string()),
            model: (!cfg.model.trim().is_empty()).then(|| cfg.model.clone()),
        })
    }
}

fn role_label(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// flatten a multi-message conversation into one prompt, one `role: content` block per message
/// in order. `structured::complete_json`'s retry loop appends an `Assistant` message (the
/// rejected reply) then a `User` message (the correction) onto the original request, so a
/// retried call arrives here as more than one message; this is what turns that back into the
/// single string the cli's stdin takes.
pub fn build_prompt(req: &CompletionRequest) -> String {
    req.messages.iter().map(|m| format!("{}: {}", role_label(m.role), m.content)).collect::<Vec<_>>().join("\n\n")
}

/// build the cli's argv (everything but the binary name and the prompt, which goes on stdin).
/// pure so it is unit-testable without spawning anything.
pub fn build_args(model: Option<&str>, system: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--tools".to_string(),
        String::new(),
        "--strict-mcp-config".to_string(),
    ];
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(s) = system {
        args.push("--system-prompt".to_string());
        args.push(s.to_string());
    }
    args
}

/// map a successful (`is_error: false`) `claude -p --output-format json` reply into a
/// `CompletionResponse`. pure so it is unit-testable against fixture json. the cli's shape has
/// no reasoning block equivalent (thinking, when it happens, is not returned in print mode), so
/// this only ever produces one `Text` block; `stop_reason` is normalized to `"end_turn"` since
/// the cli's own `stop_reason` values (`"end_turn"`, `"stop_sequence"`, ...) do not carry the
/// same meaning as an api response's and every other adapter's callers only compare against
/// `"end_turn"`.
pub fn map_response(raw: &Value) -> CompletionResponse {
    let text = raw["result"].as_str().unwrap_or_default().to_string();
    let usage = Usage {
        input_tokens: raw["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: raw["usage"]["output_tokens"].as_u64().unwrap_or(0),
    };
    CompletionResponse { blocks: vec![Block::Text { text }], usage, stop_reason: "end_turn".to_string(), raw: raw.clone() }
}

fn stderr_tail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() { String::new() } else { format!(" (stderr: {})", truncate(trimmed, 500)) }
}

impl Provider for ClaudeCodeProvider {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse> {
        let prompt = build_prompt(req);
        let args = build_args(self.model.as_deref(), req.system.as_deref());
        let out = run_with_timeout(&self.bin, &args, &prompt, timeout_from_env())?;

        let parsed: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
        match parsed {
            Some(raw) if raw["is_error"].as_bool().unwrap_or(false) => {
                let result = raw["result"].as_str().unwrap_or("(no result text)");
                bail!("claude cli reported an error: {result}{}", stderr_tail(&out.stderr));
            }
            Some(raw) if !out.status.success() => {
                let result = raw["result"].as_str().unwrap_or("(no result text)");
                bail!("claude cli exited with {:?}: {result}{}", out.status.code(), stderr_tail(&out.stderr));
            }
            Some(raw) => Ok(map_response(&raw)),
            None => {
                bail!(
                    "claude cli exited with {:?} and stdout was not valid json: {}{}",
                    out.status.code(),
                    truncate(out.stdout.trim(), 200),
                    stderr_tail(&out.stderr)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use serde_json::json;

    #[test]
    fn build_prompt_flattens_and_labels_roles() {
        let req = CompletionRequest {
            system: Some("be terse".into()),
            messages: vec![
                Message { role: Role::User, content: "classify this".into() },
                Message { role: Role::Assistant, content: "not json".into() },
                Message { role: Role::User, content: "your previous answer was rejected".into() },
            ],
            max_tokens: 64,
            temperature: Some(0.0),
            json_schema: None,
        };
        let prompt = build_prompt(&req);
        assert_eq!(prompt, "user: classify this\n\nassistant: not json\n\nuser: your previous answer was rejected");
        // system is carried separately to --system-prompt, not folded into the flattened prompt.
        assert!(!prompt.contains("be terse"));
    }

    #[test]
    fn build_args_omits_model_and_system_when_absent() {
        let args = build_args(None, None);
        assert_eq!(args, vec!["-p", "--output-format", "json", "--tools", "", "--strict-mcp-config"]);
    }

    #[test]
    fn build_args_includes_model_and_system_when_present() {
        let args = build_args(Some("haiku"), Some("be terse"));
        assert!(args.windows(2).any(|w| w == ["--model", "haiku"]));
        assert!(args.windows(2).any(|w| w == ["--system-prompt", "be terse"]));
    }

    #[test]
    fn map_response_reads_result_and_usage() {
        let raw = json!({
            "is_error": false, "result": "ok",
            "usage": { "input_tokens": 2, "output_tokens": 4 },
        });
        let resp = map_response(&raw);
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.usage.input_tokens, 2);
        assert_eq!(resp.usage.output_tokens, 4);
        match &resp.blocks[0] {
            Block::Text { text } => assert_eq!(text, "ok"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn new_treats_blank_model_as_none() {
        let cfg = ProviderConfig { kind: "claude-code".into(), model: "  ".into(), base_url: None, api_key_env: None, max_tokens: None };
        let provider = ClaudeCodeProvider::new(&cfg).unwrap();
        assert!(provider.model.is_none());
    }

    /// hits the owner's local `claude` cli in print mode; spends real subscription usage.
    /// requires KB_LIVE_PROVIDER_TESTS=1 and `claude` on PATH (or KB_CLAUDE_CODE_BIN set to it).
    /// run with: KB_LIVE_PROVIDER_TESTS=1 cargo test -p kb --lib provider::claude_code::tests::live_completion -- --ignored --nocapture
    #[test]
    #[ignore = "hits the owner's local claude cli; see the doc comment for how to run it"]
    fn live_completion() {
        if std::env::var("KB_LIVE_PROVIDER_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping: set KB_LIVE_PROVIDER_TESTS=1 to run");
            return;
        }
        let cfg = ProviderConfig { kind: "claude-code".into(), model: "haiku".into(), base_url: None, api_key_env: None, max_tokens: Some(16) };
        let provider = ClaudeCodeProvider::new(&cfg).unwrap();
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
        eprintln!("live claude-code reply: {text:?}");
    }
}
