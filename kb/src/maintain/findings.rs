//! nightly curation detectors: design.md §15.3's finding table, one function per row.
//!
//! every detector reads the newest index artifact (for structural facts) plus, for a few, the
//! repository's git history directly (for provenance and the two review-backed detectors).
//! `detect` runs the whole table (or one kind, for `--only`) and returns every `Finding`;
//! `auto_fix` is filled only when the corpus is at or above `manifest.toml`'s `min_corpus` --
//! `nightly` (in `mod.rs`) is the only caller that decides what to do with the result.

use crate::config::Config;
use crate::git;
use crate::model::{self, Layer, MergeClass, Operation, Ref};
use crate::curate::ops::ProposedOp;
use crate::provider;
use crate::review;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// design.md §15.3's finding table, one variant per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingKind {
    Contradiction,
    Duplication,
    Stale,
    Drift,
    Orphan,
    OverBudget,
    MissingProvenance,
    BrokenGraph,
    LongChain,
    BadMergedChange,
    SupersededPastDeleteDays,
    DraftPastTtl,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::Contradiction => "contradiction",
            FindingKind::Duplication => "duplication",
            FindingKind::Stale => "stale",
            FindingKind::Drift => "drift",
            FindingKind::Orphan => "orphan",
            FindingKind::OverBudget => "over-budget",
            FindingKind::MissingProvenance => "missing-provenance",
            FindingKind::BrokenGraph => "broken-graph",
            FindingKind::LongChain => "long-chain",
            FindingKind::BadMergedChange => "bad-merged-change",
            FindingKind::SupersededPastDeleteDays => "superseded-past-delete-days",
            FindingKind::DraftPastTtl => "draft-past-ttl",
        }
    }
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FindingKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "contradiction" => Ok(FindingKind::Contradiction),
            "duplication" => Ok(FindingKind::Duplication),
            "stale" => Ok(FindingKind::Stale),
            "drift" => Ok(FindingKind::Drift),
            "orphan" => Ok(FindingKind::Orphan),
            "over-budget" | "over_budget" => Ok(FindingKind::OverBudget),
            "missing-provenance" | "missing_provenance" => Ok(FindingKind::MissingProvenance),
            "broken-graph" | "broken_graph" => Ok(FindingKind::BrokenGraph),
            "long-chain" | "long_chain" => Ok(FindingKind::LongChain),
            "bad-merged-change" | "bad_merged_change" => Ok(FindingKind::BadMergedChange),
            "superseded-past-delete-days" | "superseded_past_delete_days" => Ok(FindingKind::SupersededPastDeleteDays),
            "draft-past-ttl" | "draft_past_ttl" => Ok(FindingKind::DraftPastTtl),
            other => Err(format!("unknown finding kind `{other}`")),
        }
    }
}

/// one detector's output: what is wrong, which documents it names, and (when the corpus is large
/// enough and a fix exists) the operation that would resolve it.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub doc_ids: Vec<String>,
    pub detail: String,
    pub band: Option<MergeClass>,
    pub auto_fix: Option<ProposedOp>,
}

/// run every detector (or just `only`), against the newest index artifact `conn` and the
/// repository at `repo`. `since_commit` bounds the two review-backed detectors (contradiction,
/// bad merged change) to commits landed after it; `nightly::resolve_since` computes it.
pub fn detect(cfg: &Config, conn: &Connection, repo: &Path, since_commit: &str, only: Option<FindingKind>) -> Result<Vec<Finding>> {
    let enabled = maintenance_enabled(conn);
    let mut out = Vec::new();
    let want = |k: FindingKind| only.is_none_or(|o| o == k);

    if want(FindingKind::Stale) {
        out.extend(detect_stale(conn, enabled)?);
    }
    if want(FindingKind::Orphan) {
        out.extend(detect_orphan(conn, cfg, enabled)?);
    }
    if want(FindingKind::Drift) {
        out.extend(detect_drift(conn, enabled)?);
    }
    if want(FindingKind::OverBudget) {
        out.extend(detect_over_budget(conn, cfg)?);
    }
    if want(FindingKind::MissingProvenance) {
        out.extend(detect_missing_provenance(conn, repo, enabled)?);
    }
    if want(FindingKind::BrokenGraph) {
        out.extend(detect_broken_graph(conn)?);
    }
    if want(FindingKind::LongChain) {
        out.extend(detect_long_chain(conn, enabled)?);
    }
    if want(FindingKind::SupersededPastDeleteDays) {
        out.extend(detect_superseded_past_delete_days(conn, cfg, enabled)?);
    }
    if want(FindingKind::DraftPastTtl) {
        out.extend(detect_draft_past_ttl(conn, cfg, enabled)?);
    }
    if want(FindingKind::Duplication) {
        out.extend(detect_duplication(conn, cfg)?);
    }
    // both detectors re-review the same merged commits; each commit is reviewed once
    let mut reviews: HashMap<String, Option<review::ReviewOutput>> = HashMap::new();
    if want(FindingKind::Contradiction) {
        out.extend(detect_contradiction(cfg, repo, since_commit, &mut reviews)?);
    }
    if want(FindingKind::BadMergedChange) {
        out.extend(detect_bad_merged_change(cfg, repo, since_commit, enabled, &mut reviews)?);
    }
    Ok(out)
}

/// below `manifest.toml`'s `min_corpus` canon documents, `index::build` already writes
/// `maintenance_enabled = "0"` to `meta` (design.md §17.4); nightly curation only escalates in
/// that case (design.md §15.3), so every detector consults this before filling `auto_fix`.
fn maintenance_enabled(conn: &Connection) -> bool {
    conn.query_row("select value from meta where key = 'maintenance_enabled'", [], |r| r.get::<_, String>(0)).map(|v| v == "1").unwrap_or(false)
}

fn gate_fix(enabled: bool, fix: ProposedOp) -> Option<ProposedOp> {
    enabled.then_some(fix)
}

/// a toml datetime as it appears inside `documents.frontmatter` json: `toml::value::Datetime`
/// serializes through serde as `{"$__toml_private_datetime": "<text>"}` (the `toml` crate's
/// generic round-trip marker), not as a plain string, so `review_by` is read with this helper
/// rather than `Value::as_str`.
fn toml_datetime_str(v: &serde_json::Value) -> Option<&str> {
    v.get("$__toml_private_datetime").and_then(serde_json::Value::as_str)
}

fn parse_toml_datetime(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

fn review_by_of(fm_json: &str) -> Option<DateTime<Utc>> {
    let v: serde_json::Value = serde_json::from_str(fm_json).ok()?;
    let s = toml_datetime_str(v.get("review_by")?)?;
    parse_toml_datetime(s)
}

fn has_source_ref(fm_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(fm_json) else { return false };
    v.get("refs").and_then(|r| r.as_array()).is_some_and(|refs| refs.iter().any(|r| r.get("rel").and_then(|x| x.as_str()) == Some("source")))
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn age_days(since: &DateTime<Utc>) -> i64 {
    (crate::clock::now() - *since).num_days()
}

// -------------------------------------------------------------------------------------------
// stale: review_by passed, no reads and no cites in 90 days (design.md §15.3, §17.4).
// -------------------------------------------------------------------------------------------

fn detect_stale(conn: &Connection, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "select d.id, d.title, d.frontmatter, g.reads_90d, g.cites_90d
         from documents d join graph_metrics g on g.doc_id = d.id
         where d.kind = 'document' and d.status = 'canon'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
    })?;
    let now = crate::clock::now();
    for row in rows {
        let (id, title, fm_json, reads, cites) = row?;
        if reads != 0 || cites != 0 {
            continue;
        }
        let Some(review_by) = review_by_of(&fm_json) else { continue };
        if review_by > now {
            continue;
        }
        let detail = format!("{} ({id}): review_by {} passed with 0 reads_90d and 0 cites_90d", title.unwrap_or_default(), review_by.date_naive());
        let fix = ProposedOp {
            op: Operation::Retire,
            target_id: Some(id.clone()),
            justification: "nightly: stale (review_by passed, no reads or cites in 90 days)".to_string(),
            confidence: 1.0,
            ..Default::default()
        };
        out.push(Finding { kind: FindingKind::Stale, doc_ids: vec![id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// orphan: no canon dependents, no code dependents, no reads in 90 days, older than orphan_days.
// -------------------------------------------------------------------------------------------

fn detect_orphan(conn: &Connection, cfg: &Config, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "select d.id, d.title, d.created, g.in_canon, g.in_code, g.reads_90d
         from documents d join graph_metrics g on g.doc_id = d.id
         where d.kind = 'document' and d.status = 'canon'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?))
    })?;
    let orphan_days = cfg.manifest.thresholds.orphan_days as i64;
    for row in rows {
        let (id, title, created, in_canon, in_code, reads) = row?;
        if in_canon != 0 || in_code != 0 || reads != 0 {
            continue;
        }
        let Some(created) = created.as_deref().and_then(parse_rfc3339) else { continue };
        if age_days(&created) <= orphan_days {
            continue;
        }
        let detail = format!("{} ({id}): no canon or code dependents, 0 reads_90d, created {} days ago", title.unwrap_or_default(), age_days(&created));
        let fix = ProposedOp {
            op: Operation::Retire,
            target_id: Some(id.clone()),
            justification: "nightly: orphan (no dependents, no reads, past orphan_days)".to_string(),
            confidence: 1.0,
            ..Default::default()
        };
        out.push(Finding { kind: FindingKind::Orphan, doc_ids: vec![id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// drift: refs.drift_state not ok. `target_superseded` chains get a `link` fix to the final,
// non-superseded target; every other drift state (missing, tombstoned, heading_missing,
// quarantined, hash_changed) is escalated -- resolving those needs the content-hash relocation
// pass design.md §17.1 defers to a later step.
// -------------------------------------------------------------------------------------------

/// follow `superseded_by` from `start` while the target is itself superseded or archived,
/// stopping at the first non-superseded target, a missing link, or a cycle.
fn walk_supersede_chain(conn: &Connection, start: &str) -> Result<String> {
    let mut current = start.to_string();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.clone()) {
            break;
        }
        let row: Option<(Option<String>, Option<String>)> =
            conn.query_row("select status, superseded_by from documents where id = ?1", [&current], |r| Ok((r.get(0)?, r.get(1)?))).ok();
        match row {
            Some((status, Some(sb))) if matches!(status.as_deref(), Some("superseded") | Some("archived")) && sb != "none" => {
                match crate::model::parse_kb_uri(&sb) {
                    Some((next, _)) => current = next,
                    None => break,
                }
            }
            _ => break,
        }
    }
    Ok(current)
}

fn detect_drift(conn: &Connection, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("select from_id, rel, uri, resolved_id, drift_state, drift_detail from refs where drift_state is not null and drift_state != 'ok'")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    for row in rows {
        let (from_id, rel, uri, resolved_id, drift_state, drift_detail) = row?;
        if drift_state == "target_superseded" {
            let Some(resolved_id) = &resolved_id else { continue };
            let final_id = walk_supersede_chain(conn, resolved_id)?;
            let detail = format!("{from_id}: {rel} ref {uri} points at a superseded document; final target kb:{final_id}");
            let fix = ProposedOp {
                op: Operation::Link,
                target_id: Some(from_id.clone()),
                field: Some(rel.clone()),
                content: Some(uri.clone()),
                new_id: Some(final_id),
                refs: vec![Ref { rel: rel.clone(), uri: uri.clone(), sha256: None, media_type: None }],
                justification: "nightly: drift (relink past a superseded target)".to_string(),
                confidence: 1.0,
                ..Default::default()
            };
            out.push(Finding { kind: FindingKind::Drift, doc_ids: vec![from_id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
        } else {
            let detail = format!("{from_id}: {rel} ref {uri} drift_state={drift_state}{}", drift_detail.map(|d| format!(" ({d})")).unwrap_or_default());
            out.push(Finding { kind: FindingKind::Drift, doc_ids: vec![from_id.clone()], detail, band: None, auto_fix: None });
        }
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// over budget: word count above the layer budget. design.md §15.3's table has no auto-fix in
// either column for this row -- always escalate, with a split outline the human writes.
// -------------------------------------------------------------------------------------------

fn detect_over_budget(conn: &Connection, cfg: &Config) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("select id, title, layer, word_count from documents where kind = 'document' and status in ('canon', 'draft')")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?)))?;
    for row in rows {
        let (id, title, layer, words) = row?;
        let Some(layer) = layer.as_deref().and_then(|l| Layer::from_str(l).ok()) else { continue };
        let budget = cfg.budget_for(layer);
        if (words as usize) <= budget {
            continue;
        }
        let detail = format!("{} ({id}): {words} words exceeds the {layer} budget of {budget}", title.unwrap_or_default());
        out.push(Finding { kind: FindingKind::OverBudget, doc_ids: vec![id], detail, band: None, auto_fix: None });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// missing provenance: agent-created without a source ref (escalate; no fix invents a source),
// or superseded/archived without a banner or a pointer (auto-fix: add from template).
// -------------------------------------------------------------------------------------------

fn detect_missing_provenance(conn: &Connection, repo: &Path, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();

    // (a) agent-created, no source ref: detected by a `Run:` trailer on the commit that added
    // the file, per design.md §15.3.
    let mut stmt = conn.prepare("select id, title, path, frontmatter from documents where kind = 'document'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    for row in rows {
        let (id, title, path, fm_json) = row?;
        if has_source_ref(&fm_json) {
            continue;
        }
        let Ok(Some((_sha, author, message))) = git::creating_commit(repo, &path) else { continue };
        if !author.starts_with("agent:") || !message.lines().any(|l| l.trim_start().starts_with("Run:")) {
            continue;
        }
        let detail = format!("{} ({id}): agent-created ({path}) with no source ref", title.unwrap_or_default());
        out.push(Finding { kind: FindingKind::MissingProvenance, doc_ids: vec![id], detail, band: None, auto_fix: None });
    }

    // (b) superseded/archived missing a banner or a forward pointer. a *dangling* pointer (present
    // but naming a document that does not exist) is not something a template can safely fix --
    // rewriting someone's stated successor is not this detector's call -- so that always
    // escalates, distinctly from a genuinely absent pointer or banner.
    let mut stmt2 = conn.prepare("select id, title, body, superseded_by from documents where kind = 'document' and status in ('superseded', 'archived')")?;
    let rows2 = stmt2.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?;
    for row in rows2 {
        let (id, title, body, superseded_by) = row?;
        let missing_banner = !crate::check::has_banner(&body);
        let pointer = superseded_by.as_deref().filter(|s| !s.is_empty());
        let dangling_target = pointer.and_then(model::parse_kb_uri).and_then(|(target_id, _)| {
            let exists: i64 = conn.query_row("select count(*) from documents where id = ?1", [&target_id], |r| r.get(0)).unwrap_or(0);
            (exists == 0).then_some(target_id)
        });
        let missing_pointer = pointer.is_none();

        if let Some(target_id) = dangling_target {
            let detail = format!("{} ({id}): superseded_by points at {target_id}, which does not exist", title.unwrap_or_default());
            out.push(Finding { kind: FindingKind::MissingProvenance, doc_ids: vec![id], detail, band: None, auto_fix: None });
            continue;
        }
        if !missing_banner && !missing_pointer {
            continue;
        }
        let detail = format!(
            "{} ({id}): {}",
            title.unwrap_or_default(),
            match (missing_banner, missing_pointer) {
                (true, true) => "missing both the forward-pointer banner and the superseded_by pointer",
                (true, false) => "missing the forward-pointer banner",
                (false, true) => "missing the superseded_by pointer",
                (false, false) => unreachable!(),
            }
        );
        let fix = ProposedOp {
            op: Operation::Link,
            target_id: Some(id.clone()),
            justification: "nightly: missing provenance (add banner/pointer from template)".to_string(),
            confidence: 1.0,
            ..Default::default()
        };
        out.push(Finding { kind: FindingKind::MissingProvenance, doc_ids: vec![id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// broken graph: cycle rows already computed by `index::build` (design.md §17.1's kahn/tarjan
// pass). always escalate: breaking a cycle is an editorial decision.
// -------------------------------------------------------------------------------------------

fn detect_broken_graph(conn: &Connection) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("select doc_ids, detail from findings where kind = 'cycle'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
    for row in rows {
        let (doc_ids_json, detail) = row?;
        let doc_ids: Vec<String> = serde_json::from_str(&doc_ids_json).unwrap_or_default();
        out.push(Finding { kind: FindingKind::BrokenGraph, doc_ids, detail: detail.unwrap_or_else(|| "cycle".to_string()), band: None, auto_fix: None });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// long chain: `superseded_by` target itself superseded. always gets the `link to shorten` fix
// (design.md §15.3's table repeats the same fix in both columns for this row).
// -------------------------------------------------------------------------------------------

fn detect_long_chain(conn: &Connection, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt =
        conn.prepare("select id, title, superseded_by from documents where kind = 'document' and status in ('superseded', 'archived')")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
    for row in rows {
        let (id, title, superseded_by) = row?;
        let Some(sb) = superseded_by.filter(|s| s != "none") else { continue };
        let Some((target_id, _)) = crate::model::parse_kb_uri(&sb) else { continue };
        let target_status: Option<String> = conn.query_row("select status from documents where id = ?1", [&target_id], |r| r.get(0)).ok();
        if !matches!(target_status.as_deref(), Some("superseded") | Some("archived")) {
            continue;
        }
        let final_id = walk_supersede_chain(conn, &target_id);
        let final_id = final_id?;
        if final_id == target_id {
            // one hop to a status this build could not resolve further; still worth shortening.
        }
        let detail = format!("{} ({id}): superseded_by chains through {target_id} to kb:{final_id}", title.unwrap_or_default());
        let fix = ProposedOp {
            op: Operation::Link,
            target_id: Some(id.clone()),
            new_id: Some(final_id),
            justification: "nightly: long chain (point superseded_by at the final target)".to_string(),
            confidence: 1.0,
            ..Default::default()
        };
        out.push(Finding { kind: FindingKind::LongChain, doc_ids: vec![id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// superseded past delete days / draft past ttl: design.md §15.3's two "review class always"
// delete rows. `nightly` (mod.rs) never lands these through the normal auto-merge path; see its
// module comment.
// -------------------------------------------------------------------------------------------

fn detect_superseded_past_delete_days(conn: &Connection, cfg: &Config, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("select id, title, last_updated, created from documents where kind = 'document' and status = 'superseded'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?)))?;
    let limit = cfg.manifest.thresholds.superseded_delete_days as i64;
    for row in rows {
        let (id, title, last_updated, created) = row?;
        let Some(since) = last_updated.as_deref().or(created.as_deref()).and_then(parse_rfc3339) else { continue };
        let age = age_days(&since);
        if age <= limit {
            continue;
        }
        let detail = format!("{} ({id}): superseded {age} days ago (past superseded_delete_days = {limit})", title.unwrap_or_default());
        let fix = ProposedOp {
            op: Operation::Delete,
            target_id: Some(id.clone()),
            justification: "nightly: superseded past superseded_delete_days".to_string(),
            confidence: 1.0,
            ..Default::default()
        };
        out.push(Finding { kind: FindingKind::SupersededPastDeleteDays, doc_ids: vec![id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
    }
    Ok(out)
}

fn detect_draft_past_ttl(conn: &Connection, cfg: &Config, enabled: bool) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("select id, title, created, last_updated from documents where kind = 'document' and status = 'draft'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?)))?;
    let ttl = cfg.manifest.thresholds.draft_ttl_days as i64;
    for row in rows {
        let (id, title, created, last_updated) = row?;
        let Some(created_dt) = created.as_deref().and_then(parse_rfc3339) else { continue };
        let age = age_days(&created_dt);
        if age <= ttl {
            continue;
        }
        // "no commits since": the newest commit touching the document is (within a few minutes
        // of) its creating commit, i.e. the only commit is the one that created it. `created` is
        // the ulid's own embedded timestamp, not necessarily bit-identical to the creating
        // commit's committer date, hence the tolerance rather than exact equality.
        let no_commits_since = match last_updated.as_deref().and_then(parse_rfc3339) {
            None => true,
            Some(lu) => (lu - created_dt).num_minutes().abs() <= 5,
        };
        if !no_commits_since {
            continue;
        }
        let detail = format!("{} ({id}): draft {age} days old (past draft_ttl_days = {ttl}), no commits since creation", title.unwrap_or_default());
        let fix = ProposedOp {
            op: Operation::Delete,
            target_id: Some(id.clone()),
            justification: "nightly: draft past draft_ttl_days with no commits since".to_string(),
            confidence: 1.0,
            ..Default::default()
        };
        out.push(Finding { kind: FindingKind::DraftPastTtl, doc_ids: vec![id], detail, band: None, auto_fix: gate_fix(enabled, fix) });
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// duplication: chunk pairs above near_duplicate cosine when `chunk_vectors` exists (vec0 is a
// later build step; see this crate's report); the documented fallback is jaccard over 5-word
// shingles above 0.8, across or within documents. always escalate proposing `merge`.
// -------------------------------------------------------------------------------------------

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row("select count(*) from sqlite_master where type = 'table' and name = ?1", [name], |r| r.get::<_, i64>(0)).unwrap_or(0) > 0
}

fn shingles(text: &str, n: usize) -> HashSet<u64> {
    use std::hash::{Hash, Hasher};
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < n {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        words.join(" ").hash(&mut h);
        return [h.finish()].into_iter().collect();
    }
    let mut out = HashSet::new();
    for w in words.windows(n) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        w.join(" ").hash(&mut h);
        out.insert(h.finish());
    }
    out
}

fn jaccard(a: &HashSet<u64>, b: &HashSet<u64>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

/// a hard cap on chunk-pair comparisons so a large corpus (no vector index yet) cannot make
/// nightly curation itself pathological; documented in this crate's report.
const DUPLICATION_MAX_CHUNKS: usize = 4_000;

fn detect_duplication(conn: &Connection, cfg: &Config) -> Result<Vec<Finding>> {
    // the cosine path over `chunk_vectors` belongs to the vector index (a later build step, not
    // present in this crate); `table_exists` lets a future cutover branch here instead of
    // rewriting this function's caller. until then every corpus uses the text fallback below.
    let _ = table_exists(conn, "chunk_vectors");
    let mut out = Vec::new();
    let mut stmt = conn.prepare("select chunk_id, doc_id, text from chunks where kind = 'document' and status in ('canon', 'draft')")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut chunks: Vec<(i64, String, HashSet<u64>)> = Vec::new();
    for row in rows {
        let (chunk_id, doc_id, text) = row?;
        chunks.push((chunk_id, doc_id, shingles(&text, 5)));
    }
    if chunks.len() > DUPLICATION_MAX_CHUNKS {
        return Ok(out);
    }
    // the design's fallback threshold (0.8) is fixed, independent of the configured cosine
    // near_duplicate threshold (0.92); use whichever is lower, so a stricter local override
    // still tightens the fallback.
    let effective = cfg.manifest.thresholds.near_duplicate.min(0.8);
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    for i in 0..chunks.len() {
        for j in (i + 1)..chunks.len() {
            let (id_a, doc_a, sh_a) = &chunks[i];
            let (id_b, doc_b, sh_b) = &chunks[j];
            if id_a == id_b {
                continue;
            }
            let score = jaccard(sh_a, sh_b);
            if score < effective {
                continue;
            }
            let (da, db) = if doc_a <= doc_b { (doc_a.clone(), doc_b.clone()) } else { (doc_b.clone(), doc_a.clone()) };
            if !seen_pairs.insert((da.clone(), db.clone())) {
                continue;
            }
            let detail = format!("{da} and {db}: near-duplicate chunks (jaccard {score:.2} over 5-word shingles)");
            out.push(Finding { kind: FindingKind::Duplication, doc_ids: vec![da, db], detail, band: None, auto_fix: None });
        }
    }
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// contradiction / bad merged change: only when a `review` provider is configured (design.md
// §15.3, §19). both re-run the `review` hook (content.rs) over already-merged commits with
// fresh one-hop neighbors; no separate `curate-nightly` provider role exists in kb.toml.
// -------------------------------------------------------------------------------------------

fn review_input_for_commit(repo: &Path, sha: &str) -> review::ReviewInput {
    let parent = format!("{sha}^");
    let message = git::commit_message(repo, sha).unwrap_or_default();
    let actor = git::commit_author(repo, sha).unwrap_or_else(|_| "unknown".to_string());
    review::ReviewInput {
        session_id: None,
        base: parent,
        head: sha.to_string(),
        main: None,
        justification: message,
        actor,
        context_excerpts: vec![],
        session_facts: None,
    }
}

fn doc_ids_touched(repo: &Path, base: &str, head: &str) -> Vec<String> {
    let Ok(changed) = git::diff_name_status(repo, base, head) else { return vec![] };
    let mut ids = Vec::new();
    for (status, old, new) in changed {
        let path = if status == 'D' { old } else { new };
        if let Some(stem) = Path::new(&path).file_stem().and_then(|s| s.to_str())
            && crate::model::is_ulid(stem)
        {
            ids.push(stem.to_string());
        }
    }
    ids
}

/// the content review of one merged commit, from `cache` when this pass already asked.
fn review_commit(cfg: &Config, repo: &Path, sha: &str, cache: &mut HashMap<String, Option<review::ReviewOutput>>) -> Option<review::ReviewOutput> {
    cache
        .entry(sha.to_string())
        .or_insert_with(|| {
            let input = review_input_for_commit(repo, sha);
            review::review(cfg, &input).ok().flatten()
        })
        .clone()
}

fn detect_contradiction(cfg: &Config, repo: &Path, since_commit: &str, cache: &mut HashMap<String, Option<review::ReviewOutput>>) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    if provider::provider_for_role(cfg, "review")?.is_none() {
        return Ok(out);
    }
    let commits = git::log_shas_since(repo, since_commit, "main").unwrap_or_default();
    for sha in commits {
        let Some(result) = review_commit(cfg, repo, &sha, cache) else { continue };
        for f in &result.findings {
            if f.kind != review::FindingKind::Contradiction {
                continue;
            }
            let detail = format!("commit {sha}{}: {}", f.section.as_deref().map(|s| format!(" #{s}")).unwrap_or_default(), f.reason);
            out.push(Finding { kind: FindingKind::Contradiction, doc_ids: vec![f.doc_id.clone()], detail, band: None, auto_fix: None });
        }
    }
    Ok(out)
}

fn detect_bad_merged_change(cfg: &Config, repo: &Path, since_commit: &str, enabled: bool, cache: &mut HashMap<String, Option<review::ReviewOutput>>) -> Result<Vec<Finding>> {
    let mut out = Vec::new();
    if provider::provider_for_role(cfg, "review")?.is_none() {
        return Ok(out);
    }
    let commits = git::log_shas_since(repo, since_commit, "main").unwrap_or_default();
    for sha in commits {
        let message = git::commit_message(repo, &sha).unwrap_or_default();
        if !message.contains("Session:") {
            // only re-read commits a session settled and merge landed; a hand-written or nightly
            // commit (including this detector's own revert-fix commits) is not in scope.
            continue;
        }
        let Some(result) = review_commit(cfg, repo, &sha, cache) else { continue };
        if result.verdict == review::Verdict::Approve {
            continue;
        }
        let doc_ids = doc_ids_touched(repo, &format!("{sha}^"), &sha);
        let detail = format!("commit {sha}: re-review {:?} (confidence {:.2}): {}", result.verdict, result.confidence, result.summary);
        // the commit sha to revert is carried in `content` -- `BadMergedChange` findings are
        // never handed to `curate::ops::apply` (a revert is not one of its operations), so
        // `nightly` (mod.rs) reads it back out directly; see this crate's report.
        let auto_fix = if enabled && result.confidence >= cfg.manifest.thresholds.nightly_revert_confidence {
            Some(ProposedOp {
                op: Operation::Escalate,
                target_id: doc_ids.first().cloned(),
                content: Some(sha.clone()),
                justification: format!("nightly: revert {sha} (bad merged change, confidence {:.2})", result.confidence),
                confidence: result.confidence,
                ..Default::default()
            })
        } else {
            None
        };
        out.push(Finding { kind: FindingKind::BadMergedChange, doc_ids, detail, band: None, auto_fix });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::create_schema;

    fn conn_with_schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn.execute("insert into meta (key, value) values ('maintenance_enabled', '1')", []).unwrap();
        conn
    }

    fn insert_doc(conn: &Connection, id: &str, status: &str, layer: &str, extra_fm: &str, word_count: i64, created: &str) {
        conn.execute(
            "insert into documents (id, kind, path, layer, title, status, owner, created, frontmatter, body, word_count)
             values (?1,'document',?2,?3,?4,?5,'caleb',?6,?7,'',?8)",
            rusqlite::params![id, format!("docs/{id}.md"), layer, format!("title {id}"), status, created, extra_fm, word_count],
        )
        .unwrap();
        conn.execute("insert into graph_metrics (doc_id, in_canon, in_code, reads_90d, cites_90d) values (?1,0,0,0,0)", [id]).unwrap();
    }

    #[test]
    fn stale_fires_on_past_review_by_with_no_usage() {
        let conn = conn_with_schema();
        let old = (crate::clock::now() - chrono::Duration::days(400)).to_rfc3339();
        insert_doc(&conn, "01K5A7Q2XW3ZRCM8T6YB4VJH0A", "canon", "reference", r#"{"review_by":{"$__toml_private_datetime":"2020-01-01"}}"#, 10, &old);
        let out = detect_stale(&conn, true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, FindingKind::Stale);
        assert!(out[0].auto_fix.is_some());
        assert_eq!(out[0].auto_fix.as_ref().unwrap().op, Operation::Retire);
    }

    #[test]
    fn stale_auto_fix_is_none_below_min_corpus() {
        let conn = conn_with_schema();
        let old = (crate::clock::now() - chrono::Duration::days(400)).to_rfc3339();
        insert_doc(&conn, "01K5A7Q2XW3ZRCM8T6YB4VJH0A", "canon", "reference", r#"{"review_by":{"$__toml_private_datetime":"2020-01-01"}}"#, 10, &old);
        let out = detect_stale(&conn, false).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].auto_fix.is_none());
    }

    #[test]
    fn stale_skips_documents_with_usage() {
        let conn = conn_with_schema();
        let old = (crate::clock::now() - chrono::Duration::days(400)).to_rfc3339();
        insert_doc(&conn, "01K5A7Q2XW3ZRCM8T6YB4VJH0A", "canon", "reference", r#"{"review_by":{"$__toml_private_datetime":"2020-01-01"}}"#, 10, &old);
        conn.execute("update graph_metrics set reads_90d = 3 where doc_id = '01K5A7Q2XW3ZRCM8T6YB4VJH0A'", []).unwrap();
        assert!(detect_stale(&conn, true).unwrap().is_empty());
    }

    #[test]
    fn orphan_fires_when_no_dependents_and_old() {
        let conn = conn_with_schema();
        let old = (crate::clock::now() - chrono::Duration::days(200)).to_rfc3339();
        insert_doc(&conn, "01K5A7Q2XW3ZRCM8T6YB4VJH0B", "canon", "reference", "{}", 10, &old);
        let cfg = Config { root: std::path::PathBuf::new(), kb: Default::default(), manifest: Default::default(), groups: Default::default() };
        let out = detect_orphan(&conn, &cfg, true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, FindingKind::Orphan);
    }

    #[test]
    fn over_budget_always_escalates() {
        let conn = conn_with_schema();
        let now = crate::clock::now().to_rfc3339();
        insert_doc(&conn, "01K5A7Q2XW3ZRCM8T6YB4VJH0C", "canon", "reference", "{}", 100_000, &now);
        let cfg = Config { root: std::path::PathBuf::new(), kb: Default::default(), manifest: Default::default(), groups: Default::default() };
        let out = detect_over_budget(&conn, &cfg).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].auto_fix.is_none());
    }

    #[test]
    fn broken_graph_reads_findings_table() {
        let conn = conn_with_schema();
        conn.execute("insert into findings (kind, doc_ids, detail) values ('cycle', '[\"a\",\"b\"]', 'hierarchy graph')", []).unwrap();
        let out = detect_broken_graph(&conn).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].doc_ids, vec!["a".to_string(), "b".to_string()]);
        assert!(out[0].auto_fix.is_none());
    }

    #[test]
    fn drift_target_superseded_gets_a_link_fix_others_escalate() {
        let conn = conn_with_schema();
        conn.execute(
            "insert into refs (from_id, rel, uri, resolved_id, origin, drift_state) values ('a','related','kb:b','b','frontmatter','target_superseded')",
            [],
        )
        .unwrap();
        conn.execute("insert into documents (id, kind, path, status) values ('b','document','docs/b.md','superseded')", []).unwrap();
        conn.execute(
            "insert into refs (from_id, rel, uri, resolved_id, origin, drift_state) values ('c','related','kb:missing',null,'frontmatter','missing')",
            [],
        )
        .unwrap();
        let out = detect_drift(&conn, true).unwrap();
        assert_eq!(out.len(), 2);
        let ts = out.iter().find(|f| f.doc_ids == vec!["a".to_string()]).unwrap();
        assert!(ts.auto_fix.is_some());
        let missing = out.iter().find(|f| f.doc_ids == vec!["c".to_string()]).unwrap();
        assert!(missing.auto_fix.is_none());
    }

    #[test]
    fn long_chain_shortens_to_the_final_target() {
        // `superseded_by` values must be real `kb:<ulid>` uris -- `parse_kb_uri` rejects anything
        // shorter, so this needs full 26-char ids, unlike the other fixture tests here that never
        // walk a second hop.
        const X: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0X";
        const Y: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0Y";
        const Z: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0Z";
        let conn = conn_with_schema();
        conn.execute(
            &format!("insert into documents (id, kind, path, status, superseded_by) values ('{X}','document','docs/x.md','superseded','kb:{Y}')"),
            [],
        )
        .unwrap();
        conn.execute(
            &format!("insert into documents (id, kind, path, status, superseded_by) values ('{Y}','document','docs/y.md','superseded','kb:{Z}')"),
            [],
        )
        .unwrap();
        conn.execute(&format!("insert into documents (id, kind, path, status) values ('{Z}','document','docs/z.md','canon')"), []).unwrap();
        let out = detect_long_chain(&conn, true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].doc_ids, vec![X.to_string()]);
        assert_eq!(out[0].auto_fix.as_ref().unwrap().new_id.as_deref(), Some(Z));
    }

    #[test]
    fn superseded_past_delete_days_fires() {
        let conn = conn_with_schema();
        let old = (crate::clock::now() - chrono::Duration::days(120)).to_rfc3339();
        conn.execute(
            "insert into documents (id, kind, path, status, last_updated, created) values ('s1','document','docs/s1.md','superseded',?1,?1)",
            [&old],
        )
        .unwrap();
        let cfg = Config { root: std::path::PathBuf::new(), kb: Default::default(), manifest: Default::default(), groups: Default::default() };
        let out = detect_superseded_past_delete_days(&conn, &cfg, true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].auto_fix.as_ref().unwrap().op, Operation::Delete);
    }

    #[test]
    fn draft_past_ttl_requires_no_commits_since_creation() {
        let conn = conn_with_schema();
        let old = (crate::clock::now() - chrono::Duration::days(60)).to_rfc3339();
        let recent = crate::clock::now().to_rfc3339();
        // d1: only its creating commit, 60 days ago -- no commits since creation.
        conn.execute(
            "insert into documents (id, kind, path, status, created, last_updated) values ('d1','document','docs/d1.md','draft',?1,?1)",
            [&old],
        )
        .unwrap();
        // d2: created 60 days ago but touched again recently -- has commits since creation.
        conn.execute(
            "insert into documents (id, kind, path, status, created, last_updated) values ('d2','document','docs/d2.md','draft',?1,?2)",
            [&old, &recent],
        )
        .unwrap();
        let cfg = Config { root: std::path::PathBuf::new(), kb: Default::default(), manifest: Default::default(), groups: Default::default() };
        let out = detect_draft_past_ttl(&conn, &cfg, true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].doc_ids, vec!["d1".to_string()]);
    }

    #[test]
    fn duplication_fallback_finds_near_identical_chunks() {
        let conn = conn_with_schema();
        let text = "the quick brown fox jumps over the lazy dog again and again today";
        conn.execute("insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h1','a','document','canon','_body',0,?1)", [text]).unwrap();
        conn.execute("insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h2','b','document','canon','_body',0,?1)", [text]).unwrap();
        let cfg = Config { root: std::path::PathBuf::new(), kb: Default::default(), manifest: Default::default(), groups: Default::default() };
        let out = detect_duplication(&conn, &cfg).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].auto_fix.is_none());
    }

    #[test]
    fn finding_kind_from_str_round_trips() {
        for k in [
            FindingKind::Contradiction,
            FindingKind::Duplication,
            FindingKind::Stale,
            FindingKind::Drift,
            FindingKind::Orphan,
            FindingKind::OverBudget,
            FindingKind::MissingProvenance,
            FindingKind::BrokenGraph,
            FindingKind::LongChain,
            FindingKind::BadMergedChange,
            FindingKind::SupersededPastDeleteDays,
            FindingKind::DraftPastTtl,
        ] {
            assert_eq!(k.as_str().parse::<FindingKind>().unwrap(), k);
        }
    }
}
