//! `kb archive` subcommands: the capture catalog (ls, show, export, put, quarantine, capture).
//! not wired into `cli::mod`; see this crate's report for the wiring snippet.

use crate::config::Config;
use crate::store::catalog::{self, Capture};
use crate::store::{export, normalize, sources};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum ArchiveCommand {
    /// list captures, most recent first.
    Ls {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// show one capture's metadata and normalized text.
    Show { id: String },
    /// write a capture's normalized text (or, with --original, its raw bytes) to a file.
    Export {
        id: String,
        #[arg(long)]
        original: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// capture one local file by hand (source "manual").
    Put {
        path: PathBuf,
        #[arg(long)]
        source: Option<String>,
    },
    /// list quarantined captures, or release one back into the normal blob store.
    Quarantine {
        #[arg(long)]
        release: Option<String>,
    },
    /// pull every configured source (or just --source) into the catalog.
    Capture {
        #[arg(long)]
        source: Option<String>,
    },
}

pub fn run(cfg: &Config, cmd: ArchiveCommand, json: bool) -> Result<i32> {
    match cmd {
        ArchiveCommand::Ls { status, source, limit } => run_ls(cfg, status, source, limit, json),
        ArchiveCommand::Show { id } => run_show(cfg, &id, json),
        ArchiveCommand::Export { id, original, out } => run_export(cfg, &id, original, out, json),
        ArchiveCommand::Put { path, source } => run_put(cfg, &path, source, json),
        ArchiveCommand::Quarantine { release } => run_quarantine(cfg, release, json),
        ArchiveCommand::Capture { source } => run_capture(cfg, source, json),
    }
}

fn run_ls(cfg: &Config, status: Option<String>, source: Option<String>, limit: usize, json: bool) -> Result<i32> {
    let rows = catalog::list(cfg, status.as_deref(), source.as_deref(), limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("no captures");
    } else {
        for c in &rows {
            println!("{} {:<10} {:<12} {}", c.capture_id, c.status, c.source, c.title.as_deref().unwrap_or(&c.source_uri));
        }
    }
    Ok(0)
}

fn run_show(cfg: &Config, id: &str, json: bool) -> Result<i32> {
    let Some(cap) = catalog::get(cfg, id)? else {
        bail!("no capture matching `{id}`");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&cap)?);
        return Ok(0);
    }
    print_capture(&cap);
    let text = catalog::text(cfg, &cap)?;
    println!();
    let preview: String = text.chars().take(2000).collect();
    println!("{preview}");
    if text.len() > preview.len() {
        println!("... ({} bytes total)", text.len());
    }
    Ok(0)
}

fn print_capture(c: &Capture) {
    println!("capture_id:      {}", c.capture_id);
    println!("source:          {}", c.source);
    println!("source_uri:      {}", c.source_uri);
    println!("captured_at:     {}", c.captured_at);
    println!("media_type:      {}", c.media_type);
    println!("title:           {}", c.title.as_deref().unwrap_or("-"));
    println!("status:          {}", c.status);
    println!("sha256_original: {}", c.sha256_original);
    println!("sha256_text:     {}", c.sha256_text);
}

fn run_export(cfg: &Config, id: &str, original: bool, out: Option<PathBuf>, json: bool) -> Result<i32> {
    let path = export::export(cfg, id, original, out.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "path": path }))?);
    } else {
        println!("wrote {}", path.display());
    }
    Ok(0)
}

/// capture one local file by hand; source defaults to "manual".
fn run_put(cfg: &Config, path: &std::path::Path, source: Option<String>, json: bool) -> Result<i32> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let filename = path.file_name().and_then(|n| n.to_str());
    let normalized = normalize::normalize("", &bytes, filename)?;
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let cap = catalog::put_capture(
        cfg,
        catalog::PutCapture {
            source: source.unwrap_or_else(|| "manual".to_string()),
            source_uri: format!("file:{}", abs.display()),
            source_cursor: None,
            media_type: normalized.media_type,
            title: normalized.title,
            original: bytes,
            text: normalized.text,
            captured_at: None,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cap)?);
    } else {
        println!("{} {} ({})", cap.capture_id, cap.status, cap.source_uri);
    }
    Ok(0)
}

fn run_quarantine(cfg: &Config, release: Option<String>, json: bool) -> Result<i32> {
    match release {
        None => {
            let rows = catalog::list(cfg, Some("quarantined"), None, 500)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("no quarantined captures");
            } else {
                for c in &rows {
                    println!("{} {:<12} {}", c.capture_id, c.source, c.source_uri);
                }
            }
            Ok(0)
        }
        Some(id) => {
            let released = export::release_quarantine(cfg, &id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&released)?);
            } else {
                println!("released {} ({})", released.capture_id, released.source_uri);
            }
            Ok(0)
        }
    }
}

fn run_capture(cfg: &Config, source: Option<String>, json: bool) -> Result<i32> {
    let report = sources::capture_all(cfg, source.as_deref())?;
    if json {
        let obj = serde_json::json!({
            "per_source": report.per_source,
            "quarantined": report.quarantined,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        for (name, count) in &report.per_source {
            println!("{name}: {count} new");
        }
        println!("quarantined: {}", report.quarantined);
    }
    Ok(0)
}
