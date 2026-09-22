//! `curate-nightly` (design.md §15.3): run the detectors in `findings.rs`, apply what can be
//! fixed automatically, escalate the rest, and tag a checkpoint on a clean pass. `compact.rs`
//! (design.md §21) is the maintenance step that runs after a checkpoint is backed up.
//!
//! **delete and revert are always review class.** design.md §15.3 marks the "superseded past
//! delete days", "draft past ttl", and "bad merged change" rows "review class always", but
//! `merge::run_structure_gate` (owned by the `merge` module, not this one) skips deletions
//! entirely and has no special case for a nightly-authored revert -- left to the normal path,
//! a low-criticality document with no other reason to review would land automatically, which is
//! exactly what these three rows forbid. lacking a way to force `merge::merge`'s classification
//! from outside, this module instead runs those three kinds with `MergeOpts { check_only: true,
//! .. }` (so nothing ever lands from this call) and always records an escalation, regardless of
//! what the check-only pass would have classified. every other auto-fixable kind uses
//! `MergeOpts::default()` and lands normally when the gate and policy allow it. see the report
//! for the follow-up this implies for `merge::run_structure_gate`.

pub mod compact;
pub mod findings;

pub use compact::{CompactOpts, CompactReport, compact};
pub use findings::{Finding, FindingKind};

use crate::config::Config;
use crate::curate::ops::{self, Applied, ProposedOp, locate_doc};
use crate::git;
use crate::index;
use crate::merge::{self, MergeOpts};
use crate::model::{self, MergeClass, Operation, Status};
use crate::parse::frontmatter::sha256_hex;
use crate::parse::{parse_document, render_document};
use crate::policy::{self, Actor, DocPolicyView, OpRequest};
use crate::review::udiff::render_unified;
use crate::runlog::{RunLog, RunStart, RunStatus};
use crate::session;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `kb maintain nightly` input.
#[derive(Debug, Clone, Default)]
pub struct NightlyOpts {
    pub dry_run: bool,
    /// a commit-ish, or a duration like `24h`/`7d`; defaults to the last nightly run's marker,
    /// else 24 hours.
    pub since: Option<String>,
    pub only: Option<FindingKind>,
}

/// `kb maintain nightly`'s result.
#[derive(Debug, Clone, Serialize)]
pub struct NightlyReport {
    pub findings: Vec<Finding>,
    pub sessions: Vec<String>,
    pub escalations: Vec<String>,
    pub checkpoint: Option<String>,
}

/// a finding recorded for a human at `<state>/escalations/<ulid>.toml`; `kb maintain escalations`
/// lists these and `--resolve` marks one handled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationRecord {
    pub kind: String,
    pub doc_ids: Vec<String>,
    pub detail: String,
    pub ts: String,
    #[serde(default)]
    pub resolved: bool,
}

fn escalations_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir().join("escalations")
}

fn write_escalation(cfg: &Config, f: &Finding) -> Result<String> {
    let dir = escalations_dir(cfg);
    std::fs::create_dir_all(&dir)?;
    let id = model::new_ulid();
    let rec = EscalationRecord { kind: f.kind.to_string(), doc_ids: f.doc_ids.clone(), detail: f.detail.clone(), ts: Utc::now().to_rfc3339(), resolved: false };
    std::fs::write(dir.join(format!("{id}.toml")), toml::to_string_pretty(&rec).context("serializing escalation")?)?;
    Ok(id)
}

/// every escalation on file, oldest first, as `(id, record)`.
pub fn list_escalations(cfg: &Config) -> Result<Vec<(String, EscalationRecord)>> {
    let dir = escalations_dir(cfg);
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut paths: Vec<PathBuf> =
        std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml")).collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let rec: EscalationRecord = toml::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("parsing {}", p.display()))?;
        out.push((id, rec));
    }
    Ok(out)
}

/// mark one escalation resolved by exact id or an unambiguous prefix; returns its full id.
pub fn resolve_escalation(cfg: &Config, id_or_prefix: &str) -> Result<String> {
    let dir = escalations_dir(cfg);
    let matches: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with(id_or_prefix)))
        .collect();
    match matches.len() {
        0 => bail!("no escalation matches `{id_or_prefix}`"),
        1 => {
            let p = &matches[0];
            let mut rec: EscalationRecord = toml::from_str(&std::fs::read_to_string(p)?)?;
            rec.resolved = true;
            std::fs::write(p, toml::to_string_pretty(&rec)?)?;
            Ok(p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string())
        }
        n => bail!("`{id_or_prefix}` matches {n} escalations; give a longer prefix"),
    }
}

fn default_principal(cfg: &Config) -> String {
    let (_, email) = git::identity(&cfg.root);
    let email = email.unwrap_or_else(|| "unknown".to_string());
    cfg.principal_for_email(&email).unwrap_or(email)
}

/// parse a duration like `24h`, `7d`, `2w` into days; `None` when `s` does not end in one of
/// `h`/`d`/`w`/`m` (minutes) with a leading number.
fn parse_duration_days(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: f64 = num.parse().ok()?;
    match unit {
        "h" => Some(n / 24.0),
        "d" => Some(n),
        "w" => Some(n * 7.0),
        "m" => Some(n / (24.0 * 60.0)),
        _ => None,
    }
}

fn first_commit(cfg: &Config) -> Result<String> {
    let out = git::run(&cfg.root, &["rev-list", "--max-parents=0", "main"])?;
    out.lines().next().map(str::to_string).context("repository has no commits")
}

/// resolve `--since` (or its default) to a commit-ish that starts the window the two
/// review-backed detectors and "yesterday's merges" language in design.md §15.3 read.
pub fn resolve_since(cfg: &Config, since: Option<&str>) -> Result<String> {
    if let Some(s) = since {
        if let Ok(sha) = git::rev_parse(&cfg.root, s)
            && !sha.is_empty()
        {
            return Ok(sha);
        }
        if let Some(days) = parse_duration_days(s) {
            let cutoff = (Utc::now() - Duration::seconds((days * 86_400.0) as i64)).to_rfc3339();
            return match git::rev_list_before(&cfg.root, "main", &cutoff)? {
                Some(sha) => Ok(sha),
                None => first_commit(cfg),
            };
        }
        bail!("--since `{s}` is neither a commit-ish nor a duration like `24h`/`7d`");
    }
    let marker = cfg.state_dir().join("maintain").join("last_nightly");
    if let Ok(text) = std::fs::read_to_string(&marker) {
        let sha = text.trim();
        if !sha.is_empty() && git::rev_parse(&cfg.root, sha).map(|s| !s.is_empty()).unwrap_or(false) {
            return Ok(sha.to_string());
        }
    }
    let cutoff = (Utc::now() - Duration::hours(24)).to_rfc3339();
    match git::rev_list_before(&cfg.root, "main", &cutoff)? {
        Some(sha) => Ok(sha),
        None => first_commit(cfg),
    }
}

fn write_last_nightly(cfg: &Config) -> Result<()> {
    let dir = cfg.state_dir().join("maintain");
    std::fs::create_dir_all(&dir)?;
    let head = git::rev_parse(&cfg.root, "main")?;
    std::fs::write(dir.join("last_nightly"), head)?;
    Ok(())
}

/// a `DocPolicyView` built straight from the newest index artifact (not a session worktree, so
/// this can decide whether a fix is even worth a session before opening one).
fn doc_policy_view_from_artifact(conn: &Connection, cfg: &Config, doc_id: &str) -> Result<Option<DocPolicyView>> {
    let fm_json: Option<String> = conn.query_row("select frontmatter from documents where id = ?1", [doc_id], |r| r.get(0)).ok();
    let Some(fm_json) = fm_json else { return Ok(None) };
    let fm: model::Frontmatter = serde_json::from_str(&fm_json).with_context(|| format!("parsing frontmatter json for {doc_id}"))?;
    let word_count: i64 = conn.query_row("select word_count from documents where id = ?1", [doc_id], |r| r.get(0)).unwrap_or(0);
    let crit_band: Option<String> = conn.query_row("select crit_band from documents where id = ?1", [doc_id], |r| r.get(0)).unwrap_or(None);
    let band = crit_band.map(|b| if b == "low" { MergeClass::Auto } else { MergeClass::Review });
    let (in_canon, in_code): (i64, i64) = conn
        .query_row("select in_canon, in_code from graph_metrics where doc_id = ?1", [doc_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((0, 0));
    Ok(Some(DocPolicyView::from_frontmatter(cfg, &fm, word_count as usize, band, in_canon as u32, in_code as u32)?))
}

// ---------------------------------------------------------------------------------------------
// custom appliers: the three editorial fixes `curate::ops::apply` has no operation for (it only
// carries the writer-facing vocabulary; these patch frontmatter/body fields the nightly detectors
// own directly).
// ---------------------------------------------------------------------------------------------

fn write_and_diff(path: &Path, before_text: &str, after_text: &str, rel_label: &str) -> Result<(Option<String>, String, String)> {
    std::fs::write(path, after_text).with_context(|| format!("writing {}", path.display()))?;
    let before_hash = Some(sha256_hex(before_text.as_bytes()));
    let after_hash = sha256_hex(after_text.as_bytes());
    let diff = render_unified(rel_label, rel_label, before_text, after_text);
    Ok((before_hash, after_hash, diff))
}

/// the drift (`target_superseded`) fix: repoint one `rel`/`uri` ref at the chain's final target.
/// `op.field` carries the ref's `rel`, `op.content` its current (drifted) `uri`, `op.new_id` the
/// resolved final document id.
fn apply_relink(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().context("relink fix requires target_id")?;
    let rel = op.field.clone().context("relink fix requires field (the ref's rel)")?;
    let old_uri = op.content.clone().context("relink fix requires content (the drifted uri)")?;
    let new_id = op.new_id.clone().context("relink fix requires new_id (the resolved target)")?;
    let path = locate_doc(worktree, &target_id)?;
    let before_text = std::fs::read_to_string(&path)?;
    let mut doc = parse_document(&before_text)?;
    let new_uri = format!("kb:{new_id}");
    let mut changed = false;
    for r in doc.frontmatter.refs.iter_mut() {
        if r.rel == rel && r.uri == old_uri {
            r.uri = new_uri.clone();
            changed = true;
        }
    }
    if !changed {
        bail!("relink fix: no `{rel}` ref with uri `{old_uri}` found on {target_id}");
    }
    let after_text = render_document(&doc.frontmatter, &doc.body)?;
    let rel_label = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, &before_text, &after_text, &rel_label)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

/// the long-chain fix: point `superseded_by` straight at the chain's final target (`op.new_id`).
fn apply_reset_superseded_by(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().context("long-chain fix requires target_id")?;
    let new_id = op.new_id.clone().context("long-chain fix requires new_id")?;
    let path = locate_doc(worktree, &target_id)?;
    let before_text = std::fs::read_to_string(&path)?;
    let mut doc = parse_document(&before_text)?;
    doc.frontmatter.superseded_by = Some(format!("kb:{new_id}"));
    let after_text = render_document(&doc.frontmatter, &doc.body)?;
    let rel_label = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, &before_text, &after_text, &rel_label)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

/// the missing-provenance fix for an already superseded/archived document: fill `superseded_by`
/// (falling back to `parent`, then `"none"`, matching `curate::ops::apply_retire`) when it is
/// missing, and prepend the template banner when the body has none.
fn apply_add_banner_and_pointer(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().context("banner fix requires target_id")?;
    let path = locate_doc(worktree, &target_id)?;
    let before_text = std::fs::read_to_string(&path)?;
    let mut doc = parse_document(&before_text)?;
    let status = doc.frontmatter.status.unwrap_or(Status::Archived);

    if doc.frontmatter.superseded_by.as_deref().is_none_or(str::is_empty) {
        let fallback = doc.frontmatter.parent.clone().unwrap_or_else(|| "none".to_string());
        doc.frontmatter.superseded_by = Some(fallback);
    }

    let mut body = doc.body.clone();
    if !crate::check::has_banner(&body) {
        let target = doc.frontmatter.superseded_by.as_deref().filter(|s| *s != "none").and_then(model::parse_kb_uri).and_then(|(id, _)| {
            let t = locate_doc(worktree, &id).ok()?;
            let text = std::fs::read_to_string(t).ok()?;
            let title = parse_document(&text).ok()?.frontmatter.title?;
            Some((title, format!("kb:{id}")))
        });
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let banner_text = crate::check::render_banner(status, &today, target.as_ref().map(|(t, u)| (t.as_str(), u.as_str())));
        body = format!("{banner_text}\n{}", body.trim_start_matches('\n'));
    }

    let after_text = render_document(&doc.frontmatter, &body)?;
    let rel_label = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, &before_text, &after_text, &rel_label)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

// ---------------------------------------------------------------------------------------------
// group handling: one session per finding kind for the ordinary editorial fixes; a dedicated,
// always-review path for delete and revert (see this module's doc comment).
// ---------------------------------------------------------------------------------------------

struct GroupOutcome {
    session_id: Option<String>,
    all_landed: bool,
    escalated: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn handle_editorial_group(cfg: &Config, conn: &Connection, actor_str: &str, kind: FindingKind, group: &[&Finding], log: &mut RunLog) -> Result<GroupOutcome> {
    let actor = Actor::parse(actor_str).map_err(|e| anyhow!(e))?;
    let mut to_apply: Vec<&Finding> = Vec::new();
    let mut escalated = Vec::new();

    for f in group {
        let op = f.auto_fix.clone().expect("group only contains auto-fixable findings");
        let target = op.target_id.clone().unwrap_or_default();
        let doc_view = doc_policy_view_from_artifact(conn, cfg, &target)?;
        let req = OpRequest { op: op.op, section: None, field: op.field.clone(), added_words: 0, target_layer: None };
        let decision = policy::decide(cfg, &actor, doc_view.as_ref(), &req);
        log.policy_decision(&target, op.op, decision.allow, decision.class, &decision.reasons)?;
        if decision.allow {
            to_apply.push(f);
        } else {
            write_escalation(cfg, f)?;
            escalated.push(format!("{}: {} (policy denied: {})", f.kind, f.detail, decision.reasons.join("; ")));
        }
    }

    if to_apply.is_empty() {
        return Ok(GroupOutcome { session_id: None, all_landed: true, escalated });
    }

    let doc_ids: Vec<String> = to_apply.iter().flat_map(|f| f.doc_ids.iter().cloned()).collect();
    let mut sess = session::open(cfg, &format!("nightly curation: {kind}"), Some(actor_str), &doc_ids)?;
    let owner = policy::responsible_party(cfg, &actor);

    let mut justifications = Vec::new();
    let mut applied_any = false;
    for f in &to_apply {
        let op = f.auto_fix.clone().expect("checked above");
        let applied = match f.kind {
            FindingKind::Stale | FindingKind::Orphan => ops::apply(cfg, &sess.worktree, &op, &owner),
            FindingKind::Drift => apply_relink(&sess.worktree, &op),
            FindingKind::LongChain => apply_reset_superseded_by(&sess.worktree, &op),
            FindingKind::MissingProvenance => apply_add_banner_and_pointer(&sess.worktree, &op),
            other => bail!("{other}: not an editorial-group finding"),
        };
        match applied {
            Ok(applied) => {
                log.op(op.op, &applied.doc_id, None, applied.before_hash.as_deref(), Some(&applied.after_hash), Some(applied.diff.as_bytes()))?;
                git::run(&sess.worktree, &["add", "-A"])?;
                git::commit_all(&sess.worktree, &format!("wip: nightly {} {}", f.kind, applied.doc_id))?;
                justifications.push(format!("- {}: {}", f.kind, f.detail));
                applied_any = true;
            }
            Err(e) => {
                write_escalation(cfg, f)?;
                escalated.push(format!("{}: {} (apply failed: {e:#})", f.kind, f.detail));
            }
        }
    }

    if !applied_any {
        git::worktree_remove(&cfg.root, &sess.worktree).ok();
        git::branch_delete_force(&cfg.root, &sess.branch).ok();
        let _ = git::delete_ref(&cfg.root, &format!("refs/kb/sessions/{}/base", sess.id));
        return Ok(GroupOutcome { session_id: None, all_landed: escalated.is_empty(), escalated });
    }

    let message = format!("nightly curation: {kind}\n\n{}", justifications.join("\n"));
    session::settle(cfg, &mut sess, Some(&message))?;
    let outcome = merge::merge(cfg, &sess.id, MergeOpts { actor: Some(actor_str.to_string()), ..Default::default() })?;
    let all_landed = outcome.landed.is_some() && escalated.is_empty();
    if outcome.landed.is_none() {
        escalated.push(format!(
            "{kind}: session {} settled but did not land ({:?}, reasons: {})",
            sess.id,
            outcome.class,
            outcome.reasons.join("; ")
        ));
    }
    Ok(GroupOutcome { session_id: Some(sess.id.clone()), all_landed, escalated })
}

/// delete is always review class (design.md §15.3): see this module's doc comment for why that
/// means `check_only: true` here rather than the normal `MergeOpts::default()` path, and why the
/// caller always records an escalation regardless of what the check-only pass classified.
fn handle_delete_finding(cfg: &Config, actor_str: &str, f: &Finding, log: &mut RunLog, conn: &Connection) -> Result<(Option<String>, String)> {
    let op = f.auto_fix.clone().expect("delete findings are only handled when auto-fixable");
    let target = op.target_id.clone().unwrap_or_default();
    let actor = Actor::parse(actor_str).map_err(|e| anyhow!(e))?;
    let doc_view = doc_policy_view_from_artifact(conn, cfg, &target)?;
    let req = OpRequest { op: Operation::Delete, section: None, field: None, added_words: 0, target_layer: None };
    let decision = policy::decide(cfg, &actor, doc_view.as_ref(), &req);
    log.policy_decision(&target, Operation::Delete, decision.allow, decision.class, &decision.reasons)?;
    if !decision.allow {
        write_escalation(cfg, f)?;
        return Ok((None, format!("policy denied: {}", decision.reasons.join("; "))));
    }

    let mut sess = session::open(cfg, &format!("nightly curation: delete {target}"), Some(actor_str), std::slice::from_ref(&target))?;
    let path = locate_doc(&sess.worktree, &target)?;
    let rel = path.strip_prefix(&sess.worktree).unwrap_or(&path).to_string_lossy().into_owned();
    git::run(&sess.worktree, &["rm", "-q", rel.as_str()])?;
    git::commit_all(&sess.worktree, &format!("wip: nightly delete {target}"))?;
    session::settle(cfg, &mut sess, Some(&format!("nightly curation: delete {target}\n\n{}", f.detail)))?;
    let outcome = merge::merge(cfg, &sess.id, MergeOpts { check_only: true, approve: false, actor: Some(actor_str.to_string()) })?;
    write_escalation(cfg, f)?;
    Ok((
        Some(sess.id.clone()),
        format!("checked, never landed automatically (class {:?}; reasons: {})", outcome.class, outcome.reasons.join("; ")),
    ))
}

/// bad merged change is always review class too; same `check_only: true` reasoning as delete.
fn handle_revert_finding(cfg: &Config, actor_str: &str, f: &Finding) -> Result<(Option<String>, String)> {
    let op = f.auto_fix.clone().expect("revert findings are only handled when auto-fixable");
    let Some(sha) = op.content.clone() else {
        return Ok((None, "no commit sha recorded on the finding".to_string()));
    };
    let mut sess = session::open(cfg, &format!("nightly curation: revert {sha}"), Some(actor_str), &[])?;
    if let Err(e) = git::revert_no_edit(&sess.worktree, &sha) {
        git::worktree_remove(&cfg.root, &sess.worktree).ok();
        git::branch_delete_force(&cfg.root, &sess.branch).ok();
        return Ok((None, format!("revert failed: {e:#}")));
    }
    session::settle(cfg, &mut sess, Some(&format!("nightly curation: revert {sha}\n\n{}", f.detail)))?;
    let outcome = merge::merge(cfg, &sess.id, MergeOpts { check_only: true, approve: false, actor: Some(actor_str.to_string()) })?;
    Ok((
        Some(sess.id.clone()),
        format!("checked, never landed automatically (class {:?}; reasons: {})", outcome.class, outcome.reasons.join("; ")),
    ))
}

// ---------------------------------------------------------------------------------------------
// the entry point.
// ---------------------------------------------------------------------------------------------

/// `kb maintain findings`: the detectors only, read-only -- no session, no escalation file, no
/// checkpoint. builds a fresh index artifact first when none exists yet.
pub fn run_findings(cfg: &Config, only: Option<FindingKind>) -> Result<Vec<Finding>> {
    let since_commit = resolve_since(cfg, None)?;
    if index::open_newest(cfg).is_err() {
        index::build(cfg, None)?;
    }
    let conn = index::open_newest(cfg)?;
    findings::detect(cfg, &conn, &cfg.root, &since_commit, only)
}

/// `kb maintain nightly`: design.md §15.3, run from the scheduler at 03:00 local (design.md §30).
pub fn nightly(cfg: &Config, opts: NightlyOpts) -> Result<NightlyReport> {
    let since_commit = resolve_since(cfg, opts.since.as_deref())?;
    if index::open_newest(cfg).is_err() {
        index::build(cfg, None)?;
    }
    let conn = index::open_newest(cfg)?;
    let all_findings = findings::detect(cfg, &conn, &cfg.root, &since_commit, opts.only)?;

    if opts.dry_run {
        let escalations = all_findings.iter().filter(|f| f.auto_fix.is_none()).map(|f| format!("{}: {}", f.kind, f.detail)).collect();
        return Ok(NightlyReport { findings: all_findings, sessions: vec![], escalations, checkpoint: None });
    }

    let principal = default_principal(cfg);
    let actor_str = format!("agent:curate-nightly@{principal}");
    let mut log = RunLog::create(
        cfg,
        RunStart { actor: actor_str.clone(), principal: Some(principal), skill: Some("curate-nightly".to_string()), ..Default::default() },
    )?;

    let mut sessions = Vec::new();
    let mut escalations = Vec::new();
    let mut clean = true;

    for f in all_findings.iter().filter(|f| f.auto_fix.is_none()) {
        write_escalation(cfg, f)?;
        escalations.push(format!("{}: {}", f.kind, f.detail));
        clean = false;
    }

    let mut by_kind: std::collections::BTreeMap<&'static str, Vec<&Finding>> = Default::default();
    for f in all_findings.iter().filter(|f| f.auto_fix.is_some()) {
        by_kind.entry(f.kind.as_str()).or_default().push(f);
    }

    for group in by_kind.into_values() {
        let kind = group[0].kind;
        match kind {
            FindingKind::SupersededPastDeleteDays | FindingKind::DraftPastTtl => {
                for f in group {
                    let (session_id, note) = handle_delete_finding(cfg, &actor_str, f, &mut log, &conn)?;
                    if let Some(id) = session_id {
                        sessions.push(id);
                    }
                    escalations.push(format!("{}: {} ({note})", f.kind, f.detail));
                    clean = false;
                }
            }
            FindingKind::BadMergedChange => {
                for f in group {
                    let (session_id, note) = handle_revert_finding(cfg, &actor_str, f)?;
                    if let Some(id) = session_id {
                        sessions.push(id);
                    }
                    escalations.push(format!("{}: {} ({note})", f.kind, f.detail));
                    clean = false;
                }
            }
            _ => {
                let outcome = handle_editorial_group(cfg, &conn, &actor_str, kind, &group, &mut log)?;
                if let Some(id) = outcome.session_id {
                    sessions.push(id);
                }
                if !outcome.all_landed {
                    clean = false;
                }
                escalations.extend(outcome.escalated);
            }
        }
    }
    drop(conn);

    let summary = format!("nightly curation: {} findings, {} escalations, {} sessions", all_findings.len(), escalations.len(), sessions.len());
    let checkpoint = maybe_checkpoint(cfg, clean, &summary)?;

    log.report(&summary)?;
    log.run_end(RunStatus::Ok, None, None, Some(&summary), &[], &[])?;
    log.seal(cfg)?;

    write_last_nightly(cfg)?;

    Ok(NightlyReport { findings: all_findings, sessions, escalations, checkpoint })
}

/// tag `checkpoint/<yyyy-mm-dd>` on `main`'s head when the pass was clean (design.md §15.3);
/// idempotent within one day (a second clean run on the same date reuses the existing tag).
fn maybe_checkpoint(cfg: &Config, clean: bool, summary: &str) -> Result<Option<String>> {
    if !clean {
        return Ok(None);
    }
    let date = Utc::now().format("%Y-%m-%d").to_string();
    let tag = format!("checkpoint/{date}");
    if git::tag_exists(&cfg.root, &tag) {
        return Ok(Some(tag));
    }
    let head = git::rev_parse(&cfg.root, "main")?;
    git::tag_annotated(&cfg.root, &tag, &head, summary)?;
    Ok(Some(tag))
}
