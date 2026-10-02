//! generic command adapter (`kind = "command"`): runs an arbitrary local command named by
//! `base_url`, writes the flattened prompt to its stdin, and reads the reply from its stdout.
//! lets a small custom classifier (a python script, a shell one-liner) plug into any role
//! without a dedicated adapter. `model` and `api_key_env` are ignored; `max_tokens` and
//! `temperature` are request-side hints the command is free to disregard.

use super::subprocess::{run_with_timeout, timeout_from_env};
use super::{Block, CompletionRequest, CompletionResponse, Provider, Role, Usage, truncate};
use crate::config::ProviderConfig;
use anyhow::{Result, anyhow, bail};
use serde_json::json;

pub struct CommandProvider {
    /// the whole configured command line, kept around for error messages.
    command_line: String,
    /// `command_line` split on whitespace; `program` is `parts[0]`, the rest are its args. no
    /// quoting support -- a path with a space in it does not work. see the report for this gap.
    parts: Vec<String>,
}

impl CommandProvider {
    pub fn new(cfg: &ProviderConfig) -> Result<Self> {
        let command_line = cfg
            .base_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("command provider: `base_url` must hold the command to run, e.g. base_url = \"python3 /path/to/classifier.py\""))?;
        let parts: Vec<String> = command_line.split_whitespace().map(str::to_string).collect();
        if parts.is_empty() {
            bail!("command provider: `base_url` has no command in it");
        }
        Ok(Self { command_line, parts })
    }
}

fn role_label(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// flatten a request into the text sent on stdin: an optional `system: ...` line, then one
/// `role: content` line per message, in order.
pub fn build_prompt(req: &CompletionRequest) -> String {
    let mut parts = Vec::new();
    if let Some(s) = &req.system {
        parts.push(format!("system: {s}"));
    }
    parts.extend(req.messages.iter().map(|m| format!("{}: {}", role_label(m.role), m.content)));
    parts.join("\n\n")
}

impl Provider for CommandProvider {
    fn name(&self) -> &str {
        "command"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse> {
        let prompt = build_prompt(req);
        let (program, args) = self.parts.split_first().expect("checked non-empty in `new`");
        let out = run_with_timeout(program, args, &prompt, timeout_from_env(), None)?;

        if !out.status.success() {
            bail!(
                "command provider `{}` exited with {:?}: {}",
                self.command_line,
                out.status.code(),
                truncate(out.stderr.trim(), 500)
            );
        }
        let text = out.stdout.trim().to_string();
        Ok(CompletionResponse {
            blocks: vec![Block::Text { text }],
            usage: Usage::default(),
            stop_reason: "end_turn".to_string(),
            raw: json!({ "stdout": out.stdout, "stderr": out.stderr }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    fn cfg(base_url: &str) -> ProviderConfig {
        ProviderConfig { kind: "command".into(), model: String::new(), base_url: Some(base_url.to_string()), api_key_env: None, max_tokens: None }
    }

    #[test]
    fn new_rejects_missing_base_url() {
        let c = ProviderConfig { kind: "command".into(), model: String::new(), base_url: None, api_key_env: None, max_tokens: None };
        assert!(CommandProvider::new(&c).is_err());
    }

    #[test]
    fn new_splits_command_and_args() {
        let provider = CommandProvider::new(&cfg("python3 /path/to/classifier.py --flag")).unwrap();
        assert_eq!(provider.parts, vec!["python3", "/path/to/classifier.py", "--flag"]);
    }

    #[test]
    fn build_prompt_includes_system_and_messages() {
        let req = CompletionRequest {
            system: Some("be terse".into()),
            messages: vec![Message { role: Role::User, content: "hi".into() }],
            max_tokens: 64,
            temperature: None,
            json_schema: None,
        };
        assert_eq!(build_prompt(&req), "system: be terse\n\nuser: hi");
    }

    #[test]
    fn complete_runs_the_command_and_reads_stdout() {
        // `cat` echoes stdin back to stdout: a stand-in for a real classifier script that
        // proves the plumbing (spawn, feed stdin, read stdout) without shelling out to python.
        let provider = CommandProvider::new(&cfg("cat")).unwrap();
        let req = CompletionRequest {
            system: None,
            messages: vec![Message { role: Role::User, content: "hello from a test".into() }],
            max_tokens: 64,
            temperature: None,
            json_schema: None,
        };
        let resp = provider.complete(&req).unwrap();
        let text = super::super::first_text(&resp).unwrap();
        assert_eq!(text, "user: hello from a test");
    }

    #[test]
    fn complete_errors_on_nonzero_exit() {
        let provider = CommandProvider::new(&cfg("false")).unwrap();
        let req = CompletionRequest { system: None, messages: vec![], max_tokens: 64, temperature: None, json_schema: None };
        assert!(provider.complete(&req).is_err());
    }
}
