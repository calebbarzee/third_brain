//! chunk prefixes: a one-sentence statement of where a chunk sits in its document, prepended to
//! the chunk text before embedding and stored in `chunks.prefix` (a separate fts5 column already,
//! per schema.rs). design.md §17.1 ("prefixes, embeddings | cached by chunk key and model"),
//! §19's `contextualize` role, §31 (the eval gate this feature must clear before it can be
//! enabled by default).

use crate::config::Config;
use crate::provider::{CompletionRequest, Message, Provider, Role, first_text};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");
const DEFAULT_CONTEXTUALIZE: &str = include_str!("../../prompts/contextualize.md");

/// the cache key used when no `contextualize` provider is configured: the deterministic prefix
/// never changes for a given (title, heading_path), so it is cheap to cache anyway, and a stable
/// key keeps `prefix_cache` rows meaningful (design.md: "cached by chunk key and model").
pub const DETERMINISTIC_MODEL_KEY: &str = "deterministic";

/// summary of one `build_prefixes` call.
#[derive(Debug, Clone, Default)]
pub struct PrefixReport {
    pub model: String,
    /// prefixes served from `prefix_cache` (this build or a previous one, via
    /// `vectors::seed_caches_from_previous`).
    pub cached: usize,
    /// prefixes computed fresh in this call: by a provider, or deterministically when none is
    /// configured.
    pub generated: usize,
    pub provider_used: bool,
}

/// the deterministic fallback prefix: `"<title> > <heading path>"`. always available, so fts and
/// vectors always have a prefix even with no `contextualize` provider configured (design.md §19:
/// "with no provider configured a hook returns a deferred prompt" -- but a prefix is cheap enough
/// that this hook degrades instead of deferring).
pub fn deterministic_prefix(title: &str, heading_path: &str) -> String {
    format!("{title} > {heading_path}")
}

fn load_prompt(root: &std::path::Path, name: &str, default: &'static str) -> Result<String> {
    let path = root.join("prompts").join(name);
    if path.is_file() {
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    } else {
        Ok(default.to_string())
    }
}

/// one line of model output: the first non-empty line, trimmed of surrounding quotes and
/// whitespace. a model that ignores "output only that one sentence" and adds a preamble or a
/// trailing note still yields a usable prefix this way.
fn first_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").trim_matches('"').to_string()
}

/// ask the `contextualize` provider for one sentence naming `title` and `heading_path`; falls
/// back to the deterministic prefix on any provider error or empty reply, since a chunk must
/// always end up with some prefix.
fn generate_with_provider(cfg: &Config, provider: &dyn Provider, title: &str, heading_path: &str, text: &str) -> String {
    let fallback = deterministic_prefix(title, heading_path);
    let style = load_prompt(&cfg.root, "style.md", DEFAULT_STYLE).unwrap_or_default();
    let prompt = load_prompt(&cfg.root, "contextualize.md", DEFAULT_CONTEXTUALIZE).unwrap_or_default();
    let system = format!("{style}\n\n{prompt}");
    // truncate the passage itself: the prefix only needs to name where the passage sits, not
    // read all of it, and a very long chunk should not blow the prompt budget.
    let excerpt: String = text.chars().take(600).collect();
    let user = format!("document title: {title}\nsection (slug path): {heading_path}\n\npassage:\n{excerpt}");
    let req = CompletionRequest {
        system: Some(system),
        messages: vec![Message { role: Role::User, content: user }],
        max_tokens: 80,
        temperature: Some(0.0),
        json_schema: None,
    };
    match provider.complete(&req) {
        Ok(resp) => {
            let line = first_text(&resp).map(first_line).unwrap_or_default();
            if line.is_empty() { fallback } else { line }
        }
        Err(_) => fallback,
    }
}

fn get_cached_prefix(conn: &Connection, chunk_hash: &str, model: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("select prefix from prefix_cache where chunk_hash = ?1 and prefix_model = ?2", params![chunk_hash, model], |r| r.get(0))
        .optional()?)
}

fn put_cached_prefix(conn: &Connection, chunk_hash: &str, model: &str, prefix: &str) -> Result<()> {
    conn.execute("insert or replace into prefix_cache (chunk_hash, prefix_model, prefix) values (?1, ?2, ?3)", params![chunk_hash, model, prefix])?;
    Ok(())
}

/// fill every chunk's `prefix` column (an `update`, which schema.rs's `chunks_au` trigger keeps
/// `chunks_fts` in sync with), using `prefix_cache` first and computing (via `provider`, or
/// deterministically when `provider` is `None`) on a miss. runs over every row in `chunks`
/// regardless of `kind`/`status`: design.md's vector-table restriction to canon/draft document
/// chunks is `vectors`' concern, not this one's -- fts benefits from a prefix on every row.
pub fn build_prefixes(conn: &Connection, cfg: &Config, provider: Option<&dyn Provider>) -> Result<PrefixReport> {
    let model = match provider {
        Some(_) => cfg.kb.providers.get("contextualize").map(|p| p.model.clone()).unwrap_or_else(|| DETERMINISTIC_MODEL_KEY.to_string()),
        None => DETERMINISTIC_MODEL_KEY.to_string(),
    };

    let mut title_stmt = conn.prepare("select title from documents where id = ?1")?;
    let mut select_stmt = conn.prepare("select chunk_id, chunk_hash, doc_id, heading_path, text from chunks order by chunk_id")?;
    let rows: Vec<(i64, String, String, String, String)> = select_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut report = PrefixReport { model: model.clone(), provider_used: provider.is_some(), ..Default::default() };
    for (chunk_id, chunk_hash, doc_id, heading_path, text) in rows {
        let prefix = match get_cached_prefix(conn, &chunk_hash, &model)? {
            Some(p) => {
                report.cached += 1;
                p
            }
            None => {
                let title: Option<String> = title_stmt.query_row(params![doc_id], |r| r.get(0)).optional()?.flatten();
                let title = title.unwrap_or_else(|| doc_id.clone());
                let generated = match provider {
                    Some(p) => generate_with_provider(cfg, p, &title, &heading_path, &text),
                    None => deterministic_prefix(&title, &heading_path),
                };
                put_cached_prefix(conn, &chunk_hash, &model, &generated)?;
                report.generated += 1;
                generated
            }
        };
        conn.execute("update chunks set prefix = ?1 where chunk_id = ?2", params![prefix, chunk_id])?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_prefix_is_stable_and_readable() {
        assert_eq!(deterministic_prefix("install guide", "install/linux"), "install guide > install/linux");
        // same inputs, same output, every time -- no randomness, no model call.
        assert_eq!(deterministic_prefix("install guide", "install/linux"), deterministic_prefix("install guide", "install/linux"));
    }

    #[test]
    fn first_line_takes_first_nonempty_trimmed_line() {
        assert_eq!(first_line("\n  from the install guide, section linux.  \n\nextra notes"), "from the install guide, section linux.");
        assert_eq!(first_line("\"quoted sentence.\""), "quoted sentence.");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn build_prefixes_without_provider_is_deterministic_and_cached() {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::create_schema(&conn).unwrap();
        conn.execute("insert into documents (id, kind, path, title) values ('d1', 'document', 'docs/d1.md', 'my title')", []).unwrap();
        conn.execute(
            "insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text) values ('h1','d1','document','canon','intro',0,'hello')",
            [],
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();

        let report = build_prefixes(&conn, &cfg, None).unwrap();
        assert_eq!(report.generated, 1);
        assert_eq!(report.cached, 0);
        assert!(!report.provider_used);
        let prefix: String = conn.query_row("select prefix from chunks where chunk_hash = 'h1'", [], |r| r.get(0)).unwrap();
        assert_eq!(prefix, "my title > intro");

        // a second pass over the same (already-prefixed) row should hit the cache, not recompute.
        conn.execute("update chunks set prefix = null where chunk_hash = 'h1'", []).unwrap();
        let report2 = build_prefixes(&conn, &cfg, None).unwrap();
        assert_eq!(report2.cached, 1);
        assert_eq!(report2.generated, 0);
    }
}
