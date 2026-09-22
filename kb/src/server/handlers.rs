//! business logic shared by the rest (`server::rest`) and mcp (`server::mcp`) surfaces: search,
//! get, refs, propose, run, and (stubbed) answer, plus the two webhook actions. design.md §18
//! ("mcp surface: search, get, refs, propose, run. rest exposes the same five plus
//! `GET /blob/<sha256>`") and §16 (run log permissions).
//!
//! kept in one place so the json a client gets back is identical whether it asked over http or
//! mcp; `rest` and `mcp` differ only in how they parse the request and shape the transport-level
//! envelope around this module's return value.

use crate::config::Config;
use crate::curate::CurateInput;
use crate::index::{self, LsFilter, SearchMode, SearchOpts};
use crate::provider;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// `query_vec` for `index::search`'s hybrid/vec modes comes from whichever provider is
/// configured for the `embed` role, if any and if it supports `embed`; a missing or
/// non-embedding provider degrades to fts silently (`index::search` already treats a `None`
/// query embedding as "fall back to fts" for any mode that asked for vectors). the `rerank` role
/// is passed through the same way; `Provider::supports_rerank` makes an unsupporting provider a
/// no-op inside `index::search` itself.
pub fn search(cfg: &Config, query: &str, opts: &SearchOpts) -> Result<Value> {
    let conn = index::open_newest(cfg)?;
    let embed_provider = provider::provider_for_role(cfg, "embed").unwrap_or(None);
    let query_vec = match &embed_provider {
        Some(p) if p.supports_embed() => p.embed(&[query.to_string()])?.into_iter().next(),
        _ => None,
    };
    let rerank_provider = provider::provider_for_role(cfg, "rerank").unwrap_or(None);
    let hits = index::search(&conn, query, opts, query_vec.as_deref(), rerank_provider.as_deref())?;
    Ok(serde_json::to_value(hits)?)
}

/// parse the rest/mcp `mode` argument (`"fts"`, `"hybrid"`, or `"vec"`) into `SearchMode`; an
/// absent or unrecognized value is `None`, which `index::search` resolves to hybrid-when-available
/// else fts (design.md §18).
pub fn parse_search_mode(mode: Option<&str>) -> Option<SearchMode> {
    match mode {
        Some("fts") => Some(SearchMode::Fts),
        Some("hybrid") => Some(SearchMode::Hybrid),
        Some("vec") => Some(SearchMode::Vec),
        _ => None,
    }
}

/// `None` when no document matches `id` (the caller turns that into a 404 or a "not found" tool
/// result); an ambiguous prefix still surfaces as an `Err`.
pub fn get(cfg: &Config, id: &str) -> Result<Option<Value>> {
    let conn = index::open_newest(cfg)?;
    let rec = index::get(&conn, id)?;
    rec.map(|r| Ok(serde_json::to_value(r)?)).transpose()
}

pub fn refs(cfg: &Config, id: &str, incoming: bool, depth: u32) -> Result<Value> {
    let conn = index::open_newest(cfg)?;
    let rows = index::refs(&conn, id, incoming, depth)?;
    Ok(serde_json::to_value(rows)?)
}

pub fn docs_list(cfg: &Config, filter: &LsFilter) -> Result<Value> {
    let conn = index::open_newest(cfg)?;
    let rows = index::ls(&conn, filter)?;
    Ok(serde_json::to_value(rows)?)
}

pub fn review_list(cfg: &Config) -> Result<Value> {
    Ok(serde_json::to_value(crate::merge::review_list(cfg)?)?)
}

pub fn runs_list(cfg: &Config, since: Option<Duration>, status: Option<&str>) -> Result<Value> {
    Ok(serde_json::to_value(crate::runlog::verify::list(cfg, since, status)?)?)
}

/// `POST /propose` and the `propose` mcp tool: design.md §18, run under actor
/// `agent:propose@<principal>` so the run log and every policy decision it makes are attributed
/// to the caller, per §16's deferred-prompt rule ("the client acts under the user's principal").
pub fn propose(cfg: &Config, principal: &str, task: &str, text: Option<String>, capture_ids: Vec<String>, dry_run: bool) -> Result<Value> {
    let input = CurateInput { actor: format!("agent:propose@{principal}"), task: task.to_string(), capture_ids, proposal_text: text, dry_run };
    let report = crate::curate::run_with_provider(cfg, input, None)?;
    Ok(serde_json::to_value(report)?)
}

/// `POST /answer` and the `answer` mcp tool: design.md §19 (the `answer` hook) and §27 ("ask a
/// question"). `src/answer/` landed (from a concurrent agent) while this module was being
/// written, so this calls it directly rather than the stub the task's fallback path described.
pub fn answer(cfg: &Config, question: &str, limit: usize) -> Result<Value> {
    let opts = crate::answer::AnswerOpts { limit, ..Default::default() };
    let result = crate::answer::answer(cfg, question, &opts)?;
    Ok(serde_json::to_value(result)?)
}

/// `POST /webhook/source/<name>`: store the raw body as a capture, source `webhook:<name>`,
/// media type `application/json` (design.md §19's `capture` hook, triggered here by a source
/// webhook instead of a scheduler).
pub fn webhook_capture(cfg: &Config, source_name: &str, body: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(body).into_owned();
    let cap = crate::store::catalog::put_capture(
        cfg,
        crate::store::catalog::PutCapture {
            source: format!("webhook:{source_name}"),
            source_uri: format!("webhook:{source_name}:{}", crate::model::new_ulid()),
            source_cursor: None,
            media_type: "application/json".to_string(),
            title: None,
            original: body.to_vec(),
            text,
            captured_at: None,
        },
    )?;
    Ok(serde_json::to_value(cap)?)
}

// ---------------------------------------------------------------------------------------------
// run log permissions (design.md §16: "a run log carries the permissions of the documents it
// touched ... a reader lacking any of them sees the run row (status, actor, times) and a count
// of withheld documents")
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct RunSummary {
    run_id: String,
    actor: String,
    principal: Option<String>,
    skill: Option<String>,
    status: String,
    started_at: String,
    finished_at: Option<String>,
}

#[allow(clippy::type_complexity)]
fn read_run_row(path: &Path) -> Result<(RunSummary, Vec<String>)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
    let (run_id, actor, principal, skill, status, started_at, finished_at, access_json): (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        String,
    ) = conn.query_row("select run_id, actor, principal, skill, status, started_at, finished_at, access_read from run limit 1", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
    })?;
    let access_read: Vec<String> = serde_json::from_str(&access_json).unwrap_or_default();
    Ok((RunSummary { run_id, actor, principal, skill, status, started_at, finished_at }, access_read))
}

/// count of documents the run touched, via `event.doc_id` (a generated column already present on
/// every event whose payload carries one -- `op`, `read`, `policy_decision`). used only for the
/// withheld count shown to a reader who cannot see the full timeline.
fn touched_doc_count(path: &Path) -> Result<i64> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
    conn.query_row("select count(distinct doc_id) from event where doc_id is not null", [], |r| r.get(0)).context("counting touched documents")
}

/// `GET /run/<id>` and the `run` mcp tool. `reader_groups` is the caller's principal's groups
/// (`config::Config::groups_of`); access is granted when the run's `access_read` union is empty
/// (nothing recorded, so nothing to withhold), contains the `"*"` wildcard (design.md's
/// `access.read` default, meaning any reader), or shares at least one group with the reader --
/// the same "any listed group may read" semantics `model::Access::read` uses on one document.
/// this is a coarser check than resolving each touched document's own `access.read` individually
/// (the run row only stores their union), a known simplification -- see this crate's report.
pub fn run_show(cfg: &Config, reader_groups: &[String], id: &str) -> Result<Value> {
    let path = crate::runlog::resolve_run_path(cfg, id)?;
    let (summary, access_read) = read_run_row(&path)?;
    let allowed = access_read.is_empty() || access_read.iter().any(|g| g == "*" || reader_groups.contains(g));
    if !allowed {
        let withheld = touched_doc_count(&path)?;
        return Ok(json!({
            "run_id": summary.run_id,
            "status": summary.status,
            "actor": summary.actor,
            "started_at": summary.started_at,
            "finished_at": summary.finished_at,
            "withheld_documents": withheld,
        }));
    }
    let timeline = crate::runlog::verify::timeline(&path, false)?;
    Ok(json!({ "run": summary, "events": timeline }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runlog::{RunLog, RunStart, RunStatus};

    fn make_run(cfg: &Config, access_read: &[&str]) -> String {
        let mut log = RunLog::create(cfg, RunStart { actor: "agent:curate@caleb".into(), principal: Some("caleb".into()), ..Default::default() }).unwrap();
        let run_id = log.run_id().to_string();
        log.op(crate::model::Operation::Append, "01K5A7Q2XW3ZRCM8T6YB4VJH0A", None, None, Some("h"), None).unwrap();
        let access: Vec<String> = access_read.iter().map(|s| s.to_string()).collect();
        log.run_end(RunStatus::Ok, None, None, None, &[], &access).unwrap();
        log.seal(cfg).unwrap();
        run_id
    }

    #[test]
    fn run_show_full_timeline_when_reader_has_a_matching_group() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let run_id = make_run(&cfg, &["platform"]);
        let out = run_show(&cfg, &["platform".to_string()], &run_id).unwrap();
        assert!(out.get("events").is_some(), "{out}");
    }

    #[test]
    fn run_show_withholds_when_reader_has_no_matching_group() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let run_id = make_run(&cfg, &["restricted"]);
        let out = run_show(&cfg, &["other".to_string()], &run_id).unwrap();
        assert!(out.get("events").is_none(), "{out}");
        assert_eq!(out["withheld_documents"], 1);
        assert_eq!(out["status"], "ok");
    }

    #[test]
    fn run_show_open_to_everyone_when_access_read_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let run_id = make_run(&cfg, &[]);
        let out = run_show(&cfg, &[], &run_id).unwrap();
        assert!(out.get("events").is_some());
    }
}
