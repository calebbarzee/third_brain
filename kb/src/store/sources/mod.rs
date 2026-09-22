//! source adapters: pull external content into the capture catalog. contract: design.md §9, §19
//! (the `capture` hook). `kb.toml [sources.<name>]` configures one adapter per table entry.

use super::catalog::{self, PutCapture};
use super::normalize::normalize;
use crate::config::Config;
use crate::git;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use std::path::PathBuf;

/// one pull's result: the items to hand to `catalog::put_capture`, and the cursor to persist for
/// next time (`None` leaves the cursor untouched, e.g. an empty pull).
pub struct Pull {
    pub items: Vec<PutCapture>,
    pub next_cursor: Option<String>,
}

pub trait Source {
    fn name(&self) -> &str;
    /// pull everything new since `since_cursor` (`None` on the very first pull).
    fn pull(&self, cfg: &Config, since_cursor: Option<&str>) -> Result<Pull>;
}

/// `kind = "files"`: a directory, matched against a glob, cursor = the newest mtime seen (rfc3339).
pub struct FileDropSource {
    name: String,
    path: PathBuf,
    glob: String,
}

impl Source for FileDropSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn pull(&self, _cfg: &Config, since_cursor: Option<&str>) -> Result<Pull> {
        let since: Option<DateTime<Utc>> = since_cursor.and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
        let mut items = Vec::new();
        let mut newest = since;
        if self.path.is_dir() {
            for entry in walkdir::WalkDir::new(&self.path).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry.path().strip_prefix(&self.path).unwrap_or(entry.path());
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if !glob_match(&self.glob, &rel_str) {
                    continue;
                }
                let meta = entry.metadata().with_context(|| format!("stat {}", entry.path().display()))?;
                let mtime: DateTime<Utc> = meta.modified()?.into();
                if let Some(s) = since
                    && mtime <= s
                {
                    continue;
                }
                let bytes = std::fs::read(entry.path()).with_context(|| format!("reading {}", entry.path().display()))?;
                let filename = entry.path().file_name().and_then(|n| n.to_str());
                let normalized = normalize("", &bytes, filename)?;
                let abs = std::fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path().to_path_buf());
                let cursor = mtime.to_rfc3339();
                items.push(PutCapture {
                    source: self.name.clone(),
                    source_uri: format!("file:{}", abs.display()),
                    source_cursor: Some(cursor.clone()),
                    media_type: normalized.media_type,
                    title: normalized.title,
                    original: bytes,
                    text: normalized.text,
                    captured_at: Some(cursor),
                });
                newest = Some(newest.map_or(mtime, |n| n.max(mtime)));
            }
        }
        items.sort_by(|a, b| a.source_uri.cmp(&b.source_uri));
        Ok(Pull { items, next_cursor: newest.map(|d| d.to_rfc3339()) })
    }
}

/// `kind = "git"`: one capture per commit on `branch` since the cursor sha, message plus
/// `git show --stat` as text.
pub struct GitLogSource {
    name: String,
    repo: PathBuf,
    branch: String,
}

impl Source for GitLogSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn pull(&self, _cfg: &Config, since_cursor: Option<&str>) -> Result<Pull> {
        let range = match since_cursor {
            Some(sha) => format!("{sha}..{}", self.branch),
            None => self.branch.clone(),
        };
        let out = git::run(&self.repo, &["rev-list", "--reverse", &range])?;
        let shas: Vec<String> = out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        let mut items = Vec::new();
        let mut last = since_cursor.map(str::to_string);
        for sha in &shas {
            let message = git::commit_message(&self.repo, sha)?;
            let stat = git::run(&self.repo, &["show", "--stat", sha])?;
            let captured_at = git::committer_date(&self.repo, sha)?;
            let text = format!("{message}\n\n{stat}");
            let title = message.lines().next().map(str::to_string);
            items.push(PutCapture {
                source: self.name.clone(),
                source_uri: format!("git:{}@{sha}", self.repo.display()),
                source_cursor: Some(sha.clone()),
                media_type: "text/plain".to_string(),
                title,
                original: text.clone().into_bytes(),
                text,
                captured_at: Some(captured_at),
            });
            last = Some(sha.clone());
        }
        Ok(Pull { items, next_cursor: last })
    }
}

/// build one adapter per `[sources.<name>]` table in `kb.toml`.
pub fn sources_from_config(cfg: &Config) -> Result<Vec<Box<dyn Source>>> {
    let mut out: Vec<Box<dyn Source>> = Vec::new();
    for (name, value) in &cfg.kb.sources {
        let table = value.as_table().with_context(|| format!("kb.toml [sources.{name}] must be a table"))?;
        let kind = table.get("kind").and_then(|v| v.as_str()).with_context(|| format!("kb.toml [sources.{name}] is missing `kind`"))?;
        match kind {
            "files" => {
                let path = table.get("path").and_then(|v| v.as_str()).with_context(|| format!("[sources.{name}] is missing `path`"))?;
                let glob = table.get("glob").and_then(|v| v.as_str()).unwrap_or("**/*").to_string();
                out.push(Box::new(FileDropSource { name: name.clone(), path: cfg.root.join(path), glob }));
            }
            "git" => {
                let repo = table.get("repo").and_then(|v| v.as_str()).with_context(|| format!("[sources.{name}] is missing `repo`"))?;
                let branch = table.get("branch").and_then(|v| v.as_str()).unwrap_or("main").to_string();
                out.push(Box::new(GitLogSource { name: name.clone(), repo: cfg.root.join(repo), branch }));
            }
            other => bail!("kb.toml [sources.{name}]: unknown kind `{other}`"),
        }
    }
    Ok(out)
}

pub struct CaptureReport {
    /// (source name, count of newly stored, non-quarantined captures).
    pub per_source: Vec<(String, usize)>,
    pub quarantined: usize,
}

/// pull every configured source (or just `only`, when given) from its saved cursor, store what
/// comes back, and advance each source's cursor.
pub fn capture_all(cfg: &Config, only: Option<&str>) -> Result<CaptureReport> {
    let sources = sources_from_config(cfg)?;
    let mut per_source = Vec::new();
    let mut quarantined = 0usize;
    for src in sources {
        if let Some(only) = only
            && src.name() != only
        {
            continue;
        }
        let cursor = catalog::get_cursor(cfg, src.name())?;
        let pull = src.pull(cfg, cursor.as_deref())?;
        let mut new_count = 0usize;
        for item in pull.items {
            let cap = catalog::put_capture(cfg, item)?;
            if cap.status == "quarantined" {
                quarantined += 1;
            } else {
                new_count += 1;
            }
        }
        if let Some(next) = pull.next_cursor {
            catalog::set_cursor(cfg, src.name(), &next)?;
        }
        per_source.push((src.name().to_string(), new_count));
    }
    Ok(CaptureReport { per_source, quarantined })
}

/// a glob supporting `*` (any run excluding `/`) and `**` (any run including `/`), same shape as
/// `check::secrets`'s `allow_paths` matcher.
fn glob_match(pattern: &str, path: &str) -> bool {
    match glob_regex(pattern) {
        Ok(re) => re.is_match(path),
        Err(_) => false,
    }
}

fn glob_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let mut out = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        // `**/` matches zero or more whole path segments, including none at all,
                        // so `**/*` also matches a file with no leading directory.
                        out.push_str("(?:.*/)?");
                    } else {
                        out.push_str(".*");
                    }
                } else {
                    out.push_str("[^/]*");
                }
            }
            '\\' | '.' | '+' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    Regex::new(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star_star() {
        assert!(glob_match("**/*", "a/b/c.md"));
        assert!(glob_match("**/*", "c.md"));
        assert!(glob_match("*.md", "a.md"));
        assert!(!glob_match("*.md", "a/b.md"));
    }

    #[test]
    fn file_drop_pulls_new_files_and_advances_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let src_dir = dir.path().join("drop");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.md"), "# A\n\nbody\n").unwrap();

        let src = FileDropSource { name: "files".into(), path: src_dir.clone(), glob: "**/*".into() };
        let pull = src.pull(&cfg, None).unwrap();
        assert_eq!(pull.items.len(), 1);
        assert!(pull.next_cursor.is_some());
        assert_eq!(pull.items[0].title.as_deref(), Some("A"));

        // a second pull from the advanced cursor sees nothing new.
        let pull2 = src.pull(&cfg, pull.next_cursor.as_deref()).unwrap();
        assert!(pull2.items.is_empty());
    }

    #[test]
    fn sources_from_config_builds_files_adapter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kb.toml"), "[sources.drop]\nkind = \"files\"\npath = \"inbox\"\nglob = \"**/*\"\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        let sources = sources_from_config(&cfg).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name(), "drop");
    }

    #[test]
    fn sources_from_config_rejects_unknown_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kb.toml"), "[sources.x]\nkind = \"bogus\"\n").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(sources_from_config(&cfg).is_err());
    }
}
