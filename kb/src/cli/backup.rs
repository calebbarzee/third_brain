//! `kb backup ...`: mirror push, bundles, and the content-store copy (design.md §21).
//! not wired into `cli::Command` -- see this crate's report for the snippet to add there.

use crate::backup::{self, BackupKind};
use crate::config::Config;
use anyhow::{Result, bail};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum BackupCommand {
    /// run one backup: mirror, bundle, bundle-full, store-copy, or all (default: all)
    Run {
        #[arg(long)]
        kind: Option<String>,
    },
    /// print the backups manifest: last mirrored, last bundled, last store copy
    Manifest,
    /// `git bundle verify` on the newest bundle
    Verify,
    /// clone from the newest full bundle plus incrementals and diff against main
    RestoreDrill { into: PathBuf },
}

fn parse_kind(s: &Option<String>) -> Result<BackupKind> {
    Ok(match s.as_deref() {
        None | Some("all") => BackupKind::All,
        Some("mirror") => BackupKind::Mirror,
        Some("bundle") => BackupKind::Bundle { full: false },
        Some("bundle-full") => BackupKind::Bundle { full: true },
        Some("store-copy") => BackupKind::StoreCopy,
        Some(other) => bail!("unknown backup kind `{other}` (expected mirror, bundle, bundle-full, store-copy, or all)"),
    })
}

pub fn run(cfg: &Config, cmd: BackupCommand, json: bool) -> Result<i32> {
    match cmd {
        BackupCommand::Run { kind } => {
            let report = backup::run(cfg, parse_kind(&kind)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                if let Some(m) = &report.mirrored {
                    println!("mirrored: {m}");
                }
                if let Some(p) = &report.bundle_path {
                    println!("bundled: {} ({})", p.display(), report.bundle_sha256.as_deref().unwrap_or("-"));
                }
                if report.store_files_copied > 0 {
                    println!("store: copied {} file(s)", report.store_files_copied);
                }
                for w in &report.warnings {
                    println!("warning: {w}");
                }
            }
            Ok(0)
        }
        BackupCommand::Manifest => {
            let manifest = backup::manifest(cfg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&manifest)?);
            } else {
                println!("last mirrored: {} ({})", manifest.last_mirrored.as_deref().unwrap_or("-"), manifest.last_mirrored_at.as_deref().unwrap_or("-"));
                println!("last bundled: {} ({})", manifest.last_bundled.as_deref().unwrap_or("-"), manifest.last_bundled_at.as_deref().unwrap_or("-"));
                println!(
                    "last store copy: {} ({})",
                    manifest.last_store_copy.as_deref().unwrap_or("-"),
                    manifest.last_store_copy_at.as_deref().unwrap_or("-")
                );
            }
            Ok(0)
        }
        BackupCommand::Verify => {
            let lines = backup::verify(cfg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&lines)?);
            } else {
                for l in &lines {
                    println!("{l}");
                }
            }
            Ok(0)
        }
        BackupCommand::RestoreDrill { into } => {
            let diffs = backup::restore_drill(cfg, &into)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&diffs)?);
            } else if diffs.is_empty() {
                println!("restore drill: no differences");
            } else {
                for d in &diffs {
                    println!("{d}");
                }
            }
            Ok(if diffs.is_empty() { 0 } else { 1 })
        }
    }
}
