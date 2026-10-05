//! frontmatter split, toml parse, stable render, content hash. contract in design.md §6.
use crate::model::{Document, Frontmatter};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const DELIM: &str = "+++";

/// split file text into (frontmatter toml text or None, body text).
/// the frontmatter is the text between an opening `+++` line at the very start and the next `+++` line.
pub fn split_frontmatter(text: &str) -> Result<(Option<String>, String)> {
	let mut lines = text.split_inclusive('\n');
	let first = match lines.next() {
		Some(l) => l,
		None => return Ok((None, String::new())),
	};
	if first.trim_end_matches(['\r', '\n']) != DELIM {
		return Ok((None, text.to_string()));
	}
	let mut fm = String::new();
	let mut consumed = first.len();
	for line in lines {
		consumed += line.len();
		if line.trim_end_matches(['\r', '\n']) == DELIM {
			return Ok((Some(fm), text[consumed..].to_string()));
		}
		fm.push_str(line);
	}
	bail!("frontmatter opened with +++ but never closed")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
	let mut h = Sha256::new();
	h.update(bytes);
	hex::encode(h.finalize())
}

pub fn parse_frontmatter(toml_text: &str) -> Result<Frontmatter> {
	toml::from_str::<Frontmatter>(toml_text).context("frontmatter is not valid toml for the document schema")
}

pub fn parse_document(text: &str) -> Result<Document> {
	let (fm_text, body) = split_frontmatter(text)?;
	let frontmatter = match fm_text {
		Some(t) => parse_frontmatter(&t)?,
		None => Frontmatter::default(),
	};
	let parsed = super::body::parse_body(&body);
	Ok(Document { frontmatter, body, sections: parsed.sections, links: parsed.links, content_hash: sha256_hex(text.as_bytes()) })
}

/// render frontmatter and body back to file text with `+++` delimiters. key order follows the
/// schema (struct field order), unknown keys sorted after, tables after scalars.
pub fn render_document(fm: &Frontmatter, body: &str) -> Result<String> {
	let fm_text = toml::to_string(fm).context("frontmatter cannot be serialized")?;
	let mut out = String::from(DELIM);
	out.push('\n');
	out.push_str(&fm_text);
	if !out.ends_with('\n') {
		out.push('\n');
	}
	out.push_str(DELIM);
	out.push('\n');
	if !body.is_empty() && !body.starts_with('\n') {
		out.push('\n');
	}
	out.push_str(body);
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::model::{Layer, Status};

	const SAMPLE: &str = "+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\nlayer = \"decision\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"append-only\"\nowner = \"caleb\"\ntags = [\"a\"]\nreview_by = 2027-03-10\ncustom = 3\n\n[access]\nread = [\"*\"]\nwrite = [\"owner\"]\n\n[[refs]]\nrel = \"source\"\nuri = \"capture:01K5A7Q2XW3ZRCM8T6YB4VJH0N\"\n+++\n\n# t\n\nbody\n";

	#[test]
	fn split_and_parse() {
		let (fm, body) = split_frontmatter(SAMPLE).unwrap();
		assert!(fm.unwrap().starts_with("id = "));
		assert_eq!(body, "\n# t\n\nbody\n");
		let doc = parse_document(SAMPLE).unwrap();
		assert_eq!(doc.frontmatter.layer, Some(Layer::Decision));
		assert_eq!(doc.frontmatter.status, Some(Status::Canon));
		assert_eq!(doc.frontmatter.extra.get("custom").and_then(|v| v.as_integer()), Some(3));
		assert_eq!(doc.frontmatter.refs.len(), 1);
		assert_eq!(doc.sections.len(), 1);
	}

	#[test]
	fn no_frontmatter() {
		let doc = parse_document("# hello\n\ntext\n").unwrap();
		assert!(doc.frontmatter.id.is_none());
		assert_eq!(doc.body, "# hello\n\ntext\n");
	}

	#[test]
	fn unclosed_frontmatter_errors() {
		assert!(parse_document("+++\nid = \"x\"\n").is_err());
	}

	#[test]
	fn render_roundtrip_is_stable() {
		let doc = parse_document(SAMPLE).unwrap();
		let out = render_document(&doc.frontmatter, &doc.body).unwrap();
		let again = parse_document(&out).unwrap();
		assert_eq!(again.frontmatter, doc.frontmatter);
		assert_eq!(again.body, doc.body);
		let out2 = render_document(&again.frontmatter, &again.body).unwrap();
		assert_eq!(out, out2);
		let id_pos = out.find("id = ").unwrap();
		let access_pos = out.find("[access]").unwrap();
		let refs_pos = out.find("[[refs]]").unwrap();
		assert!(id_pos < access_pos && access_pos < refs_pos);
	}
}
