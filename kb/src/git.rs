//! thin wrappers over the git binary. every function takes a repo path and runs `git -C <repo> ...`.
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// config overrides applied to every invocation: a user's `color.ui=always` or pager must never
/// reach parsed output. `diff.external` is deliberately left alone here: setting it to an empty
/// string (rather than leaving it unset) makes git try to exec an empty command and fail every
/// diff outright (`error: cannot run : No such file or directory`); callers that build diff text
/// pass `--no-ext-diff` themselves instead (see `diff_range_path`), which is the flag actually
/// meant for this.
const NO_COLOR: [&str; 4] = ["-c", "color.ui=never", "-c", "core.pager=cat"];

/// run git with args; error carries stderr on a non-zero exit. stdout returned as utf-8 (lossy).
pub fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(NO_COLOR)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// like `run` but returns raw bytes (for `git show` of arbitrary content).
pub fn run_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(repo).args(NO_COLOR).args(args).output()?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

pub fn is_repo(path: &Path) -> bool {
    run(path, &["rev-parse", "--is-inside-work-tree"]).map(|s| s.trim() == "true").unwrap_or(false)
}

pub fn toplevel(path: &Path) -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(run(path, &["rev-parse", "--show-toplevel"])?.trim()))
}

pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    Ok(run(repo, &["rev-parse", "--verify", "--quiet", rev])?.trim().to_string())
}

pub fn head(repo: &Path) -> Result<String> {
    rev_parse(repo, "HEAD")
}

/// true when the repository has at least one commit.
pub fn has_commits(repo: &Path) -> bool {
    head(repo).is_ok()
}

pub fn config_get(repo: &Path, key: &str) -> Option<String> {
    run(repo, &["config", "--get", key]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// current git identity as (name, email), from repo then global config.
pub fn identity(repo: &Path) -> (Option<String>, Option<String>) {
    (config_get(repo, "user.name"), config_get(repo, "user.email"))
}

/// file content at a revision. `rev` may be a sha, branch, or `<sha>^`.
pub fn show_file(repo: &Path, rev: &str, path: &str) -> Result<Vec<u8>> {
    run_bytes(repo, &["show", &format!("{rev}:{path}")])
}

/// (status letter, old path, new path) for `git diff --name-status -M base..head`.
/// status is one of A, M, D, R, C, T; renames carry both paths, others repeat the same path.
pub fn diff_name_status(repo: &Path, base: &str, head: &str) -> Result<Vec<(char, String, String)>> {
    let out = run(repo, &["diff", "--name-status", "-M", "-z", &format!("{base}..{head}")])?;
    Ok(parse_name_status(&out))
}

/// same as `diff_name_status` but for the working tree and index against a revision.
pub fn diff_name_status_worktree(repo: &Path, base: &str) -> Result<Vec<(char, String, String)>> {
    let out = run(repo, &["diff", "--name-status", "-M", "-z", base])?;
    Ok(parse_name_status(&out))
}

fn parse_name_status(out: &str) -> Vec<(char, String, String)> {
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    let mut rows = Vec::new();
    while let Some(status) = parts.next() {
        let letter = status.chars().next().unwrap_or('M');
        match letter {
            'R' | 'C' => {
                let old = parts.next().unwrap_or_default().to_string();
                let new = parts.next().unwrap_or_default().to_string();
                rows.push((letter, old, new));
            }
            _ => {
                let p = parts.next().unwrap_or_default().to_string();
                rows.push((letter, p.clone(), p));
            }
        }
    }
    rows
}

/// tracked files under `prefix` at `rev`.
pub fn ls_tree(repo: &Path, rev: &str, prefix: &str) -> Result<Vec<String>> {
    let out = run(repo, &["ls-tree", "-r", "--name-only", "-z", rev, "--", prefix])?;
    Ok(out.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect())
}

/// porcelain v1 status lines for a path (empty when clean).
pub fn status_porcelain(repo: &Path, path: &str) -> Result<Vec<String>> {
    let out = run(repo, &["status", "--porcelain", "--", path])?;
    Ok(out.lines().map(str::to_string).collect())
}

pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// last commit author date (iso 8601) touching `path`, or None when untracked.
pub fn last_commit_date(repo: &Path, path: &str) -> Option<String> {
    run(repo, &["log", "-1", "--format=%aI", "--", path]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// the sha of the newest commit touching `path`; the delete guard's backup precondition
/// (design.md §17.4) compares this against `backup::manifest`'s last mirrored/bundled commits.
pub fn last_commit_sha(repo: &Path, path: &str) -> Option<String> {
    run(repo, &["log", "-1", "--format=%H", "--", path]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// initialize a repository for tests and fixtures with a fixed identity.
pub fn init_for_tests(dir: &Path) -> Result<()> {
    run(dir, &["init", "-q", "-b", "main"])?;
    run(dir, &["config", "user.name", "test user"])?;
    run(dir, &["config", "user.email", "test@example.com"])?;
    Ok(())
}

pub fn commit_all(repo: &Path, message: &str) -> Result<String> {
    run(repo, &["add", "-A"])?;
    run(repo, &["commit", "-q", "-m", message, "--allow-empty"])?;
    head(repo)
}

/// like `commit_all`, but with a fixed author and committer date (`GIT_AUTHOR_DATE` and
/// `GIT_COMMITTER_DATE`); lets a test backdate a commit without touching the system clock.
pub fn commit_all_dated(repo: &Path, message: &str, when_rfc3339: &str) -> Result<String> {
    run(repo, &["add", "-A"])?;
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(NO_COLOR)
        .args(["commit", "-q", "-m", message, "--allow-empty"])
        .env("GIT_AUTHOR_DATE", when_rfc3339)
        .env("GIT_COMMITTER_DATE", when_rfc3339)
        .output()
        .context("running git commit")?;
    if !out.status.success() {
        bail!("git commit failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    head(repo)
}

/// committer date (rfc3339) of a commit; used as "now" for churn windows.
pub fn committer_date(repo: &Path, rev: &str) -> Result<String> {
    Ok(run(repo, &["log", "-1", "--format=%cI", rev])?.trim().to_string())
}

/// one `git log --numstat` stream over `docs/`, with a header per commit carrying hash, committer
/// date, author, and the `Run` and `Co-authored-by` trailers, followed by numstat rows. this is the
/// single pass churn, contributors, and agent-share are all derived from. see `index::gitlog` for the
/// parser and for why `--numstat` and `--name-status` are not combined in one invocation (git only
/// honors one diff-format flag per invocation; the later flag silently wins).
pub fn log_docs_numstat(repo: &Path, target: &str) -> Result<String> {
    run(
        repo,
        &[
            "log",
            target,
            "--no-merges",
            "-M",
            "--numstat",
            "--format=%x1e%H%x00%cI%x00%aN <%aE>%x00%(trailers:key=Run,valueonly,separator=%x1f)%x00%(trailers:key=Co-authored-by,valueonly,separator=%x1f)%x00",
            "--",
            "docs/",
        ],
    )
}

/// commits that deleted paths under `docs/`, for tombstones: hash, committer date, `Run` trailer,
/// then the deleted paths.
pub fn log_deleted_docs(repo: &Path, target: &str) -> Result<String> {
    run(
        repo,
        &[
            "log",
            target,
            "--diff-filter=D",
            "--name-only",
            "--format=%x1e%H%x00%cI%x00%(trailers:key=Run,valueonly)%x00",
            "--",
            "docs/",
        ],
    )
}

/// `git blame --line-porcelain` for one path at one revision: one full metadata record per final
/// line, in line order. used by `index::query::blame` to attribute sections to their last commit.
pub fn blame_porcelain(repo: &Path, rev: &str, path: &str) -> Result<String> {
    run(repo, &["blame", "--line-porcelain", rev, "--", path])
}

/// create a worktree at `path` on a new branch `branch`, starting at `start_point`.
pub fn worktree_add_new_branch(repo: &Path, path: &Path, branch: &str, start_point: &str) -> Result<()> {
    run(repo, &["worktree", "add", "-b", branch, &path.display().to_string(), start_point])?;
    Ok(())
}

/// recreate a worktree at `path` for a branch that already exists.
pub fn worktree_add_existing_branch(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    run(repo, &["worktree", "add", &path.display().to_string(), branch])?;
    Ok(())
}

pub fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    run(repo, &["worktree", "remove", "--force", &path.display().to_string()])?;
    Ok(())
}

pub fn worktree_prune(repo: &Path) -> Result<()> {
    run(repo, &["worktree", "prune"])?;
    Ok(())
}

pub fn update_ref(repo: &Path, refname: &str, sha: &str) -> Result<()> {
    run(repo, &["update-ref", refname, sha])?;
    Ok(())
}

pub fn delete_ref(repo: &Path, refname: &str) -> Result<()> {
    run(repo, &["update-ref", "-d", refname])?;
    Ok(())
}

pub fn branch_delete_force(repo: &Path, branch: &str) -> Result<()> {
    run(repo, &["branch", "-D", branch])?;
    Ok(())
}

/// `git rev-list --count <range>`, 0 when the range does not parse.
pub fn rev_list_count(repo: &Path, range: &str) -> Result<u32> {
    Ok(run(repo, &["rev-list", "--count", range])?.trim().parse().unwrap_or(0))
}

/// commit subjects (`%s`) over `range`, oldest first.
pub fn log_subjects(repo: &Path, range: &str) -> Result<Vec<String>> {
    Ok(run(repo, &["log", "--format=%s", "--reverse", range])?.lines().map(str::to_string).collect())
}

/// remote branches whose tip is a descendant of (or equal to) `rev`; empty when there is no
/// remote or none contain it.
pub fn branches_r_contains(repo: &Path, rev: &str) -> Result<Vec<String>> {
    Ok(run(repo, &["branch", "-r", "--contains", rev])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// enable per-worktree config (`extensions.worktreeConfig`), needed before a worktree's own
/// `sparse-checkout` settings can differ from the main checkout's.
pub fn enable_sparse_worktree_config(repo: &Path) -> Result<()> {
    run(repo, &["config", "extensions.worktreeConfig", "true"])?;
    Ok(())
}

/// set a worktree's sparse-checkout to exactly `patterns`. non-cone mode, since patterns
/// include individual file paths (`docs/<id>.md`) alongside directory prefixes.
pub fn sparse_checkout_set(worktree: &Path, patterns: &[String]) -> Result<()> {
    run(worktree, &["sparse-checkout", "init", "--no-cone"])?;
    let mut args: Vec<&str> = vec!["sparse-checkout", "set"];
    args.extend(patterns.iter().map(String::as_str));
    run(worktree, &args)?;
    Ok(())
}

/// unified diff text for one path between two revisions (`git diff <base>..<head> -- <path>`).
/// empty string when the path is unchanged at the two revisions. used by review packet
/// gathering (design.md §15.2); conflict resolution builds its own diffs in-process instead
/// (`review::udiff`), since a trial-merge tree is not a commit `git diff` can address directly.
/// `run` already forces `color.ui=never` and a no-op pager (see `NO_COLOR`); `--no-color` and
/// `--no-ext-diff` are passed here too so the text stays plain and parseable even if a caller's
/// repo config points `diff.external` somewhere unexpected for this specific invocation.
pub fn diff_range_path(repo: &Path, base: &str, head: &str, path: &str) -> Result<String> {
    run(repo, &["diff", "--no-color", "--no-ext-diff", &format!("{base}..{head}"), "--", path])
}

/// full commit message (`%B`, untrimmed subject plus body) of one revision.
pub fn commit_message(repo: &Path, rev: &str) -> Result<String> {
    Ok(run(repo, &["log", "-1", "--format=%B", rev])?.trim_end().to_string())
}

/// full commit messages (`%B`) of every commit in `base..theirs` that touched `path`, oldest
/// first. used to gather "the messages of main commits touching the path since base" for a
/// conflict resolve packet (design.md §14).
pub fn log_messages_for_path(repo: &Path, base: &str, theirs: &str, path: &str) -> Result<Vec<String>> {
    let out = run(repo, &["log", &format!("{base}..{theirs}"), "--format=%x1e%B", "--", path])?;
    Ok(out.split('\u{1e}').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// `(major, minor)` parsed from `git --version`; `None` when the output does not match the
/// expected `git version X.Y[...]` shape.
fn parse_git_version(s: &str) -> Option<(u32, u32)> {
    let v = s.trim().strip_prefix("git version ")?;
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// true when the local git binary is at least `major.minor`. used to gate `merge_tree_write_tree`
/// on git >= 2.38, the version `merge-tree --write-tree` was introduced in.
pub fn version_at_least(major: u32, minor: u32) -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_git_version(&String::from_utf8_lossy(&o.stdout)))
        .map(|v| v >= (major, minor))
        .unwrap_or(false)
}

/// trial-merge `head` and `main` with `git merge-tree --write-tree`, returning the resulting
/// tree's sha without touching the working tree, the index, or history. `Ok(None)` when the
/// local git predates 2.38 (design.md §15.2's documented fallback: omit the third diff with a
/// note) or when the command produced no parseable tree oid. conflicts inside the trial merge
/// are not treated as an error: `--write-tree` still writes a tree (with conflict markers in the
/// affected blobs) and this function returns it; the caller diffs the one path it cares about.
pub fn merge_tree_write_tree(repo: &Path, head: &str, main: &str) -> Result<Option<String>> {
    if !version_at_least(2, 38) {
        return Ok(None);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(NO_COLOR)
        .args(["merge-tree", "--write-tree", head, main])
        .output()
        .context("running git merge-tree --write-tree")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout.lines().next().unwrap_or("").trim();
    let is_sha = first_line.len() == 40 && first_line.bytes().all(|b| b.is_ascii_hexdigit());
    Ok(is_sha.then(|| first_line.to_string()))
}

/// file content at a tree object (as opposed to `show_file`'s commit-ish `rev`). `Ok(None)` when
/// the path does not exist in that tree (e.g. deleted by the trial merge).
pub fn show_tree_path(repo: &Path, tree: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let out = Command::new("git").arg("-C").arg(repo).args(NO_COLOR).args(["show", &format!("{tree}:{path}")]).output()?;
    Ok(out.status.success().then_some(out.stdout))
}

// ---------------------------------------------------------------------------------------------
// merge, conflicts, review notes, and history (design.md §13, §14, §16.merge-review, §21, §22).
// ---------------------------------------------------------------------------------------------

/// names of every configured remote.
pub fn remotes(repo: &Path) -> Vec<String> {
    run(repo, &["remote"]).map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

pub fn remote_exists(repo: &Path, name: &str) -> bool {
    remotes(repo).iter().any(|r| r == name)
}

pub fn fetch(repo: &Path, remote: &str, refspec: &str) -> Result<()> {
    run(repo, &["fetch", remote, refspec])?;
    Ok(())
}

/// outcome of `git rebase`: `clean` is true only when it finished without stopping.
/// `message` carries combined stdout and stderr for diagnostics on failure.
pub struct RebaseResult {
    pub clean: bool,
    pub message: String,
}

pub fn rebase(repo: &Path, onto: &str) -> Result<RebaseResult> {
    let out = Command::new("git").arg("-C").arg(repo).args(NO_COLOR).args(["rebase", onto]).output().with_context(|| format!("running git rebase {onto}"))?;
    Ok(RebaseResult {
        clean: out.status.success(),
        message: format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    })
}

/// `git rebase --continue` with a no-op editor: the rebased commit's message is set afterward
/// with `amend_message` rather than through the rebase's own editor prompt.
pub fn rebase_continue(repo: &Path) -> Result<RebaseResult> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(NO_COLOR)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .output()
        .context("running git rebase --continue")?;
    Ok(RebaseResult {
        clean: out.status.success(),
        message: format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    })
}

/// true when a rebase is stopped mid-way in `repo`'s worktree (conflict or otherwise).
pub fn rebase_in_progress(repo: &Path) -> bool {
    for name in ["rebase-merge", "rebase-apply"] {
        if let Ok(rel) = run(repo, &["rev-parse", "--git-path", name])
            && !rel.trim().is_empty()
            && repo.join(rel.trim()).exists()
        {
            return true;
        }
    }
    false
}

/// paths with unresolved merge conflicts in the worktree.
pub fn conflicted_paths(repo: &Path) -> Result<Vec<String>> {
    let out = run(repo, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(str::to_string).filter(|s| !s.is_empty()).collect())
}

/// amend the current commit's message in place; used only on the last commit of a private
/// session branch mid-rebase, where rewriting is fine (design.md §14).
pub fn amend_message(repo: &Path, message: &str) -> Result<String> {
    run(repo, &["commit", "--amend", "-q", "-m", message])?;
    head(repo)
}

/// add or replace a git note under `notes_ref` (short name, e.g. `kb-review`) on `rev`.
pub fn notes_add(repo: &Path, notes_ref: &str, rev: &str, content: &str) -> Result<()> {
    run(repo, &["notes", "--ref", notes_ref, "add", "-f", "-m", content, rev])?;
    Ok(())
}

/// append text to an existing (or new) note under `notes_ref` on `rev`.
pub fn notes_append(repo: &Path, notes_ref: &str, rev: &str, content: &str) -> Result<()> {
    run(repo, &["notes", "--ref", notes_ref, "append", "-m", content, rev])?;
    Ok(())
}

/// a rev's note under `notes_ref`, or `None` when it carries none.
pub fn notes_show(repo: &Path, notes_ref: &str, rev: &str) -> Result<Option<String>> {
    match run(repo, &["notes", "--ref", notes_ref, "show", rev]) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

/// push a notes ref explicitly: git never pushes `refs/notes/*` implicitly (design.md §13).
pub fn notes_push(repo: &Path, remote: &str, notes_ref: &str) -> Result<()> {
    run(repo, &["push", remote, &format!("refs/notes/{notes_ref}")])?;
    Ok(())
}

pub fn config_set(repo: &Path, key: &str, value: &str) -> Result<()> {
    run(repo, &["config", key, value])?;
    Ok(())
}

/// compare-and-swap ref update: fails when `refname` does not currently point at `old_sha`.
/// this is the local half of merge step 6's fast-forward land (design.md §13).
pub fn update_ref_cas(repo: &Path, refname: &str, new_sha: &str, old_sha: &str) -> Result<()> {
    run(repo, &["update-ref", refname, new_sha, old_sha])?;
    Ok(())
}

/// a plain push, no force flag: git accepts it only as a fast-forward on the remote side.
pub fn push_plain(repo: &Path, remote: &str, refspec: &str) -> Result<()> {
    run(repo, &["push", remote, refspec])?;
    Ok(())
}

pub fn revert_no_edit(repo: &Path, rev: &str) -> Result<()> {
    run(repo, &["revert", "--no-edit", rev])?;
    Ok(())
}

/// commit shas on `rev`'s history whose message matches `pattern` (`git log --grep`), newest first.
pub fn log_grep(repo: &Path, rev: &str, pattern: &str) -> Result<Vec<String>> {
    Ok(run(repo, &["log", rev, "--format=%H", "--grep", pattern])?.lines().map(str::to_string).filter(|s| !s.is_empty()).collect())
}

/// short ref names matching `pattern` (e.g. `refs/heads/session/*`).
pub fn list_refs(repo: &Path, pattern: &str) -> Result<Vec<String>> {
    Ok(run(repo, &["for-each-ref", "--format=%(refname:short)", pattern])?.lines().map(str::to_string).filter(|s| !s.is_empty()).collect())
}

/// short ref names matching `pattern` that are ancestors of (already merged into) `target`.
pub fn branches_merged(repo: &Path, target: &str, pattern: &str) -> Result<Vec<String>> {
    Ok(run(repo, &["for-each-ref", "--format=%(refname:short)", "--merged", target, pattern])?
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect())
}

/// paths under `pathspec` at `rev` whose content contains the literal string `pattern`
/// (`git grep -l -F`); used by the delete guard (design.md §17.4). an empty result (git's exit
/// code 1 for "no matches") is not an error.
pub fn grep_literal(repo: &Path, rev: &str, pattern: &str, pathspec: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(NO_COLOR)
        .args(["grep", "-l", "-F", pattern, rev, "--", pathspec])
        .output()
        .context("running git grep")?;
    if out.status.success() {
        let prefix = format!("{rev}:");
        Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.strip_prefix(&prefix).unwrap_or(l).to_string()).collect())
    } else if out.status.code() == Some(1) {
        Ok(vec![])
    } else {
        bail!("git grep failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

/// `git worktree list --porcelain` raw output, for `kb tick` and `kb doctor` to parse.
pub fn worktree_list_porcelain(repo: &Path) -> Result<String> {
    run(repo, &["worktree", "list", "--porcelain"])
}

// ---------------------------------------------------------------------------------------------
// additions for `maintain` (nightly curation, compaction) and `backup`. design.md §15.3, §21.
// ---------------------------------------------------------------------------------------------

/// the last commit on `rev` at or before `before_rfc3339` (`git log --before`), or `None` when
/// every commit on `rev` is newer than the cutoff (a young repository).
pub fn rev_list_before(repo: &Path, rev: &str, before_rfc3339: &str) -> Result<Option<String>> {
    let out = run(repo, &["log", "-1", "--format=%H", &format!("--before={before_rfc3339}"), rev])?;
    let sha = out.trim();
    Ok(if sha.is_empty() { None } else { Some(sha.to_string()) })
}

/// commit shas touching `docs/` in `since..until`, oldest first (matches the order a nightly
/// pass wants to re-read: yesterday's earliest merge first).
pub fn log_shas_since(repo: &Path, since: &str, until: &str) -> Result<Vec<String>> {
    Ok(run(repo, &["log", "--reverse", "--format=%H", &format!("{since}..{until}"), "--", "docs/"])?
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect())
}

/// author name of one commit (`git log -1 --format=%an`), e.g. `agent:<skill>@<principal>` or a
/// human name.
pub fn commit_author(repo: &Path, rev: &str) -> Result<String> {
    Ok(run(repo, &["log", "-1", "--format=%an", rev])?.trim().to_string())
}

/// the oldest commit that added `path` (`git log --diff-filter=A --follow`, last line of history),
/// with its author and full message; `None` when the path has no history (deleted before any add
/// landed, or never committed).
pub fn creating_commit(repo: &Path, path: &str) -> Result<Option<(String, String, String)>> {
    let out = run(repo, &["log", "--diff-filter=A", "--follow", "--format=%H", "--", path])?;
    let Some(sha) = out.lines().rfind(|s| !s.is_empty()) else { return Ok(None) };
    let author = commit_author(repo, sha)?;
    let message = commit_message(repo, sha).unwrap_or_default();
    Ok(Some((sha.to_string(), author, message)))
}

/// create an annotated tag `name` at `target`, dated by the commit (not `--force`; a name
/// collision is an error, matching design.md §21's "never on `main`" spirit of not overwriting
/// history markers).
pub fn tag_annotated(repo: &Path, name: &str, target: &str, message: &str) -> Result<()> {
    run(repo, &["tag", "-a", name, "-m", message, target])?;
    Ok(())
}

/// true when tag `name` exists.
pub fn tag_exists(repo: &Path, name: &str) -> bool {
    rev_parse(repo, &format!("refs/tags/{name}")).map(|s| !s.is_empty()).unwrap_or(false)
}

/// the commit a tag (annotated or lightweight) points at.
pub fn tag_commit(repo: &Path, name: &str) -> Result<String> {
    rev_parse(repo, &format!("{name}^{{commit}}"))
}

/// `git init --bare` at an arbitrary path (not necessarily inside an existing repository, so this
/// does not go through `run`'s `-C <repo>` wrapping).
pub fn init_bare(path: &Path) -> Result<()> {
    let out = Command::new("git").args(NO_COLOR).arg("init").arg("--bare").arg(path).output().context("running git init --bare")?;
    if !out.status.success() {
        bail!("git init --bare {} failed: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `git push --mirror <remote>`: every ref, exactly as `repo` has it.
pub fn push_mirror(repo: &Path, remote: &str) -> Result<()> {
    run(repo, &["push", "--mirror", remote])?;
    Ok(())
}

/// `git bundle create <out> <revs...>`.
pub fn bundle_create(repo: &Path, out: &Path, revs: &[&str]) -> Result<()> {
    let mut args = vec!["bundle", "create"];
    let out_str = out.to_string_lossy().into_owned();
    args.push(&out_str);
    args.extend_from_slice(revs);
    run(repo, &args)?;
    Ok(())
}

/// `git bundle verify <bundle>`; the human-readable report on success.
pub fn bundle_verify(repo: &Path, bundle: &Path) -> Result<String> {
    run(repo, &["bundle", "verify", bundle.to_string_lossy().as_ref()])
}

/// `git clone <source> <dest>`, for a restore drill: `dest` must not yet be a repository, so this
/// does not go through `run`'s `-C <repo>` wrapping.
pub fn clone(source: &Path, dest: &Path) -> Result<()> {
    let out = Command::new("git").args(NO_COLOR).arg("clone").arg(source).arg(dest).output().context("running git clone")?;
    if !out.status.success() {
        bail!("git clone {} {} failed: {}", source.display(), dest.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `git repack -adb`, falling back to `-ad` when the installed git rejects `-b` (bitmaps need a
/// single-pack, non-shallow repository on some builds).
pub fn repack(repo: &Path) -> Result<String> {
    match run(repo, &["repack", "-adb"]) {
        Ok(out) => Ok(out),
        Err(_) => run(repo, &["repack", "-ad"]),
    }
}

pub fn commit_graph_write_reachable(repo: &Path) -> Result<()> {
    run(repo, &["commit-graph", "write", "--reachable"])?;
    Ok(())
}

/// best-effort: an older git without `multi-pack-index` is not fatal to compaction.
pub fn multi_pack_index_write(repo: &Path) -> Result<()> {
    let _ = run(repo, &["multi-pack-index", "write"]);
    Ok(())
}

pub fn gc_auto(repo: &Path) -> Result<()> {
    run(repo, &["gc", "--auto"])?;
    Ok(())
}

/// short names of `refs/heads/session/*` branches already merged into `target`, matched with
/// `branches_merged`; deletes each local branch and its hidden base ref.
pub fn branch_delete(repo: &Path, branch: &str) -> Result<()> {
    run(repo, &["branch", "-d", branch])?;
    Ok(())
}

/// the canonicalized common git directory (the shared `.git` every worktree of one repository
/// points at). two paths return equal values here iff they belong to the same repository,
/// worktrees included; used by `kb import` to decide whether `--move` can `git mv` a source file
/// instead of copying it.
pub fn common_dir(repo: &Path) -> Result<std::path::PathBuf> {
    let raw = run(repo, &["rev-parse", "--git-common-dir"])?;
    let raw = raw.trim();
    let p = Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { repo.join(p) };
    Ok(std::fs::canonicalize(&joined).unwrap_or(joined))
}

/// `git mv <from> <to>`, both paths relative to (or resolvable from) `repo`.
pub fn mv(repo: &Path, from: &str, to: &str) -> Result<()> {
    run(repo, &["mv", from, to])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_version() {
        assert_eq!(parse_git_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(parse_git_version("git version 2.38.1.windows.1"), Some((2, 38)));
        assert_eq!(parse_git_version("not git"), None);
    }

    #[test]
    fn diff_range_path_and_merge_tree() {
        let dir = tempfile::tempdir().unwrap();
        init_for_tests(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "one\n").unwrap();
        let base = commit_all(dir.path(), "base").unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "two\n").unwrap();
        let head = commit_all(dir.path(), "head").unwrap();
        let diff = diff_range_path(dir.path(), &base, &head, "docs/a.md").unwrap();
        assert!(diff.contains("-one"));
        assert!(diff.contains("+two"));

        assert_eq!(commit_message(dir.path(), &head).unwrap(), "head");

        if version_at_least(2, 38) {
            let tree = merge_tree_write_tree(dir.path(), &head, &base).unwrap();
            assert!(tree.is_some());
            let content = show_tree_path(dir.path(), tree.as_deref().unwrap(), "docs/a.md").unwrap();
            assert_eq!(content.unwrap(), b"two\n");
        }
    }

    #[test]
    fn common_dir_matches_across_a_worktree_and_mv_moves_a_tracked_file() {
        let dir = tempfile::tempdir().unwrap();
        init_for_tests(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "one\n").unwrap();
        commit_all(dir.path(), "seed").unwrap();

        let wt = dir.path().join(".wt");
        run(dir.path(), &["worktree", "add", "-b", "side", &wt.display().to_string(), "main"]).unwrap();
        assert_eq!(common_dir(dir.path()).unwrap(), common_dir(&wt).unwrap());

        mv(&wt, "docs/a.md", "docs/b.md").unwrap();
        assert!(!wt.join("docs/a.md").exists());
        assert!(wt.join("docs/b.md").exists());
    }

    #[test]
    fn diff_and_show() {
        let dir = tempfile::tempdir().unwrap();
        init_for_tests(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "one\n").unwrap();
        let c1 = commit_all(dir.path(), "one").unwrap();
        std::fs::write(dir.path().join("docs/a.md"), "two\n").unwrap();
        std::fs::write(dir.path().join("docs/b.md"), "b\n").unwrap();
        let c2 = commit_all(dir.path(), "two").unwrap();
        let rows = diff_name_status(dir.path(), &c1, &c2).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|(s, _, p)| *s == 'M' && p == "docs/a.md"));
        assert!(rows.iter().any(|(s, _, p)| *s == 'A' && p == "docs/b.md"));
        assert_eq!(show_file(dir.path(), &c1, "docs/a.md").unwrap(), b"one\n");
        assert!(is_ancestor(dir.path(), &c1, &c2));
        assert!(!is_ancestor(dir.path(), &c2, &c1));
        assert_eq!(identity(dir.path()).1.as_deref(), Some("test@example.com"));
    }
}
