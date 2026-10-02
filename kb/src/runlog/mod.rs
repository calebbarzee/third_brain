//! the run log: one sqlite file per agent run, hash-chained and append-only. design.md §16.
//!
//! a run log lives at `<state>/runs/<run-ulid>.sqlite` while open, and is copied read-only to
//! `<state>/store/runs/<run-ulid>.sqlite` (the local content store) when `seal` is called. every
//! event is written in its own `BEGIN IMMEDIATE` transaction, chained by
//! `hash = sha256(prev_hash || kind || ts || canonical_payload_json)`; `run_end` copies the last
//! link into `run.final_hash`. `verify` (in `runlog::verify`) recomputes the chain from scratch.
pub mod schema;
pub mod verify;

use crate::config::Config;
use crate::model::{MergeClass, Operation};
use crate::parse::frontmatter::sha256_hex;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// every event kind the schema accepts, in the order design.md §16 lists them (the design-0003
/// kinds, then the ones added for reviewed, gated, settled runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    RunStart,
    Context,
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
    Read,
    PolicyDecision,
    Decision,
    Op,
    Checkpoint,
    Error,
    Report,
    RunEnd,
    Gate,
    ReviewVerdict,
    Settle,
    Conflict,
    Check,
    Classify,
    MergeStart,
    MergeEnd,
    DeferredRequest,
    DeferredResult,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::RunStart => "run_start",
            EventKind::Context => "context",
            EventKind::LlmRequest => "llm_request",
            EventKind::LlmResponse => "llm_response",
            EventKind::ToolCall => "tool_call",
            EventKind::ToolResult => "tool_result",
            EventKind::Read => "read",
            EventKind::PolicyDecision => "policy_decision",
            EventKind::Decision => "decision",
            EventKind::Op => "op",
            EventKind::Checkpoint => "checkpoint",
            EventKind::Error => "error",
            EventKind::Report => "report",
            EventKind::RunEnd => "run_end",
            EventKind::Gate => "gate",
            EventKind::ReviewVerdict => "review_verdict",
            EventKind::Settle => "settle",
            EventKind::Conflict => "conflict",
            EventKind::Check => "check",
            EventKind::Classify => "classify",
            EventKind::MergeStart => "merge_start",
            EventKind::MergeEnd => "merge_end",
            EventKind::DeferredRequest => "deferred_request",
            EventKind::DeferredResult => "deferred_result",
        }
    }
}

/// terminal run status, written by `run_end`. `running` is the row's default at `create` time and
/// is never produced by this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Ok,
    Failed,
    Escalated,
    Deferred,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Failed => "failed",
            RunStatus::Escalated => "escalated",
            RunStatus::Deferred => "deferred",
        }
    }
}

/// a review's verdict (design.md §16's `review` table and the content review hook's output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Escalate,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request_changes",
            Verdict::Escalate => "escalate",
        }
    }
}

/// arguments to `RunLog::create`; one field per `run` column filled in at start time.
#[derive(Debug, Clone, Default)]
pub struct RunStart {
    pub actor: String,
    pub principal: Option<String>,
    pub skill: Option<String>,
    pub skill_version: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub worktree_path: Option<String>,
    pub base_commit: Option<String>,
    pub prompt_checksums: BTreeMap<String, String>,
    pub parser_version: Option<u32>,
    pub deferred: bool,
}

/// one open or sealed run log.
pub struct RunLog {
    conn: Connection,
    run_id: String,
    path: PathBuf,
    /// hash of the most recently written event; the chain's genesis value before any event exists.
    prev_hash: String,
    /// attributes model calls on this thread to this run until the log is sealed or dropped;
    /// `None` for a log opened read-only.
    _transcript_run: Option<crate::transcript::RunGuard>,
}

fn runs_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir().join("runs")
}

fn store_runs_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir().join("store").join("runs")
}

/// the hash chain's starting value, derived from the run id so it never collides across runs and
/// `verify` can recompute it without any extra bookkeeping.
fn genesis_hash(run_id: &str) -> String {
    sha256_hex(format!("genesis:{run_id}").as_bytes())
}

fn compute_hash(prev_hash: &str, kind: &str, ts: &str, payload_json: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(kind.as_bytes());
    h.update(ts.as_bytes());
    h.update(payload_json.as_bytes());
    hex::encode(h.finalize())
}

/// find a `<ulid>.sqlite` file whose stem starts with `id_or_prefix`, first under `<state>/runs/`
/// (open logs) then under `<state>/store/runs/` (sealed logs). the first (lexically smallest)
/// match wins when a prefix is ambiguous, matching `session::load`'s "give a longer prefix" spirit
/// is left to callers that want to detect ambiguity explicitly via `find_matches`.
pub fn resolve_run_path(cfg: &Config, id_or_prefix: &str) -> Result<PathBuf> {
    for dir in [runs_dir(cfg), store_runs_dir(cfg)] {
        let matches = find_matches(&dir, id_or_prefix);
        match matches.len() {
            0 => continue,
            1 => return Ok(matches.into_iter().next().unwrap()),
            _ => bail!("`{id_or_prefix}` matches more than one run log in {}; give a longer prefix", dir.display()),
        }
    }
    bail!("no run log found matching `{id_or_prefix}`")
}

fn find_matches(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("sqlite"))
        .filter(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with(prefix)))
        .collect();
    matches.sort();
    matches
}

impl RunLog {
    /// start a new run: create `<state>/runs/<run-ulid>.sqlite`, apply the schema, insert the
    /// `run` row, and log `run_start`.
    pub fn create(cfg: &Config, start: RunStart) -> Result<RunLog> {
        let run_id = crate::model::new_ulid();
        let dir = runs_dir(cfg);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{run_id}.sqlite"));
        let conn = Connection::open(&path).with_context(|| format!("creating {}", path.display()))?;
        conn.execute_batch("pragma journal_mode = WAL; pragma synchronous = FULL;")?;
        schema::create_schema(&conn)?;

        let started_at = crate::clock::now().to_rfc3339();
        let prompt_checksums_json = serde_json::to_string(&start.prompt_checksums)?;
        conn.execute(
            "insert into run (run_id, actor, principal, skill, skill_version, provider, model,
                prompt_checksums, parser_version, base_commit, started_at, session_id,
                worktree_path, deferred)
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                run_id,
                start.actor,
                start.principal,
                start.skill,
                start.skill_version,
                start.provider,
                start.model,
                prompt_checksums_json,
                start.parser_version,
                start.base_commit,
                started_at,
                start.session_id,
                start.worktree_path,
                start.deferred as i64,
            ],
        )
        .context("inserting run row")?;

        let transcript_run = crate::transcript::enter_run(&run_id, &start.actor, start.session_id.as_deref());
        let mut log = RunLog { conn, run_id: run_id.clone(), path, prev_hash: genesis_hash(&run_id), _transcript_run: Some(transcript_run) };
        log.log(
            EventKind::RunStart,
            serde_json::json!({
                "actor": log_actor(&log.run_id, &start.actor),
                "principal": start.principal,
                "skill": start.skill,
                "skill_version": start.skill_version,
                "provider": start.provider,
                "model": start.model,
                "session_id": start.session_id,
                "worktree_path": start.worktree_path,
                "base_commit": start.base_commit,
                "prompt_checksums": start.prompt_checksums,
                "parser_version": start.parser_version,
                "deferred": start.deferred,
            }),
        )?;
        Ok(log)
    }

    /// open a run log read-only by exact id or unambiguous prefix, looking under `<state>/runs/`
    /// then `<state>/store/runs/`.
    pub fn open(cfg: &Config, id_or_prefix: &str) -> Result<RunLog> {
        let path = resolve_run_path(cfg, id_or_prefix)?;
        let conn =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))?;
        let run_id: String = conn.query_row("select run_id from run limit 1", [], |r| r.get(0)).context("reading run row")?;
        let prev_hash: String = conn
            .query_row("select hash from event where run_id = ?1 order by seq desc limit 1", params![run_id], |r| r.get(0))
            .unwrap_or_else(|_| genesis_hash(&run_id));
        Ok(RunLog { conn, run_id, path, prev_hash, _transcript_run: None })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// append one event: `ts` is the current time in rfc3339, `hash` chains `prev_hash`, `kind`,
    /// `ts`, and the canonical (sorted-key) json of `payload`. one `BEGIN IMMEDIATE` transaction
    /// per event; returns the assigned `seq`.
    pub fn log(&mut self, kind: EventKind, payload: Value) -> Result<i64> {
        let ts = crate::clock::now().to_rfc3339();
        // serde_json's `Map` is a `BTreeMap` by default (the `preserve_order` feature is off in
        // this crate), so `to_string` already emits object keys in sorted order: this is the
        // "canonical payload json" the hash is computed over.
        let payload_json = serde_json::to_string(&payload).context("serializing event payload")?;
        let hash = compute_hash(&self.prev_hash, kind.as_str(), &ts, &payload_json);
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "insert into event (run_id, ts, kind, payload, prev_hash, hash) values (?1,?2,?3,?4,?5,?6)",
            params![self.run_id, ts, kind.as_str(), payload_json, self.prev_hash, hash],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;
        self.prev_hash = hash;
        Ok(seq)
    }

    /// store `bytes` in the content-addressed `blob` table, returning its sha256 hex digest.
    /// a no-op when the blob is already present. stored uncompressed: flate2 (or another
    /// compression crate) is not yet a dependency of this crate, so op diffs go in raw for now.
    fn store_blob(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let exists: Option<i64> = self.conn.query_row("select 1 from blob where sha256 = ?1", params![sha], |r| r.get(0)).optional()?;
        if exists.is_none() {
            self.conn.execute("insert into blob (sha256, size, content) values (?1,?2,?3)", params![sha, bytes.len() as i64, bytes])?;
        }
        Ok(sha)
    }

    pub fn context(&mut self, packet: Value) -> Result<i64> {
        self.log(EventKind::Context, packet)
    }

    pub fn llm_request(&mut self, delta_json: Value, full_request_hash: &str) -> Result<i64> {
        self.log(EventKind::LlmRequest, serde_json::json!({"delta": delta_json, "full_request_hash": full_request_hash}))
    }

    pub fn llm_response(&mut self, blocks_json: Value, usage: Value) -> Result<i64> {
        self.log(EventKind::LlmResponse, serde_json::json!({"blocks": blocks_json, "usage": usage}))
    }

    pub fn tool_call(&mut self, name: &str, input: Value) -> Result<i64> {
        self.log(EventKind::ToolCall, serde_json::json!({"name": name, "input": input}))
    }

    pub fn tool_result(&mut self, id: &str, content: Value, is_error: bool) -> Result<i64> {
        self.log(EventKind::ToolResult, serde_json::json!({"id": id, "content": content, "is_error": is_error}))
    }

    pub fn read(&mut self, doc_id: &str, hash: &str) -> Result<i64> {
        self.log(EventKind::Read, serde_json::json!({"doc_id": doc_id, "hash": hash}))
    }

    pub fn policy_decision(&mut self, doc_id: &str, op: Operation, allow: bool, class: MergeClass, reasons: &[String]) -> Result<i64> {
        self.log(EventKind::PolicyDecision, serde_json::json!({"doc_id": doc_id, "op": op, "allow": allow, "class": class, "reasons": reasons}))
    }

    pub fn decision(&mut self, text: &str) -> Result<i64> {
        self.log(EventKind::Decision, serde_json::json!({"text": text}))
    }

    /// log an `op` event and insert the matching `op` row (kept in sync: the row's `event_seq`
    /// is the event's own `seq`). `diff`, when given, is content-addressed into `blob` first so
    /// the `op` row's `diff_blob` foreign key always resolves.
    pub fn op(&mut self, kind: Operation, doc_id: &str, section: Option<&str>, before_hash: Option<&str>, after_hash: Option<&str>, diff: Option<&[u8]>) -> Result<i64> {
        let diff_blob = match diff {
            Some(bytes) => Some(self.store_blob(bytes)?),
            None => None,
        };
        let payload = serde_json::json!({
            "kind": kind, "doc_id": doc_id, "section": section,
            "before_hash": before_hash, "after_hash": after_hash, "diff_blob": diff_blob,
        });
        let seq = self.log(EventKind::Op, payload)?;
        self.conn.execute(
            "insert into op (event_seq, kind, doc_id, section, before_hash, after_hash, diff_blob) values (?1,?2,?3,?4,?5,?6,?7)",
            params![seq, kind.as_str(), doc_id, section, before_hash, after_hash, diff_blob],
        )?;
        Ok(seq)
    }

    pub fn checkpoint(&mut self) -> Result<i64> {
        self.log(EventKind::Checkpoint, serde_json::json!({}))
    }

    pub fn error(&mut self, msg: &str) -> Result<i64> {
        self.log(EventKind::Error, serde_json::json!({"message": msg}))
    }

    pub fn report(&mut self, text: &str) -> Result<i64> {
        self.log(EventKind::Report, serde_json::json!({"text": text}))
    }

    pub fn gate(&mut self, class: MergeClass, reasons: &[String]) -> Result<i64> {
        self.log(EventKind::Gate, serde_json::json!({"class": class, "reasons": reasons}))
    }

    /// log a `review_verdict` event and insert the matching `review` row.
    #[allow(clippy::too_many_arguments)]
    pub fn review_verdict(
        &mut self,
        iteration: u32,
        reviewer_run_id: Option<&str>,
        verdict: Verdict,
        confidence: f64,
        findings_json: Value,
        commit: Option<&str>,
    ) -> Result<i64> {
        let payload = serde_json::json!({
            "iteration": iteration, "reviewer_run_id": reviewer_run_id, "verdict": verdict,
            "confidence": confidence, "findings": findings_json, "commit": commit,
        });
        let seq = self.log(EventKind::ReviewVerdict, payload)?;
        let findings_str = serde_json::to_string(&findings_json)?;
        self.conn.execute(
            "insert into review (run_id, iteration, reviewer_run_id, verdict, confidence, findings, commit_sha) values (?1,?2,?3,?4,?5,?6,?7)",
            params![self.run_id, iteration, reviewer_run_id, verdict.as_str(), confidence, findings_str, commit],
        )?;
        Ok(seq)
    }

    pub fn settle(&mut self, commits: &[String]) -> Result<i64> {
        self.log(EventKind::Settle, serde_json::json!({"commits": commits}))
    }

    pub fn conflict(&mut self, path: &str, diffs_json: Value, resolution: &str) -> Result<i64> {
        self.log(EventKind::Conflict, serde_json::json!({"path": path, "diffs": diffs_json, "resolution": resolution}))
    }

    pub fn check(&mut self, name: &str, verdict: bool, reason: Option<&str>) -> Result<i64> {
        self.log(EventKind::Check, serde_json::json!({"name": name, "verdict": verdict, "reason": reason}))
    }

    pub fn merge_start(&mut self, payload: Value) -> Result<i64> {
        self.log(EventKind::MergeStart, payload)
    }

    pub fn merge_end(&mut self, payload: Value) -> Result<i64> {
        self.log(EventKind::MergeEnd, payload)
    }

    pub fn deferred_request(&mut self, packet: Value) -> Result<i64> {
        self.log(EventKind::DeferredRequest, packet)
    }

    pub fn deferred_result(&mut self, result: Value) -> Result<i64> {
        self.log(EventKind::DeferredResult, result)
    }

    /// finish the run: log `run_end`, then update the `run` row (the one update the schema
    /// allows outside of `event`/`op`/`review`, which stay append-only). returns the final link
    /// of the hash chain, the same value copied into `run.final_hash`.
    #[allow(clippy::too_many_arguments)]
    pub fn run_end(
        &mut self,
        status: RunStatus,
        head_commit: Option<&str>,
        merge_commit: Option<&str>,
        justification: Option<&str>,
        settled_commits: &[String],
        access_read: &[String],
    ) -> Result<String> {
        let finished_at = crate::clock::now().to_rfc3339();
        let settled_json = serde_json::to_string(settled_commits)?;
        let access_json = serde_json::to_string(access_read)?;
        let payload = serde_json::json!({
            "status": status, "head_commit": head_commit, "merge_commit": merge_commit,
            "justification": justification, "settled_commits": settled_commits, "access_read": access_read,
        });
        self.log(EventKind::RunEnd, payload)?;
        let final_hash = self.prev_hash.clone();
        self.conn
            .execute(
                "update run set status = ?1, finished_at = ?2, head_commit = ?3, merge_commit = ?4,
                    justification = ?5, settled_commits = ?6, access_read = ?7, final_hash = ?8
                 where run_id = ?9",
                params![status.as_str(), finished_at, head_commit, merge_commit, justification, settled_json, access_json, final_hash, self.run_id],
            )
            .context("updating run row at run_end")?;
        Ok(final_hash)
    }

    /// checkpoint the wal, switch to a single-file rollback journal, vacuum, mark `user_version`
    /// as sealed, make the file read-only, and copy it into the local content store
    /// (`<state>/store/runs/<run-ulid>.sqlite`), also marked read-only. returns the store path.
    pub fn seal(self, cfg: &Config) -> Result<PathBuf> {
        self.conn.execute_batch("pragma wal_checkpoint(TRUNCATE);").context("checkpointing wal")?;
        self.conn.pragma_update(None, "journal_mode", "DELETE").context("leaving wal mode")?;
        self.conn.pragma_update(None, "user_version", 1).context("setting user_version")?;
        self.conn.execute_batch("vacuum;").context("vacuuming sealed run log")?;
        let path = self.path.clone();
        drop(self.conn);

        set_readonly(&path)?;

        let store_dir = store_runs_dir(cfg);
        std::fs::create_dir_all(&store_dir).with_context(|| format!("creating {}", store_dir.display()))?;
        let dest = store_dir.join(format!("{}.sqlite", self.run_id));
        std::fs::copy(&path, &dest).with_context(|| format!("copying {} to {}", path.display(), dest.display()))?;
        set_readonly(&dest)?;
        Ok(dest)
    }
}

fn set_readonly(path: &Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms).with_context(|| format!("setting {} read-only", path.display()))
}

/// `run_start`'s actor is also its own field on the `run` row; kept as a tiny helper so the
/// payload and the row never drift apart if this ever needs normalizing.
fn log_actor(_run_id: &str, actor: &str) -> String {
    actor.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn minimal_start() -> RunStart {
        RunStart { actor: "agent:curate@caleb".into(), deferred: false, ..Default::default() }
    }

    #[test]
    fn full_lifecycle_verify_search_and_seal() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let mut start = minimal_start();
        start.principal = Some("caleb".into());
        start.skill = Some("curate".into());
        start.session_id = Some("sess1".into());
        start.worktree_path = Some("/tmp/wt".into());
        start.base_commit = Some("deadbeef".into());
        start.parser_version = Some(1);

        let mut log = RunLog::create(&cfg, start).unwrap();
        let run_id = log.run_id().to_string();

        log.context(serde_json::json!({"task": "curate one capture"})).unwrap();
        for i in 0..25 {
            log.tool_call("read_file", serde_json::json!({"path": format!("docs/{i}.md")})).unwrap();
        }
        log.op(Operation::Append, "01K5A7Q2XW3ZRCM8T6YB4VJH0N", Some("notes"), Some("beforehash"), Some("afterhash"), Some(b"--- a\n+++ b\n"))
            .unwrap();
        log.decision("the append is in scope").unwrap();
        log.checkpoint().unwrap();
        log.report("appended one section").unwrap();
        for i in 0..5 {
            log.read(&format!("doc{i}"), "h").unwrap();
        }
        let final_hash = log.run_end(RunStatus::Ok, Some("headsha"), None, Some("justification text"), &["c1".into()], &["*".into()]).unwrap();
        assert!(!final_hash.is_empty());

        let sealed_path = log.seal(&cfg).unwrap();
        assert!(sealed_path.is_file());
        assert!(std::fs::metadata(&sealed_path).unwrap().permissions().readonly());

        let report = verify::verify(&sealed_path).unwrap();
        assert!(report.ok, "{report:?}");
        assert!(report.events >= 30, "expected at least 30 events, got {}", report.events);
        assert!(report.final_hash_matches);
        assert!(report.first_bad_seq.is_none());

        let opened = RunLog::open(&cfg, &run_id[..8]).unwrap();
        assert_eq!(opened.run_id(), run_id);

        let hits = verify::search(&sealed_path, "read_file").unwrap();
        assert!(!hits.is_empty(), "fts search should find the tool_call payloads");

        // corrupt one payload directly (dropping the append-only trigger first) and confirm
        // `verify` reports the first broken link, not just an overall failure.
        let corrupt_path = dir.path().join("corrupt.sqlite");
        std::fs::copy(&sealed_path, &corrupt_path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&corrupt_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let conn = Connection::open(&corrupt_path).unwrap();
        conn.execute("drop trigger event_no_update", []).unwrap();
        conn.execute("update event set payload = '{\"tampered\":true}' where seq = 2", []).unwrap();
        drop(conn);
        let bad_report = verify::verify(&corrupt_path).unwrap();
        assert!(!bad_report.ok);
        assert_eq!(bad_report.first_bad_seq, Some(2));
    }

    #[test]
    fn append_only_triggers_reject_update_and_delete_on_event() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let mut log = RunLog::create(&cfg, minimal_start()).unwrap();
        log.decision("x").unwrap();
        assert!(log.conn.execute("update event set payload = '{}' where seq = 1", []).is_err());
        assert!(log.conn.execute("delete from event where seq = 1", []).is_err());
    }

    #[test]
    fn append_only_triggers_reject_update_and_delete_on_op_and_review() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let mut log = RunLog::create(&cfg, minimal_start()).unwrap();
        log.op(Operation::Create, "01K5A7Q2XW3ZRCM8T6YB4VJH0N", None, None, Some("afterhash"), None).unwrap();
        assert!(log.conn.execute("update op set kind = 'delete' where op_id = 1", []).is_err());
        assert!(log.conn.execute("delete from op where op_id = 1", []).is_err());

        log.review_verdict(1, None, Verdict::Approve, 0.9, serde_json::json!([]), None).unwrap();
        assert!(log.conn.execute("update review set verdict = 'escalate' where review_id = 1", []).is_err());
        assert!(log.conn.execute("delete from review where review_id = 1", []).is_err());
    }

    #[test]
    fn open_rejects_ambiguous_prefix() {
        let dir = tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let a = RunLog::create(&cfg, minimal_start()).unwrap();
        let b = RunLog::create(&cfg, minimal_start()).unwrap();
        let common_prefix = a.run_id().chars().zip(b.run_id().chars()).take_while(|(x, y)| x == y).map(|(x, _)| x).collect::<String>();
        // two fresh ulids share their leading timestamp characters; the shared prefix (capped at
        // 8 characters, which can be shorter when a millisecond boundary was crossed) matches both
        // logs and must be reported as ambiguous.
        let n = common_prefix.len().min(8);
        assert!(n >= 4, "ulids created back to back should share at least 4 leading characters");
        assert!(RunLog::open(&cfg, &common_prefix[..n]).is_err());
    }
}
