//! `kb index` subcommands. not wired into `src/cli/mod.rs`; see that agent's report for the
//! registration snippet.
use crate::config::Config;
use crate::index;
use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum IndexCommand {
    /// build (or reuse) the index artifact for a commit; defaults to HEAD.
    Build {
        /// step 1 always does a full build; this flag is accepted for forward compatibility with
        /// the incremental builder and is a no-op today.
        #[arg(long)]
        full: bool,
        /// rebuild the target commit's artifact into a new `<commit>-<n>.sqlite` file even though
        /// `<commit>.sqlite` already exists, so a capture landed since the last build becomes
        /// searchable without waiting for the next `main` commit.
        #[arg(long)]
        refresh_captures: bool,
        commit: Option<String>,
    },
    /// fts5 search over indexed documents.
    Search {
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// widen candidates to every status (superseded, archived included); fts only.
        #[arg(long)]
        status_any: bool,
        /// widen the candidate set to captures as well as documents; off by default, since
        /// captures never appear in default search (design.md invariant 9).
        #[arg(long)]
        include_captures: bool,
    },
    /// fetch one document by id (full ulid or unique prefix).
    Get { id: String },
    /// print frontmatter, the derived block, and the body for one document.
    Show { id: String },
    /// walk the reference graph from one document.
    Refs {
        id: String,
        #[arg(long)]
        incoming: bool,
        #[arg(long, default_value_t = 1)]
        depth: u32,
    },
    /// list indexed documents, optionally filtered.
    Ls {
        #[arg(long)]
        layer: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        /// `document` (default) or `capture`.
        #[arg(long)]
        kind: Option<String>,
    },
    /// per-section last actor and commit for one document.
    Blame { id: String },
    /// graph metrics summary: top documents by pagerank and criticality, and any cycle findings.
    Graph {
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
}

pub fn run(cfg: &Config, cmd: IndexCommand, json: bool) -> Result<i32> {
    match cmd {
        IndexCommand::Build { full: _, refresh_captures, commit } => {
            let report =
                if refresh_captures { index::refresh_captures(cfg, commit.as_deref())? } else { index::build(cfg, commit.as_deref())? };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "commit": report.commit,
                        "artifact_path": report.artifact_path,
                        "already_built": report.already_built,
                        "document_count": report.document_count,
                        "canon_count": report.canon_count,
                        "capture_count": report.capture_count,
                        "chunk_count": report.chunk_count,
                        "finding_count": report.finding_count,
                        "maintenance_enabled": report.maintenance_enabled,
                    })
                );
            } else if report.already_built {
                println!(
                    "already built: {} ({} documents, {} canon, {} captures)",
                    report.commit, report.document_count, report.canon_count, report.capture_count
                );
            } else {
                println!(
                    "built {}: {} documents ({} canon), {} captures, {} chunks, {} finding(s){}",
                    report.commit,
                    report.document_count,
                    report.canon_count,
                    report.capture_count,
                    report.chunk_count,
                    report.finding_count,
                    if report.maintenance_enabled { "" } else { " (maintenance disabled: below min_corpus)" }
                );
            }
            Ok(0)
        }
        IndexCommand::Search { query, limit, status_any, include_captures } => {
            let conn = index::open_newest(cfg)?;
            let opts = index::SearchOpts { limit, status_any, include_captures };
            let hits = index::search(&conn, &query, &opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("no results for `{query}`");
            } else {
                for h in &hits {
                    let tag = if h.kind == "document" { String::new() } else { format!("[{}] ", h.kind) };
                    let source = h.source_uri.as_deref().map(|s| format!("  <- {s}")).unwrap_or_default();
                    println!(
                        "{tag}{}  {:>7.2}  {}#{}  {}{source}",
                        h.doc_id,
                        h.score,
                        h.title.as_deref().unwrap_or("(untitled)"),
                        h.heading_path,
                        h.snippet
                    );
                }
            }
            Ok(0)
        }
        IndexCommand::Get { id } => {
            let conn = index::open_newest(cfg)?;
            match index::get(&conn, &id)? {
                Some(rec) => {
                    println!("{}", serde_json::to_string_pretty(&rec)?);
                    Ok(0)
                }
                None => {
                    eprintln!("no document matches `{id}`");
                    Ok(1)
                }
            }
        }
        IndexCommand::Show { id } => {
            let result = index::show(cfg, &id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&result.frontmatter)?);
                println!(
                    "--- derived: last_updated={} updated_state={} ---",
                    result.last_updated.as_deref().unwrap_or("(unknown)"),
                    result.updated_state
                );
                println!("{}", result.body);
            }
            Ok(0)
        }
        IndexCommand::Refs { id, incoming, depth } => {
            let conn = index::open_newest(cfg)?;
            let rows = index::refs(&conn, &id, incoming, depth)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in &rows {
                    println!("{} --{}--> {} ({})", r.from_id, r.rel, r.resolved_id.as_deref().unwrap_or(&r.uri), r.drift_state.as_deref().unwrap_or("ok"));
                }
            }
            Ok(0)
        }
        IndexCommand::Ls { layer, status, tag, kind } => {
            let conn = index::open_newest(cfg)?;
            let rows = index::ls(&conn, &index::LsFilter { layer, status, tag, kind })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in &rows {
                    println!(
                        "{}  {:<10} {:<10} {:>5.1}  {}",
                        r.id,
                        r.layer.as_deref().unwrap_or("-"),
                        r.status.as_deref().unwrap_or("-"),
                        r.criticality_pct.unwrap_or(0.0),
                        r.title.as_deref().unwrap_or("(untitled)")
                    );
                }
            }
            Ok(0)
        }
        IndexCommand::Blame { id } => {
            let conn = index::open_newest(cfg)?;
            let rows = index::blame(cfg, &conn, &id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in &rows {
                    println!("{:<30} {} {} ({})", r.heading_path, r.commit, r.actor, r.date.as_deref().unwrap_or("unknown"));
                }
            }
            Ok(0)
        }
        IndexCommand::Graph { top } => {
            let conn = index::open_newest(cfg)?;
            let rows = index::ls(&conn, &index::LsFilter::default())?;
            let mut ranked: Vec<_> = rows.into_iter().filter(|r| r.criticality_pct.is_some()).collect();
            ranked.sort_by(|a, b| b.criticality_pct.partial_cmp(&a.criticality_pct).unwrap_or(std::cmp::Ordering::Equal));
            ranked.truncate(top);
            if json {
                println!("{}", serde_json::to_string_pretty(&ranked)?);
            } else {
                for r in &ranked {
                    println!("{}  {:>5.1}  {}", r.id, r.criticality_pct.unwrap_or(0.0), r.title.as_deref().unwrap_or("(untitled)"));
                }
            }
            Ok(0)
        }
    }
}
