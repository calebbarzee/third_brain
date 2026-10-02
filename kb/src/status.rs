//! `kb status`: one screen summarizing repository health. contract: this is a read-only report,
//! nothing here writes to the repository or its state. every section degrades gracefully when
//! its data source is absent (no index built yet, no captures, no scheduler installed, another
//! agent's `backup` module not yet present), so `kb status` never fails just because a repo is
//! young.

use crate::config::Config;
use crate::git;
use crate::merge;
use crate::model::Status;
use crate::session::{self, SessionState};
use crate::store::catalog;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenSessionRow {
    pub id: String,
    pub state: String,
    pub actor: String,
    pub age_hours: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexStatus {
    pub artifact: Option<String>,
    pub artifact_commit: Option<String>,
    pub main_commit: Option<String>,
    /// `true` when the newest artifact was built for the current `main` head.
    pub current: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CaptureCounts {
    pub new: u64,
    pub curated: u64,
    pub quarantined: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupStatus {
    pub kind: String,
    pub at: String,
}

/// the full report. every field is best-effort: a missing subsystem yields the empty/default
/// value for its section rather than an error.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusReport {
    pub root: PathBuf,
    pub main_head: Option<String>,
    /// layer -> status -> count, over `docs/**/*.md` in the working tree.
    pub documents: BTreeMap<String, BTreeMap<String, u64>>,
    pub total_documents: u64,
    pub open_sessions: Vec<OpenSessionRow>,
    pub review_queue: u64,
    pub escalations: u64,
    pub index: IndexStatus,
    pub captures: CaptureCounts,
    pub backups: Vec<BackupStatus>,
    pub provider_roles: Vec<String>,
    /// `kb-*` lines from `systemctl --user list-timers`, or empty when systemctl is unavailable.
    pub scheduler_units: Vec<String>,
}

/// gather the whole report. never fails on a missing optional subsystem; only a broken `docs/`
/// walk or a corrupt session file can surface as `Err`.
pub fn gather(cfg: &Config) -> Result<StatusReport> {
    let main_head = git::rev_parse(&cfg.root, "main").ok().filter(|s| !s.is_empty());
    let (documents, total_documents) = document_counts(cfg);
    let open_sessions = open_sessions(cfg)?;
    let review_queue = merge::review_list(cfg).map(|v| v.len() as u64).unwrap_or(0);
    let escalations = escalation_count(cfg);
    let index = index_status(cfg, main_head.as_deref());
    let captures = capture_counts(cfg);
    let backups = backup_status(cfg);
    let provider_roles = cfg.kb.providers.iter().filter(|(_, p)| !p.kind.is_empty()).map(|(role, _)| role.clone()).collect();
    let scheduler_units = scheduler_units();

    Ok(StatusReport {
        root: cfg.root.clone(),
        main_head,
        documents,
        total_documents,
        open_sessions,
        review_queue,
        escalations,
        index,
        captures,
        backups,
        provider_roles,
        scheduler_units,
    })
}

/// layer -> status -> count over every `docs/**/*.md` file in the working tree. a file that
/// fails to parse is skipped (validate reports it elsewhere); this is a census, not a check.
fn document_counts(cfg: &Config) -> (BTreeMap<String, BTreeMap<String, u64>>, u64) {
    let docs_dir = cfg.root.join("docs");
    let mut by_layer: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut total = 0u64;
    if !docs_dir.is_dir() {
        return (by_layer, total);
    }
    for entry in walkdir::WalkDir::new(&docs_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let Ok(doc) = crate::parse::parse_document(&text) else { continue };
        let layer = doc.frontmatter.layer.map(|l| l.as_str().to_string()).unwrap_or_else(|| "unknown".to_string());
        let status = doc.frontmatter.status.unwrap_or(Status::Draft).as_str().to_string();
        *by_layer.entry(layer).or_default().entry(status).or_insert(0) += 1;
        total += 1;
    }
    (by_layer, total)
}

fn open_sessions(cfg: &Config) -> Result<Vec<OpenSessionRow>> {
    let mut rows = Vec::new();
    for s in session::list(cfg)? {
        if matches!(s.state, SessionState::Merged | SessionState::Abandoned) {
            continue;
        }
        let age_hours = match chrono::DateTime::parse_from_rfc3339(&s.opened_at) {
            Ok(t) => (crate::clock::now() - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 3600.0,
            Err(_) => 0.0,
        };
        rows.push(OpenSessionRow { id: s.id, state: s.state.as_str().to_string(), actor: s.actor, age_hours });
    }
    Ok(rows)
}

/// `<state>/escalations/*.toml` count; `0` when the directory does not exist. design.md leaves
/// the escalation record's shape to the `curate`/`review` owners; this only counts files.
fn escalation_count(cfg: &Config) -> u64 {
    let dir = cfg.state_dir().join("escalations");
    std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|ext| ext == "toml")).count() as u64)
        .unwrap_or(0)
}

fn index_status(cfg: &Config, main_head: Option<&str>) -> IndexStatus {
    let artifact = crate::index::newest_artifact(cfg);
    let artifact_commit = artifact.as_ref().and_then(|p| p.file_stem()).and_then(|s| s.to_str()).map(|stem| {
        // strip a `-<n>` refresh suffix (see `index::build::refresh_captures`) to compare
        // against `main`'s sha.
        stem.split('-').next().unwrap_or(stem).to_string()
    });
    let current = matches!((&artifact_commit, main_head), (Some(a), Some(m)) if a == m);
    IndexStatus {
        artifact: artifact.map(|p| p.display().to_string()),
        artifact_commit,
        main_commit: main_head.map(str::to_string),
        current,
    }
}

fn capture_counts(cfg: &Config) -> CaptureCounts {
    let Ok(conn) = catalog::open_state(cfg) else { return CaptureCounts::default() };
    let count = |status: &str| -> u64 {
        conn.query_row("select count(*) from captures where status = ?1", [status], |r| r.get::<_, i64>(0)).unwrap_or(0).max(0) as u64
    };
    CaptureCounts { new: count("new"), curated: count("curated"), quarantined: count("quarantined") }
}

/// last backup per kind, from the `backups` table in `state.sqlite`. that table belongs to the
/// `backup` module (owned by another agent, concurrently in progress); read defensively so `kb
/// status` works before and after it lands: no `state.sqlite`, no `backups` table, or a
/// differently-shaped one all yield an empty list rather than an error.
fn backup_status(cfg: &Config) -> Vec<BackupStatus> {
    let Ok(conn) = catalog::open_state(cfg) else { return Vec::new() };
    let exists: bool = conn
        .query_row("select count(*) from sqlite_master where type = 'table' and name = 'backups'", [], |r| r.get::<_, i64>(0))
        .map(|n| n > 0)
        .unwrap_or(false);
    if !exists {
        return Vec::new();
    }
    let query = "select kind, max(ts) from backups group by kind order by kind";
    let Ok(mut stmt) = conn.prepare(query) else { return Vec::new() };
    let Ok(rows) = stmt.query_map([], |r| Ok(BackupStatus { kind: r.get(0)?, at: r.get(1)? })) else { return Vec::new() };
    rows.filter_map(|r| r.ok()).collect()
}

/// `kb-*` lines from `systemctl --user list-timers --all`; empty when systemctl is unavailable
/// (no systemd user session, e.g. most sandboxes and containers) rather than an error.
fn scheduler_units() -> Vec<String> {
    std::process::Command::new("systemctl")
        .args(["--user", "list-timers", "--all", "--no-legend"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| l.contains("kb-")).map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentPolicy, Layer};

    fn doc_text(id: &str, layer: Layer, status: Status) -> String {
        format!(
            "+++\nid = \"{id}\"\nlayer = \"{}\"\ntitle = \"t\"\nstatus = \"{}\"\nagent_policy = \"{}\"\nowner = \"caleb\"\n+++\n\nbody\n",
            layer.as_str(),
            status.as_str(),
            AgentPolicy::Editable
        )
    }

    #[test]
    fn document_counts_group_by_layer_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH0A.md"), doc_text("01K5A7Q2XW3ZRCM8T6YB4VJH0A", Layer::Reference, Status::Canon)).unwrap();
        std::fs::write(dir.path().join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH0B.md"), doc_text("01K5A7Q2XW3ZRCM8T6YB4VJH0B", Layer::Reference, Status::Draft)).unwrap();
        std::fs::write(dir.path().join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH0C.md"), doc_text("01K5A7Q2XW3ZRCM8T6YB4VJH0C", Layer::Decision, Status::Canon)).unwrap();

        let (by_layer, total) = document_counts(&cfg);
        assert_eq!(total, 3);
        assert_eq!(by_layer["reference"]["canon"], 1);
        assert_eq!(by_layer["reference"]["draft"], 1);
        assert_eq!(by_layer["decision"]["canon"], 1);
    }

    #[test]
    fn escalation_count_is_zero_without_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(escalation_count(&cfg), 0);
        std::fs::create_dir_all(cfg.state_dir().join("escalations")).unwrap();
        std::fs::write(cfg.state_dir().join("escalations/one.toml"), "id = \"x\"\n").unwrap();
        std::fs::write(cfg.state_dir().join("escalations/note.txt"), "ignored").unwrap();
        assert_eq!(escalation_count(&cfg), 1);
    }

    #[test]
    fn backup_status_is_empty_before_the_table_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(backup_status(&cfg).is_empty());
    }

    #[test]
    fn capture_counts_reflect_put_capture() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        catalog::put_capture(
            &cfg,
            catalog::PutCapture {
                source: "manual".into(),
                source_uri: "file:/tmp/a.txt".into(),
                source_cursor: None,
                media_type: "text/plain".into(),
                title: None,
                original: b"hello".to_vec(),
                text: "hello".into(),
                captured_at: None,
            },
        )
        .unwrap();
        let counts = capture_counts(&cfg);
        assert_eq!(counts.new, 1);
        assert_eq!(counts.curated, 0);
    }

    #[test]
    fn gather_runs_clean_on_a_freshly_initialized_repo() {
        let dir = tempfile::tempdir().unwrap();
        crate::cli::init::init(dir.path(), false, false).unwrap();
        git::run(dir.path(), &["config", "user.name", "test user"]).unwrap();
        git::run(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        git::commit_all(dir.path(), "seed").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let report = gather(&cfg).unwrap();
        assert_eq!(report.total_documents, 0);
        assert!(report.main_head.is_some());
        assert!(report.open_sessions.is_empty());
    }
}
