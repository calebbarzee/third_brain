//! `kb check` subcommands: validate, infer, gate, graph, secrets.
//! not wired into `cli::mod`; see this crate's report for the wiring snippet.

use crate::check::{self, GateDecision};
use crate::config::Config;
use crate::git;
use crate::model::{Document, Finding, Frontmatter, MergeClass, Severity};
use crate::parse::parse_document;
use anyhow::{Result, bail};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum CheckCommand {
    /// parse, fill deterministic fields, and check every rule in design.md §6/§8; `--fix` writes.
    /// with no paths, validates every `docs/**/*.md` file and also checks duplicate ids and
    /// future-dated ulids across the corpus.
    Validate {
        paths: Vec<PathBuf>,
        #[arg(long)]
        fix: bool,
        /// validate the staged documents (added, copied, modified, renamed under docs/) and re-stage fixes
        #[arg(long)]
        staged: bool,
        /// with --fix, skip the `classify` provider call for documents still missing layer/tags
        #[arg(long)]
        no_classify: bool,
    },
    /// classify a git range's per-document diffs into the operation vocabulary (design.md §11).
    Infer { range: String },
    /// run the structure gate over a git range (design.md §15.1).
    Gate {
        range: String,
        /// classify as an agent change; omit for a human change.
        #[arg(long)]
        agent: bool,
    },
    /// check hierarchy and succession graph acyclicity over `docs/**/*.md` (design.md §8).
    Graph,
    /// scan for secrets (design.md §20): `--staged` for the index, a range for `base..head`,
    /// or every `docs/**/*.md` file when neither is given.
    Secrets {
        #[arg(long)]
        staged: bool,
        range: Option<String>,
    },
}

pub fn run(cfg: &Config, cmd: CheckCommand, json: bool) -> Result<i32> {
    match cmd {
        CheckCommand::Validate { paths, fix, staged, no_classify } => run_validate(cfg, paths, fix, staged, no_classify, json),
        CheckCommand::Infer { range } => run_infer(cfg, &range, json),
        CheckCommand::Gate { range, agent } => run_gate(cfg, &range, agent, json),
        CheckCommand::Graph => run_graph(cfg, json),
        CheckCommand::Secrets { staged, range } => run_secrets(cfg, staged, range, json),
    }
}

fn run_validate(cfg: &Config, mut paths: Vec<PathBuf>, fix: bool, staged: bool, no_classify: bool, json: bool) -> Result<i32> {
    let mut findings: Vec<Finding> = Vec::new();
    if staged {
        // staged documents come from the index, not the argument list
        let out = crate::git::run(&cfg.root, &["diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z", "--", "docs/"])?;
        paths = out.split('\0').filter(|p| p.ends_with(".md")).map(|p| cfg.root.join(p)).collect();
        if paths.is_empty() {
            return Ok(0);
        }
        let mut changed = false;
        for path in &paths {
            let mut o = check::validate_file(cfg, path, fix)?;
            maybe_classify(cfg, fix, no_classify, &mut o)?;
            findings.extend(o.findings.iter().cloned());
            changed |= o.changed;
        }
        if fix && changed {
            // re-stage everything under docs/ that validate touched, including renames
            crate::git::run(&cfg.root, &["add", "-A", "--", "docs/"])?;
        }
        print_findings(&findings);
        return Ok(exit_code(&findings));
    }
    if paths.is_empty() {
        let mut outcome = check::validate_repo(cfg, fix)?;
        for (_, file_outcome) in &mut outcome.files {
            maybe_classify(cfg, fix, no_classify, file_outcome)?;
        }
        for (_, file_outcome) in &outcome.files {
            findings.extend(file_outcome.findings.iter().cloned());
        }
        findings.extend(outcome.findings.iter().cloned());
        if json {
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        } else {
            print_findings(&findings);
        }
    } else {
        let mut outcomes = Vec::new();
        for path in paths {
            let mut o = check::validate_file(cfg, &path, fix)?;
            maybe_classify(cfg, fix, no_classify, &mut o)?;
            findings.extend(o.findings.iter().cloned());
            outcomes.push(o);
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&outcomes)?);
        } else {
            print_findings(&findings);
        }
    }
    Ok(exit_code(&findings))
}

/// design.md §6: after `--fix`'s deterministic fills, ask the configured `classify` provider for
/// any of `layer`/`tags` still missing, and write the result. a no-op with no `--fix`, with
/// `--no-classify`, when nothing is missing, or when no provider is configured for the role --
/// `validate_file` itself never calls a provider (see its module doc), so this lives here instead.
fn maybe_classify(cfg: &Config, fix: bool, no_classify: bool, outcome: &mut check::ValidateOutcome) -> Result<()> {
    if !fix || no_classify || check::missing_classifiable(&outcome.document.frontmatter).is_empty() {
        return Ok(());
    }
    let Some(suggestion) = crate::provider::classify::classify(cfg, &outcome.document, &[])? else {
        return Ok(());
    };
    let filled = crate::provider::classify::apply_classification(&mut outcome.document.frontmatter, &suggestion);
    if filled.is_empty() {
        return Ok(());
    }
    let rendered = crate::parse::render_document(&outcome.document.frontmatter, &outcome.document.body)?;
    std::fs::write(&outcome.path, rendered)?;
    outcome.changed = true;
    outcome.findings.push(Finding {
        severity: Severity::Info,
        code: "suggested".into(),
        message: format!("suggested: {}", filled.join(",")),
        path: Some(outcome.path.display().to_string()),
        line: None,
        doc_id: outcome.document.frontmatter.id.clone(),
    });
    Ok(())
}

fn run_infer(cfg: &Config, range: &str, json: bool) -> Result<i32> {
    let (base, head) = split_range(range)?;
    let rows = check::infer_range(cfg, &cfg.root, &base, &head)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for (path, ops) in &rows {
            println!("{path}");
            for op in ops {
                let addr = op.section.as_deref().or(op.field.as_deref()).unwrap_or("-");
                println!("  {} {addr}: {}", op.op, op.detail);
            }
        }
    }
    Ok(0)
}

fn run_gate(cfg: &Config, range: &str, agent: bool, json: bool) -> Result<i32> {
    let (base, head) = split_range(range)?;
    let rows = git::diff_name_status(&cfg.root, &base, &head)?;
    let mut results: Vec<(String, GateDecision)> = Vec::new();
    for (status, old_path, new_path) in rows {
        let path = if new_path.is_empty() { old_path.clone() } else { new_path.clone() };
        if !path.starts_with("docs/") || !path.ends_with(".md") {
            continue;
        }
        let base_doc = if status == 'A' {
            None
        } else {
            read_doc(&cfg.root, &base, &old_path)
        };
        let head_doc = if status == 'D' { None } else { read_doc(&cfg.root, &head, &path) };
        let decision = match &head_doc {
            Some(h) => check::classify_change(cfg, base_doc.as_ref(), h, None, agent),
            None => GateDecision {
                class: MergeClass::Review,
                reasons: vec!["document deleted".to_string()],
            },
        };
        results.push((path, decision));
    }
    let any_review = results.iter().any(|(_, d)| d.class == MergeClass::Review);
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for (path, decision) in &results {
            let class = if decision.class == MergeClass::Review { "review" } else { "auto" };
            println!("{path}: {class}");
            for reason in &decision.reasons {
                println!("  - {reason}");
            }
        }
    }
    Ok(if any_review { 1 } else { 0 })
}

fn run_graph(cfg: &Config, json: bool) -> Result<i32> {
    let docs = collect_docs(cfg)?;
    let findings = check::check_graph(&docs);
    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        print_findings(&findings);
    }
    Ok(exit_code(&findings))
}

fn run_secrets(cfg: &Config, staged: bool, range: Option<String>, json: bool) -> Result<i32> {
    let findings = if let Some(r) = &range {
        let (base, head) = split_range(r)?;
        check::scan_range(cfg, &cfg.root, &base, &head)?
    } else if staged {
        check::scan_staged(cfg, &cfg.root)?
    } else {
        scan_all_docs(cfg)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        print_findings(&findings);
    }
    Ok(exit_code(&findings))
}

fn scan_all_docs(cfg: &Config) -> Result<Vec<Finding>> {
    let docs_dir = cfg.root.join("docs");
    let mut findings = Vec::new();
    if docs_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&docs_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "md") {
                continue;
            }
            let rel = entry.path().strip_prefix(&cfg.root).unwrap_or(entry.path());
            let text = std::fs::read_to_string(entry.path())?;
            findings.extend(check::scan_text(cfg, &rel.display().to_string(), &text));
        }
    }
    Ok(findings)
}

fn collect_docs(cfg: &Config) -> Result<Vec<(String, Frontmatter)>> {
    let docs_dir = cfg.root.join("docs");
    let mut docs = Vec::new();
    if docs_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&docs_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() || entry.path().extension().is_none_or(|e| e != "md") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path())
                && let Ok(doc) = parse_document(&text)
                && let Some(id) = doc.frontmatter.id.clone()
            {
                docs.push((id, doc.frontmatter));
            }
        }
    }
    Ok(docs)
}

fn read_doc(repo: &std::path::Path, rev: &str, path: &str) -> Option<Document> {
    let bytes = git::show_file(repo, rev, path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    parse_document(&text).ok()
}

fn split_range(range: &str) -> Result<(String, String)> {
    match range.split_once("..") {
        Some((b, h)) => Ok((b.to_string(), if h.is_empty() { "HEAD".to_string() } else { h.to_string() })),
        None => bail!("range `{range}` must look like `base..head`"),
    }
}

fn exit_code(findings: &[Finding]) -> i32 {
    if findings.iter().any(|f| f.severity == Severity::Error) {
        1
    } else {
        0
    }
}

fn print_findings(findings: &[Finding]) {
    for f in findings {
        let severity = match f.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        let loc = match (&f.path, f.line) {
            (Some(p), Some(l)) => format!("{p}:{l}"),
            (Some(p), None) => p.clone(),
            _ => String::new(),
        };
        println!("{severity} [{}] {loc} {}", f.code, f.message);
    }
}
