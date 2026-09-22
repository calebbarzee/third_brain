//! sqlite table definitions. one file per artifact; see design.md §17.2.
//!
//! `chunks_fts` is an external-content fts5 index over `chunks(prefix, text)`. sqlite's fts5 needs an
//! integer rowid to key external content, so `chunks.chunk_id` is declared `integer primary key`
//! (a rowid alias); the content-addressed identity used for incremental dedup later is `chunk_hash`.
use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA: &str = r#"
create table documents (
    id text primary key,
    kind text not null default 'document',
    path text not null,
    layer text,
    title text,
    status text,
    owner text,
    created text,
    superseded_by text,
    parent_id text,
    frontmatter text not null default '{}',
    body text not null default '',
    content_hash text,
    read_groups text not null default '[]',
    last_updated text,
    last_human_commit text,
    last_run text,
    churn_30d integer not null default 0,
    churn_90d integer not null default 0,
    churn_lines_90d real not null default 0.0,
    agent_share_90d real not null default 0.0,
    word_count integer not null default 0,
    section_count integer not null default 0,
    criticality_pct real,
    crit_band text
);

create index documents_status_idx on documents(status);
create index documents_layer_idx on documents(layer);
create index documents_parent_idx on documents(parent_id);

create table captures (
    id text primary key,
    adapter text,
    source_uri text,
    captured_at text,
    original_sha256 text,
    original_media_type text,
    normalized_text text
);

create table chunks (
    chunk_id integer primary key,
    chunk_hash text not null unique,
    doc_id text not null,
    kind text not null,
    status text not null,
    heading_path text not null,
    ordinal integer not null,
    text text not null,
    prefix text
);

create index chunks_doc_idx on chunks(doc_id);

create virtual table chunks_fts using fts5(
    prefix,
    text,
    content = 'chunks',
    content_rowid = 'chunk_id'
);

create trigger chunks_ai after insert on chunks begin
    insert into chunks_fts(rowid, prefix, text) values (new.chunk_id, new.prefix, new.text);
end;

create trigger chunks_ad after delete on chunks begin
    insert into chunks_fts(chunks_fts, rowid, prefix, text) values ('delete', old.chunk_id, old.prefix, old.text);
end;

create trigger chunks_au after update on chunks begin
    insert into chunks_fts(chunks_fts, rowid, prefix, text) values ('delete', old.chunk_id, old.prefix, old.text);
    insert into chunks_fts(rowid, prefix, text) values (new.chunk_id, new.prefix, new.text);
end;

create table code_refs (
    repo text,
    path text,
    line integer,
    doc_id text,
    commit_sha text,
    stale integer not null default 0
);

create table refs (
    from_id text not null,
    rel text not null,
    uri text not null,
    scheme text,
    resolved_id text,
    target_sha256 text,
    origin text not null,
    line integer,
    drift_state text,
    drift_detail text
);

create index refs_from_idx on refs(from_id);
create index refs_in_idx on refs(resolved_id, rel);

create table contributors (
    doc_id text not null,
    actor text not null,
    role text not null,
    commits integer not null default 0,
    first_commit text,
    last_commit text
);

create index contributors_doc_idx on contributors(doc_id);

create table graph_metrics (
    doc_id text primary key,
    in_canon integer not null default 0,
    in_code integer not null default 0,
    out_degree integer not null default 0,
    pagerank real not null default 0.0,
    reads_90d integer not null default 0,
    cites_90d integer not null default 0,
    pct_in_canon real not null default 0.0,
    pct_in_code real not null default 0.0,
    pct_pagerank real not null default 0.0,
    pct_reads real not null default 0.0,
    pct_cites real not null default 0.0
);

create table tombstones (
    id text primary key,
    title text,
    path text,
    deleted_at text,
    deleting_commit text,
    run_id text,
    superseded_by text
);

create table embedding_cache (
    chunk_hash text not null,
    embedding_model text not null,
    embedding blob not null,
    primary key (chunk_hash, embedding_model)
);

create table prefix_cache (
    chunk_hash text not null,
    prefix_model text not null,
    prefix text not null,
    primary key (chunk_hash, prefix_model)
);

create table findings (
    kind text not null,
    doc_ids text not null default '[]',
    detail text
);

create table meta (
    key text primary key,
    value text not null
);
"#;

/// create every table, index, virtual table, and trigger for one artifact. call once on a fresh file.
pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).context("creating index schema")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_creates_and_fts_round_trips() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn.execute(
            "insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text, prefix) values ('h1','d1','document','canon','_body',0,'hello world','')",
            [],
        )
        .unwrap();
        let hit: String = conn
            .query_row("select text from chunks_fts where chunks_fts match 'hello'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hit, "hello world");

        conn.execute("delete from chunks where doc_id = 'd1'", []).unwrap();
        let count: i64 = conn.query_row("select count(*) from chunks_fts where chunks_fts match 'hello'", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }
}
