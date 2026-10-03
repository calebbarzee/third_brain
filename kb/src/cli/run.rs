//! `kb run` subcommands: list, show, verify, diff, search over the run log (design.md §16).
//! not wired into `cli/mod.rs` yet -- see this crate's report for the `Command` variant and
//! dispatch snippet.
use crate::config::Config;
use crate::runlog::{self, verify};
use anyhow::{Result, bail};
use clap::Subcommand;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum RunCommand {
	/// list runs from `<state>/runs` and the sealed store, newest-started first
	List {
		/// only runs started within this window, e.g. `24h`, `7d`, `30m`
		#[arg(long)]
		since: Option<String>,
		/// only runs with this exact status, e.g. `failed`
		#[arg(long)]
		status: Option<String>,
	},
	/// print a run's event timeline
	Show {
		id: String,
		/// include reasoning blocks from `llm_response` events
		#[arg(long)]
		reasoning: bool,
	},
	/// recompute the hash chain and compare it against the stored final hash
	Verify { id: String },
	/// list `op` rows with before/after content hashes
	Diff { id: String },
	/// full-text search over event payloads
	Search { id: String, needle: String },
}

pub fn run(cfg: &Config, cmd: RunCommand, json: bool) -> Result<i32> {
	match cmd {
		RunCommand::List { since, status } => run_list(cfg, since, status, json),
		RunCommand::Show { id, reasoning } => run_show(cfg, &id, reasoning, json),
		RunCommand::Verify { id } => run_verify(cfg, &id, json),
		RunCommand::Diff { id } => run_diff(cfg, &id, json),
		RunCommand::Search { id, needle } => run_search(cfg, &id, &needle, json),
	}
}

fn run_list(cfg: &Config, since: Option<String>, status: Option<String>, json: bool) -> Result<i32> {
	let since = since.as_deref().map(parse_duration).transpose()?;
	let rows = verify::list(cfg, since, status.as_deref())?;
	if json {
		println!("{}", serde_json::to_string_pretty(&rows)?);
	} else {
		for r in &rows {
			println!("{}  {:<10} {:<28} {:<16} {}", r.run_id, r.status, r.actor, r.skill.as_deref().unwrap_or("-"), r.started_at);
		}
	}
	Ok(0)
}

fn run_show(cfg: &Config, id: &str, reasoning: bool, json: bool) -> Result<i32> {
	let path = runlog::resolve_run_path(cfg, id)?;
	let rows = verify::timeline(&path, reasoning)?;
	if json {
		println!("{}", serde_json::to_string_pretty(&rows)?);
	} else {
		for r in &rows {
			println!("{:>6}  {}  {:<16} {}", r.seq, r.ts, r.kind, r.summary);
		}
	}
	Ok(0)
}

fn run_verify(cfg: &Config, id: &str, json: bool) -> Result<i32> {
	let path = runlog::resolve_run_path(cfg, id)?;
	let report = verify::verify(&path)?;
	if json {
		println!("{}", serde_json::to_string_pretty(&report)?);
	} else {
		println!("events: {}", report.events);
		println!("final hash matches: {}", report.final_hash_matches);
		match report.first_bad_seq {
			Some(seq) => println!("chain broken at seq {seq}"),
			None => println!("chain ok"),
		}
	}
	Ok(if report.ok { 0 } else { 1 })
}

fn run_diff(cfg: &Config, id: &str, json: bool) -> Result<i32> {
	let path = runlog::resolve_run_path(cfg, id)?;
	let rows = verify::diff(&path)?;
	if json {
		println!("{}", serde_json::to_string_pretty(&rows)?);
	} else {
		for r in &rows {
			println!(
				"{} {} {} {} -> {}",
				r.op_id,
				r.kind,
				r.doc_id.as_deref().unwrap_or("-"),
				r.before_hash.as_deref().unwrap_or("-"),
				r.after_hash.as_deref().unwrap_or("-")
			);
		}
	}
	Ok(0)
}

fn run_search(cfg: &Config, id: &str, needle: &str, json: bool) -> Result<i32> {
	let path = runlog::resolve_run_path(cfg, id)?;
	let hits = verify::search(&path, needle)?;
	if json {
		println!("{}", serde_json::to_string_pretty(&hits)?);
	} else {
		for h in &hits {
			println!("{:>6}  {:<16} {}", h.seq, h.kind, h.snippet);
		}
	}
	Ok(0)
}

/// parse a duration like `24h`, `7d`, `30m`, `45s`.
pub(crate) fn parse_duration(s: &str) -> Result<Duration> {
	let s = s.trim();
	if s.len() < 2 {
		bail!("duration `{s}` must look like `24h`, `7d`, or `30m`");
	}
	let (num, unit) = s.split_at(s.len() - 1);
	let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("duration `{s}` must start with a number"))?;
	let secs = match unit {
		"s" => n,
		"m" => n * 60,
		"h" => n * 3_600,
		"d" => n * 86_400,
		_ => bail!("duration `{s}` must end in s, m, h, or d"),
	};
	Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_hours_days_minutes_seconds() {
		assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(24 * 3_600));
		assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(7 * 86_400));
		assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(30 * 60));
		assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
		assert!(parse_duration("bogus").is_err());
		assert!(parse_duration("h").is_err());
	}
}
