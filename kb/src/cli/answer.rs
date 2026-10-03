//! `kb answer`: ask a question over the knowledge base, or evaluate retrieval quality against
//! `eval/queries.toml`. design.md §13's hooks table row `answer`, §31 (evaluation).
use crate::answer::{self, AnswerOpts, eval};
use crate::config::Config;
use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Subcommand, Debug)]
pub enum AnswerCommand {
	/// ask a question; search, answer from the passages found, and cite them.
	Ask {
		question: String,
		#[arg(long, default_value_t = 5)]
		limit: usize,
		#[arg(long)]
		include_captures: bool,
		/// skip writing `read`/`cite` usage events for this question.
		#[arg(long)]
		no_record: bool,
	},
	/// mrr@10 and recall@5 over an eval set (design.md §31); `--compare a,b` runs both features
	/// and reports the delta and whether the §31 enable rule passes.
	Eval {
		/// `fts`, `hybrid`, or `hybrid+rerank`; default `hybrid`.
		#[arg(long)]
		features: Option<String>,
		/// two features, comma-separated (e.g. `fts,hybrid`): the first is the baseline.
		#[arg(long)]
		compare: Option<String>,
		/// path to the eval set; default `eval/queries.toml`.
		#[arg(long)]
		set: Option<PathBuf>,
	},
	/// write a starter eval set from each indexed document's own title (design.md build step 5:
	/// "a weak but useful bootstrap"); rows are marked `seeded = true`.
	EvalSeed {
		/// cap on how many documents to seed; default: every canon/draft document with a title.
		#[arg(long)]
		from_search: Option<usize>,
	},
}

fn eval_set_path(cfg: &Config, set: &Option<PathBuf>) -> PathBuf {
	match set {
		Some(p) if p.is_absolute() => p.clone(),
		Some(p) => cfg.root.join(p),
		None => cfg.root.join("eval").join("queries.toml"),
	}
}

fn print_metrics_table(rows: &[(&str, eval::EvalMetrics)]) {
	println!("{:<14} {:>8} {:>10} {:>6}", "feature", "mrr@10", "recall@5", "n");
	for (name, m) in rows {
		println!("{name:<14} {:>8.4} {:>10.4} {:>6}", m.mrr_at_10, m.recall_at_5, m.n);
	}
}

pub fn run(cfg: &Config, cmd: AnswerCommand, json: bool) -> Result<i32> {
	match cmd {
		AnswerCommand::Ask { question, limit, include_captures, no_record } => {
			let opts = AnswerOpts { limit, include_captures, record: !no_record, readable: None };
			let result = answer::answer(cfg, &question, &opts)?;
			if json {
				println!("{}", serde_json::to_string_pretty(&result)?);
				return Ok(0);
			}
			if result.deferred {
				println!("no `answer` provider configured; here is the rendered prompt for the calling agent to answer itself:\n");
				println!("{}", result.deferred_prompt.unwrap_or_default());
				return Ok(0);
			}
			println!("{}", result.text);
			if !result.citations.is_empty() {
				println!("\ncitations:");
				for c in &result.citations {
					println!("  [kb:{}#{}] {}", c.doc_id, c.heading_path, c.title.as_deref().unwrap_or("(untitled)"));
				}
			}
			if !result.dropped_citations.is_empty() {
				println!("\ndropped (not among the passages given): {}", result.dropped_citations.join(", "));
			}
			println!("\nrun: {}", result.run_id);
			Ok(0)
		}
		AnswerCommand::Eval { features, compare, set } => {
			let path = eval_set_path(cfg, &set);
			let queries = eval::load_queries(&path).with_context(|| format!("loading eval set {}", path.display()))?;
			if queries.is_empty() {
				println!("no queries in {} (run `kb answer eval-seed` for a starter set)", path.display());
				return Ok(1);
			}

			if let Some(compare) = compare {
				let (a_name, b_name) = compare.split_once(',').context("--compare expects `a,b`, e.g. --compare fts,hybrid")?;
				let a_feature = eval::Feature::from_str(a_name.trim()).map_err(anyhow::Error::msg)?;
				let b_feature = eval::Feature::from_str(b_name.trim()).map_err(anyhow::Error::msg)?;
				let a = eval::run_eval(cfg, a_feature, &queries)?;
				let b = eval::run_eval(cfg, b_feature, &queries)?;
				let gate = eval::check_enable_gate(queries.len(), &a, &b);
				if json {
					println!(
						"{}",
						serde_json::json!({
							"baseline": {"feature": a_feature.as_str(), "mrr_at_10": a.mrr_at_10, "recall_at_5": a.recall_at_5, "n": a.n},
							"candidate": {"feature": b_feature.as_str(), "mrr_at_10": b.mrr_at_10, "recall_at_5": b.recall_at_5, "n": b.n},
							"mrr_delta": b.mrr_at_10 - a.mrr_at_10,
							"recall_delta": b.recall_at_5 - a.recall_at_5,
							"enable_gate_passes": gate.passes,
							"reasons": gate.reasons,
						})
					);
				} else {
					print_metrics_table(&[(a_feature.as_str(), a), (b_feature.as_str(), b)]);
					println!("\nmrr@10 delta:    {:+.4}", b.mrr_at_10 - a.mrr_at_10);
					println!("recall@5 delta:  {:+.4}", b.recall_at_5 - a.recall_at_5);
					if gate.passes {
						println!("\nenable rule (design.md §31): PASSES");
					} else {
						println!("\nenable rule (design.md §31): does not pass");
						for r in &gate.reasons {
							println!("  - {r}");
						}
					}
				}
				return Ok(if gate.passes { 0 } else { 1 });
			}

			let feature = match &features {
				Some(f) => eval::Feature::from_str(f).map_err(anyhow::Error::msg)?,
				None => eval::Feature::Hybrid,
			};
			let metrics = eval::run_eval(cfg, feature, &queries)?;
			if json {
				println!(
					"{}",
					serde_json::json!({"feature": feature.as_str(), "mrr_at_10": metrics.mrr_at_10, "recall_at_5": metrics.recall_at_5, "n": metrics.n})
				);
			} else {
				print_metrics_table(&[(feature.as_str(), metrics)]);
			}
			Ok(0)
		}
		AnswerCommand::EvalSeed { from_search } => {
			let path = eval_set_path(cfg, &None);
			let existing = eval::load_queries(&path).unwrap_or_default();
			let seeded = eval::seed_from_documents(cfg, from_search)?;
			let existing_texts: std::collections::HashSet<String> = existing.iter().map(|q| q.text.clone()).collect();
			let mut merged = existing;
			let mut added = 0usize;
			for row in seeded {
				if !existing_texts.contains(&row.text) {
					merged.push(row);
					added += 1;
				}
			}
			eval::write_queries(&path, &merged)?;
			if json {
				println!("{}", serde_json::json!({"path": path, "added": added, "total": merged.len()}));
			} else {
				println!("added {added} seeded quer{} to {} ({} total)", if added == 1 { "y" } else { "ies" }, path.display(), merged.len());
			}
			Ok(0)
		}
	}
}
