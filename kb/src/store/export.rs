//! `kb archive export`: write a capture's normalized text (with a generated header) or raw bytes
//! to a file. contract: design.md §9.

use super::blob;
use super::catalog::{self, Capture};
use crate::config::Config;
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

/// export capture `id` (exact id or unambiguous prefix) to `out`, or a default path under
/// `<state>/export/` when `out` is `None`. `original` writes the raw captured bytes; otherwise the
/// normalized text, prefixed with a generated `+++`-delimited toml header (no kb frontmatter:
/// this is not a document). returns the path written.
pub fn export(cfg: &Config, id: &str, original: bool, out: Option<&Path>) -> Result<PathBuf> {
    let capture = catalog::get(cfg, id)?.ok_or_else(|| anyhow!("no capture matching `{id}`"))?;
    let path = out.map(Path::to_path_buf).unwrap_or_else(|| default_export_path(cfg, &capture, original));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if original {
        let bytes = read_original(cfg, &capture)?;
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    } else {
        let text = catalog::text(cfg, &capture)?;
        let body = format!("{}{text}", header(&capture));
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

fn read_original(cfg: &Config, capture: &Capture) -> Result<Vec<u8>> {
    if capture.status == "quarantined" {
        return blob::read_quarantine(cfg, &capture.sha256_original)?
            .ok_or_else(|| anyhow!("missing quarantined original blob {}", capture.sha256_original));
    }
    blob::open_store(cfg)?.get(&capture.sha256_original)?.ok_or_else(|| anyhow!("missing original blob {}", capture.sha256_original))
}

/// a generated `+++`-delimited toml header carrying provenance and hashes. deliberately not a kb
/// document frontmatter (no `id`/`layer`/`status`/etc.): `kb::parse::parse_document` still parses
/// it, since unrecognized keys land in `Frontmatter::extra`.
fn header(capture: &Capture) -> String {
    let mut out = String::from("+++\n");
    out.push_str(&format!("source_id = {}\n", toml_string(&capture.capture_id)));
    out.push_str(&format!("source = {}\n", toml_string(&capture.source)));
    out.push_str(&format!("source_uri = {}\n", toml_string(&capture.source_uri)));
    out.push_str(&format!("captured_at = {}\n", toml_string(&capture.captured_at)));
    out.push_str(&format!("media_type = {}\n", toml_string(&capture.media_type)));
    out.push_str(&format!("sha256_original = {}\n", toml_string(&capture.sha256_original)));
    out.push_str(&format!("sha256_text = {}\n", toml_string(&capture.sha256_text)));
    if let Some(title) = &capture.title {
        out.push_str(&format!("title = {}\n", toml_string(title)));
    }
    out.push_str("+++\n\n");
    out
}

/// a toml basic string literal: quote, and escape backslashes, quotes, and control characters.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn default_export_path(cfg: &Config, capture: &Capture, original: bool) -> PathBuf {
    let dir = cfg.state_dir().join("export");
    if original {
        dir.join(format!("{}.orig", capture.capture_id))
    } else {
        dir.join(format!("{}.md", capture.capture_id))
    }
}

/// used by `kb archive quarantine --release`: move a quarantined capture's blobs into the normal
/// blob store and mark it `new`. `id` must resolve to a quarantined capture.
pub fn release_quarantine(cfg: &Config, id: &str) -> Result<Capture> {
    let capture = catalog::get(cfg, id)?.ok_or_else(|| anyhow!("no capture matching `{id}`"))?;
    if capture.status != "quarantined" {
        bail!("capture {} is not quarantined (status: {})", capture.capture_id, capture.status);
    }
    let original = blob::read_quarantine(cfg, &capture.sha256_original)?
        .ok_or_else(|| anyhow!("missing quarantined original blob {}", capture.sha256_original))?;
    let text = blob::read_quarantine(cfg, &capture.sha256_text)?.ok_or_else(|| anyhow!("missing quarantined text blob {}", capture.sha256_text))?;
    let store = blob::open_store(cfg)?;
    store.put(&original, &capture.media_type)?;
    store.put(&text, "text/plain")?;
    blob::remove_quarantine(cfg, &capture.sha256_original)?;
    blob::remove_quarantine(cfg, &capture.sha256_text)?;
    catalog::set_status(cfg, &capture.capture_id, "new", None)?;
    Ok(Capture { status: "new".to_string(), ..capture })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::catalog::PutCapture;

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        (dir, cfg)
    }

    #[test]
    fn export_text_writes_parseable_header() {
        let (dir, cfg) = cfg();
        let cap = catalog::put_capture(
            &cfg,
            PutCapture {
                source: "manual".into(),
                source_uri: "file:/notes/a.md".into(),
                source_cursor: None,
                media_type: "text/markdown".into(),
                title: Some("A".into()),
                original: b"# A\n\nbody\n".to_vec(),
                text: "# A\n\nbody\n".into(),
                captured_at: None,
            },
        )
        .unwrap();

        let out = dir.path().join("out.md");
        let path = export(&cfg, &cap.capture_id, false, Some(&out)).unwrap();
        assert_eq!(path, out);
        let written = std::fs::read_to_string(&path).unwrap();
        let doc = crate::parse::parse_document(&written).unwrap();
        assert_eq!(doc.frontmatter.extra.get("source_id").and_then(|v| v.as_str()), Some(cap.capture_id.as_str()));
        assert_eq!(doc.frontmatter.extra.get("source_uri").and_then(|v| v.as_str()), Some("file:/notes/a.md"));
        assert!(doc.body.contains("body"));
    }

    #[test]
    fn export_original_writes_raw_bytes() {
        let (dir, cfg) = cfg();
        let cap = catalog::put_capture(
            &cfg,
            PutCapture {
                source: "manual".into(),
                source_uri: "file:/notes/b.md".into(),
                source_cursor: None,
                media_type: "text/markdown".into(),
                title: None,
                original: b"raw bytes here".to_vec(),
                text: "raw bytes here".into(),
                captured_at: None,
            },
        )
        .unwrap();
        let out = dir.path().join("out.bin");
        export(&cfg, &cap.capture_id, true, Some(&out)).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"raw bytes here");
    }

    #[test]
    fn release_quarantine_moves_blob_and_updates_status() {
        let (_dir, cfg) = cfg();
        let token = format!("ghp_{}", "a".repeat(36));
        let cap = catalog::put_capture(
            &cfg,
            PutCapture {
                source: "manual".into(),
                source_uri: "file:/secret.txt".into(),
                source_cursor: None,
                media_type: "text/plain".into(),
                title: None,
                original: token.as_bytes().to_vec(),
                text: token.clone(),
                captured_at: None,
            },
        )
        .unwrap();
        assert_eq!(cap.status, "quarantined");
        let released = release_quarantine(&cfg, &cap.capture_id).unwrap();
        assert_eq!(released.status, "new");
        assert!(blob::open_store(&cfg).unwrap().exists(&cap.sha256_text).unwrap());
        assert_eq!(blob::read_quarantine(&cfg, &cap.sha256_text).unwrap(), None);
    }
}
