//! `kb validate [--fix]`: frontmatter and body checks per document, deterministic fills.
//! contract in design.md §6, §8, §11 ("classify" hook and the model live elsewhere; this module
//! never calls a provider).

use super::banner::{banner, has_banner};
use crate::config::Config;
use crate::git;
use crate::model::{Document, Finding, Frontmatter, Severity, Status, is_ulid, new_ulid, parse_kb_uri};
use crate::parse::{parse_document, render_document, split_frontmatter};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// scheme allow-list for `[[refs]]` uris (design.md §8).
const REF_SCHEMES: [&str; 10] = ["kb", "capture", "person", "git", "pr", "jira", "slack", "https", "http", "blob"];

/// outcome of validating one document.
#[derive(serde::Serialize)]
pub struct ValidateOutcome {
    pub findings: Vec<Finding>,
    /// true when `--fix` wrote the file (a content edit, or a rename, or both).
    pub changed: bool,
    /// the document as it stands after validation: the fixed version when `changed`,
    /// otherwise exactly what was on disk.
    pub document: Document,
    /// where the document actually lives now: the input path, or the renamed path when a
    /// missing `id` fill also renamed the file to `<ulid>.md`.
    pub path: PathBuf,
}

/// which of `layer`/`tags` are still at their deterministic default or empty, i.e. candidates
/// for the `classify` hook (owned by another module) to fill with a model suggestion. call this
/// on the frontmatter as parsed, before `validate_file`'s own fills are applied.
pub fn missing_classifiable(fm: &Frontmatter) -> Vec<&'static str> {
    let mut out = Vec::new();
    if fm.layer.is_none() {
        out.push("layer");
    }
    if fm.tags.is_empty() {
        out.push("tags");
    }
    out
}

/// validate one file, optionally fixing it deterministically. never touches anything outside
/// `path`'s own file.
pub fn validate_file(cfg: &Config, path: &Path, fix: bool) -> Result<ValidateOutcome> {
    let path_str = path.display().to_string();
    let original = std::fs::read_to_string(path)?;
    let mut findings = Vec::new();
    let mut changed = false;

    let is_legacy_yaml = !original.starts_with("+++\n") && original != "+++" && original.starts_with("---\n");
    if is_legacy_yaml {
        findings.push(Finding {
            severity: Severity::Info,
            code: "legacy-frontmatter".into(),
            message: "leading `---` yaml block found; migrate to toml frontmatter with --fix".into(),
            path: Some(path_str.clone()),
            line: Some(1),
            doc_id: None,
        });
    }

    // legacy yaml migration happens first, before any other fill, so a non-ulid legacy `id`
    // lands in `extra.source_id` and the real `id` field is then filled by the ulid step below.
    let mut text = original.clone();
    if fix
        && is_legacy_yaml
        && let Some((fm, body)) = migrate_legacy_yaml(&original)
    {
        text = render_document(&fm, &body)?;
        changed = true;
    }

    let (mut fm, mut body) = safe_parse(&text, &mut findings, &path_str);

    let original_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let mut final_path = path.to_path_buf();
    let mut rename_from: Option<PathBuf> = None;

    // deterministic fills, in the fixed order from design.md §6.
    if fm.id.is_none() && fix {
        let new_id = new_ulid();
        if !is_ulid(&original_stem) {
            rename_from = Some(path.to_path_buf());
            final_path = path.with_file_name(format!("{new_id}.md"));
        }
        fm.id = Some(new_id);
        changed = true;
    }

    if fm.title.is_none() && fix {
        let body_parse = crate::parse::body::parse_body(&body);
        let heading = body_parse
            .sections
            .iter()
            .find(|s| !s.heading.is_empty())
            .map(|s| s.heading.clone());
        fm.title = Some(heading.unwrap_or_else(|| original_stem.clone()));
        changed = true;
    }

    if fm.status.is_none() && fix {
        fm.status = Some(Status::Draft);
        changed = true;
    }

    if fm.layer.is_none() && fix {
        fm.layer = Some(cfg.kb.defaults.layer);
        changed = true;
    }

    if fm.agent_policy.is_none() && fix {
        let layer = fm.layer.unwrap_or(cfg.kb.defaults.layer);
        fm.agent_policy = Some(cfg.agent_policy_for(layer));
        changed = true;
    }

    if fm.owner.is_none() && fix {
        let (_, email) = git::identity(&cfg.root);
        let owner = match &email {
            Some(e) => cfg.principal_for_email(e).unwrap_or_else(|| e.clone()),
            None => "unknown".to_string(),
        };
        fm.owner = Some(owner);
        changed = true;
    }

    if fix && matches!(fm.status, Some(Status::Superseded) | Some(Status::Archived)) && fm.superseded_by.is_some() && !has_banner(&body) {
        let date = today();
        let sb = fm.superseded_by.as_deref().unwrap();
        let target = if sb == "none" {
            None
        } else {
            parse_kb_uri(sb).map(|(id, _)| {
                let uri = format!("kb:{id}");
                let title = lookup_title(&cfg.root, &id).unwrap_or_else(|| uri.clone());
                (title, uri)
            })
        };
        let banner_text = banner(fm.status.unwrap(), &date, target.as_ref().map(|(t, u)| (t.as_str(), u.as_str())));
        body = format!("{banner_text}\n{}", body.trim_start_matches('\n'));
        changed = true;
    }

    // --- checks, against the (possibly now fixed) state ---
    for field in fm.missing_required() {
        findings.push(Finding {
            severity: Severity::Error,
            code: "missing-field".into(),
            message: format!("required field `{field}` is missing"),
            path: Some(path_str.clone()),
            line: None,
            doc_id: fm.id.clone(),
        });
    }

    let stem_for_check = final_path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    if let Some(id) = &fm.id {
        if !is_ulid(id) {
            findings.push(Finding {
                severity: Severity::Error,
                code: "id-not-ulid".into(),
                message: format!("id `{id}` is not a ulid"),
                path: Some(path_str.clone()),
                line: None,
                doc_id: Some(id.clone()),
            });
        } else if id != &stem_for_check {
            findings.push(Finding {
                severity: Severity::Error,
                code: "id-mismatch".into(),
                message: format!("id `{id}` does not match filename stem `{stem_for_check}`"),
                path: Some(path_str.clone()),
                line: None,
                doc_id: Some(id.clone()),
            });
        }
    }

    if let (Some(layer), Some(agent_policy)) = (fm.layer, fm.agent_policy) {
        let expected = cfg.agent_policy_for(layer);
        if agent_policy != expected {
            findings.push(Finding {
                severity: Severity::Warning,
                code: "agent-policy-mismatch".into(),
                message: format!("agent_policy `{agent_policy}` differs from the `{layer}` default `{expected}`"),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            });
        }
    }

    if matches!(fm.status, Some(Status::Superseded) | Some(Status::Archived)) {
        match &fm.superseded_by {
            None => findings.push(Finding {
                severity: Severity::Error,
                code: "missing-superseded-by".into(),
                message: "superseded or archived document has no `superseded_by`".into(),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            }),
            Some(sb) if sb != "none" && parse_kb_uri(sb).is_none() => findings.push(Finding {
                severity: Severity::Error,
                code: "invalid-superseded-by".into(),
                message: format!("`superseded_by` value `{sb}` is neither `none` nor a `kb:` uri"),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            }),
            _ => {}
        }
        if !has_banner(&body) {
            findings.push(Finding {
                severity: Severity::Error,
                code: "missing-banner".into(),
                message: "superseded or archived document is missing the forward-pointer banner as its first line".into(),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            });
        }
    }

    if let Some(parent) = &fm.parent
        && parse_kb_uri(parent).is_none()
    {
        findings.push(Finding {
            severity: Severity::Error,
            code: "invalid-parent".into(),
            message: format!("`parent` value `{parent}` is not a `kb:` uri"),
            path: Some(path_str.clone()),
            line: None,
            doc_id: fm.id.clone(),
        });
    }

    for r in &fm.refs {
        if !crate::model::REL_VALUES.contains(&r.rel.as_str()) {
            findings.push(Finding {
                severity: Severity::Warning,
                code: "unknown-rel".into(),
                message: format!("ref rel `{}` is not one of the known values", r.rel),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            });
        }
        let scheme = r.uri.split_once(':').map(|(s, _)| s).unwrap_or(&r.uri);
        if !REF_SCHEMES.contains(&scheme) {
            findings.push(Finding {
                severity: Severity::Warning,
                code: "unknown-scheme".into(),
                message: format!("ref uri `{}` has an unrecognized scheme `{scheme}`", r.uri),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            });
        }
    }

    let final_body_parse = crate::parse::body::parse_body(&body);
    if let Some(layer) = fm.layer {
        let budget = cfg.budget_for(layer);
        if final_body_parse.word_count > budget {
            findings.push(Finding {
                severity: Severity::Warning,
                code: "over-budget".into(),
                message: format!("{} words exceeds the {layer} budget of {budget}", final_body_parse.word_count),
                path: Some(path_str.clone()),
                line: None,
                doc_id: fm.id.clone(),
            });
        }
    }

    for key in fm.extra.keys() {
        findings.push(Finding {
            severity: Severity::Info,
            code: "unknown-field".into(),
            message: format!("top-level key `{key}` is not part of the document schema"),
            path: Some(path_str.clone()),
            line: None,
            doc_id: fm.id.clone(),
        });
    }

    let document = if changed {
        let final_text = render_document(&fm, &body)?;
        std::fs::write(&final_path, &final_text)?;
        if let Some(old) = &rename_from
            && old != &final_path
        {
            std::fs::remove_file(old)?;
            findings.push(Finding {
                severity: Severity::Info,
                code: "renamed".into(),
                message: format!("renamed {} to {}", old.display(), final_path.display()),
                path: Some(final_path.display().to_string()),
                line: None,
                doc_id: fm.id.clone(),
            });
        }
        parse_document(&final_text)?
    } else {
        // do NOT re-parse `original` here: when frontmatter fails to parse, `safe_parse` above
        // already turned that into an `invalid-frontmatter`/`unclosed-frontmatter` Finding and
        // fell back to a default `Frontmatter` plus the readable body text. re-running
        // `parse_document(&original)?` would parse the very same text a second time, outside
        // that safety net, and propagate the same error unhandled. build the Document from what
        // `safe_parse` already produced instead.
        let body_parse = crate::parse::body::parse_body(&body);
        Document {
            frontmatter: fm.clone(),
            body: body.clone(),
            sections: body_parse.sections,
            links: body_parse.links,
            content_hash: crate::parse::frontmatter::sha256_hex(original.as_bytes()),
        }
    };

    Ok(ValidateOutcome {
        findings,
        changed,
        document,
        path: final_path,
    })
}

/// validate every `docs/**/*.md` file, plus corpus-wide checks that need every document at once:
/// duplicate ids, and a ulid whose encoded timestamp is in the future.
pub fn validate_repo(cfg: &Config, fix: bool) -> Result<RepoValidateOutcome> {
    let docs_dir = cfg.root.join("docs");
    let mut files = Vec::new();
    if docs_dir.is_dir() {
        let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&docs_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "md"))
            .map(|e| e.path().to_path_buf())
            .collect();
        paths.sort();
        for path in paths {
            let outcome = validate_file(cfg, &path, fix)?;
            files.push((path, outcome));
        }
    }

    let mut findings = Vec::new();
    let mut by_id: std::collections::HashMap<String, Vec<PathBuf>> = std::collections::HashMap::new();
    let now_ms = crate::clock::now().timestamp_millis().max(0) as u64;
    for (path, outcome) in &files {
        let Some(id) = &outcome.document.frontmatter.id else { continue };
        by_id.entry(id.clone()).or_default().push(path.clone());
        if is_ulid(id)
            && let Ok(u) = ulid::Ulid::from_string(id)
            && u.timestamp_ms() > now_ms
        {
            findings.push(Finding {
                severity: Severity::Error,
                code: "future-timestamp".into(),
                message: format!("id `{id}` encodes a timestamp in the future"),
                path: Some(path.display().to_string()),
                line: None,
                doc_id: Some(id.clone()),
            });
        }
    }
    for (id, paths) in by_id {
        if paths.len() > 1 {
            let list = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
            findings.push(Finding {
                severity: Severity::Error,
                code: "duplicate-id".into(),
                message: format!("id `{id}` is used by more than one file: {list}"),
                path: None,
                line: None,
                doc_id: Some(id),
            });
        }
    }

    Ok(RepoValidateOutcome { files, findings })
}

#[derive(serde::Serialize)]
pub struct RepoValidateOutcome {
    pub files: Vec<(PathBuf, ValidateOutcome)>,
    /// corpus-wide findings not tied to a single file's own checks.
    pub findings: Vec<Finding>,
}

/// parse frontmatter and body without ever hard-failing: a parse error becomes an error
/// `Finding` and an empty frontmatter, so the rest of validation can still run.
fn safe_parse(text: &str, findings: &mut Vec<Finding>, path_str: &str) -> (Frontmatter, String) {
    match split_frontmatter(text) {
        Ok((Some(fm_text), body)) => match crate::parse::frontmatter::parse_frontmatter(&fm_text) {
            Ok(fm) => (fm, body),
            Err(e) => {
                findings.push(Finding {
                    severity: Severity::Error,
                    code: "invalid-frontmatter".into(),
                    message: format!("frontmatter does not parse: {e:#}"),
                    path: Some(path_str.to_string()),
                    line: None,
                    doc_id: None,
                });
                (Frontmatter::default(), body)
            }
        },
        Ok((None, body)) => (Frontmatter::default(), body),
        Err(e) => {
            findings.push(Finding {
                severity: Severity::Error,
                code: "unclosed-frontmatter".into(),
                message: format!("{e:#}"),
                path: Some(path_str.to_string()),
                line: None,
                doc_id: None,
            });
            (Frontmatter::default(), text.to_string())
        }
    }
}

fn today() -> String {
    crate::clock::now().format("%Y-%m-%d").to_string()
}

/// the target document's title, when it exists under `root`'s `docs/` tree (flat or sharded into
/// subdirectories) and has one: used as the banner's link text instead of the bare `kb:<id>` uri.
fn lookup_title(root: &Path, target_id: &str) -> Option<String> {
    let docs_dir = root.join("docs");
    let target_name = format!("{target_id}.md");
    walkdir::WalkDir::new(&docs_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().is_file() && e.file_name().to_str() == Some(target_name.as_str()))
        .and_then(|e| std::fs::read_to_string(e.path()).ok())
        .and_then(|text| parse_document(&text).ok())
        .and_then(|doc| doc.frontmatter.title)
}

/// migration only, with no other fill and no rename: for a caller that needs valid toml
/// frontmatter before doing its own thing with a document (`provider classify --write`, design.md
/// §6), so it never renders a fresh `+++` block above an untouched legacy `---` header. `None`
/// when `text` already has toml frontmatter, or has no frontmatter at all.
pub fn migrate_legacy_frontmatter(text: &str) -> Option<(Frontmatter, String)> {
    let is_legacy_yaml = !text.starts_with("+++\n") && text != "+++" && text.starts_with("---\n");
    if !is_legacy_yaml {
        return None;
    }
    migrate_legacy_yaml(text)
}

/// migrate a leading obsidian-style `---` yaml block (only `key: value` and `key: [a, b]`
/// lines) into a `Frontmatter` plus the body with the block removed. `None` when the text does
/// not open with such a block.
fn migrate_legacy_yaml(text: &str) -> Option<(Frontmatter, String)> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return None;
    }
    let mut consumed = first.len();
    let mut yaml_lines = Vec::new();
    let mut closed = false;
    for line in lines {
        consumed += line.len();
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            closed = true;
            break;
        }
        yaml_lines.push(trimmed.to_string());
    }
    if !closed {
        return None;
    }

    let mut fm = Frontmatter::default();
    for line in yaml_lines {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            continue;
        }
        if let Some(list) = parse_yaml_list(value) {
            match key {
                "tags" => fm.tags = list,
                "aliases" => {
                    fm.extra.insert(
                        "aliases".into(),
                        toml::Value::Array(list.into_iter().map(toml::Value::String).collect()),
                    );
                }
                other => {
                    fm.extra.insert(
                        other.to_string(),
                        toml::Value::Array(list.into_iter().map(toml::Value::String).collect()),
                    );
                }
            }
        } else {
            let scalar = unquote(value);
            match key {
                "id" if is_ulid(&scalar) => fm.id = Some(scalar),
                "id" => {
                    fm.extra.insert("source_id".into(), toml::Value::String(scalar));
                }
                "tags" if scalar.is_empty() => {}
                "tags" => fm.tags = vec![scalar],
                other => {
                    fm.extra.insert(other.to_string(), toml::Value::String(scalar));
                }
            }
        }
    }
    let body = text[consumed..].trim_start_matches('\n').to_string();
    Some((fm, body))
}

fn parse_yaml_list(value: &str) -> Option<Vec<String>> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(vec![]);
    }
    Some(inner.split(',').map(|s| unquote(s.trim())).collect())
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Layer;

    fn cfg(dir: &Path) -> Config {
        Config::load(dir).unwrap()
    }

    #[test]
    fn fills_missing_fields_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# hello world\n\nsome text\n").unwrap();
        let outcome = validate_file(&c, &path, true).unwrap();
        assert!(outcome.changed);
        assert!(outcome.findings.iter().any(|f| f.code == "renamed"));
        let fm = &outcome.document.frontmatter;
        assert!(is_ulid(fm.id.as_deref().unwrap()));
        assert_eq!(fm.title.as_deref(), Some("hello world"));
        assert_eq!(fm.status, Some(Status::Draft));
        assert_eq!(fm.layer, Some(Layer::Reference));
        assert_eq!(fm.agent_policy, Some(c.agent_policy_for(Layer::Reference)));
        assert!(fm.owner.is_some());
        let new_path = dir.path().join(format!("{}.md", fm.id.as_deref().unwrap()));
        assert!(new_path.is_file());
        assert!(!path.is_file());
    }

    #[test]
    fn no_fix_reports_missing_fields_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let path = dir.path().join("01K5A7Q2XW3ZRCM8T6YB4VJH0N.md");
        std::fs::write(&path, "# t\n\nbody\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let outcome = validate_file(&c, &path, false).unwrap();
        assert!(!outcome.changed);
        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.code == "missing-field" && f.severity == Severity::Error)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn migrates_legacy_yaml_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let path = dir.path().join("static_ip.md");
        std::fs::write(&path, "---\nid: static_ip\naliases: []\ntags: [net, ip]\n---\nsome text\n").unwrap();
        let outcome = validate_file(&c, &path, true).unwrap();
        assert!(outcome.findings.iter().any(|f| f.code == "legacy-frontmatter"));
        let fm = &outcome.document.frontmatter;
        assert_eq!(fm.extra.get("source_id").and_then(|v| v.as_str()), Some("static_ip"));
        assert_eq!(fm.tags, vec!["net".to_string(), "ip".to_string()]);
        assert!(is_ulid(fm.id.as_deref().unwrap()));
        assert!(outcome.document.body.contains("some text"));
        assert!(!outcome.document.body.contains("aliases:"));
    }

    #[test]
    fn superseded_without_banner_is_error_and_fix_adds_one() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let path = dir.path().join("01K5A7Q2XW3ZRCM8T6YB4VJH0N.md");
        let text = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nsuperseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n+++\n\nbody text\n";
        std::fs::write(&path, text).unwrap();
        let checked = validate_file(&c, &path, false).unwrap();
        assert!(checked.findings.iter().any(|f| f.code == "missing-banner"));
        let fixed = validate_file(&c, &path, true).unwrap();
        assert!(fixed.changed);
        assert!(has_banner(&fixed.document.body));
    }

    #[test]
    fn banner_uses_target_title_when_the_target_document_exists() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(
            dir.path().join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH0P.md"),
            "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\nlayer = \"reference\"\ntitle = \"the replacement doc\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\nbody\n",
        )
        .unwrap();
        let path = dir.path().join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH0N.md");
        let text = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nsuperseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n+++\n\nbody text\n";
        std::fs::write(&path, text).unwrap();
        let fixed = validate_file(&c, &path, true).unwrap();
        assert!(
            fixed.document.body.contains("[the replacement doc](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P)"),
            "banner should link the target's title, not its bare id: {}",
            fixed.document.body
        );
    }

    #[test]
    fn banner_falls_back_to_the_uri_when_the_target_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let path = dir.path().join("01K5A7Q2XW3ZRCM8T6YB4VJH0N.md");
        let text = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nsuperseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n+++\n\nbody text\n";
        std::fs::write(&path, text).unwrap();
        let fixed = validate_file(&c, &path, true).unwrap();
        assert!(fixed.document.body.contains("[kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P)"));
    }

    #[test]
    fn agent_policy_mismatch_is_warning() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let path = dir.path().join("01K5A7Q2XW3ZRCM8T6YB4VJH0N.md");
        let text = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"principle\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\nbody\n";
        std::fs::write(&path, text).unwrap();
        let outcome = validate_file(&c, &path, false).unwrap();
        let f = outcome.findings.iter().find(|f| f.code == "agent-policy-mismatch").unwrap();
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn missing_classifiable_reports_defaults() {
        let fm = Frontmatter::default();
        assert_eq!(missing_classifiable(&fm), vec!["layer", "tags"]);
    }
}
