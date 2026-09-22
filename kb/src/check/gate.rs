//! the structure gate: classifies a change auto or review class from what it touches.
//! contract: design.md §15.1, applied to one document's base/head pair.

use crate::config::Config;
use crate::model::{AgentPolicy, Document, Frontmatter, Layer, MergeClass, Status};
use std::collections::HashMap;

/// the gate's verdict for one document's change: `class`, and the reasons behind it. an empty
/// `reasons` means `class == Auto`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GateDecision {
    pub class: MergeClass,
    pub reasons: Vec<String>,
}

/// classify a document's change. `base` is `None` for a `create`. `band` is the document's
/// criticality band (`None` below the minimum corpus, per design.md §17.4). `actor_is_agent`
/// gates the rules that apply only to agent changes; humans are classified the same way but
/// this function never denies anything, it only picks `auto` or `review`.
pub fn classify_change(
    cfg: &Config,
    base: Option<&Document>,
    head: &Document,
    band: Option<MergeClass>,
    actor_is_agent: bool,
) -> GateDecision {
    let mut reasons = Vec::new();
    let layer = head.frontmatter.layer.unwrap_or(cfg.kb.defaults.layer);
    let structure = head.frontmatter.structure.clone().unwrap_or_else(|| cfg.structure_for(layer));
    let critical_all = structure.critical.iter().any(|c| c == "*");

    let base_fm = base.map(|d| &d.frontmatter);
    let base_body = base.map(|d| d.body.as_str());

    if actor_is_agent
        && let Some(bfm) = base_fm
        && policy_fields_changed(bfm, &head.frontmatter)
    {
        reasons.push("policy field changed by an agent".to_string());
    }

    let touched = touched_sections(base, head);
    let critical_touched: Vec<&str> = touched
        .iter()
        .map(String::as_str)
        .filter(|s| critical_all || structure.critical.iter().any(|c| c == s))
        .collect();
    if !critical_touched.is_empty() {
        reasons.push(format!("critical section(s) changed: {}", critical_touched.join(", ")));
    }

    if let Some(b) = base {
        let head_paths: std::collections::HashSet<&str> = head.sections.iter().map(|s| s.path.as_str()).collect();
        for req in &structure.required {
            let was_present = b.sections.iter().any(|s| &s.path == req);
            if was_present && !head_paths.contains(req.as_str()) {
                reasons.push(format!("required heading `{req}` removed or renamed"));
            }
        }
    }

    let body_changed = base_body.map(|b| b != head.body).unwrap_or(!head.body.trim().is_empty());
    if matches!(band, Some(MergeClass::Review)) && body_changed {
        reasons.push("document is above the criticality band and its body changed".to_string());
    }

    let agent_policy = head.frontmatter.agent_policy.unwrap_or_else(|| cfg.agent_policy_for(layer));
    let fm_changed = base_fm.map(|b| b != &head.frontmatter).unwrap_or(true);
    if actor_is_agent && agent_policy == AgentPolicy::Locked && (fm_changed || body_changed) {
        reasons.push("agent_policy is locked".to_string());
    }

    if actor_is_agent && agent_policy == AgentPolicy::AppendOnly && !is_append_only_change(base, head) {
        reasons.push("agent_policy is append-only and this change is not a pure append".to_string());
    }

    // design.md §11: `supersede` (status -> superseded) and `retire` (status -> archived) are
    // always review class for a `decision`-layer document or higher (principle, design,
    // decision), regardless of which sections the banner insertion happens to touch -- a banner
    // landing before the first heading lands in a new `_preamble` section, which the
    // touched-section rule above never classifies as critical on its own.
    let base_status = base_fm.and_then(|b| b.status);
    let is_supersede_or_retire = matches!(head.frontmatter.status, Some(Status::Superseded) | Some(Status::Archived))
        && head.frontmatter.status != base_status
        && head.frontmatter.superseded_by.is_some();
    if is_supersede_or_retire && layer.rank() <= Layer::Decision.rank() {
        let verb = if head.frontmatter.status == Some(Status::Archived) { "retire" } else { "supersede" };
        reasons.push(format!("{verb} of a `{layer}` document or higher is always review class"));
    }

    if matches!(head.frontmatter.status, Some(Status::Superseded) | Some(Status::Archived)) {
        if !super::banner::has_banner(&head.body) {
            reasons.push("superseded/archived banner is missing".to_string());
        } else if let Some(bb) = base_body
            && super::banner::has_banner(bb)
            && first_line(bb) != first_line(&head.body)
        {
            reasons.push("superseded/archived banner was altered".to_string());
        }
    }

    let class = if reasons.is_empty() { MergeClass::Auto } else { MergeClass::Review };
    GateDecision { class, reasons }
}

fn policy_fields_changed(base: &Frontmatter, head: &Frontmatter) -> bool {
    base.id != head.id
        || base.layer != head.layer
        || base.owner != head.owner
        || base.access != head.access
        || base.agent_policy != head.agent_policy
}

/// slug paths whose text changed, were added, or were removed between `base` (or nothing, for
/// a create) and `head`.
fn touched_sections(base: Option<&Document>, head: &Document) -> Vec<String> {
    // trailing whitespace is trimmed before comparing: inserting a new heading after a section
    // adds a blank-line separator to the end of that section's byte range even though its
    // content did not change, and that must not count as a touch.
    let head_map: HashMap<String, &str> = head
        .sections
        .iter()
        .map(|s| (s.path.clone(), head.body[s.start..s.end].trim_end()))
        .collect();
    let base_map: HashMap<String, &str> = match base {
        Some(b) => b
            .sections
            .iter()
            .map(|s| (s.path.clone(), b.body[s.start..s.end].trim_end()))
            .collect(),
        None => HashMap::new(),
    };
    let mut out = Vec::new();
    for (path, text) in &head_map {
        match base_map.get(path) {
            Some(btext) if btext == text => {}
            _ => out.push(path.clone()),
        }
    }
    for path in base_map.keys() {
        if !head_map.contains_key(path) {
            out.push(path.clone());
        }
    }
    out
}

/// true when the change is a pure append: every base section is byte-identical in head, any new
/// sections come after them, and `refs` only gained entries; used to auto-clear `append-only`.
fn is_append_only_change(base: Option<&Document>, head: &Document) -> bool {
    let body_ok = match base {
        None => true,
        Some(b) => {
            let base_paths: Vec<&str> = b.sections.iter().map(|s| s.path.as_str()).collect();
            let head_paths: Vec<&str> = head.sections.iter().map(|s| s.path.as_str()).collect();
            head_paths.len() >= base_paths.len()
                && head_paths[..base_paths.len()] == base_paths[..]
                && b.sections
                    .iter()
                    .zip(head.sections.iter())
                    .all(|(bs, hs)| b.body[bs.start..bs.end].trim_end() == head.body[hs.start..hs.end].trim_end())
        }
    };
    let refs_ok = base.is_none_or(|b| b.frontmatter.refs.iter().all(|r| head.frontmatter.refs.contains(r)));
    let other_fields_ok = base.is_none_or(|b| {
        let mut hb = head.frontmatter.clone();
        hb.refs = b.frontmatter.refs.clone();
        hb == b.frontmatter
    });
    body_ok && refs_ok && other_fields_ok
}

fn first_line(body: &str) -> Option<&str> {
    body.lines().map(str::trim).find(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_document;

    fn cfg() -> Config {
        let dir = tempfile::tempdir().unwrap();
        Config::load(dir.path()).unwrap()
    }

    const FM: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"procedure\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n";

    #[test]
    fn auto_when_nothing_touches_structure() {
        let c = cfg();
        let base = parse_document(&format!("{FM}# one\n\ntext\n")).unwrap();
        let head = parse_document(&format!("{FM}# one\n\ntext\n\n# two\n\nmore\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head, None, true);
        assert_eq!(d.class, MergeClass::Auto);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn agent_policy_field_change_is_review_for_agent_only() {
        let c = cfg();
        let base = parse_document(&format!("{FM}# one\n\ntext\n")).unwrap();
        let head_text = FM.replace("owner = \"caleb\"", "owner = \"ana\"");
        let head = parse_document(&format!("{head_text}# one\n\ntext\n")).unwrap();
        let agent = classify_change(&c, Some(&base), &head, None, true);
        assert_eq!(agent.class, MergeClass::Review);
        let human = classify_change(&c, Some(&base), &head, None, false);
        assert_eq!(human.class, MergeClass::Auto);
    }

    #[test]
    fn critical_section_change_is_review_regardless_of_actor() {
        let c = cfg();
        let base = parse_document(&format!("{FM}# prerequisites\n\nold\n\n# rollback\n\nold\n")).unwrap();
        let head = parse_document(&format!("{FM}# prerequisites\n\nnew\n\n# rollback\n\nold\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head, None, false);
        assert_eq!(d.class, MergeClass::Review);
        assert!(d.reasons[0].contains("prerequisites"));
    }

    #[test]
    fn locked_agent_policy_forces_review_for_agents() {
        let c = cfg();
        let locked_fm = FM.replace("agent_policy = \"editable\"", "agent_policy = \"locked\"");
        let base = parse_document(&format!("{locked_fm}# one\n\ntext\n")).unwrap();
        let head = parse_document(&format!("{locked_fm}# one\n\ntext more\n")).unwrap();
        let agent = classify_change(&c, Some(&base), &head, None, true);
        assert_eq!(agent.class, MergeClass::Review);
        let human = classify_change(&c, Some(&base), &head, None, false);
        assert_eq!(human.class, MergeClass::Auto);
    }

    #[test]
    fn append_only_allows_pure_append_for_agents() {
        let c = cfg();
        let ao_fm = FM.replace("agent_policy = \"editable\"", "agent_policy = \"append-only\"");
        let base = parse_document(&format!("{ao_fm}# one\n\ntext\n")).unwrap();
        let head_append = parse_document(&format!("{ao_fm}# one\n\ntext\n\n# two\n\nmore\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head_append, None, true);
        assert_eq!(d.class, MergeClass::Auto);

        let head_revise = parse_document(&format!("{ao_fm}# one\n\nchanged\n")).unwrap();
        let d2 = classify_change(&c, Some(&base), &head_revise, None, true);
        assert_eq!(d2.class, MergeClass::Review);
    }

    #[test]
    fn band_review_forces_review_on_any_body_change() {
        let c = cfg();
        let base = parse_document(&format!("{FM}# one\n\ntext\n")).unwrap();
        let head = parse_document(&format!("{FM}# one\n\ntext more\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head, Some(MergeClass::Review), false);
        assert_eq!(d.class, MergeClass::Review);
    }

    #[test]
    fn decision_supersede_is_always_review() {
        let c = cfg();
        let decision_fm = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"decision\"\ntitle = \"a decision\"\nstatus = \"canon\"\nagent_policy = \"append-only\"\nowner = \"caleb\"\n+++\n\n";
        let base = parse_document(&format!("{decision_fm}# decision\n\nwe chose x\n\n# consequences\n\ny follows\n")).unwrap();
        let superseded_fm = decision_fm
            .replace("status = \"canon\"", "status = \"superseded\"")
            .replacen("owner = \"caleb\"\n", "owner = \"caleb\"\nsuperseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n", 1);
        let banner_line = "> superseded 2026-01-01 by [x](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P). kept for history; not current.\n";
        // the banner lands before the first heading, in a new `_preamble` section -- the
        // `decision`/`consequences` critical sections themselves are untouched.
        let head = parse_document(&format!("{superseded_fm}{banner_line}\n# decision\n\nwe chose x\n\n# consequences\n\ny follows\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head, None, false);
        assert_eq!(d.class, MergeClass::Review);
        assert!(d.reasons.iter().any(|r| r.contains("supersede") && r.contains("decision")), "reasons: {:?}", d.reasons);
    }

    #[test]
    fn reference_supersede_is_not_forced_review() {
        // a `reference` document (below `decision`) is not covered by the always-review rule; a
        // clean supersede with no other touched critical structure stays auto.
        let c = cfg();
        let ref_fm = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n";
        let base = parse_document(&format!("{ref_fm}# one\n\ntext\n")).unwrap();
        let superseded_fm = ref_fm
            .replace("status = \"canon\"", "status = \"superseded\"")
            .replacen("owner = \"caleb\"\n", "owner = \"caleb\"\nsuperseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n", 1);
        let banner_line = "> superseded 2026-01-01 by [x](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P). kept for history; not current.\n";
        let head = parse_document(&format!("{superseded_fm}{banner_line}\n# one\n\ntext\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head, None, false);
        assert_eq!(d.class, MergeClass::Auto);
    }

    #[test]
    fn banner_tampering_is_review() {
        let c = cfg();
        let sup_fm = FM
            .replace("status = \"canon\"", "status = \"superseded\"")
            .replace("+++\n\n", "superseded_by = \"kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P\"\n+++\n\n");
        let banner_line = "> superseded 2026-01-01 by [x](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0P). kept for history; not current.\n";
        let base = parse_document(&format!("{sup_fm}{banner_line}\n# one\n\ntext\n")).unwrap();
        let head_no_banner = parse_document(&format!("{sup_fm}# one\n\ntext\n")).unwrap();
        let d = classify_change(&c, Some(&base), &head_no_banner, None, false);
        assert_eq!(d.class, MergeClass::Review);
        assert!(d.reasons.iter().any(|r| r.contains("banner")));
    }
}
