//! content-addressed blob storage. contract: design.md §9.
//!
//! a blob lives at `<store>/blobs/<aa>/<sha256>`, where `<aa>` is the first two hex characters of
//! its own hash; a sidecar `<sha256>.type` file next to it records the media type it was put with.
//! writes go to a temp file in the same shard directory and are renamed into place, so a reader
//! never observes a partial file; an existing blob is never overwritten, since the same hash means
//! the same bytes.

use crate::config::Config;
use crate::parse::frontmatter::sha256_hex;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub trait BlobStore: Send + Sync {
    /// write `bytes` under their sha256 hex digest, recording `media_type` in a sidecar file.
    /// returns the digest. a second `put` of the same bytes is a no-op that returns the same hash.
    fn put(&self, bytes: &[u8], media_type: &str) -> Result<String>;
    fn get(&self, sha: &str) -> Result<Option<Vec<u8>>>;
    fn exists(&self, sha: &str) -> Result<bool>;
    /// filesystem path of the blob, when this backend has one and the blob is present.
    fn path(&self, sha: &str) -> Option<PathBuf>;
}

pub struct LocalBlobStore {
    /// `<store>/blobs`.
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn shard_dir(&self, sha: &str) -> PathBuf {
        self.root.join(&sha[..2.min(sha.len())])
    }

    fn blob_path(&self, sha: &str) -> PathBuf {
        self.shard_dir(sha).join(sha)
    }

    fn type_path(&self, sha: &str) -> PathBuf {
        self.shard_dir(sha).join(format!("{sha}.type"))
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, bytes: &[u8], media_type: &str) -> Result<String> {
        let sha = sha256_hex(bytes);
        let dir = self.shard_dir(&sha);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let target = self.blob_path(&sha);
        if !target.is_file() {
            let tmp = dir.join(format!(".{sha}.{}.tmp", std::process::id()));
            {
                let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
                f.write_all(bytes)?;
                f.sync_all()?;
            }
            // never overwrite an existing blob: same hash means same bytes, so losing this race
            // to a concurrent writer is fine, and the loser's temp file is simply discarded.
            match fs::rename(&tmp, &target) {
                Ok(()) => {}
                Err(e) if target.is_file() => {
                    let _ = fs::remove_file(&tmp);
                    let _ = e;
                }
                Err(e) => return Err(e).with_context(|| format!("renaming into {}", target.display())),
            }
        }
        // the sidecar records the media type this put was made with; harmless to rewrite when a
        // second put of identical bytes disagrees, since both describe the same content.
        fs::write(self.type_path(&sha), media_type).with_context(|| format!("writing {}.type", sha))?;
        Ok(sha)
    }

    fn get(&self, sha: &str) -> Result<Option<Vec<u8>>> {
        if !is_sha256_hex(sha) {
            return Ok(None);
        }
        let p = self.blob_path(sha);
        if !p.is_file() {
            return Ok(None);
        }
        Ok(Some(fs::read(&p).with_context(|| format!("reading {}", p.display()))?))
    }

    fn exists(&self, sha: &str) -> Result<bool> {
        Ok(is_sha256_hex(sha) && self.blob_path(sha).is_file())
    }

    fn path(&self, sha: &str) -> Option<PathBuf> {
        if !is_sha256_hex(sha) {
            return None;
        }
        let p = self.blob_path(sha);
        p.is_file().then_some(p)
    }
}

/// a blob address: exactly 64 lowercase hex characters. anything else (a `/`, `..`, a multibyte
/// character) never reaches a path join or a byte slice.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `<repo root>/<cfg.kb.store.path>`, the root of the content store (blobs/, quarantine/ live under it).
pub fn store_root(cfg: &Config) -> PathBuf {
    cfg.root.join(&cfg.kb.store.path)
}

/// open the configured blob store backend. only `local` is implemented in this build.
pub fn open_store(cfg: &Config) -> Result<Box<dyn BlobStore>> {
    match cfg.kb.store.backend.as_str() {
        "local" => Ok(Box::new(LocalBlobStore::new(store_root(cfg).join("blobs")))),
        "object" => bail!("object store backend not implemented in this build"),
        other => bail!("unknown store backend `{other}`"),
    }
}

/// path of a quarantined blob: `<store>/quarantine/<sha256>`, flat (no shard directory), since a
/// quarantined item is reviewed and released or discarded by hand rather than addressed at scale.
pub fn quarantine_path(cfg: &Config, sha: &str) -> PathBuf {
    // a non-hash name maps to a fixed path that never holds a file
    let name = if is_sha256_hex(sha) { sha } else { "invalid" };
    store_root(cfg).join("quarantine").join(name)
}

/// write `bytes` into quarantine under their own hash. does not check the hash against `sha`;
/// callers pass the hash they already computed for the content.
pub fn write_quarantine(cfg: &Config, sha: &str, bytes: &[u8]) -> Result<()> {
    let path = quarantine_path(cfg, sha);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// read a quarantined blob's bytes, if present.
pub fn read_quarantine(cfg: &Config, sha: &str) -> Result<Option<Vec<u8>>> {
    let path = quarantine_path(cfg, sha);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(fs::read(&path).with_context(|| format!("reading {}", path.display()))?))
}

/// remove a quarantined blob, e.g. after it has been released into the normal blob store.
pub fn remove_quarantine(cfg: &Config, sha: &str) -> Result<()> {
    let path = quarantine_path(cfg, sha);
    if path.is_file() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// the recorded media type for a blob, if the sidecar file is present.
pub fn media_type_of(root: &Path, sha: &str) -> Option<String> {
    if !is_sha256_hex(sha) {
        return None;
    }
    let shard = &sha[..2.min(sha.len())];
    fs::read_to_string(root.join(shard).join(format!("{sha}.type"))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_hash_addresses_never_reach_the_filesystem() {
        let (dir, cfg) = cfg();
        let store = open_store(&cfg).unwrap();
        std::fs::create_dir_all(store_root(&cfg).join("blobs")).unwrap();
        std::fs::write(dir.path().join("kb.toml.secret"), "x").unwrap();
        for bad in ["/etc/hostname", "../state.sqlite", "..%2F..%2Fkb.toml", "\u{20ac}abc", "€abc", "ABCDEF", &"a".repeat(63)] {
            assert!(store.get(bad).unwrap().is_none(), "{bad}");
            assert!(!store.exists(bad).unwrap(), "{bad}");
            assert!(store.path(bad).is_none(), "{bad}");
            assert!(media_type_of(&store_root(&cfg).join("blobs"), bad).is_none(), "{bad}");
        }
        let sha = store.put(b"hello", "text/plain").unwrap();
        assert_eq!(store.get(&sha).unwrap().as_deref(), Some(&b"hello"[..]));
    }

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        (dir, cfg)
    }

    #[test]
    fn put_get_and_dedup() {
        let (_dir, cfg) = cfg();
        let store = open_store(&cfg).unwrap();
        let sha1 = store.put(b"hello world", "text/plain").unwrap();
        assert_eq!(sha1, sha256_hex(b"hello world"));
        assert!(store.exists(&sha1).unwrap());
        assert_eq!(store.get(&sha1).unwrap().unwrap(), b"hello world");

        // same bytes twice: same hash, no error, no duplicate content.
        let sha2 = store.put(b"hello world", "text/plain").unwrap();
        assert_eq!(sha1, sha2);

        let path = store.path(&sha1).unwrap();
        assert!(path.starts_with(store_root(&cfg).join("blobs").join(&sha1[..2])));
        assert!(path.is_file());
    }

    #[test]
    fn missing_blob_is_none() {
        let (_dir, cfg) = cfg();
        let store = open_store(&cfg).unwrap();
        assert_eq!(store.get("deadbeef").unwrap(), None);
        assert!(!store.exists("deadbeef").unwrap());
        assert!(store.path("deadbeef").is_none());
    }

    #[test]
    fn never_overwrites_existing_blob() {
        let (_dir, cfg) = cfg();
        let store = open_store(&cfg).unwrap();
        let sha = store.put(b"content", "text/plain").unwrap();
        let path = store.path(&sha).unwrap();
        let before = fs::metadata(&path).unwrap().len();
        store.put(b"content", "text/plain").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), before);
    }

    #[test]
    fn object_backend_not_implemented() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kb.toml"), "[store]\nbackend = \"object\"\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(open_store(&cfg).is_err());
    }

    #[test]
    fn quarantine_write_read_remove() {
        let (_dir, cfg) = cfg();
        write_quarantine(&cfg, "abc123", b"secret stuff").unwrap();
        assert_eq!(read_quarantine(&cfg, "abc123").unwrap().unwrap(), b"secret stuff");
        remove_quarantine(&cfg, "abc123").unwrap();
        assert_eq!(read_quarantine(&cfg, "abc123").unwrap(), None);
    }
}
