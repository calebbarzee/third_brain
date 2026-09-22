//! normalize captured bytes into text plus a title, per media type. contract: design.md §9.
//! a change here changes `sha256_text` for future captures; that is drift, not an error
//! (design.md §9, "pitfalls").

use anyhow::Result;
use std::path::Path;

/// bump this when normalization for any media type changes in a way that would produce different
/// text for the same bytes. stored per capture row so drift is a finding, not silent.
pub const NORMALIZER_VERSION: u32 = 1;

pub struct Normalized {
    pub text: String,
    pub title: Option<String>,
    pub media_type: String,
}

/// normalize `bytes` captured under `media_type` (guessed from `filename`'s extension when
/// `media_type` is empty) into text plus a best-effort title.
pub fn normalize(media_type: &str, bytes: &[u8], filename: Option<&str>) -> Result<Normalized> {
    let media_type = if media_type.trim().is_empty() {
        guess_media_type(filename).unwrap_or_else(|| "application/octet-stream".to_string())
    } else {
        media_type.to_string()
    };
    match media_type.as_str() {
        "text/markdown" | "text/plain" => Ok(normalize_text(bytes, filename, media_type)),
        "application/json" => Ok(normalize_json(bytes, media_type)),
        "text/csv" => Ok(normalize_csv(bytes, filename, media_type)),
        "text/html" => Ok(normalize_html(bytes, media_type)),
        "application/pdf" => Ok(normalize_pdf(bytes, media_type)),
        other => Ok(Normalized { text: String::new(), title: None, media_type: other.to_string() }),
    }
}

/// guess a media type from a filename's extension; `None` when the extension is unknown or absent.
pub fn guess_media_type(filename: Option<&str>) -> Option<String> {
    let ext = Path::new(filename?).extension()?.to_str()?.to_ascii_lowercase();
    let mt = match ext.as_str() {
        "md" => "text/markdown",
        "txt" => "text/plain",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => return None,
    };
    Some(mt.to_string())
}

fn file_stem(filename: Option<&str>) -> Option<String> {
    Path::new(filename?).file_stem()?.to_str().map(str::to_string)
}

/// the text of the first markdown/plain heading line (`#...`), if any.
fn first_heading(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
}

fn normalize_text(bytes: &[u8], filename: Option<&str>, media_type: String) -> Normalized {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let title = first_heading(&text).or_else(|| file_stem(filename));
    Normalized { text, title, media_type }
}

fn normalize_json(bytes: &[u8], media_type: String) -> Normalized {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(value) => {
            let title = value.as_object().and_then(|o| o.get("title")).and_then(|t| t.as_str()).map(str::to_string);
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned());
            Normalized { text, title, media_type }
        }
        // not valid json: keep the media type (the capture is still what it claims to be) but
        // fall back to a lossy read rather than losing the content.
        Err(_) => Normalized { text: String::from_utf8_lossy(bytes).into_owned(), title: None, media_type },
    }
}

fn normalize_csv(bytes: &[u8], filename: Option<&str>, media_type: String) -> Normalized {
    Normalized { text: String::from_utf8_lossy(bytes).into_owned(), title: file_stem(filename), media_type }
}

fn normalize_html(bytes: &[u8], media_type: String) -> Normalized {
    let source = String::from_utf8_lossy(bytes);
    let (text, title) = strip_html(&source);
    Normalized { text, title, media_type }
}

/// crude, dependency-free html-to-text: drops tags, `<script>`/`<style>` contents, and comments;
/// decodes a handful of common entities; collapses whitespace runs. good enough for search text,
/// not a spec-compliant parser.
fn strip_html(html: &str) -> (String, Option<String>) {
    let bytes = html.as_bytes();
    let mut i = 0usize;
    let mut out = String::with_capacity(html.len());
    let mut title: Option<String> = None;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            if html[i..].starts_with("<!--") {
                match html[i + 4..].find("-->") {
                    Some(end) => i += 4 + end + 3,
                    None => break,
                }
                continue;
            }
            let tag_end = match html[i..].find('>') {
                Some(rel) => i + rel + 1,
                None => break,
            };
            let tag_body = &html[i + 1..tag_end - 1];
            let tag_name: String = tag_body.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
            let is_closing = tag_body.trim_start().starts_with('/');

            if tag_name == "title" && !is_closing {
                let close = find_ci(html, "</title", tag_end).unwrap_or(html.len());
                let inner = &html[tag_end..close.min(html.len())];
                let t = decode_entities(inner.trim());
                if !t.is_empty() {
                    title = Some(t);
                }
                i = html[close.min(html.len())..].find('>').map(|rel| close + rel + 1).unwrap_or(html.len());
                continue;
            }
            if (tag_name == "script" || tag_name == "style") && !is_closing {
                let closing_tag = format!("</{tag_name}");
                let close = find_ci(html, &closing_tag, tag_end).unwrap_or(html.len());
                i = html[close.min(html.len())..].find('>').map(|rel| close + rel + 1).unwrap_or(html.len());
                continue;
            }
            // block-level tags become a line break so words from adjacent elements do not run together.
            if matches!(tag_name.as_str(), "p" | "br" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                out.push('\n');
            }
            i = tag_end;
            continue;
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    let decoded = decode_entities(&out);
    let collapsed = collapse_whitespace(&decoded);
    (collapsed, title)
}

/// first byte offset of `needle` in `haystack[from..]`, ascii case-insensitive.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pat = needle.as_bytes();
    if from >= hay.len() || pat.is_empty() {
        return None;
    }
    (from..=hay.len().saturating_sub(pat.len())).find(|&start| hay[start..start + pat.len()].eq_ignore_ascii_case(pat))
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// collapse runs of horizontal whitespace to a single space, keep at most one blank line between
/// paragraphs, and trim each line.
fn collapse_whitespace(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in s.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(collapsed);
    }
    let mut out = String::new();
    let mut blank_run = 0;
    for line in lines {
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim_matches('\n').to_string()
}

fn normalize_pdf(bytes: &[u8], media_type: String) -> Normalized {
    match run_pdftotext(bytes) {
        Ok(text) => Normalized { text, title: None, media_type },
        Err(_) => Normalized { text: String::new(), title: Some("(no text extractor)".to_string()), media_type },
    }
}

/// shell out to `pdftotext` on a temp file; any failure (not on PATH, non-zero exit, io error)
/// is folded into a single `Err` by the caller, which falls back to empty text.
fn run_pdftotext(bytes: &[u8]) -> Result<String> {
    let dir = std::env::temp_dir();
    let name = format!("kb-normalize-{}-{}.pdf", std::process::id(), &sha256_hex_local(bytes)[..16]);
    let path = dir.join(name);
    std::fs::write(&path, bytes)?;
    let result = std::process::Command::new("pdftotext").arg(&path).arg("-").output();
    let _ = std::fs::remove_file(&path);
    let out = result?;
    if !out.status.success() {
        anyhow::bail!("pdftotext exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn sha256_hex_local(bytes: &[u8]) -> String {
    crate::parse::frontmatter::sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_title_from_heading() {
        let n = normalize("text/markdown", b"# My Title\n\nbody text\n", None).unwrap();
        assert_eq!(n.title.as_deref(), Some("My Title"));
        assert_eq!(n.text, "# My Title\n\nbody text\n");
    }

    #[test]
    fn text_title_falls_back_to_filename() {
        let n = normalize("text/plain", b"just words, no heading\n", Some("notes/my-note.txt")).unwrap();
        assert_eq!(n.title.as_deref(), Some("my-note"));
    }

    #[test]
    fn media_type_guessed_from_extension() {
        let n = normalize("", b"{\"a\": 1}", Some("data.json")).unwrap();
        assert_eq!(n.media_type, "application/json");
        assert!(n.text.contains('\n'), "pretty-printed json should be multi-line: {}", n.text);
    }

    #[test]
    fn json_pretty_prints() {
        // serde_json's default `Value` map has no `preserve_order` feature enabled, so keys come
        // back sorted rather than in source order; assert on that rather than input order.
        let n = normalize("application/json", b"{\"b\":2,\"a\":1}", None).unwrap();
        assert_eq!(n.text, "{\n  \"a\": 1,\n  \"b\": 2\n}");
    }

    #[test]
    fn invalid_json_falls_back_to_raw_text() {
        let n = normalize("application/json", b"not json", None).unwrap();
        assert_eq!(n.text, "not json");
        assert_eq!(n.media_type, "application/json");
    }

    #[test]
    fn csv_passes_through() {
        let n = normalize("text/csv", b"a,b\n1,2\n", Some("table.csv")).unwrap();
        assert_eq!(n.text, "a,b\n1,2\n");
        assert_eq!(n.title.as_deref(), Some("table"));
    }

    #[test]
    fn html_strips_tags_and_finds_title() {
        let html = b"<html><head><title>Hello &amp; World</title><style>.x{color:red}</style></head><body><p>First para.</p><p>Second <b>para</b>.</p><script>var x=1;</script></body></html>";
        let n = normalize("text/html", html, None).unwrap();
        assert_eq!(n.title.as_deref(), Some("Hello & World"));
        assert!(!n.text.contains("color:red"));
        assert!(!n.text.contains("var x"));
        assert!(n.text.contains("First para."));
        assert!(n.text.contains("Second"));
        assert!(n.text.contains("para"));
        assert!(!n.text.contains('<'));
    }

    #[test]
    fn unknown_media_type_is_empty() {
        let n = normalize("application/octet-stream", b"\x00\x01", None).unwrap();
        assert_eq!(n.text, "");
        assert_eq!(n.title, None);
    }

    #[test]
    fn pdf_without_extractor_or_with_one_never_errors() {
        // whether or not pdftotext happens to be on this machine's PATH, normalize must not fail;
        // on any failure to extract (missing binary, or a bad pdf it rejects) it falls back to
        // empty text with a note rather than propagating an error.
        let n = normalize("application/pdf", b"%PDF-1.4 not a real pdf", None).unwrap();
        assert_eq!(n.media_type, "application/pdf");
    }
}
