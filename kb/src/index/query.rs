//! read-side queries over one artifact: search, get, refs, ls, blame, show. design.md §17.3, §18.
use crate::config::Config;
use crate::git;
use crate::index::build::{RefRow, open_newest};
use crate::index::gitlog;
use crate::index::vectors;
use crate::model;
use crate::provider::Provider;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
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

/// `fts` is bm25-only (the original, and still the default when no vector index exists);
/// `hybrid` fuses fts and `vec0` results with reciprocal rank fusion; `vec` is vector-only.
/// design.md §18.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
	Fts,
	Hybrid,
	Vec,
}

#[derive(Debug, Clone)]
pub struct SearchOpts {
	pub limit: usize,
	pub status_any: bool,
	pub include_captures: bool,
	/// `None` resolves to `Hybrid` when a query embedding is given and `chunk_vectors` exists in
	/// this artifact, else `Fts` -- design.md §18's "default hybrid when vectors exist else fts".
	pub mode: Option<SearchMode>,
	/// attach per-hit fts rank, vec rank, and fused score to each result.
	pub explain: bool,
}

impl Default for SearchOpts {
	fn default() -> Self {
		Self { limit: 5, status_any: false, include_captures: false, mode: None, explain: false }
	}
}

/// per-hit ranking detail, present on every hit when `SearchOpts.explain` is set.
#[derive(Debug, Clone, Serialize)]
pub struct SearchExplain {
	/// 1-indexed rank in the fts5 bm25 top-20, `None` when the document did not appear there.
	pub fts_rank: Option<usize>,
	/// 1-indexed rank in the `vec0` top-20 (after cutting the raw top-80), `None` when the
	/// document did not appear there or no vector search ran.
	pub vec_rank: Option<usize>,
	/// the reciprocal-rank-fusion score (`sum of 1/(k + rank)` over whichever of the two ranks
	/// are present, `k = 60`); for fts-only or vec-only search this is just that one list's own
	/// score, so the field is always populated once `explain` is requested.
	pub fused_score: f64,
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
	/// per-hit fts/vec rank and fused score; `Some` only when `SearchOpts.explain` was set.
	pub explain: Option<SearchExplain>,
}

/// one document's best-scoring chunk within one ranked list (fts or vec), before fusion.
#[derive(Debug, Clone)]
struct RankedCandidate {
	doc_id: String,
	heading_path: String,
	snippet: String,
	score: f64,
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
	let rows =
		stmt.query_map(params![match_expr, pool_limit], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, String>(3)?)))?;
	Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// fts top-n per design.md §18 ("fts5 bm25 top 20").
const FTS_TOP_N: usize = 20;
/// vec0 raw top-n before cutting to `VEC_CUT_N` (design.md §18: "`vec0` top 80 ... cut to 20").
const VEC_RAW_N: usize = 80;
const VEC_CUT_N: usize = 20;
/// reciprocal rank fusion constant (design.md §18: "reciprocal rank fusion with k of 60").
const RRF_K: f64 = 60.0;

/// fts5 bm25 search over the candidate chunk set, collapsed to the best-scoring chunk per
/// document, sorted descending, truncated to `top_n`. candidates are `kind = 'document'` (plus
/// `'capture'` when `include_captures`) and `status in (canon, draft)` unless `status_any` widens
/// it to every status (design.md §18).
fn fts_candidates(conn: &Connection, query: &str, opts: &SearchOpts, top_n: usize) -> Result<Vec<RankedCandidate>> {
	let kind_clause = if opts.include_captures { "c.kind in ('document','capture')" } else { "c.kind = 'document'" };
	let status_clause = if opts.status_any { "1 = 1" } else { "c.status in ('canon','draft')" };
	let sql = format!(
		"select c.doc_id, c.heading_path, bm25(chunks_fts) as score, snippet(chunks_fts, 1, '', '', ' … ', 16) as snip
         from chunks_fts join chunks c on c.chunk_id = chunks_fts.rowid
         where chunks_fts match ?1 and {kind_clause} and {status_clause}
         order by score
         limit ?2"
	);
	// fetch a wider pool than top_n so collapsing to one chunk per document still leaves top_n
	// distinct documents.
	let pool_limit = (top_n.max(1) * 20) as i64;

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
	let mut best: HashMap<String, RankedCandidate> = HashMap::new();
	for (doc_id, heading_path, bm25, snip) in rows {
		let score = -bm25;
		best.entry(doc_id.clone())
			.and_modify(|slot| {
				if score > slot.score {
					slot.heading_path = heading_path.clone();
					slot.snippet = snip.clone();
					slot.score = score;
				}
			})
			.or_insert(RankedCandidate { doc_id, heading_path, snippet: snip, score });
	}
	let mut ranked: Vec<RankedCandidate> = best.into_values().collect();
	ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
	ranked.truncate(top_n);
	Ok(ranked)
}

/// `chunk_vectors` knn search, collapsed to the best-scoring chunk per document, sorted
/// descending, cut to `VEC_CUT_N` after the raw `VEC_RAW_N` query (design.md §18). `chunk_vectors`
/// only ever holds `kind = 'document'`, `status in (canon, draft)` chunks (design.md §17.1), so no
/// extra filtering is needed here -- this is also why `status_any` has no vector-search effect
/// (design.md §18: "status=any ... fts only, since they leave the vector table").
fn vec_candidates(conn: &Connection, query_vec: &[f32]) -> Result<Vec<RankedCandidate>> {
	let hits = vectors::nearest(conn, query_vec, VEC_RAW_N, "")?;
	let mut best: HashMap<String, RankedCandidate> = HashMap::new();
	for (chunk_id, score) in hits {
		let row: Option<(String, String, String)> = conn
			.query_row("select doc_id, heading_path, text from chunks where chunk_id = ?1", params![chunk_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
			.optional()?;
		let Some((doc_id, heading_path, text)) = row else { continue };
		let snippet: String = if text.chars().count() > 300 { text.chars().take(300).collect::<String>() + "…" } else { text };
		best.entry(doc_id.clone())
			.and_modify(|slot| {
				if score > slot.score {
					slot.heading_path = heading_path.clone();
					slot.snippet = snippet.clone();
					slot.score = score;
				}
			})
			.or_insert(RankedCandidate { doc_id, heading_path, snippet, score });
	}
	let mut ranked: Vec<RankedCandidate> = best.into_values().collect();
	ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
	ranked.truncate(VEC_CUT_N);
	Ok(ranked)
}

/// one document's fused ranking: its rank (1-indexed) in each list it appeared in, its fused rrf
/// score, and the display fields (heading path, snippet) taken from whichever list ranked it
/// higher -- an fts hit's bm25 snippet is more informative than a raw text excerpt, so fts wins
/// ties.
struct Fused {
	heading_path: String,
	snippet: String,
	fts_rank: Option<usize>,
	vec_rank: Option<usize>,
	fused_score: f64,
}

fn rrf_fuse(fts: &[RankedCandidate], vec: &[RankedCandidate]) -> Vec<(String, Fused)> {
	let mut fused: HashMap<String, Fused> = HashMap::new();
	for (i, c) in fts.iter().enumerate() {
		let rank = i + 1;
		let entry = fused.entry(c.doc_id.clone()).or_insert(Fused {
			heading_path: c.heading_path.clone(),
			snippet: c.snippet.clone(),
			fts_rank: None,
			vec_rank: None,
			fused_score: 0.0,
		});
		entry.fts_rank = Some(rank);
		entry.fused_score += 1.0 / (RRF_K + rank as f64);
	}
	for (i, c) in vec.iter().enumerate() {
		let rank = i + 1;
		let entry = fused.entry(c.doc_id.clone()).or_insert(Fused {
			heading_path: c.heading_path.clone(),
			snippet: c.snippet.clone(),
			fts_rank: None,
			vec_rank: None,
			fused_score: 0.0,
		});
		entry.vec_rank = Some(rank);
		entry.fused_score += 1.0 / (RRF_K + rank as f64);
	}
	let mut out: Vec<(String, Fused)> = fused.into_iter().collect();
	out.sort_by(|a, b| b.1.fused_score.partial_cmp(&a.1.fused_score).unwrap_or(std::cmp::Ordering::Equal));
	out
}

/// hybrid search over one artifact (design.md §18): fts5 bm25, `vec0` nearest-neighbor when a
/// query embedding and a vector index are both available, reciprocal rank fusion, an optional
/// cross-encoder rerank when the given provider supports it (none do yet -- design.md §32), and
/// per-document collapse throughout. `query_vec` is the caller's own embedding of `query` (this
/// function never calls a provider itself, so search stays usable with no embed role configured);
/// pass `None` to force fts-only regardless of `opts.mode`. `rerank` is the provider configured
/// for the `rerank` role, if any; passing a provider that does not support it is a no-op, not an
/// error (`Provider::supports_rerank`).
pub fn search(conn: &Connection, query: &str, opts: &SearchOpts, query_vec: Option<&[f32]>, rerank: Option<&dyn Provider>) -> Result<Vec<SearchHit>> {
	let vectors_exist = vectors::vector_table_exists(conn);
	let mode = opts.mode.unwrap_or(if vectors_exist && query_vec.is_some() { SearchMode::Hybrid } else { SearchMode::Fts });
	// a mode that needs vectors but has none available (no query embedding, or no vector index in
	// this artifact) degrades to fts rather than failing outright.
	let effective_mode = if matches!(mode, SearchMode::Hybrid | SearchMode::Vec) && (!vectors_exist || query_vec.is_none()) { SearchMode::Fts } else { mode };

	let (mut doc_ids_in_order, fused_by_doc): (Vec<String>, HashMap<String, Fused>) = match effective_mode {
		SearchMode::Fts => {
			let ranked = fts_candidates(conn, query, opts, opts.limit.max(FTS_TOP_N))?;
			let ids = ranked.iter().map(|c| c.doc_id.clone()).collect();
			let map = ranked
				.into_iter()
				.enumerate()
				.map(|(i, c)| {
					(c.doc_id, Fused { heading_path: c.heading_path, snippet: c.snippet, fts_rank: Some(i + 1), vec_rank: None, fused_score: c.score })
				})
				.collect();
			(ids, map)
		}
		SearchMode::Vec => {
			let ranked = vec_candidates(conn, query_vec.unwrap())?;
			let ids = ranked.iter().map(|c| c.doc_id.clone()).collect();
			let map = ranked
				.into_iter()
				.enumerate()
				.map(|(i, c)| {
					(c.doc_id, Fused { heading_path: c.heading_path, snippet: c.snippet, fts_rank: None, vec_rank: Some(i + 1), fused_score: c.score })
				})
				.collect();
			(ids, map)
		}
		SearchMode::Hybrid => {
			let fts = fts_candidates(conn, query, opts, FTS_TOP_N)?;
			let vec = vec_candidates(conn, query_vec.unwrap())?;
			let fused = rrf_fuse(&fts, &vec);
			let ids = fused.iter().map(|(id, _)| id.clone()).collect();
			(ids, fused.into_iter().collect())
		}
	};
	doc_ids_in_order.truncate(opts.limit.max(1) * 4); // headroom for a rerank step, cut again below

	let mut hits = Vec::with_capacity(doc_ids_in_order.len());
	for doc_id in &doc_ids_in_order {
		let Some(fused) = fused_by_doc.get(doc_id) else { continue };
		let summary: Option<DocSummary> = conn
			.query_row("select title, layer, status, kind, path, criticality_pct, last_updated from documents where id = ?1", params![doc_id], |r| {
				Ok(DocSummary {
					title: r.get(0)?,
					layer: r.get(1)?,
					status: r.get(2)?,
					kind: r.get(3)?,
					path: r.get(4)?,
					criticality_pct: r.get(5)?,
					last_updated: r.get(6)?,
				})
			})
			.optional()?;
		let Some(DocSummary { title, layer, status, kind, path, criticality_pct, last_updated }) = summary else { continue };
		let source_uri = if kind == "capture" { Some(path) } else { None };
		let all_refs = refs_out(conn, doc_id)?;
		let source_refs = all_refs.iter().filter(|r| r.rel == "source").cloned().collect();
		let code_refs = all_refs.iter().filter(|r| r.rel == "code").cloned().collect();
		let asset_refs = all_refs.iter().filter(|r| r.rel == "asset").cloned().collect();
		let responsible = responsible_for_doc(conn, doc_id)?;
		let explain = opts.explain.then_some(SearchExplain { fts_rank: fused.fts_rank, vec_rank: fused.vec_rank, fused_score: fused.fused_score });
		hits.push(SearchHit {
			doc_id: doc_id.clone(),
			title,
			layer,
			status,
			kind,
			source_uri,
			criticality_pct,
			heading_path: fused.heading_path.clone(),
			snippet: fused.snippet.clone(),
			score: fused.fused_score,
			last_updated,
			responsible,
			source_refs,
			code_refs,
			asset_refs,
			explain,
		});
	}

	// cross-encoder rerank of the top of the pool when the configured provider supports it
	// (design.md §18); no adapter implements it yet (`supports_rerank` always false today), so
	// this is dead code in practice until one does -- see design.md §32.
	if let Some(provider) = rerank
		&& provider.supports_rerank()
		&& !hits.is_empty()
	{
		let texts: Vec<String> = hits.iter().map(|h| h.snippet.clone()).collect();
		if let Ok(scores) = provider.rerank(query, &texts) {
			let mut scored: Vec<(f64, SearchHit)> = hits.into_iter().zip(scores).map(|(h, s)| (s, h)).collect();
			scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
			hits = scored.into_iter().map(|(_, h)| h).collect();
		}
	}

	hits.truncate(opts.limit);
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
			.filter(|(idx, _)| {
				let line_no = idx + 1;
				line_no >= start_line && line_no <= end_line
			})
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
		} && let Some(row) = find_doc_row(&conn, &resolved)?
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
	let path =
		paths.into_iter().find(|p| Path::new(p).file_stem().and_then(|s| s.to_str()) == Some(id)).with_context(|| format!("no document with id `{id}`"))?;
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

#[cfg(test)]
mod hybrid_tests {
	use super::*;

	fn cand(doc_id: &str, score: f64) -> RankedCandidate {
		RankedCandidate { doc_id: doc_id.to_string(), heading_path: "_body".to_string(), snippet: "snip".to_string(), score }
	}

	#[test]
	fn rrf_favors_a_document_ranked_high_in_both_lists() {
		// "a" is #1 fts, #2 vec; "b" is #2 fts, #1 vec; "c" is fts-only, rank 3.
		let fts = vec![cand("a", 10.0), cand("b", 8.0), cand("c", 6.0)];
		let vec = vec![cand("b", 0.9), cand("a", 0.8)];
		let fused = rrf_fuse(&fts, &vec);
		let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
		// a: 1/(60+1) + 1/(60+2) = 0.032514; b: 1/(60+2) + 1/(60+1) = same total (order differs
		// only in which list ranked which #1), so a and b tie exactly and beat c.
		assert!(ids[0] == "a" || ids[0] == "b");
		assert!(ids[1] == "a" || ids[1] == "b");
		assert_eq!(ids[2], "c", "fts-only rank-3 document should fuse to last");

		let a = &fused.iter().find(|(id, _)| id == "a").unwrap().1;
		let b = &fused.iter().find(|(id, _)| id == "b").unwrap().1;
		assert!((a.fused_score - b.fused_score).abs() < 1e-12, "a and b should tie: {} vs {}", a.fused_score, b.fused_score);
		let c = &fused.iter().find(|(id, _)| id == "c").unwrap().1;
		assert!(a.fused_score > c.fused_score);
		assert_eq!(c.vec_rank, None);
		assert_eq!(c.fts_rank, Some(3));
	}

	#[test]
	fn rrf_matches_the_k_60_formula_exactly() {
		let fts = vec![cand("only-fts", 1.0)];
		let vec = vec![cand("only-vec", 1.0)];
		let fused = rrf_fuse(&fts, &vec);
		for (id, f) in &fused {
			let expected = 1.0 / (RRF_K + 1.0);
			assert!((f.fused_score - expected).abs() < 1e-12, "{id}: {} vs {expected}", f.fused_score);
		}
	}

	#[test]
	fn rrf_of_empty_lists_is_empty() {
		let fused = rrf_fuse(&[], &[]);
		assert!(fused.is_empty());
	}

	#[test]
	fn search_mode_defaults_to_fts_without_a_query_vector() {
		let conn = Connection::open_in_memory().unwrap();
		crate::index::schema::create_schema(&conn).unwrap();
		conn.execute("insert into documents (id, kind, path, title, status) values ('d1', 'document', 'docs/d1.md', 'hello world', 'canon')", []).unwrap();
		conn.execute(
			"insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h1','d1','document','canon','_body',0,'hello world')",
			[],
		)
		.unwrap();
		let opts = SearchOpts::default();
		let hits = search(&conn, "hello", &opts, None, None).unwrap();
		assert_eq!(hits.len(), 1);
		assert_eq!(hits[0].doc_id, "d1");
	}
}
