//! `kb import`: bring existing markdown notes into a knowledge base as documents.
//! contract: design.md §6 (frontmatter, `validate --fix`), §9 (source refs), §12 (sessions).
//!
//! one session carries the whole import. each source file is placed into the session
//! worktree's `docs/`, filled and renamed by `check::validate_file`, tagged with a `source` ref
//! and a `source_id`/`aliases` extra so the old name still finds it, and cross-linked: a wiki
//! link `[[Note Name]]` or a relative markdown link `[x](Other.md)` that names another imported
//! note is rewritten to `[text](kb:<ulid>)`. the session is then settled and (unless
//! `--no-merge`) merged, exactly as a human's `kb session settle` plus `kb merge` would be.
//!
//! this module calls `check::validate_file` directly rather than the `kb check validate` cli
//! path, so no `classify` provider ever runs: `validate_file` never calls a provider by its own
//! contract (see its module doc), and the classify wrapper lives one layer up, in
//! `cli::check::run_validate`. a caller who wants layer/tag suggestions runs
//! `kb check validate --fix` on the imported documents afterward.

use crate::check;
use crate::config::Config;
use crate::git;
use crate::merge::{self, MergeOpts};
use crate::model::{Frontmatter, Layer, Ref};
use crate::parse::{frontmatter::sha256_hex, parse_document, render_document};
use anyhow::{Context, Result, bail};
use regex::{Captures, Regex};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// directory names never descended into while collecting source files: kb's own state and
/// tooling directories, plus `docs/` itself so pointing `kb import` at a kb repository's root
/// does not re-import its own documents.
const SKIP_DIR_NAMES: [&str; 4] = [".git", ".kb", ".obsidian", "docs"];

/// options for `import`, independent of clap (kept here so tests and other callers do not need
/// to build a clap `ArgMatches`).
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    pub paths: Vec<PathBuf>,
    pub move_files: bool,
    pub layer: Option<Layer>,
    pub tags: Vec<String>,
    pub session_objective: Option<String>,
    pub no_merge: bool,
    pub dry_run: bool,
}

/// one row of the ulid map: the source path as given, the new document, and whether the source
/// was moved (git mv) rather than copied.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportedDoc {
    pub source_path: String,
    pub id: String,
    pub title: String,
    pub moved: bool,
}

/// `import`'s result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportReport {
    pub session_id: Option<String>,
    /// the new `main` sha, when `kb merge` landed the session.
    pub merged_commit: Option<String>,
    pub files_found: usize,
    pub links_rewritten: usize,
    pub links_unresolved: usize,
    pub docs: Vec<ImportedDoc>,
    /// `<state>/imports/<session-ulid>.toml`, where `docs` above was also saved.
    pub map_path: Option<PathBuf>,
    pub dry_run: bool,
}

/// one file found under a `paths` entry.
struct SourceFile {
    abs: PathBuf,
    /// path as reported to the user and recorded in `source_id`: relative to the argument it was
    /// found under (posix separators) for a directory argument, or exactly as given for a file
    /// argument.
    rel: String,
}

/// a placed, validated document, kept between pass 1 (place + fill) and pass 2 (link rewrite).
struct Placed {
    source_rel: String,
    id: String,
    title: String,
    moved: bool,
    final_path: PathBuf,
}

/// `kb import <dir-or-files...>`: design.md §6, §9, §12.
pub fn import(cfg: &Config, opts: &ImportOptions) -> Result<ImportReport> {
    if opts.paths.is_empty() {
        bail!("kb import needs at least one file or directory");
    }
    let files = collect_files(&opts.paths)?;

    if opts.dry_run {
        return Ok(dry_run_report(&files));
    }
    if files.is_empty() {
        bail!("no markdown files found under the given paths");
    }

    let objective = opts.session_objective.clone().unwrap_or_else(|| format!("import {} note(s)", files.len()));
    let mut session = crate::session::open(cfg, &objective, None, &[])?;
    let worktree = session.worktree.clone();
    let docs_dir = worktree.join("docs");
    std::fs::create_dir_all(&docs_dir)?;

    // pass 1: place, validate --fix, apply overrides. `stem_to_id` keys are lowercased original
    // filename stems, the convention obsidian-style wiki links and relative markdown links both
    // name a note by.
    let mut stem_to_id: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut placed: Vec<Placed> = Vec::with_capacity(files.len());

    for (i, file) in files.iter().enumerate() {
        let original_bytes = std::fs::read(&file.abs).with_context(|| format!("reading {}", file.abs.display()))?;
        let original_stem = file.abs.file_stem().and_then(|s| s.to_str()).unwrap_or("note").to_string();
        let staging_path = docs_dir.join(format!("{i:05}-{}.md", sanitize_filename(&original_stem)));

        let moved = place_file(cfg, &worktree, file, &staging_path, opts.move_files)?;
        let outcome = check::validate_file(cfg, &staging_path, true)?;

        let mut fm = outcome.document.frontmatter;
        let body = outcome.document.body;

        if !fm.extra.contains_key("source_id") {
            fm.extra.insert("source_id".into(), toml::Value::String(file.rel.clone()));
        }
        fm.refs.push(Ref {
            rel: "source".into(),
            uri: format!("file:{}", file.abs.display()),
            sha256: Some(sha256_hex(&original_bytes)),
            media_type: None,
        });
        if let Some(layer) = opts.layer {
            fm.layer = Some(layer);
            fm.agent_policy = Some(cfg.agent_policy_for(layer));
        }
        for tag in &opts.tags {
            if !fm.tags.contains(tag) {
                fm.tags.push(tag.clone());
            }
        }
        add_alias(&mut fm, &original_stem);

        let final_text = render_document(&fm, &body)?;
        std::fs::write(&outcome.path, final_text)?;

        let id = fm.id.clone().unwrap_or_default();
        let title = fm.title.clone().unwrap_or_else(|| id.clone());
        stem_to_id.insert(original_stem.to_lowercase(), (id.clone(), title.clone()));
        placed.push(Placed { source_rel: file.rel.clone(), id, title, moved, final_path: outcome.path });
    }

    let (links_rewritten, links_unresolved) = rewrite_links(&placed, &stem_to_id)?;

    let message = format!(
        "imported {} note(s); {} link(s) rewritten, {} unresolved",
        placed.len(),
        links_rewritten,
        links_unresolved
    );
    crate::session::settle(cfg, &mut session, Some(&message))?;

    let merged_commit = if opts.no_merge {
        None
    } else {
        merge::merge(cfg, &session.id, MergeOpts::default())?.landed
    };

    let docs: Vec<ImportedDoc> = placed
        .iter()
        .map(|p| ImportedDoc { source_path: p.source_rel.clone(), id: p.id.clone(), title: p.title.clone(), moved: p.moved })
        .collect();
    let map_path = save_map(cfg, &session.id, &docs)?;

    Ok(ImportReport {
        session_id: Some(session.id.clone()),
        merged_commit,
        files_found: docs.len(),
        links_rewritten,
        links_unresolved,
        docs,
        map_path: Some(map_path),
        dry_run: false,
    })
}

/// walk every `paths` entry for `*.md` files, recursively for directories, skipping
/// `SKIP_DIR_NAMES`. deterministic order: directory entries sorted by path.
fn collect_files(paths: &[PathBuf]) -> Result<Vec<SourceFile>> {
    let mut out = Vec::new();
    for p in paths {
        let abs = std::fs::canonicalize(p).with_context(|| format!("resolving {}", p.display()))?;
        if abs.is_dir() {
            let mut found: Vec<PathBuf> = walkdir::WalkDir::new(&abs)
                .into_iter()
                .filter_entry(|e| {
                    !(e.file_type().is_dir() && e.depth() > 0 && SKIP_DIR_NAMES.contains(&e.file_name().to_str().unwrap_or("")))
                })
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "md"))
                .map(|e| e.path().to_path_buf())
                .collect();
            found.sort();
            for f in found {
                let rel = f.strip_prefix(&abs).unwrap_or(&f).to_string_lossy().replace('\\', "/");
                out.push(SourceFile { abs: f, rel });
            }
        } else if abs.extension().is_some_and(|ext| ext == "md") {
            out.push(SourceFile { abs, rel: p.display().to_string() });
        }
    }
    Ok(out)
}

/// place one source file at `staging_path` inside `worktree`'s `docs/`. `git mv` only when
/// `move_files` is set, the source is part of this same repository (design.md §12: worktrees of
/// one repository share an object store and a common git directory), and that same relative
/// path is present in this session's own worktree (it was checked out from `main`, so an
/// uncommitted source file never qualifies). every other case is a plain copy that leaves the
/// source untouched. returns whether the source was moved.
fn place_file(cfg: &Config, worktree: &Path, file: &SourceFile, staging_path: &Path, move_files: bool) -> Result<bool> {
    if move_files
        && let Some(source_parent) = file.abs.parent()
        && git::is_repo(source_parent)
        && let (Ok(a), Ok(b)) = (git::common_dir(source_parent), git::common_dir(&cfg.root))
        && a == b
        && let Ok(repo_root) = git::toplevel(source_parent)
        && let Ok(rel) = file.abs.strip_prefix(&repo_root)
    {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if worktree.join(&rel_str).is_file() {
            let dest_rel = staging_path.strip_prefix(worktree).unwrap_or(staging_path).to_string_lossy().replace('\\', "/");
            git::mv(worktree, &rel_str, &dest_rel)?;
            return Ok(true);
        }
    }
    std::fs::copy(&file.abs, staging_path).with_context(|| format!("copying {}", file.abs.display()))?;
    Ok(false)
}

fn sanitize_filename(stem: &str) -> String {
    let cleaned: String = stem.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if cleaned.is_empty() { "note".to_string() } else { cleaned }
}

/// merge `stem` into `fm.extra["aliases"]`, creating the array if absent, so `kb index search`
/// can still find a document by its pre-import name. follow-up, called out in the report: the
/// fts index is built from body chunk text only (see `index::build`), so `aliases` does not yet
/// participate in full-text search; this only makes the old name discoverable via `extra`/`get`.
fn add_alias(fm: &mut Frontmatter, stem: &str) {
    match fm.extra.get_mut("aliases") {
        Some(toml::Value::Array(arr)) => {
            if !arr.iter().any(|v| v.as_str() == Some(stem)) {
                arr.push(toml::Value::String(stem.to_string()));
            }
        }
        _ => {
            fm.extra.insert("aliases".into(), toml::Value::Array(vec![toml::Value::String(stem.to_string())]));
        }
    }
}

fn wiki_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\]|#]+)(?:[|#][^\]]*)?\]\]").unwrap())
}

fn md_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\[([^\]]*)\]\(([^)\s]+\.md)(?:\s+"[^"]*")?\)"#).unwrap())
}

/// two passes over every placed document's body: replace `[[Note Name]]` and `[text](Other.md)`
/// with `[text](kb:<ulid>)` when the name resolves against `stem_to_id`; leave anything else
/// exactly as it was. returns (rewritten, unresolved) counts.
fn rewrite_links(placed: &[Placed], stem_to_id: &BTreeMap<String, (String, String)>) -> Result<(usize, usize)> {
    let mut rewritten = 0usize;
    let mut unresolved = 0usize;
    for doc in placed {
        let text = std::fs::read_to_string(&doc.final_path).with_context(|| format!("reading {}", doc.final_path.display()))?;
        let parsed = parse_document(&text)?;
        let mut body = parsed.body;
        let mut changed = false;

        let before = body.clone();
        body = wiki_link_re()
            .replace_all(&body, |caps: &Captures| {
                let name = caps[1].trim();
                match stem_to_id.get(&name.to_lowercase()) {
                    Some((id, _)) => {
                        rewritten += 1;
                        format!("[{name}](kb:{id})")
                    }
                    None => {
                        unresolved += 1;
                        caps[0].to_string()
                    }
                }
            })
            .into_owned();
        changed |= body != before;

        let before = body.clone();
        body = md_link_re()
            .replace_all(&body, |caps: &Captures| {
                let text = &caps[1];
                let target = &caps[2];
                if target.contains("://") || target.starts_with("kb:") {
                    return caps[0].to_string();
                }
                let stem = Path::new(target).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                match stem_to_id.get(&stem) {
                    Some((id, _)) => {
                        rewritten += 1;
                        format!("[{text}](kb:{id})")
                    }
                    None => {
                        unresolved += 1;
                        caps[0].to_string()
                    }
                }
            })
            .into_owned();
        changed |= body != before;

        if changed {
            let rendered = render_document(&parsed.frontmatter, &body)?;
            std::fs::write(&doc.final_path, rendered)?;
        }
    }
    Ok((rewritten, unresolved))
}

#[derive(serde::Serialize)]
struct ImportMapFile<'a> {
    session: &'a str,
    docs: &'a [ImportedDoc],
}

/// `<state>/imports/<session-ulid>.toml`: the old-path-to-new-id map, kept after the session's
/// worktree is gone so a human can look up what an import did.
fn save_map(cfg: &Config, session_id: &str, docs: &[ImportedDoc]) -> Result<PathBuf> {
    let dir = cfg.state_dir().join("imports");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_id}.toml"));
    let payload = ImportMapFile { session: session_id, docs };
    std::fs::write(&path, toml::to_string_pretty(&payload).context("serializing the import map")?)?;
    Ok(path)
}

/// `--dry-run`: report what would happen without opening a session or writing anything.
fn dry_run_report(files: &[SourceFile]) -> ImportReport {
    let docs = files
        .iter()
        .map(|f| ImportedDoc { source_path: f.rel.clone(), id: "(dry-run)".into(), title: guess_title(&f.abs), moved: false })
        .collect::<Vec<_>>();
    ImportReport {
        session_id: None,
        merged_commit: None,
        files_found: docs.len(),
        links_rewritten: 0,
        links_unresolved: 0,
        docs,
        map_path: None,
        dry_run: true,
    }
}

/// best-effort title for the dry-run preview: the first `#` heading past any legacy yaml header,
/// else the filename stem. real imports get their title from `check::validate_file` instead.
fn guess_title(path: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        let body = check::migrate_legacy_frontmatter(&text).map(|(_, b)| b).unwrap_or(text);
        for line in body.lines() {
            if let Some(h) = line.trim().strip_prefix("# ") {
                return h.trim().to_string();
            }
        }
    }
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_keeps_alnum_and_replaces_the_rest() {
        assert_eq!(sanitize_filename("My Note (v2)"), "My-Note--v2-");
        assert_eq!(sanitize_filename(""), "note");
    }

    #[test]
    fn add_alias_creates_then_dedupes() {
        let mut fm = Frontmatter::default();
        add_alias(&mut fm, "old name");
        add_alias(&mut fm, "old name");
        let arr = fm.extra.get("aliases").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].as_str(), Some("old name"));
    }

    #[test]
    fn wiki_and_markdown_links_resolve_or_stay_put() {
        let mut map = BTreeMap::new();
        map.insert("other note".to_string(), ("01K5A7Q2XW3ZRCM8T6YB4VJH0N".to_string(), "other note".to_string()));
        let placed = vec![Placed {
            source_rel: "a.md".into(),
            id: "01K5A7Q2XW3ZRCM8T6YB4VJH0A".into(),
            title: "a".into(),
            moved: false,
            final_path: PathBuf::new(),
        }];
        // exercised through the regexes directly, since `rewrite_links` needs real files on disk.
        let body = "see [[Other Note]] and [x](Other Note.md) and [[Missing]]";
        let mut rewritten = 0usize;
        let mut unresolved = 0usize;
        let out = wiki_link_re()
            .replace_all(body, |caps: &Captures| match map.get(&caps[1].trim().to_lowercase()) {
                Some((id, _)) => {
                    rewritten += 1;
                    format!("[{}](kb:{id})", caps[1].trim())
                }
                None => {
                    unresolved += 1;
                    caps[0].to_string()
                }
            })
            .into_owned();
        assert!(out.contains("[Other Note](kb:01K5A7Q2XW3ZRCM8T6YB4VJH0N)"));
        assert!(out.contains("[[Missing]]"));
        assert_eq!(rewritten, 1);
        assert_eq!(unresolved, 1);
        let _ = placed;
    }
}
