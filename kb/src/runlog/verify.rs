//! read-only inspection of a run log file: hash chain verification, the op diff list, fts
//! search, a human-readable timeline, and a directory listing across open and sealed runs.
//! design.md §16's `kb run verify|diff|search|list` and the `show` timeline live on top of these.
use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// result of recomputing a run log's hash chain from its stored events.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub events: u64,
    pub first_bad_seq: Option<i64>,
    pub final_hash_matches: bool,
}

/// recompute every event's hash from `(prev_hash, kind, ts, payload)` and compare it to the
/// stored `hash`, and compare `prev_hash` to the previous event's stored `hash` (or the genesis
/// value for the first event). `ok` is true only when every link matches and the last event's
/// hash equals `run.final_hash`.
pub fn verify(path: &Path) -> Result<VerifyReport> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
    let run_id: String = conn.query_row("select run_id from run limit 1", [], |r| r.get(0)).context("reading run row")?;
    let stored_final_hash: Option<String> =
        conn.query_row("select final_hash from run where run_id = ?1", params![run_id], |r| r.get(0)).context("reading final_hash")?;

    let mut stmt = conn.prepare("select seq, kind, ts, payload, prev_hash, hash from event where run_id = ?1 order by seq asc")?;
    let mut rows = stmt.query(params![run_id])?;

    let mut expected_prev = super::genesis_hash(&run_id);
    let mut events: u64 = 0;
    let mut first_bad_seq: Option<i64> = None;
    let mut last_hash = expected_prev.clone();

    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let kind: String = row.get(1)?;
        let ts: String = row.get(2)?;
        let payload: String = row.get(3)?;
        let prev_hash: String = row.get(4)?;
        let hash: String = row.get(5)?;
        events += 1;

        let recomputed = super::compute_hash(&prev_hash, &kind, &ts, &payload);
        let link_ok = prev_hash == expected_prev && recomputed == hash;
        if !link_ok && first_bad_seq.is_none() {
            first_bad_seq = Some(seq);
        }
        expected_prev = hash.clone();
        last_hash = hash;
    }

    let final_hash_matches = stored_final_hash.as_deref() == Some(last_hash.as_str());
    let ok = first_bad_seq.is_none() && final_hash_matches;
    Ok(VerifyReport { ok, events, first_bad_seq, final_hash_matches })
}

/// one `op` row, joined with nothing else: the before/after content hashes are the whole point
/// of `kb run diff`.
#[derive(Debug, Clone, Serialize)]
pub struct DiffRow {
    pub op_id: i64,
    pub event_seq: i64,
    pub kind: String,
    pub doc_id: Option<String>,
    pub section: Option<String>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub diff_blob: Option<String>,
}

pub fn diff(path: &Path) -> Result<Vec<DiffRow>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
    let mut stmt = conn.prepare("select op_id, event_seq, kind, doc_id, section, before_hash, after_hash, diff_blob from op order by op_id asc")?;
    let rows = stmt.query_map([], |r| {
        Ok(DiffRow {
            op_id: r.get(0)?,
            event_seq: r.get(1)?,
            kind: r.get(2)?,
            doc_id: r.get(3)?,
            section: r.get(4)?,
            before_hash: r.get(5)?,
            after_hash: r.get(6)?,
            diff_blob: r.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("reading op rows")
}

/// one fts5 hit: the event it came from, plus a `snippet()` excerpt of the matching payload.
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub seq: i64,
    pub kind: String,
    pub ts: String,
    pub snippet: String,
}

pub fn search(path: &Path, needle: &str) -> Result<Vec<SearchHit>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
    let mut stmt = conn.prepare(
        "select e.seq, e.kind, e.ts, snippet(event_fts, 0, '[', ']', '...', 8)
         from event_fts join event e on e.seq = event_fts.rowid
         where event_fts match ?1
         order by e.seq asc",
    )?;
    let rows = stmt.query_map(params![needle], |r| Ok(SearchHit { seq: r.get(0)?, kind: r.get(1)?, ts: r.get(2)?, snippet: r.get(3)? }))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().context("running fts search")
}

/// one line of `kb run show`: the summary omits reasoning blocks from `llm_response` payloads
/// unless `with_reasoning` was set, since those can be large and are rarely needed at a glance.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineRow {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub summary: String,
}

pub fn timeline(path: &Path, with_reasoning: bool) -> Result<Vec<TimelineRow>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
    let mut stmt = conn.prepare("select seq, ts, kind, payload from event order by seq asc")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let ts: String = row.get(1)?;
        let kind: String = row.get(2)?;
        let payload: String = row.get(3)?;
        out.push(TimelineRow { seq, ts, kind: kind.clone(), summary: summarize(&kind, &payload, with_reasoning) });
    }
    Ok(out)
}

const SUMMARY_MAX: usize = 200;

fn summarize(kind: &str, payload: &str, with_reasoning: bool) -> String {
    let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let line = match kind {
        "tool_call" => {
            let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{name} {}", compact(value.get("input")))
        }
        "op" => {
            let op_kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let doc_id = value.get("doc_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{op_kind} {doc_id}")
        }
        "llm_response" if !with_reasoning => {
            let kinds: Vec<&str> = value
                .get("blocks")
                .and_then(|b| b.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(|t| t.as_str()) != Some("reasoning"))
                        .filter_map(|b| b.get("type").and_then(|t| t.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            format!("blocks: {}", kinds.join(", "))
        }
        _ => compact(Some(&value)),
    };
    truncate(&line, SUMMARY_MAX)
}

fn compact(v: Option<&Value>) -> String {
    v.map(|v| serde_json::to_string(v).unwrap_or_default()).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

/// one row of `kb run list`.
#[derive(Debug, Clone, Serialize)]
pub struct RunListRow {
    pub run_id: String,
    pub actor: String,
    pub skill: Option<String>,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub path: PathBuf,
}

/// every run under `<state>/runs/` and `<state>/store/runs/`, newest-started first, optionally
/// filtered to runs started within `since` of now and/or matching `status` exactly. a run that
/// exists in both directories (open, then sealed into the store) is listed once, preferring the
/// `<state>/runs/` copy.
pub fn list(cfg: &Config, since: Option<Duration>, status: Option<&str>) -> Result<Vec<RunListRow>> {
    let cutoff = since.and_then(|d| chrono::Duration::from_std(d).ok()).map(|d| Utc::now() - d);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in [super::runs_dir(cfg), super::store_runs_dir(cfg)] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sqlite") {
                continue;
            }
            let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) else { continue };
            let row = conn.query_row("select run_id, actor, skill, status, started_at, finished_at from run limit 1", [], |r| {
                Ok(RunListRow { run_id: r.get(0)?, actor: r.get(1)?, skill: r.get(2)?, status: r.get(3)?, started_at: r.get(4)?, finished_at: r.get(5)?, path: path.clone() })
            });
            let Ok(row) = row else { continue };
            if !seen.insert(row.run_id.clone()) {
                continue;
            }
            if let Some(s) = status
                && row.status != s
            {
                continue;
            }
            if let Some(cutoff) = cutoff {
                let started = DateTime::parse_from_rfc3339(&row.started_at).ok().map(|d| d.with_timezone(&Utc));
                if started.is_none_or(|d| d < cutoff) {
                    continue;
                }
            }
            out.push(row);
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(out)
}
