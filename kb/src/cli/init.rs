//! `kb init`: lay down a knowledge base repository skeleton and install the pre-commit hook.
use anyhow::{Context, Result};
use std::path::Path;

pub const KB_TOML: &str = r#"# kb.toml: deployment configuration. see design.md §5, §10, §19.

[store]
backend = "local"        # local | object
path = ".kb/store"

[defaults]
layer = "reference"
write_groups = []

# role -> provider. roles: classify, curate, review, resolve, answer, contextualize, embed, rerank.
# kinds: anthropic | openai | ollama. keys come from the named environment variable, never from this file.
# [providers.classify]
# kind = "ollama"
# model = "llama3.2"
# base_url = "http://localhost:11434"

[secrets]
rules = []
allow_paths = []

# code repositories to grep for kb:<ulid> back-references
# [[code_repos]]
# name = "platform"
# url = "git@github.com:org/platform.git"
# default_branch = "main"
"#;

pub const MANIFEST_TOML: &str = r#"# manifest.toml: versions, budgets, weights, thresholds. see design.md §30.
parser_version = 1
chunker_version = 1

[budgets]
principle = 1000
design = 4000
decision = 1500
procedure = 3000
reference = 6000

[thresholds]
criticality_band_pct = 20.0
min_corpus = 50
near_duplicate = 0.92
review_confidence = 0.7
nightly_revert_confidence = 0.9
draft_ttl_days = 30
superseded_delete_days = 90
orphan_days = 90
classify_retries = 3
review_retries = 2
# false: a person approves every agent change; turn on only after design.md §31's eval gates pass
agent_auto_merge = false

[weights]
in_canon = 0.25
in_code = 0.30
pagerank = 0.20
reads_90d = 0.15
cites_90d = 0.10
pagerank_damping = 0.85

[structure.principle]
critical = ["*"]

[structure.design]
critical = ["*"]

[structure.decision]
critical = ["decision", "consequences"]

[structure.procedure]
critical = ["prerequisites", "rollback"]

[packet]
max_tokens = 32000
"#;

pub const GROUPS_TOML: &str = r#"# groups.toml: principals and groups. no secrets live here. see design.md §10.

# [principals.alice]
# kind = "human"
# emails = ["alice@example.com"]
# groups = ["admins"]

# [principals.svc-curate]
# kind = "service"
# owner = "alice"
# groups = ["agents"]
# layer_ceiling = "procedure"

# [groups.admins]
# members = ["alice"]
"#;

pub const STYLE_MD: &str = r#"write plainly. one idea per sentence. use the reader's terms, not jargon. state facts and cite sources by reference. omit needless words. never invent a fact you cannot point to. when unsure, escalate instead of guessing.
"#;

pub const EVAL_TOML: &str = r#"# eval/queries.toml: retrieval evaluation set. see design.md §31.
# [[query]]
# text = "how do we set a static ip on the launcher"
# expected = ["kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N"]
# area = "ops"
"#;

pub const GITIGNORE: &str = ".kb/\n";

pub const PRE_COMMIT_TEMPLATE: &str = r#"#!/bin/sh
# installed by kb init. fills frontmatter, validates, and blocks secrets on staged documents.
# KB_BIN overrides the binary; otherwise `kb` on PATH, otherwise the binary that installed this hook.
set -e
KB="${KB_BIN:-$(command -v kb 2>/dev/null || echo "__KB_PATH__")}"
"$KB" check validate --fix --staged || exit 1
"$KB" check secrets --staged || exit 1
"#;

/// the hook text with the absolute path of the running binary as the fallback.
pub fn pre_commit_hook() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kb".into());
    PRE_COMMIT_TEMPLATE.replace("__KB_PATH__", &exe)
}

/// whether `existing` is a hook we generated, allowing for a different embedded fallback binary
/// path (`__KB_PATH__`): that differs across machines and builds, so it is not a meaningful
/// difference for idempotency purposes.
fn is_our_pre_commit_hook(existing: &str) -> bool {
    let mut parts = PRE_COMMIT_TEMPLATE.splitn(2, "__KB_PATH__");
    let prefix = parts.next().unwrap_or("");
    let suffix = parts.next().unwrap_or("");
    existing.len() >= prefix.len() + suffix.len() && existing.starts_with(prefix) && existing.ends_with(suffix)
}

/// create the skeleton under `root`. existing files are left alone unless `force`.
pub fn init(root: &Path, force: bool, install_hook: bool) -> Result<Vec<String>> {
    let mut created = Vec::new();
    let write = |rel: &str, content: &str, created: &mut Vec<String>| -> Result<()> {
        let p = root.join(rel);
        if p.exists() && !force {
            return Ok(());
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content).with_context(|| format!("writing {}", p.display()))?;
        created.push(rel.to_string());
        Ok(())
    };
    for dir in ["docs", "schemas", "prompts", "eval", "redactions", ".kb"] {
        std::fs::create_dir_all(root.join(dir))?;
    }
    write("kb.toml", KB_TOML, &mut created)?;
    write("manifest.toml", MANIFEST_TOML, &mut created)?;
    write("groups.toml", GROUPS_TOML, &mut created)?;
    write("prompts/style.md", STYLE_MD, &mut created)?;
    write("eval/queries.toml", EVAL_TOML, &mut created)?;
    write("redactions/.gitkeep", "", &mut created)?;
    write("docs/.gitkeep", "", &mut created)?;
    if !root.join(".gitignore").exists() {
        write(".gitignore", GITIGNORE, &mut created)?;
    } else {
        let existing = std::fs::read_to_string(root.join(".gitignore"))?;
        if !existing.lines().any(|l| l.trim() == ".kb/" || l.trim() == ".kb") {
            std::fs::write(root.join(".gitignore"), format!("{existing}\n.kb/\n"))?;
            created.push(".gitignore (appended .kb/)".into());
        }
    }
    // a directory inside another repository (a dotfiles home, a project's scratch folder) gets
    // its own repository; otherwise every session branch would land in the enclosing one.
    let is_own_toplevel = crate::git::is_repo(root)
        && crate::git::toplevel(root).ok().and_then(|t| std::fs::canonicalize(t).ok()) == std::fs::canonicalize(root).ok();
    if !is_own_toplevel {
        crate::git::run(root, &["init", "-q", "-b", "main"]).context("git init")?;
        created.push(".git".into());
    }
    if install_hook {
        let hooks = crate::git::run(root, &["rev-parse", "--git-path", "hooks"])?;
        let hooks_dir = root.join(hooks.trim());
        std::fs::create_dir_all(&hooks_dir)?;
        let hook = hooks_dir.join("pre-commit");
        let write_hook = |created: &mut Vec<String>| -> Result<()> {
            std::fs::write(&hook, pre_commit_hook())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
            }
            created.push("pre-commit hook".into());
            Ok(())
        };
        if !hook.exists() {
            write_hook(&mut created)?;
        } else if force {
            // --force means "regenerate this regardless": report it even when the content
            // happens to already match, matching every other file this function writes.
            write_hook(&mut created)?;
        } else {
            let existing = std::fs::read_to_string(&hook).with_context(|| format!("reading {}", hook.display()))?;
            if is_our_pre_commit_hook(&existing) {
                // same hook (modulo the embedded binary path): nothing to do, silently.
            } else {
                created.push(format!(
                    "warning: pre-commit hook exists at {} and differs from kb's; rerun with --force to replace it",
                    hook.display()
                ));
            }
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_skeleton_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let created = init(dir.path(), false, true).unwrap();
        assert!(created.iter().any(|c| c == "kb.toml"));
        assert!(dir.path().join(".git").exists());
        let cfg = crate::config::Config::load(dir.path()).unwrap();
        assert_eq!(cfg.manifest.thresholds.min_corpus, 50);
        assert_eq!(cfg.budget_for(crate::model::Layer::Design), 4000);
        let again = init(dir.path(), false, false).unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn init_inside_another_repository_creates_its_own() {
        let outer = tempfile::tempdir().unwrap();
        crate::git::run(outer.path(), &["init", "-q", "-b", "main"]).unwrap();
        let inner = outer.path().join("scratch").join("kb");
        init(&inner, false, true).unwrap();
        assert!(inner.join(".git").exists(), "kb init attached to the enclosing repository");
        let top = std::fs::canonicalize(crate::git::toplevel(&inner).unwrap()).unwrap();
        assert_eq!(top, std::fs::canonicalize(&inner).unwrap());
    }
}
