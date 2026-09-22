//! the operation applier: turn one `ProposedOp` into a file write plus a diff, inside a
//! session's worktree. design.md §11 (operations), §25 (capture to canon).
//!
//! `apply` takes one extra argument beyond the three named in the plan (`owner: &str`): design.md
//! §11 says `create` sets `owner` "from the principal", but the accountable principal is only
//! known to the caller (`curate::run`, which holds the `Actor`), not to this function or to
//! `ProposedOp`. threading it through as a parameter is simpler than inventing a field on
//! `ProposedOp` that every other operation would leave unused.

use crate::check::banner::banner;
use crate::config::Config;
use crate::model::{Frontmatter, Layer, Operation, Ref, Status, new_ulid};
use crate::parse::frontmatter::sha256_hex;
use crate::parse::{parse_document, render_document};
use crate::review::udiff::render_unified;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// frontmatter fields `revise(field=...)` may touch. every other field is policy-owned (`id`,
/// `layer`, `status`, `agent_policy`, `owner`, `access`, `structure`, `superseded_by`, `parent`)
/// and changes only through `create`, `supersede`, or `retire`.
const REVISABLE_FIELDS: [&str; 4] = ["title", "tags", "review_by", "refs"];

/// one operation a curate (or classify) run proposes. field use varies by `op`; see design.md
/// §11 and prompts/curate.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedOp {
    pub op: Operation,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub layer: Option<Layer>,
    #[serde(default)]
    pub refs: Vec<Ref>,
    #[serde(default)]
    pub new_id: Option<String>,
    #[serde(default)]
    pub justification: String,
    #[serde(default)]
    pub confidence: f64,
}

/// `Operation` has no `Default` of its own (adding one would mean editing model.rs's existing
/// derive list rather than appending to it), so `ProposedOp` cannot derive `Default`; this
/// manual impl picks `escalate` as the inert default, matching this module's own "write
/// nothing" handling of that operation.
impl Default for ProposedOp {
    fn default() -> Self {
        ProposedOp {
            op: Operation::Escalate,
            target_id: None,
            section: None,
            field: None,
            content: None,
            title: None,
            layer: None,
            refs: Vec::new(),
            new_id: None,
            justification: String::new(),
            confidence: 0.0,
        }
    }
}

/// the result of one `apply`: the touched document's id, its path in the worktree, before/after
/// content hashes, and a unified diff of the change.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub doc_id: String,
    pub path: PathBuf,
    pub before_hash: Option<String>,
    pub after_hash: String,
    pub diff: String,
    /// `supersede` both creates a new document and marks the target superseded; this carries the
    /// new document's own `Applied` record. every other operation leaves this `None`.
    pub secondary: Option<Box<Applied>>,
}

/// find `docs/<id>.md`, or its sharded form `docs/<id[..3]>/<id>.md` (the same two forms
/// `session::sparse_patterns` checks out for an agent session).
pub fn locate_doc(worktree: &Path, id: &str) -> Result<PathBuf> {
    let flat = worktree.join("docs").join(format!("{id}.md"));
    if flat.is_file() {
        return Ok(flat);
    }
    if id.len() >= 3 {
        let sharded = worktree.join("docs").join(&id[..3]).join(format!("{id}.md"));
        if sharded.is_file() {
            return Ok(sharded);
        }
    }
    bail!("document `{id}` was not found in the worktree (checked docs/{id}.md and its sharded path)")
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') { s.to_string() } else { format!("{s}\n") }
}

/// `##`-style leading marker level of the first line, if it is a heading.
fn leading_heading_level(s: &str) -> Option<u8> {
    let first = s.lines().next()?;
    let hashes = first.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && first.as_bytes().get(hashes).is_some_and(|b| b.is_ascii_whitespace()) {
        Some(hashes as u8)
    } else {
        None
    }
}

/// `append`'s content must start with a heading; wrap it under one named `title` (or
/// `"addendum"`) when it does not.
fn ensure_heading(content: &str, title: Option<&str>) -> String {
    if leading_heading_level(content.trim_start()).is_some() {
        content.to_string()
    } else {
        let heading = title.unwrap_or("addendum");
        format!("## {heading}\n\n{}", content.trim_start())
    }
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

fn read_and_parse(path: &Path) -> Result<(String, crate::model::Document)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc = parse_document(&text)?;
    Ok((text, doc))
}

fn write_and_diff(path: &Path, before_text: Option<&str>, after_text: &str, rel_label: &str) -> Result<(Option<String>, String, String)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, after_text).with_context(|| format!("writing {}", path.display()))?;
    let before_hash = before_text.map(|t| sha256_hex(t.as_bytes()));
    let after_hash = sha256_hex(after_text.as_bytes());
    let old_label = if before_text.is_some() { rel_label } else { "/dev/null" };
    let diff = render_unified(old_label, rel_label, before_text.unwrap_or(""), after_text);
    Ok((before_hash, after_hash, diff))
}

/// build a fresh `create`d document's frontmatter and body, and write it. shared by the plain
/// `create` operation and by `supersede`'s first half.
fn write_new_document(cfg: &Config, worktree: &Path, op: &ProposedOp, owner: &str) -> Result<Applied> {
    let title = op.title.clone().ok_or_else(|| anyhow!("create requires a title"))?;
    if !op.refs.iter().any(|r| r.rel == "source") {
        bail!("create requires a source ref (design.md §11 provenance)");
    }
    let layer = op.layer.unwrap_or(cfg.kb.defaults.layer);
    let id = new_ulid();
    let fm = Frontmatter {
        id: Some(id.clone()),
        layer: Some(layer),
        title: Some(title),
        status: Some(Status::Draft),
        agent_policy: Some(cfg.agent_policy_for(layer)),
        owner: Some(owner.to_string()),
        tags: vec![],
        refs: op.refs.clone(),
        ..Default::default()
    };
    let body = ensure_trailing_newline(op.content.as_deref().unwrap_or(""));
    let text = render_document(&fm, &body)?;
    let rel = format!("docs/{id}.md");
    let path = worktree.join(&rel);
    let (before_hash, after_hash, diff) = write_and_diff(&path, None, &text, &rel)?;
    Ok(Applied { doc_id: id, path, before_hash, after_hash, diff, secondary: None })
}

/// see design.md §11 and this module's doc comment for why `owner` is a separate parameter.
pub fn apply(cfg: &Config, worktree: &Path, op: &ProposedOp, owner: &str) -> Result<Applied> {
    match op.op {
        Operation::Create => write_new_document(cfg, worktree, op, owner),
        Operation::Append => apply_append(worktree, op),
        Operation::Revise => apply_revise(op, worktree),
        Operation::Rewrite => apply_rewrite(worktree, op),
        Operation::Supersede => apply_supersede(cfg, worktree, op, owner),
        Operation::Link => apply_link(worktree, op),
        Operation::Retire => apply_retire(worktree, op),
        Operation::Split | Operation::Merge | Operation::Delete => {
            bail!("{}: review-class operation; open a review item instead", op.op)
        }
        Operation::Escalate => Ok(Applied {
            doc_id: op.target_id.clone().unwrap_or_else(|| "none".to_string()),
            path: PathBuf::new(),
            before_hash: None,
            after_hash: "escalate".to_string(),
            diff: String::new(),
            secondary: None,
        }),
    }
}

fn apply_append(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().ok_or_else(|| anyhow!("append requires target_id"))?;
    let path = locate_doc(worktree, &target_id)?;
    let (before_text, doc) = read_and_parse(&path)?;
    let addition = ensure_heading(op.content.as_deref().unwrap_or(""), op.title.as_deref());
    let mut body = doc.body.clone();
    if !body.is_empty() && !body.ends_with("\n\n") {
        body = ensure_trailing_newline(&body);
        body.push('\n');
    }
    body.push_str(&ensure_trailing_newline(&addition));
    let after_text = render_document(&doc.frontmatter, &body)?;
    let rel = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

fn apply_revise(op: &ProposedOp, worktree: &Path) -> Result<Applied> {
    let target_id = op.target_id.clone().ok_or_else(|| anyhow!("revise requires target_id"))?;
    let path = locate_doc(worktree, &target_id)?;
    let (before_text, mut doc) = read_and_parse(&path)?;

    if let Some(section_path) = &op.section {
        let section = doc
            .sections
            .iter()
            .find(|s| &s.path == section_path)
            .cloned()
            .ok_or_else(|| anyhow!("revise: section `{section_path}` does not exist in {target_id}"))?;
        let content = op.content.clone().unwrap_or_default();
        let new_section_text = if section.level == 0 {
            ensure_trailing_newline(content.trim_start())
        } else {
            match leading_heading_level(content.trim_start()) {
                Some(lv) if lv == section.level => ensure_trailing_newline(&content),
                Some(lv) => bail!("revise: replacement heading level {lv} does not match section `{section_path}`'s level {}", section.level),
                None => {
                    let heading_line = format!("{} {}\n", "#".repeat(section.level as usize), section.heading);
                    format!("{heading_line}\n{}", ensure_trailing_newline(content.trim_start()))
                }
            }
        };
        let mut body = doc.body.clone();
        body.replace_range(section.start..section.end, &new_section_text);
        let after_text = render_document(&doc.frontmatter, &body)?;
        let rel = format!("docs/{target_id}.md");
        let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;
        return Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None });
    }

    let field = op.field.as_deref().ok_or_else(|| anyhow!("revise requires either section or field"))?;
    if !REVISABLE_FIELDS.contains(&field) {
        bail!("revise cannot set frontmatter field `{field}`: only {} may be revised", REVISABLE_FIELDS.join(", "));
    }
    apply_field_revision(&mut doc.frontmatter, field, op)?;
    let after_text = render_document(&doc.frontmatter, &doc.body)?;
    let rel = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

fn apply_field_revision(fm: &mut Frontmatter, field: &str, op: &ProposedOp) -> Result<()> {
    match field {
        "title" => {
            let title = op.title.clone().or_else(|| op.content.clone()).ok_or_else(|| anyhow!("revise(field=title) needs a title or content"))?;
            fm.title = Some(title);
        }
        "tags" => {
            let raw = op.content.clone().unwrap_or_default();
            fm.tags = raw.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect();
        }
        "review_by" => {
            let raw = op.content.clone().ok_or_else(|| anyhow!("revise(field=review_by) needs content"))?;
            fm.review_by = Some(raw.trim().parse::<toml::value::Datetime>().map_err(|e| anyhow!("invalid review_by date `{raw}`: {e}"))?);
        }
        "refs" => {
            fm.refs = op.refs.clone();
        }
        other => bail!("revise cannot set frontmatter field `{other}`"),
    }
    Ok(())
}

fn apply_rewrite(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().ok_or_else(|| anyhow!("rewrite requires target_id"))?;
    let path = locate_doc(worktree, &target_id)?;
    let (before_text, doc) = read_and_parse(&path)?;
    let body = ensure_trailing_newline(op.content.as_deref().unwrap_or(""));
    let after_text = render_document(&doc.frontmatter, &body)?;
    let rel = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

fn apply_link(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().ok_or_else(|| anyhow!("link requires target_id"))?;
    let path = locate_doc(worktree, &target_id)?;
    let (before_text, mut doc) = read_and_parse(&path)?;
    if op.field.as_deref() == Some("remove") {
        doc.frontmatter.refs.retain(|existing| !op.refs.iter().any(|r| r.rel == existing.rel && r.uri == existing.uri));
    } else {
        for r in &op.refs {
            if !doc.frontmatter.refs.iter().any(|existing| existing.rel == r.rel && existing.uri == r.uri) {
                doc.frontmatter.refs.push(r.clone());
            }
        }
    }
    let after_text = render_document(&doc.frontmatter, &doc.body)?;
    let rel = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

/// best-effort title lookup for a `kb:<id>` uri, read straight from the worktree; `None` when
/// the target is not (or no longer) in this session's sparse checkout.
fn lookup_title(worktree: &Path, id: &str) -> Option<String> {
    let path = locate_doc(worktree, id).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    parse_document(&text).ok()?.frontmatter.title
}

fn apply_supersede(cfg: &Config, worktree: &Path, op: &ProposedOp, owner: &str) -> Result<Applied> {
    let target_id = op.target_id.clone().ok_or_else(|| anyhow!("supersede requires target_id"))?;
    let new_doc = write_new_document(cfg, worktree, op, owner)?;

    let path = locate_doc(worktree, &target_id)?;
    let (before_text, mut doc) = read_and_parse(&path)?;
    doc.frontmatter.status = Some(Status::Superseded);
    let successor_uri = format!("kb:{}", new_doc.doc_id);
    doc.frontmatter.superseded_by = Some(successor_uri.clone());
    let target_title = lookup_title(worktree, &new_doc.doc_id).unwrap_or_else(|| successor_uri.clone());
    let banner_text = banner(Status::Superseded, &today(), Some((&target_title, &successor_uri)));
    let body = format!("{banner_text}\n{}", doc.body.trim_start_matches('\n'));
    let after_text = render_document(&doc.frontmatter, &body)?;
    let rel = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;

    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: Some(Box::new(new_doc)) })
}

fn apply_retire(worktree: &Path, op: &ProposedOp) -> Result<Applied> {
    let target_id = op.target_id.clone().ok_or_else(|| anyhow!("retire requires target_id"))?;
    let path = locate_doc(worktree, &target_id)?;
    let (before_text, mut doc) = read_and_parse(&path)?;

    let successor_uri = op
        .new_id
        .clone()
        .map(|n| if n.starts_with("kb:") { n } else { format!("kb:{n}") })
        .or_else(|| doc.frontmatter.parent.clone())
        .unwrap_or_else(|| "none".to_string());
    doc.frontmatter.status = Some(Status::Archived);
    doc.frontmatter.superseded_by = Some(successor_uri.clone());

    let target = if successor_uri == "none" {
        None
    } else {
        let id = crate::model::parse_kb_uri(&successor_uri).map(|(id, _)| id);
        let title = id.as_deref().and_then(|id| lookup_title(worktree, id)).unwrap_or_else(|| successor_uri.clone());
        Some((title, successor_uri.clone()))
    };
    let banner_text = banner(Status::Archived, &today(), target.as_ref().map(|(t, u)| (t.as_str(), u.as_str())));
    let body = format!("{banner_text}\n{}", doc.body.trim_start_matches('\n'));
    let after_text = render_document(&doc.frontmatter, &body)?;
    let rel = format!("docs/{target_id}.md");
    let (before_hash, after_hash, diff) = write_and_diff(&path, Some(&before_text), &after_text, &rel)?;
    Ok(Applied { doc_id: target_id, path, before_hash, after_hash, diff, secondary: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Status as DocStatus;
    use std::fs;

    fn cfg_at(dir: &Path) -> Config {
        Config::load(dir).unwrap()
    }

    fn write_doc(worktree: &Path, id: &str, extra_fm: &str, body: &str) {
        fs::create_dir_all(worktree.join("docs")).unwrap();
        let text = format!("+++\nid = \"{id}\"\n{extra_fm}\n+++\n\n{body}");
        fs::write(worktree.join("docs").join(format!("{id}.md")), text).unwrap();
    }

    const ID: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";

    #[test]
    fn create_writes_frontmatter_and_source_ref() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        let op = ProposedOp {
            op: Operation::Create,
            title: Some("new doc".into()),
            layer: Some(Layer::Reference),
            content: Some("# new doc\n\nbody text.\n".into()),
            refs: vec![Ref { rel: "source".into(), uri: "capture:01ABC".into(), sha256: None, media_type: None }],
            justification: "captured a new fact".into(),
            confidence: 0.9,
            ..Default::default()
        };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        assert!(applied.path.is_file());
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert_eq!(doc.frontmatter.status, Some(DocStatus::Draft));
        assert_eq!(doc.frontmatter.owner.as_deref(), Some("caleb"));
        assert!(doc.frontmatter.refs.iter().any(|r| r.rel == "source"));
        assert!(applied.before_hash.is_none());
    }

    #[test]
    fn create_without_source_ref_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        let op = ProposedOp { op: Operation::Create, title: Some("t".into()), content: Some("x".into()), ..Default::default() };
        assert!(apply(&cfg, dir.path(), &op, "caleb").is_err());
    }

    #[test]
    fn append_wraps_prose_in_a_heading() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# t\n\nintro.\n",
        );
        let op = ProposedOp { op: Operation::Append, target_id: Some(ID.into()), content: Some("just some prose, no heading.".into()), ..Default::default() };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert!(doc.body.contains("## addendum"), "{}", doc.body);
        assert!(doc.body.contains("just some prose"));
        assert!(applied.before_hash.is_some());
    }

    #[test]
    fn append_keeps_an_existing_heading_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# t\n\nintro.\n",
        );
        let op = ProposedOp { op: Operation::Append, target_id: Some(ID.into()), content: Some("## notes\n\nsome notes.".into()), ..Default::default() };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert!(doc.body.contains("## notes"));
        assert!(!doc.body.contains("## addendum"));
    }

    #[test]
    fn revise_section_keeps_the_heading_when_content_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"procedure\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "## steps\n\nold steps.\n",
        );
        let op = ProposedOp {
            op: Operation::Revise,
            target_id: Some(ID.into()),
            section: Some("steps".into()),
            content: Some("new steps.".into()),
            ..Default::default()
        };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert!(doc.body.contains("## steps"));
        assert!(doc.body.contains("new steps."));
        assert!(!doc.body.contains("old steps."));
    }

    #[test]
    fn revise_field_refuses_owner() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# t\n\nbody.\n",
        );
        let op =
            ProposedOp { op: Operation::Revise, target_id: Some(ID.into()), field: Some("owner".into()), content: Some("someone-else".into()), ..Default::default() };
        let err = apply(&cfg, dir.path(), &op, "caleb").unwrap_err();
        assert!(err.to_string().contains("owner"), "{err}");
    }

    #[test]
    fn revise_field_title_updates_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"old title\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# old title\n\nbody.\n",
        );
        let op = ProposedOp { op: Operation::Revise, target_id: Some(ID.into()), field: Some("title".into()), title: Some("new title".into()), ..Default::default() };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert_eq!(doc.frontmatter.title.as_deref(), Some("new title"));
    }

    #[test]
    fn supersede_sets_status_pointer_and_banner() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"old doc\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# old doc\n\nstale content.\n",
        );
        let op = ProposedOp {
            op: Operation::Supersede,
            target_id: Some(ID.into()),
            title: Some("new doc".into()),
            layer: Some(Layer::Reference),
            content: Some("# new doc\n\nfresh content.\n".into()),
            refs: vec![Ref { rel: "source".into(), uri: "capture:01XYZ".into(), sha256: None, media_type: None }],
            ..Default::default()
        };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, old_doc) = read_and_parse(&applied.path).unwrap();
        assert_eq!(old_doc.frontmatter.status, Some(DocStatus::Superseded));
        let secondary = applied.secondary.expect("supersede creates a new document");
        assert_eq!(old_doc.frontmatter.superseded_by.as_deref(), Some(format!("kb:{}", secondary.doc_id).as_str()));
        assert!(crate::check::banner::has_banner(&old_doc.body), "{}", old_doc.body);
        assert!(secondary.path.is_file());
    }

    #[test]
    fn retire_falls_back_to_parent_then_none() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"orphan\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# orphan\n\ncontent.\n",
        );
        let op = ProposedOp { op: Operation::Retire, target_id: Some(ID.into()), ..Default::default() };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert_eq!(doc.frontmatter.status, Some(DocStatus::Archived));
        assert_eq!(doc.frontmatter.superseded_by.as_deref(), Some("none"));
        assert!(crate::check::banner::has_banner(&doc.body));

        let parent_id = "01K5A7Q2XW3ZRCM8T6YB4VJH0B";
        write_doc(
            dir.path(),
            parent_id,
            &format!(
                "layer = \"reference\"\ntitle = \"child\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nparent = \"kb:{ID}\""
            ),
            "# child\n\ncontent.\n",
        );
        let op2 = ProposedOp { op: Operation::Retire, target_id: Some(parent_id.into()), ..Default::default() };
        let applied2 = apply(&cfg, dir.path(), &op2, "caleb").unwrap();
        let (_, doc2) = read_and_parse(&applied2.path).unwrap();
        assert_eq!(doc2.frontmatter.superseded_by.as_deref(), Some(format!("kb:{ID}").as_str()));
    }

    #[test]
    fn retire_prefers_explicit_new_id_over_parent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        let successor = "01K5A7Q2XW3ZRCM8T6YB4VJH0C";
        write_doc(
            dir.path(),
            ID,
            &format!(
                "layer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\nparent = \"kb:{successor}zzz\""
            ),
            "# t\n\ncontent.\n",
        );
        let op = ProposedOp { op: Operation::Retire, target_id: Some(ID.into()), new_id: Some(successor.into()), ..Default::default() };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert_eq!(doc.frontmatter.superseded_by.as_deref(), Some(format!("kb:{successor}").as_str()));
    }

    #[test]
    fn split_merge_delete_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        for op_kind in [Operation::Split, Operation::Merge, Operation::Delete] {
            let op = ProposedOp { op: op_kind, target_id: Some(ID.into()), ..Default::default() };
            let err = apply(&cfg, dir.path(), &op, "caleb").unwrap_err();
            assert!(err.to_string().contains("review item"), "{err}");
        }
    }

    #[test]
    fn escalate_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        let op = ProposedOp { op: Operation::Escalate, target_id: Some(ID.into()), justification: "unsure".into(), ..Default::default() };
        let applied = apply(&cfg, dir.path(), &op, "caleb").unwrap();
        assert_eq!(applied.diff, "");
        assert_eq!(applied.after_hash, "escalate");
    }

    #[test]
    fn link_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_at(dir.path());
        write_doc(
            dir.path(),
            ID,
            "layer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# t\n\nbody.\n",
        );
        let add = ProposedOp {
            op: Operation::Link,
            target_id: Some(ID.into()),
            refs: vec![Ref { rel: "related".into(), uri: "kb:01OTHER".into(), sha256: None, media_type: None }],
            ..Default::default()
        };
        let applied = apply(&cfg, dir.path(), &add, "caleb").unwrap();
        let (_, doc) = read_and_parse(&applied.path).unwrap();
        assert!(doc.frontmatter.refs.iter().any(|r| r.uri == "kb:01OTHER"));

        let remove = ProposedOp {
            op: Operation::Link,
            target_id: Some(ID.into()),
            field: Some("remove".into()),
            refs: vec![Ref { rel: "related".into(), uri: "kb:01OTHER".into(), sha256: None, media_type: None }],
            ..Default::default()
        };
        let applied2 = apply(&cfg, dir.path(), &remove, "caleb").unwrap();
        let (_, doc2) = read_and_parse(&applied2.path).unwrap();
        assert!(!doc2.frontmatter.refs.iter().any(|r| r.uri == "kb:01OTHER"));
    }
}
