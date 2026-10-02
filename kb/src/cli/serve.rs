//! `kb serve`, `kb mcp`, and `kb token`: design.md §23 (local and server) and §29 step 6. not
//! wired into `cli::mod`; see this crate's report for the `Command` variant and dispatch snippet.

use crate::config::Config;
use crate::server::{self, ServeOpts, auth};
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ServeCommand {
    /// start the http server (rest and mcp) plus the background reconciliation tick
    Serve {
        #[arg(long, default_value = "127.0.0.1:7777")]
        bind: String,
        /// bind to 127.0.0.1 only and map every request to the git identity's principal, no
        /// bearer token required (single-user local mode, design.md §10)
        #[arg(long)]
        no_auth: bool,
        /// reconciliation tick interval; 0 disables the background tick thread
        #[arg(long, default_value_t = 60)]
        tick_seconds: u64,
        /// serve reads only: propose, the webhooks, and the mcp propose tool are refused
        #[arg(long)]
        read_only: bool,
    },
    /// run the mcp server over stdio, under the current git identity's principal
    Mcp,
    /// bearer tokens for the http server (design.md §10)
    Token {
        #[command(subcommand)]
        cmd: TokenCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum TokenCommand {
    /// mint a token for `principal` and print it once; only its sha256 is stored
    Issue { principal: String },
    /// list every issued token (hash, principal, timestamps, revoked)
    List,
    /// revoke every token matching a hash prefix or an exact principal name
    Revoke { target: String },
}

pub fn run(cfg: &Config, cmd: ServeCommand, json: bool) -> Result<i32> {
    match cmd {
        ServeCommand::Serve { bind, no_auth, tick_seconds, read_only } => {
            crate::transcript::set_default_trigger("http");
            server::serve_blocking(cfg.clone(), ServeOpts { bind, no_auth, tick_seconds, read_only })?;
            Ok(0)
        }
        ServeCommand::Mcp => {
            crate::transcript::set_default_trigger("mcp:stdio");
            server::mcp::run_stdio(cfg)?;
            Ok(0)
        }
        ServeCommand::Token { cmd: TokenCommand::Issue { principal } } => {
            let token = auth::issue(cfg, &principal)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"token": token, "principal": principal}))?);
            } else {
                println!("{token}");
                eprintln!("store this token now; it will not be shown again");
            }
            Ok(0)
        }
        ServeCommand::Token { cmd: TokenCommand::List } => {
            let rows = auth::list(cfg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in &rows {
                    let status = if r.revoked { "revoked" } else { "active" };
                    println!("{}  {:<20} {:<9} created {}  last_used {}", &r.token_sha256[..16], r.principal, status, r.created_at, r.last_used.as_deref().unwrap_or("-"));
                }
            }
            Ok(0)
        }
        ServeCommand::Token { cmd: TokenCommand::Revoke { target } } => {
            let n = auth::revoke(cfg, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"revoked": n}))?);
            } else {
                println!("revoked {n} token(s) matching `{target}`");
            }
            Ok(0)
        }
    }
}
