//! `kb gate ask`: manually run the quick classifier (the `gate` role, design.md §19's provider
//! table plus the quick-gate addendum in `merge::merge_with_providers`) against a line of text.
//! not wired into `cli/mod.rs` yet -- see the report that shipped with this file for the
//! `Command::Gate` variant and dispatch snippet.

use crate::config::Config;
use crate::provider::gate::{GateQuestion, gate};
use anyhow::{Result, bail};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum GateCommand {
    /// classify `text` into one of `--labels` using the provider configured for role `gate`
    Ask {
        text: String,
        /// comma-separated label list, e.g. "trivial,substantive" (at least two)
        #[arg(long)]
        labels: String,
        /// one sentence steering what the labels mean for this call
        #[arg(long)]
        context: Option<String>,
    },
}

pub fn run(cfg: &Config, cmd: GateCommand, json: bool) -> Result<i32> {
    match cmd {
        GateCommand::Ask { text, labels, context } => run_ask(cfg, &text, &labels, context, json),
    }
}

fn run_ask(cfg: &Config, text: &str, labels: &str, context: Option<String>, json: bool) -> Result<i32> {
    let labels: Vec<String> = labels.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if labels.len() < 2 {
        bail!("--labels needs at least two comma-separated labels");
    }
    let q = GateQuestion { labels, text: text.to_string(), context };
    let Some(answer) = gate(cfg, &q)? else {
        println!("no provider configured for role \"gate\"");
        return Ok(1);
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else {
        println!("label: {}", answer.label);
        println!("p: {:.2}", answer.p);
        println!("reason: {}", answer.reason);
    }
    Ok(0)
}
