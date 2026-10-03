//! `kb session ...` and `kb eod`: clap wiring over the `session` module. not yet mounted in
//! `cli::Command` (see the report for the snippet to add there and to `cli::run`).
use crate::config::Config;
use crate::session;
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
	/// open a new session worktree, or recreate one for an existing branch with --resume
	Open {
		#[arg(long)]
		objective: String,
		#[arg(long)]
		actor: Option<String>,
		/// candidate document ids to sparse-checkout; empty means a full checkout
		#[arg(long)]
		sparse: Vec<String>,
		/// recreate the worktree for this session id instead of opening a new one
		#[arg(long)]
		resume: Option<String>,
	},
	/// stage, gate, and squash a session's work into one commit on top of its base
	Settle {
		id: String,
		#[arg(short, long)]
		message: Option<String>,
	},
	/// list every session: actor, state, objective, files touched, commits since base
	List {
		/// include merged and abandoned sessions
		#[arg(long)]
		all: bool,
	},
	/// show one session's full record
	Show { id: String },
	/// pull a session out of (default) or back into (--off) `kb eod`
	Hold {
		id: String,
		#[arg(long)]
		off: bool,
	},
	/// remove a session's worktree, keeping its branch for 30 days
	Abandon { id: String },
	/// remove a session's worktree and branch (branch kept with --keep-branch)
	Cleanup {
		id: String,
		#[arg(long)]
		keep_branch: bool,
	},
	/// end-of-day sweep: settle every open session, abandon ones idle over 24h
	Eod,
}

/// run one `kb session` (or `kb eod`) subcommand; returns the process exit code.
pub fn run(cfg: &Config, cmd: SessionCommand, json: bool) -> Result<i32> {
	match cmd {
		SessionCommand::Open { objective, actor, sparse, resume } => {
			let s = match resume {
				Some(id) => session::resume(cfg, &id, &objective)?,
				None => session::open(cfg, &objective, actor.as_deref(), &sparse)?,
			};
			if json {
				println!("{}", serde_json::to_string_pretty(&s)?);
			} else {
				println!("session {} ({}) opened for {}", s.id, s.branch, s.actor);
			}
			// last line is always the worktree path, so scripts can `cd $(kb session open ... | tail -1)`.
			println!("{}", s.worktree.display());
			Ok(0)
		}
		SessionCommand::Settle { id, message } => {
			let mut s = session::load(cfg, &id)?;
			match session::settle(cfg, &mut s, message.as_deref()) {
				Ok(commits) => {
					if json {
						println!("{}", serde_json::to_string_pretty(&commits)?);
					} else {
						for c in &commits {
							println!("settled {c}");
						}
					}
					Ok(0)
				}
				Err(e) => {
					eprintln!("settle failed: {e:#}");
					Ok(1)
				}
			}
		}
		SessionCommand::List { all } => {
			let mut rows = session::sessions_report(cfg)?;
			if !all {
				rows.retain(|r| !matches!(r.state.as_str(), "merged" | "abandoned"));
			}
			if json {
				println!("{}", serde_json::to_string_pretty(&rows)?);
			} else {
				for r in rows {
					println!("{}  {:<10}  {:<12}  {} files  {} commits  {}", r.id, r.state, r.actor, r.files_changed.len(), r.commits, r.objective);
				}
			}
			Ok(0)
		}
		SessionCommand::Show { id } => {
			let s = session::load(cfg, &id)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&s)?);
			} else {
				println!("{s:#?}");
			}
			Ok(0)
		}
		SessionCommand::Hold { id, off } => {
			session::hold(cfg, &id, !off)?;
			Ok(0)
		}
		SessionCommand::Abandon { id } => {
			session::abandon(cfg, &id)?;
			Ok(0)
		}
		SessionCommand::Cleanup { id, keep_branch } => {
			let mut s = session::load(cfg, &id)?;
			session::cleanup(cfg, &mut s, !keep_branch)?;
			Ok(0)
		}
		SessionCommand::Eod => {
			let report = session::eod(cfg)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&report)?);
			} else {
				println!("settled {}, abandoned {}, skipped {}, failed {}", report.settled, report.abandoned, report.skipped, report.failed.len());
				for f in &report.failed {
					eprintln!("eod failed: {f}");
				}
			}
			Ok(if report.failed.is_empty() { 0 } else { 1 })
		}
	}
}
