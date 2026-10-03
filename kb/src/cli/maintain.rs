//! `kb maintain ...`: nightly curation, its escalation queue, and compaction. design.md §15.3, §21.
//! not wired into `cli::Command` -- see this crate's report for the snippet to add there.

use crate::config::Config;
use crate::maintain::{self, FindingKind, NightlyOpts};
use anyhow::{Context, Result};
use clap::Subcommand;
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum MaintainCommand {
	/// run the nightly curation pass: detect, auto-fix in band, escalate the rest
	Nightly {
		/// detect and report without opening any session or writing anything
		#[arg(long)]
		dry_run: bool,
		/// a commit-ish, or a duration like `24h`/`7d`; defaults to since the last nightly run
		#[arg(long)]
		since: Option<String>,
		/// run one finding kind only
		#[arg(long)]
		only: Option<String>,
	},
	/// run the detectors only and print what they find; no session, no escalation file
	Findings {
		#[arg(long)]
		only: Option<String>,
	},
	/// the escalation queue: findings nightly curation could not (or must not) auto-fix
	Escalations {
		/// mark one escalation resolved, by id or an unambiguous prefix
		#[arg(long)]
		resolve: Option<String>,
	},
	/// compact the repository: prune worktrees, drop merged session branches, repack, gc
	Compact {
		#[arg(long)]
		dry_run: bool,
	},
}

fn parse_only(s: &Option<String>) -> Result<Option<FindingKind>> {
	match s {
		Some(s) => Ok(Some(FindingKind::from_str(s).map_err(anyhow::Error::msg)?)),
		None => Ok(None),
	}
}

pub fn run(cfg: &Config, cmd: MaintainCommand, json: bool) -> Result<i32> {
	match cmd {
		MaintainCommand::Nightly { dry_run, since, only } => {
			let opts = NightlyOpts { dry_run, since, only: parse_only(&only)? };
			let report = maintain::nightly(cfg, opts)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&report)?);
			} else {
				for f in &report.findings {
					println!("{}: {} (auto_fix={})", f.kind, f.detail, f.auto_fix.is_some());
				}
				println!("{} findings, {} sessions, {} escalations", report.findings.len(), report.sessions.len(), report.escalations.len());
				for e in &report.escalations {
					println!("escalated: {e}");
				}
				match &report.checkpoint {
					Some(tag) => println!("checkpoint tagged: {tag}"),
					None => println!("no checkpoint tagged (pass was not clean, or this was a dry run)"),
				}
			}
			Ok(0)
		}
		MaintainCommand::Findings { only } => {
			let findings = maintain::run_findings(cfg, parse_only(&only)?)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&findings)?);
			} else if findings.is_empty() {
				println!("no findings");
			} else {
				for f in &findings {
					println!("{}: {} (auto_fix={})", f.kind, f.detail, f.auto_fix.is_some());
				}
			}
			Ok(0)
		}
		MaintainCommand::Escalations { resolve } => {
			if let Some(id) = resolve {
				let full_id = maintain::resolve_escalation(cfg, &id)?;
				if json {
					println!("{}", serde_json::json!({"resolved": full_id}));
				} else {
					println!("resolved {full_id}");
				}
				return Ok(0);
			}
			let items = maintain::list_escalations(cfg)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&items)?);
			} else if items.is_empty() {
				println!("no escalations");
			} else {
				for (id, rec) in &items {
					let status = if rec.resolved { "resolved" } else { "open" };
					println!("{id} [{status}] {}: {} ({})", rec.kind, rec.detail, rec.doc_ids.join(", "));
				}
			}
			Ok(0)
		}
		MaintainCommand::Compact { dry_run } => {
			let report = maintain::compact(cfg, maintain::CompactOpts { dry_run }).context("compact failed")?;
			if json {
				println!("{}", serde_json::to_string_pretty(&report)?);
			} else if let Some(reason) = &report.skipped {
				println!("compact skipped: {reason}");
			} else {
				for w in &report.warnings {
					println!("warning: {w}");
				}
				println!(
					"compact: checkpoint {}, deleted {} session branch(es), repacked={}, moved {} session file(s), removed {} old index artifact(s)",
					report.checkpoint_tag.as_deref().unwrap_or("-"),
					report.deleted_session_branches.len(),
					report.repacked,
					report.moved_session_files,
					report.index_artifacts_removed,
				);
				for r in &report.refused_session_branches {
					println!("refused: {r}");
				}
			}
			Ok(0)
		}
	}
}
