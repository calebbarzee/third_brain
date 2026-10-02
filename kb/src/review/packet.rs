//! build the content-review context packet (design.md §15.2): for every changed `docs/*.md`
//! file between `base` and `head`, the three diffs, inferred operations, one-hop graph
//! neighbors, and derived metrics, all bounded to keep the prompt within budget.

use crate::check::infer::{self, InferredOp};
use crate::config::Config;
use crate::git;
use crate::index;
use crate::model::{Document, Frontmatter};
use crate::parse::parse_document;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// facts about the session that produced the change, for the `completeness` finding kind
/// (design.md §12 "end of day", §15.2).
#[derive(Debug, Clone, Serialize)]
pub struct SessionFacts {
    pub age_hours: f64,
    pub wip_count: u32,
    pub settled_by_eod: bool,
    pub uncommitted_at_settle: u32,
}

/// everything the caller supplies to build and run a content review.
#[derive(Debug, Clone)]
pub struct ReviewInput {
    pub session_id: Option<String>,
    pub base: String,
    pub head: String,
    /// current `main` sha, when it differs from `base` (a rebase moved main since the session
    /// opened). `None` skips the second and third diffs.
    pub main: Option<String>,
    /// the settled commit message.
    pub justification: String,
    pub actor: String,
    /// captures or conversation excerpts that prompted the edit; untrusted, may be empty.
    pub context_excerpts: Vec<String>,
    pub session_facts: Option<SessionFacts>,
}

/// derived metrics for one document, from the index when available; every field is `None`
/// ("unknown" when rendered) when no index artifact exists.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DerivedMetrics {
    pub criticality_pct: Option<f64>,
    pub in_canon: Option<i64>,
    pub in_code: Option<i64>,
    pub churn_30d: Option<i64>,
    pub last_human_commit: Option<String>,
}

/// deterministic, code-computed structural facts about one document's change: never a model's
/// guess. these are what a reviewer (human or model) checks the `justification` against first --
/// a justification that never mentions a removed section is a `scope` finding (design.md §15.2).
#[derive(Debug, Clone, Default, Serialize)]
pub struct DocStructure {
    /// slug paths (design.md §7) present in `head` but not `base`.
    pub sections_added: Vec<String>,
    /// slug paths present in `base` but not `head`.
    pub sections_removed: Vec<String>,
    /// slug paths present on both sides with different text.
    pub sections_changed: Vec<String>,
    /// best-effort rename guesses: a removed and an added heading at the same position (index)
    /// among the sections that only exist on one side. `(old heading text, new heading text)`.
    pub headings_renamed: Vec<(String, String)>,
    /// frontmatter field names that differ between `base` and `head`.
    pub frontmatter_changed: Vec<String>,
    pub words_before: usize,
    pub words_after: usize,
}

/// everything gathered about one changed document.
#[derive(Debug, Clone, Serialize)]
pub struct DocChange {
    /// the document's ulid, from whichever side of the diff has it (head, else base).
    pub doc_id: Option<String>,
    pub path: String,
    pub diff_base_head: String,
    pub diff_base_main: Option<String>,
    pub diff_head_main_trial: Option<String>,
    /// set instead of `diff_head_main_trial` when the third diff was skipped (design.md §15.2's
    /// documented fallback: git < 2.38, or no `main` given).
    pub diff_head_main_note: Option<String>,
    pub inferred_ops: Vec<InferredOp>,
    pub neighbors: Vec<String>,
    pub metrics: DerivedMetrics,
    pub structure: DocStructure,
}

/// the bounded, renderable context packet.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewPacket {
    pub session_id: Option<String>,
    pub base: String,
    pub head: String,
    pub main: Option<String>,
    pub justification: String,
    pub actor: String,
    pub context_excerpts: Vec<String>,
    pub session_facts: Option<SessionFacts>,
    pub documents: Vec<DocChange>,
    /// changed paths outside docs/ (prompts, config, files moved out of docs/): status letter and a
    /// short diff, so the reviewer sees the whole change and not only the documents.
    #[serde(default)]
    pub other_changes: Vec<OtherChange>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OtherChange {
    pub path: String,
    pub status: String,
    pub diff: String,
}

const DIFF_MAX_CHARS: usize = 12_000;
const TRUNCATE_MARKER: &str = "\n... [diff truncated]\n";

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}{TRUNCATE_MARKER}")
}

/// gather the review packet for `input`: parse every changed `docs/*.md` file between
/// `input.base` and `input.head`, compute the three diffs and inferred operations, and pull
/// one-hop neighbors and derived metrics from the newest index artifact when one exists. an
/// unreadable or missing index is not an error (`index::open_newest` errors are swallowed):
/// neighbors and metrics simply come back empty/unknown, per this crate's contract for callers
/// of the index module.
pub fn gather(cfg: &Config, input: &ReviewInput) -> Result<ReviewPacket> {
    let repo = &cfg.root;
    let rows = git::diff_name_status(repo, &input.base, &input.head)?;
    let conn = index::open_newest(cfg).ok();

    let mut documents = Vec::new();
    let mut other_changes = Vec::new();
    for (status, old_path, new_path) in rows {
        let path = if new_path.is_empty() { old_path.clone() } else { new_path.clone() };
        if !path.starts_with("docs/") || !path.ends_with(".md") {
            let diff = truncate_chars(&git::diff_range_path(repo, &input.base, &input.head, &path).unwrap_or_default(), 2_000);
            other_changes.push(OtherChange { path: path.clone(), status: status.to_string(), diff });
            continue;
        }

        let base_doc = if status == 'A' { None } else { read_doc(repo, &input.base, &old_path) };
        let head_doc = if status == 'D' { None } else { read_doc(repo, &input.head, &path) };
        let doc_id = head_doc
            .as_ref()
            .and_then(|d| d.frontmatter.id.clone())
            .or_else(|| base_doc.as_ref().and_then(|d| d.frontmatter.id.clone()))
            .or_else(|| Path::new(&path).file_stem().and_then(|s| s.to_str()).map(str::to_string));

        let diff_base_head = truncate_chars(&git::diff_range_path(repo, &input.base, &input.head, &path).unwrap_or_default(), DIFF_MAX_CHARS);

        let (diff_base_main, diff_head_main_trial, diff_head_main_note) = match &input.main {
            None => (None, None, None),
            Some(main) => {
                let base_main = truncate_chars(&git::diff_range_path(repo, &input.base, main, &path).unwrap_or_default(), DIFF_MAX_CHARS);
                let (trial, note) = third_diff(repo, &input.head, main, &path);
                (Some(base_main), trial.map(|d| truncate_chars(&d, DIFF_MAX_CHARS)), note)
            }
        };

        let inferred_ops = infer::infer(base_doc.as_ref(), head_doc.as_ref());
        let structure = compute_structure(base_doc.as_ref(), head_doc.as_ref());

        let (neighbors, metrics) = match (&conn, &doc_id) {
            (Some(conn), Some(id)) => (neighbor_titles(conn, id).unwrap_or_default(), derived_metrics(conn, id).unwrap_or_default()),
            _ => (Vec::new(), DerivedMetrics::default()),
        };

        documents.push(DocChange { doc_id, path, diff_base_head, diff_base_main, diff_head_main_trial, diff_head_main_note, inferred_ops, neighbors, metrics, structure });
    }

    let excerpt_max_chars = (cfg.manifest.packet.capture_excerpt_tokens as usize) * 4;
    let context_excerpts = input.context_excerpts.iter().map(|e| truncate_chars(e, excerpt_max_chars)).collect();

    let packet = ReviewPacket {
        session_id: input.session_id.clone(),
        base: input.base.clone(),
        head: input.head.clone(),
        main: input.main.clone(),
        justification: input.justification.clone(),
        actor: input.actor.clone(),
        context_excerpts,
        session_facts: input.session_facts.clone(),
        documents,
        other_changes,
    };

    let whole_max_chars = (cfg.manifest.packet.review_context_tokens as usize) * 4;
    Ok(bound_whole_packet(packet, whole_max_chars))
}

/// the head-vs-main-after-trial-merge diff (design.md §15.2's third diff): `git merge-tree
/// --write-tree` the two commits and diff the changed document's text in the resulting tree
/// against its text at `head`, in-process (a trial-merge tree is not a commit `git diff` can
/// address). returns `(None, Some(note))` on the documented fallback: git predates 2.38.
fn third_diff(repo: &Path, head: &str, main: &str, path: &str) -> (Option<String>, Option<String>) {
    if !git::version_at_least(2, 38) {
        return (None, Some("third diff omitted: git merge-tree --write-tree requires git >= 2.38".to_string()));
    }
    let Ok(Some(tree)) = git::merge_tree_write_tree(repo, head, main) else {
        return (None, Some("third diff omitted: git merge-tree --write-tree produced no tree".to_string()));
    };
    let head_bytes = git::show_file(repo, head, path).unwrap_or_default();
    let head_text = String::from_utf8_lossy(&head_bytes).into_owned();
    let merged_text = match git::show_tree_path(repo, &tree, path) {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        _ => String::new(),
    };
    let diff = super::udiff::render_unified(&format!("{path} (head)"), &format!("{path} (after trial merge with main)"), &head_text, &merged_text);
    (Some(diff), None)
}

/// compute `DocStructure` straight from `parse::parse_body` and the frontmatter on each side --
/// deterministic, never a model call, per this module's contract.
fn compute_structure(base: Option<&Document>, head: Option<&Document>) -> DocStructure {
    let base_body = base.map(|d| d.body.as_str()).unwrap_or("");
    let head_body = head.map(|d| d.body.as_str()).unwrap_or("");
    let base_parse = crate::parse::parse_body(base_body);
    let head_parse = crate::parse::parse_body(head_body);

    let key = crate::parse::body::diff_key;
    let own = crate::parse::body::own_text;
    let base_map: HashMap<&str, &str> = base_parse.sections.iter().enumerate().map(|(i, s)| (key(&s.path), own(base_body, &base_parse.sections, i))).collect();
    let head_map: HashMap<&str, &str> = head_parse.sections.iter().enumerate().map(|(i, s)| (key(&s.path), own(head_body, &head_parse.sections, i))).collect();

    let mut sections_added = Vec::new();
    let mut sections_changed = Vec::new();
    for (path, text) in &head_map {
        match base_map.get(path) {
            None => sections_added.push(path.to_string()),
            Some(btext) if btext != text => sections_changed.push(path.to_string()),
            _ => {}
        }
    }
    let mut sections_removed: Vec<String> = base_map.keys().filter(|p| !head_map.contains_key(*p)).map(|p| p.to_string()).collect();
    sections_added.sort();
    sections_changed.sort();
    sections_removed.sort();

    // best-effort rename guess: a removed and an added heading at the same position (index),
    // counting only the sections that exist on one side. not a certainty -- just a hint.
    let removed_headings: Vec<&str> = base_parse.sections.iter().filter(|s| sections_removed.iter().any(|p| p == key(&s.path))).map(|s| s.heading.as_str()).collect();
    let added_headings: Vec<&str> = head_parse.sections.iter().filter(|s| sections_added.iter().any(|p| p == key(&s.path))).map(|s| s.heading.as_str()).collect();
    let headings_renamed = removed_headings
        .iter()
        .zip(added_headings.iter())
        .map(|(r, a)| (r.to_string(), a.to_string()))
        .collect();

    let default_fm = Frontmatter::default();
    let base_fm = base.map(|d| &d.frontmatter).unwrap_or(&default_fm);
    let head_fm = head.map(|d| &d.frontmatter).unwrap_or(&default_fm);
    let frontmatter_changed = frontmatter_diff_keys(base_fm, head_fm);

    DocStructure {
        sections_added,
        sections_removed,
        sections_changed,
        headings_renamed,
        frontmatter_changed,
        words_before: base_parse.word_count,
        words_after: head_parse.word_count,
    }
}

/// names of the `Frontmatter` fields that differ between `base` and `head`.
fn frontmatter_diff_keys(base: &Frontmatter, head: &Frontmatter) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! check {
        ($field:ident) => {
            if base.$field != head.$field {
                out.push(stringify!($field).to_string());
            }
        };
    }
    check!(id);
    check!(layer);
    check!(title);
    check!(status);
    check!(agent_policy);
    check!(owner);
    check!(tags);
    check!(parent);
    check!(superseded_by);
    check!(review_by);
    check!(access);
    check!(structure);
    check!(refs);
    if base.extra != head.extra {
        out.push("extra".to_string());
    }
    out
}

fn read_doc(repo: &Path, rev: &str, path: &str) -> Option<Document> {
    let bytes = git::show_file(repo, rev, path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    parse_document(&text).ok()
}

/// titles of documents one hop away in the reference graph, either direction.
fn neighbor_titles(conn: &Connection, id: &str) -> Result<Vec<String>> {
    let Some(rec) = index::get(conn, id)? else { return Ok(Vec::new()) };
    let mut ids: Vec<String> = Vec::new();
    ids.extend(rec.refs_out.iter().filter_map(|r| r.resolved_id.clone()));
    ids.extend(rec.refs_in.iter().map(|r| r.from_id.clone()));
    ids.sort();
    ids.dedup();
    let mut titles = Vec::new();
    for nid in ids {
        if let Ok(Some(n)) = index::get(conn, &nid) {
            titles.push(n.title.unwrap_or(nid));
        }
    }
    Ok(titles)
}

/// `criticality_pct`, `churn_30d`, and `last_human_commit` come from `index::get`; `in_canon`
/// and `in_code` live in the `graph_metrics` table, queried directly since `index::query`
/// does not expose them on `DocRecord` (design.md §17.2).
fn derived_metrics(conn: &Connection, id: &str) -> Result<DerivedMetrics> {
    let Some(rec) = index::get(conn, id)? else { return Ok(DerivedMetrics::default()) };
    let graph: Option<(i64, i64)> = conn
        .query_row("select in_canon, in_code from graph_metrics where doc_id = ?1", params![rec.id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(DerivedMetrics {
        criticality_pct: rec.criticality_pct,
        in_canon: graph.map(|(c, _)| c),
        in_code: graph.map(|(_, c)| c),
        churn_30d: Some(rec.churn_30d),
        last_human_commit: rec.last_human_commit,
    })
}

/// enforce the whole-packet bound by trimming diffs evenly, deepest documents first, until the
/// rendered packet fits (or every diff has been reduced to its truncation marker).
fn bound_whole_packet(mut packet: ReviewPacket, max_chars: usize) -> ReviewPacket {
    let mut rendered = render(&packet);
    if rendered.chars().count() <= max_chars {
        return packet;
    }
    // shrink the largest diff fields by half, repeatedly, until the packet fits or nothing
    // shrinks any further.
    loop {
        let mut shrank = false;
        for doc in &mut packet.documents {
            shrank |= shrink_field(&mut doc.diff_base_head);
            if let Some(f) = doc.diff_base_main.as_mut() {
                shrank |= shrink_field(f);
            }
            if let Some(f) = doc.diff_head_main_trial.as_mut() {
                shrank |= shrink_field(f);
            }
        }
        rendered = render(&packet);
        if rendered.chars().count() <= max_chars || !shrank {
            break;
        }
    }
    packet
}

/// halve `text` (down to its truncation marker) when it is still worth shrinking; returns
/// whether it changed, so the caller knows when further passes would no longer help.
fn shrink_field(text: &mut String) -> bool {
    if text.chars().count() > 200 {
        let half = text.chars().count() / 2;
        *text = truncate_chars(text, half);
        true
    } else {
        false
    }
}

/// toml-ish sections, one per document plus a header, for the review prompt's user message and
/// for `kb review packet`'s human-readable output.
pub fn render(packet: &ReviewPacket) -> String {
    let mut out = String::new();
    out.push_str("[review]\n");
    if let Some(sid) = &packet.session_id {
        out.push_str(&format!("session_id = \"{sid}\"\n"));
    }
    out.push_str(&format!("base = \"{}\"\n", packet.base));
    out.push_str(&format!("head = \"{}\"\n", packet.head));
    if let Some(main) = &packet.main {
        out.push_str(&format!("main = \"{main}\"\n"));
    }
    out.push_str(&format!("actor = \"{}\"\n", packet.actor));
    // the reviewer's own idea of today may differ from kb's; dates in the change are judged
    // against this one
    out.push_str(&format!("today = \"{}\"\n", crate::clock::now().format("%Y-%m-%d")));
    out.push_str(&format!("justification = \"\"\"\n{}\n\"\"\"\n", packet.justification));

    if let Some(facts) = &packet.session_facts {
        out.push_str("\n[session_facts]\n");
        out.push_str(&format!("age_hours = {}\n", facts.age_hours));
        out.push_str(&format!("wip_count = {}\n", facts.wip_count));
        out.push_str(&format!("settled_by_eod = {}\n", facts.settled_by_eod));
        out.push_str(&format!("uncommitted_at_settle = {}\n", facts.uncommitted_at_settle));
    }

    if !packet.context_excerpts.is_empty() {
        out.push_str("\n[context_excerpts]\n# untrusted: captured content or conversation, quoted verbatim below.\n");
        for (i, e) in packet.context_excerpts.iter().enumerate() {
            out.push_str(&format!("excerpt_{i} = \"\"\"\n{e}\n\"\"\"\n"));
        }
    }

    for doc in &packet.documents {
        out.push_str(&format!("\n[[document]]\nid = \"{}\"\npath = \"{}\"\n", doc.doc_id.as_deref().unwrap_or("unknown"), doc.path));
        out.push_str("inferred_ops = [");
        out.push_str(&doc.inferred_ops.iter().map(|o| format!("\"{}\"", o.op)).collect::<Vec<_>>().join(", "));
        out.push_str("]\n");
        out.push_str(&format!("neighbors = [{}]\n", doc.neighbors.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ")));
        out.push_str(&format!(
            "criticality_pct = {}\nin_canon = {}\nin_code = {}\nchurn_30d = {}\nlast_human_commit = \"{}\"\n",
            fmt_opt_f64(doc.metrics.criticality_pct),
            fmt_opt_i64(doc.metrics.in_canon),
            fmt_opt_i64(doc.metrics.in_code),
            fmt_opt_i64(doc.metrics.churn_30d),
            doc.metrics.last_human_commit.as_deref().unwrap_or("unknown"),
        ));

        out.push_str(&format!("\n[document.structure]\nsections_added = [{}]\n", quote_list(&doc.structure.sections_added)));
        out.push_str(&format!("sections_removed = [{}]\n", quote_list(&doc.structure.sections_removed)));
        out.push_str(&format!("sections_changed = [{}]\n", quote_list(&doc.structure.sections_changed)));
        out.push_str(&format!(
            "headings_renamed = [{}]\n",
            doc.structure.headings_renamed.iter().map(|(o, n)| format!("[\"{o}\", \"{n}\"]")).collect::<Vec<_>>().join(", ")
        ));
        out.push_str(&format!("frontmatter_changed = [{}]\n", quote_list(&doc.structure.frontmatter_changed)));
        out.push_str(&format!("words_before = {}\nwords_after = {}\n", doc.structure.words_before, doc.structure.words_after));

        out.push_str(&format!("\ndiff_base_head = \"\"\"\n{}\n\"\"\"\n", doc.diff_base_head));
        if let Some(d) = &doc.diff_base_main {
            out.push_str(&format!("diff_base_main = \"\"\"\n{d}\n\"\"\"\n"));
        }
        if let Some(d) = &doc.diff_head_main_trial {
            out.push_str(&format!("diff_head_main_after_trial_merge = \"\"\"\n{d}\n\"\"\"\n"));
        }
        if let Some(note) = &doc.diff_head_main_note {
            out.push_str(&format!("# {note}\n"));
        }
    }
    if !packet.other_changes.is_empty() {
        out.push_str("\n# changes outside docs/: config, prompts, or files moved out of the document tree\n");
        for c in &packet.other_changes {
            out.push_str(&format!("\n[[other_change]]\npath = \"{}\"\nstatus = \"{}\"\ndiff = \"\"\"\n{}\n\"\"\"\n", c.path, c.status, c.diff));
        }
    }
    out
}

fn quote_list(items: &[String]) -> String {
    items.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")
}

fn fmt_opt_f64(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "\"unknown\"".to_string())
}

fn fmt_opt_i64(v: Option<i64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "\"unknown\"".to_string())
}

impl ReviewPacket {
    pub fn render(&self) -> String {
        render(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git as gitmod;
    use crate::model::Layer;

    #[test]
    fn appending_the_first_heading_removes_no_section() {
        let fm = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n";
        let base = crate::parse::parse_document(&format!("{fm}launcher ip 192.168.50.10\n")).unwrap();
        let head = crate::parse::parse_document(&format!("{fm}launcher ip 192.168.50.10\n\n## laptop side\n\n192.168.50.20/24\n")).unwrap();
        let s = compute_structure(Some(&base), Some(&head));
        assert!(s.sections_removed.is_empty(), "{:?}", s.sections_removed);
        assert!(s.sections_changed.is_empty(), "{:?}", s.sections_changed);
        assert_eq!(s.sections_added, vec!["laptop-side".to_string()]);
    }

    #[test]
    fn a_subsection_edit_changes_only_that_subsection() {
        let fm = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n";
        let base = crate::parse::parse_document(&format!("{fm}# t\n\nintro\n\n## steps\n\nold step\n\n## notes\n\nkeep\n")).unwrap();
        let head = crate::parse::parse_document(&format!("{fm}# t\n\nintro\n\n## steps\n\nnew step\n\n## notes\n\nkeep\n")).unwrap();
        let s = compute_structure(Some(&base), Some(&head));
        assert_eq!(s.sections_changed, vec!["t/steps".to_string()]);
        assert!(s.sections_added.is_empty() && s.sections_removed.is_empty());
    }

    fn write_doc(id: &str, title: &str, body_extra: &str) -> String {
        format!(
            "+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"{title}\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# {title}\n\n{body_extra}\n"
        )
    }

    #[test]
    fn gather_produces_three_diffs_and_inferred_ops() {
        let dir = tempfile::tempdir().unwrap();
        gitmod::init_for_tests(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        let id = "01K5A7Q2XW3ZRCM8T6YB4VJH0N";
        std::fs::write(dir.path().join(format!("docs/{id}.md")), write_doc(id, "t", "one\n")).unwrap();
        let base = gitmod::commit_all(dir.path(), "base").unwrap();
        std::fs::write(dir.path().join(format!("docs/{id}.md")), write_doc(id, "t", "one\n\n# two\n\nmore\n")).unwrap();
        let head = gitmod::commit_all(dir.path(), "head: append two").unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        let input = ReviewInput {
            session_id: None,
            base: base.clone(),
            head: head.clone(),
            main: Some(base.clone()),
            justification: "append two".into(),
            actor: "human:caleb".into(),
            context_excerpts: vec![],
            session_facts: None,
        };
        let packet = gather(&cfg, &input).unwrap();
        assert_eq!(packet.documents.len(), 1);
        let doc = &packet.documents[0];
        assert_eq!(doc.doc_id.as_deref(), Some(id));
        assert!(doc.diff_base_head.contains("two"));
        assert!(doc.diff_base_main.is_some());
        // main == base here, so the trial merge introduces no change; still exercises the path.
        assert!(doc.diff_head_main_trial.is_some() || doc.diff_head_main_note.is_some());
        assert_eq!(doc.inferred_ops.len(), 1);
        assert_eq!(doc.inferred_ops[0].op.to_string(), "append");
        let _ = Layer::Reference; // keep the import meaningful without over-asserting on layer

        // structural facts are computed straight from parse::parse_body, not the diff text: a
        // new `# two` heading is a plain add, the `t` section's own text is untouched, and no
        // frontmatter field changed between base and head.
        assert_eq!(doc.structure.sections_added, vec!["two".to_string()]);
        assert!(doc.structure.sections_removed.is_empty());
        assert!(doc.structure.sections_changed.is_empty());
        assert!(doc.structure.headings_renamed.is_empty());
        assert!(doc.structure.frontmatter_changed.is_empty());
        assert_eq!(doc.structure.words_before, 2);
        assert_eq!(doc.structure.words_after, 4);

        // the rendered packet carries a `[document.structure]` block ahead of the diffs, so a
        // reviewer sees the deterministic facts before reading the prose diff.
        let rendered = packet.render();
        let structure_pos = rendered.find("[document.structure]").expect("structure block present");
        let diff_pos = rendered.find("diff_base_head").expect("diff present");
        assert!(structure_pos < diff_pos, "structure facts must render before the diffs");
        assert!(rendered.contains("sections_added = [\"two\"]"));
        assert!(rendered.contains("words_before = 2"));
        assert!(rendered.contains("words_after = 4"));
    }

    #[test]
    fn truncate_chars_adds_marker_only_when_over_limit() {
        assert_eq!(truncate_chars("short", 100), "short");
        let long = "a".repeat(50);
        let truncated = truncate_chars(&long, 10);
        assert!(truncated.starts_with(&"a".repeat(10)));
        assert!(truncated.contains("truncated"));
    }
}
