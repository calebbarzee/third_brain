//! `kb merge`, `kb review`, `kb revert`, `kb doctor`, `kb tick`: clap wiring over the `merge`
//! module. not yet mounted in `cli::Command` (see the report for the snippet to add there and to
//! `cli::run`, following the same not-yet-mounted pattern as `cli::session`).

use crate::config::Config;
use crate::merge::{self, MergeOpts, MergeOutcome};
use crate::model::MergeClass;
use crate::session::SessionState;
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum MergeCommand {
	/// rebase, run the checks, and land (or file for review) one session
	Merge {
		session: String,
		/// run the checks and report the class without landing
		#[arg(long)]
		check: bool,
		/// land a review-class session now (an approval note already exists, or this stands in for one)
		#[arg(long)]
		approve: bool,
	},
	/// the review queue: sessions waiting on a human approval
	Review {
		#[command(subcommand)]
		cmd: ReviewCommand,
	},
	/// revert a landed commit, run, or session as a new forward commit
	Revert { target: String },
	/// compare `main` to the last known head and report worktree/session drift
	Doctor,
	/// local reconciliation pass: index freshness, stuck merges, worktree hygiene
	Tick,
}

#[derive(Subcommand, Debug)]
pub enum ReviewCommand {
	/// list unmerged sessions carrying a review note
	List,
	/// approve and land a review-class session
	Approve {
		session: String,
		/// approve as this principal instead of the current git identity
		#[arg(long)]
		as_principal: Option<String>,
	},
	/// reject a review-class session, sending it back to its author
	Reject {
		session: String,
		#[arg(long)]
		reason: String,
	},
}

/// print one `MergeOutcome` and translate it to a process exit code:
/// 0 landed, 3 review class waiting, 4 back to the author, 5 needs a human.
fn report_outcome(outcome: &MergeOutcome, json: bool) -> Result<i32> {
	if json {
		println!("{}", serde_json::to_string_pretty(outcome)?);
	} else {
		match &outcome.landed {
			Some(sha) => println!("merged: main is now {sha}"),
			None => match outcome.state {
				SessionState::NeedsAuthor => println!("not landed: back to the author (fix on the session branch, settle, and merge again)"),
				SessionState::NeedsHuman => println!("not landed: needs a human (conflict left in the worktree)"),
				_ if outcome.class == MergeClass::Review => println!("not landed: review class; waiting for `kb review approve <session>`"),
				_ => println!("not landed: state {}", outcome.state),
			},
		}
		for c in &outcome.checks {
			println!("  [{}] {}: {}", if c.pass { "ok" } else { "FAIL" }, c.name, c.detail);
		}
		for r in &outcome.reasons {
			println!("  reason: {r}");
		}
	}
	Ok(if outcome.landed.is_some() {
		0
	} else {
		match outcome.state {
			SessionState::NeedsHuman => 5,
			SessionState::NeedsAuthor => 4,
			SessionState::Settled if outcome.class == MergeClass::Review => 3,
			SessionState::Settled => 4,
			_ => 1,
		}
	})
}

/// run one `kb merge`/`kb review`/`kb revert`/`kb doctor`/`kb tick` subcommand; returns the
/// process exit code.
pub fn run(cfg: &Config, cmd: MergeCommand, json: bool) -> Result<i32> {
	match cmd {
		MergeCommand::Merge { session, check, approve } => {
			let outcome = merge::merge(cfg, &session, MergeOpts { check_only: check, approve, ..MergeOpts::default() })?;
			report_outcome(&outcome, json)
		}
		MergeCommand::Review { cmd } => match cmd {
			ReviewCommand::List => {
				let items = merge::review_list(cfg)?;
				if json {
					println!("{}", serde_json::to_string_pretty(&items)?);
				} else {
					for i in &items {
						println!("{}  {}  {}  {}", i.session, i.branch, i.note.class, i.note.reasons.join("; "));
					}
				}
				Ok(0)
			}
			ReviewCommand::Approve { session, as_principal } => {
				let outcome = merge::review_approve(cfg, &session, as_principal)?;
				report_outcome(&outcome, json)
			}
			ReviewCommand::Reject { session, reason } => {
				merge::review_reject(cfg, &session, &reason)?;
				Ok(0)
			}
		},
		MergeCommand::Revert { target } => {
			let outcome = merge::history::revert(cfg, &target)?;
			report_outcome(&outcome, json)
		}
		MergeCommand::Doctor => {
			let report = merge::history::doctor(cfg)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&report)?);
			} else {
				for line in &report {
					println!("{line}");
				}
			}
			Ok(0)
		}
		MergeCommand::Tick => {
			let report = merge::tick::tick(cfg)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&report)?);
			} else {
				println!(
					"tick: indexed={} stale-merging-settled={} worktrees-adopted={} worktrees-removed={} worktrees-kept-dirty={} merged-sessions-cleaned={}",
					report.indexed,
					report.settled_stale_merging,
					report.adopted_worktrees,
					report.removed_worktrees,
					report.kept_dirty_worktrees,
					report.cleaned_merged_sessions
				);
			}
			Ok(0)
		}
	}
}
