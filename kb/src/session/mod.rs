//! the session model: one worktree on one branch, shared by humans and agents.
//! contract: design.md §12 (sessions), §21 (worktree hygiene), §22 (tick).

use crate::check;
use crate::config::Config;
use crate::git;
use crate::model::new_ulid;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// a session's place in its lifecycle. design.md §12 names open, settled, merging, needs-human,
/// merged, abandoned; hold and needs-author are local additions used by `kb eod` and the secrets
/// gate respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionState {
    Open,
    Settled,
    Merging,
    NeedsHuman,
    NeedsAuthor,
    Hold,
    Merged,
    Abandoned,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Open => "open",
            SessionState::Settled => "settled",
            SessionState::Merging => "merging",
            SessionState::NeedsHuman => "needs-human",
            SessionState::NeedsAuthor => "needs-author",
            SessionState::Hold => "hold",
            SessionState::Merged => "merged",
            SessionState::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// one session: a worktree on `branch`, pinned to `base_commit`, recorded at
/// `<state>/sessions/<id>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub actor: String,
    pub objective: String,
    pub base_commit: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub opened_at: String,
    pub touched_at: String,
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// doc ids the session sparse-checked out, empty for a full checkout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparse: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settled_commits: Vec<String>,
    #[serde(default)]
    pub ops: u32,
    /// set by `kb eod` when it settled this session as part of the end-of-day sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eod: Option<bool>,
    /// when the session was abandoned; its branch is kept until 30 days after this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_at: Option<String>,
}

/// report returned by `eod`: counts of what the sweep did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EodReport {
    pub settled: u32,
    pub abandoned: u32,
    pub skipped: u32,
}

/// one row of `kb sessions`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub actor: String,
    pub state: SessionState,
    pub objective: String,
    pub opened_at: String,
    pub files_changed: Vec<String>,
    pub commits: u32,
}

fn sessions_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir().join("sessions")
}

fn session_path(cfg: &Config, id: &str) -> PathBuf {
    sessions_dir(cfg).join(format!("{id}.toml"))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// hours elapsed since an rfc3339 timestamp; 0.0 when it does not parse.
fn hours_since(rfc3339: &str) -> f64 {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 3600.0,
        Err(_) => 0.0,
    }
}

/// write a session's file, creating `<state>/sessions/` as needed.
pub fn save(cfg: &Config, session: &Session) -> Result<()> {
    std::fs::create_dir_all(sessions_dir(cfg))?;
    let text = toml::to_string_pretty(session).context("serializing session")?;
    std::fs::write(session_path(cfg, &session.id), text)?;
    Ok(())
}

/// every session file under `<state>/sessions/`, sorted by id (so by open time, ulids sort
/// chronologically).
pub fn list(cfg: &Config) -> Result<Vec<Session>> {
    let dir = sessions_dir(cfg);
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        out.push(toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?);
    }
    Ok(out)
}

/// load one session by exact id or by an unambiguous id prefix.
pub fn load(cfg: &Config, id_or_prefix: &str) -> Result<Session> {
    let exact = session_path(cfg, id_or_prefix);
    if exact.is_file() {
        let text = std::fs::read_to_string(&exact)?;
        return toml::from_str(&text).with_context(|| format!("parsing {}", exact.display()));
    }
    let matches: Vec<Session> = list(cfg)?.into_iter().filter(|s| s.id.starts_with(id_or_prefix)).collect();
    match matches.len() {
        0 => bail!("no session matches `{id_or_prefix}`"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => bail!("`{id_or_prefix}` matches {n} sessions; give a longer prefix"),
    }
}

/// actor default: `human:<principal or email>` from the current git identity.
fn default_actor(cfg: &Config) -> String {
    let (_, email) = git::identity(&cfg.root);
    let email = email.unwrap_or_else(|| "unknown".to_string());
    let who = cfg.principal_for_email(&email).unwrap_or(email);
    format!("human:{who}")
}

/// sparse-checkout patterns for a candidate set of document ids: both the flat and sharded
/// path forms, plus the schemas/prompts/config paths every agent session needs regardless of
/// its candidate set (design.md §12, §21).
fn sparse_patterns(sparse: &[String]) -> Vec<String> {
    let mut patterns = Vec::new();
    for doc_id in sparse {
        patterns.push(format!("docs/{doc_id}.md"));
        if doc_id.len() >= 3 {
            patterns.push(format!("docs/{}/{doc_id}.md", &doc_id[..3]));
        }
    }
    for extra in ["schemas/", "prompts/", "manifest.toml", "groups.toml", "kb.toml"] {
        patterns.push(extra.to_string());
    }
    patterns
}

/// `kb session open`: branch off `main`'s current head into a fresh worktree under
/// `<state>/worktrees/<ulid>`, pin the base ref, and write the session file.
pub fn open(cfg: &Config, objective: &str, actor: Option<&str>, sparse: &[String]) -> Result<Session> {
    let base = git::rev_parse(&cfg.root, "main")?;
    if base.is_empty() {
        bail!("branch `main` has no commits; open a session after the first commit");
    }

    let id = new_ulid();
    let branch = format!("session/{id}");
    let worktree = cfg.state_dir().join("worktrees").join(&id);
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git::worktree_add_new_branch(&cfg.root, &worktree, &branch, "main")?;
    git::update_ref(&cfg.root, &format!("refs/kb/sessions/{id}/base"), &base)?;

    if !sparse.is_empty() {
        git::enable_sparse_worktree_config(&cfg.root)?;
        git::sparse_checkout_set(&worktree, &sparse_patterns(sparse))?;
    }

    let now = now_rfc3339();
    let session = Session {
        id,
        actor: actor.map(str::to_string).unwrap_or_else(|| default_actor(cfg)),
        objective: objective.to_string(),
        base_commit: base,
        branch,
        worktree,
        opened_at: now.clone(),
        touched_at: now,
        state: SessionState::Open,
        run_id: None,
        sparse: sparse.to_vec(),
        settled_commits: vec![],
        ops: 0,
        eod: None,
        abandoned_at: None,
    };
    save(cfg, &session)?;
    Ok(session)
}

/// `kb session open --resume <id>`: recreate the worktree for an existing branch when the
/// worktree is gone (evicted by hygiene, or the machine was rebuilt). a no-op on the worktree
/// when it is already there.
pub fn resume(cfg: &Config, id: &str, objective: &str) -> Result<Session> {
    let mut session = load(cfg, id)?;
    if matches!(session.state, SessionState::Merged | SessionState::Abandoned) {
        bail!("session {} is {} and cannot be resumed", session.id, session.state);
    }
    if !session.worktree.is_dir() {
        git::worktree_prune(&cfg.root)?;
        if let Some(parent) = session.worktree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        git::worktree_add_existing_branch(&cfg.root, &session.worktree, &session.branch)?;
        if !session.sparse.is_empty() {
            git::enable_sparse_worktree_config(&cfg.root)?;
            git::sparse_checkout_set(&session.worktree, &sparse_patterns(&session.sparse))?;
        }
    }
    if !objective.is_empty() {
        session.objective = objective.to_string();
    }
    session.touched_at = now_rfc3339();
    save(cfg, &session)?;
    Ok(session)
}

/// one changed document, for the settled commit message.
struct DocChange {
    id: String,
    title: String,
    /// "created" | "deleted" | "n sections changed" | a caller-provided line.
    summary: String,
}

/// read a document's `(title, id)`, preferring the worktree copy and falling back to `rev` for
/// a path deleted from the worktree.
fn read_title_id(repo: &Path, rev: &str, path: &str) -> (String, String) {
    let text = std::fs::read_to_string(repo.join(path))
        .ok()
        .or_else(|| git::show_file(repo, rev, path).ok().and_then(|b| String::from_utf8(b).ok()));
    let fallback_id = || Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string();
    let Some(text) = text else {
        let id = fallback_id();
        return (id.clone(), id);
    };
    match crate::parse::parse_document(&text) {
        Ok(doc) => {
            let id = doc.frontmatter.id.unwrap_or_else(fallback_id);
            let title = doc.frontmatter.title.unwrap_or_else(|| id.clone());
            (title, id)
        }
        Err(_) => {
            let id = fallback_id();
            (id.clone(), id)
        }
    }
}

/// count sections whose content differs between two full file texts (frontmatter and all), at
/// least 1 whenever the texts differ. a parse failure on either side counts as one changed
/// section rather than failing settle.
fn count_changed_sections(old_text: &str, new_text: &str) -> usize {
    if old_text == new_text {
        return 0;
    }
    let old_body = crate::parse::parse_document(old_text).map(|d| d.body).unwrap_or_default();
    let new_body = crate::parse::parse_document(new_text).map(|d| d.body).unwrap_or_default();
    let old_sections = crate::parse::body::parse_body(&old_body).sections;
    let new_sections = crate::parse::body::parse_body(&new_body).sections;

    let extract = |text: &str, s: &crate::model::Section| text.get(s.start..s.end).unwrap_or("").to_string();
    let old_map: std::collections::BTreeMap<String, String> =
        old_sections.iter().map(|s| (s.path.clone(), extract(&old_body, s))).collect();
    let new_map: std::collections::BTreeMap<String, String> =
        new_sections.iter().map(|s| (s.path.clone(), extract(&new_body, s))).collect();

    let mut paths: std::collections::BTreeSet<&String> = old_map.keys().collect();
    paths.extend(new_map.keys());
    paths.into_iter().filter(|p| old_map.get(*p) != new_map.get(*p)).count().max(1)
}

/// per-document lines for the settled commit message, from `git diff --name-status base..head`.
fn describe_changes(repo: &Path, base: &str, _head: &str) -> Result<Vec<DocChange>> {
    // compare the base against the working tree, so staged work that was never wip-committed
    // (and renames done by validate --fix during the commit hook) still gets a line.
    let rows = git::diff_name_status_worktree(repo, base)?;
    let mut out = Vec::new();
    for (status, old_path, new_path) in rows {
        if !new_path.starts_with("docs/") || !new_path.ends_with(".md") {
            continue;
        }
        let (title, id) = read_title_id(repo, base, &new_path);
        let summary = match status {
            'A' => "created".to_string(),
            'D' => {
                let (del_title, del_id) = read_title_id(repo, base, &old_path);
                out.push(DocChange { id: del_id, title: del_title, summary: "deleted".to_string() });
                continue;
            }
            _ => {
                let old_text = git::show_file(repo, base, &old_path).ok().and_then(|b| String::from_utf8(b).ok());
                let new_text = std::fs::read_to_string(repo.join(&new_path)).ok();
                match (old_text, new_text) {
                    (Some(o), Some(n)) => {
                        let n_changed = count_changed_sections(&o, &n).max(1);
                        format!("{n_changed} section{} changed", if n_changed == 1 { "" } else { "s" })
                    }
                    _ => "content changed".to_string(),
                }
            }
        };
        out.push(DocChange { id, title, summary });
    }
    Ok(out)
}

/// todo: separate a settle into a content commit plus a `meta:` commit carrying only
/// machine-filled frontmatter on an otherwise human-authored file (design.md §12: "a session
/// settles into two commits only when a `meta:` commit ... is separated from a human's content
/// change, or when a `retire` or `supersede` is paired with the replacing `create`"). always
/// false until implemented; `settle` always takes the single-commit path.
fn split_meta_commit(_cfg: &Config, _session: &Session) -> bool {
    false
}

/// build the settled commit message per design.md §12.
fn build_settle_message(cfg: &Config, session: &Session, base: &str, changes: &[DocChange], why: &str, ops: u32) -> String {
    let objective_chars: Vec<char> = session.objective.replace('\n', " ").chars().collect();
    let objective = if objective_chars.len() > 72 {
        let truncated: String = objective_chars[..69].iter().collect();
        format!("{truncated}...")
    } else {
        objective_chars.into_iter().collect()
    };

    let mut lines = vec![objective, String::new()];
    for c in changes {
        lines.push(format!("{}  {}: {}", c.id, c.title, c.summary));
    }
    if !changes.is_empty() {
        lines.push(String::new());
    }

    let why_text = if why.trim().is_empty() { "no operation justification recorded" } else { why.trim() };
    lines.push(format!("why: {why_text}"));
    lines.push(String::new());

    lines.push(format!("Session: {}", session.id));
    lines.push(format!("Actor: {}", session.actor));
    lines.push(format!("Base: {base}"));
    lines.push(format!("Ops: {ops}"));
    if let Some(run_id) = &session.run_id {
        lines.push(format!("Run: {run_id}"));
    }
    if let Some(principal) = session.actor.strip_prefix("agent:").and_then(|rest| rest.split('@').nth(1))
        && let Some(owner) = cfg.groups.principals.get(principal).and_then(|p| p.owner.clone())
    {
        lines.push(format!("Co-authored-by: {owner}"));
    }

    lines.join("\n")
}

/// `kb session settle`: stage everything, run the secrets gate and `validate --fix`, then
/// squash the session's `wip:` commits into one commit on top of `base_commit`. returns the
/// settled commit sha(s) (always one until `split_meta_commit` is implemented).
pub fn settle(cfg: &Config, session: &mut Session, message: Option<&str>) -> Result<Vec<String>> {
    let repo = session.worktree.clone();
    let base = session.base_commit.clone();

    git::run(&repo, &["add", "-A"])?;

    let secret_findings = check::scan_staged(cfg, &repo)?;
    if !secret_findings.is_empty() {
        session.state = SessionState::NeedsAuthor;
        session.touched_at = now_rfc3339();
        save(cfg, session)?;
        let summary: Vec<String> = secret_findings.iter().map(|f| format!("{}: {}", f.path.as_deref().unwrap_or("?"), f.message)).collect();
        bail!("settle refused, possible secrets in staged changes:\n{}", summary.join("\n"));
    }

    // re-run validate --fix on every staged document and re-stage what it touched.
    let staged = git::run(&repo, &["diff", "--cached", "--name-only", "-z"])?;
    let staged_docs: Vec<String> = staged.split('\0').filter(|p| !p.is_empty() && p.starts_with("docs/") && p.ends_with(".md")).map(str::to_string).collect();
    let mut any_fixed = false;
    for rel in &staged_docs {
        let full = repo.join(rel);
        if full.is_file() {
            let outcome = check::validate_file(cfg, &full, true)?;
            any_fixed |= outcome.changed;
        }
    }
    if any_fixed {
        git::run(&repo, &["add", "-A"])?;
    }

    let head_before = git::head(&repo)?;
    let ops = git::rev_list_count(&repo, &format!("{base}..{head_before}"))?;

    // refuse when the branch was pushed and other work landed on top of it: any remote branch
    // containing this tip other than the session's own remote-tracking copy.
    let own_ref_suffixes = [format!("/{}", session.branch), session.branch.clone()];
    let foreign: Vec<String> = git::branches_r_contains(&repo, &head_before)?
        .into_iter()
        .filter(|b| !own_ref_suffixes.iter().any(|s| b.ends_with(s.as_str())))
        .collect();
    if !foreign.is_empty() {
        bail!("session branch `{}` was pushed and other work is based on it ({}); settle refused", session.branch, foreign.join(", "));
    }

    let changes = describe_changes(&repo, &base, &head_before)?;
    let why = match message {
        Some(m) => m.to_string(),
        None => git::log_subjects(&repo, &format!("{base}..{head_before}")).unwrap_or_default().join("; "),
    };

    // single-commit path only; see `split_meta_commit`.
    let _ = split_meta_commit(cfg, session);
    let settle_message = build_settle_message(cfg, session, &base, &changes, &why, ops);

    git::run(&repo, &["reset", "--soft", &base])?;
    git::run(&repo, &["commit", "-q", "-m", &settle_message])?;
    let new_head = git::head(&repo)?;

    session.settled_commits = vec![new_head.clone()];
    session.state = SessionState::Settled;
    session.ops = ops;
    session.touched_at = now_rfc3339();
    save(cfg, session)?;

    Ok(vec![new_head])
}

/// `kb session cleanup`: remove the worktree, optionally delete the branch and always drop the
/// hidden base ref, then prune. state becomes `Merged` when the branch was deleted, `Abandoned`
/// when it was kept.
pub fn cleanup(cfg: &Config, session: &mut Session, delete_branch: bool) -> Result<()> {
    if session.worktree.is_dir() {
        git::worktree_remove(&cfg.root, &session.worktree)?;
    }
    if delete_branch {
        git::branch_delete_force(&cfg.root, &session.branch)?;
    }
    let _ = git::delete_ref(&cfg.root, &format!("refs/kb/sessions/{}/base", session.id));
    git::worktree_prune(&cfg.root)?;

    session.state = if delete_branch { SessionState::Merged } else { SessionState::Abandoned };
    session.touched_at = now_rfc3339();
    save(cfg, session)?;
    Ok(())
}

/// `kb session abandon`: cleanup keeping the branch, recording when so 30-day retention can be
/// enforced later by compaction (design.md §21).
pub fn abandon(cfg: &Config, id: &str) -> Result<()> {
    let mut session = load(cfg, id)?;
    cleanup(cfg, &mut session, false)?;
    session.abandoned_at = Some(now_rfc3339());
    save(cfg, &session)?;
    Ok(())
}

/// `kb session hold`: pull a session out of (`on = true`) or back into (`on = false`) `kb eod`.
pub fn hold(cfg: &Config, id: &str, on: bool) -> Result<()> {
    let mut session = load(cfg, id)?;
    session.state = if on { SessionState::Hold } else { SessionState::Open };
    session.touched_at = now_rfc3339();
    save(cfg, &session)?;
    Ok(())
}

/// `kb eod`: settle every open, non-held session, committing uncommitted work first; abandon
/// sessions idle more than 24 hours with a clean tree and no commits since base.
pub fn eod(cfg: &Config) -> Result<EodReport> {
    let mut report = EodReport::default();
    for mut session in list(cfg)? {
        if session.state != SessionState::Open {
            continue;
        }
        if !session.worktree.is_dir() {
            report.skipped += 1;
            continue;
        }

        let repo = session.worktree.clone();
        if !git::status_porcelain(&repo, ".")?.is_empty() {
            git::commit_all(&repo, "wip: end of day")?;
        }

        let ops = git::rev_list_count(&repo, &format!("{}..HEAD", session.base_commit))?;
        let clean_now = git::status_porcelain(&repo, ".")?.is_empty();
        if ops == 0 && clean_now && hours_since(&session.touched_at) > 24.0 {
            abandon(cfg, &session.id)?;
            report.abandoned += 1;
            continue;
        }

        settle(cfg, &mut session, Some("end of day settle"))?;
        session.eod = Some(true);
        save(cfg, &session)?;
        report.settled += 1;
    }
    Ok(report)
}

/// `kb sessions`: id, actor, state, objective, files touched, commits since base, for every
/// session file, live worktrees included.
pub fn sessions_report(cfg: &Config) -> Result<Vec<SessionRow>> {
    let mut rows = Vec::new();
    for session in list(cfg)? {
        let mut files = std::collections::BTreeSet::new();
        let mut commits = 0u32;
        if session.worktree.is_dir() {
            if let Ok(head) = git::head(&session.worktree) {
                commits = git::rev_list_count(&session.worktree, &format!("{}..{}", session.base_commit, head)).unwrap_or(0);
                if let Ok(diff) = git::diff_name_status(&session.worktree, &session.base_commit, &head) {
                    for (_, _, p) in diff {
                        files.insert(p);
                    }
                }
            }
            if let Ok(status) = git::status_porcelain(&session.worktree, ".") {
                for line in status {
                    if let Some(path) = line.get(3..) {
                        files.insert(path.trim().to_string());
                    }
                }
            }
        }
        rows.push(SessionRow {
            id: session.id,
            actor: session.actor,
            state: session.state,
            objective: session.objective,
            opened_at: session.opened_at,
            files_changed: files.into_iter().collect(),
            commits,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_for_message() -> Session {
        Session {
            id: "01K5A7Q2XW3ZRCM8T6YB4VJH0N".into(),
            actor: "human:caleb".into(),
            objective: "wire up the retry path for token refresh".into(),
            base_commit: "abc123".into(),
            branch: "session/01K5A7Q2XW3ZRCM8T6YB4VJH0N".into(),
            worktree: PathBuf::from("/tmp/does-not-matter"),
            opened_at: "2026-09-21T00:00:00+00:00".into(),
            touched_at: "2026-09-21T00:00:00+00:00".into(),
            state: SessionState::Open,
            run_id: None,
            sparse: vec![],
            settled_commits: vec![],
            ops: 0,
            eod: None,
            abandoned_at: None,
        }
    }

    #[test]
    fn message_has_objective_changes_why_and_trailers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let session = session_for_message();
        let changes = vec![
            DocChange { id: "01K...A".into(), title: "token refresh".into(), summary: "created".into() },
            DocChange { id: "01K...B".into(), title: "old note".into(), summary: "deleted".into() },
        ];
        let msg = build_settle_message(&cfg, &session, "abc123", &changes, "moved retry logic", 3);
        let lines: Vec<&str> = msg.lines().collect();
        assert_eq!(lines[0], "wire up the retry path for token refresh");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "01K...A  token refresh: created");
        assert_eq!(lines[3], "01K...B  old note: deleted");
        assert!(msg.contains("why: moved retry logic"));
        assert!(msg.contains("Session: 01K5A7Q2XW3ZRCM8T6YB4VJH0N"));
        assert!(msg.contains("Actor: human:caleb"));
        assert!(msg.contains("Base: abc123"));
        assert!(msg.contains("Ops: 3"));
        assert!(!msg.contains("Run:"));
        assert!(!msg.contains("Co-authored-by:"));
    }

    #[test]
    fn objective_over_72_chars_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let mut session = session_for_message();
        session.objective = "x".repeat(100);
        let msg = build_settle_message(&cfg, &session, "abc123", &[], "why", 1);
        let first_line = msg.lines().next().unwrap();
        assert!(first_line.chars().count() <= 72, "{} chars", first_line.chars().count());
        assert!(first_line.ends_with("..."));
    }

    #[test]
    fn no_message_falls_back_to_wip_subjects() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let session = session_for_message();
        let msg = build_settle_message(&cfg, &session, "abc123", &[], "one; two", 2);
        assert!(msg.contains("why: one; two"));
    }

    #[test]
    fn run_id_and_co_authored_by_appear_when_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[principals.svc-curate]\nkind = \"service\"\nowner = \"caleb\"\n[principals.caleb]\nkind = \"human\"\nemails = [\"caleb@example.com\"]\n",
        )
        .unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let mut session = session_for_message();
        session.actor = "agent:curate@svc-curate".into();
        session.run_id = Some("01RUNRUNRUNRUNRUNRUNRUNRUN".into());
        let msg = build_settle_message(&cfg, &session, "abc123", &[], "why", 5);
        assert!(msg.contains("Run: 01RUNRUNRUNRUNRUNRUNRUNRUN"));
        assert!(msg.contains("Co-authored-by: caleb"));
    }

    #[test]
    fn no_op_split_meta_commit_stub_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(!split_meta_commit(&cfg, &session_for_message()));
    }

    #[test]
    fn changed_sections_counts_differing_slugs_and_floors_at_one() {
        let old = "+++\nid = \"x\"\n+++\n\n# a\n\none\n\n# b\n\ntwo\n";
        let new = "+++\nid = \"x\"\n+++\n\n# a\n\none changed\n\n# b\n\ntwo\n";
        assert_eq!(count_changed_sections(old, new), 1);
        assert_eq!(count_changed_sections(old, old), 0);
    }
}
