//! sqlite table definitions for one run log. design.md §16; column changes over the design-0003
//! schema (removed `claim`; added `session_id`, `worktree_path`, `settled_commits`, `merge_commit`,
//! `justification`, `access_read`, `deferred`; new `review` table; new event kinds) are listed there.
//!
//! `event` is the hash-chained, append-only log: `event_ai` mirrors every insert into `event_fts`
//! (an external-content fts5 index over `payload`), and `event_no_update`/`event_no_delete` make the
//! table append-only by raising on any attempt to change or remove a row. `op` and `review` get the
//! same append-only pair. `event.seq` is declared `integer primary key`, which sqlite keeps as a
//! rowid alias even under `strict` (needed for fts5's `content_rowid`). `tool_name` and `doc_id` are
//! generated columns pulled out of `payload` so callers can index and filter events without parsing
//! json first.
use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA: &str = r#"
create table run (
    run_id text primary key,
    parent_run_id text,
    actor text not null,
    principal text,
    skill text,
    skill_version text,
    provider text,
    model text,
    reasoning_config text,
    prompt_checksums text not null default '{}',
    parser_version integer,
    base_commit text,
    head_commit text,
    pr_url text,
    started_at text not null,
    finished_at text,
    status text not null default 'running'
        check (status in ('running', 'ok', 'failed', 'escalated', 'deferred')),
    final_hash text,
    session_id text,
    worktree_path text,
    settled_commits text not null default '[]',
    merge_commit text,
    justification text,
    access_read text not null default '[]',
    deferred integer not null default 0
) strict;

create table event (
    seq integer primary key,
    run_id text not null references run(run_id),
    ts text not null,
    kind text not null check (kind in (
        'run_start', 'context', 'llm_request', 'llm_response', 'tool_call', 'tool_result',
        'read', 'policy_decision', 'decision', 'op', 'checkpoint', 'error', 'report', 'run_end',
        'gate', 'review_verdict', 'settle', 'conflict', 'check', 'classify',
        'merge_start', 'merge_end', 'deferred_request', 'deferred_result'
    )),
    payload text not null default '{}',
    prev_hash text not null,
    hash text not null,
    tool_name text generated always as (json_extract(payload, '$.name')) virtual,
    doc_id text generated always as (json_extract(payload, '$.doc_id')) virtual
) strict;

create index event_run_idx on event(run_id);
create index event_kind_idx on event(kind);
create index event_doc_idx on event(doc_id);

create virtual table event_fts using fts5(
    payload,
    content = 'event',
    content_rowid = 'seq'
);

create trigger event_ai after insert on event begin
    insert into event_fts(rowid, payload) values (new.seq, new.payload);
end;

create trigger event_no_update before update on event begin
    select raise(abort, 'event is append-only');
end;

create trigger event_no_delete before delete on event begin
    select raise(abort, 'event is append-only');
end;

create table blob (
    sha256 text primary key,
    size integer not null,
    content blob not null
) strict;

create table op (
    op_id integer primary key,
    event_seq integer not null references event(seq),
    kind text not null check (kind in (
        'create', 'append', 'revise', 'rewrite', 'supersede', 'split', 'merge', 'link',
        'retire', 'delete', 'escalate'
    )),
    doc_id text,
    section text,
    before_hash text,
    after_hash text,
    diff_blob text references blob(sha256)
) strict;

create index op_event_idx on op(event_seq);
create index op_doc_idx on op(doc_id);

create trigger op_no_update before update on op begin
    select raise(abort, 'op is append-only');
end;

create trigger op_no_delete before delete on op begin
    select raise(abort, 'op is append-only');
end;

-- the design's field name for the last column is `commit`, a sql keyword; stored here as
-- `commit_sha` to avoid quoting it everywhere it is read or written.
create table review (
    review_id integer primary key,
    run_id text not null references run(run_id),
    iteration integer not null,
    reviewer_run_id text,
    verdict text not null check (verdict in ('approve', 'request_changes', 'escalate')),
    confidence real,
    findings text not null default '[]',
    commit_sha text
) strict;

create index review_run_idx on review(run_id);

create trigger review_no_update before update on review begin
    select raise(abort, 'review is append-only');
end;

create trigger review_no_delete before delete on review begin
    select raise(abort, 'review is append-only');
end;
"#;

/// create every table, index, virtual table, and trigger for one run log. call once on a fresh
/// file, after `journal_mode` and `synchronous` are set.
pub fn create_schema(conn: &Connection) -> Result<()> {
	conn.execute_batch(SCHEMA).context("creating run log schema")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn schema_creates_and_fts_round_trips() {
		let conn = Connection::open_in_memory().unwrap();
		create_schema(&conn).unwrap();
		conn.execute("insert into run (run_id, actor, started_at) values ('r1', 'human:x', '2026-01-01T00:00:00Z')", []).unwrap();
		conn.execute(
			"insert into event (run_id, ts, kind, payload, prev_hash, hash) values ('r1','2026-01-01T00:00:00Z','tool_call','{\"name\":\"read_file\"}','a','b')",
			[],
		)
		.unwrap();
		let hit: String = conn.query_row("select payload from event_fts where event_fts match 'read_file'", [], |r| r.get(0)).unwrap();
		assert!(hit.contains("read_file"));
		let tool_name: String = conn.query_row("select tool_name from event", [], |r| r.get(0)).unwrap();
		assert_eq!(tool_name, "read_file");
	}

	#[test]
	fn event_table_is_append_only() {
		let conn = Connection::open_in_memory().unwrap();
		create_schema(&conn).unwrap();
		conn.execute("insert into run (run_id, actor, started_at) values ('r1','human:x','2026-01-01T00:00:00Z')", []).unwrap();
		conn.execute("insert into event (run_id, ts, kind, payload, prev_hash, hash) values ('r1','2026-01-01T00:00:00Z','decision','{}','a','b')", [])
			.unwrap();
		assert!(conn.execute("update event set payload = '{}' where seq = 1", []).is_err());
		assert!(conn.execute("delete from event where seq = 1", []).is_err());
	}
}
