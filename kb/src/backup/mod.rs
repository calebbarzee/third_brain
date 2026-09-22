//! design.md §21's backup flow: mirror push, bundles (daily full, hourly incremental), a
//! second-location copy of the content store, and the backups manifest that `kb merge`'s delete
//! guard and `kb maintain compact` both read.
//!
//! `kb.toml` has no `[backup]` section and this crate does not add one; instead `<root>/backup.toml`
//! is its own small, optional file (`mirror`, `bundle_dir`, `store_copy`, all optional -- a repo
//! with none configured still builds and tests, it just never backs up).

use crate::config::Config;
use crate::git;
use crate::parse::frontmatter::sha256_hex;
use crate::store::catalog;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// `<root>/backup.toml`. every field is optional; a missing file is the same as every field unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    /// a bare repository path (created with `git init --bare` if missing and local) or a remote
    /// git url `git push --mirror` accepts.
    pub mirror: Option<String>,
    pub bundle_dir: Option<PathBuf>,
    pub store_copy: Option<PathBuf>,
}

pub fn load_backup_config(cfg: &Config) -> Result<BackupConfig> {
    let path = cfg.root.join("backup.toml");
    if !path.is_file() {
        return Ok(BackupConfig::default());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// what `kb backup run` does.
#[derive(Debug, Clone)]
pub enum BackupKind {
    Mirror,
    Bundle { full: bool },
    StoreCopy,
    /// mirror, then a full bundle, then a store copy -- everything `backup.toml` configures.
    All,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BackupReport {
    pub mirrored: Option<String>,
    pub bundle_path: Option<PathBuf>,
    pub bundle_sha256: Option<String>,
    pub store_files_copied: usize,
    pub warnings: Vec<String>,
}

/// the backups manifest: what has been backed up most recently, read by the delete guard
/// precondition and by `kb maintain compact`'s checkpoint check.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BackupManifest {
    pub last_mirrored: Option<String>,
    pub last_mirrored_at: Option<String>,
    pub last_bundled: Option<String>,
    pub last_bundled_at: Option<String>,
    pub last_store_copy: Option<String>,
    pub last_store_copy_at: Option<String>,
}

fn open_backup_db(cfg: &Config) -> Result<Connection> {
    let conn = catalog::open_state(cfg)?;
    conn.execute_batch(
        "create table if not exists backups (
            id integer primary key autoincrement,
            kind text not null,
            commit_sha text not null,
            location text not null,
            sha256 text,
            ts text not null
        );",
    )
    .context("creating backups table")?;
    Ok(conn)
}

fn record_backup(cfg: &Config, kind: &str, commit: &str, location: &str, sha256: Option<&str>) -> Result<()> {
    let conn = open_backup_db(cfg)?;
    conn.execute(
        "insert into backups (kind, commit_sha, location, sha256, ts) values (?1,?2,?3,?4,?5)",
        rusqlite::params![kind, commit, location, sha256, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn query_last(conn: &Connection, kind: &str) -> Option<(String, String)> {
    conn.query_row("select commit_sha, ts from backups where kind = ?1 order by id desc limit 1", [kind], |r| Ok((r.get(0)?, r.get(1)?))).ok()
}

/// the backups manifest, from `<state>/state.sqlite`'s `backups` table.
pub fn manifest(cfg: &Config) -> Result<BackupManifest> {
    let conn = open_backup_db(cfg)?;
    let mirrored = query_last(&conn, "mirror");
    let bundled = query_last(&conn, "bundle");
    let store_copy = query_last(&conn, "store_copy");
    Ok(BackupManifest {
        last_mirrored: mirrored.as_ref().map(|(c, _)| c.clone()),
        last_mirrored_at: mirrored.map(|(_, t)| t),
        last_bundled: bundled.as_ref().map(|(c, _)| c.clone()),
        last_bundled_at: bundled.map(|(_, t)| t),
        last_store_copy: store_copy.as_ref().map(|(c, _)| c.clone()),
        last_store_copy_at: store_copy.map(|(_, t)| t),
    })
}

fn ensure_mirror_target(target: &str) -> Result<()> {
    if target.contains("://") || target.contains('@') {
        return Ok(()); // a remote url; assumed to exist already.
    }
    let path = Path::new(target);
    if !path.exists() {
        git::init_bare(path)?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

fn list_bundle_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut out: Vec<PathBuf> =
        std::fs::read_dir(dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "bundle")).collect();
    out.sort();
    Ok(out)
}

/// parse `kb-<yyyymmdd>-<hhmm>-<full|incr>.bundle`'s timestamp.
fn parse_bundle_stamp(path: &Path) -> Option<DateTime<Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() != 4 || parts[0] != "kb" {
        return None;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(&format!("{}{}", parts[1], parts[2]), "%Y%m%d%H%M").ok()?;
    Some(naive.and_utc())
}

/// design.md §30: hourly (incremental) bundles kept 7 days, daily (full) bundles 90 days, and the
/// first full bundle of each calendar month kept 3 years.
fn apply_bundle_retention(dir: &Path) -> Result<()> {
    let bundles = list_bundle_files(dir)?;
    let now = Utc::now();
    let mut months_kept: HashSet<(i32, u32)> = HashSet::new();
    for path in &bundles {
        let Some(stamp) = parse_bundle_stamp(path) else { continue };
        let age_days = (now - stamp).num_days();
        let is_full = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("-full."));
        let limit = if is_full {
            let first_of_month = months_kept.insert((stamp.year(), stamp.month()));
            if first_of_month { 3 * 365 } else { 90 }
        } else {
            7
        };
        if age_days > limit {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// `kb backup run --kind <mirror|bundle|store-copy|all>`.
pub fn run(cfg: &Config, kind: BackupKind) -> Result<BackupReport> {
    let bc = load_backup_config(cfg)?;
    let mut report = BackupReport::default();

    let do_mirror = matches!(kind, BackupKind::Mirror | BackupKind::All);
    let do_bundle = matches!(kind, BackupKind::Bundle { .. } | BackupKind::All);
    let do_store = matches!(kind, BackupKind::StoreCopy | BackupKind::All);
    let full_bundle = matches!(kind, BackupKind::Bundle { full: true } | BackupKind::All);

    if do_mirror {
        match &bc.mirror {
            Some(target) => {
                ensure_mirror_target(target)?;
                git::push_mirror(&cfg.root, target)?;
                let head = git::rev_parse(&cfg.root, "main")?;
                record_backup(cfg, "mirror", &head, target, None)?;
                report.mirrored = Some(head);
            }
            None => report.warnings.push("no mirror configured in backup.toml".to_string()),
        }
    }

    if do_bundle {
        match &bc.bundle_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let head = git::rev_parse(&cfg.root, "main")?;
                let last_bundled = {
                    let conn = open_backup_db(cfg)?;
                    query_last(&conn, "bundle").map(|(c, _)| c)
                };
                let (file_kind, revs): (&str, Vec<String>) = if full_bundle {
                    ("full", vec!["--all".to_string()])
                } else {
                    match &last_bundled {
                        Some(last) if last != &head => ("incr", vec![format!("{last}..main")]),
                        Some(_) => {
                            report.warnings.push("nothing new to bundle since the last incremental".to_string());
                            return Ok(report);
                        }
                        None => ("full", vec!["--all".to_string()]),
                    }
                };
                let stamp = Utc::now().format("%Y%m%d-%H%M").to_string();
                let filename = format!("kb-{stamp}-{file_kind}.bundle");
                let path = dir.join(&filename);
                let revs_ref: Vec<&str> = revs.iter().map(String::as_str).collect();
                git::bundle_create(&cfg.root, &path, &revs_ref)?;
                let sha = sha256_file(&path)?;
                record_backup(cfg, "bundle", &head, &path.display().to_string(), Some(&sha))?;
                report.bundle_path = Some(path);
                report.bundle_sha256 = Some(sha);
                apply_bundle_retention(dir)?;
            }
            None => report.warnings.push("no bundle_dir configured in backup.toml".to_string()),
        }
    }

    if do_store {
        match &bc.store_copy {
            Some(dest) => {
                let src = cfg.state_dir().join("store");
                report.store_files_copied = copy_new_or_changed(&src, dest)?;
                let head = git::rev_parse(&cfg.root, "main").unwrap_or_default();
                record_backup(cfg, "store_copy", &head, &dest.display().to_string(), None)?;
            }
            None => report.warnings.push("no store_copy configured in backup.toml".to_string()),
        }
    }

    Ok(report)
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// recursive copy of `src` into `dest`, skipping files that already exist at `dest` with the same
/// size (design.md §21's content-store second-location copy needs no more than that).
fn copy_new_or_changed(src: &Path, dest: &Path) -> Result<usize> {
    if !src.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(dest)?;
    let mut files = Vec::new();
    walk_files(src, &mut files)?;
    let mut count = 0;
    for f in files {
        let rel = f.strip_prefix(src).unwrap_or(&f);
        let dst_path = dest.join(rel);
        let same_size = std::fs::metadata(&dst_path).ok().zip(std::fs::metadata(&f).ok()).is_some_and(|(a, b)| a.len() == b.len());
        if !same_size {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&f, &dst_path)?;
            count += 1;
        }
    }
    Ok(count)
}

/// `git bundle verify` on the newest bundle in `backup.toml`'s `bundle_dir`.
pub fn verify(cfg: &Config) -> Result<Vec<String>> {
    let bc = load_backup_config(cfg)?;
    let dir = bc.bundle_dir.context("no bundle_dir configured in backup.toml")?;
    let bundles = list_bundle_files(&dir)?;
    let newest = bundles.last().context("no bundle files found")?;
    let out = git::bundle_verify(&cfg.root, newest)?;
    Ok(out.lines().map(str::to_string).collect())
}

/// clone from the newest full bundle plus every incremental after it into `into`, then diff its
/// `main` against the repository's own `main`. an empty result means no differences; otherwise
/// each entry describes one (design.md §21's quarterly restore drill).
pub fn restore_drill(cfg: &Config, into: &Path) -> Result<Vec<String>> {
    let bc = load_backup_config(cfg)?;
    let dir = bc.bundle_dir.context("no bundle_dir configured in backup.toml")?;
    let bundles = list_bundle_files(&dir)?;
    let full_idx = bundles.iter().rposition(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains("-full.")));
    let Some(idx) = full_idx else { bail!("no full bundle found in {}", dir.display()) };
    let full = &bundles[idx];
    let incrementals = &bundles[idx + 1..];

    if into.exists() {
        std::fs::remove_dir_all(into).with_context(|| format!("clearing {}", into.display()))?;
    }
    git::clone(full, into)?;
    for incr in incrementals {
        git::run(into, &["fetch", &incr.display().to_string(), "+main:main"])?;
    }

    let restored_main = git::rev_parse(into, "main")?;
    let source_main = git::rev_parse(&cfg.root, "main")?;
    if restored_main == source_main {
        return Ok(vec![]);
    }
    let mut diffs = vec![format!("restored main {restored_main} != source main {source_main}")];
    if let Ok(stat) = git::run(into, &["diff", "--stat", &source_main, &restored_main]) {
        diffs.push(stat);
    }
    Ok(diffs)
}

fn commit_at_or_before(repo: &Path, commit: &str, backed_up: &str) -> bool {
    commit == backed_up || git::is_ancestor(repo, commit, backed_up)
}

/// the delete guard's backup precondition (design.md §17.4): a file may be deleted only once its
/// last commit is at or before both the newest mirrored and the newest bundled commit. `true` with
/// a warning when no backup is configured at all (a purely local repository may have none).
/// `src/merge/mod.rs` (not owned by this crate's task) is where this should be called from
/// `check_delete_guard`; see this crate's report for the wiring.
pub fn delete_allowed_by_backup(cfg: &Config, file_last_commit: &str) -> Result<(bool, String)> {
    let m = manifest(cfg)?;
    if m.last_mirrored.is_none() && m.last_bundled.is_none() {
        return Ok((true, "no backup configured (backup.toml has no mirror or bundle_dir); allowing delete without a verified backup".to_string()));
    }
    let mirrored_ok = m.last_mirrored.as_deref().is_none_or(|last| commit_at_or_before(&cfg.root, file_last_commit, last));
    let bundled_ok = m.last_bundled.as_deref().is_none_or(|last| commit_at_or_before(&cfg.root, file_last_commit, last));
    if mirrored_ok && bundled_ok {
        Ok((true, format!("backed up: last mirrored {:?}, last bundled {:?}", m.last_mirrored, m.last_bundled)))
    } else {
        Ok((false, format!("not yet backed up: the file's last commit ({file_last_commit}) is newer than the last mirror and/or bundle")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_config_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let bc = load_backup_config(&cfg).unwrap();
        assert!(bc.mirror.is_none());
        assert!(bc.bundle_dir.is_none());
    }

    #[test]
    fn backup_config_reads_backup_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("backup.toml"), "mirror = \"/tmp/x.git\"\nbundle_dir = \"/tmp/bundles\"\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let bc = load_backup_config(&cfg).unwrap();
        assert_eq!(bc.mirror.as_deref(), Some("/tmp/x.git"));
        assert_eq!(bc.bundle_dir, Some(PathBuf::from("/tmp/bundles")));
    }

    #[test]
    fn bundle_stamp_parses() {
        let stamp = parse_bundle_stamp(Path::new("kb-20260101-0930-full.bundle")).unwrap();
        assert_eq!(stamp.year(), 2026);
        assert_eq!(stamp.month(), 1);
    }

    #[test]
    fn delete_allowed_with_no_backup_configured() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let (ok, detail) = delete_allowed_by_backup(&cfg, "deadbeef").unwrap();
        assert!(ok);
        assert!(detail.contains("no backup configured"), "{detail}");
    }
}
