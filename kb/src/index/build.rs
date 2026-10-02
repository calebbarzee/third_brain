//! index build: parse every `docs/**/*.md` at one commit, chunk, resolve refs, derive churn and
//! contributors from git history, compute the reference graph and criticality, and write one sqlite
//! artifact. design.md §17.1; step 1 is always a full build (no base/incremental selection yet).
use crate::config::Config;
use crate::git;
use crate::index::gitlog::{self, CommitInfo};
use crate::index::graph;
use crate::index::criticality;
use crate::index::prefix;
use crate::index::schema::create_schema;
use crate::index::vectors;
use crate::model::{self, Frontmatter, InlineLink, Section, Status};
use crate::provider::{self, Provider};
use crate::store::catalog::{self, Capture};
use crate::store::usage;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset};
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// read groups for a capture's `documents` row: captures carry no `[access]` of their own in this
/// build, so every capture gets the catalog's own default (design.md §9's `access_json` default).
const CAPTURE_READ_GROUPS: &str = "[\"*\"]";

/// summary returned by `build`. `already_built` is true when the target commit's artifact already
/// existed (or was created by a concurrent build that won the rename race); no work was repeated.
#[derive(Debug, Clone)]
pub struct BuildReport {
    pub commit: String,
    pub artifact_path: PathBuf,
    pub already_built: bool,
    pub document_count: usize,
    pub canon_count: usize,
    pub capture_count: usize,
    pub chunk_count: usize,
    pub finding_count: usize,
    pub maintenance_enabled: bool,
    /// whether `chunk_vectors` was populated this build (an `embed`-role provider was configured
    /// and supports it; design.md §17.1, §19).
    pub vectors_enabled: bool,
    /// rows in `chunk_vectors` (0 when `vectors_enabled` is false).
    pub vector_count: usize,
    /// chunks whose prefix was computed fresh this build (a provider call, or the deterministic
    /// fallback with no `contextualize` provider configured), as opposed to served from
    /// `prefix_cache`.
    pub prefixes_generated: usize,
}

/// a resolved edge from a document to a uri, as stored in the `refs` table.
#[derive(Debug, Clone, Serialize)]
pub struct RefRow {
    pub from_id: String,
    pub rel: String,
    pub uri: String,
    pub scheme: Option<String>,
    pub resolved_id: Option<String>,
    pub target_sha256: Option<String>,
    pub origin: String,
    pub line: Option<u32>,
    pub drift_state: Option<String>,
    pub drift_detail: Option<String>,
}

fn index_dir(cfg: &Config) -> PathBuf {
    cfg.root.join(".kb").join("index")
}

/// build (or reuse) the artifact for `target` (defaults to `HEAD`), using whatever `contextualize`
/// and `embed` providers are configured in `kb.toml`. exactly `build_with_providers(cfg, target,
/// None, None)`.
pub fn build(cfg: &Config, target: Option<&str>) -> Result<BuildReport> {
    build_with_providers(cfg, target, None, None)
}

/// `build`, with explicit provider overrides for the `contextualize` and `embed` roles: `Some`
/// bypasses `kb.toml` entirely for that role (used by tests, with a fake provider that never
/// touches the network); `None` falls back to `provider::provider_for_role`, same as `build`. the
/// final rename onto `<commit>.sqlite` happens through `fs::hard_link`, which is atomic and fails
/// with `AlreadyExists` when another build already finished first, so two concurrent builds of
/// one commit are harmless and only one artifact ever survives.
pub fn build_with_providers(
    cfg: &Config,
    target: Option<&str>,
    contextualize: Option<&dyn Provider>,
    embed: Option<&dyn Provider>,
) -> Result<BuildReport> {
    let root = &cfg.root;
    let target_rev = target.unwrap_or("HEAD");
    let commit = git::rev_parse(root, target_rev).with_context(|| format!("resolving {target_rev}"))?;
    if commit.is_empty() {
        bail!("could not resolve `{target_rev}` to a commit");
    }
    let dir = index_dir(cfg);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let final_path = dir.join(format!("{commit}.sqlite"));
    if final_path.is_file() {
        return quick_report(&final_path, &commit);
    }

    // one build per commit at a time: the tick, a post-merge hook, and a manual build can all
    // fire for the same commit within seconds. the lock names the pid so a dead holder is cleared.
    let _lock = BuildLock::acquire(&dir, &commit)?;
    remove_stale_tmp(&dir);
    let tmp_path = dir.join(format!("{commit}.sqlite.tmp-{}", std::process::id()));
    if tmp_path.exists() {
        std::fs::remove_file(&tmp_path).ok();
    }
    let mut report = build_into(cfg, &commit, &tmp_path, contextualize, embed)?;

    match std::fs::hard_link(&tmp_path, &final_path) {
        Ok(()) => {
            std::fs::remove_file(&tmp_path).ok();
            apply_retention(&dir)?;
            report.artifact_path = final_path;
            Ok(report)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp_path).ok();
            quick_report(&final_path, &commit)
        }
        Err(e) => Err(e).context("linking the built index into place"),
    }
}

/// rebuild `target`'s artifact (default `HEAD`) even when `<commit>.sqlite` already exists,
/// writing it to `<commit>-<n>.sqlite` (n incrementing from 1, skipping names already taken)
/// instead of touching the existing immutable artifact. captures are read fresh at every build
/// (design.md §17.1's "changed set" stage), so a plain `kb index build` on the same commit is a
/// no-op (`quick_report`, above) and a newly landed capture waits for the next `main` commit; this
/// is the escape hatch for `kb index build --refresh-captures` when only captures changed.
/// `newest_artifact` sorts by file mtime regardless of name, so the refreshed file becomes current
/// the moment it lands.
pub fn refresh_captures(cfg: &Config, target: Option<&str>) -> Result<BuildReport> {
    let root = &cfg.root;
    let target_rev = target.unwrap_or("HEAD");
    let commit = git::rev_parse(root, target_rev).with_context(|| format!("resolving {target_rev}"))?;
    if commit.is_empty() {
        bail!("could not resolve `{target_rev}` to a commit");
    }
    let dir = index_dir(cfg);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let tmp_path = dir.join(format!("{commit}-refresh.sqlite.tmp-{}", std::process::id()));
    if tmp_path.exists() {
        std::fs::remove_file(&tmp_path).ok();
    }
    let mut report = build_into(cfg, &commit, &tmp_path, None, None)?;

    let mut n = 1u32;
    loop {
        let final_path = dir.join(format!("{commit}-{n}.sqlite"));
        match std::fs::hard_link(&tmp_path, &final_path) {
            Ok(()) => {
                std::fs::remove_file(&tmp_path).ok();
                apply_retention(&dir)?;
                report.artifact_path = final_path;
                return Ok(report);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
                continue;
            }
            Err(e) => return Err(e).context("linking the refreshed index into place"),
        }
    }
}

/// the most recently built artifact, by file mtime, or `None` when `.kb/index/` has none. this
/// also picks up `<commit>-<n>.sqlite` refresh artifacts (see `refresh_captures`): any `.sqlite`
/// file counts, so the newest one by mtime wins regardless of which naming scheme produced it.
pub fn newest_artifact(cfg: &Config) -> Option<PathBuf> {
    let dir = index_dir(cfg);
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".sqlite") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
            best = Some((path, mtime));
        }
    }
    best.map(|(p, _)| p)
}

/// open the newest artifact read-only.
pub fn open_newest(cfg: &Config) -> Result<Connection> {
    super::vectors::register_extension();
    let path = newest_artifact(cfg).context("no index artifact found; run `kb index build` first")?;
    Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| format!("opening {}", path.display()))
}


/// advisory per-commit build lock: a file holding the builder's pid, created exclusively.
struct BuildLock {
    path: std::path::PathBuf,
}

impl BuildLock {
    fn acquire(dir: &Path, commit: &str) -> Result<BuildLock> {
        let path = dir.join(format!("{commit}.lock"));
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(BuildLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u32>().ok());
                    match holder {
                        Some(pid) if pid_alive(pid) => bail!("an index build for {commit} is already running (pid {pid})"),
                        _ => {
                            // stale lock from a dead process
                            std::fs::remove_file(&path).ok();
                        }
                    }
                }
                Err(e) => return Err(e).context("creating the index build lock"),
            }
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// delete `*.sqlite.tmp-<pid>` files whose pid is no longer alive (a killed or crashed build).
fn remove_stale_tmp(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some((_, pid)) = name.rsplit_once(".tmp-")
            && let Ok(pid) = pid.parse::<u32>()
            && !pid_alive(pid)
        {
            std::fs::remove_file(e.path()).ok();
        }
    }
}

fn quick_report(path: &Path, commit: &str) -> Result<BuildReport> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let document_count: i64 = conn.query_row("select count(*) from documents where kind = 'document'", [], |r| r.get(0))?;
    let canon_count: i64 = conn.query_row("select count(*) from documents where status = 'canon'", [], |r| r.get(0))?;
    let capture_count: i64 = conn.query_row("select count(*) from documents where kind = 'capture'", [], |r| r.get(0))?;
    let chunk_count: i64 = conn.query_row("select count(*) from chunks", [], |r| r.get(0))?;
    let finding_count: i64 = conn.query_row("select count(*) from findings", [], |r| r.get(0))?;
    let maintenance_enabled: String =
        conn.query_row("select value from meta where key = 'maintenance_enabled'", [], |r| r.get(0)).unwrap_or_else(|_| "1".to_string());
    let vectors_enabled: String = conn.query_row("select value from meta where key = 'vectors_enabled'", [], |r| r.get(0)).unwrap_or_else(|_| "0".to_string());
    let vector_count: i64 = conn.query_row("select count(*) from sqlite_master where type = 'table' and name = 'chunk_vectors'", [], |r| r.get(0))
        .ok()
        .filter(|n: &i64| *n > 0)
        .and_then(|_| conn.query_row("select count(*) from chunk_vectors", [], |r| r.get(0)).ok())
        .unwrap_or(0);
    let prefixes_generated: i64 =
        conn.query_row("select value from meta where key = 'prefixes_generated'", [], |r| r.get(0)).ok().and_then(|s: String| s.parse().ok()).unwrap_or(0);
    Ok(BuildReport {
        commit: commit.to_string(),
        artifact_path: path.to_path_buf(),
        already_built: true,
        document_count: document_count as usize,
        canon_count: canon_count as usize,
        capture_count: capture_count as usize,
        chunk_count: chunk_count as usize,
        finding_count: finding_count as usize,
        maintenance_enabled: maintenance_enabled == "1",
        vectors_enabled: vectors_enabled == "1",
        vector_count: vector_count as usize,
        prefixes_generated: prefixes_generated as usize,
    })
}

/// one document parsed from git at the target commit.
struct ParsedDoc {
    id: String,
    path: String,
    frontmatter: Frontmatter,
    body: String,
    sections: Vec<Section>,
    links: Vec<InlineLink>,
    content_hash: String,
    word_count: usize,
}

fn parse_docs_at(root: &Path, commit: &str) -> Result<Vec<ParsedDoc>> {
    let paths = git::ls_tree(root, commit, "docs")?;
    let mut docs = Vec::new();
    for path in paths {
        if !path.ends_with(".md") {
            continue;
        }
        let Some(id) = Path::new(&path).file_stem().and_then(|s| s.to_str()).filter(|s| model::is_ulid(s)) else {
            continue;
        };
        let bytes = git::show_file(root, commit, &path)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let (fm_text, body) = crate::parse::split_frontmatter(&text)?;
        let frontmatter = match fm_text {
            Some(t) => crate::parse::frontmatter::parse_frontmatter(&t)?,
            None => Frontmatter::default(),
        };
        let bp = crate::parse::parse_body(&body);
        let content_hash = crate::parse::frontmatter::sha256_hex(text.as_bytes());
        docs.push(ParsedDoc {
            id: id.to_string(),
            path,
            frontmatter,
            body,
            sections: bp.sections,
            links: bp.links,
            content_hash,
            word_count: bp.word_count,
        });
    }
    Ok(docs)
}

/// the end of one chunk's text: the section's own range, cut short at the first child section so a
/// section with children gets only its own text (design.md §7 addresses the full range including
/// children; the chunk boundary is deliberately narrower).
fn chunk_end(sections: &[Section], idx: usize) -> usize {
    let section = &sections[idx];
    match sections.get(idx + 1) {
        Some(next) if next.path.starts_with(&format!("{}/", section.path)) => next.start,
        _ => section.end,
    }
}

fn scheme_of(uri: &str) -> Option<String> {
    uri.split_once(':').map(|(s, _)| s.to_string())
}

/// drift state for a `kb:` reference. local schemes are checked at build time (design.md §8); other
/// schemes are left `ok` here pending the reconciliation tick.
fn drift_for_kb(resolved_id: Option<&str>, frag: Option<&str>, by_id: &HashMap<String, &ParsedDoc>, tombstones: &HashSet<String>) -> (&'static str, Option<String>) {
    let Some(rid) = resolved_id else {
        return ("missing", Some("uri is not a valid kb: reference".to_string()));
    };
    if tombstones.contains(rid) {
        return ("tombstoned", None);
    }
    let Some(target) = by_id.get(rid) else {
        return ("missing", None);
    };
    if let Some(f) = frag
        && !target.sections.iter().any(|s| s.path == f)
    {
        return ("heading_missing", Some(format!("no section `{f}` in {rid}")));
    }
    match target.frontmatter.status {
        Some(Status::Superseded) | Some(Status::Archived) => ("target_superseded", None),
        _ => ("ok", None),
    }
}

fn kb_ref(uri: &str) -> (Option<String>, Option<String>) {
    match model::parse_kb_uri(uri) {
        Some((id, frag)) => (Some(id), frag),
        None => (None, None),
    }
}

/// the catalog fields a `capture:` ref needs to check drift, keyed by capture id, over every
/// catalog row regardless of status or redaction: a ref may point at a quarantined or redacted
/// capture, and that is itself the finding (design.md §8).
struct CaptureDrift {
    status: String,
    sha256_text: String,
}

fn capture_drift_map(conn: &Connection) -> Result<HashMap<String, CaptureDrift>> {
    let mut stmt = conn.prepare("select capture_id, status, sha256_text from captures")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, status, sha256_text) = row?;
        map.insert(id, CaptureDrift { status, sha256_text });
    }
    Ok(map)
}

/// drift state for a `capture:<ulid>` reference (design.md §8): `missing` when the catalog has no
/// such row, `quarantined` when it does but is quarantined, `hash_changed` when the ref carries a
/// `sha256` that no longer matches `sha256_text`, `ok` otherwise.
fn drift_for_capture(capture_id: &str, captures: &HashMap<String, CaptureDrift>, ref_sha256: Option<&str>) -> (&'static str, Option<String>) {
    let Some(cap) = captures.get(capture_id) else {
        return ("missing", None);
    };
    if cap.status == "quarantined" {
        return ("quarantined", None);
    }
    if let Some(sha) = ref_sha256
        && sha != cap.sha256_text
    {
        return ("hash_changed", Some(format!("ref sha256 {sha} != current {}", cap.sha256_text)));
    }
    ("ok", None)
}

fn build_refs(docs: &[ParsedDoc], by_id: &HashMap<String, &ParsedDoc>, tombstones: &HashSet<String>, captures: &HashMap<String, CaptureDrift>) -> Vec<RefRow> {
    let mut refs = Vec::new();
    for doc in docs {
        for r in &doc.frontmatter.refs {
            let scheme = scheme_of(&r.uri);
            let is_kb = scheme.as_deref() == Some("kb");
            let is_capture = scheme.as_deref() == Some("capture");
            let target_sha256 = if scheme.as_deref() == Some("blob") { r.uri.strip_prefix("blob:sha256:").map(str::to_string) } else { r.sha256.clone() };
            let (resolved_id, drift_state, drift_detail) = if is_kb {
                let (resolved_id, frag) = kb_ref(&r.uri);
                let (state, detail) = drift_for_kb(resolved_id.as_deref(), frag.as_deref(), by_id, tombstones);
                (resolved_id, state, detail)
            } else if is_capture {
                let capture_id = r.uri.strip_prefix("capture:").unwrap_or("").to_string();
                let (state, detail) = drift_for_capture(&capture_id, captures, r.sha256.as_deref());
                let resolved_id = if state == "missing" { None } else { Some(capture_id) };
                (resolved_id, state, detail)
            } else {
                (None, "ok", None)
            };
            refs.push(RefRow {
                from_id: doc.id.clone(),
                rel: r.rel.clone(),
                uri: r.uri.clone(),
                scheme,
                resolved_id,
                target_sha256,
                origin: "frontmatter".to_string(),
                line: None,
                drift_state: Some(drift_state.to_string()),
                drift_detail,
            });
        }
        if let Some(p) = &doc.frontmatter.parent {
            let scheme = scheme_of(p);
            let is_kb = scheme.as_deref() == Some("kb");
            let (resolved_id, frag) = if is_kb { kb_ref(p) } else { (None, None) };
            let (drift_state, drift_detail) = if is_kb { drift_for_kb(resolved_id.as_deref(), frag.as_deref(), by_id, tombstones) } else { ("ok", None) };
            refs.push(RefRow {
                from_id: doc.id.clone(),
                rel: "parent".to_string(),
                uri: p.clone(),
                scheme,
                resolved_id,
                target_sha256: None,
                origin: "frontmatter".to_string(),
                line: None,
                drift_state: Some(drift_state.to_string()),
                drift_detail,
            });
        }
        for l in &doc.links {
            if l.uri.starts_with("kb:") {
                let (resolved_id, frag) = kb_ref(&l.uri);
                let (drift_state, drift_detail) = drift_for_kb(resolved_id.as_deref(), frag.as_deref(), by_id, tombstones);
                refs.push(RefRow {
                    from_id: doc.id.clone(),
                    rel: "related".to_string(),
                    uri: l.uri.clone(),
                    scheme: Some("kb".to_string()),
                    resolved_id,
                    target_sha256: None,
                    origin: "inline".to_string(),
                    line: Some(l.line as u32),
                    drift_state: Some(drift_state.to_string()),
                    drift_detail,
                });
            } else if let Some(hex) = l.uri.strip_prefix("blob:sha256:") {
                refs.push(RefRow {
                    from_id: doc.id.clone(),
                    rel: "asset".to_string(),
                    uri: l.uri.clone(),
                    scheme: Some("blob".to_string()),
                    resolved_id: None,
                    target_sha256: Some(hex.to_string()),
                    origin: "inline".to_string(),
                    line: Some(l.line as u32),
                    drift_state: Some("ok".to_string()),
                    drift_detail: None,
                });
            }
        }
    }
    refs
}

struct TombstoneRow {
    id: String,
    title: Option<String>,
    path: String,
    deleted_at: String,
    deleting_commit: String,
    run_id: Option<String>,
    superseded_by: Option<String>,
}

fn build_tombstones(root: &Path, commit: &str, by_id: &HashMap<String, &ParsedDoc>) -> Result<Vec<TombstoneRow>> {
    let raw = git::log_deleted_docs(root, commit)?;
    let events = gitlog::parse_deleted_log(&raw);
    let mut out: HashMap<String, TombstoneRow> = HashMap::new();
    for ev in &events {
        for path in &ev.paths {
            if !path.ends_with(".md") {
                continue;
            }
            let Some(id) = Path::new(path).file_stem().and_then(|s| s.to_str()).filter(|s| model::is_ulid(s)) else {
                continue;
            };
            if by_id.contains_key(id) || out.contains_key(id) {
                // present in the target tree (recreated later) or already recorded from a more
                // recent deletion (git log iterates newest-first, so the first hit wins).
                continue;
            }
            let mut title = None;
            let mut superseded_by = None;
            if let Ok(bytes) = git::show_file(root, &format!("{}^", ev.sha), path) {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Ok((Some(fm_text), _)) = crate::parse::split_frontmatter(&text)
                    && let Ok(fm) = crate::parse::frontmatter::parse_frontmatter(&fm_text)
                {
                    title = fm.title;
                    superseded_by = fm.superseded_by;
                }
            }
            out.insert(
                id.to_string(),
                TombstoneRow {
                    id: id.to_string(),
                    title,
                    path: path.clone(),
                    deleted_at: ev.date.to_rfc3339(),
                    deleting_commit: ev.sha.clone(),
                    run_id: ev.run_trailer.clone(),
                    superseded_by,
                },
            );
        }
    }
    Ok(out.into_values().collect())
}

#[derive(Default, Clone)]
struct DocChurn {
    churn_30d: i64,
    churn_90d: i64,
    lines_90d: f64,
    agent_90d: i64,
    total_90d: i64,
    last_updated: Option<DateTime<FixedOffset>>,
    last_human_commit: Option<String>,
    last_human_commit_date: Option<DateTime<FixedOffset>>,
    last_run: Option<String>,
    last_run_date: Option<DateTime<FixedOffset>>,
}

struct ContribAgg {
    commits: i64,
    first_sha: String,
    first_date: DateTime<FixedOffset>,
    last_sha: String,
    last_date: DateTime<FixedOffset>,
}

fn bump_contrib(map: &mut HashMap<(String, String, String), ContribAgg>, doc_id: &str, actor: &str, role: &str, sha: &str, date: DateTime<FixedOffset>) {
    let key = (doc_id.to_string(), actor.to_string(), role.to_string());
    let entry = map.entry(key).or_insert_with(|| ContribAgg { commits: 0, first_sha: sha.to_string(), first_date: date, last_sha: sha.to_string(), last_date: date });
    entry.commits += 1;
    if date < entry.first_date {
        entry.first_date = date;
        entry.first_sha = sha.to_string();
    }
    if date > entry.last_date {
        entry.last_date = date;
        entry.last_sha = sha.to_string();
    }
}

/// human author, or the principal's configured owner for an agent commit (`agent:<skill>@<principal>`
/// with a `Run:` trailer), or the raw author when the principal is not in `groups.toml`. a human
/// author is normalized to their `groups.toml` principal name by email when known, so the same
/// person committing under slightly different `user.name` values still aggregates to one actor.
fn responsible_for(c: &CommitInfo, cfg: &Config) -> String {
    if c.is_agent() {
        if let Some(principal) = c.agent_principal()
            && let Some(owner) = cfg.groups.principals.get(principal).and_then(|p| p.owner.clone())
        {
            return owner;
        }
        return c.author_name.clone();
    }
    cfg.principal_for_email(&c.author_email).unwrap_or_else(|| c.author_name.clone())
}

struct ChurnAndContrib {
    churn: HashMap<String, DocChurn>,
    contrib: HashMap<(String, String, String), ContribAgg>,
}

fn derive_churn_and_contributors(root: &Path, commit: &str, now: DateTime<FixedOffset>, cfg: &Config) -> Result<ChurnAndContrib> {
    let raw = git::log_docs_numstat(root, commit)?;
    let commits = gitlog::parse_numstat_log(&raw);
    let window30 = now - chrono::Duration::days(30);
    let window90 = now - chrono::Duration::days(90);
    let mut churn: HashMap<String, DocChurn> = HashMap::new();
    let mut contrib: HashMap<(String, String, String), ContribAgg> = HashMap::new();

    for c in &commits {
        let mut touched: HashSet<String> = HashSet::new();
        for f in &c.files {
            if !f.path.ends_with(".md") {
                continue;
            }
            let Some(id) = Path::new(&f.path).file_stem().and_then(|s| s.to_str()).filter(|s| model::is_ulid(s)) else {
                continue;
            };
            touched.insert(id.to_string());
            if c.date >= window90 {
                let entry = churn.entry(id.to_string()).or_default();
                entry.lines_90d += (f.added.unwrap_or(0) + f.deleted.unwrap_or(0)) as f64;
            }
        }
        let responsible = responsible_for(c, cfg);
        for id in &touched {
            let entry = churn.entry(id.clone()).or_default();
            if c.date >= window30 {
                entry.churn_30d += 1;
            }
            if c.date >= window90 {
                entry.churn_90d += 1;
                entry.total_90d += 1;
                if c.is_agent() {
                    entry.agent_90d += 1;
                }
            }
            if entry.last_updated.map(|d| c.date > d).unwrap_or(true) {
                entry.last_updated = Some(c.date);
            }
            if !c.is_agent() && entry.last_human_commit_date.map(|d| c.date > d).unwrap_or(true) {
                entry.last_human_commit = Some(c.sha.clone());
                entry.last_human_commit_date = Some(c.date);
            }
            if let Some(run) = &c.run_trailer
                && entry.last_run_date.map(|d| c.date > d).unwrap_or(true)
            {
                entry.last_run = Some(run.clone());
                entry.last_run_date = Some(c.date);
            }
            bump_contrib(&mut contrib, id, &c.author_name, "author", &c.sha, c.date);
            for co in &c.co_authors {
                bump_contrib(&mut contrib, id, co, "co-author", &c.sha, c.date);
            }
            bump_contrib(&mut contrib, id, &responsible, "responsible", &c.sha, c.date);
        }
    }
    Ok(ChurnAndContrib { churn, contrib })
}

/// millisecond timestamp encoded in a ulid, as an rfc3339 date.
fn ulid_created(id: &str) -> Option<String> {
    let u = ulid::Ulid::from_string(id).ok()?;
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(u.timestamp_ms() as i64).map(|d| d.to_rfc3339())
}

/// the read groups for a document: `access.read` when set, else the documented default of `["*"]`
/// (model.rs's `default_read`, which only applies during toml deserialization, not to `::default()`).
fn read_groups_for(fm: &Frontmatter) -> Vec<String> {
    fm.access.as_ref().map(|a| a.read.clone()).unwrap_or_else(|| vec!["*".to_string()])
}

/// one catalog row read at build time, plus its normalized text and parsed sections. catalog rows
/// are read fresh at every build rather than diffed against a base (design.md §17.1), so this is
/// always a full re-read; a real incremental builder would cache this like it caches document
/// chunks.
struct ParsedCapture {
    capture: Capture,
    text: String,
    sections: Vec<Section>,
    word_count: usize,
}

/// every catalog row eligible for indexing: `status != 'quarantined'` and `redacted = 0`
/// (design.md §9, §17.2). a capture failing either check still exists in the catalog and can still
/// be the target of a `capture:` ref -- see `capture_drift_map`, which reads every row unfiltered.
fn load_captures(conn: &Connection, cfg: &Config) -> Result<Vec<ParsedCapture>> {
    let mut stmt = conn.prepare(
        "select capture_id, source, source_uri, captured_at, media_type, title, sha256_original, sha256_text, status \
         from captures where status != 'quarantined' and redacted = 0 order by capture_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Capture {
            capture_id: r.get(0)?,
            source: r.get(1)?,
            source_uri: r.get(2)?,
            captured_at: r.get(3)?,
            media_type: r.get(4)?,
            title: r.get(5)?,
            sha256_original: r.get(6)?,
            sha256_text: r.get(7)?,
            status: r.get(8)?,
        })
    })?;
    let captures: Vec<Capture> = rows.collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::with_capacity(captures.len());
    for capture in captures {
        let text = catalog::text(cfg, &capture).with_context(|| format!("reading text blob for capture {}", capture.capture_id))?;
        let bp = crate::parse::parse_body(&text);
        out.push(ParsedCapture { capture, sections: bp.sections, word_count: bp.word_count, text });
    }
    Ok(out)
}

fn build_into(
    cfg: &Config,
    commit: &str,
    tmp_path: &Path,
    contextualize_override: Option<&dyn Provider>,
    embed_override: Option<&dyn Provider>,
) -> Result<BuildReport> {
    // register the sqlite-vec extension before any connection in this build is opened
    // (`sqlite3_auto_extension` only affects connections opened after the call); harmless to
    // call again on every build (`Once`-guarded).
    vectors::register_extension();
    // the newest existing artifact, captured before this build writes anything: its
    // embedding_cache and prefix_cache rows are copied forward below so a chunk whose hash is
    // unchanged since that build is not re-embedded or re-prefixed (design.md §17.1).
    let prev_artifact = newest_artifact(cfg);

    let root = &cfg.root;
    let docs = parse_docs_at(root, commit)?;
    let by_id: HashMap<String, &ParsedDoc> = docs.iter().map(|d| (d.id.clone(), d)).collect();
    let tombstone_rows = build_tombstones(root, commit, &by_id)?;
    let tombstone_ids: HashSet<String> = tombstone_rows.iter().map(|t| t.id.clone()).collect();

    // captures: read from the catalog at build time (not diffed against the base commit), chunked
    // like documents but tagged kind = 'capture' so the default candidate set never sees them
    // (design.md §9, invariant 9).
    let catalog_conn = catalog::open_state(cfg)?;
    let captures = load_captures(&catalog_conn, cfg)?;
    let capture_drift = capture_drift_map(&catalog_conn)?;
    drop(catalog_conn);

    // usage: reads and cites over the last 90 days, from `answer`/`search --record` events in the
    // state database (design.md §17.3, §17.4). `usage_present` is false until the first event is
    // ever written, which is when criticality::criticality_pct redistributes reads_90d/cites_90d's
    // weight to the other three metrics; once any usage row exists, every canon document gets a
    // real (possibly zero) count instead.
    let (usage_present, usage_by_doc) = usage::aggregate_90d(cfg, crate::clock::now())?;

    let refs = build_refs(&docs, &by_id, &tombstone_ids, &capture_drift);

    let now_str = git::committer_date(root, commit)?;
    let now = DateTime::parse_from_rfc3339(&now_str).with_context(|| format!("parsing committer date {now_str}"))?;
    let derived = derive_churn_and_contributors(root, commit, now, cfg)?;

    // graph metrics: in_canon (canon referrers, any rel but supersedes), out_degree (distinct kb
    // targets), pagerank (over kb edges among canon documents only).
    let canon_ids: Vec<String> = docs.iter().filter(|d| d.frontmatter.status == Some(Status::Canon)).map(|d| d.id.clone()).collect();
    let canon_set: HashSet<&str> = canon_ids.iter().map(String::as_str).collect();

    let mut in_canon: HashMap<String, HashSet<String>> = HashMap::new();
    let mut out_degree: HashMap<String, HashSet<String>> = HashMap::new();
    let mut pr_edges: HashMap<String, Vec<String>> = canon_ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for r in &refs {
        if r.scheme.as_deref() != Some("kb") {
            continue;
        }
        let Some(target) = &r.resolved_id else { continue };
        out_degree.entry(r.from_id.clone()).or_default().insert(target.clone());
        if r.rel != "supersedes" && canon_set.contains(r.from_id.as_str()) {
            in_canon.entry(target.clone()).or_default().insert(r.from_id.clone());
        }
        if canon_set.contains(r.from_id.as_str()) && canon_set.contains(target.as_str()) {
            pr_edges.get_mut(&r.from_id).unwrap().push(target.clone());
        }
    }
    let damping = cfg.manifest.weights.pagerank_damping;
    let pagerank = graph::pagerank(&canon_ids, &pr_edges, damping, 50);

    // criticality: percentile-rank every document against the canon population, then band.
    let canon_population = criticality::CanonPopulation {
        in_canon: canon_ids.iter().map(|id| in_canon.get(id).map(|s| s.len()).unwrap_or(0) as f64).collect(),
        in_code: canon_ids.iter().map(|_| 0.0).collect(),
        pagerank: canon_ids.iter().map(|id| *pagerank.get(id).unwrap_or(&0.0)).collect(),
        reads: canon_ids.iter().map(|id| usage_present.then(|| usage_by_doc.get(id).map(|(r, _)| *r as f64).unwrap_or(0.0))).collect(),
        cites: canon_ids.iter().map(|id| usage_present.then(|| usage_by_doc.get(id).map(|(_, c)| *c as f64).unwrap_or(0.0))).collect(),
    };
    let weights = criticality::Weights {
        in_canon: cfg.manifest.weights.in_canon,
        in_code: cfg.manifest.weights.in_code,
        pagerank: cfg.manifest.weights.pagerank,
        reads_90d: cfg.manifest.weights.reads_90d,
        cites_90d: cfg.manifest.weights.cites_90d,
    };
    let mut pct_by_id: HashMap<String, f64> = HashMap::new();
    for doc in &docs {
        let (reads_90d, cites_90d) = if usage_present {
            let (r, c) = usage_by_doc.get(&doc.id).copied().unwrap_or((0, 0));
            (Some(r as f64), Some(c as f64))
        } else {
            (None, None)
        };
        let metrics = criticality::RawMetrics {
            in_canon: in_canon.get(&doc.id).map(|s| s.len()).unwrap_or(0) as f64,
            in_code: 0.0,
            pagerank: *pagerank.get(&doc.id).unwrap_or(&0.0),
            reads_90d,
            cites_90d,
        };
        pct_by_id.insert(doc.id.clone(), criticality::criticality_pct(&metrics, &canon_population, &weights));
    }
    let maintenance_enabled = canon_ids.len() >= cfg.manifest.thresholds.min_corpus;
    let cutoff = criticality::band_cutoff(canon_ids.iter().map(|id| pct_by_id[id]).collect(), cfg.manifest.thresholds.criticality_band_pct);
    let band_for = |id: &str| -> &'static str {
        if !maintenance_enabled {
            "review"
        } else {
            criticality::band_for(pct_by_id[id], cutoff)
        }
    };

    // graph findings: kahn's algorithm over the hierarchy (parent) and succession (superseded_by)
    // graphs; any node left over after removing a topological order sits on a cycle.
    let all_ids: Vec<String> = docs.iter().map(|d| d.id.clone()).collect();
    let hierarchy_edges: Vec<(String, String)> = refs.iter().filter(|r| r.rel == "parent").filter_map(|r| r.resolved_id.clone().map(|t| (r.from_id.clone(), t))).collect();
    let succession_edges: Vec<(String, String)> =
        docs.iter().filter_map(|d| d.frontmatter.superseded_by.as_ref().and_then(|s| model::parse_kb_uri(s)).map(|(t, _)| (d.id.clone(), t))).collect();
    let hierarchy_cycle = graph::kahn_leftover(&all_ids, &hierarchy_edges);
    let succession_cycle = graph::kahn_leftover(&all_ids, &succession_edges);

    // write the artifact.
    if tmp_path.exists() {
        std::fs::remove_file(tmp_path)?;
    }
    let mut conn = Connection::open(tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
    create_schema(&conn)?;
    {
        let tx = conn.transaction()?;
        {
            let mut doc_stmt = tx.prepare(
                "insert into documents (id, kind, path, layer, title, status, owner, created, superseded_by, parent_id,
                    frontmatter, body, content_hash, read_groups, last_updated, last_human_commit, last_run,
                    churn_30d, churn_90d, churn_lines_90d, agent_share_90d, word_count, section_count,
                    criticality_pct, crit_band)
                 values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
            )?;
            let mut chunk_stmt = tx.prepare("insert into chunks (chunk_hash, doc_id, kind, status, heading_path, ordinal, text, prefix) values (?1,?2,?3,?4,?5,?6,?7,null)")?;
            for doc in &docs {
                let status = doc.frontmatter.status.map(|s| s.as_str().to_string()).unwrap_or_else(|| "draft".to_string());
                let churn = derived.churn.get(&doc.id);
                let lines = doc.body.lines().count().max(1) as f64;
                let churn_lines_90d = churn.map(|c| c.lines_90d / lines).unwrap_or(0.0);
                let agent_share_90d = churn.and_then(|c| if c.total_90d > 0 { Some(c.agent_90d as f64 / c.total_90d as f64) } else { None }).unwrap_or(0.0);
                let parent_id = doc.frontmatter.parent.as_ref().and_then(|p| model::parse_kb_uri(p)).map(|(id, _)| id);
                let frontmatter_json = serde_json::to_string(&doc.frontmatter)?;
                let read_groups = serde_json::to_string(&read_groups_for(&doc.frontmatter))?;
                doc_stmt.execute(params![
                    doc.id,
                    "document",
                    doc.path,
                    doc.frontmatter.layer.map(|l| l.as_str().to_string()),
                    doc.frontmatter.title,
                    status,
                    doc.frontmatter.owner,
                    ulid_created(&doc.id),
                    doc.frontmatter.superseded_by,
                    parent_id,
                    frontmatter_json,
                    doc.body,
                    doc.content_hash,
                    read_groups,
                    churn.and_then(|c| c.last_updated).map(|d| d.to_rfc3339()),
                    churn.and_then(|c| c.last_human_commit.clone()),
                    churn.and_then(|c| c.last_run.clone()),
                    churn.map(|c| c.churn_30d).unwrap_or(0),
                    churn.map(|c| c.churn_90d).unwrap_or(0),
                    churn_lines_90d,
                    agent_share_90d,
                    doc.word_count as i64,
                    doc.sections.len() as i64,
                    pct_by_id.get(&doc.id).copied(),
                    band_for(&doc.id),
                ])?;

                let mut ordinal = 0i64;
                for (idx, section) in doc.sections.iter().enumerate() {
                    let end = chunk_end(&doc.sections, idx);
                    if end <= section.start {
                        continue;
                    }
                    let text = &doc.body[section.start..end];
                    if text.trim().is_empty() {
                        continue;
                    }
                    let chunk_hash = crate::parse::frontmatter::sha256_hex(format!("{}\u{1}{}\u{1}{}", doc.id, section.path, text).as_bytes());
                    chunk_stmt.execute(params![chunk_hash, doc.id, "document", status, section.path, ordinal, text])?;
                    ordinal += 1;
                }
            }

            // captures: kind = 'capture', status = 'draft', no layer/owner/superseded_by/parent_id,
            // no churn or criticality (design.md §9, §17.2, §17.4). the chunk `kind` is what keeps
            // them out of the default candidate set, not the document `status`.
            let mut capture_stmt = tx.prepare(
                "insert into captures (id, adapter, source_uri, captured_at, original_sha256, original_media_type, normalized_text)
                 values (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for cap in &captures {
                let title = cap.capture.title.clone().unwrap_or_else(|| cap.capture.source_uri.clone());
                doc_stmt.execute(params![
                    cap.capture.capture_id,
                    "capture",
                    cap.capture.source_uri,
                    Option::<String>::None, // layer
                    title,
                    "draft",
                    Option::<String>::None, // owner
                    ulid_created(&cap.capture.capture_id),
                    Option::<String>::None, // superseded_by
                    Option::<String>::None, // parent_id
                    "{}",                   // frontmatter: captures have none
                    cap.text,
                    cap.capture.sha256_text,
                    CAPTURE_READ_GROUPS,
                    Option::<String>::None, // last_updated: not git-derived
                    Option::<String>::None, // last_human_commit
                    Option::<String>::None, // last_run
                    0i64,                   // churn_30d
                    0i64,                   // churn_90d
                    0.0f64,                 // churn_lines_90d
                    0.0f64,                 // agent_share_90d
                    cap.word_count as i64,
                    cap.sections.len() as i64,
                    Option::<f64>::None,  // criticality_pct: captures carry no graph metrics
                    Option::<String>::None, // crit_band
                ])?;

                let mut ordinal = 0i64;
                for (idx, section) in cap.sections.iter().enumerate() {
                    let end = chunk_end(&cap.sections, idx);
                    if end <= section.start {
                        continue;
                    }
                    let text = &cap.text[section.start..end];
                    if text.trim().is_empty() {
                        continue;
                    }
                    let chunk_hash =
                        crate::parse::frontmatter::sha256_hex(format!("{}\u{1}{}\u{1}{}", cap.capture.capture_id, section.path, text).as_bytes());
                    chunk_stmt.execute(params![chunk_hash, cap.capture.capture_id, "capture", "draft", section.path, ordinal, text])?;
                    ordinal += 1;
                }

                capture_stmt.execute(params![
                    cap.capture.capture_id,
                    cap.capture.source,
                    cap.capture.source_uri,
                    cap.capture.captured_at,
                    cap.capture.sha256_original,
                    cap.capture.media_type,
                    cap.text,
                ])?;
            }
        }
        {
            let mut ref_stmt = tx.prepare(
                "insert into refs (from_id, rel, uri, scheme, resolved_id, target_sha256, origin, line, drift_state, drift_detail)
                 values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for r in &refs {
                ref_stmt.execute(params![r.from_id, r.rel, r.uri, r.scheme, r.resolved_id, r.target_sha256, r.origin, r.line, r.drift_state, r.drift_detail])?;
            }
        }
        {
            let mut contrib_stmt =
                tx.prepare("insert into contributors (doc_id, actor, role, commits, first_commit, last_commit) values (?1,?2,?3,?4,?5,?6)")?;
            for ((doc_id, actor, role), agg) in &derived.contrib {
                contrib_stmt.execute(params![doc_id, actor, role, agg.commits, agg.first_sha, agg.last_sha])?;
            }
        }
        {
            let mut gm_stmt = tx.prepare(
                "insert into graph_metrics (doc_id, in_canon, in_code, out_degree, pagerank, reads_90d, cites_90d,
                    pct_in_canon, pct_in_code, pct_pagerank, pct_reads, pct_cites)
                 values (?1,?2,0,?3,?4,?5,?6,0,0,0,0,0)",
            )?;
            for doc in &docs {
                let (reads_90d, cites_90d) = usage_by_doc.get(&doc.id).copied().unwrap_or((0, 0));
                gm_stmt.execute(params![
                    doc.id,
                    in_canon.get(&doc.id).map(|s| s.len()).unwrap_or(0) as i64,
                    out_degree.get(&doc.id).map(|s| s.len()).unwrap_or(0) as i64,
                    *pagerank.get(&doc.id).unwrap_or(&0.0),
                    reads_90d,
                    cites_90d,
                ])?;
            }
        }
        {
            let mut ts_stmt =
                tx.prepare("insert into tombstones (id, title, path, deleted_at, deleting_commit, run_id, superseded_by) values (?1,?2,?3,?4,?5,?6,?7)")?;
            for t in &tombstone_rows {
                ts_stmt.execute(params![t.id, t.title, t.path, t.deleted_at, t.deleting_commit, t.run_id, t.superseded_by])?;
            }
        }
        {
            let mut finding_stmt = tx.prepare("insert into findings (kind, doc_ids, detail) values (?1,?2,?3)")?;
            if !hierarchy_cycle.is_empty() {
                finding_stmt.execute(params!["cycle", serde_json::to_string(&hierarchy_cycle)?, "hierarchy graph (parent edges)"])?;
            }
            if !succession_cycle.is_empty() {
                finding_stmt.execute(params!["cycle", serde_json::to_string(&succession_cycle)?, "succession graph (superseded_by edges)"])?;
            }
        }
        {
            let weights_json = serde_json::to_string(&cfg.manifest.weights)?;
            let mut meta_stmt = tx.prepare("insert into meta (key, value) values (?1,?2)")?;
            meta_stmt.execute(params!["commit", commit])?;
            meta_stmt.execute(params!["parser_version", cfg.manifest.parser_version.to_string()])?;
            meta_stmt.execute(params!["chunker_version", cfg.manifest.chunker_version.to_string()])?;
            meta_stmt.execute(params!["weights", weights_json])?;
            meta_stmt.execute(params!["canon_count", canon_ids.len().to_string()])?;
            meta_stmt.execute(params!["built_at", crate::clock::now().to_rfc3339()])?;
            meta_stmt.execute(params!["maintenance_enabled", if maintenance_enabled { "1" } else { "0" }])?;
        }
        tx.commit()?;
    }

    // prefixes and embeddings (design.md §17.1, §19's `contextualize` and `embed` roles): seed
    // the two caches forward from the newest previous artifact first, so an unchanged chunk hash
    // is neither re-prefixed nor re-embedded, then generate prefixes for every chunk (fts
    // benefits even with no provider, via the deterministic fallback) and embed only the
    // canon/draft document chunks that are eligible for the vector table.
    vectors::seed_caches_from_previous(&conn, prev_artifact.as_deref());
    let owned_contextualize;
    let contextualize_provider: Option<&dyn Provider> = match contextualize_override {
        Some(p) => Some(p),
        None => {
            owned_contextualize = provider::provider_for_role(cfg, "contextualize")?;
            owned_contextualize.as_deref()
        }
    };
    let prefix_report = prefix::build_prefixes(&conn, cfg, contextualize_provider)?;

    let owned_embed;
    let embed_provider: Option<&dyn Provider> = match embed_override {
        Some(p) => Some(p),
        None => {
            owned_embed = provider::provider_for_role(cfg, "embed")?;
            owned_embed.as_deref()
        }
    };
    let vector_report = vectors::build_vectors(&conn, cfg, embed_provider)?;
    {
        let mut meta_stmt = conn.prepare("insert into meta (key, value) values (?1,?2)")?;
        meta_stmt.execute(params!["prefix_model", prefix_report.model])?;
        meta_stmt.execute(params!["prefixes_generated", prefix_report.generated.to_string()])?;
        meta_stmt.execute(params!["vectors_enabled", if vector_report.enabled && vector_report.chunk_count > 0 { "1" } else { "0" }])?;
        meta_stmt.execute(params!["vector_backend", vector_report.backend])?;
        meta_stmt.execute(params!["vector_dim", vector_report.dim.to_string()])?;
        meta_stmt.execute(params!["vector_count", vector_report.chunk_count.to_string()])?;
        if let Some(model) = &vector_report.model {
            meta_stmt.execute(params!["embedding_model", model])?;
        }
        if let Some(reason) = &vector_report.skipped_reason {
            meta_stmt.execute(params!["vectors_skip_reason", reason])?;
        }
    }

    let ok: String = conn.query_row("pragma integrity_check", [], |r| r.get(0))?;
    if ok != "ok" {
        bail!("index integrity check failed: {ok}");
    }
    conn.execute_batch("vacuum;")?;
    drop(conn);

    let chunk_count = {
        let conn = Connection::open_with_flags(tmp_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.query_row("select count(*) from chunks", [], |r| r.get::<_, i64>(0))? as usize
    };

    Ok(BuildReport {
        commit: commit.to_string(),
        artifact_path: tmp_path.to_path_buf(),
        already_built: false,
        document_count: docs.len(),
        canon_count: canon_ids.len(),
        capture_count: captures.len(),
        chunk_count,
        finding_count: (!hierarchy_cycle.is_empty() as usize) + (!succession_cycle.is_empty() as usize),
        maintenance_enabled,
        vectors_enabled: vector_report.enabled && vector_report.chunk_count > 0,
        vector_count: vector_report.chunk_count,
        prefixes_generated: prefix_report.generated,
    })
}

/// keep the last 20 artifacts plus one per day for 90 days, by file mtime; delete the rest.
fn apply_retention(dir: &Path) -> Result<()> {
    let mut artifacts: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".sqlite") {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        artifacts.push((path, mtime));
    }
    artifacts.sort_by(|a, b| b.1.cmp(&a.1));
    let keep: HashSet<PathBuf> = retain_artifacts(&artifacts, crate::clock::system_now()).into_iter().collect();
    for (path, _) in &artifacts {
        if !keep.contains(path) {
            std::fs::remove_file(path).ok();
        }
    }
    Ok(())
}

/// pure retention policy, factored out for testing: `artifacts` sorted newest-first by mtime.
fn retain_artifacts(artifacts: &[(PathBuf, SystemTime)], now: SystemTime) -> Vec<PathBuf> {
    const DAY: u64 = 86_400;
    let mut keep = Vec::new();
    let mut days_seen: HashSet<i64> = HashSet::new();
    for (path, mtime) in artifacts.iter().take(20) {
        keep.push(path.clone());
        days_seen.insert(day_index(*mtime));
    }
    for (path, mtime) in artifacts.iter().skip(20) {
        let age = now.duration_since(*mtime).unwrap_or_default();
        if age.as_secs() > 90 * DAY {
            continue;
        }
        if days_seen.insert(day_index(*mtime)) {
            keep.push(path.clone());
        }
    }
    keep
}

fn day_index(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs() as i64 / 86_400
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Layer, Status as St};

    fn section(path: &str, start: usize, end: usize) -> Section {
        Section { path: path.to_string(), level: 1, heading: path.to_string(), start, end, line: 1 }
    }

    #[test]
    fn chunk_end_stops_before_first_child() {
        let sections = vec![section("a", 0, 100), section("a/b", 20, 60), section("a/c", 60, 100)];
        assert_eq!(chunk_end(&sections, 0), 20, "section a should stop where a/b begins");
        assert_eq!(chunk_end(&sections, 1), 60, "leaf section a/b keeps its own range");
        assert_eq!(chunk_end(&sections, 2), 100);
    }

    fn doc(id: &str, status: St, sections: Vec<Section>) -> ParsedDoc {
        ParsedDoc {
            id: id.to_string(),
            path: format!("docs/{id}.md"),
            frontmatter: Frontmatter { status: Some(status), layer: Some(Layer::Reference), ..Default::default() },
            body: String::new(),
            sections,
            links: vec![],
            content_hash: String::new(),
            word_count: 0,
        }
    }

    #[test]
    fn drift_states_cover_the_documented_cases() {
        let target_id = "01K5A7Q2XW3ZRCM8T6YB4VJH0N";
        let missing_id = "01K5A7Q2XW3ZRCM8T6YB4VJH0P";
        let superseded_id = "01K5A7Q2XW3ZRCM8T6YB4VJH0Q";
        let tombstoned_id = "01K5A7Q2XW3ZRCM8T6YB4VJH0R";
        let target = doc(target_id, St::Canon, vec![section("install", 0, 10)]);
        let superseded = doc(superseded_id, St::Superseded, vec![]);
        let docs = [target, superseded];
        let by_id: HashMap<String, &ParsedDoc> = docs.iter().map(|d| (d.id.clone(), d)).collect();
        let tombstones: HashSet<String> = [tombstoned_id.to_string()].into_iter().collect();

        assert_eq!(drift_for_kb(Some(target_id), None, &by_id, &tombstones).0, "ok");
        assert_eq!(drift_for_kb(Some(target_id), Some("install"), &by_id, &tombstones).0, "ok");
        assert_eq!(drift_for_kb(Some(target_id), Some("nope"), &by_id, &tombstones).0, "heading_missing");
        assert_eq!(drift_for_kb(Some(missing_id), None, &by_id, &tombstones).0, "missing");
        assert_eq!(drift_for_kb(Some(superseded_id), None, &by_id, &tombstones).0, "target_superseded");
        assert_eq!(drift_for_kb(Some(tombstoned_id), None, &by_id, &tombstones).0, "tombstoned");
        assert_eq!(drift_for_kb(None, None, &by_id, &tombstones).0, "missing");
    }

    #[test]
    fn retention_keeps_last_20_plus_one_per_day_within_90() {
        let now = crate::clock::system_now();
        let mut artifacts = Vec::new();
        // 25 artifacts, one per day, newest first.
        for i in 0..25u64 {
            artifacts.push((PathBuf::from(format!("{i}.sqlite")), now - std::time::Duration::from_secs(i * 86_400)));
        }
        // one more, same day as artifact 0 but older within that day.
        artifacts.push((PathBuf::from("0b.sqlite"), now - std::time::Duration::from_secs(3_600)));
        artifacts.sort_by(|a, b| b.1.cmp(&a.1));
        let keep = retain_artifacts(&artifacts, now);
        assert!(keep.len() >= 20, "should keep at least the newest 20, got {}", keep.len());
        // day 24 (index 24, age 24 days) is within 90 days and not covered by the top 20, so it survives.
        assert!(keep.contains(&PathBuf::from("24.sqlite")));
        // a very old artifact (index 25 would be 25 days, still <90; push one far beyond 90 days to confirm eviction)
        let mut artifacts2 = artifacts.clone();
        artifacts2.push((PathBuf::from("ancient.sqlite"), now - std::time::Duration::from_secs(200 * 86_400)));
        artifacts2.sort_by(|a, b| b.1.cmp(&a.1));
        let keep2 = retain_artifacts(&artifacts2, now);
        assert!(!keep2.contains(&PathBuf::from("ancient.sqlite")));
    }

    #[test]
    fn responsible_for_agent_commit_uses_configured_owner() {
        let mut cfg_text = String::new();
        cfg_text.push_str("[principals.writer]\nkind = \"service\"\nowner = \"caleb\"\n");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("groups.toml"), cfg_text).unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let commit = CommitInfo {
            sha: "abc".into(),
            date: DateTime::parse_from_rfc3339("2026-01-01T00:00:00-06:00").unwrap(),
            author_name: "agent:writer@writer".into(),
            author_email: String::new(),
            run_trailer: None,
            co_authors: vec![],
            files: vec![],
        };
        assert_eq!(responsible_for(&commit, &cfg), "caleb");

        let unknown = CommitInfo { author_name: "agent:writer@ghost".into(), ..commit.clone() };
        assert_eq!(responsible_for(&unknown, &cfg), "agent:writer@ghost");
    }
}
