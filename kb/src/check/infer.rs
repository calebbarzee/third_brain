//! `kb infer`: classify a human diff into the operation vocabulary, with no model.
//! contract: design.md §11 table, followed exactly.

use crate::config::Config;
use crate::git;
use crate::model::{Document, Frontmatter, Operation, Status};
use crate::parse::parse_document;
use anyhow::Result;
use std::path::Path;

/// one inferred operation. `section` is a slug path (for `append`/`revise(section)`),
/// `field` is a frontmatter key (for `revise(field)`/`link`); an operation with neither
/// (`create`, `delete`, `supersede`, `retire`, `rewrite`) leaves both `None`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InferredOp {
    pub op: Operation,
    pub detail: String,
    pub section: Option<String>,
    pub field: Option<String>,
}

/// classify the diff between `base` and `head` (the same document at two revisions, or `None`
/// on either side for creation/deletion) into operations, per design.md §11.
pub fn infer(base: Option<&Document>, head: Option<&Document>) -> Vec<InferredOp> {
    match (base, head) {
        (None, None) => vec![],
        (None, Some(_)) => vec![InferredOp {
            op: Operation::Create,
            detail: "new document".into(),
            section: None,
            field: None,
        }],
        (Some(_), None) => vec![InferredOp {
            op: Operation::Delete,
            detail: "document removed".into(),
            section: None,
            field: None,
        }],
        (Some(b), Some(h)) => infer_diff(b, h),
    }
}

fn infer_diff(base: &Document, head: &Document) -> Vec<InferredOp> {
    let refs_changed = base.frontmatter.refs != head.frontmatter.refs;
    let status_changed = base.frontmatter.status != head.frontmatter.status;
    let mut field_diffs = frontmatter_field_diffs(&base.frontmatter, &head.frontmatter);

    let mut parts: Vec<InferredOp> = Vec::new();

    if status_changed {
        match head.frontmatter.status {
            Some(Status::Superseded) if head.frontmatter.superseded_by.is_some() => {
                field_diffs.retain(|f| f != "superseded_by");
                parts.push(InferredOp {
                    op: Operation::Supersede,
                    detail: format!(
                        "status changed to superseded, superseded_by = {}",
                        head.frontmatter.superseded_by.as_deref().unwrap_or("")
                    ),
                    section: None,
                    field: None,
                });
            }
            Some(Status::Archived) => {
                field_diffs.retain(|f| f != "superseded_by");
                parts.push(InferredOp {
                    op: Operation::Retire,
                    detail: "status changed to archived".into(),
                    section: None,
                    field: None,
                });
            }
            _ => field_diffs.push("status".to_string()),
        }
    }

    for f in field_diffs {
        parts.push(InferredOp {
            op: Operation::Revise,
            detail: format!("field `{f}` changed"),
            section: None,
            field: Some(f),
        });
    }

    if refs_changed {
        parts.push(InferredOp {
            op: Operation::Link,
            detail: "refs changed".into(),
            section: None,
            field: Some("refs".into()),
        });
    }

    match section_diff(base, head) {
        BodyChange::None => {}
        BodyChange::Append(paths) => parts.push(InferredOp {
            op: Operation::Append,
            detail: format!("appended section(s): {}", paths.join(", ")),
            section: paths.into_iter().next(),
            field: None,
        }),
        BodyChange::ReviseSection(path) => parts.push(InferredOp {
            op: Operation::Revise,
            detail: format!("section `{path}` changed"),
            section: Some(path),
            field: None,
        }),
        BodyChange::Rewrite => parts.push(InferredOp {
            op: Operation::Rewrite,
            detail: "more than one section changed, or the heading set changed".into(),
            section: None,
            field: None,
        }),
    }

    parts
}

/// names of frontmatter fields (schema fields and `extra` keys) that differ between `base` and
/// `head`. excludes `status` and `refs`, which the caller folds into `supersede`/`retire`/`link`.
fn frontmatter_field_diffs(base: &Frontmatter, head: &Frontmatter) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! check {
        ($name:literal, $field:ident) => {
            if base.$field != head.$field {
                out.push($name.to_string());
            }
        };
    }
    check!("id", id);
    check!("layer", layer);
    check!("title", title);
    check!("agent_policy", agent_policy);
    check!("owner", owner);
    check!("tags", tags);
    check!("parent", parent);
    check!("superseded_by", superseded_by);
    check!("review_by", review_by);
    check!("access", access);
    check!("structure", structure);

    let mut extra_keys: Vec<&String> = base.extra.keys().chain(head.extra.keys()).collect();
    extra_keys.sort();
    extra_keys.dedup();
    for key in extra_keys {
        if base.extra.get(key) != head.extra.get(key) {
            out.push(key.clone());
        }
    }
    out
}

enum BodyChange {
    None,
    Append(Vec<String>),
    ReviseSection(String),
    Rewrite,
}

/// compare sections by slug path and text, per design.md §11.
fn section_diff(base: &Document, head: &Document) -> BodyChange {
    if base.body == head.body {
        return BodyChange::None;
    }

    let base_paths: Vec<&str> = base.sections.iter().map(|s| crate::parse::body::diff_key(&s.path)).collect();
    let head_paths: Vec<&str> = head.sections.iter().map(|s| crate::parse::body::diff_key(&s.path)).collect();

    // new sections appended after every existing one, with the existing ones untouched. trailing
    // whitespace is trimmed before comparing: inserting a heading after a section adds a
    // blank-line separator to the end of that section's byte range even though its content did
    // not change, and that must not read as a change to it.
    if head_paths.len() > base_paths.len() && head_paths[..base_paths.len()] == base_paths[..] {
        let prefix_unchanged = base
            .sections
            .iter()
            .zip(head.sections.iter())
            .all(|(b, h)| base.body[b.start..b.end].trim_end() == head.body[h.start..h.end].trim_end());
        if prefix_unchanged {
            let new_paths = head.sections[base_paths.len()..].iter().map(|s| s.path.clone()).collect();
            return BodyChange::Append(new_paths);
        }
    }

    // same heading set and order: either one section changed (revise) or several did (rewrite).
    if base_paths == head_paths {
        let changed: Vec<&str> = base
            .sections
            .iter()
            .zip(head.sections.iter())
            .filter(|(b, h)| base.body[b.start..b.end].trim_end() != head.body[h.start..h.end].trim_end())
            .map(|(_, h)| h.path.as_str())
            .collect();
        return match changed.len() {
            1 => BodyChange::ReviseSection(changed[0].to_string()),
            _ => BodyChange::Rewrite,
        };
    }

    // heading set changed in a way that is not a pure append: removed, reordered, or inserted
    // in the middle.
    BodyChange::Rewrite
}

/// infer operations per changed `docs/*.md` file between two revisions.
pub fn infer_range(_cfg: &Config, repo: &Path, base_rev: &str, head_rev: &str) -> Result<Vec<(String, Vec<InferredOp>)>> {
    let rows = git::diff_name_status(repo, base_rev, head_rev)?;
    let mut out = Vec::new();
    for (status, old_path, new_path) in rows {
        let path = if new_path.is_empty() { old_path.clone() } else { new_path.clone() };
        if !path.starts_with("docs/") || !path.ends_with(".md") {
            continue;
        }
        let base_doc = if status == 'A' { None } else { read_doc(repo, base_rev, &old_path) };
        let head_doc = if status == 'D' { None } else { read_doc(repo, head_rev, &path) };
        let ops = infer(base_doc.as_ref(), head_doc.as_ref());
        out.push((path, ops));
    }
    Ok(out)
}

fn read_doc(repo: &Path, rev: &str, path: &str) -> Option<Document> {
    let bytes = git::show_file(repo, rev, path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    parse_document(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_document;

    fn doc(text: &str) -> Document {
        parse_document(text).unwrap()
    }

    const HEAD_FM: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n";

    /// a note with no headings gains its first `##` section: the old text moves from `_body` to
    /// `_preamble` unchanged, so this is an append, not a rewrite.
    #[test]
    fn first_heading_added_to_a_headingless_note_is_an_append() {
        let b = doc(&format!("{HEAD_FM}launcher ip 192.168.50.10\n"));
        let h = doc(&format!("{HEAD_FM}launcher ip 192.168.50.10\n\n## laptop side\n\n192.168.50.20/24\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert_eq!(ops[0].op, Operation::Append, "{ops:?}");
    }

    #[test]
    fn create_and_delete() {
        let h = doc(&format!("{HEAD_FM}# t\n\nbody\n"));
        assert_eq!(infer(None, Some(&h))[0].op, Operation::Create);
        assert_eq!(infer(Some(&h), None)[0].op, Operation::Delete);
        assert!(infer(None, None).is_empty());
    }

    #[test]
    fn revise_field() {
        let b = doc(&format!("{HEAD_FM}# t\n\nbody\n"));
        let h_text = HEAD_FM.replace("title = \"t\"", "title = \"new title\"");
        let h = doc(&format!("{h_text}# t\n\nbody\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Revise);
        assert_eq!(ops[0].field.as_deref(), Some("title"));
    }

    #[test]
    fn supersede_and_retire() {
        let b = doc(&format!("{HEAD_FM}# t\n\nbody\n"));
        // body is unchanged from base: this isolates the frontmatter-only transition. a real
        // supersede also rewrites the banner into the body, which infer sees as a second, mixed
        // part (a body change) exactly as design.md §11's "any mix" row says it should.
        let sup_fm = HEAD_FM.replace(
            "status = \"draft\"",
            "status = \"superseded\"\nsuperseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"",
        );
        let h = doc(&format!("{sup_fm}# t\n\nbody\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Supersede);

        let arc_fm = HEAD_FM.replace("status = \"draft\"", "status = \"archived\"");
        let h2 = doc(&format!("{arc_fm}# t\n\nbody\n"));
        let ops2 = infer(Some(&b), Some(&h2));
        assert_eq!(ops2.len(), 1);
        assert_eq!(ops2[0].op, Operation::Retire);
    }

    #[test]
    fn link_only() {
        let b = doc(&format!("{HEAD_FM}# t\n\nbody\n"));
        let h_fm = HEAD_FM.replacen(
            "+++\n\n",
            "\n[[refs]]\nrel = \"related\"\nuri = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n+++\n\n",
            1,
        );
        let h = doc(&format!("{h_fm}# t\n\nbody\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Link);
    }

    #[test]
    fn append_only() {
        let b = doc(&format!("{HEAD_FM}# one\n\ntext\n"));
        let h = doc(&format!("{HEAD_FM}# one\n\ntext\n\n# two\n\nmore\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Append);
        assert_eq!(ops[0].section.as_deref(), Some("two"));
    }

    #[test]
    fn revise_section_only() {
        let b = doc(&format!("{HEAD_FM}# one\n\ntext\n\n# two\n\nmore\n"));
        let h = doc(&format!("{HEAD_FM}# one\n\ntext\n\n# two\n\nchanged\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Revise);
        assert_eq!(ops[0].section.as_deref(), Some("two"));
    }

    #[test]
    fn rewrite_on_multiple_sections_or_heading_change() {
        let b = doc(&format!("{HEAD_FM}# one\n\ntext\n\n# two\n\nmore\n"));
        let h = doc(&format!("{HEAD_FM}# one\n\nchanged\n\n# two\n\nalso changed\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Operation::Rewrite);

        let h2 = doc(&format!("{HEAD_FM}# one renamed\n\ntext\n\n# two\n\nmore\n"));
        let ops2 = infer(Some(&b), Some(&h2));
        assert_eq!(ops2.len(), 1);
        assert_eq!(ops2[0].op, Operation::Rewrite);
    }

    #[test]
    fn mixed_is_listed_per_part() {
        let b = doc(&format!("{HEAD_FM}# one\n\ntext\n"));
        let h_fm = HEAD_FM.replace("title = \"t\"", "title = \"new\"");
        let h = doc(&format!("{h_fm}# one\n\ntext\n\n# two\n\nmore\n"));
        let ops = infer(Some(&b), Some(&h));
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().any(|o| o.op == Operation::Revise && o.field.as_deref() == Some("title")));
        assert!(ops.iter().any(|o| o.op == Operation::Append));
    }
}
