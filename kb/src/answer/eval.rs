//! retrieval evaluation: mrr@10 and recall@5 over `eval/queries.toml`, and the design.md §31
//! enable rule ("at least 30 queries, +0.02 mrr, recall not down by more than 0.02").

use crate::config::Config;
use crate::index::{self, SearchMode, SearchOpts};
use crate::provider::{self, Provider};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;

/// one row of `eval/queries.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryRow {
	pub text: String,
	/// `kb:<ulid>` (or a bare ulid) naming a document that answers the query; more than one
	/// counts as correct if any is retrieved.
	pub expected: Vec<String>,
	#[serde(default)]
	pub area: Option<String>,
	/// set by `kb eval seed`: a bootstrap row (a document's own title as its query), weaker
	/// signal than a hand-written one but still useful to get an eval set past the 30-query
	/// floor (design.md §31).
	#[serde(default)]
	pub seeded: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct QueriesFile {
	#[serde(default)]
	query: Vec<QueryRow>,
}

/// load `eval/queries.toml` (or another path); a missing file is an empty set, not an error, so
/// a fresh repo's `kb eval` has something sane to report.
pub fn load_queries(path: &Path) -> Result<Vec<QueryRow>> {
	if !path.is_file() {
		return Ok(Vec::new());
	}
	let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
	let parsed: QueriesFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
	Ok(parsed.query)
}

/// a debug-escaped toml basic string: good enough for the plain titles and questions this file
/// holds, without pulling in a toml serializer just to write one array of small tables.
fn toml_string(s: &str) -> String {
	format!("{s:?}")
}

/// write `queries` to `path` as `eval/queries.toml`, replacing its content entirely (callers that
/// want to keep existing rows pass `load_queries(path)?` back in, appended to).
pub fn write_queries(path: &Path, queries: &[QueryRow]) -> Result<()> {
	let mut out = String::from("# eval/queries.toml: retrieval evaluation set. see design.md §31.\n");
	for q in queries {
		out.push_str("\n[[query]]\n");
		out.push_str(&format!("text = {}\n", toml_string(&q.text)));
		let expected = q.expected.iter().map(|e| toml_string(e)).collect::<Vec<_>>().join(", ");
		out.push_str(&format!("expected = [{expected}]\n"));
		if let Some(area) = &q.area {
			out.push_str(&format!("area = {}\n", toml_string(area)));
		}
		out.push_str(&format!("seeded = {}\n", q.seeded));
	}
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent)?;
	}
	std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

/// `kb eval seed --from-search <n>`: a starter eval set from each indexed document's own title,
/// expecting that document back (design.md build step 5: "a weak but useful bootstrap"). `limit`
/// caps how many documents are seeded, newest-last by id (ulids sort chronologically); `None`
/// seeds every canon/draft document with a title.
pub fn seed_from_documents(cfg: &Config, limit: Option<usize>) -> Result<Vec<QueryRow>> {
	let conn = index::open_newest(cfg)?;
	let mut stmt = conn.prepare("select id, title from documents where kind = 'document' and status in ('canon','draft') and title is not null order by id")?;
	let mut rows: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
	if let Some(n) = limit {
		rows.truncate(n);
	}
	Ok(rows.into_iter().map(|(id, title)| QueryRow { text: title, expected: vec![format!("kb:{id}")], area: None, seeded: true }).collect())
}

/// the retrieval configuration compared by `kb eval` (design.md §31's `contextualize` gate
/// compares "with prefixes vs without", which for a built index reduces to a search mode/feature
/// choice: `fts` (bm25 alone), `hybrid` (fts + vec0 + rrf), `hybrid+rerank` (hybrid plus a
/// cross-encoder rerank step, when a `rerank` provider supports it -- design.md §32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
	Fts,
	Hybrid,
	HybridRerank,
}

impl FromStr for Feature {
	type Err = String;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"fts" => Ok(Feature::Fts),
			"hybrid" => Ok(Feature::Hybrid),
			"hybrid+rerank" => Ok(Feature::HybridRerank),
			other => Err(format!("unknown --features value `{other}` (expected fts, hybrid, or hybrid+rerank)")),
		}
	}
}

impl Feature {
	pub fn as_str(self) -> &'static str {
		match self {
			Feature::Fts => "fts",
			Feature::Hybrid => "hybrid",
			Feature::HybridRerank => "hybrid+rerank",
		}
	}
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EvalMetrics {
	pub mrr_at_10: f64,
	pub recall_at_5: f64,
	pub n: usize,
}

/// a `kb:<ulid>` or bare ulid from `expected` reduced to the plain id `SearchHit.doc_id` uses.
fn expected_id(raw: &str) -> String {
	raw.strip_prefix("kb:").unwrap_or(raw).to_string()
}

/// mrr@10 and recall@5 for `feature` over `queries`, against the newest built index. a query's
/// reciprocal rank is `1 / rank` of the first hit whose `doc_id` is in `expected`, `0` when none
/// of the top 10 hits match; recall@5 counts a query as a hit when any of the top 5 matches.
pub fn run_eval(cfg: &Config, feature: Feature, queries: &[QueryRow]) -> Result<EvalMetrics> {
	let conn = index::open_newest(cfg)?;
	let mode = match feature {
		Feature::Fts => Some(SearchMode::Fts),
		Feature::Hybrid | Feature::HybridRerank => Some(SearchMode::Hybrid),
	};
	let embed_provider: Option<Box<dyn Provider>> =
		if matches!(feature, Feature::Hybrid | Feature::HybridRerank) { provider::provider_for_role(cfg, "embed")? } else { None };
	let rerank_provider: Option<Box<dyn Provider>> = if matches!(feature, Feature::HybridRerank) { provider::provider_for_role(cfg, "rerank")? } else { None };

	let mut rr_sum = 0.0f64;
	let mut recall_hits = 0usize;
	for q in queries {
		let expected: Vec<String> = q.expected.iter().map(|e| expected_id(e)).collect();
		let query_vec = match &embed_provider {
			Some(p) if p.supports_embed() => p.embed(std::slice::from_ref(&q.text)).ok().and_then(|mut v| v.pop()),
			_ => None,
		};
		let opts = SearchOpts { limit: 10, status_any: false, include_captures: false, mode, explain: false };
		let hits = index::search(&conn, &q.text, &opts, query_vec.as_deref(), rerank_provider.as_deref())?;
		if let Some(rank) = hits.iter().position(|h| expected.contains(&h.doc_id)).map(|i| i + 1) {
			rr_sum += 1.0 / rank as f64;
		}
		if hits.iter().take(5).any(|h| expected.contains(&h.doc_id)) {
			recall_hits += 1;
		}
	}
	let n = queries.len();
	Ok(EvalMetrics { mrr_at_10: if n > 0 { rr_sum / n as f64 } else { 0.0 }, recall_at_5: if n > 0 { recall_hits as f64 / n as f64 } else { 0.0 }, n })
}

/// design.md §31: "a retrieval feature is enabled in `manifest.toml` only when the eval set has
/// at least 30 queries and the feature meets the `contextualize` rule against the currently
/// enabled set" -- that rule being "up by 0.02 absolute [mrr], recall at 5 not down by more than
/// 0.02" (§31's `contextualize` row). `n_queries` is the eval set size actually used for both
/// runs (the same set for baseline and candidate).
pub struct EnableGate {
	pub passes: bool,
	pub reasons: Vec<String>,
}

pub fn check_enable_gate(n_queries: usize, baseline: &EvalMetrics, candidate: &EvalMetrics) -> EnableGate {
	let mut reasons = Vec::new();
	if n_queries < 30 {
		reasons.push(format!("eval set has {n_queries} queries; the enable rule needs at least 30"));
	}
	let mrr_delta = candidate.mrr_at_10 - baseline.mrr_at_10;
	if mrr_delta < 0.02 {
		reasons.push(format!("mrr@10 changed by {mrr_delta:+.4}; the enable rule needs at least +0.02"));
	}
	let recall_delta = candidate.recall_at_5 - baseline.recall_at_5;
	if recall_delta < -0.02 {
		reasons.push(format!("recall@5 dropped by {:.4}, more than the 0.02 allowance", -recall_delta));
	}
	EnableGate { passes: reasons.is_empty(), reasons }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_a_queries_toml() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("queries.toml");
		std::fs::write(
			&path,
			r#"
[[query]]
text = "how do we set a static ip"
expected = ["kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N"]
area = "ops"

[[query]]
text = "install steps"
expected = ["kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P"]
seeded = true
"#,
		)
		.unwrap();
		let rows = load_queries(&path).unwrap();
		assert_eq!(rows.len(), 2);
		assert_eq!(rows[0].area.as_deref(), Some("ops"));
		assert!(!rows[0].seeded);
		assert!(rows[1].seeded);
	}

	#[test]
	fn missing_file_is_an_empty_set() {
		let dir = tempfile::tempdir().unwrap();
		let rows = load_queries(&dir.path().join("nope.toml")).unwrap();
		assert!(rows.is_empty());
	}

	#[test]
	fn write_then_load_round_trips() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("eval").join("queries.toml");
		let rows = vec![
			QueryRow {
				text: "a \"quoted\" question".to_string(),
				expected: vec!["kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N".to_string()],
				area: Some("ops".to_string()),
				seeded: false,
			},
			QueryRow { text: "seeded question".to_string(), expected: vec!["kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P".to_string()], area: None, seeded: true },
		];
		write_queries(&path, &rows).unwrap();
		let loaded = load_queries(&path).unwrap();
		assert_eq!(loaded.len(), 2);
		assert_eq!(loaded[0].text, "a \"quoted\" question");
		assert!(loaded[1].seeded);
	}

	#[test]
	fn feature_from_str() {
		assert_eq!(Feature::from_str("fts").unwrap(), Feature::Fts);
		assert_eq!(Feature::from_str("hybrid").unwrap(), Feature::Hybrid);
		assert_eq!(Feature::from_str("hybrid+rerank").unwrap(), Feature::HybridRerank);
		assert!(Feature::from_str("nonsense").is_err());
	}

	#[test]
	fn enable_gate_requires_30_queries_and_the_mrr_and_recall_deltas() {
		let baseline = EvalMetrics { mrr_at_10: 0.5, recall_at_5: 0.6, n: 40 };

		let too_few = check_enable_gate(20, &baseline, &EvalMetrics { mrr_at_10: 0.6, recall_at_5: 0.6, n: 20 });
		assert!(!too_few.passes);
		assert!(too_few.reasons.iter().any(|r| r.contains("30")));

		let not_enough_mrr = check_enable_gate(40, &baseline, &EvalMetrics { mrr_at_10: 0.51, recall_at_5: 0.6, n: 40 });
		assert!(!not_enough_mrr.passes);

		let recall_regressed = check_enable_gate(40, &baseline, &EvalMetrics { mrr_at_10: 0.55, recall_at_5: 0.5, n: 40 });
		assert!(!recall_regressed.passes, "recall dropped by 0.10, more than the 0.02 allowance");

		let passes = check_enable_gate(40, &baseline, &EvalMetrics { mrr_at_10: 0.53, recall_at_5: 0.59, n: 40 });
		assert!(passes.passes, "{:?}", passes.reasons);
	}
}
