//! full transcripts of every model call and every mcp tool call kb serves, one row each, in the
//! `transcripts` table of `<state>/state.sqlite`. a row holds the whole request (system prompt,
//! every message, schema) and the whole response, who caused it, what triggered it (a timer, the
//! pre-commit hook, a cli command, an mcp client), and the run and session it belongs to.
//!
//! run logs (design.md §16) record what a run decided; this table records what every model was
//! asked and answered, including calls made outside any run (the pre-commit classify, the quick
//! gate) and the calls an outside agent makes through mcp.

use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::cell::RefCell;
use std::sync::OnceLock;

const SCHEMA: &str = "
create table if not exists transcripts (
    id text primary key,
    ts text not null,
    kind text not null,
    role text,
    provider text,
    model text,
    actor text,
    trigger text not null,
    command text,
    run_id text,
    session_id text,
    request text not null,
    response text,
    error text,
    duration_ms integer,
    input_tokens integer,
    output_tokens integer
) strict;
create index if not exists transcripts_ts on transcripts(ts);
create index if not exists transcripts_run on transcripts(run_id);
create index if not exists transcripts_session on transcripts(session_id);
";

/// the run a call happens inside: set by `runlog::RunLog::create`, cleared when the run log is
/// sealed or dropped.
#[derive(Debug, Clone)]
struct RunContext {
	run_id: String,
	actor: String,
	session_id: Option<String>,
}

thread_local! {
	static RUNS: RefCell<Vec<RunContext>> = const { RefCell::new(Vec::new()) };
}

static DEFAULT_TRIGGER: OnceLock<String> = OnceLock::new();

/// removes its run from the current thread's context when dropped.
#[derive(Debug)]
pub struct RunGuard {
	run_id: String,
}

impl Drop for RunGuard {
	fn drop(&mut self) {
		RUNS.with(|r| r.borrow_mut().retain(|c| c.run_id != self.run_id));
	}
}

/// mark `run_id` as the run every call on this thread belongs to, until the guard drops.
pub fn enter_run(run_id: &str, actor: &str, session_id: Option<&str>) -> RunGuard {
	RUNS.with(|r| r.borrow_mut().push(RunContext { run_id: run_id.to_string(), actor: actor.to_string(), session_id: session_id.map(str::to_string) }));
	RunGuard { run_id: run_id.to_string() }
}

fn current_run() -> Option<RunContext> {
	RUNS.with(|r| r.borrow().last().cloned())
}

/// what started this process when `KB_TRIGGER` is unset: `kb mcp` and `kb serve` set it once.
pub fn set_default_trigger(trigger: &str) {
	let _ = DEFAULT_TRIGGER.set(trigger.to_string());
}

/// `KB_TRIGGER` (the systemd units set `timer:<job>`, the pre-commit hook `hook:pre-commit`),
/// else the process default, else `cli`.
pub fn trigger() -> String {
	std::env::var("KB_TRIGGER").ok().filter(|t| !t.trim().is_empty()).or_else(|| DEFAULT_TRIGGER.get().cloned()).unwrap_or_else(|| "cli".to_string())
}

fn command_line() -> String {
	std::env::args().skip(1).collect::<Vec<_>>().join(" ")
}

/// one call to record. `actor` is used only when no run is active on this thread.
pub struct Entry<'a> {
	pub kind: &'a str,
	pub role: &'a str,
	pub provider: Option<&'a str>,
	pub model: Option<&'a str>,
	pub actor: &'a str,
	pub request: Value,
	pub response: Option<Value>,
	pub error: Option<String>,
	pub started: DateTime<Utc>,
	pub duration_ms: u64,
	pub input_tokens: Option<u64>,
	pub output_tokens: Option<u64>,
}

fn open(state_dir: &std::path::Path) -> Result<Connection> {
	std::fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
	let path = state_dir.join("state.sqlite");
	let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
	conn.busy_timeout(std::time::Duration::from_millis(5000))?;
	conn.pragma_update(None, "journal_mode", "WAL")?;
	conn.execute_batch(SCHEMA).context("creating transcripts schema")?;
	Ok(conn)
}

/// write one transcript row; returns its id.
pub fn record(state_dir: &std::path::Path, entry: Entry<'_>) -> Result<String> {
	let id = crate::model::new_ulid();
	let run = current_run();
	let actor = run.as_ref().map(|r| r.actor.clone()).unwrap_or_else(|| entry.actor.to_string());
	let conn = open(state_dir)?;
	conn.execute(
		"insert into transcripts (id, ts, kind, role, provider, model, actor, trigger, command, run_id, session_id,
            request, response, error, duration_ms, input_tokens, output_tokens)
         values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
		params![
			id,
			entry.started.to_rfc3339(),
			entry.kind,
			entry.role,
			entry.provider,
			entry.model,
			actor,
			trigger(),
			command_line(),
			run.as_ref().map(|r| r.run_id.clone()),
			run.as_ref().and_then(|r| r.session_id.clone()),
			serde_json::to_string(&entry.request)?,
			entry.response.map(|v| serde_json::to_string(&v)).transpose()?,
			entry.error,
			entry.duration_ms as i64,
			entry.input_tokens.map(|n| n as i64),
			entry.output_tokens.map(|n| n as i64),
		],
	)
	.context("inserting transcript row")?;
	Ok(id)
}

/// one transcript row, as `kb transcript show` prints it.
#[derive(Debug, Clone, Serialize)]
pub struct Transcript {
	pub id: String,
	pub ts: String,
	pub kind: String,
	pub role: Option<String>,
	pub provider: Option<String>,
	pub model: Option<String>,
	pub actor: Option<String>,
	pub trigger: String,
	pub command: Option<String>,
	pub run_id: Option<String>,
	pub session_id: Option<String>,
	pub request: Value,
	pub response: Option<Value>,
	pub error: Option<String>,
	pub duration_ms: Option<i64>,
	pub input_tokens: Option<i64>,
	pub output_tokens: Option<i64>,
}

const COLUMNS: &str =
	"id, ts, kind, role, provider, model, actor, trigger, command, run_id, session_id, request, response, error, duration_ms, input_tokens, output_tokens";

fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Transcript> {
	let request: String = r.get(11)?;
	let response: Option<String> = r.get(12)?;
	Ok(Transcript {
		id: r.get(0)?,
		ts: r.get(1)?,
		kind: r.get(2)?,
		role: r.get(3)?,
		provider: r.get(4)?,
		model: r.get(5)?,
		actor: r.get(6)?,
		trigger: r.get(7)?,
		command: r.get(8)?,
		run_id: r.get(9)?,
		session_id: r.get(10)?,
		request: serde_json::from_str(&request).unwrap_or(Value::String(request)),
		response: response.map(|s| serde_json::from_str(&s).unwrap_or(Value::String(s))),
		error: r.get(13)?,
		duration_ms: r.get(14)?,
		input_tokens: r.get(15)?,
		output_tokens: r.get(16)?,
	})
}

/// filters for `list`; every field is optional.
#[derive(Debug, Clone, Default)]
pub struct Filter {
	pub since: Option<DateTime<Utc>>,
	pub kind: Option<String>,
	pub role: Option<String>,
	pub run_id: Option<String>,
	pub session_id: Option<String>,
	pub limit: usize,
}

/// transcripts newest first.
pub fn list(cfg: &Config, filter: &Filter) -> Result<Vec<Transcript>> {
	let conn = open(&cfg.state_dir())?;
	let mut sql = format!("select {COLUMNS} from transcripts where 1 = 1");
	let mut values: Vec<String> = Vec::new();
	for (column, value) in [
		("ts >=", filter.since.map(|d| d.to_rfc3339())),
		("kind =", filter.kind.clone()),
		("role =", filter.role.clone()),
		("run_id =", filter.run_id.clone()),
		("session_id =", filter.session_id.clone()),
	] {
		if let Some(v) = value {
			values.push(v);
			sql.push_str(&format!(" and {column} ?{}", values.len()));
		}
	}
	sql.push_str(" order by ts desc, id desc");
	if filter.limit > 0 {
		sql.push_str(&format!(" limit {}", filter.limit));
	}
	let mut stmt = conn.prepare(&sql)?;
	let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), from_row)?;
	Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// one transcript by id or unique prefix.
pub fn get(cfg: &Config, id: &str) -> Result<Option<Transcript>> {
	let conn = open(&cfg.state_dir())?;
	let mut stmt = conn.prepare(&format!("select {COLUMNS} from transcripts where id like ?1 || '%' order by id limit 2"))?;
	let rows: Vec<Transcript> = stmt.query_map([id], from_row)?.collect::<rusqlite::Result<_>>()?;
	match rows.len() {
		0 => Ok(None),
		1 => Ok(rows.into_iter().next()),
		_ => anyhow::bail!("transcript prefix `{id}` is ambiguous"),
	}
}

/// number of transcript rows; `None` when the table does not exist yet.
pub fn count(cfg: &Config) -> Option<i64> {
	let path = cfg.state_dir().join("state.sqlite");
	let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
	conn.query_row("select count(*) from transcripts", [], |r| r.get(0)).optional().ok().flatten()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn records_inside_and_outside_a_run() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let entry = |role: &'static str| Entry {
			kind: "llm",
			role,
			provider: Some("fake"),
			model: Some("m"),
			actor: "human:caleb",
			request: serde_json::json!({"messages": [{"role": "user", "content": "q"}]}),
			response: Some(serde_json::json!({"text": "a"})),
			error: None,
			started: Utc::now(),
			duration_ms: 3,
			input_tokens: Some(1),
			output_tokens: Some(2),
		};
		record(&cfg.state_dir(), entry("classify")).unwrap();
		{
			let _guard = enter_run("01K5A7Q2XW3ZRCM8T6YB4VJH0A", "agent:curate@svc", Some("01K5A7Q2XW3ZRCM8T6YB4VJH0B"));
			record(&cfg.state_dir(), entry("review")).unwrap();
		}
		record(&cfg.state_dir(), entry("answer")).unwrap();

		let rows = list(&cfg, &Filter::default()).unwrap();
		assert_eq!(rows.len(), 3);
		let review = rows.iter().find(|r| r.role.as_deref() == Some("review")).unwrap();
		assert_eq!(review.actor.as_deref(), Some("agent:curate@svc"));
		assert_eq!(review.run_id.as_deref(), Some("01K5A7Q2XW3ZRCM8T6YB4VJH0A"));
		assert_eq!(review.session_id.as_deref(), Some("01K5A7Q2XW3ZRCM8T6YB4VJH0B"));
		let answer = rows.iter().find(|r| r.role.as_deref() == Some("answer")).unwrap();
		assert!(answer.run_id.is_none(), "the guard must clear the run when dropped");
		assert_eq!(answer.actor.as_deref(), Some("human:caleb"));
		assert_eq!(answer.request["messages"][0]["content"], "q");
		let by_run = list(&cfg, &Filter { run_id: Some("01K5A7Q2XW3ZRCM8T6YB4VJH0A".into()), ..Filter::default() }).unwrap();
		assert_eq!(by_run.len(), 1);
		assert!(get(&cfg, &review.id).unwrap().is_some());
	}
}
