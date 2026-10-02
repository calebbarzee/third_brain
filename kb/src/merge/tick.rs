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
    /// orphan kb worktrees left in place because they hold uncommitted work.
    pub kept_dirty_worktrees: u32,
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
            .map(|t| (crate::clock::now() - t.with_timezone(&chrono::Utc)).num_minutes() > 10)
            .unwrap_or(false);
        if stale {
            s.state = SessionState::Settled;
            s.touched_at = crate::clock::now().to_rfc3339();
            session::save(cfg, &s)?;
            report.settled_stale_merging += 1;
        }
    }

    // remote session/* branches (here: local ones, since this subset has no server) merged into
    // main: close out their session file.
    // a session counts as landed only when it was settled and its commits are reachable from
    // main. a fresh open session sits at its base commit, which git also reports as "merged", so
    // state and head-versus-base are checked before anything is removed.
    if let Ok(merged_branches) = git::branches_merged(&cfg.root, "main", "refs/heads/session/*") {
        for branch in merged_branches {
            if let Some(id) = branch.strip_prefix("session/")
                && let Ok(mut s) = session::load(cfg, id)
                && matches!(s.state, SessionState::Settled | SessionState::Merging)
                && let Ok(head) = git::rev_parse(&cfg.root, &branch)
                && head != s.base_commit
                && git::is_ancestor(&cfg.root, &head, "main")
            {
                session::cleanup(cfg, &mut s, true)?;
                report.cleaned_merged_sessions += 1;
            }
        }
    }

    let sessions = session::list(cfg)?;
    let session_ids: HashSet<String> = sessions.iter().map(|s| s.id.clone()).collect();
    // only worktrees kb itself created, directly under `<state>/worktrees/`, are candidates; any
    // other worktree of this repository (an editor's, an agent harness's) is not kb's to touch.
    let kb_worktrees = std::fs::canonicalize(cfg.state_dir().join("worktrees")).ok();
    if let (Ok(listing), Some(kb_worktrees)) = (git::worktree_list_porcelain(&cfg.root), kb_worktrees) {
        for block in listing.split("\n\n") {
            let Some(path) = block.lines().find_map(|l| l.strip_prefix("worktree ")) else {
                continue;
            };
            let path = std::path::Path::new(path);
            let Ok(canonical) = std::fs::canonicalize(path) else {
                continue;
            };
            if canonical.parent() != Some(kb_worktrees.as_path()) {
                continue;
            }
            let Some(id) = canonical.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            if !crate::model::is_ulid(&id) || session_ids.contains(&id) {
                continue;
            }
            let branch = format!("session/{id}");
            if git::rev_parse(&cfg.root, &branch).is_err() {
                match git::worktree_remove(&cfg.root, path) {
                    Ok(()) => report.removed_worktrees += 1,
                    Err(_) => report.kept_dirty_worktrees += 1,
                }
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
        crate::clock::now().to_rfc3339(),
    )?;
    Ok(report)
}
