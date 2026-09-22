//! `kb policy` subcommands: decide one operation, or list every operation an actor may currently
//! perform on a document. not wired into `cli::mod`; see this crate's report for the wiring
//! snippet.
//!
//! neither command consults the index: `band` is always `None` and `in_canon`/`in_code` are
//! always `0`, so a document below its criticality band or with canon/code dependents reads as
//! if the corpus were below the minimum for banding. run `kb index build` and read the artifact
//! for the real numbers before relying on a `decide` or `allowed` result for anything but a quick
//! local check.

use crate::config::Config;
use crate::model::{MergeClass, Operation};
use crate::parse::{parse_body, parse_document};
use crate::policy::{Actor, DocPolicyView, OpRequest, allowed_ops, decide};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum PolicyCommand {
    /// decide whether `actor` may perform `op` on the document at `path`, and at what merge
    /// class. exits 2 when denied.
    Decide {
        /// `human:<principal>` or `agent:<skill>@<principal>`
        #[arg(long)]
        actor: String,
        /// one of: create, append, revise, rewrite, supersede, split, merge, link, retire,
        /// delete, escalate
        #[arg(long)]
        op: String,
        path: PathBuf,
        /// slug path of the section this operation targets, for `revise` and `link`
        #[arg(long)]
        section: Option<String>,
        /// words this operation would add, for the budget guard
        #[arg(long)]
        added_words: Option<usize>,
    },
    /// list every operation `actor` may currently perform on the document at `path`, with its
    /// merge class and reasons.
    Allowed {
        #[arg(long)]
        actor: String,
        path: PathBuf,
    },
}

pub fn run(cfg: &Config, cmd: PolicyCommand, json: bool) -> Result<i32> {
    match cmd {
        PolicyCommand::Decide { actor, op, path, section, added_words } => run_decide(cfg, &actor, &op, &path, section, added_words, json),
        PolicyCommand::Allowed { actor, path } => run_allowed(cfg, &actor, &path, json),
    }
}

fn run_decide(
    cfg: &Config,
    actor_str: &str,
    op_str: &str,
    path: &Path,
    section: Option<String>,
    added_words: Option<usize>,
    json: bool,
) -> Result<i32> {
    let actor = Actor::parse(actor_str).map_err(anyhow::Error::msg)?;
    let op = parse_op(op_str)?;
    let doc = load_doc_view(cfg, path)?;
    let req = OpRequest { op, section, field: None, added_words: added_words.unwrap_or(0), target_layer: None };
    let decision = decide(cfg, &actor, Some(&doc), &req);
    print_decision(&decision, json)?;
    Ok(if decision.allow { 0 } else { 2 })
}

fn run_allowed(cfg: &Config, actor_str: &str, path: &Path, json: bool) -> Result<i32> {
    let actor = Actor::parse(actor_str).map_err(anyhow::Error::msg)?;
    let doc = load_doc_view(cfg, path)?;
    let ops = allowed_ops(cfg, &actor, &doc);
    if json {
        println!("{}", serde_json::to_string_pretty(&ops)?);
    } else {
        for a in &ops {
            let class = if a.class == MergeClass::Review { "review" } else { "auto" };
            println!("{} {class}", a.op);
            for reason in &a.reasons {
                println!("  - {reason}");
            }
        }
    }
    Ok(0)
}

/// parse the file at `path` and build the `DocPolicyView` the policy function reads. the index is
/// not consulted (see the module doc comment): `band` is `None`, `in_canon` and `in_code` are `0`.
fn load_doc_view(cfg: &Config, path: &Path) -> Result<DocPolicyView> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc = parse_document(&text)?;
    let body = parse_body(&doc.body);
    DocPolicyView::from_frontmatter(cfg, &doc.frontmatter, body.word_count, None, 0, 0)
}

fn parse_op(s: &str) -> Result<Operation> {
    Ok(match s {
        "create" => Operation::Create,
        "append" => Operation::Append,
        "revise" => Operation::Revise,
        "rewrite" => Operation::Rewrite,
        "supersede" => Operation::Supersede,
        "split" => Operation::Split,
        "merge" => Operation::Merge,
        "link" => Operation::Link,
        "retire" => Operation::Retire,
        "delete" => Operation::Delete,
        "escalate" => Operation::Escalate,
        other => bail!("unknown operation `{other}`"),
    })
}

fn print_decision(d: &crate::policy::Decision, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(d)?);
    } else {
        println!("{} {}", if d.allow { "allow" } else { "deny" }, if d.class == MergeClass::Review { "review" } else { "auto" });
        for reason in &d.reasons {
            println!("  - {reason}");
        }
        if let Some(op) = d.suggest {
            println!("  suggest: {op}");
        }
    }
    Ok(())
}
