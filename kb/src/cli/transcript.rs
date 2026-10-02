//! `kb transcript`: read the full transcripts of model calls and mcp tool calls
//! (`crate::transcript`).
use crate::config::Config;
use crate::transcript::{self, Filter};
use anyhow::{Result, bail};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum TranscriptCommand {
    /// list transcripts newest first: time, kind, role, actor, trigger, run, tokens, duration
    List {
        /// only calls within this window, e.g. `24h`, `7d`, `30m`
        #[arg(long)]
        since: Option<String>,
        /// `llm` (a model call) or `mcp` (a tool call from an mcp client)
        #[arg(long)]
        kind: Option<String>,
        /// a provider role (`classify`, `review`, `curate`, `answer`, ...) or an mcp tool name
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// print one transcript in full: every message sent and the whole reply
    Show { id: String },
}

pub fn run(cfg: &Config, cmd: TranscriptCommand, json: bool) -> Result<i32> {
    match cmd {
        TranscriptCommand::List { since, kind, role, run, session, limit } => {
            let since = since.as_deref().map(super::run::parse_duration).transpose()?;
            let filter = Filter {
                since: since.and_then(|d| chrono::Duration::from_std(d).ok()).map(|d| crate::clock::now() - d),
                kind,
                role,
                run_id: run,
                session_id: session,
                limit,
            };
            let rows = transcript::list(cfg, &filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(0);
            }
            for t in &rows {
                let tokens = match (t.input_tokens, t.output_tokens) {
                    (Some(i), Some(o)) => format!("{i}/{o} tok"),
                    _ => "-".to_string(),
                };
                let status = if t.error.is_some() { "error" } else { "ok" };
                println!(
                    "{}  {}  {:<4} {:<10} {:<28} {:<16} run={} {} {}ms {status}",
                    t.id,
                    t.ts,
                    t.kind,
                    t.role.as_deref().unwrap_or("-"),
                    t.actor.as_deref().unwrap_or("-"),
                    t.trigger,
                    t.run_id.as_deref().unwrap_or("-"),
                    tokens,
                    t.duration_ms.unwrap_or(0),
                );
            }
            Ok(0)
        }
        TranscriptCommand::Show { id } => {
            let Some(t) = transcript::get(cfg, &id)? else {
                bail!("no transcript matches `{id}`");
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&t)?);
                return Ok(0);
            }
            println!("id: {}\ntime: {}\nkind: {}  role: {}  provider: {}  model: {}", t.id, t.ts, t.kind, t.role.as_deref().unwrap_or("-"), t.provider.as_deref().unwrap_or("-"), t.model.as_deref().unwrap_or("-"));
            println!("actor: {}  trigger: {}  run: {}  session: {}", t.actor.as_deref().unwrap_or("-"), t.trigger, t.run_id.as_deref().unwrap_or("-"), t.session_id.as_deref().unwrap_or("-"));
            println!("command: kb {}", t.command.as_deref().unwrap_or(""));
            if let Some(system) = t.request.get("system").and_then(|v| v.as_str()) {
                println!("\n--- system ---\n{system}");
            }
            match t.request.get("messages").and_then(|v| v.as_array()) {
                Some(messages) => {
                    for m in messages {
                        println!("\n--- {} ---\n{}", m["role"].as_str().unwrap_or("?"), m["content"].as_str().unwrap_or(""));
                    }
                }
                None => println!("\n--- request ---\n{}", serde_json::to_string_pretty(&t.request)?),
            }
            if let Some(resp) = &t.response {
                let text: Vec<&str> = resp["blocks"].as_array().map(|b| b.iter().filter_map(|x| x["text"].as_str()).collect()).unwrap_or_default();
                if text.is_empty() {
                    println!("\n--- response ---\n{}", serde_json::to_string_pretty(resp)?);
                } else {
                    println!("\n--- response ---\n{}", text.join("\n"));
                }
            }
            if let Some(e) = &t.error {
                println!("\n--- error ---\n{e}");
            }
            Ok(0)
        }
    }
}
