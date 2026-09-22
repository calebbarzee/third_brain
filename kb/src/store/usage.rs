//! usage events: one row per document per day, in `<state>/state.sqlite` (design.md §17.3,
//! §17.4: "`reads_90d`, `cites_90d` | `read` and `cite` events written by `answer` into the
//! `usage` table of the state database, aggregated nightly"). `answer` and `search --record`
//! write here; `index::build` reads the last 90 days into `graph_metrics`.

use super::catalog::open_state;
use crate::config::Config;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::collections::HashMap;

const SCHEMA: &str = "create table if not exists usage (
    doc_id text not null,
    day text not null,
    reads integer not null default 0,
    cites integer not null default 0,
    primary key (doc_id, day)
);";

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).context("creating usage schema")
}

/// today's date (utc) as `YYYY-MM-DD`, the `usage.day` key.
fn today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

/// record one read for each of `doc_ids` (duplicates in the slice count once each, matching "one
/// read per returned document per query" -- a document appearing twice in one result list is
/// unusual but not this function's concern to dedup).
pub fn record_reads(cfg: &Config, doc_ids: &[String]) -> Result<()> {
    if doc_ids.is_empty() {
        return Ok(());
    }
    let conn = open_state(cfg)?;
    ensure_schema(&conn)?;
    let day = today();
    for doc_id in doc_ids {
        bump(&conn, doc_id, &day, "reads")?;
    }
    Ok(())
}

/// record one cite for each of `doc_ids` (one per cited document, design.md's "cite" event).
pub fn record_cites(cfg: &Config, doc_ids: &[String]) -> Result<()> {
    if doc_ids.is_empty() {
        return Ok(());
    }
    let conn = open_state(cfg)?;
    ensure_schema(&conn)?;
    let day = today();
    for doc_id in doc_ids {
        bump(&conn, doc_id, &day, "cites")?;
    }
    Ok(())
}

fn bump(conn: &Connection, doc_id: &str, day: &str, column: &str) -> Result<()> {
    let sql = format!(
        "insert into usage (doc_id, day, {column}) values (?1, ?2, 1)
         on conflict(doc_id, day) do update set {column} = {column} + 1"
    );
    conn.execute(&sql, params![doc_id, day])?;
    Ok(())
}

/// whether the usage table has ever received a row, and each document's summed reads/cites over
/// the 90 days before `now`. the first element is design.md §17.4's "usage data absent" switch:
/// `false` means every document's weight stays redistributed to the other three metrics (the
/// step-1 behavior); `true` means every canon document gets a real (possibly zero) count.
#[allow(clippy::type_complexity)]
pub fn aggregate_90d(cfg: &Config, now: DateTime<Utc>) -> Result<(bool, HashMap<String, (i64, i64)>)> {
    let conn = open_state(cfg)?;
    ensure_schema(&conn)?;
    let any_rows: i64 = conn.query_row("select count(*) from usage", [], |r| r.get(0))?;
    if any_rows == 0 {
        return Ok((false, HashMap::new()));
    }
    let cutoff = (now - chrono::Duration::days(90)).format("%Y-%m-%d").to_string();
    let mut stmt = conn.prepare("select doc_id, sum(reads), sum(cites) from usage where day >= ?1 group by doc_id")?;
    let rows = stmt.query_map(params![cutoff], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (doc_id, reads, cites) = row?;
        out.insert(doc_id, (reads, cites));
    }
    Ok((true, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        (dir, cfg)
    }

    #[test]
    fn absent_until_first_write() {
        let (_dir, cfg) = cfg();
        let (present, agg) = aggregate_90d(&cfg, Utc::now()).unwrap();
        assert!(!present);
        assert!(agg.is_empty());
    }

    #[test]
    fn records_and_aggregates_reads_and_cites() {
        let (_dir, cfg) = cfg();
        record_reads(&cfg, &["d1".to_string(), "d2".to_string()]).unwrap();
        record_reads(&cfg, &["d1".to_string()]).unwrap();
        record_cites(&cfg, &["d1".to_string()]).unwrap();

        let (present, agg) = aggregate_90d(&cfg, Utc::now()).unwrap();
        assert!(present);
        assert_eq!(agg.get("d1"), Some(&(2, 1)));
        assert_eq!(agg.get("d2"), Some(&(1, 0)));
        assert_eq!(agg.get("d3"), None, "a document with no usage rows is simply absent from the map");
    }

    #[test]
    fn usage_outside_90_days_is_excluded() {
        let (_dir, cfg) = cfg();
        let conn = open_state(&cfg).unwrap();
        ensure_schema(&conn).unwrap();
        conn.execute("insert into usage (doc_id, day, reads, cites) values ('old', '2000-01-01', 5, 5)", []).unwrap();
        conn.execute("insert into usage (doc_id, day, reads, cites) values ('recent', ?1, 3, 1)", params![today()]).unwrap();
        let (present, agg) = aggregate_90d(&cfg, Utc::now()).unwrap();
        assert!(present);
        assert_eq!(agg.get("old"), None);
        assert_eq!(agg.get("recent"), Some(&(3, 1)));
    }
}
