//! `kb maintain compact`: design.md §21's compaction table. runs only after a `checkpoint/<date>`
//! tag exists and the backups manifest shows that commit mirrored and bundled (skipped, with a
//! warning, when no backup is configured at all -- a local-only repository may have none). never
//! touches anything reachable from `main`.

use crate::backup;
use crate::config::Config;
use crate::git;
use crate::session::{self, SessionState};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Default)]
pub struct CompactOpts {
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CompactReport {
    /// `None` when compaction ran; `Some(reason)` when it refused or skipped a precondition.
    pub skipped: Option<String>,
    pub warnings: Vec<String>,
    pub checkpoint_tag: Option<String>,
    pub deleted_session_branches: Vec<String>,
    pub refused_session_branches: Vec<String>,
    pub repacked: bool,
    pub commit_graph_written: bool,
    pub gc_ran: bool,
    pub moved_session_files: usize,
    pub index_artifacts_removed: usize,
}

fn newest_checkpoint_tag(repo: &Path) -> Option<String> {
    git::list_refs(repo, "refs/tags/checkpoint/*").ok()?.into_iter().max()
}

/// `kb maintain compact`.
pub fn compact(cfg: &Config, opts: CompactOpts) -> Result<CompactReport> {
    let mut report = CompactReport::default();

    let Some(tag) = newest_checkpoint_tag(&cfg.root) else {
        report.skipped = Some("no checkpoint/<date> tag exists; run `kb maintain nightly` for a clean pass first".to_string());
        return Ok(report);
    };
    report.checkpoint_tag = Some(tag.clone());
    let checkpoint_commit = git::tag_commit(&cfg.root, &tag)?;

    let manifest = backup::manifest(cfg)?;
    match (&manifest.last_mirrored, &manifest.last_bundled) {
        (None, None) => {
            report.warnings.push("no backup configured (backup.toml has no mirror or bundle_dir); compacting without a verified backup".to_string());
        }
        (mirrored, bundled) => {
            let mirrored_ok = mirrored.as_deref().is_some_and(|m| checkpoint_ancestor_ok(&cfg.root, &checkpoint_commit, m));
            let bundled_ok = bundled.as_deref().is_some_and(|b| checkpoint_ancestor_ok(&cfg.root, &checkpoint_commit, b));
            if mirrored.is_some() && !mirrored_ok {
                report.skipped = Some(format!("checkpoint {tag} ({checkpoint_commit}) is not yet reflected in the last mirror push"));
                return Ok(report);
            }
            if bundled.is_some() && !bundled_ok {
                report.skipped = Some(format!("checkpoint {tag} ({checkpoint_commit}) is not yet covered by the last bundle"));
                return Ok(report);
            }
            if mirrored.is_none() {
                report.warnings.push("no mirror configured in backup.toml".to_string());
            }
            if bundled.is_none() {
                report.warnings.push("no bundle_dir configured in backup.toml".to_string());
            }
        }
    }

    if opts.dry_run {
        return Ok(report);
    }

    git::worktree_prune(&cfg.root)?;

    // merged `session/*` branches: delete the branch and its hidden base ref, but refuse when the
    // session's run log exists and is not yet sealed (design.md §21's "never drop a session's
    // `wip` refs before its log is sealed" -- the hidden base ref is this crate's stand-in for
    // that, since a squashed session leaves no other `wip` state to protect).
    let merged = git::branches_merged(&cfg.root, "main", "refs/heads/session/*").unwrap_or_default();
    for branch in merged {
        let Some(id) = branch.strip_prefix("session/") else { continue };
        if let Ok(sess) = session::load(cfg, id)
            && let Some(run_id) = &sess.run_id
            && !run_log_sealed(cfg, run_id)
        {
            report.refused_session_branches.push(format!("{branch}: run {run_id} is not sealed yet"));
            continue;
        }
        if git::branch_delete(&cfg.root, &branch).is_ok() {
            let _ = git::delete_ref(&cfg.root, &format!("refs/kb/sessions/{id}/base"));
            report.deleted_session_branches.push(branch);
        }
    }

    report.repacked = git::repack(&cfg.root).is_ok();
    report.commit_graph_written = git::commit_graph_write_reachable(&cfg.root).is_ok();
    let _ = git::multi_pack_index_write(&cfg.root);
    report.gc_ran = git::gc_auto(&cfg.root).is_ok();

    report.moved_session_files = move_closed_sessions(cfg)?;
    report.index_artifacts_removed = enforce_index_retention(cfg)?;

    Ok(report)
}

fn checkpoint_ancestor_ok(repo: &Path, checkpoint_commit: &str, backed_up_commit: &str) -> bool {
    checkpoint_commit == backed_up_commit || git::is_ancestor(repo, checkpoint_commit, backed_up_commit)
}

fn run_log_sealed(cfg: &Config, run_id: &str) -> bool {
    // a sealed run log lives under `<state>/store/runs/`; an id prefix is enough since
    // `runlog::resolve_run_path` accepts one too, but compaction only ever has the full id here.
    let dir = cfg.state_dir().join("store").join("runs");
    std::fs::read_dir(&dir)
        .map(|entries| {
            entries.flatten().any(|e| e.path().file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with(run_id)))
        })
        .unwrap_or(false)
}

/// move session files in state `merged` or `abandoned`, untouched for 30 days, from
/// `<state>/sessions/` to `<state>/store/sessions/` (design.md §21).
fn move_closed_sessions(cfg: &Config) -> Result<usize> {
    let src_dir = cfg.state_dir().join("sessions");
    let dst_dir = cfg.state_dir().join("store").join("sessions");
    if !src_dir.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(&dst_dir)?;
    let mut moved = 0;
    for sess in session::list(cfg)? {
        if !matches!(sess.state, SessionState::Merged | SessionState::Abandoned) {
            continue;
        }
        let reference_ts = sess.abandoned_at.as_deref().unwrap_or(&sess.touched_at);
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(reference_ts) else { continue };
        let age_days = (chrono::Utc::now() - ts.with_timezone(&chrono::Utc)).num_days();
        if age_days < 30 {
            continue;
        }
        let src = src_dir.join(format!("{}.toml", sess.id));
        let dst = dst_dir.join(format!("{}.toml", sess.id));
        if src.is_file() && std::fs::rename(&src, &dst).is_ok() {
            moved += 1;
        }
    }
    Ok(moved)
}

/// keep the last 20 index artifacts plus one per day for 90 days, mirroring
/// `index::build`'s (private) retention rule; see this crate's report.
fn enforce_index_retention(cfg: &Config) -> Result<usize> {
    let dir = cfg.state_dir().join("index");
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut artifacts: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".sqlite")) {
            let mtime = entry.metadata()?.modified()?;
            artifacts.push((path, mtime));
        }
    }
    artifacts.sort_by(|a, b| b.1.cmp(&a.1));
    const DAY_SECS: u64 = 86_400;
    let now = SystemTime::now();
    let mut keep: HashSet<PathBuf> = HashSet::new();
    let mut days_seen: HashSet<i64> = HashSet::new();
    for (path, mtime) in artifacts.iter().take(20) {
        keep.insert(path.clone());
        days_seen.insert(day_index(*mtime));
    }
    for (path, mtime) in artifacts.iter().skip(20) {
        let age = now.duration_since(*mtime).unwrap_or_default();
        if age.as_secs() > 90 * DAY_SECS {
            continue;
        }
        if days_seen.insert(day_index(*mtime)) {
            keep.insert(path.clone());
        }
    }
    let mut removed = 0;
    for (path, _) in &artifacts {
        if !keep.contains(path) && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn day_index(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs() as i64 / 86_400
}
