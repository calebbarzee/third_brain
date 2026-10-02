//! `kb curate ...`: turn captures or a free-text proposal into operations inside a session.
//! not wired into `cli::Command` -- see this crate's report for the snippet to add there.
use crate::config::Config;
use crate::curate::{self, CurateInput, CurateReport};
use crate::git;
use anyhow::{Context, Result};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum CurateCommand {
    /// turn one or more captures into operations
    Captures {
        /// capture ids (or unique prefixes) to curate
        #[arg(long)]
        ids: Vec<String>,
        /// curate every capture with status "new" instead of a fixed --ids list
        #[arg(long)]
        all_new: bool,
        #[arg(long)]
        task: Option<String>,
        /// defaults to agent:curate@<principal of the current git identity>
        #[arg(long)]
        actor: Option<String>,
        /// print the proposed operations without opening a session or writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// turn a free-text proposal into operations
    Propose {
        /// the proposal text, inline
        text: Option<String>,
        /// read the proposal text from a file instead of the command line
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        task: String,
        /// defaults to agent:curate@<principal of the current git identity>
        #[arg(long)]
        actor: Option<String>,
        /// print the proposed operations without opening a session or writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// apply a hand-written ops json inside an existing session; how the deferred path completes
    Apply {
        #[arg(long)]
        session: String,
        #[arg(long)]
        ops_json: PathBuf,
    },
}

/// `agent:curate@<principal>`, resolving the principal from the current git identity the same
/// way `session::open`'s human default does.
fn default_actor(cfg: &Config) -> String {
    let (_, email) = git::identity(&cfg.root);
    let email = email.unwrap_or_else(|| "unknown".to_string());
    let principal = cfg.principal_for_email(&email).unwrap_or(email);
    format!("agent:curate@{principal}")
}

pub fn run(cfg: &Config, cmd: CurateCommand, json: bool) -> Result<i32> {
    match cmd {
        CurateCommand::Captures { ids, all_new, task, actor, dry_run } => {
            let capture_ids = if all_new {
                match curate::list_new_captures(cfg) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("could not list new captures: {e:#}");
                        vec![]
                    }
                }
            } else {
                ids
            };
            let input = CurateInput {
                actor: actor.unwrap_or_else(|| default_actor(cfg)),
                task: task.unwrap_or_else(|| "curate captures".to_string()),
                capture_ids,
                proposal_text: None,
                dry_run,
            };
            report(curate::run(cfg, input), json)
        }
        CurateCommand::Propose { text, file, task, actor, dry_run } => {
            let proposal_text = match (text, file) {
                (Some(t), _) => t,
                (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?,
                (None, None) => anyhow::bail!("propose needs either inline text or --file"),
            };
            let input =
                CurateInput { actor: actor.unwrap_or_else(|| default_actor(cfg)), task, capture_ids: vec![], proposal_text: Some(proposal_text), dry_run };
            report(curate::run(cfg, input), json)
        }
        CurateCommand::Apply { session, ops_json } => {
            let text = std::fs::read_to_string(&ops_json).with_context(|| format!("reading {}", ops_json.display()))?;
            let ops: Vec<curate::ProposedOp> = serde_json::from_str(&text).context("ops json does not match the expected shape")?;
            let mut s = crate::session::load(cfg, &session)?;
            let actor = crate::policy::Actor::parse(&s.actor).map_err(anyhow::Error::msg)?;
            let owner = crate::policy::responsible_party(cfg, &actor);
            for op in &ops {
                let decision = curate::decide_op(cfg, &actor, &s.worktree, op)?;
                if !decision.allow {
                    anyhow::bail!("{} {}: policy denied: {}", op.op, op.target_id.as_deref().unwrap_or("(new)"), decision.reasons.join("; "));
                }
                curate::ops::apply(cfg, &s.worktree, op, &owner)?;
                git::run(&s.worktree, &["add", "-A"])?;
                git::commit_all(&s.worktree, &format!("wip: {} {}", op.op, op.target_id.as_deref().unwrap_or("(new)")))?;
            }
            let commits = crate::session::settle(cfg, &mut s, None)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&commits)?);
            } else {
                for c in &commits {
                    println!("settled {c}");
                }
                println!("next: kb merge {}", s.id);
            }
            Ok(0)
        }
    }
}

fn report(result: Result<CurateReport>, json: bool) -> Result<i32> {
    match result {
        Ok(r) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else if r.deferred {
                println!("deferred: no provider configured for role \"curate\"; packet printed above (run {})", r.run_id);
            } else {
                println!(
                    "run {} session {} applied {} escalations {} discards {}",
                    r.run_id,
                    r.session_id.as_deref().unwrap_or("-"),
                    r.applied.len(),
                    r.escalations.len(),
                    r.discards
                );
                if !r.settled_commits.is_empty() {
                    println!("next: kb merge {}", r.session_id.as_deref().unwrap_or(""));
                }
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("curate failed: {e:#}");
            Ok(1)
        }
    }
}
