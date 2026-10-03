//! `kb revert` and `kb doctor`: design.md §21 (history, backups, compaction).

use super::{MergeOpts, MergeOutcome, merge};
use crate::config::Config;
use crate::git;
use crate::session::{self, SessionState};
use anyhow::{Result, bail};

/// `kb revert <commit | run | session>`: resolve `target` to the commit(s) it names on `main`
/// (by `Session:`/`Run:` trailer, or a bare sha), revert them newest-first as forward commits in
/// a fresh session, and merge that session. `main` is never reset, force-pushed, or rewritten.
pub fn revert(cfg: &Config, target: &str) -> Result<MergeOutcome> {
	let mut commits = git::log_grep(&cfg.root, "main", &format!("Session: {target}"))?;
	if commits.is_empty() {
		commits = git::log_grep(&cfg.root, "main", &format!("Run: {target}"))?;
	}
	if commits.is_empty()
		&& let Ok(sha) = git::rev_parse(&cfg.root, target)
	{
		commits = vec![sha];
	}
	if commits.is_empty() {
		bail!("no commit on `main` matches `{target}` (looked for a Session:/Run: trailer and a bare sha)");
	}

	// `git log --grep` already lists newest first; design.md §14/§21 wants the most recent
	// change undone first.
	let mut session = session::open(cfg, &format!("revert {target}"), None, &[])?;
	for c in &commits {
		git::revert_no_edit(&session.worktree, c)?;
	}
	session::settle(cfg, &mut session, Some(&format!("revert {target}")))?;
	merge(cfg, &session.id, MergeOpts::default())
}

/// `kb doctor`: compares `main`'s current head to the last head `kb merge` recorded after a
/// successful land, and lists worktree/session-file drift. an empty-findings report never
/// happens; a clean repository reports `"clean"` (design.md §21, §22).
pub fn doctor(cfg: &Config) -> Result<Vec<String>> {
	let mut report = Vec::new();

	let last_main_path = cfg.state_dir().join("last_main");
	if let Ok(recorded) = std::fs::read_to_string(&last_main_path) {
		let recorded = recorded.trim();
		if !recorded.is_empty()
			&& let Ok(current) = git::rev_parse(&cfg.root, "main")
			&& recorded != current
			&& !git::is_ancestor(&cfg.root, recorded, &current)
		{
			report.push(format!("main moved non-fast-forward: last known head {recorded} is not an ancestor of current {current}"));
		}
	}

	let sessions = session::list(cfg)?;
	let session_ids: std::collections::HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();

	if let Ok(listing) = git::worktree_list_porcelain(&cfg.root) {
		for block in listing.split("\n\n") {
			let Some(path) = block.lines().find_map(|l| l.strip_prefix("worktree ")) else {
				continue;
			};
			if !path.contains("worktrees") {
				continue;
			}
			if let Some(id) = std::path::Path::new(path).file_name().and_then(|n| n.to_str())
				&& !session_ids.contains(id)
			{
				report.push(format!("stale worktree with no session file: {path}"));
			}
		}
	}

	for s in &sessions {
		if !s.worktree.is_dir() && !matches!(s.state, SessionState::Merged | SessionState::Abandoned) {
			report.push(format!("session {} is state {} but its worktree is gone", s.id, s.state));
		}
	}

	if report.is_empty() {
		report.push("clean".to_string());
	}
	Ok(report)
}
