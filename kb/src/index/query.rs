//! read-side queries over one artifact: search, get, refs, ls, blame, show. design.md §17.3, §18.
use crate::config::Config;
use crate::git;
use crate::index::build::{RefRow, open_newest};
use crate::index::gitlog;
use crate::model;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::{HashSet};
use std::path::Path;

/// resolve a full ulid or a unique prefix to a document id.
pub fn resolve_id(conn: &Connection, id_or_prefix: &str) -> Result<String> {
    if model::is_ulid(id_or_prefix) {
        let exact: Option<String> = conn.query_row("select id from documents where id = ?1", params![id_or_prefix], |r| r.get(0)).optional()?;
        if let Some(id) = exact {
            return Ok(id);
        }
    }
    let mut stmt = conn.prepare("select id from documents where id like ?1")?;
    let matches: Vec<String> = stmt.query_map(params![format!("{id_or_prefix}%")], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    match matches.len() {
        0 => bail!("no document matches id or prefix `{id_or_prefix}`"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => bail!("id prefix `{id_or_prefix}` is ambiguous ({n} matches)"),
    }
}

fn row_to_ref(r: &rusqlite::Row) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        from_id: r.get(0)?,
        rel: r.get(1)?,
        uri: r.get(2)?,
        scheme: r.get(3)?,
        resolved_id: r.get(4)?,
        target_sha256: r.get(5)?,
        origin: r.get(6)?,
        line: r.get(7)?,
        drift_state: r.get(8)?,
        drift_detail: r.get(9)?,
    })
}

const REF_COLUMNS: &str = "from_id, rel, uri, scheme, resolved_id, target_sha256, origin, line, drift_state, drift_detail";

fn refs_out(conn: &Connection, id: &str) -> Result<Vec<RefRow>> {
    let mut stmt = conn.prepare(&format!("select {REF_COLUMNS} from refs where from_id = ?1"))?;
    Ok(stmt.query_map(params![id], row_to_ref)?.collect::<rusqlite::Result<_>>()?)
}

fn refs_incoming(conn: &Connection, id: &str) -> Result<Vec<RefRow>> {
    let mut stmt = conn.prepare(&format!("select {REF_COLUMNS} from refs where resolved_id = ?1"))?;
    Ok(stmt.query_map(params![id], row_to_ref)?.collect::<rusqlite::Result<_>>()?)
}

fn responsible_for_doc(conn: &Connection, id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("select distinct actor from contributors where doc_id = ?1 and role = 'responsible' order by actor")?;
    Ok(stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

// ---------------------------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub limit: usize,
    pub status_any: bool,
    pub include_captures: bool,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self { limit: 5, status_any: false, include_captures: false }
    }
}

/// the handful of document columns a search hit decorates a chunk match with.
struct DocSummary {
    title: Option<String>,
    layer: Option<String>,
    status: Option<String>,
    kind: String,
    path: String,
    criticality_pct: Option<f64>,
    last_updated: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub doc_id: String,
    pub title: Option<String>,
    pub layer: Option<String>,
    pub status: Option<String>,
    pub kind: String,
    /// the capture's source uri, when `kind = capture` (a capture's `path` column is its
    /// `source_uri`); `None` for a document hit. design.md §18: a capture result shows kind and
    /// source_uri.
    pub source_uri: Option<String>,
    pub criticality_pct: Option<f64>,
    pub heading_path: String,
    pub snippet: String,
    /// higher is better (negated bm25; fts5's own bm25 scores are negative, more negative = closer).
    pub score: f64,
    pub last_updated: Option<String>,
    pub responsible: Vec<String>,
    pub source_refs: Vec<RefRow>,
    pub code_refs: Vec<RefRow>,
    pub asset_refs: Vec<RefRow>,
}

/// turn one raw whitespace-delimited token into an fts5 string literal, so it can never be read
/// as fts5 query syntax (column filters, `NOT`/`-`, `*` prefix, `^`, parentheses, ...). an
/// embedded `"` is escaped by doubling it, per fts5's own string-literal rules.
fn quote_fts_token(tok: &str) -> String {
    format!("\"{}\"", tok.replace('"', "\"\""))
}

/// turn a raw user query into a safe fts5 match expression: whitespace-separated barewords are
/// each quoted as an fts5 string literal, a user-supplied `"..."` phrase is passed through
/// unquoted-again so it keeps its phrase-match semantics, and the pieces are joined with fts5's
/// implicit `AND`. this is what keeps a hyphenated word like `foo-bar` (`-` is fts5's column
/// filter / NOT operator) from being read as query syntax instead of a search term.
fn sanitize_fts_query(query: &str) -> String {
    let mut out = Vec::new();
    let mut chars = query.trim().chars().peekable();
    let mut buf = String::new();
    while let Some(&c) = chars.peek() {
        if c == '"' {
            let mut phrase = String::from(chars.next().unwrap());
            for c2 in chars.by_ref() {
                phrase.push(c2);
                if c2 == '"' {
                    break;
                }
            }
            if !buf.is_empty() {
                out.push(quote_fts_token(&buf));
                buf.clear();
            }
            out.push(phrase);
        } else if c.is_whitespace() {
            chars.next();
            if !buf.is_empty() {
                out.push(quote_fts_token(&buf));
                buf.clear();
            }
        } else {
            buf.push(c);
            chars.next();
        }
    }
    if !buf.is_empty() {
        out.push(quote_fts_token(&buf));
    }
    out.join(" ")
}

#[allow(clippy::type_complexity)]
fn run_fts_query(conn: &Connection, sql: &str, match_expr: &str, pool_limit: i64) -> Result<Vec<(String, String, f64, String)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![match_expr, pool_limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, String>(3)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// fts5 bm25 search over the candidate chunk set, collapsed to the best-scoring chunk per document.
/// candidates are `kind = 'document'` (plus `'capture'` when `include_captures`) and `status in
/// (canon, draft)` unless `status_any` widens it to every status (design.md §18).
pub fn search(conn: &Connection, query: &str, opts: &SearchOpts) -> Result<Vec<SearchHit>> {
    let kind_clause = if opts.include_captures { "c.kind in ('document','capture')" } else { "c.kind = 'document'" };
    let status_clause = if opts.status_any { "1 = 1" } else { "c.status in ('canon','draft')" };
    let sql = format!(
        "select c.doc_id, c.heading_path, bm25(chunks_fts) as score, snippet(chunks_fts, 1, '', '', ' … ', 16) as snip
         from chunks_fts join chunks c on c.chunk_id = chunks_fts.rowid
         where chunks_fts match ?1 and {kind_clause} and {status_clause}
         order by score
         limit ?2"
    );
    // fetch a wider pool than the final limit so collapsing to one chunk per document still leaves
    // `limit` distinct documents.
    let pool_limit = (opts.limit.max(1) * 20) as i64;

    // never hand the user's raw text straight to fts5's own query syntax: sanitize it into safe
    // string-literal tokens first, and if that still fails to parse (an unbalanced quote, say),
    // fall back to treating the whole query as one literal phrase. either way, a raw sqlite/fts5
    // error never reaches the caller.
    let sanitized = sanitize_fts_query(query);
    let rows = match run_fts_query(conn, &sql, &sanitized, pool_limit) {
        Ok(rows) => rows,
        Err(_) => {
            let fallback = quote_fts_token(query.trim());
            run_fts_query(conn, &sql, &fallback, pool_limit).map_err(|_| anyhow::anyhow!("search query `{query}` could not be understood"))?
        }
    };

    // lower (more negative) bm25 is a better match; collapse to the best chunk per document.
    let mut best: std::collections::HashMap<String, (String, f64, String)> = std::collections::HashMap::new();
    for (doc_id, heading_path, bm25, snip) in rows {
        let score = -bm25;
        best.entry(doc_id).and_modify(|slot| {
            if score > slot.1 {
                *slot = (heading_path.clone(), score, snip.clone());
            }
        }).or_insert((heading_path, score, snip));
    }
    let mut ranked: Vec<(String, (String, f64, String))> = best.into_iter().collect();
    ranked.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(opts.limit);

    let mut hits = Vec::with_capacity(ranked.len());
    for (doc_id, (heading_path, score, snippet)) in ranked {
        let snippet: String = if snippet.chars().count() > 300 { snippet.chars().take(300).collect::<String>() + "…" } else { snippet };
        let summary: DocSummary =
            conn.query_row("select title, layer, status, kind, path, criticality_pct, last_updated from documents where id = ?1", params![doc_id], |r| {
                Ok(DocSummary {
                    title: r.get(0)?,
                    layer: r.get(1)?,
                    status: r.get(2)?,
                    kind: r.get(3)?,
                    path: r.get(4)?,
                    criticality_pct: r.get(5)?,
                    last_updated: r.get(6)?,
                })
            })?;
        let DocSummary { title, layer, status, kind, path, criticality_pct, last_updated } = summary;
        let source_uri = if kind == "capture" { Some(path) } else { None };
        let all_refs = refs_out(conn, &doc_id)?;
        let source_refs = all_refs.iter().filter(|r| r.rel == "source").cloned().collect();
        let code_refs = all_refs.iter().filter(|r| r.rel == "code").cloned().collect();
        let asset_refs = all_refs.iter().filter(|r| r.rel == "asset").cloned().collect();
        let responsible = responsible_for_doc(conn, &doc_id)?;
        hits.push(SearchHit {
            doc_id,
            title,
            layer,
            status,
            kind,
            source_uri,
            criticality_pct,
            heading_path,
            snippet,
            score,
            last_updated,
            responsible,
            source_refs,
            code_refs,
            asset_refs,
        });
    }
    Ok(hits)
}

// ---------------------------------------------------------------------------------------------
// get
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Forward {
    pub id: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub chain_continues: bool,
}

/// the catalog-derived fields of a capture's `captures` row: adapter, source, and original hashes,
/// alongside the `documents` row's `body` (normalized text) and `content_hash` (`sha256_text`).
/// `None` for a document. design.md §18: `get(<capture id>)` returns the capture with body and
/// source fields.
#[derive(Debug, Clone, Serialize)]
pub struct CaptureFields {
    pub adapter: Option<String>,
    pub source_uri: Option<String>,
    pub captured_at: Option<String>,
    pub original_sha256: Option<String>,
    pub original_media_type: Option<String>,
}

fn find_capture_fields(conn: &Connection, id: &str) -> Result<Option<CaptureFields>> {
    Ok(conn
        .query_row("select adapter, source_uri, captured_at, original_sha256, original_media_type from captures where id = ?1", params![id], |r| {
            Ok(CaptureFields { adapter: r.get(0)?, source_uri: r.get(1)?, captured_at: r.get(2)?, original_sha256: r.get(3)?, original_media_type: r.get(4)? })
        })
        .optional()?)
}

#[derive(Debug, Clone, Serialize)]
pub struct DocRecord {
    pub id: String,
    pub kind: String,
    pub path: String,
    pub layer: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub owner: Option<String>,
    pub created: Option<String>,
    pub superseded_by: Option<String>,
    pub parent_id: Option<String>,
    pub frontmatter: serde_json::Value,
    pub body: String,
    pub content_hash: Option<String>,
    pub read_groups: Vec<String>,
    pub last_updated: Option<String>,
    pub last_human_commit: Option<String>,
    pub last_run: Option<String>,
    pub churn_30d: i64,
    pub churn_90d: i64,
    pub churn_lines_90d: f64,
    pub agent_share_90d: f64,
    pub word_count: i64,
    pub section_count: i64,
    pub criticality_pct: Option<f64>,
    pub crit_band: Option<String>,
    pub refs_out: Vec<RefRow>,
    pub refs_in: Vec<RefRow>,
    /// one-hop forward target when `superseded_by` is a `kb:` uri (design.md §17.3).
    pub forward: Option<Forward>,
    /// `Some` when `kind = capture`; the catalog's own fields for this row.
    pub capture: Option<CaptureFields>,
}

#[allow(clippy::type_complexity)]
struct DocRow {
    id: String,
    kind: String,
    path: String,
    layer: Option<String>,
    title: Option<String>,
    status: Option<String>,
    owner: Option<String>,
    created: Option<String>,
    superseded_by: Option<String>,
    parent_id: Option<String>,
    frontmatter: String,
    body: String,
    content_hash: Option<String>,
    read_groups: String,
    last_updated: Option<String>,
    last_human_commit: Option<String>,
    last_run: Option<String>,
    churn_30d: i64,
    churn_90d: i64,
    churn_lines_90d: f64,
    agent_share_90d: f64,
    word_count: i64,
    section_count: i64,
    criticality_pct: Option<f64>,
    crit_band: Option<String>,
}

const DOC_COLUMNS: &str = "id, kind, path, layer, title, status, owner, created, superseded_by, parent_id, frontmatter, body,
    content_hash, read_groups, last_updated, last_human_commit, last_run, churn_30d, churn_90d, churn_lines_90d,
    agent_share_90d, word_count, section_count, criticality_pct, crit_band";

fn row_to_doc(r: &rusqlite::Row) -> rusqlite::Result<DocRow> {
    Ok(DocRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        path: r.get(2)?,
        layer: r.get(3)?,
        title: r.get(4)?,
        status: r.get(5)?,
        owner: r.get(6)?,
        created: r.get(7)?,
        superseded_by: r.get(8)?,
        parent_id: r.get(9)?,
        frontmatter: r.get(10)?,
        body: r.get(11)?,
        content_hash: r.get(12)?,
        read_groups: r.get(13)?,
        last_updated: r.get(14)?,
        last_human_commit: r.get(15)?,
        last_run: r.get(16)?,
        churn_30d: r.get(17)?,
        churn_90d: r.get(18)?,
        churn_lines_90d: r.get(19)?,
        agent_share_90d: r.get(20)?,
        word_count: r.get(21)?,
        section_count: r.get(22)?,
        criticality_pct: r.get(23)?,
        crit_band: r.get(24)?,
    })
}

fn find_doc_row(conn: &Connection, id: &str) -> Result<Option<DocRow>> {
    Ok(conn.query_row(&format!("select {DOC_COLUMNS} from documents where id = ?1"), params![id], row_to_doc).optional()?)
}

/// `id` accepts a full ulid or a unique prefix. `None` when the artifact has no matching document.
pub fn get(conn: &Connection, id: &str) -> Result<Option<DocRecord>> {
    let resolved = match resolve_id(conn, id) {
        Ok(r) => r,
        // an ambiguous prefix is a user error worth surfacing, not a plain "no match" -- same
        // treatment as `refs`, `show`, and `blame`, which all propagate resolve_id's error as-is.
        Err(e) if e.to_string().contains("ambiguous") => return Err(e),
        Err(_) => return Ok(None),
    };
    let Some(row) = find_doc_row(conn, &resolved)? else { return Ok(None) };

    let forward = match row.superseded_by.as_deref() {
        Some(sb) if sb != "none" => match model::parse_kb_uri(sb) {
            Some((target_id, _)) => {
                let target: Option<(Option<String>, Option<String>)> =
                    conn.query_row("select title, status from documents where id = ?1", params![target_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                target.map(|(title, status)| {
                    let chain_continues = matches!(status.as_deref(), Some("superseded") | Some("archived"));
                    Forward { id: target_id, title, status, chain_continues }
                })
            }
            None => None,
        },
        _ => None,
    };

    let capture = if row.kind == "capture" { find_capture_fields(conn, &row.id)? } else { None };

    Ok(Some(DocRecord {
        refs_out: refs_out(conn, &row.id)?,
        refs_in: refs_incoming(conn, &row.id)?,
        frontmatter: serde_json::from_str(&row.frontmatter).unwrap_or(serde_json::Value::Null),
        read_groups: serde_json::from_str(&row.read_groups).unwrap_or_default(),
        capture,
        id: row.id,
        kind: row.kind,
        path: row.path,
        layer: row.layer,
        title: row.title,
        status: row.status,
        owner: row.owner,
        created: row.created,
        superseded_by: row.superseded_by,
        parent_id: row.parent_id,
        body: row.body,
        content_hash: row.content_hash,
        last_updated: row.last_updated,
        last_human_commit: row.last_human_commit,
        last_run: row.last_run,
        churn_30d: row.churn_30d,
        churn_90d: row.churn_90d,
        churn_lines_90d: row.churn_lines_90d,
        agent_share_90d: row.agent_share_90d,
        word_count: row.word_count,
        section_count: row.section_count,
        criticality_pct: row.criticality_pct,
        crit_band: row.crit_band,
        forward,
    }))
}

// ---------------------------------------------------------------------------------------------
// refs
// ---------------------------------------------------------------------------------------------

/// breadth-first walk of the reference graph starting at `id`, up to `depth` hops. `incoming`
/// walks `refs_in` edges (who points at this node) instead of outgoing ones.
pub fn refs(conn: &Connection, id: &str, incoming: bool, depth: u32) -> Result<Vec<RefRow>> {
    let start = resolve_id(conn, id)?;
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.clone());
    let mut frontier = vec![start];
    let mut collected = Vec::new();
    for _ in 0..depth.max(1) {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for node in &frontier {
            let edges = if incoming { refs_incoming(conn, node)? } else { refs_out(conn, node)? };
            for edge in edges {
                let other = if incoming { Some(edge.from_id.clone()) } else { edge.resolved_id.clone() };
                if let Some(other) = other
                    && visited.insert(other.clone())
                {
                    next.push(other);
                }
                collected.push(edge);
            }
        }
        frontier = next;
    }
    Ok(collected)
}

// ---------------------------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct LsFilter {
    pub layer: Option<String>,
    pub status: Option<String>,
    pub tag: Option<String>,
    /// `document` or `capture`; `None` defaults to `document` so a plain `kb index ls` keeps the
    /// same candidate set search defaults to (design.md invariant 9: captures are reached only by
    /// explicit request).
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LsRow {
    pub id: String,
    pub title: Option<String>,
    pub layer: Option<String>,
    pub status: Option<String>,
    pub kind: String,
    pub criticality_pct: Option<f64>,
    pub last_updated: Option<String>,
}

pub fn ls(conn: &Connection, filter: &LsFilter) -> Result<Vec<LsRow>> {
    let mut sql = String::from("select id, title, layer, status, kind, criticality_pct, last_updated from documents where 1 = 1");
    let mut values: Vec<String> = Vec::new();
    sql.push_str(" and kind = ?");
    values.push(filter.kind.clone().unwrap_or_else(|| "document".to_string()));
    if let Some(layer) = &filter.layer {
        sql.push_str(" and layer = ?");
        values.push(layer.clone());
    }
    if let Some(status) = &filter.status {
        sql.push_str(" and status = ?");
        values.push(status.clone());
    }
    if let Some(tag) = &filter.tag {
        sql.push_str(" and exists (select 1 from json_each(frontmatter, '$.tags') where value = ?)");
        values.push(tag.clone());
    }
    sql.push_str(" order by id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |r| {
        Ok(LsRow { id: r.get(0)?, title: r.get(1)?, layer: r.get(2)?, status: r.get(3)?, kind: r.get(4)?, criticality_pct: r.get(5)?, last_updated: r.get(6)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---------------------------------------------------------------------------------------------
// blame
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BlameRow {
    pub heading_path: String,
    pub actor: String,
    pub commit: String,
    pub date: Option<String>,
}

/// per-section last actor and commit, via `git blame --line-porcelain` on the whole file (one git
/// invocation) rather than one `git log -L` per section: `--line-porcelain` repeats full metadata
/// for every output line in file order, so the parse is a single linear pass, and one process per
/// document is cheaper than one per section for documents with many headings.
pub fn blame(cfg: &Config, conn: &Connection, id: &str) -> Result<Vec<BlameRow>> {
    let resolved = resolve_id(conn, id)?;
    let (path, body): (String, String) = conn.query_row("select path, body from documents where id = ?1", params![resolved], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let rev: Option<String> = conn.query_row("select value from meta where key = 'commit'", [], |r| r.get(0)).optional()?;
    let rev = rev.unwrap_or_else(|| "HEAD".to_string());

    let sections = crate::parse::parse_body(&body).sections;
    let full_bytes = git::show_file(&cfg.root, &rev, &path)?;
    let full_text = String::from_utf8_lossy(&full_bytes).into_owned();
    let body_start = full_text.len().saturating_sub(body.len());
    let offset_lines = full_text.get(..body_start).unwrap_or("").matches('\n').count();

    let raw = git::blame_porcelain(&cfg.root, &rev, &path)?;
    let lines = parse_blame_porcelain(&raw);

    let mut rows = Vec::new();
    for section in &sections {
        let start_line = offset_lines + section.line;
        let section_line_count = body.get(section.start..section.end).map(|s| s.lines().count()).unwrap_or(1).max(1);
        let end_line = start_line + section_line_count - 1;
        let best = lines
            .iter()
            .enumerate()
            .filter(|(idx, _)| { let line_no = idx + 1; line_no >= start_line && line_no <= end_line })
            .map(|(_, entry)| entry)
            .max_by_key(|entry| entry.2);
        if let Some((sha, author, time)) = best {
            let date = chrono::DateTime::<chrono::Utc>::from_timestamp(*time, 0).map(|d| d.to_rfc3339());
            rows.push(BlameRow { heading_path: section.path.clone(), actor: author.clone(), commit: sha.clone(), date });
        }
    }
    Ok(rows)
}

/// (sha, author name, author-time unix seconds) for each final line, in file order.
fn parse_blame_porcelain(raw: &str) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    let mut cur_sha = String::new();
    let mut cur_author = String::new();
    let mut cur_time: i64 = 0;
    for line in raw.lines() {
        if line.starts_with('\t') {
            out.push((cur_sha.clone(), cur_author.clone(), cur_time));
        } else if let Some(rest) = line.strip_prefix("author ") {
            cur_author = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("author-time ") {
            cur_time = rest.trim().parse().unwrap_or(0);
        } else if is_blame_header(line) {
            cur_sha = line.split(' ').next().unwrap_or("").to_string();
        }
    }
    out
}

/// a porcelain header line: `<40-hex-sha> <orig-line> <final-line> [<count>]`.
fn is_blame_header(line: &str) -> bool {
    let Some(sha) = line.split(' ').next() else { return false };
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ShowResult {
    pub frontmatter: serde_json::Value,
    pub body: String,
    pub last_updated: Option<String>,
    /// (actor, role) pairs; `role` is `author`, `co-author`, or `responsible`.
    pub contributors: Vec<(String, String)>,
    pub updated_state: String,
    pub updated_at: Option<String>,
}

/// frontmatter, a derived block, and the body. the derived block comes from the newest artifact
/// when the document is in it; otherwise (no artifact yet, or a document not indexed) `last_updated`
/// and `contributors` are computed on the fly from git, per design.md §17.3.
pub fn show(cfg: &Config, id: &str) -> Result<ShowResult> {
    if let Ok(conn) = open_newest(cfg)
        && let Some(resolved) = match resolve_id(&conn, id) {
            Ok(r) => Some(r),
            // an ambiguous prefix is a user error, not a reason to fall back to git
            Err(e) if e.to_string().contains("ambiguous") => return Err(e),
            Err(_) => None,
        }
        && let Some(row) = find_doc_row(&conn, &resolved)?
    {
        let contributors = responsible_and_authors(&conn, &row.id)?;
        let (updated_state, updated_at) = dirty_state(cfg, &row.path)?;
        return Ok(ShowResult {
            frontmatter: serde_json::from_str(&row.frontmatter).unwrap_or(serde_json::Value::Null),
            body: row.body,
            last_updated: row.last_updated,
            contributors,
            updated_state,
            updated_at,
        });
    }
    show_from_git(cfg, id)
}

fn responsible_and_authors(conn: &Connection, id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("select actor, role from contributors where doc_id = ?1 order by role, actor")?;
    Ok(stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

fn show_from_git(cfg: &Config, id: &str) -> Result<ShowResult> {
    let root = &cfg.root;
    let paths = git::ls_tree(root, "HEAD", "docs")?;
    let path = paths
        .into_iter()
        .find(|p| Path::new(p).file_stem().and_then(|s| s.to_str()) == Some(id))
        .with_context(|| format!("no document with id `{id}`"))?;
    let bytes = git::show_file(root, "HEAD", &path)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (fm_text, body) = crate::parse::split_frontmatter(&text)?;
    let frontmatter = match fm_text {
        Some(t) => crate::parse::frontmatter::parse_frontmatter(&t)?,
        None => Default::default(),
    };
    let last_updated = git::last_commit_date(root, &path);

    let raw = git::log_docs_numstat(root, "HEAD")?;
    let commits = gitlog::parse_numstat_log(&raw);
    let mut contributors = Vec::new();
    let mut seen = HashSet::new();
    for c in &commits {
        if c.files.iter().any(|f| f.path == path) && seen.insert(c.author_name.clone()) {
            contributors.push((c.author_name.clone(), "author".to_string()));
        }
    }
    let (updated_state, updated_at) = dirty_state(cfg, &path)?;
    Ok(ShowResult { frontmatter: serde_json::to_value(&frontmatter)?, body, last_updated, contributors, updated_state, updated_at })
}

fn dirty_state(cfg: &Config, path: &str) -> Result<(String, Option<String>)> {
    let dirty = !git::status_porcelain(&cfg.root, path)?.is_empty();
    if dirty {
        let mtime = std::fs::metadata(cfg.root.join(path)).ok().and_then(|m| m.modified().ok());
        let iso = mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| chrono::DateTime::<chrono::Utc>::from_timestamp(d.as_secs() as i64, 0))
            .map(|d| d.to_rfc3339());
        Ok(("uncommitted".to_string(), iso))
    } else {
        Ok(("committed".to_string(), git::last_commit_date(&cfg.root, path)))
    }
}
