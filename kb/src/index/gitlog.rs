//! parses the raw text of `git::log_docs_numstat` and `git::log_deleted_docs` into structured rows.
//! kept separate from `build` so the parsing itself is unit-testable without a real repository.
use chrono::{DateTime, FixedOffset};

/// one file touched by one commit, after rename resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub path: String,
}

/// one commit from the numstat stream.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub date: DateTime<FixedOffset>,
    pub author_name: String,
    pub author_email: String,
    pub run_trailer: Option<String>,
    pub co_authors: Vec<String>,
    pub files: Vec<FileStat>,
}

impl CommitInfo {
    /// agent commits are authored `agent:<skill>@<principal>`.
    pub fn is_agent(&self) -> bool {
        self.author_name.starts_with("agent:")
    }

    /// the `<principal>` in `agent:<skill>@<principal>`, when this is an agent commit.
    pub fn agent_principal(&self) -> Option<&str> {
        self.author_name.strip_prefix("agent:")?.rsplit_once('@').map(|(_, p)| p)
    }
}

/// resolve a numstat path field to the final path, collapsing `dir/{old => new}` and the plain
/// `old\tnew` rename forms git emits when there is no common prefix.
fn resolve_rename(fields: &[&str]) -> Option<String> {
    let path = fields.get(2)?;
    if fields.len() >= 4 {
        return fields.last().map(|s| s.to_string());
    }
    if let (Some(start), Some(end)) = (path.find('{'), path.find('}')) {
        let prefix = &path[..start];
        let inside = &path[start + 1..end];
        let suffix = &path[end + 1..];
        if let Some((_, new)) = inside.split_once(" => ") {
            return Some(format!("{prefix}{new}{suffix}"));
        }
    }
    Some((*path).to_string())
}

/// parse the output of `git::log_docs_numstat`.
pub fn parse_numstat_log(raw: &str) -> Vec<CommitInfo> {
    let mut out = Vec::new();
    for record in raw.split('\u{1e}') {
        if record.trim().is_empty() {
            continue;
        }
        let Some(nl) = record.find('\n') else { continue };
        let header = &record[..nl];
        let rest = &record[nl + 1..];
        let fields: Vec<&str> = header.split('\u{0}').collect();
        if fields.len() < 5 {
            continue;
        }
        let Ok(date) = DateTime::parse_from_rfc3339(fields[1]) else { continue };
        let (author_name, author_email) = split_author(fields[2]);
        let run_trailer = non_empty(fields[3]).map(|s| s.split('\u{1f}').next().unwrap_or(s).to_string());
        let co_authors = non_empty(fields[4]).map(|s| s.split('\u{1f}').map(str::to_string).collect()).unwrap_or_default();
        let mut files = Vec::new();
        for line in rest.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let added = parts[0].parse::<u64>().ok();
            let deleted = parts[1].parse::<u64>().ok();
            if let Some(path) = resolve_rename(&parts) {
                files.push(FileStat { added, deleted, path });
            }
        }
        out.push(CommitInfo { sha: fields[0].to_string(), date, author_name, author_email, run_trailer, co_authors, files });
    }
    out
}

/// one deletion event from `git::log_deleted_docs`.
#[derive(Debug, Clone)]
pub struct DeletionEvent {
    pub sha: String,
    pub date: DateTime<FixedOffset>,
    pub run_trailer: Option<String>,
    pub paths: Vec<String>,
}

pub fn parse_deleted_log(raw: &str) -> Vec<DeletionEvent> {
    let mut out = Vec::new();
    for record in raw.split('\u{1e}') {
        if record.trim().is_empty() {
            continue;
        }
        let Some(nl) = record.find('\n') else { continue };
        let header = &record[..nl];
        let rest = &record[nl + 1..];
        let fields: Vec<&str> = header.split('\u{0}').collect();
        if fields.len() < 3 {
            continue;
        }
        let Ok(date) = DateTime::parse_from_rfc3339(fields[1]) else { continue };
        let run_trailer = non_empty(fields[2]).map(str::to_string);
        let paths = rest.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        out.push(DeletionEvent { sha: fields[0].to_string(), date, run_trailer, paths });
    }
    out
}

fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// split `"Name <email>"` into (name, email).
fn split_author(s: &str) -> (String, String) {
    match s.rsplit_once(" <") {
        Some((name, rest)) => (name.to_string(), rest.trim_end_matches('>').to_string()),
        None => (s.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_commits() {
        let raw = "\u{1e}aaa\u{0}2026-01-01T00:00:00-06:00\u{0}t <t@example.com>\u{0}\u{0}\u{0}\n\n1\t0\tdocs/a.md\n\u{1e}bbb\u{0}2026-01-02T00:00:00-06:00\u{0}agent:writer@caleb\u{0}run-1\u{0}Bob <bob@example.com>\u{0}\n\n2\t1\tdocs/a.md\n1\t0\tdocs/b.md\n";
        let commits = parse_numstat_log(raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "aaa");
        assert_eq!(commits[0].author_name, "t");
        assert!(!commits[0].is_agent());
        assert_eq!(commits[0].files, vec![FileStat { added: Some(1), deleted: Some(0), path: "docs/a.md".into() }]);

        assert_eq!(commits[1].sha, "bbb");
        assert!(commits[1].is_agent());
        assert_eq!(commits[1].agent_principal(), Some("caleb"));
        assert_eq!(commits[1].run_trailer.as_deref(), Some("run-1"));
        assert_eq!(commits[1].co_authors, vec!["Bob <bob@example.com>".to_string()]);
        assert_eq!(commits[1].files.len(), 2);
    }

    #[test]
    fn resolves_brace_rename() {
        let raw = "\u{1e}ccc\u{0}2026-01-03T00:00:00-06:00\u{0}t <t@example.com>\u{0}\u{0}\u{0}\n\n1\t0\tdocs/{old.md => new.md}\n";
        let commits = parse_numstat_log(raw);
        assert_eq!(commits[0].files[0].path, "docs/new.md");
    }

    #[test]
    fn deleted_log_parses() {
        let raw = "\u{1e}ddd\u{0}2026-01-04T00:00:00-06:00\u{0}\n\ndocs/gone.md\n";
        let events = parse_deleted_log(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].paths, vec!["docs/gone.md".to_string()]);
    }
}
