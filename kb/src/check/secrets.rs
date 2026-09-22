//! the secrets gate: a built-in ruleset plus `kb.toml [secrets]`. contract in design.md §20.
//! a finding never carries the matched text: only the rule id and a 4-character prefix.

use crate::config::Config;
use crate::git;
use crate::model::{Finding, Severity};
use anyhow::Result;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// (rule id, pattern, severity) for the built-in ruleset. all but the generic catch-all are
/// shaped closely enough to a real credential that a hit is an error; the generic pattern is a
/// warning, since `key = value` is common outside credentials too.
fn builtin_rules() -> &'static [(&'static str, &'static str, Severity)] {
    &[
        ("aws-access-key-id", r"\bAKIA[0-9A-Z]{16}\b", Severity::Error),
        (
            "aws-secret-key",
            r"(?i)aws[^\n]{0,40}[:=]\s*['\x22]?[A-Za-z0-9/+=]{40}['\x22]?",
            Severity::Error,
        ),
        ("github-pat", r"\bgh[pousr]_[A-Za-z0-9]{36,}\b", Severity::Error),
        ("github-fine-grained-pat", r"\bgithub_pat_[A-Za-z0-9_]{20,}\b", Severity::Error),
        ("slack-token", r"\bxox[abpors]-[A-Za-z0-9-]{10,}\b", Severity::Error),
        (
            "slack-webhook",
            r"https://hooks\.slack\.com/services/[A-Za-z0-9/]+",
            Severity::Error,
        ),
        ("anthropic-key", r"\bsk-ant-[A-Za-z0-9\-_]{20,}\b", Severity::Error),
        ("openai-key", r"\bsk-[A-Za-z0-9]{20,}\b", Severity::Error),
        ("google-api-key", r"\bAIza[0-9A-Za-z\-_]{35}\b", Severity::Error),
        ("private-key-block", r"-----BEGIN [A-Z ]*PRIVATE KEY-----", Severity::Error),
        (
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{5,}\.eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]*",
            Severity::Error,
        ),
        (
            "generic-credential",
            r#"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*["']?[A-Za-z0-9_\-]{16,}"#,
            Severity::Warning,
        ),
    ]
}

fn compiled_builtin_rules() -> &'static [(&'static str, Regex, Severity)] {
    static RULES: OnceLock<Vec<(&'static str, Regex, Severity)>> = OnceLock::new();
    RULES.get_or_init(|| {
        builtin_rules()
            .iter()
            .map(|(id, pat, sev)| (*id, Regex::new(pat).expect("built-in rule regex"), *sev))
            .collect()
    })
}

/// scan one file's text. `path` (used for `allow_paths` matching and finding `path` field) need
/// not exist on disk; both work on a text buffer.
pub fn scan_text(cfg: &Config, path: &str, text: &str) -> Vec<Finding> {
    if cfg.kb.secrets.allow_paths.iter().any(|pat| glob_match(pat, path)) {
        return vec![];
    }

    let mut findings = Vec::new();
    let custom: Vec<(String, Regex, Severity)> = cfg
        .kb
        .secrets
        .rules
        .iter()
        .filter_map(|r| Regex::new(&r.regex).ok().map(|re| (r.id.clone(), re, Severity::Error)))
        .collect();

    for (line_no, line) in text.lines().enumerate() {
        let line_no = line_no + 1;
        for (id, re, sev) in compiled_builtin_rules() {
            for m in re.find_iter(line) {
                let matched = m.as_str();
                // `sk-ant-...` already matches the more specific anthropic rule; do not double-report.
                if *id == "openai-key" && matched.starts_with("sk-ant-") {
                    continue;
                }
                findings.push(finding(*sev, id, matched, path, line_no));
            }
        }
        for (id, re, sev) in &custom {
            for m in re.find_iter(line) {
                findings.push(finding(*sev, id, m.as_str(), path, line_no));
            }
        }
    }
    findings
}

fn finding(severity: Severity, rule_id: &str, matched: &str, path: &str, line: usize) -> Finding {
    let prefix: String = matched.chars().take(4).collect();
    Finding {
        severity,
        code: "secret".into(),
        message: format!("possible secret ({rule_id}): {prefix}***"),
        path: Some(path.to_string()),
        line: Some(line),
        doc_id: None,
    }
}

/// scan the staged diff (`git diff --cached -U0`) for secrets added on added lines only.
pub fn scan_staged(cfg: &Config, repo: &Path) -> Result<Vec<Finding>> {
    // `--no-color` matters even off a terminal: a user's `color.ui = always` would otherwise
    // wrap every line in escape codes and break the `+`/`+++`/`@@` prefix parsing below.
    let diff = git::run(repo, &["diff", "--no-color", "--cached", "-U0"])?;
    Ok(scan_diff(cfg, &diff))
}

/// scan every added line between two revisions (`git diff -U0 base..head`).
pub fn scan_range(cfg: &Config, repo: &Path, base: &str, head: &str) -> Result<Vec<Finding>> {
    let diff = git::run(repo, &["diff", "--no-color", "-U0", &format!("{base}..{head}")])?;
    Ok(scan_diff(cfg, &diff))
}

fn scan_diff(cfg: &Config, diff_text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (path, line_no, content) in added_lines(diff_text) {
        for f in scan_text(cfg, &path, &content) {
            findings.push(Finding { line: Some(line_no), ..f });
        }
    }
    findings
}

/// parse a unified diff (as produced with `-U0`) into `(path, new-file line number, content)`
/// for every added line.
fn added_lines(diff_text: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let mut path = String::new();
    let mut new_line = 0usize;
    for line in diff_text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            path = rest.strip_prefix("b/").unwrap_or(rest).to_string();
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(plus) = rest.split_whitespace().find(|s| s.starts_with('+')) {
                let start = plus.trim_start_matches('+').split(',').next().unwrap_or("1");
                new_line = start.parse().unwrap_or(1);
            }
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            out.push((path.clone(), new_line, added.to_string()));
            new_line += 1;
        } else if line.starts_with('-') {
            // removed line: does not exist in the new file, so the new-line counter holds.
        } else if line.starts_with(' ') {
            new_line += 1;
        }
    }
    out
}

/// a glob supporting `*` (any run excluding `/`) and `**` (any run including `/`).
fn glob_match(pattern: &str, path: &str) -> bool {
    match glob_regex(pattern) {
        Ok(re) => re.is_match(path),
        Err(_) => false,
    }
}

fn glob_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let mut out = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    out.push_str(".*");
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        out.push('/');
                    }
                } else {
                    out.push_str("[^/]*");
                }
            }
            '\\' | '.' | '+' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    Regex::new(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let dir = tempfile::tempdir().unwrap();
        Config::load(dir.path()).unwrap()
    }

    #[test]
    fn detects_github_pat_and_redacts() {
        let c = cfg();
        let text = format!("x = \"ghp_{}\"\n", "a".repeat(36));
        let findings = scan_text(&c, "docs/x.md", &text);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("ghp_***"));
        assert!(!findings[0].message.contains(&"a".repeat(36)));
        assert_eq!(findings[0].line, Some(1));
        assert_eq!(findings[0].severity, Severity::Error);
    }

    #[test]
    fn anthropic_key_not_double_reported_as_openai() {
        let c = cfg();
        let text = format!("sk-ant-{}\n", "b".repeat(30));
        let findings = scan_text(&c, "docs/x.md", &text);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("anthropic-key"));
    }

    #[test]
    fn generic_pattern_is_warning() {
        let c = cfg();
        let text = "password: correcthorsebatterystaple\n";
        let findings = scan_text(&c, "docs/x.md", text);
        assert!(findings.iter().any(|f| f.code == "secret" && f.severity == Severity::Warning));
    }

    #[test]
    fn allow_paths_skips_scanning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kb.toml"), "[secrets]\nallow_paths = [\"fixtures/**\"]\n").unwrap();
        let c = Config::load(dir.path()).unwrap();
        let text = format!("ghp_{}\n", "a".repeat(36));
        assert!(scan_text(&c, "fixtures/anything/x.md", &text).is_empty());
        assert!(!scan_text(&c, "docs/x.md", &text).is_empty());
    }

    #[test]
    fn added_lines_parses_unified_diff() {
        let diff = "diff --git a/docs/x.md b/docs/x.md\nindex 111..222 100644\n--- a/docs/x.md\n+++ b/docs/x.md\n@@ -1,0 +2,1 @@\n+new line here\n";
        let lines = added_lines(diff);
        assert_eq!(lines, vec![("docs/x.md".to_string(), 2, "new line here".to_string())]);
    }
}
