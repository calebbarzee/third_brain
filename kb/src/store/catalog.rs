//! the capture catalog: one row per captured item in `<state>/state.sqlite`. contract: design.md
//! §9, §19 (capture hook), §20 (secrets at store put).

use super::blob;
use crate::check;
use crate::config::Config;
use crate::model::new_ulid;
use crate::parse::frontmatter::sha256_hex;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};

const SCHEMA: &str = r#"
create table if not exists captures (
  capture_id text primary key, source text not null, source_uri text not null, source_cursor text,
  captured_at text not null, media_type text not null, sha256_original text not null, sha256_text text not null,
  normalizer_version integer not null, title text, access_json text not null default '["*"]', redacted integer not null default 0,
  status text not null default 'new' check (status in ('new','curated','discarded','quarantined')), curated_run text
);
create index if not exists captures_source on captures(source, captured_at);
create table if not exists source_cursors (source text primary key, cursor text not null, updated_at text not null);
"#;

const SELECT_COLS: &str = "capture_id, source, source_uri, captured_at, media_type, title, sha256_original, sha256_text, status";

/// a capture row, the subset the rest of the crate (and the curate module) works with.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Capture {
    pub capture_id: String,
    pub source: String,
    pub source_uri: String,
    pub captured_at: String,
    pub media_type: String,
    pub title: Option<String>,
    pub sha256_original: String,
    pub sha256_text: String,
    pub status: String,
}

/// input to `put_capture`: a source adapter (or a manual `kb archive put`) already normalized the
/// text; the catalog computes hashes, runs the secrets gate, and writes the blobs.
pub struct PutCapture {
    pub source: String,
    pub source_uri: String,
    pub source_cursor: Option<String>,
    pub media_type: String,
    pub title: Option<String>,
    pub original: Vec<u8>,
    pub text: String,
    pub captured_at: Option<String>,
}

/// open (creating if needed) `<state>/state.sqlite` with the catalog tables, wal mode, and a busy
/// timeout so concurrent captures do not fail outright on lock contention.
pub fn open_state(cfg: &Config) -> Result<Connection> {
    let dir = cfg.state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("state.sqlite");
    let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA).context("creating catalog schema")?;
    Ok(conn)
}

fn capture_from_row(row: &Row) -> rusqlite::Result<Capture> {
    Ok(Capture {
        capture_id: row.get(0)?,
        source: row.get(1)?,
        source_uri: row.get(2)?,
        captured_at: row.get(3)?,
        media_type: row.get(4)?,
        title: row.get(5)?,
        sha256_original: row.get(6)?,
        sha256_text: row.get(7)?,
        status: row.get(8)?,
    })
}

/// one capture by exact id or by an unambiguous id prefix; `None` when nothing or more than one
/// row matches the prefix.
pub fn get(cfg: &Config, id_or_prefix: &str) -> Result<Option<Capture>> {
    let conn = open_state(cfg)?;
    let exact = conn
        .query_row(&format!("select {SELECT_COLS} from captures where capture_id = ?1"), [id_or_prefix], capture_from_row)
        .optional()?;
    if exact.is_some() {
        return Ok(exact);
    }
    let pattern = format!("{id_or_prefix}%");
    let mut stmt = conn.prepare(&format!("select {SELECT_COLS} from captures where capture_id like ?1"))?;
    let mut rows = stmt.query([&pattern])?;
    let first = rows.next()?.map(capture_from_row).transpose()?;
    if first.is_none() {
        return Ok(None);
    }
    if rows.next()?.is_some() {
        return Ok(None); // ambiguous prefix
    }
    Ok(first)
}

/// the normalized text blob for a capture, from the blob store or, when the capture is
/// quarantined, from quarantine.
pub fn text(cfg: &Config, capture: &Capture) -> Result<String> {
    let bytes = if capture.status == "quarantined" {
        blob::read_quarantine(cfg, &capture.sha256_text)?
            .with_context(|| format!("missing quarantined text blob {}", capture.sha256_text))?
    } else {
        blob::open_store(cfg)?
            .get(&capture.sha256_text)?
            .with_context(|| format!("missing text blob {}", capture.sha256_text))?
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// most recent captures, optionally filtered by status and/or source.
pub fn list(cfg: &Config, status: Option<&str>, source: Option<&str>, limit: usize) -> Result<Vec<Capture>> {
    let conn = open_state(cfg)?;
    let mut sql = format!("select {SELECT_COLS} from captures where 1 = 1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(s) = status {
        sql.push_str(" and status = ?");
        args.push(Box::new(s.to_string()));
    }
    if let Some(s) = source {
        sql.push_str(" and source = ?");
        args.push(Box::new(s.to_string()));
    }
    sql.push_str(" order by captured_at desc limit ?");
    args.push(Box::new(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), capture_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// set a capture's status (and, when given, the curating run id).
pub fn set_status(cfg: &Config, id: &str, status: &str, run_id: Option<&str>) -> Result<()> {
    let conn = open_state(cfg)?;
    conn.execute(
        "update captures set status = ?1, curated_run = coalesce(?2, curated_run) where capture_id = ?3",
        params![status, run_id, id],
    )?;
    Ok(())
}

fn existing_by_source(conn: &Connection, source_uri: &str, sha256_original: &str) -> Result<Option<Capture>> {
    Ok(conn
        .query_row(
            &format!("select {SELECT_COLS} from captures where source_uri = ?1 and sha256_original = ?2"),
            params![source_uri, sha256_original],
            capture_from_row,
        )
        .optional()?)
}

/// store one captured item: dedups by `(source_uri, sha256_original)`, scans the text (and the
/// original, when it decodes as utf-8) for secrets, writes the blob(s) into quarantine on a hit
/// or into the normal blob store otherwise, and inserts the catalog row. never swallows an error:
/// a scan or write failure propagates rather than silently skipping the secrets gate.
pub fn put_capture(cfg: &Config, req: PutCapture) -> Result<Capture> {
    let sha256_original = sha256_hex(&req.original);
    let sha256_text = sha256_hex(req.text.as_bytes());

    let conn = open_state(cfg)?;
    if let Some(existing) = existing_by_source(&conn, &req.source_uri, &sha256_original)? {
        return Ok(existing);
    }

    let mut findings = check::scan_text(cfg, &req.source_uri, &req.text);
    if let Ok(original_text) = std::str::from_utf8(&req.original) {
        findings.extend(check::scan_text(cfg, &req.source_uri, original_text));
    }
    let quarantined = !findings.is_empty();

    if quarantined {
        blob::write_quarantine(cfg, &sha256_original, &req.original)?;
        blob::write_quarantine(cfg, &sha256_text, req.text.as_bytes())?;
    } else {
        let store = blob::open_store(cfg)?;
        store.put(&req.original, &req.media_type)?;
        store.put(req.text.as_bytes(), "text/plain")?;
    }

    let capture_id = new_ulid();
    let captured_at = req.captured_at.clone().unwrap_or_else(|| crate::clock::now().to_rfc3339());
    let status = if quarantined { "quarantined" } else { "new" };
    conn.execute(
        "insert into captures (capture_id, source, source_uri, source_cursor, captured_at, media_type, sha256_original, sha256_text, normalizer_version, title, status) \
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            capture_id,
            req.source,
            req.source_uri,
            req.source_cursor,
            captured_at,
            req.media_type,
            sha256_original,
            sha256_text,
            super::normalize::NORMALIZER_VERSION,
            req.title,
            status,
        ],
    )?;

    Ok(Capture {
        capture_id,
        source: req.source,
        source_uri: req.source_uri,
        captured_at,
        media_type: req.media_type,
        title: req.title,
        sha256_original,
        sha256_text,
        status: status.to_string(),
    })
}

pub fn get_cursor(cfg: &Config, source: &str) -> Result<Option<String>> {
    let conn = open_state(cfg)?;
    Ok(conn.query_row("select cursor from source_cursors where source = ?1", [source], |r| r.get(0)).optional()?)
}

pub fn set_cursor(cfg: &Config, source: &str, cursor: &str) -> Result<()> {
    let conn = open_state(cfg)?;
    conn.execute(
        "insert into source_cursors (source, cursor, updated_at) values (?1, ?2, ?3) \
         on conflict(source) do update set cursor = excluded.cursor, updated_at = excluded.updated_at",
        params![source, cursor, crate::clock::now().to_rfc3339()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        (dir, cfg)
    }

    fn req(uri: &str, text: &str) -> PutCapture {
        PutCapture {
            source: "manual".into(),
            source_uri: uri.into(),
            source_cursor: None,
            media_type: "text/plain".into(),
            title: Some("t".into()),
            original: text.as_bytes().to_vec(),
            text: text.into(),
            captured_at: None,
        }
    }

    #[test]
    fn put_get_and_list() {
        let (_dir, cfg) = cfg();
        let cap = put_capture(&cfg, req("file:/a.txt", "hello there")).unwrap();
        assert_eq!(cap.status, "new");
        let fetched = get(&cfg, &cap.capture_id).unwrap().unwrap();
        assert_eq!(fetched.source_uri, "file:/a.txt");
        assert_eq!(text(&cfg, &fetched).unwrap(), "hello there");

        let prefix = &cap.capture_id[..6];
        assert_eq!(get(&cfg, prefix).unwrap().unwrap().capture_id, cap.capture_id);

        let rows = list(&cfg, None, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        let rows = list(&cfg, Some("curated"), None, 10).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn dedup_by_source_uri_and_original_hash() {
        let (_dir, cfg) = cfg();
        let a = put_capture(&cfg, req("file:/a.txt", "same content")).unwrap();
        let b = put_capture(&cfg, req("file:/a.txt", "same content")).unwrap();
        assert_eq!(a.capture_id, b.capture_id);
        assert_eq!(list(&cfg, None, None, 10).unwrap().len(), 1);
    }

    #[test]
    fn secret_hit_quarantines() {
        let (_dir, cfg) = cfg();
        let token = format!("ghp_{}", "a".repeat(36));
        let cap = put_capture(&cfg, req("file:/token.txt", &token)).unwrap();
        assert_eq!(cap.status, "quarantined");
        assert_eq!(text(&cfg, &cap).unwrap(), token);
        // the blob must not have landed in the normal store.
        assert!(!blob::open_store(&cfg).unwrap().exists(&cap.sha256_text).unwrap());
    }

    #[test]
    fn set_status_updates_row() {
        let (_dir, cfg) = cfg();
        let cap = put_capture(&cfg, req("file:/a.txt", "hello")).unwrap();
        set_status(&cfg, &cap.capture_id, "curated", Some("run-1")).unwrap();
        let fetched = get(&cfg, &cap.capture_id).unwrap().unwrap();
        assert_eq!(fetched.status, "curated");
    }

    #[test]
    fn cursor_round_trips() {
        let (_dir, cfg) = cfg();
        assert_eq!(get_cursor(&cfg, "files").unwrap(), None);
        set_cursor(&cfg, "files", "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(get_cursor(&cfg, "files").unwrap().as_deref(), Some("2026-01-01T00:00:00Z"));
        set_cursor(&cfg, "files", "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(get_cursor(&cfg, "files").unwrap().as_deref(), Some("2026-01-02T00:00:00Z"));
    }
}
