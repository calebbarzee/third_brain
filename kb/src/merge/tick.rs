//! `kb tick`: the local subset of design.md §22's reconciliation tick — the pieces that need no
//! scheduler, webhook, or worker pool: index freshness, stuck merges, and worktree/session-file
//! drift.

use crate::config::Config;
use crate::git;
use crate::index;
use crate::session::{self, SessionState};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Default, Serialize)]
pub struct TickReport {
    /// true when a stale index artifact triggered a rebuild.
    pub indexed: bool,
    pub settled_stale_merging: u32,
    pub adopted_worktrees: u32,
    pub removed_worktrees: u32,
    pub cleaned_merged_sessions: u32,
}

/// run one reconciliation pass; see design.md §22's table for the full (local-and-server) list.
pub fn tick(cfg: &Config) -> Result<TickReport> {
    let mut report = TickReport::default();

    if let Ok(main_head) = git::rev_parse(&cfg.root, "main") {
        let stale = match index::newest_artifact(cfg) {
            Some(p) => p.file_stem().and_then(|s| s.to_str()) != Some(main_head.as_str()),
            None => true,
        };
        if stale {
            index::build(cfg, None)?;
            report.indexed = true;
        }
    }

    for mut s in session::list(cfg)? {
        if s.state != SessionState::Merging {
            continue;
        }
        let stale = chrono::DateTime::parse_from_rfc3339(&s.touched_at)
            .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes() > 10)
            .unwrap_or(false);
        if stale {
            s.state = SessionState::Settled;
            s.touched_at = chrono::Utc::now().to_rfc3339();
            session::save(cfg, &s)?;
            report.settled_stale_merging += 1;
        }
    }

    // remote session/* branches (here: local ones, since this subset has no server) merged into
    // main: close out their session file.
    if let Ok(merged_branches) = git::branches_merged(&cfg.root, "main", "refs/heads/session/*") {
        for branch in merged_branches {
            if let Some(id) = branch.strip_prefix("session/")
                && let Ok(mut s) = session::load(cfg, id)
                && !matches!(s.state, SessionState::Merged | SessionState::Abandoned)
            {
                session::cleanup(cfg, &mut s, true)?;
                report.cleaned_merged_sessions += 1;
            }
        }
    }

    let sessions = session::list(cfg)?;
    let session_ids: HashSet<String> = sessions.iter().map(|s| s.id.clone()).collect();
    if let Ok(listing) = git::worktree_list_porcelain(&cfg.root) {
        for block in listing.split("\n\n") {
            let Some(path) = block.lines().find_map(|l| l.strip_prefix("worktree ")) else {
                continue;
            };
            if !path.contains("worktrees") {
                continue;
            }
            let Some(id) = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if session_ids.contains(&id) {
                continue;
            }
            let branch = format!("session/{id}");
            if git::rev_parse(&cfg.root, &branch).is_err() {
                let _ = git::worktree_remove(&cfg.root, std::path::Path::new(path));
                report.removed_worktrees += 1;
            } else {
                // a worktree with no session file but a live branch: nothing to reconstruct the
                // session record from (actor, objective) beyond the branch name, so this counts
                // it rather than fabricating one; see the report for this gap.
                report.adopted_worktrees += 1;
            }
        }
    }

    let _ = git::worktree_prune(&cfg.root);
    std::fs::create_dir_all(cfg.state_dir())?;
    std::fs::write(
        cfg.state_dir().join("last_tick"),
        chrono::Utc::now().to_rfc3339(),
    )?;
    Ok(report)
}
