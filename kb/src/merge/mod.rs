//! `kb merge`: design.md §13 (merge), §14 (conflicts), §15 (the three checks), §16 (run log
//! events), §17.4 (delete guard), §20 (secrets), §21 (history), §22 (tick).
//!
//! `merge` runs one session through fetch, rebase, policy, structure gate, and content review,
//! then lands it (fast-forward only, no force flag anywhere) or files it for `kb review`.
//! `tick` and `history` (in their own files) cover the local reconciliation subset and
//! revert/doctor.

pub mod history;
pub mod tick;

use crate::check;
use crate::config::Config;
use crate::git;
use crate::index;
use crate::model::{AgentPolicy, MergeClass, Status};
use crate::parse::{parse_body, parse_document};
use crate::policy::{Actor, ActorKind};
use crate::provider::{self, Provider};
use crate::review;
use crate::runlog::{RunLog, RunStart, RunStatus, Verdict as RunlogVerdict};
use crate::session::{self, Session, SessionState};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// short name of the git notes ref review items are written to (design.md §13).
pub const REVIEW_NOTES_REF: &str = "kb-review";

/// one check's outcome, for `MergeOutcome::checks` and the git note written for review class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

impl CheckResult {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        CheckResult { name: name.to_string(), pass: true, detail: detail.into() }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Self {
        CheckResult { name: name.to_string(), pass: false, detail: detail.into() }
    }
}

/// `kb merge`'s result.
#[derive(Debug, Clone, Serialize)]
pub struct MergeOutcome {
    /// the new `main` sha, when landed.
    pub landed: Option<String>,
    pub class: MergeClass,
    pub state: SessionState,
    pub checks: Vec<CheckResult>,
    pub reasons: Vec<String>,
    pub run_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct MergeOpts {
    /// run the checks and report the class without landing.
    pub check_only: bool,
    /// an approval note exists, or `--approve` was given: land a review-class session anyway.
    pub approve: bool,
    /// actor recorded on the merge run log; defaults to the session's own actor.
    pub actor: Option<String>,
}

/// a review item written under `refs/notes/kb-review` on a session's head commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewNote {
    pub session: String,
    pub class: String,
    pub reasons: Vec<String>,
    pub checks: Vec<CheckResult>,
    pub run_id: String,
    pub ts: String,
}

/// one row of `kb review list`.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewItem {
    pub session: String,
    pub branch: String,
    pub head: String,
    pub note: ReviewNote,
}

/// `kb merge <session>`: design.md §13, steps 1 through 7.
pub fn merge(cfg: &Config, session_id: &str, opts: MergeOpts) -> Result<MergeOutcome> {
    merge_with_providers(cfg, session_id, opts, None)
}

/// like `merge`, but the quick gate's provider (role `gate`) can be substituted with
/// `gate_override` instead of the one `kb.toml` names -- the seam a test uses to script a
/// `FakeProvider` gate answer without a real `gate` role configured. production code always
/// calls `merge`, which forwards here with `None` (build whatever `kb.toml` configures, same as
/// every other role).
pub fn merge_with_providers(cfg: &Config, session_id: &str, opts: MergeOpts, gate_override: Option<&dyn Provider>) -> Result<MergeOutcome> {
    let mut session = session::load(cfg, session_id)?;

    let mut log = RunLog::create(
        cfg,
        RunStart {
            actor: opts.actor.clone().unwrap_or_else(|| session.actor.clone()),
            skill: Some("merge".into()),
            session_id: Some(session.id.clone()),
            worktree_path: Some(session.worktree.display().to_string()),
            base_commit: Some(session.base_commit.clone()),
            ..Default::default()
        },
    )?;
    let run_id = log.run_id().to_string();
    log.merge_start(serde_json::json!({
        "session": session.id, "check_only": opts.check_only, "approve": opts.approve,
    }))?;

    session.state = SessionState::Merging;
    session.run_id = Some(run_id.clone());
    session::save(cfg, &session)?;

    let actor_is_agent = matches!(Actor::parse(&session.actor).map(|a| a.kind), Ok(ActorKind::Agent));
    let max_retries = cfg.manifest.thresholds.merge_retries;

    let mut checks: Vec<CheckResult> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    // every loop iteration overwrites this before reading it (step 4's gate decision always
    // runs first); the initial value only matters if `max_retries` were negative, which
    // `Thresholds`'s `u32` type rules out.
    #[allow(unused_assignments)]
    let mut class = MergeClass::Auto;

    for attempt in 0..=max_retries {
        checks.clear();
        reasons.clear();
        let mut forced_review_reason: Option<String> = None;

        // step 1: fetch.
        let main_ref = fetch_main(cfg)?;

        // step 2: rebase.
        let rebase_res = git::rebase(&session.worktree, &main_ref)?;
        let head = if rebase_res.clean {
            git::head(&session.worktree)?
        } else if git::rebase_in_progress(&session.worktree) {
            if actor_is_agent {
                match resolve_conflicts_once(cfg, &session, &mut log)? {
                    Some((new_head, force_review)) => {
                        if force_review {
                            forced_review_reason =
                                Some("a resolved conflict touched a locked layer or a policy field (owner, access, status)".to_string());
                        }
                        new_head
                    }
                    None => {
                        let paths = git::conflicted_paths(&session.worktree).unwrap_or_default();
                        return handle_conflict_escalate(cfg, &mut session, log, run_id, paths);
                    }
                }
            } else {
                let paths = git::conflicted_paths(&session.worktree).unwrap_or_default();
                return handle_conflict_escalate(cfg, &mut session, log, run_id, paths);
            }
        } else {
            log.error(&format!("rebase failed: {}", rebase_res.message))?;
            session.state = SessionState::NeedsHuman;
            session::save(cfg, &session)?;
            log.run_end(RunStatus::Failed, None, None, None, &[], &[])?;
            log.seal(cfg).ok();
            return Ok(MergeOutcome {
                landed: None,
                class: MergeClass::Review,
                state: SessionState::NeedsHuman,
                checks,
                reasons: vec![format!("rebase error: {}", rebase_res.message.trim())],
                run_id,
            });
        };

        // merge-base(main, head) after a clean rebase is main itself.
        let base = git::rev_parse(&cfg.root, &main_ref)?;

        // step 3: policy.
        match run_policy_checks(cfg, &session, &mut log, &base, &head, actor_is_agent, &mut checks)? {
            PolicyOutcome::NeedsAuthor(detail) => {
                session.state = SessionState::NeedsAuthor;
                session::save(cfg, &session)?;
                reasons.push(detail);
                log.run_end(RunStatus::Failed, Some(&head), None, None, &[], &[])?;
                log.seal(cfg).ok();
                return Ok(MergeOutcome { landed: None, class: MergeClass::Review, state: SessionState::NeedsAuthor, checks, reasons, run_id });
            }
            PolicyOutcome::Fail(detail) => {
                session.state = SessionState::Settled;
                session::save(cfg, &session)?;
                reasons.push(detail);
                log.run_end(RunStatus::Failed, Some(&head), None, None, &[], &[])?;
                log.seal(cfg).ok();
                return Ok(MergeOutcome { landed: None, class: MergeClass::Review, state: SessionState::Settled, checks, reasons, run_id });
            }
            PolicyOutcome::Ok => {}
        }

        // step 4: structure gate.
        let gate = run_structure_gate(cfg, &session, &base, &head, actor_is_agent, &mut log)?;
        class = gate.class;
        reasons = gate.reasons.clone();
        if let Some(r) = forced_review_reason {
            class = MergeClass::Review;
            reasons.push(r);
        }
        let detail = if reasons.is_empty() { "auto: no critical structure touched".to_string() } else { reasons.join("; ") };
        checks.push(CheckResult { name: "gate".into(), pass: class == MergeClass::Auto, detail });

        // step 5: content review, with a quick-gate short circuit ahead of it. a cheap
        // classifier (the `gate` role) gets one shot at "trivial" before the full review call:
        // never asked for an agent actor or a review-class change (both always get full
        // review), and a skip only takes effect at or above `review_confidence`, the same bar
        // §15.2 uses for the full reviewer's own approve/review split.
        let quick_gate = quick_gate_check(cfg, &session, &head, class, actor_is_agent, gate_override, &mut log)?;
        if let QuickGate::Skip { p } = quick_gate {
            checks.push(CheckResult::ok("review", format!("quick gate: trivial (p={p:.2}); full review skipped")));
        } else {
        match content_review(cfg, &session, &base, &head)? {
            Some(out) => {
                let decision = review::decide(cfg, &out);
                let rl_verdict = match out.verdict {
                    review::Verdict::Approve => RunlogVerdict::Approve,
                    review::Verdict::RequestChanges => RunlogVerdict::RequestChanges,
                    review::Verdict::Escalate => RunlogVerdict::Escalate,
                };
                log.review_verdict(1, None, rl_verdict, out.confidence, serde_json::to_value(&out.findings)?, Some(&head))?;
                if !decision.merge_allowed && opts.approve {
                    // a human approval overrides a request_changes verdict; the approver is the
                    // responsible party and the findings stay in the run log and the outcome.
                    class = MergeClass::Review;
                    reasons.push(format!("review requested changes (confidence {:.2}), overridden by approval: {}", out.confidence, out.summary));
                    for f in &out.findings {
                        reasons.push(format!("{:?} {}: {}", f.kind, f.doc_id, f.reason));
                    }
                    checks.push(CheckResult::ok("review", format!("requested changes, overridden by approval: {}", out.summary)));
                } else if !decision.merge_allowed {
                    // request_changes: the session goes back to its author with the findings.
                    session.state = SessionState::NeedsAuthor;
                    session::save(cfg, &session)?;
                    checks.push(CheckResult::fail("review", format!("requested changes: {}", out.summary)));
                    let mut reasons = vec![format!("review requested changes (confidence {:.2}): {}", out.confidence, out.summary)];
                    for f in &out.findings {
                        let section = f.section.as_deref().map(|s| format!("#{s}")).unwrap_or_default();
                        reasons.push(format!("{:?} {}{}: {}", f.kind, f.doc_id, section, f.reason));
                    }
                    log.run_end(RunStatus::Failed, Some(&head), None, None, &[], &[])?;
                    log.seal(cfg).ok();
                    return Ok(MergeOutcome { landed: None, class: MergeClass::Review, state: SessionState::NeedsAuthor, checks, reasons, run_id });
                }
                if decision.merge_allowed {
                    if decision.class == MergeClass::Review {
                        class = MergeClass::Review;
                        reasons.push(decision.reason.clone());
                    }
                    checks.push(CheckResult::ok("review", format!("{:?} confidence {:.2}", out.verdict, out.confidence)));
                }
            }
            None => {
                checks.push(CheckResult::ok("review", "no review provider configured; self_review"));
            }
        }
        }

        if opts.check_only {
            log.run_end(RunStatus::Ok, Some(&head), None, None, &[], &[])?;
            log.seal(cfg).ok();
            session.state = SessionState::Settled;
            session::save(cfg, &session)?;
            return Ok(MergeOutcome { landed: None, class, state: SessionState::Settled, checks, reasons, run_id });
        }

        // step 6: land.
        if class == MergeClass::Auto || opts.approve {
            if !git::is_ancestor(&cfg.root, &base, &head) {
                bail!("post-rebase head does not descend from `{main_ref}`; refusing to land");
            }
            match land(cfg, &session, &base, &head)? {
                LandResult::Landed(new_main) => {
                    write_last_main(cfg, &new_main)?;
                    // step 7: post. index build errors are recorded, never fatal to the merge.
                    if let Err(e) = index::build(cfg, None) {
                        log.error(&format!("post-merge index build failed: {e:#}"))?;
                    }
                    session::cleanup(cfg, &mut session, true)?;
                    log.merge_end(serde_json::json!({"landed": new_main, "class": class}))?;
                    log.run_end(RunStatus::Ok, Some(&head), Some(&new_main), None, &session.settled_commits, &[])?;
                    log.seal(cfg).ok();
                    return Ok(MergeOutcome { landed: Some(new_main), class, state: SessionState::Merged, checks, reasons, run_id });
                }
                LandResult::Rejected => {
                    checks.push(CheckResult::fail(&format!("land-attempt-{attempt}"), "main moved during merge; retrying"));
                    continue;
                }
            }
        } else {
            write_review_note(cfg, &session, class, &reasons, &checks, &run_id)?;
            session.state = SessionState::Settled;
            session::save(cfg, &session)?;
            log.merge_end(serde_json::json!({"class": "review", "reasons": reasons}))?;
            log.run_end(RunStatus::Escalated, Some(&head), None, None, &[], &[])?;
            log.seal(cfg).ok();
            return Ok(MergeOutcome { landed: None, class, state: SessionState::Settled, checks, reasons, run_id });
        }
    }

    // step 6's compare-and-swap kept losing the race: escalate rather than retry forever.
    class = MergeClass::Review;
    reasons.push(format!("exhausted {max_retries} merge retries; escalated to review"));
    write_review_note(cfg, &session, class, &reasons, &checks, &run_id)?;
    session.state = SessionState::Settled;
    session::save(cfg, &session)?;
    log.run_end(RunStatus::Escalated, None, None, None, &[], &[])?;
    log.seal(cfg).ok();
    Ok(MergeOutcome { landed: None, class, state: SessionState::Settled, checks, reasons, run_id })
}

/// step 1: fetch `main` from `origin` when a remote is configured; the ref to rebase onto and to
/// treat as the post-rebase base.
fn fetch_main(cfg: &Config) -> Result<String> {
    if git::remote_exists(&cfg.root, "origin") {
        let _ = git::fetch(&cfg.root, "origin", "main");
        if git::rev_parse(&cfg.root, "refs/remotes/origin/main").is_ok() {
            return Ok("refs/remotes/origin/main".to_string());
        }
    }
    Ok("main".to_string())
}

enum LandResult {
    Landed(String),
    Rejected,
}

/// step 6: land locally or remotely, never with a force flag. when the repository's own checkout
/// has `main` checked out, landing goes through `git merge --ff-only` in that checkout so the
/// index and working tree follow the branch; a bare `update-ref` would leave them stale and a
/// later commit from that checkout would silently drop the landed files. when `main` is not
/// checked out, a compare-and-swap `update-ref` is used. a remote push is a plain push (git
/// accepts it only as a fast-forward), followed by the same local fast-forward (design.md §13, §21).
fn land(cfg: &Config, session: &Session, base: &str, head: &str) -> Result<LandResult> {
    if git::remote_exists(&cfg.root, "origin") {
        match git::push_plain(&cfg.root, "origin", &format!("{}:main", session.branch)) {
            Ok(()) => {
                // the remote is truth once the push succeeded; follow it locally when that is a
                // fast-forward, and leave the local branch alone otherwise (doctor reports the lag).
                let _ = fast_forward_local_main(cfg, None, head);
                Ok(LandResult::Landed(head.to_string()))
            }
            Err(_) => Ok(LandResult::Rejected),
        }
    } else {
        fast_forward_local_main(cfg, Some(base), head)
    }
}

/// move the local `main` to `head` as a fast-forward from `base`. returns `Rejected` when `main`
/// moved in between (the compare-and-swap failed); errors when the checkout has local changes
/// that the fast-forward would overwrite, which is the user's call to resolve.
fn fast_forward_local_main(cfg: &Config, expected: Option<&str>, head: &str) -> Result<LandResult> {
    let primary_on_main = git::run(&cfg.root, &["symbolic-ref", "-q", "HEAD"]).map(|s| s.trim() == "refs/heads/main").unwrap_or(false);
    let current = git::rev_parse(&cfg.root, "refs/heads/main")?;
    if let Some(exp) = expected
        && current != exp
    {
        return Ok(LandResult::Rejected);
    }
    if !git::is_ancestor(&cfg.root, &current, head) {
        return Ok(LandResult::Rejected);
    }
    if primary_on_main {
        match git::run(&cfg.root, &["merge", "--ff-only", "--no-edit", head]) {
            Ok(_) => Ok(LandResult::Landed(head.to_string())),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("local changes") || msg.contains("would be overwritten") || msg.contains("untracked working tree") {
                    bail!("cannot land: the main checkout at {} has local changes that the fast-forward would overwrite; commit or stash them, then run kb merge again", cfg.root.display());
                }
                Ok(LandResult::Rejected)
            }
        }
    } else {
        match git::update_ref_cas(&cfg.root, "refs/heads/main", head, &current) {
            Ok(()) => Ok(LandResult::Landed(head.to_string())),
            Err(_) => Ok(LandResult::Rejected),
        }
    }
}

fn write_last_main(cfg: &Config, sha: &str) -> Result<()> {
    std::fs::create_dir_all(cfg.state_dir())?;
    std::fs::write(cfg.state_dir().join("last_main"), sha)?;
    Ok(())
}

fn write_review_note(cfg: &Config, session: &Session, class: MergeClass, reasons: &[String], checks: &[CheckResult], run_id: &str) -> Result<()> {
    let note = ReviewNote {
        session: session.id.clone(),
        class: if class == MergeClass::Auto { "auto" } else { "review" }.to_string(),
        reasons: reasons.to_vec(),
        checks: checks.to_vec(),
        run_id: run_id.to_string(),
        ts: chrono::Utc::now().to_rfc3339(),
    };
    let toml_text = toml::to_string_pretty(&note).context("serializing review note")?;
    let head = git::head(&session.worktree)?;
    git::config_set(&cfg.root, "notes.rewriteRef", "refs/notes/kb-review")?;
    git::notes_add(&cfg.root, REVIEW_NOTES_REF, &head, &toml_text)?;
    if git::remote_exists(&cfg.root, "origin") {
        let _ = git::notes_push(&cfg.root, "origin", REVIEW_NOTES_REF);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// step 3: policy (design.md §13, §15.1's prerequisites, §17.4, §20)
// ---------------------------------------------------------------------------------------------

enum PolicyOutcome {
    Ok,
    /// back to the author to fix and re-settle (validate, graph, budget, checksums, delete guard).
    Fail(String),
    /// a secret was found: the author (human, or the agent principal's owner) must review it.
    NeedsAuthor(String),
}

#[allow(clippy::too_many_arguments)]
fn run_policy_checks(
    cfg: &Config,
    session: &Session,
    log: &mut RunLog,
    base: &str,
    head: &str,
    actor_is_agent: bool,
    checks: &mut Vec<CheckResult>,
) -> Result<PolicyOutcome> {
    let repo = &session.worktree;

    // secrets over base..head, covering every wip commit and the settled commit.
    let secret_findings = check::scan_range(cfg, repo, base, head)?;
    log.check("secrets", secret_findings.is_empty(), (!secret_findings.is_empty()).then_some("possible secret found"))?;
    if !secret_findings.is_empty() {
        let detail = secret_findings
            .iter()
            .map(|f| format!("{}:{} {}", f.path.as_deref().unwrap_or("?"), f.line.unwrap_or(0), f.message))
            .collect::<Vec<_>>()
            .join("; ");
        checks.push(CheckResult::fail("secrets", &detail));
        return Ok(PolicyOutcome::NeedsAuthor(detail));
    }
    checks.push(CheckResult::ok("secrets", "clean"));

    let changed = git::diff_name_status(repo, base, head)?;
    let changed_docs: Vec<(char, String, String)> = changed.into_iter().filter(|(_, _, p)| p.starts_with("docs/") && p.ends_with(".md")).collect();
    let deleted_paths: BTreeSet<String> = changed_docs.iter().filter(|(s, _, _)| *s == 'D').map(|(_, old, _)| old.clone()).collect();

    // validate every changed document that still exists at head.
    let mut validate_detail = Vec::new();
    for (status, _old, new_path) in &changed_docs {
        if *status == 'D' {
            continue;
        }
        let full = repo.join(new_path);
        if full.is_file() {
            let outcome = check::validate_file(cfg, &full, false)?;
            let errs: Vec<String> = outcome.findings.iter().filter(|f| f.severity == crate::model::Severity::Error).map(|f| f.message.clone()).collect();
            if !errs.is_empty() {
                validate_detail.push(format!("{new_path}: {}", errs.join(", ")));
            }
        }
    }
    log.check("validate", validate_detail.is_empty(), (!validate_detail.is_empty()).then(|| validate_detail.join("; ")).as_deref())?;
    if !validate_detail.is_empty() {
        checks.push(CheckResult::fail("validate", validate_detail.join("; ")));
        return Ok(PolicyOutcome::Fail(format!("validate: {}", validate_detail.join("; "))));
    }
    checks.push(CheckResult::ok("validate", "ok"));

    // graph acyclicity over every document at head.
    let all_docs = collect_docs_at(repo, head)?;
    let graph_findings = check::check_graph(&all_docs);
    let graph_errs: Vec<String> = graph_findings.iter().filter(|f| f.severity == crate::model::Severity::Error).map(|f| f.message.clone()).collect();
    log.check("graph", graph_errs.is_empty(), (!graph_errs.is_empty()).then(|| graph_errs.join("; ")).as_deref())?;
    if !graph_errs.is_empty() {
        checks.push(CheckResult::fail("graph", graph_errs.join("; ")));
        return Ok(PolicyOutcome::Fail(format!("graph: {}", graph_errs.join("; "))));
    }
    checks.push(CheckResult::ok("graph", "acyclic"));

    // budgets: agents only, over budget escalates instead of landing.
    if actor_is_agent {
        let ops = check::infer_range(cfg, repo, base, head)?;
        for (path, _ops) in &ops {
            let full = repo.join(path);
            let Ok(text) = std::fs::read_to_string(&full) else { continue };
            let Ok(doc) = parse_document(&text) else { continue };
            let layer = doc.frontmatter.layer.unwrap_or(cfg.kb.defaults.layer);
            let budget = cfg.budget_for(layer);
            let words = parse_body(&doc.body).word_count;
            if words > budget {
                let detail = format!("{path}: {words} words exceeds the {layer} budget of {budget}");
                log.check("budget", false, Some(&detail))?;
                checks.push(CheckResult::fail("budget", &detail));
                return Ok(PolicyOutcome::Fail(format!("budget; escalate: {detail}")));
            }
        }
    }
    checks.push(CheckResult::ok("budget", "within limits"));

    // prompt checksums: a mismatch between manifest.toml and the worktree's prompts/ fails the run.
    if !cfg.manifest.checksums.is_empty() {
        let mut mismatches = Vec::new();
        for (name, expected) in &cfg.manifest.checksums {
            let p = repo.join("prompts").join(name);
            if let Ok(bytes) = std::fs::read(&p) {
                let actual = crate::parse::frontmatter::sha256_hex(&bytes);
                if &actual != expected {
                    mismatches.push(name.clone());
                }
            }
        }
        log.check("prompt-checksums", mismatches.is_empty(), (!mismatches.is_empty()).then(|| mismatches.join(", ")).as_deref())?;
        if !mismatches.is_empty() {
            checks.push(CheckResult::fail("prompt-checksums", mismatches.join(", ")));
            return Ok(PolicyOutcome::Fail(format!("prompt checksum mismatch: {}", mismatches.join(", "))));
        }
    }
    checks.push(CheckResult::ok("prompt-checksums", "match"));

    // delete guard: design.md §17.4.
    for (status, old_path, _new_path) in &changed_docs {
        if *status != 'D' {
            continue;
        }
        if let Err(reason) = check_delete_guard(cfg, repo, base, old_path, &deleted_paths) {
            if actor_is_agent {
                log.check("delete-guard", false, Some(&reason))?;
                checks.push(CheckResult::fail("delete-guard", &reason));
                return Ok(PolicyOutcome::Fail(format!("delete guard: {reason}")));
            }
            // humans are never blocked (design §10): the deletion is recorded and, since every
            // deletion is review class in the structure gate, a human still approves it.
            log.check("delete-guard", true, Some(&format!("human deletion, guard not met: {reason}")))?;
            checks.push(CheckResult::ok("delete-guard", format!("human deletion; review class ({reason})")));
            return Ok(PolicyOutcome::Ok);
        }
    }
    checks.push(CheckResult::ok("delete-guard", "ok"));

    Ok(PolicyOutcome::Ok)
}

fn collect_docs_at(repo: &Path, rev: &str) -> Result<Vec<(String, crate::model::Frontmatter)>> {
    let mut out = Vec::new();
    for path in git::ls_tree(repo, rev, "docs/")? {
        if !path.ends_with(".md") {
            continue;
        }
        let Ok(bytes) = git::show_file(repo, rev, &path) else { continue };
        let Ok(text) = String::from_utf8(bytes) else { continue };
        let Ok(doc) = parse_document(&text) else { continue };
        if let Some(id) = doc.frontmatter.id.clone() {
            out.push((id, doc.frontmatter));
        }
    }
    Ok(out)
}

/// design.md §17.4: a document may be deleted only when its base status is superseded, archived,
/// or a draft older than `draft_ttl_days`, and it has no remaining canon dependent (other than one
/// deleted in the same session). the "retire and delete not in one session" and code-repository
/// mirror halves of this guard are not checked here (a session that squashes to one commit per
/// design.md §12 leaves no visible mid-session `retire` state to check against, and no code
/// repository mirror is configured in this local subset); see the report for this gap.
fn check_delete_guard(cfg: &Config, repo: &Path, base: &str, path: &str, deleted_paths: &BTreeSet<String>) -> std::result::Result<(), String> {
    let bytes = git::show_file(repo, base, path).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let doc = parse_document(&text).map_err(|e| e.to_string())?;
    let id = doc.frontmatter.id.clone().unwrap_or_default();

    let ok_status = match doc.frontmatter.status {
        Some(Status::Superseded) | Some(Status::Archived) => true,
        Some(Status::Draft) => match git::last_commit_date(repo, path) {
            Some(date) => chrono::DateTime::parse_from_rfc3339(&date)
                .map(|d| (chrono::Utc::now() - d.with_timezone(&chrono::Utc)).num_days() as u32 > cfg.manifest.thresholds.draft_ttl_days)
                .unwrap_or(false),
            None => false,
        },
        _ => false,
    };
    if !ok_status {
        return Err(format!("{path}: status must be superseded, archived, or a draft older than {} days to delete", cfg.manifest.thresholds.draft_ttl_days));
    }
    // backed-up precondition (design §17.4): the file's last commit must be at or before the
    // newest mirrored and bundled commits; without a backup configured this passes with a note.
    if let Some(sha) = git::last_commit_sha(repo, path) {
        let (ok, detail) = crate::backup::delete_allowed_by_backup(cfg, &sha).map_err(|e| e.to_string())?;
        if !ok {
            return Err(format!("{path}: {detail}"));
        }
    }

    if !id.is_empty() {
        let pattern = format!("kb:{id}");
        let hits = git::grep_literal(repo, base, &pattern, "docs/").map_err(|e| e.to_string())?;
        for hit_path in hits {
            if hit_path == path || deleted_paths.contains(&hit_path) {
                continue;
            }
            let Ok(bytes) = git::show_file(repo, base, &hit_path) else { continue };
            let Ok(text) = String::from_utf8(bytes) else { continue };
            let Ok(hit_doc) = parse_document(&text) else { continue };
            if hit_doc.frontmatter.status == Some(Status::Canon) {
                return Err(format!("{path}: still referenced by canon document {hit_path}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// step 4: structure gate (design.md §15.1)
// ---------------------------------------------------------------------------------------------

fn run_structure_gate(cfg: &Config, session: &Session, base: &str, head: &str, actor_is_agent: bool, log: &mut RunLog) -> Result<check::GateDecision> {
    let repo = &session.worktree;
    let changed = git::diff_name_status(repo, base, head)?;
    let band_conn = index::open_newest(cfg).ok();
    // design.md §17.4: below `min_corpus` canon documents the band is undefined, and merge-time
    // classification falls back to layer and `agent_policy` rules only. an index built over a
    // tiny fixture (as in these tests, and any brand-new repository) still writes a `crit_band`
    // column per document, so this has to be checked explicitly rather than trusting a `Some`.
    let corpus_ok = band_conn
        .as_ref()
        .map(|c| {
            let count: i64 =
                c.query_row("select count(*) from documents where kind = 'document' and status = 'canon'", [], |r| r.get(0)).unwrap_or(0);
            count as u64 >= cfg.manifest.thresholds.min_corpus as u64
        })
        .unwrap_or(false);

    let mut class = MergeClass::Auto;
    let mut reasons = Vec::new();
    for (status, old_path, new_path) in changed {
        let path = if new_path.is_empty() { old_path.clone() } else { new_path.clone() };
        if !path.starts_with("docs/") || !path.ends_with(".md") {
            continue;
        }
        let base_doc = if status == 'A' {
            None
        } else {
            git::show_file(repo, base, &old_path).ok().and_then(|b| String::from_utf8(b).ok()).and_then(|t| parse_document(&t).ok())
        };
        let Some(head_doc) = (if status == 'D' {
            None
        } else {
            git::show_file(repo, head, &path).ok().and_then(|b| String::from_utf8(b).ok()).and_then(|t| parse_document(&t).ok())
        }) else {
            // a deletion is review class always (design §11, §15.3); the delete guard checks its
            // preconditions separately.
            class = MergeClass::Review;
            let label = base_doc.as_ref().and_then(|d| d.frontmatter.id.clone()).unwrap_or(path.clone());
            reasons.push(format!("{label}: delete is review class"));
            continue;
        };

        let doc_id = head_doc.frontmatter.id.clone().or_else(|| base_doc.as_ref().and_then(|d| d.frontmatter.id.clone()));
        let band = if corpus_ok {
            doc_id
                .as_ref()
                .and_then(|id| band_conn.as_ref().and_then(|c| index::get(c, id).ok().flatten()))
                .and_then(|rec| rec.crit_band)
                .map(|b| if b == "low" { MergeClass::Auto } else { MergeClass::Review })
        } else {
            None
        };

        let decision = check::classify_change(cfg, base_doc.as_ref(), &head_doc, band, actor_is_agent);
        if decision.class == MergeClass::Review {
            class = MergeClass::Review;
            let label = doc_id.unwrap_or(path);
            for r in decision.reasons {
                reasons.push(format!("{label}: {r}"));
            }
        }
    }
    log.gate(class, &reasons)?;
    Ok(check::GateDecision { class, reasons })
}

// ---------------------------------------------------------------------------------------------
// step 5: quick gate, then content review (design.md §15.2, `review::gather` + `review::review`
// + `review::decide`; the quick gate is an addendum ahead of the full review call, not in the
// design doc's own text)
// ---------------------------------------------------------------------------------------------

enum QuickGate {
    /// the gate said "trivial" at or above `review_confidence`: full review is skipped.
    Skip { p: f64 },
    /// the gate was never asked (agent actor, review-class change, or no `gate` provider), or it
    /// answered but not confidently enough: full review runs as it always has.
    RunFull,
}

/// ask the `gate` role, when one is configured, whether a settled human change on an
/// already-auto-class session is trivial enough to skip the full content review. logs a `check`
/// event with the gate's answer whenever it is actually asked; when it is not asked at all
/// (an agent actor, a review-class change, or no `gate` provider configured) there is no answer
/// to log, and `content_review` runs exactly as it did before this existed.
fn quick_gate_check(
    cfg: &Config,
    session: &Session,
    head: &str,
    class: MergeClass,
    actor_is_agent: bool,
    gate_override: Option<&dyn Provider>,
    log: &mut RunLog,
) -> Result<QuickGate> {
    if actor_is_agent || class != MergeClass::Auto {
        return Ok(QuickGate::RunFull);
    }

    let message = git::commit_message(&session.worktree, head).unwrap_or_default();
    let diff = git::run(&session.worktree, &["diff", &session.base_commit, head, "--", "docs/"]).unwrap_or_default();
    let text = provider::truncate(&format!("commit message:\n{message}\n\ndiff:\n{diff}"), 6_000);
    let question = provider::gate::GateQuestion {
        labels: vec!["trivial".to_string(), "substantive".to_string()],
        text,
        context: Some("trivial means formatting, typos, link fixes, or wording with no change of meaning".to_string()),
    };

    let answer = match gate_override {
        Some(p) => Some(provider::gate::gate_with(p, cfg, &question)?),
        None => provider::gate::gate(cfg, &question)?,
    };
    let Some(answer) = answer else { return Ok(QuickGate::RunFull) };

    let detail = format!("label={} p={:.2}: {}", answer.label, answer.p, answer.reason);
    log.check("quick-gate", answer.label == "trivial", Some(detail.as_str()))?;

    if answer.label == "trivial" && answer.p >= cfg.manifest.thresholds.review_confidence {
        Ok(QuickGate::Skip { p: answer.p })
    } else {
        Ok(QuickGate::RunFull)
    }
}

/// build the review packet's input and run it. `Ok(None)` when no `review` provider is
/// configured (the design's documented self-review fallback), not an error.
fn content_review(cfg: &Config, session: &Session, main_sha: &str, head: &str) -> Result<Option<review::ReviewOutput>> {
    let justification = git::commit_message(&cfg.root, head).unwrap_or_default();
    let main = if main_sha == session.base_commit { None } else { Some(main_sha.to_string()) };
    let input = review::ReviewInput {
        session_id: Some(session.id.clone()),
        base: session.base_commit.clone(),
        head: head.to_string(),
        main,
        justification,
        actor: session.actor.clone(),
        context_excerpts: vec![],
        session_facts: Some(review::SessionFacts {
            age_hours: session_age_hours(session),
            wip_count: session.ops,
            settled_by_eod: session.eod.unwrap_or(false),
            uncommitted_at_settle: 0,
        }),
    };
    review::review(cfg, &input)
}

fn session_age_hours(session: &Session) -> f64 {
    chrono::DateTime::parse_from_rfc3339(&session.opened_at)
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 3600.0)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------------------------
// conflicts (design.md §14, `review::gather_conflict` + `review::resolve` + `review::should_escalate`)
// ---------------------------------------------------------------------------------------------

/// build the terminal `MergeOutcome` for a conflict a human must resolve: worktree left
/// mid-rebase, session state `NeedsHuman`.
fn handle_conflict_escalate(cfg: &Config, session: &mut Session, mut log: RunLog, run_id: String, paths: Vec<String>) -> Result<MergeOutcome> {
    log.conflict(&paths.join(","), serde_json::json!({"paths": paths}), "escalated: no automatic resolution")?;
    session.state = SessionState::NeedsHuman;
    session::save(cfg, session)?;
    let reasons = vec![format!("rebase conflict on {} path(s); worktree left mid-rebase for a human to resolve", paths.len())];
    let checks = vec![CheckResult::fail("rebase", reasons[0].clone())];
    log.run_end(RunStatus::Escalated, None, None, None, &[], &[])?;
    log.seal(cfg).ok();
    Ok(MergeOutcome { landed: None, class: MergeClass::Review, state: SessionState::NeedsHuman, checks, reasons, run_id })
}

/// resolve every path conflicting at the current rebase stop, for as many stops as the rebase
/// needs (design.md §12's "never more than two" settled commits means this is at most one or two
/// stops in practice), and drive `git rebase --continue` between them. `Ok(None)` on any escalate
/// signal (no resolver configured, a contradiction, low confidence, more than 5 paths, or a
/// repeated path within one stop — repeats *across* stops are not tracked, a documented
/// simplification; see the report) leaves the worktree mid-rebase for the caller to escalate.
/// `Ok(Some((head, force_review)))` on success; `force_review` is set when a resolved conflict
/// touched a locked-layer document or one of `owner`/`access`/`status`, per design.md §14.
fn resolve_conflicts_once(cfg: &Config, session: &Session, log: &mut RunLog) -> Result<Option<(String, bool)>> {
    let repo = session.worktree.clone();
    let mut force_review = false;
    let mut seen_before: BTreeSet<String> = BTreeSet::new();

    loop {
        let paths = git::conflicted_paths(&repo)?;
        if paths.is_empty() {
            return Ok(Some((git::head(&repo)?, force_review)));
        }
        let Ok(theirs_sha) = git::rev_parse(&repo, "REBASE_HEAD") else { return Ok(None) };
        let Ok(ours_sha) = git::head(&repo) else { return Ok(None) };

        for path in &paths {
            let repeat = !seen_before.insert(path.clone());
            let input = review::gather_conflict(&repo, path, &session.base_commit, &ours_sha, &theirs_sha)?;
            let Some(out) = review::resolve(cfg, &input)? else { return Ok(None) };

            let ours_doc = parse_document(&input.ours_text).ok();
            let theirs_doc = parse_document(&input.theirs_text).ok();
            let is_locked = theirs_doc
                .as_ref()
                .map(|d| {
                    let layer = d.frontmatter.layer.unwrap_or(cfg.kb.defaults.layer);
                    d.frontmatter.agent_policy.unwrap_or_else(|| cfg.agent_policy_for(layer)) == AgentPolicy::Locked
                })
                .unwrap_or(false);
            let policy_conflict = match (&ours_doc, &theirs_doc) {
                (Some(o), Some(t)) => o.frontmatter.owner != t.frontmatter.owner || o.frontmatter.access != t.frontmatter.access || o.frontmatter.status != t.frontmatter.status,
                _ => false,
            };

            if let Some(reason) = review::should_escalate(&out, paths.len(), is_locked, repeat) {
                log.conflict(path, serde_json::json!({"confidence": out.confidence, "contradiction": out.contradiction}), &format!("escalated: {reason}"))?;
                return Ok(None);
            }
            if is_locked || policy_conflict {
                force_review = true;
            }

            std::fs::write(repo.join(path), &out.content)?;
            git::run(&repo, &["add", path])?;
            log.conflict(path, serde_json::json!({"confidence": out.confidence}), &out.summary)?;
        }

        let continue_res = git::rebase_continue(&repo)?;
        if !continue_res.clean {
            if git::rebase_in_progress(&repo) {
                continue; // another commit in this rebase conflicted; resolve that stop too.
            }
            return Ok(None);
        }

        let current_msg = git::commit_message(&repo, "HEAD").unwrap_or_default();
        let trailers: String = paths.iter().map(|p| format!("Resolved-conflict: {p} against {ours_sha}\n")).collect();
        let new_msg = format!("{}\n{trailers}Resolution-run: {}", current_msg.trim_end(), log.run_id());
        let new_head = git::amend_message(&repo, &new_msg)?;
        return Ok(Some((new_head, force_review)));
    }
}

// ---------------------------------------------------------------------------------------------
// review queue: list, approve, reject (design.md §13's review class)
// ---------------------------------------------------------------------------------------------

/// `kb review list`: unmerged `session/*` branches carrying a `kb-review` note.
pub fn review_list(cfg: &Config) -> Result<Vec<ReviewItem>> {
    let mut out = Vec::new();
    for branch in git::list_refs(&cfg.root, "refs/heads/session/*")? {
        let Ok(head) = git::rev_parse(&cfg.root, &branch) else { continue };
        let Some(note_text) = git::notes_show(&cfg.root, REVIEW_NOTES_REF, &head)? else { continue };
        // the note may carry an appended `approved-by`/`rejected` block after the toml table;
        // the table itself parses fine with `toml::from_str` since extra trailing lines outside
        // any key are rejected by the parser, so only take the leading well-formed portion.
        let Ok(note) = toml::from_str::<ReviewNote>(leading_toml_table(&note_text)) else { continue };
        out.push(ReviewItem { session: note.session.clone(), branch, head, note });
    }
    Ok(out)
}

/// the note's first toml table (up to a line that does not start with a `[`, `#`, or `key =`
/// styled entry belonging to it); `notes_append` adds plain `key = "value"` lines after it, which
/// still parse as part of the same flat table, so this just returns the whole text today. kept as
/// a named seam in case a future note shape needs real splitting.
fn leading_toml_table(text: &str) -> &str {
    text
}

/// `kb review approve <session>`: the approver must be in the write group of every changed
/// document (or `groups.toml` is empty, allowed with a warning), then `merge` reruns with
/// `approve: true`.
pub fn review_approve(cfg: &Config, session_id: &str, approver: Option<String>) -> Result<MergeOutcome> {
    let session = session::load(cfg, session_id)?;
    let principal = match approver {
        Some(p) => p,
        None => {
            let (_, email) = git::identity(&cfg.root);
            let email = email.unwrap_or_else(|| "unknown".to_string());
            cfg.principal_for_email(&email).unwrap_or(email)
        }
    };

    let groups_empty = cfg.groups.principals.is_empty() && cfg.groups.groups.is_empty();
    if groups_empty {
        eprintln!("warning: groups.toml has no principals or groups; allowing approval by `{principal}` without a membership check");
    } else {
        let head = git::head(&session.worktree)?;
        let changed = git::diff_name_status(&session.worktree, &session.base_commit, &head)?;
        let groups = cfg.groups_of(&principal);
        for (status, old_path, new_path) in &changed {
            let (rev, path) = if *status == 'D' { (session.base_commit.as_str(), old_path) } else { (head.as_str(), new_path) };
            if !path.starts_with("docs/") || !path.ends_with(".md") {
                continue;
            }
            let Ok(bytes) = git::show_file(&session.worktree, rev, path) else { continue };
            let Ok(text) = String::from_utf8(bytes) else { continue };
            let Ok(doc) = parse_document(&text) else { continue };
            let write = match &doc.frontmatter.access {
                Some(a) => a.write.clone(),
                None => {
                    let mut w = vec!["owner".to_string()];
                    w.extend(cfg.kb.defaults.write_groups.iter().cloned());
                    w
                }
            };
            let owner = doc.frontmatter.owner.clone().unwrap_or_default();
            let allowed = write.iter().any(|w| w == "*")
                || write.iter().any(|w| w == &principal)
                || (write.iter().any(|w| w == "owner") && principal == owner)
                || write.iter().any(|w| groups.contains(w));
            if !allowed {
                bail!("`{principal}` is not in the write group for {path}; approval refused");
            }
        }
    }

    let head = git::head(&session.worktree)?;
    git::notes_append(&cfg.root, REVIEW_NOTES_REF, &head, &format!("approved-by = \"{principal}\"\n"))?;

    merge(cfg, session_id, MergeOpts { check_only: false, approve: true, actor: None })
}

/// `kb review reject <session> --reason <text>`: appends a note and sends the session back to
/// its author.
pub fn review_reject(cfg: &Config, session_id: &str, reason: &str) -> Result<()> {
    let mut session = session::load(cfg, session_id)?;
    let head = git::head(&session.worktree)?;
    let escaped = reason.replace('"', "'");
    git::notes_append(&cfg.root, REVIEW_NOTES_REF, &head, &format!("rejected = true\nreject_reason = \"{escaped}\"\n"))?;
    session.state = SessionState::Settled;
    session::save(cfg, &session)?;
    println!("session {} rejected: {reason}", session.id);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! the quick gate's skip and no-skip paths (design.md §15.2's addendum), against a
    //! `FakeProvider` gate answer injected through `merge_with_providers`'s `gate_override`
    //! seam -- no real `gate` role is configured in these fixtures.

    use super::*;
    use crate::provider::fake::FakeProvider;

    const ID: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH2A";

    fn doc(body: &str) -> String {
        format!("+++\nid = \"{ID}\"\nlayer = \"reference\"\ntitle = \"quick gate doc\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# quick gate doc\n\n{body}\n")
    }

    /// a repo with the `kb init` skeleton plus one committed reference-layer document, then a
    /// human session that makes one small edit to it (auto class under the structure gate: a
    /// `reference` document, `editable`, no locked fields touched) and settles.
    fn repo_with_settled_edit() -> (tempfile::TempDir, Config, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::cli::init::init(root, false, false).unwrap();
        git::run(root, &["config", "user.name", "test user"]).unwrap();
        git::run(root, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(root.join("docs").join(format!("{ID}.md")), doc("original text.")).unwrap();
        git::commit_all(root, "seed quick gate doc").unwrap();

        let cfg = Config::load(root).unwrap();
        let mut s = session::open(&cfg, "edit quick gate doc", None, &[]).unwrap();
        std::fs::write(s.worktree.join("docs").join(format!("{ID}.md")), doc("revised text.")).unwrap();
        git::commit_all(&s.worktree, "wip: revise").unwrap();
        session::settle(&cfg, &mut s, None).unwrap();
        (dir, cfg, s)
    }

    #[test]
    fn quick_gate_skips_full_review_when_trivial_and_confident() {
        let (_dir, cfg, s) = repo_with_settled_edit();
        let threshold = cfg.manifest.thresholds.review_confidence;
        let gate = FakeProvider::new(vec![format!(r#"{{"label": "trivial", "p": {:.2}, "reason": "wording only"}}"#, threshold + 0.05)]);

        let outcome = merge_with_providers(&cfg, &s.id, MergeOpts::default(), Some(&gate)).unwrap();

        assert!(outcome.landed.is_some(), "{outcome:?}");
        assert_eq!(outcome.class, MergeClass::Auto);
        let review_check = outcome.checks.iter().find(|c| c.name == "review").expect("a review check is always recorded");
        assert!(review_check.pass);
        assert!(review_check.detail.contains("quick gate: trivial"), "{review_check:?}");
        assert!(review_check.detail.contains("full review skipped"), "{review_check:?}");
    }

    #[test]
    fn quick_gate_runs_full_review_when_substantive() {
        let (_dir, cfg, s) = repo_with_settled_edit();
        let gate = FakeProvider::new(vec![r#"{"label": "substantive", "p": 0.9, "reason": "changes the meaning"}"#.to_string()]);

        let outcome = merge_with_providers(&cfg, &s.id, MergeOpts::default(), Some(&gate)).unwrap();

        assert!(outcome.landed.is_some(), "{outcome:?}");
        let review_check = outcome.checks.iter().find(|c| c.name == "review").expect("a review check is always recorded");
        assert!(review_check.pass);
        // no `gate` provider is configured for role "review", so the full review falls back to
        // self-review, same as it always has -- proof the quick gate did not short-circuit it.
        assert!(review_check.detail.contains("self_review"), "{review_check:?}");
        assert!(!review_check.detail.contains("quick gate"), "{review_check:?}");
    }

    #[test]
    fn quick_gate_runs_full_review_when_not_confident_enough() {
        let (_dir, cfg, s) = repo_with_settled_edit();
        let threshold = cfg.manifest.thresholds.review_confidence;
        let gate = FakeProvider::new(vec![format!(r#"{{"label": "trivial", "p": {:.2}, "reason": "borderline"}}"#, threshold - 0.05)]);

        let outcome = merge_with_providers(&cfg, &s.id, MergeOpts::default(), Some(&gate)).unwrap();

        assert!(outcome.landed.is_some(), "{outcome:?}");
        let review_check = outcome.checks.iter().find(|c| c.name == "review").expect("a review check is always recorded");
        assert!(review_check.detail.contains("self_review"), "{review_check:?}");
    }

    #[test]
    fn no_gate_override_and_no_configured_provider_runs_full_review_as_before() {
        let (_dir, cfg, s) = repo_with_settled_edit();
        let outcome = merge_with_providers(&cfg, &s.id, MergeOpts::default(), None).unwrap();
        assert!(outcome.landed.is_some(), "{outcome:?}");
        let review_check = outcome.checks.iter().find(|c| c.name == "review").unwrap();
        assert!(review_check.detail.contains("self_review"), "{review_check:?}");
    }
}
