//! hierarchy and succession graph checks. contract in design.md §8: both graphs are acyclic;
//! iterative depth-first search with three-color marking; self-edges, a parent of lower rank
//! than its child, and dangling `kb:` targets are also rejected or flagged.

use crate::model::{Finding, Frontmatter, Severity, parse_kb_uri};
use std::collections::HashMap;

/// check the hierarchy graph (`parent` edges, child -> parent) and the succession graph
/// (`supersedes` refs and `superseded_by`, old -> new) over the given documents.
/// `docs` is every document's id paired with its frontmatter; a target outside this set is
/// reported as dangling rather than treated as an error, since callers may pass a subset.
pub fn check_graph(docs: &[(String, Frontmatter)]) -> Vec<Finding> {
    let by_id: HashMap<&str, &Frontmatter> = docs.iter().map(|(id, fm)| (id.as_str(), fm)).collect();
    let mut findings = Vec::new();

    let mut hierarchy: HashMap<String, Vec<String>> = HashMap::new();
    for (id, fm) in docs {
        let Some(parent) = &fm.parent else { continue };
        let Some((pid, _)) = parse_kb_uri(parent) else { continue };
        if &pid == id {
            findings.push(self_edge("hierarchy", id));
            continue;
        }
        hierarchy.entry(id.clone()).or_default().push(pid.clone());
        match by_id.get(pid.as_str()) {
            None => findings.push(dangling("hierarchy", id, &pid)),
            Some(parent_fm) => {
                if let (Some(child_layer), Some(parent_layer)) = (fm.layer, parent_fm.layer)
                    && parent_layer.rank() > child_layer.rank()
                {
                    findings.push(Finding {
                        severity: Severity::Error,
                        code: "parent-rank".into(),
                        message: format!(
                            "{id}: parent {pid} is layer {parent_layer} (rank {}), lower than child layer {child_layer} (rank {})",
                            parent_layer.rank(),
                            child_layer.rank()
                        ),
                        path: None,
                        line: None,
                        doc_id: Some(id.clone()),
                    });
                }
            }
        }
    }

    let mut succession: HashMap<String, Vec<String>> = HashMap::new();
    for (id, fm) in docs {
        if let Some(sb) = &fm.superseded_by
            && sb != "none"
            && let Some((tid, _)) = parse_kb_uri(sb)
        {
            if &tid == id {
                findings.push(self_edge("succession", id));
            } else {
                succession.entry(id.clone()).or_default().push(tid.clone());
                if !by_id.contains_key(tid.as_str()) {
                    findings.push(dangling("succession", id, &tid));
                }
            }
        }
        for r in &fm.refs {
            if r.rel != "supersedes" {
                continue;
            }
            let Some((old_id, _)) = parse_kb_uri(&r.uri) else { continue };
            if &old_id == id {
                findings.push(self_edge("succession", id));
                continue;
            }
            succession.entry(old_id.clone()).or_default().push(id.clone());
            if !by_id.contains_key(old_id.as_str()) {
                findings.push(dangling("succession", id, &old_id));
            }
        }
    }

    findings.extend(find_cycles(&hierarchy, "hierarchy"));
    findings.extend(find_cycles(&succession, "succession"));
    findings
}

fn self_edge(kind: &str, id: &str) -> Finding {
    Finding {
        severity: Severity::Error,
        code: "self-edge".into(),
        message: format!("{id}: {kind} edge points to itself"),
        path: None,
        line: None,
        doc_id: Some(id.to_string()),
    }
}

fn dangling(kind: &str, from: &str, target: &str) -> Finding {
    Finding {
        severity: Severity::Warning,
        code: "dangling".into(),
        message: format!("{from}: {kind} target kb:{target} does not resolve to a known document"),
        path: None,
        line: None,
        doc_id: Some(from.to_string()),
    }
}

/// iterative three-color depth-first search over `graph` (node -> its out-edges), reporting
/// each back edge found as one cycle finding naming `kind` (`hierarchy` or `succession`).
fn find_cycles(graph: &HashMap<String, Vec<String>>, kind: &str) -> Vec<Finding> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut color: HashMap<String, Color> = graph.keys().cloned().map(|k| (k, Color::White)).collect();
    let mut findings = Vec::new();
    let starts: Vec<String> = graph.keys().cloned().collect();

    for start in starts {
        if color.get(&start).copied() != Some(Color::White) {
            continue;
        }
        let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
        color.insert(start, Color::Gray);

        while let Some((node, idx)) = stack.pop() {
            let neighbors = graph.get(&node).cloned().unwrap_or_default();
            if idx < neighbors.len() {
                let next = neighbors[idx].clone();
                stack.push((node, idx + 1));
                match color.get(&next).copied().unwrap_or(Color::White) {
                    Color::White => {
                        color.insert(next.clone(), Color::Gray);
                        stack.push((next, 0));
                    }
                    Color::Gray => {
                        let start_pos = stack.iter().position(|(n, _)| n == &next).unwrap_or(0);
                        let mut cycle: Vec<String> = stack[start_pos..].iter().map(|(n, _)| n.clone()).collect();
                        cycle.push(next);
                        findings.push(Finding {
                            severity: Severity::Error,
                            code: "cycle".into(),
                            message: format!("{kind} cycle: {}", cycle.join(" -> ")),
                            path: None,
                            line: None,
                            doc_id: cycle.first().cloned(),
                        });
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node, Color::Black);
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Layer;

    // parent/superseded_by are `kb:<ulid>` uris, so tests need real 26-char crockford ids.
    const ID_A: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0A";
    const ID_B: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0B";
    const ID_C: &str = "01K5A7Q2XW3ZRCM8T6YB4VJH0C";

    fn fm(parent: Option<&str>, layer: Layer) -> Frontmatter {
        Frontmatter {
            layer: Some(layer),
            parent: parent.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn clean_hierarchy_has_no_findings() {
        let docs = vec![
            (ID_A.to_string(), fm(None, Layer::Principle)),
            (ID_B.to_string(), fm(Some(&format!("kb:{ID_A}")), Layer::Design)),
        ];
        assert!(check_graph(&docs).is_empty());
    }

    #[test]
    fn hierarchy_cycle_detected() {
        let docs = vec![
            (ID_A.to_string(), fm(Some(&format!("kb:{ID_B}")), Layer::Design)),
            (ID_B.to_string(), fm(Some(&format!("kb:{ID_A}")), Layer::Design)),
        ];
        let findings = check_graph(&docs);
        assert!(findings.iter().any(|f| f.code == "cycle" && f.message.contains("hierarchy")));
    }

    #[test]
    fn self_edge_detected() {
        let docs = vec![(ID_A.to_string(), fm(Some(&format!("kb:{ID_A}")), Layer::Design))];
        let findings = check_graph(&docs);
        assert!(findings.iter().any(|f| f.code == "self-edge"));
    }

    #[test]
    fn dangling_parent_is_warning() {
        let docs = vec![(ID_A.to_string(), fm(Some(&format!("kb:{ID_C}")), Layer::Design))];
        let findings = check_graph(&docs);
        let f = findings.iter().find(|f| f.code == "dangling").unwrap();
        assert_eq!(f.severity, Severity::Warning);
    }

    #[test]
    fn parent_rank_too_low_is_error() {
        // parent B is `reference` (rank 4), child A is `principle` (rank 0): parent must be
        // equal or higher (lower or equal rank number), so this is invalid.
        let docs = vec![
            (ID_A.to_string(), fm(Some(&format!("kb:{ID_B}")), Layer::Principle)),
            (ID_B.to_string(), fm(None, Layer::Reference)),
        ];
        let findings = check_graph(&docs);
        assert!(findings.iter().any(|f| f.code == "parent-rank" && f.severity == Severity::Error));
    }

    #[test]
    fn succession_cycle_via_supersedes_ref() {
        use crate::model::Ref;
        // `superseded_by = B` on A is an old(A) -> new(B) edge. A `supersedes` ref on A pointing
        // at B means "A supersedes B", an old(B) -> new(A) edge: together, a 2-cycle.
        let mut a = fm(None, Layer::Reference);
        a.superseded_by = Some(format!("kb:{ID_B}"));
        a.refs.push(Ref {
            rel: "supersedes".into(),
            uri: format!("kb:{ID_B}"),
            sha256: None,
            media_type: None,
        });
        let b = fm(None, Layer::Reference);
        let docs = vec![(ID_A.to_string(), a), (ID_B.to_string(), b)];
        let findings = check_graph(&docs);
        assert!(findings.iter().any(|f| f.code == "cycle" && f.message.contains("succession")));
    }

    #[test]
    fn superseded_by_none_is_not_an_edge() {
        let mut a = fm(None, Layer::Reference);
        a.superseded_by = Some("none".into());
        let docs = vec![("A".to_string(), a)];
        assert!(check_graph(&docs).is_empty());
    }
}
