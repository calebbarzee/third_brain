//! the context packet handed to the curate model: task, candidate documents with their
//! allowed operations, excerpts, neighbors, and style. design.md §6, §16.

use crate::config::Config;
use crate::index;
use crate::model::{Frontmatter, MergeClass};
use crate::parse::body::parse_body;
use crate::policy::{self, Actor, AllowedOp, DocPolicyView};
use crate::session;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

const DEFAULT_STYLE: &str = include_str!("../../prompts/style.md");

/// (heading slug path, matched snippet) pairs retrieval found for one document.
type MatchedSections = Vec<(String, String)>;
/// doc_id -> (best fts5 score seen across every term searched, its matched sections).
type MatchedDocs = BTreeMap<String, (f64, MatchedSections)>;

/// one candidate document the model may act on.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub layer: String,
    pub status: String,
    pub agent_policy: String,
    pub criticality_pct: Option<f64>,
    pub headings: Vec<String>,
    /// (heading slug path, matched snippet) pairs retrieval found for this candidate.
    pub matched_sections: Vec<(String, String)>,
    pub allowed_ops: Vec<AllowedOp>,
    pub budget_headroom_words: i64,
}

/// one piece of source material prompting this run: a capture's text or the free-text proposal.
#[derive(Debug, Clone, Serialize)]
pub struct Excerpt {
    pub source_uri: String,
    pub text: String,
    pub untrusted: bool,
}

/// the assembled packet, bounded to `manifest.toml [packet] max_tokens` (design.md §16).
#[derive(Debug, Clone, Serialize)]
pub struct Packet {
    pub task: String,
    pub objective: String,
    pub candidates: Vec<Candidate>,
    pub excerpts: Vec<Excerpt>,
    /// one-hop (id, title) neighbors of the candidates: both what they reference and
    /// what references them.
    pub neighbors: Vec<(String, String)>,
    pub sessions_hint: Vec<String>,
    pub style: String,
}

fn load_style(cfg: &Config) -> Result<String> {
    let path = cfg.root.join("prompts").join("style.md");
    if path.is_file() {
        Ok(std::fs::read_to_string(&path)?)
    } else {
        Ok(DEFAULT_STYLE.to_string())
    }
}

fn graph_counts(conn: &Connection, id: &str) -> (u32, u32) {
    conn.query_row("select in_canon, in_code from graph_metrics where doc_id = ?1", params![id], |r| {
        Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32))
    })
    .unwrap_or((0, 0))
}

fn crit_band(conn: &Connection, id: &str) -> Option<MergeClass> {
    let band: Option<String> = conn.query_row("select crit_band from documents where id = ?1", params![id], |r| r.get(0)).optional().ok().flatten();
    band.map(|b| if b == "low" { MergeClass::Auto } else { MergeClass::Review })
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "your", "with", "this", "that", "from", "have", "has", "had", "was",
    "were", "will", "would", "should", "could", "can", "about", "into", "onto", "over", "under", "how", "what", "when",
    "where", "why", "which", "who", "whom", "its", "a", "an", "of", "to", "in", "on", "at", "by", "is", "it", "as", "be",
    "or", "if", "do", "does", "did", "so", "we", "they", "he", "she", "them", "his", "her", "our", "us", "i", "just",
    "also", "than", "then", "there", "here", "out", "up", "down", "one", "two", "get", "got", "use", "used", "using",
    "new", "some", "any", "all", "need", "needs", "needed",
];

/// distinctive search terms from free text: lowercase word-like tokens (letters, digits, `.`,
/// `_`, `-`), length 3 or more, minus `STOPWORDS`, deduped in first-seen order, then reordered so
/// a token carrying a digit (an ip, a version, a port number) or an otherwise longer word --
/// used here as a cheap proxy for "rare", with no corpus statistics on hand -- sorts first;
/// capped at 12. AND-ing every word of a whole excerpt together (fts5's reading of several bare
/// terms) almost never matches one real chunk, which is why `build` searches per term instead
/// (see its own comment).
fn extract_terms(text: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut terms: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == '_' || c == '-')) {
        let token = raw.trim_matches(|c: char| c == '.' || c == '_' || c == '-');
        if token.len() < 3 {
            continue;
        }
        let lower = token.to_lowercase();
        if STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower.clone()) {
            terms.push(lower);
        }
    }
    terms.sort_by_key(|t| {
        let has_digit = t.bytes().any(|b| b.is_ascii_digit());
        (if has_digit { 0 } else { 1 }, std::cmp::Reverse(t.len()))
    });
    terms.truncate(12);
    terms
}

/// search once per term (each call is a single fts5 literal, which `index::search` already
/// sanitizes safely) and union the hits into `matched`, keeping the best score and every
/// distinct matched section per document. this is the OR-of-terms retrieval `build` wants:
/// `index::search`'s own query parameter joins multiple bare words with fts5's implicit AND
/// (see its `sanitize_fts_query`), and that parser also does not treat a literal `OR` in the
/// input as its own operator (it quotes it as just another literal term), so passing one
/// OR-joined string through it cannot produce OR semantics -- issuing one search per term and
/// merging here is the equivalent that works within that existing, unmodified contract.
fn search_terms(conn: &Connection, terms: &[String], matched: &mut MatchedDocs) {
    let opts = index::SearchOpts { limit: 10, status_any: false, include_captures: false };
    for term in terms {
        let Ok(hits) = index::search(conn, term, &opts) else { continue };
        for hit in hits {
            let entry = matched.entry(hit.doc_id.clone()).or_insert((0.0, Vec::new()));
            if !entry.1.iter().any(|(p, _)| p == &hit.heading_path) {
                entry.1.push((hit.heading_path.clone(), hit.snippet.clone()));
            }
            if hit.score > entry.0 {
                entry.0 = hit.score;
            }
        }
    }
}

/// top 10 candidate documents for `texts` (the excerpts prompting this run), each with its
/// `allowed_ops` for `actor`, one-hop neighbors, and every other field the curate prompt needs.
/// a missing index artifact (no `kb index build` yet) yields no candidates, not an error.
pub fn build(cfg: &Config, actor: &Actor, task: &str, texts: &[(String, String)]) -> Result<Packet> {
    let conn = index::open_newest(cfg).ok();

    // doc_id -> (best score, matched (heading_path, snippet) pairs). each excerpt contributes
    // its own distinctive terms, searched and unioned (OR), not its raw text AND-ed as a whole
    // (see `search_terms`'s comment); if no excerpt term matched anything, fall back to the
    // task text's own terms rather than returning an empty packet.
    let mut matched: MatchedDocs = BTreeMap::new();
    if let Some(conn) = &conn {
        for (_, text) in texts {
            let terms = extract_terms(text);
            if !terms.is_empty() {
                search_terms(conn, &terms, &mut matched);
            }
        }
        if matched.is_empty() {
            let task_terms = extract_terms(task);
            if !task_terms.is_empty() {
                search_terms(conn, &task_terms, &mut matched);
            }
        }
    }

    let mut ranked: Vec<(String, f64, MatchedSections)> = matched.into_iter().map(|(id, (score, ms))| (id, score, ms)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    ranked.truncate(10);

    let mut candidates = Vec::new();
    let mut neighbor_ids: BTreeMap<String, String> = BTreeMap::new();
    if let Some(conn) = &conn {
        for (id, _score, matched_sections) in &ranked {
            let Ok(Some(rec)) = index::get(conn, id) else { continue };
            let Ok(fm) = serde_json::from_value::<Frontmatter>(rec.frontmatter.clone()) else { continue };
            let band = crit_band(conn, id);
            let (in_canon, in_code) = graph_counts(conn, id);
            let Ok(view) = DocPolicyView::from_frontmatter(cfg, &fm, rec.word_count as usize, band, in_canon, in_code) else { continue };
            let allowed_ops = policy::allowed_ops(cfg, actor, &view);
            let layer = view.layer;
            let budget = cfg.budget_for(layer) as i64;
            let headings: Vec<String> = parse_body(&rec.body).sections.into_iter().filter(|s| !s.heading.is_empty()).map(|s| s.path).collect();

            for r in &rec.refs_out {
                if let Some(rid) = &r.resolved_id {
                    neighbor_ids.entry(rid.clone()).or_default();
                }
            }
            for r in &rec.refs_in {
                neighbor_ids.entry(r.from_id.clone()).or_default();
            }

            candidates.push(Candidate {
                id: rec.id.clone(),
                title: rec.title.clone().unwrap_or_else(|| rec.id.clone()),
                layer: layer.as_str().to_string(),
                status: rec.status.clone().unwrap_or_default(),
                agent_policy: view.agent_policy.as_str().to_string(),
                criticality_pct: rec.criticality_pct,
                headings,
                matched_sections: matched_sections.clone(),
                allowed_ops,
                budget_headroom_words: budget - rec.word_count,
            });
        }
        for (nid, title) in neighbor_ids.iter_mut() {
            if let Ok(Some(rec)) = index::get(conn, nid) {
                *title = rec.title.unwrap_or_else(|| nid.clone());
            }
        }
    }
    let neighbors: Vec<(String, String)> = neighbor_ids.into_iter().filter(|(id, _)| !candidates.iter().any(|c| &c.id == id)).collect();

    let cap_chars = (cfg.manifest.packet.capture_excerpt_tokens as usize).saturating_mul(4).max(1);
    let excerpts: Vec<Excerpt> = texts
        .iter()
        .map(|(uri, text)| {
            let capped = if text.chars().count() > cap_chars {
                text.chars().take(cap_chars).collect::<String>() + "… [truncated]"
            } else {
                text.clone()
            };
            Excerpt { source_uri: uri.clone(), text: capped, untrusted: true }
        })
        .collect();

    // an advisory hint only: sessions that are done (merged, abandoned) or mid-automated-flow
    // (merging, held) are not useful for a model deciding where to land new operations.
    let sessions_hint: Vec<String> = session::sessions_report(cfg)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| matches!(r.state, session::SessionState::Open | session::SessionState::Settled | session::SessionState::NeedsAuthor | session::SessionState::NeedsHuman))
        .map(|r| format!("{} {} {} {}", r.id, r.state, r.actor, r.objective))
        .collect();

    let mut packet = Packet { task: task.to_string(), objective: task.to_string(), candidates, excerpts, neighbors, sessions_hint, style: load_style(cfg)? };

    let max_chars = (cfg.manifest.packet.max_tokens as usize).saturating_mul(4).max(1);
    while render(&packet).chars().count() > max_chars && !packet.excerpts.is_empty() {
        packet.excerpts.pop();
    }
    Ok(packet)
}

/// render the packet as readable, toml-ish text for the model's user message.
pub fn render(packet: &Packet) -> String {
    let mut out = String::new();
    out.push_str(&format!("task = {:?}\n", packet.task));
    out.push_str(&format!("objective = {:?}\n\n", packet.objective));

    for c in &packet.candidates {
        out.push_str("[[candidates]]\n");
        out.push_str(&format!("id = {:?}\n", c.id));
        out.push_str(&format!("title = {:?}\n", c.title));
        out.push_str(&format!("layer = {:?}\n", c.layer));
        out.push_str(&format!("status = {:?}\n", c.status));
        out.push_str(&format!("agent_policy = {:?}\n", c.agent_policy));
        out.push_str(&format!("criticality_pct = {}\n", c.criticality_pct.map(|v| v.to_string()).unwrap_or_else(|| "unknown".to_string())));
        out.push_str(&format!("budget_headroom_words = {}\n", c.budget_headroom_words));
        out.push_str(&format!("headings = {:?}\n", c.headings));
        out.push_str("matched_sections = [\n");
        for (path, snippet) in &c.matched_sections {
            out.push_str(&format!("  {{ section = {path:?}, snippet = {snippet:?} }},\n"));
        }
        out.push_str("]\n");
        out.push_str("allowed_ops = [\n");
        for a in &c.allowed_ops {
            out.push_str(&format!("  {{ op = {:?}, class = {:?}, reasons = {:?} }},\n", a.op.as_str(), format!("{:?}", a.class).to_lowercase(), a.reasons));
        }
        out.push_str("]\n\n");
    }

    for e in &packet.excerpts {
        out.push_str("[[excerpts]]\n");
        out.push_str(&format!("source_uri = {:?}\n", e.source_uri));
        out.push_str(&format!("untrusted = {}\n", e.untrusted));
        out.push_str("text = \"\"\"\n");
        out.push_str(&e.text);
        out.push_str("\n\"\"\"\n\n");
    }

    out.push_str(&format!("neighbors = {:?}\n", packet.neighbors));
    // the hint tells the model other work is in flight without offering ids it could mistake for
    // documents; the full list stays in the packet struct for the run log.
    out.push_str(&format!("other_open_sessions = {}\n\n", packet.sessions_hint.len()));
    out.push_str("style = \"\"\"\n");
    out.push_str(&packet.style);
    out.push_str("\n\"\"\"\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::git;
    use crate::policy::{Actor, ActorKind};
    use std::path::Path;

    const ID_ROOT: &str = "01HZZZZZZZ0000000000000AA1";
    const ID_INSTALL: &str = "01HZZZZZZZ0000000000000AA2";
    const ID_OTHER: &str = "01HZZZZZZZ0000000000000AA3";
    const ID_ORPHAN: &str = "01HZZZZZZZ0000000000000AA4";

    fn write_doc(root: &Path, id: &str, extra: &str, body: &str) {
        std::fs::write(root.join("docs").join(format!("{id}.md")), format!("+++\nid = \"{id}\"\n{extra}\n+++\n\n{body}")).unwrap();
    }

    fn seed_repo_with_index() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        git::init_for_tests(root).unwrap();
        // grant svc-curate write access via the "agents" group, so `allowed_ops` is non-empty.
        std::fs::write(root.join("kb.toml"), "[defaults]\nwrite_groups = [\"agents\"]\n").unwrap();
        std::fs::write(
            root.join("groups.toml"),
            "[principals.svc-curate]\nkind = \"service\"\nowner = \"caleb\"\ngroups = [\"agents\"]\nlayer_ceiling = \"principle\"\n\n[groups.agents]\nmembers = [\"svc-curate\"]\n",
        )
        .unwrap();
        write_doc(
            root,
            ID_ROOT,
            "layer = \"reference\"\ntitle = \"Root guide\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            &format!("# Root guide\n\nStart here. See [install](kb:{ID_INSTALL}) for setup.\n"),
        );
        write_doc(
            root,
            ID_INSTALL,
            "layer = \"reference\"\ntitle = \"Install steps\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# Install steps\n\nRun the installer on a fresh machine.\n\n## Linux\n\nUse the package manager.\n",
        );
        write_doc(
            root,
            ID_OTHER,
            "layer = \"reference\"\ntitle = \"Unrelated\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# Unrelated\n\nSomething about kayaking.\n",
        );
        write_doc(
            root,
            ID_ORPHAN,
            "layer = \"reference\"\ntitle = \"Orphan\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"caleb\"",
            "# Orphan\n\nNothing links here.\n",
        );
        git::run(root, &["add", "-A"]).unwrap();
        git::run(root, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "seed"]).unwrap();
        index::build(&Config::load(root).unwrap(), None).unwrap();
        dir
    }

    fn agent(principal: &str) -> Actor {
        Actor { kind: ActorKind::Agent, principal: principal.to_string(), skill: Some("curate".to_string()) }
    }

    #[test]
    fn extract_terms_drops_stopwords_and_prefers_digits_and_length() {
        let terms = extract_terms("how do you set a static ip with nmcli on 192.168.50.10 for the launcher");
        assert!(terms.contains(&"static".to_string()));
        assert!(terms.contains(&"nmcli".to_string()));
        assert!(terms.contains(&"192.168.50.10".to_string()));
        assert!(terms.contains(&"launcher".to_string()));
        for stopword in ["how", "do", "you", "a", "with", "on", "for", "the"] {
            assert!(!terms.contains(&stopword.to_string()), "{stopword} should have been dropped: {terms:?}");
        }
        // the digit-bearing term sorts before every plain word.
        assert_eq!(terms[0], "192.168.50.10", "{terms:?}");
    }

    #[test]
    fn extract_terms_caps_at_twelve_and_dedupes() {
        let text = "alpha alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november";
        let terms = extract_terms(text);
        assert!(terms.len() <= 12, "{terms:?}");
        let mut seen = std::collections::HashSet::new();
        assert!(terms.iter().all(|t| seen.insert(t.clone())), "duplicate term: {terms:?}");
    }

    #[test]
    fn build_finds_matching_candidates_and_neighbors() {
        let dir = seed_repo_with_index();
        let cfg = Config::load(dir.path()).unwrap();
        let texts = vec![("capture:01ABC".to_string(), "package manager".to_string())];
        let packet = build(&cfg, &agent("svc-curate"), "curate one capture", &texts).unwrap();

        assert!(packet.candidates.iter().any(|c| c.id == ID_INSTALL), "{:?}", packet.candidates);
        let install = packet.candidates.iter().find(|c| c.id == ID_INSTALL).unwrap();
        assert!(!install.headings.is_empty());
        assert!(!install.matched_sections.is_empty());
        assert!(!install.allowed_ops.is_empty());
        assert!(packet.neighbors.iter().any(|(id, _)| id == ID_ROOT || id == ID_INSTALL), "{:?}", packet.neighbors);
        assert_eq!(packet.excerpts.len(), 1);
        assert!(packet.excerpts[0].untrusted);
        assert!(!packet.style.is_empty());
    }

    #[test]
    fn build_with_no_index_yields_no_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let texts = vec![("proposal:free-text".to_string(), "anything at all".to_string())];
        let packet = build(&cfg, &agent("svc-curate"), "t", &texts).unwrap();
        assert!(packet.candidates.is_empty());
        assert_eq!(packet.excerpts.len(), 1);
    }

    #[test]
    fn render_includes_task_and_candidates() {
        let dir = seed_repo_with_index();
        let cfg = Config::load(dir.path()).unwrap();
        let texts = vec![("capture:01ABC".to_string(), "package manager".to_string())];
        let packet = build(&cfg, &agent("svc-curate"), "curate one capture", &texts).unwrap();
        let text = render(&packet);
        assert!(text.contains("task ="));
        assert!(text.contains("[[candidates]]"));
        assert!(text.contains("allowed_ops"));
    }
}
