//! body parser: heading-bounded sections with slug paths, inline links, word count. contract in design.md §7.
use crate::model::{InlineLink, Section};
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct BodyParse {
    pub sections: Vec<Section>,
    pub links: Vec<InlineLink>,
    /// whitespace-separated tokens outside fenced code blocks.
    pub word_count: usize,
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(!?)\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)"#).unwrap())
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})[ \t]+(.*?)[ \t]*(?:#+[ \t]*)?$").unwrap())
}

fn code_span_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`]*`").unwrap())
}

/// lowercase, non-alphanumeric runs become one `-`, trimmed. empty becomes `section`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    if trimmed.is_empty() { "section".to_string() } else { trimmed }
}

/// returns the fence marker if the line opens or closes a fenced code block.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    let indent = line.len() - t.len();
    if indent > 3 {
        return None;
    }
    for ch in ['`', '~'] {
        let n = t.chars().take_while(|c| *c == ch).count();
        if n >= 3 {
            return Some((ch, n));
        }
    }
    None
}

/// the key a section is compared under when diffing two versions of one document. `_body` (a
/// body with no headings) and `_preamble` (text before the first heading) are the same text: an
/// append that adds the first heading turns one into the other without changing it.
pub fn diff_key(path: &str) -> &str {
    if path == "_body" { "_preamble" } else { path }
}

pub fn parse_body(body: &str) -> BodyParse {
    struct Head {
        level: u8,
        text: String,
        start: usize,
        line: usize,
    }
    let mut heads: Vec<Head> = Vec::new();
    let mut links = Vec::new();
    let mut word_count = 0usize;
    let mut in_fence: Option<(char, usize)> = None;
    let mut offset = 0usize;
    for (i, raw) in body.split_inclusive('\n').enumerate() {
        let line_no = i + 1;
        let line = raw.trim_end_matches(['\r', '\n']);
        match in_fence {
            Some((ch, n)) => {
                if let Some((c2, n2)) = fence_marker(line)
                    && c2 == ch && n2 >= n && line.trim().chars().all(|c| c == ch) {
                        in_fence = None;
                    }
            }
            None => {
                if let Some(m) = fence_marker(line) {
                    in_fence = Some(m);
                } else {
                    if let Some(c) = heading_re().captures(line) {
                        heads.push(Head {
                            level: c[1].len() as u8,
                            text: c[2].to_string(),
                            start: offset,
                            line: line_no,
                        });
                    }
                    word_count += line.split_whitespace().filter(|w| !w.chars().all(|c| c == '#')).count();
                    let stripped = code_span_re().replace_all(line, "");
                    for c in link_re().captures_iter(&stripped) {
                        links.push(InlineLink {
                            text: c[2].to_string(),
                            uri: c[3].to_string(),
                            image: &c[1] == "!",
                            line: line_no,
                        });
                    }
                }
            }
        }
        offset += raw.len();
    }
    let end = body.len();
    let mut sections = Vec::new();
    if heads.is_empty() {
        sections.push(Section { path: "_body".into(), level: 0, heading: String::new(), start: 0, end, line: 1 });
        return BodyParse { sections, links, word_count };
    }
    if !body[..heads[0].start].trim().is_empty() {
        sections.push(Section {
            path: "_preamble".into(),
            level: 0,
            heading: String::new(),
            start: 0,
            end: heads[0].start,
            line: 1,
        });
    }
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (idx, h) in heads.iter().enumerate() {
        while stack.last().is_some_and(|(lvl, _)| *lvl >= h.level) {
            stack.pop();
        }
        let base = {
            let mut parts: Vec<&str> = stack.iter().map(|(_, s)| s.as_str()).collect();
            let slug = slugify(&h.text);
            parts.push(&slug);
            parts.join("/")
        };
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        let path = if *n == 1 { base.clone() } else { format!("{base}-{n}") };
        let leaf = path.rsplit('/').next().unwrap().to_string();
        stack.push((h.level, leaf));
        let sec_end = heads[idx + 1..].iter().find(|nx| nx.level <= h.level).map(|nx| nx.start).unwrap_or(end);
        sections.push(Section { path, level: h.level, heading: h.text.clone(), start: h.start, end: sec_end, line: h.line });
    }
    BodyParse { sections, links, word_count }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Install: Linux (x86)"), "install-linux-x86");
        assert_eq!(slugify("  ---  "), "section");
    }

    #[test]
    fn nested_and_duplicate_headings() {
        let body = "intro\n\n# Install\n\n## Linux\n\ntext\n\n## Notes\n\n## Notes\n\n# Usage\n\n```\n# not a heading\n[x](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N)\n```\n\nsee [doc](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install) and ![img](blob:sha256:aa)\n";
        let p = parse_body(body);
        let paths: Vec<&str> = p.sections.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["_preamble", "install", "install/linux", "install/notes", "install/notes-2", "usage"]);
        let install = &p.sections[1];
        let usage = &p.sections[5];
        assert_eq!(&body[install.start..install.end], "# Install\n\n## Linux\n\ntext\n\n## Notes\n\n## Notes\n\n");
        assert!(usage.end == body.len());
        assert_eq!(p.links.len(), 2);
        assert_eq!(p.links[0].uri, "kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N#install");
        assert!(p.links[1].image);
        assert!(!p.links.iter().any(|l| l.line == 15));
    }

    #[test]
    fn no_headings() {
        let p = parse_body("just text\nmore\n");
        assert_eq!(p.sections[0].path, "_body");
        assert_eq!(p.word_count, 3);
    }

    #[test]
    fn code_span_links_ignored() {
        let p = parse_body("use `[a](kb:x)` literally\n");
        assert!(p.links.is_empty());
    }
}
