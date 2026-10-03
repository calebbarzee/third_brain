//! `kb review` subcommands: run content review, print the packet, resolve one conflict.
//! not wired into `cli::mod`; see this crate's report for the wiring snippet.

use crate::config::Config;
use crate::git;
use crate::model::MergeClass;
use crate::review::{self, ReviewInput};
use anyhow::{Result, bail};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum ReviewCommand {
	/// run content review over a git range (design.md §15.2) and print the verdict and decision.
	Run {
		/// `base..head`
		range: String,
		/// current main sha, when it differs from base
		#[arg(long)]
		main: Option<String>,
		/// the settled commit message; defaults to head's commit message
		#[arg(long)]
		justification: Option<String>,
		/// files holding captured/conversation excerpts to include as untrusted context
		#[arg(long)]
		excerpt: Vec<PathBuf>,
	},
	/// gather and print the review packet without calling a model.
	Packet {
		range: String,
		#[arg(long)]
		main: Option<String>,
		/// the settled commit message; defaults to head's commit message
		#[arg(long)]
		justification: Option<String>,
	},
	/// resolve one conflicted path (design.md §14).
	Resolve {
		path: String,
		#[arg(long)]
		base: String,
		#[arg(long)]
		ours: String,
		#[arg(long)]
		theirs: String,
		/// write the resolved content to the working tree
		#[arg(long)]
		write: bool,
	},
}

pub fn run(cfg: &Config, cmd: ReviewCommand, json: bool) -> Result<i32> {
	match cmd {
		ReviewCommand::Run { range, main, justification, excerpt } => run_review(cfg, &range, main, justification, excerpt, json),
		ReviewCommand::Packet { range, main, justification } => run_packet(cfg, &range, main, justification, json),
		ReviewCommand::Resolve { path, base, ours, theirs, write } => run_resolve(cfg, &path, &base, &ours, &theirs, write, json),
	}
}

fn split_range(range: &str) -> Result<(String, String)> {
	match range.split_once("..") {
		Some((b, h)) => Ok((b.to_string(), if h.is_empty() { "HEAD".to_string() } else { h.to_string() })),
		None => bail!("range `{range}` must look like `base..head`"),
	}
}

fn current_actor(cfg: &Config) -> String {
	let (_, email) = git::identity(&cfg.root);
	match email {
		Some(e) => format!("human:{e}"),
		None => "human:unknown".to_string(),
	}
}

fn read_excerpts(paths: &[PathBuf]) -> Result<Vec<String>> {
	paths.iter().map(|p| std::fs::read_to_string(p).map_err(anyhow::Error::from)).collect()
}

fn run_review(cfg: &Config, range: &str, main: Option<String>, justification: Option<String>, excerpt: Vec<PathBuf>, json: bool) -> Result<i32> {
	let (base, head) = split_range(range)?;
	let justification = match justification {
		Some(j) => j,
		None => git::commit_message(&cfg.root, &head)?,
	};
	let context_excerpts = read_excerpts(&excerpt)?;
	let input = ReviewInput { session_id: None, base, head, main, justification, actor: current_actor(cfg), context_excerpts, session_facts: None };

	match review::review(cfg, &input)? {
		None => {
			if json {
				println!("{}", serde_json::json!({ "self_review": true }));
			} else {
				println!("no provider configured for role \"review\": self_review = true");
			}
			Ok(0)
		}
		Some(out) => {
			let decision = review::decide(cfg, &out);
			if json {
				println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "output": out, "decision": decision }))?);
			} else {
				println!("verdict: {:?}  confidence: {:.2}", out.verdict, out.confidence);
				println!("summary: {}", out.summary);
				for f in &out.findings {
					println!("  [{:?}] {}{}: {}", f.kind, f.doc_id, f.section.as_deref().map(|s| format!("#{s}")).unwrap_or_default(), f.reason);
				}
				println!("decision: class={:?} merge_allowed={} ({})", decision.class, decision.merge_allowed, decision.reason);
			}
			let code = if decision.merge_allowed && decision.class == MergeClass::Auto {
				0
			} else if !decision.merge_allowed {
				4
			} else {
				3
			};
			Ok(code)
		}
	}
}

fn run_packet(cfg: &Config, range: &str, main: Option<String>, justification: Option<String>, json: bool) -> Result<i32> {
	let (base, head) = split_range(range)?;
	let justification = match justification {
		Some(j) => j,
		None => git::commit_message(&cfg.root, &head)?,
	};
	let input = ReviewInput { session_id: None, base, head, main, justification, actor: current_actor(cfg), context_excerpts: vec![], session_facts: None };
	let packet = review::gather(cfg, &input)?;
	if json {
		println!("{}", serde_json::to_string_pretty(&packet)?);
	} else {
		println!("{}", packet.render());
	}
	Ok(0)
}

fn run_resolve(cfg: &Config, path: &str, base: &str, ours: &str, theirs: &str, write: bool, json: bool) -> Result<i32> {
	let input = review::gather_conflict(&cfg.root, path, base, ours, theirs)?;
	match review::resolve(cfg, &input)? {
		None => {
			if json {
				println!("{}", serde_json::json!({ "deferred": true }));
			} else {
				println!("no provider configured for role \"resolve\": deferred");
			}
			Ok(0)
		}
		Some(out) => {
			let escalate = review::should_escalate(&out, 1, false, false);
			if write && escalate.is_none() {
				std::fs::write(cfg.root.join(path), &out.content)?;
			}
			if json {
				println!(
					"{}",
					serde_json::to_string_pretty(&serde_json::json!({
						"confidence": out.confidence,
						"contradiction": out.contradiction,
						"summary": out.summary,
						"escalate": escalate,
						"written": write && escalate.is_none(),
					}))?
				);
			} else {
				println!("confidence: {:.2}  contradiction: {}", out.confidence, out.contradiction);
				println!("summary: {}", out.summary);
				match &escalate {
					Some(reason) => println!("escalate: {reason}"),
					None if write => println!("wrote {path}"),
					None => {}
				}
			}
			Ok(if escalate.is_some() { 2 } else { 0 })
		}
	}
}
