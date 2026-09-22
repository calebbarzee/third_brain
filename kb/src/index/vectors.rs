//! the vector side of the index: `chunk_vectors` (a `sqlite-vec` `vec0` table), the
//! `embedding_cache` (keyed by chunk hash and embedding model, design.md §17.1), and nearest-
//! neighbor search. design.md §17.2, §18, appendix a (`vec0`).
//!
//! `sqlite-vec` built cleanly on this machine (`cargo add sqlite-vec` pulled 0.1.9 and `cargo
//! build` linked it with no extra setup), so this module is the `vec0`-backed implementation
//! design.md calls for, not the plain-table fallback. `cosine_nearest` below is a pure, tested
//! utility kept for that fallback case (and for anything that wants exact cosine similarity over
//! an in-memory set without touching sqlite), but it is not on the hot path today.

use crate::config::Config;
use crate::provider::Provider;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Once;

/// batch size for embed requests (design.md build step 5: "batch requests in groups of 32").
const EMBED_BATCH_SIZE: usize = 32;

/// summary of one `build_vectors` call.
#[derive(Debug, Clone, Default)]
pub struct VectorReport {
    /// whether an embed provider was configured and used; `false` means `chunk_vectors` was not
    /// touched this build and hybrid/vec search has nothing to search (design.md §18: search
    /// falls back to fts-only in that case).
    pub enabled: bool,
    pub backend: &'static str,
    pub model: Option<String>,
    pub dim: usize,
    /// embeddings computed fresh this build (a request sent to the provider).
    pub embedded: usize,
    /// embeddings served from `embedding_cache` (this build or copied forward from a previous one).
    pub cached: usize,
    /// total rows written to `chunk_vectors`.
    pub chunk_count: usize,
    pub skipped_reason: Option<String>,
}

/// embed and index every eligible chunk: `kind = 'document'` and `status in ('canon', 'draft')`
/// (design.md §18's candidate set; §17.1: "only canon and draft document chunks enter the vector
/// table"). the embedded text is the chunk's prefix (when set) prepended to its own text. skips
/// entirely, with a reason, when `provider` is `None` or does not support `embed`. callers must
/// have called `register_extension()` before `conn`'s connection was opened, and should have
/// called `build_prefixes` first so a prefix is available to prepend.
pub fn build_vectors(conn: &Connection, cfg: &Config, provider: Option<&dyn Provider>) -> Result<VectorReport> {
    let backend = "sqlite-vec";
    let Some(provider) = provider else {
        return Ok(VectorReport { backend, skipped_reason: Some("no embed provider configured for role \"embed\"".to_string()), ..Default::default() });
    };
    if !provider.supports_embed() {
        return Ok(VectorReport {
            backend,
            skipped_reason: Some(format!("provider \"{}\" configured for role \"embed\" does not support embed", provider.name())),
            ..Default::default()
        });
    }
    let model = cfg.kb.providers.get("embed").map(|p| p.model.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| provider.name().to_string());

    let mut stmt =
        conn.prepare("select chunk_id, chunk_hash, coalesce(prefix, ''), text from chunks where kind = 'document' and status in ('canon', 'draft') order by chunk_id")?;
    let rows: Vec<(i64, String, String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut embeddings: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut cached = 0usize;
    let mut pending: Vec<(i64, String)> = Vec::new();
    for (chunk_id, chunk_hash, prefix, text) in &rows {
        if let Some(v) = get_cached_embedding(conn, chunk_hash, &model)? {
            embeddings.push((*chunk_id, v));
            cached += 1;
        } else {
            let embed_text = if prefix.is_empty() { text.clone() } else { format!("{prefix}\n\n{text}") };
            pending.push((*chunk_id, embed_text));
        }
    }

    let mut embedded = 0usize;
    for batch in pending.chunks(EMBED_BATCH_SIZE) {
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vecs = provider.embed(&texts).context("embed request failed")?;
        if vecs.len() != texts.len() {
            bail!("embed provider returned {} vectors for {} inputs", vecs.len(), texts.len());
        }
        for ((chunk_id, _), vec) in batch.iter().zip(vecs.into_iter()) {
            let chunk_hash = rows.iter().find(|(id, ..)| id == chunk_id).map(|(_, h, ..)| h.as_str()).unwrap_or_default();
            put_cached_embedding(conn, chunk_hash, &model, &vec)?;
            embeddings.push((*chunk_id, vec));
            embedded += 1;
        }
    }

    if embeddings.is_empty() {
        return Ok(VectorReport {
            enabled: true,
            backend,
            model: Some(model),
            skipped_reason: Some("no canon/draft document chunks to embed".to_string()),
            ..Default::default()
        });
    }

    let dim = embeddings[0].1.len();
    create_vector_table(conn, dim)?;
    for (chunk_id, vec) in &embeddings {
        upsert_vector(conn, *chunk_id, vec)?;
    }

    Ok(VectorReport { enabled: true, backend, model: Some(model), dim, embedded, cached, chunk_count: embeddings.len(), skipped_reason: None })
}

static VEC_INIT: Once = Once::new();

/// register the `sqlite-vec` extension with rusqlite's `sqlite3_auto_extension`, so every
/// connection opened afterward (including ones opened by other modules) gets `vec0` and
/// `vec_version()`. must run before the connection that will use `chunk_vectors` is opened;
/// `Once` makes repeat calls (across builds in one process, or in tests) harmless.
pub fn register_extension() {
    VEC_INIT.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())));
    });
}

/// pack a vector of `f32` into the little-endian byte layout `vec0` (and this module's own
/// cache blobs) expect: four bytes per component, in order.
pub fn pack(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for x in vec {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// the inverse of `pack`. a malformed length (not a multiple of 4) yields as many whole
/// components as fit, since a cache row this module wrote itself is never malformed in practice.
pub fn unpack(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// create `chunk_vectors` as a `vec0` virtual table with the given dimension, if it does not
/// already exist. the dimension comes from the first embedding computed in one build (design.md
/// §17.1), so this is only callable once that is known.
pub fn create_vector_table(conn: &Connection, dim: usize) -> Result<()> {
    conn.execute_batch(&format!(
        "create virtual table if not exists chunk_vectors using vec0(chunk_id integer primary key, embedding float[{dim}])"
    ))
    .context("creating chunk_vectors vec0 table")
}

/// insert or replace one chunk's embedding.
pub fn upsert_vector(conn: &Connection, chunk_id: i64, embedding: &[f32]) -> Result<()> {
    conn.execute("insert or replace into chunk_vectors (chunk_id, embedding) values (?1, ?2)", params![chunk_id, pack(embedding)])
        .context("inserting into chunk_vectors")?;
    Ok(())
}

/// whether `chunk_vectors` exists in this artifact (it is absent when no embed provider was
/// configured at build time; design.md §18 falls back to fts-only search in that case).
pub fn vector_table_exists(conn: &Connection) -> bool {
    conn.query_row("select 1 from sqlite_master where type = 'table' and name = 'chunk_vectors'", [], |r| r.get::<_, i64>(0))
        .optional()
        .unwrap_or(None)
        .is_some()
}

/// the `k` nearest chunks to `query_vec` by `vec0`'s own distance metric (l2 by default), each as
/// `(chunk_id, score)` with higher `score` meaning closer (the negated distance, so callers can
/// treat it like `SearchHit.score`'s "higher is better" convention alongside bm25). `extra_where`,
/// when non-empty, is anded into the query as a raw sql fragment (e.g. an additional `chunk_id in
/// (...)` restriction); callers are responsible for it containing no untrusted text, since it is
/// not parameterized. requires `chunk_vectors` to exist; call `vector_table_exists` first.
pub fn nearest(conn: &Connection, query_vec: &[f32], k: usize, extra_where: &str) -> Result<Vec<(i64, f64)>> {
    let extra = if extra_where.trim().is_empty() { String::new() } else { format!(" and {extra_where}") };
    let sql = format!("select chunk_id, distance from chunk_vectors where embedding match ?1 and k = ?2{extra} order by distance");
    let mut stmt = conn.prepare(&sql).context("preparing chunk_vectors knn query")?;
    let rows = stmt.query_map(params![pack(query_vec), k as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (chunk_id, distance) = row?;
        out.push((chunk_id, -distance));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// embedding cache
// ---------------------------------------------------------------------------------------------

/// the cached embedding for `(chunk_hash, model)`, if this or a previous build already computed
/// it (see `seed_caches_from_previous`, which copies a prior artifact's cache forward so a full
/// rebuild still only embeds new or changed chunks -- design.md §17.1).
pub fn get_cached_embedding(conn: &Connection, chunk_hash: &str, model: &str) -> Result<Option<Vec<f32>>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row("select embedding from embedding_cache where chunk_hash = ?1 and embedding_model = ?2", params![chunk_hash, model], |r| r.get(0))
        .optional()?;
    Ok(bytes.map(|b| unpack(&b)))
}

pub fn put_cached_embedding(conn: &Connection, chunk_hash: &str, model: &str, embedding: &[f32]) -> Result<()> {
    conn.execute(
        "insert or replace into embedding_cache (chunk_hash, embedding_model, embedding) values (?1, ?2, ?3)",
        params![chunk_hash, model, pack(embedding)],
    )?;
    Ok(())
}

/// copy `embedding_cache` and `prefix_cache` rows from the newest existing artifact (if any) into
/// `conn`, a freshly created artifact database. every build in this codebase re-parses and
/// re-chunks from scratch (design.md §17.1's incremental/base selection is not implemented yet),
/// so without this the two caches would reset on every build; copying them forward is what makes
/// "an incremental build embeds only new chunk hashes" true in practice: unchanged chunks keep
/// the same `chunk_hash` across builds and hit the cache. best-effort: a prior artifact from
/// before these tables existed, or any attach/copy failure, is silently skipped, not fatal.
pub fn seed_caches_from_previous(conn: &Connection, prev_path: Option<&Path>) {
    let Some(prev_path) = prev_path else { return };
    let Some(prev_str) = prev_path.to_str() else { return };
    let attach = conn.execute("attach database ?1 as prev_idx", params![prev_str]);
    if attach.is_err() {
        return;
    }
    let _ = conn.execute_batch(
        "insert or ignore into embedding_cache select * from prev_idx.embedding_cache;
         insert or ignore into prefix_cache select * from prev_idx.prefix_cache;",
    );
    let _ = conn.execute_batch("detach database prev_idx;");
}

// ---------------------------------------------------------------------------------------------
// cosine similarity (pure fallback / general-purpose utility; unit-tested directly)
// ---------------------------------------------------------------------------------------------

/// cosine similarity between two equal-length vectors; 0.0 for a zero-length vector on either
/// side (rather than dividing by zero).
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64) * (*y as f64)).sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// the `k` nearest of `vectors` to `query` by cosine similarity, descending. a pure, in-memory
/// fallback for when no vector-search extension is available: fine up to roughly 100k rows, per
/// design.md's guidance for this build step.
pub fn cosine_nearest(vectors: &[(i64, Vec<f32>)], query: &[f32], k: usize) -> Vec<(i64, f64)> {
    let mut scored: Vec<(i64, f64)> = vectors.iter().map(|(id, v)| (*id, cosine(v, query))).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CompletionRequest, CompletionResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// a provider whose `embed` returns a fixed 2-dim vector derived from each text's length,
    /// and counts how many times it was called (to check batching). `Provider: Send + Sync`
    /// rules out a plain `Cell`, hence the atomic.
    struct FakeEmbedProvider {
        calls: AtomicUsize,
    }
    impl Provider for FakeEmbedProvider {
        fn name(&self) -> &str {
            "fake"
        }
        fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse> {
            anyhow::bail!("not used in this test")
        }
        fn supports_embed(&self) -> bool {
            true
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
        }
    }

    fn cfg_with_embed_model(model: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kb.toml"), format!("[providers.embed]\nkind = \"ollama\"\nmodel = \"{model}\"\n")).unwrap();
        Config::load(dir.path()).unwrap()
    }

    fn conn_with_chunks() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::create_schema(&conn).unwrap();
        conn.execute("insert into documents (id, kind, path, title, status) values ('d1', 'document', 'docs/d1.md', 't', 'canon')", []).unwrap();
        conn.execute(
            "insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h1','d1','document','canon','a',0,'hello')",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h2','d1','document','canon','b',1,'goodbye world')",
            [],
        )
        .unwrap();
        // a capture chunk must never enter the vector table (design.md §17.1).
        conn.execute("insert into documents (id, kind, path, title, status) values ('c1', 'capture', 'file:c1', 'cap', 'draft')", []).unwrap();
        conn.execute(
            "insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h3','c1','capture','draft','_body',0,'captured text')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn build_vectors_without_provider_is_skipped() {
        register_extension();
        let conn = conn_with_chunks();
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let report = build_vectors(&conn, &cfg, None).unwrap();
        assert!(!report.enabled);
        assert!(report.skipped_reason.is_some());
        assert!(!vector_table_exists(&conn));
    }

    #[test]
    fn build_vectors_embeds_only_document_canon_draft_chunks_and_caches() {
        register_extension();
        let conn = conn_with_chunks();
        let cfg = cfg_with_embed_model("fake-model");
        let provider = FakeEmbedProvider { calls: AtomicUsize::new(0) };

        let report = build_vectors(&conn, &cfg, Some(&provider)).unwrap();
        assert!(report.enabled);
        assert_eq!(report.chunk_count, 2, "only the two document chunks, not the capture chunk");
        assert_eq!(report.embedded, 2);
        assert_eq!(report.cached, 0);
        assert_eq!(report.dim, 2);
        assert!(vector_table_exists(&conn));
        let hits = nearest(&conn, &[5.0, 1.0], 5, "").unwrap();
        assert_eq!(hits.len(), 2);

        // rebuild against the same (now-populated) embedding_cache: no new provider calls. this
        // stands in for `seed_caches_from_previous` (exercised separately in build.rs's own
        // integration test), copying rows by hand since both connections are in-memory here.
        let conn2 = conn_with_chunks();
        for (chunk_hash, model, embedding) in
            conn.prepare("select chunk_hash, embedding_model, embedding from embedding_cache").unwrap().query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?))
            }).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
        {
            conn2.execute("insert into embedding_cache (chunk_hash, embedding_model, embedding) values (?1,?2,?3)", params![chunk_hash, model, embedding]).unwrap();
        }
        let report2 = build_vectors(&conn2, &cfg, Some(&provider)).unwrap();
        assert_eq!(report2.cached, 2);
        assert_eq!(report2.embedded, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1, "the second build should hit the cache and never call embed again");
    }

    #[test]
    fn pack_unpack_round_trips() {
        let v = vec![0.5_f32, -1.25, 3.0, 0.0];
        assert_eq!(unpack(&pack(&v)), v);
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_nearest_ranks_closest_first_on_a_tiny_fixture() {
        // three tiny vectors: "a" and "b" point the same way, "c" is orthogonal to both.
        let vectors = vec![
            (1_i64, vec![1.0_f32, 0.0]),
            (2_i64, vec![0.9_f32, 0.1]),
            (3_i64, vec![0.0_f32, 1.0]),
        ];
        let query = vec![1.0_f32, 0.0];
        let top2 = cosine_nearest(&vectors, &query, 2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].0, 1, "the identical vector should rank first");
        assert_eq!(top2[1].0, 2, "the near-identical vector should rank second");
        assert!(top2[0].1 > top2[1].1);
    }

    #[test]
    fn vec0_table_round_trips_through_rusqlite() {
        register_extension();
        let conn = Connection::open_in_memory().unwrap();
        create_vector_table(&conn, 3).unwrap();
        upsert_vector(&conn, 1, &[1.0, 0.0, 0.0]).unwrap();
        upsert_vector(&conn, 2, &[0.0, 1.0, 0.0]).unwrap();
        upsert_vector(&conn, 3, &[0.9, 0.1, 0.0]).unwrap();
        let hits = nearest(&conn, &[1.0, 0.0, 0.0], 2, "").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1, "the exact match should rank first, got {hits:?}");
        assert_eq!(hits[1].0, 3, "the near match should rank second, got {hits:?}");
    }

    #[test]
    fn embedding_cache_round_trips() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table embedding_cache (chunk_hash text not null, embedding_model text not null, embedding blob not null, primary key (chunk_hash, embedding_model));",
        )
        .unwrap();
        assert!(get_cached_embedding(&conn, "h1", "m1").unwrap().is_none());
        put_cached_embedding(&conn, "h1", "m1", &[1.0, 2.0]).unwrap();
        assert_eq!(get_cached_embedding(&conn, "h1", "m1").unwrap().unwrap(), vec![1.0, 2.0]);
        assert!(get_cached_embedding(&conn, "h1", "m2").unwrap().is_none(), "cache is keyed by model too");
    }
}
