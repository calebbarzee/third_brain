//! `kb import`: clap wiring over the `import` module. not yet mounted in `cli::Command`; see
//! this crate's report for the snippet to add there and to `cli::run`.
use crate::config::Config;
use crate::import::{self, ImportOptions, ImportReport};
use crate::model::Layer;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

/// `kb import <dir-or-files...>`: bring existing markdown notes into the knowledge base.
#[derive(Args, Debug)]
pub struct ImportArgs {
	/// files or directories to import, walked recursively for `*.md`
	pub paths: Vec<PathBuf>,
	/// `git mv` a source file instead of copying it, when it lives in this same repository
	#[arg(long = "move")]
	pub move_files: bool,
	/// layer applied to every imported document, overriding validate's default
	#[arg(long)]
	pub layer: Option<String>,
	/// tag applied to every imported document; repeatable
	#[arg(long = "tag")]
	pub tags: Vec<String>,
	/// the session objective; defaults to "import N note(s)"
	#[arg(long)]
	pub session_objective: Option<String>,
	/// settle the session but do not run `kb merge`
	#[arg(long)]
	pub no_merge: bool,
	/// report what would be imported without opening a session or writing anything
	#[arg(long)]
	pub dry_run: bool,
}

pub fn run_import(cfg: &Config, args: ImportArgs, json: bool) -> Result<i32> {
	let layer: Option<Layer> = args.layer.as_deref().map(|s| s.parse::<Layer>()).transpose().map_err(|e: String| anyhow::anyhow!(e))?;
	let opts = ImportOptions {
		paths: args.paths,
		move_files: args.move_files,
		layer,
		tags: args.tags,
		session_objective: args.session_objective,
		no_merge: args.no_merge,
		dry_run: args.dry_run,
	};
	let report = import::import(cfg, &opts)?;
	print_report(&report, json)?;
	Ok(0)
}

fn print_report(report: &ImportReport, json: bool) -> Result<()> {
	if json {
		println!("{}", serde_json::to_string_pretty(report)?);
		return Ok(());
	}
	if report.dry_run {
		println!("dry run: {} file(s) would be imported", report.files_found);
	} else {
		println!("imported {} file(s); {} link(s) rewritten, {} unresolved", report.files_found, report.links_rewritten, report.links_unresolved);
		if let Some(session) = &report.session_id {
			println!("session: {session}");
		}
		match &report.merged_commit {
			Some(commit) => println!("merged: {commit}"),
			None => println!("not merged (--no-merge, or the session needs review)"),
		}
		if let Some(path) = &report.map_path {
			println!("map: {}", path.display());
		}
	}
	if !report.docs.is_empty() {
		println!("{:<40}  {:<28}  title", "old path", "new id");
		for d in &report.docs {
			println!("{:<40}  {:<28}  {}", d.source_path, d.id, d.title);
		}
	}
	Ok(())
}
