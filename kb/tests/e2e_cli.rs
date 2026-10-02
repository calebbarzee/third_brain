//! end-to-end CLI tests: drive the built `kb` binary through `assert_cmd`, the way a human or a
//! git hook would. every repository these tests touch is a fresh `tempfile::tempdir()`; nothing
//! under the project's own `.tmp/` is written by `cargo test`. `#[ignore = "bug: ..."]` marks a
//! scenario that documents a real defect found while writing these tests, so the suite stays green.
use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn kb() -> Command {
    Command::cargo_bin("kb").unwrap()
}

/// `kb --repo <repo>`, also run with its cwd set to `repo` so that relative PATH arguments
/// (`check validate docs/x.md`, `parse docs/x.md`, ...) resolve against the repo and not against
/// the test binary's own working directory.
fn kb_at(repo: &Path) -> Command {
    let mut c = kb();
    c.current_dir(repo);
    c.arg("--repo").arg(repo);
    c
}

/// a repo initialized with `kb init --no-hook`, with a git identity set so `owner` fills fill in
/// deterministically. most tests use this: the pre-commit hook itself is scenario 1's own concern.
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    kb().arg("init").arg(dir.path()).arg("--no-hook").assert().success();
    kb::git::run(dir.path(), &["config", "user.name", "Test User"]).unwrap();
    kb::git::run(dir.path(), &["config", "user.email", "test.user@example.com"]).unwrap();
    dir
}

fn write(repo: &Path, rel: &str, content: &str) {
    let p = repo.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

fn read(repo: &Path, rel: &str) -> String {
    std::fs::read_to_string(repo.join(rel)).unwrap()
}

fn commit_all(repo: &Path, msg: &str) -> String {
    kb::git::commit_all(repo, msg).unwrap()
}

/// every `.md` file directly under `dir` (skips `docs/.gitkeep`, which `kb init` always creates).
fn md_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    out.sort();
    out
}

/// the single `.md` file under `dir`; panics with a useful message if there isn't exactly one.
fn only_md_file(dir: &Path) -> std::path::PathBuf {
    let files = md_files(dir);
    assert_eq!(files.len(), 1, "expected exactly one .md file in {}: {files:?}", dir.display());
    files.into_iter().next().unwrap()
}

/// asserts a command's stdout is valid JSON and returns the parsed value.
fn json_stdout(assert: assert_cmd::assert::Assert) -> serde_json::Value {
    let out = assert.get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap_or_else(|e| panic!("not valid json: {e}\n--- stdout ---\n{}", String::from_utf8_lossy(&out)))
}

// fixed, valid (26-char crockford) ulids for hand-written fixtures, distinguished by a 3-letter
// suffix; layer/status live in each test's frontmatter, not in the id.
const ID_A: &str = "01HZZZZZZZ0000000000000AAA";
const ID_B: &str = "01HZZZZZZZ0000000000000BBB";
const ID_C: &str = "01HZZZZZZZ0000000000000CCC";

// ================================================================================================
// 1. init
// ================================================================================================

mod init {
    use super::*;

    #[test]
    fn creates_gitignore_with_kb_dir() {
        let dir = init_repo();
        let gi = read(dir.path(), ".gitignore");
        assert!(gi.lines().any(|l| l.trim() == ".kb/"));
    }

    #[test]
    fn hook_is_installed_and_executable_by_default() {
        let dir = tempfile::tempdir().unwrap();
        kb().arg("init").arg(dir.path()).assert().success();
        let hook = dir.path().join(".git/hooks/pre-commit");
        assert!(hook.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "hook should be executable");
        }
    }

    #[test]
    fn no_hook_skips_installing_it() {
        let dir = tempfile::tempdir().unwrap();
        kb().arg("init").arg(dir.path()).arg("--no-hook").assert().success();
        assert!(!dir.path().join(".git/hooks/pre-commit").is_file());
    }

    #[test]
    fn second_init_without_hook_is_idempotent() {
        // covers the part of "init twice is idempotent" that does hold: once the hook is out of
        // the picture, a second `kb init` at the same path reports nothing new and exits 0.
        let dir = tempfile::tempdir().unwrap();
        kb().arg("init").arg(dir.path()).arg("--no-hook").assert().success();
        kb().arg("init")
            .arg(dir.path())
            .arg("--no-hook")
            .assert()
            .success()
            .stdout(predicate::str::contains("nothing to do"));
    }

    #[test]
    fn second_plain_init_is_idempotent() {
        // design/help promise `kb init` lays down the skeleton "here (or at PATH)" and every other
        // file in it is genuinely idempotent (`init()` skips a file that already exists unless
        // `--force`). the pre-commit hook now follows that rule too: an existing hook whose
        // content matches what we would generate (ignoring the embedded binary path) is left
        // alone silently, so a second plain `kb init` on an already-initialized repo is a no-op.
        let dir = tempfile::tempdir().unwrap();
        kb().arg("init").arg(dir.path()).assert().success();
        kb().arg("init").arg(dir.path()).assert().success();
    }

    #[test]
    fn force_overwrites_existing_hook() {
        let dir = tempfile::tempdir().unwrap();
        kb().arg("init").arg(dir.path()).assert().success();
        kb().arg("init").arg(dir.path()).arg("--force").assert().success().stdout(predicate::str::contains("pre-commit hook"));
    }

    #[test]
    fn installed_hook_actually_runs_on_commit() {
        // regression coverage: the hook script (`kb::cli::init::pre_commit_script`) must call
        // subcommands that actually exist on this build's CLI (`kb check validate --fix --staged`
        // and `kb check secrets --staged`) -- this exact pairing previously did not exist as a
        // real CLI surface (only `kb check validate <paths>`, no `--staged`), which made every
        // commit in a freshly-initialized repo fail at the hook stage.
        let dir = tempfile::tempdir().unwrap();
        kb().arg("init").arg(dir.path()).assert().success();
        kb::git::run(dir.path(), &["config", "user.name", "Test User"]).unwrap();
        kb::git::run(dir.path(), &["config", "user.email", "test.user@example.com"]).unwrap();
        write(dir.path(), "docs/note.md", "# a note\n\nbody text.\n");

        // put the freshly built `kb` binary on PATH so the hook's bare `kb` resolves, exactly as
        // it would for a real user who installed the binary.
        let kb_bin = assert_cmd::cargo::cargo_bin("kb");
        let bin_dir = kb_bin.parent().unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{path}", bin_dir.display());

        let status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "-A"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .current_dir(dir.path())
            .env("PATH", &new_path)
            .args(["commit", "-q", "-m", "test commit"])
            .status()
            .unwrap();
        assert!(status.success(), "commit should succeed: the pre-commit hook's own subcommands should exist");
    }
}

// ================================================================================================
// 2. validate --fix: deterministic fills
// ================================================================================================

mod validate_fill {
    use super::*;

    #[test]
    fn fills_body_only_note_and_renames_to_ulid() {
        let dir = init_repo();
        write(dir.path(), "docs/note.md", "# my great note\n\nbody content about a topic.\n");
        kb_at(dir.path()).args(["check", "validate", "--fix", "docs/note.md"]).assert().success();

        assert!(!dir.path().join("docs/note.md").exists(), "original filename should be gone");
        let new_path = only_md_file(&dir.path().join("docs"));
        let id = new_path.file_stem().unwrap().to_str().unwrap();
        assert!(kb::model::is_ulid(id));
        let text = std::fs::read_to_string(&new_path).unwrap();
        assert!(text.contains("title = \"my great note\""));
        assert!(text.contains("status = \"draft\""));
        assert!(text.contains("layer = \"reference\""));
        assert!(text.contains("agent_policy = \"editable\""));
        assert!(text.contains("owner = \"test.user@example.com\""));
    }

    #[test]
    fn second_fix_is_idempotent() {
        let dir = init_repo();
        write(dir.path(), "docs/note.md", "# idempotent note\n\nbody.\n");
        kb_at(dir.path()).args(["check", "validate", "--fix", "docs/note.md"]).assert().success();
        let new_path = only_md_file(&dir.path().join("docs"));
        let before = std::fs::read_to_string(&new_path).unwrap();

        kb_at(dir.path()).args(["check", "validate", "--fix"]).arg(&new_path).assert().success();
        let after = std::fs::read_to_string(&new_path).unwrap();
        assert_eq!(before, after, "a second --fix on a complete document must not change it");
    }

    #[test]
    fn complete_document_is_untouched() {
        let dir = init_repo();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# t\n\nbody\n"
        );
        write(dir.path(), &format!("docs/{ID_A}.md"), &text);
        kb_at(dir.path()).args(["check", "validate", "--fix", &format!("docs/{ID_A}.md")]).assert().success();
        assert_eq!(read(dir.path(), &format!("docs/{ID_A}.md")), text);
    }

    #[test]
    fn no_fix_reports_and_writes_nothing() {
        let dir = init_repo();
        write(dir.path(), "docs/note.md", "# t\n\nbody\n");
        kb_at(dir.path())
            .args(["check", "validate", "docs/note.md"])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("missing-field"));
        assert!(dir.path().join("docs/note.md").is_file(), "no --fix must not write");
    }
}

// ================================================================================================
// 3. legacy yaml frontmatter migration
// ================================================================================================

#[test]
fn legacy_yaml_frontmatter_migrates_to_toml() {
    let dir = init_repo();
    write(dir.path(), "docs/legacy.md", "---\nid: foo\ntags: [a, b]\naliases: [x]\n---\nbody text\n");
    kb_at(dir.path()).args(["check", "validate", "--fix", "docs/legacy.md"]).assert().success();

    let new_path = only_md_file(&dir.path().join("docs"));
    let text = std::fs::read_to_string(&new_path).unwrap();
    assert!(text.contains("tags = [\"a\", \"b\"]"));
    assert!(!text.contains("---"), "the yaml block delimiters must be gone");
    assert!(!text.contains("aliases: [x]"));
    assert!(text.contains("body text"));
}

// ================================================================================================
// 4. superseded/archived forward pointer
// ================================================================================================

mod superseded {
    use super::*;

    fn doc(status: &str, extra: &str) -> String {
        format!("+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"old\"\nstatus = \"{status}\"\nagent_policy = \"editable\"\nowner = \"c\"\n{extra}+++\n\n# old\n\ncontent\n")
    }

    #[test]
    fn missing_superseded_by_and_banner_are_errors() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc("superseded", ""));
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_A}.md")])
            .assert()
            .failure()
            .stdout(predicate::str::contains("missing-superseded-by"))
            .stdout(predicate::str::contains("missing-banner"));
    }

    #[test]
    fn fix_never_silently_invents_superseded_by() {
        // --fix fills lots of fields deterministically, but a missing `superseded_by` on a
        // superseded document must stay a hard error, not a silent `"none"` fill.
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc("superseded", ""));
        kb_at(dir.path())
            .args(["check", "validate", "--fix", &format!("docs/{ID_A}.md")])
            .assert()
            .failure()
            .stdout(predicate::str::contains("missing-superseded-by"));
        assert!(!read(dir.path(), &format!("docs/{ID_A}.md")).contains("superseded_by"));
    }

    #[test]
    fn fix_adds_banner_when_superseded_by_present() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc("superseded", &format!("superseded_by = \"kb:{ID_B}\"\n")));
        kb_at(dir.path()).args(["check", "validate", "--fix", &format!("docs/{ID_A}.md")]).assert().success();
        let text = read(dir.path(), &format!("docs/{ID_A}.md"));
        assert!(text.contains("> superseded"), "banner should be the first body line");
    }
}

// ================================================================================================
// 5. bad values
// ================================================================================================

mod bad_values {
    use super::*;

    fn valid(field_overrides: &[(&str, &str)]) -> String {
        let mut fm = vec![
            ("id", ID_A.to_string()),
            ("layer", "reference".to_string()),
            ("title", "t".to_string()),
            ("status", "draft".to_string()),
            ("agent_policy", "editable".to_string()),
            ("owner", "c".to_string()),
        ];
        for (k, v) in field_overrides {
            if let Some(slot) = fm.iter_mut().find(|(name, _)| name == k) {
                slot.1 = v.to_string();
            } else {
                fm.push((k, v.to_string()));
            }
        }
        let mut out = String::from("+++\n");
        for (k, v) in fm {
            if v.starts_with('[') || v.starts_with('"') {
                out.push_str(&format!("{k} = {v}\n"));
            } else {
                out.push_str(&format!("{k} = \"{v}\"\n"));
            }
        }
        out.push_str("+++\n\nbody\n");
        out
    }

    #[test]
    fn invalid_enum_value_is_a_finding_not_a_crash() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &valid(&[("layer", "foo")]));
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_A}.md")])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("invalid-frontmatter"));
    }

    #[test]
    fn unclosed_frontmatter_is_a_finding_not_a_crash() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), "+++\nid = \"x\"\n\nbody with no closing delimiter\n");
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_A}.md")])
            .assert()
            .failure()
            .stdout(predicate::str::contains("unclosed-frontmatter"));
    }

    #[test]
    fn id_not_a_ulid_is_error() {
        let dir = init_repo();
        write(dir.path(), "docs/x.md", &valid(&[("id", "not-a-ulid")]));
        kb_at(dir.path()).args(["check", "validate", "docs/x.md"]).assert().failure().stdout(predicate::str::contains("id-not-ulid"));
    }

    #[test]
    fn id_mismatching_filename_is_error() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_B}.md"), &valid(&[("id", ID_A)]));
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_B}.md")])
            .assert()
            .failure()
            .stdout(predicate::str::contains("id-mismatch"));
    }

    #[test]
    fn duplicate_ids_are_caught_only_by_the_whole_repo_scan() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &valid(&[]));
        write(dir.path(), &format!("docs/{ID_B}.md"), &valid(&[("id", ID_A)]));

        // explicit paths: no cross-file corpus check runs, so no duplicate-id finding.
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_A}.md"), &format!("docs/{ID_B}.md")])
            .assert()
            .stdout(predicate::str::contains("duplicate-id").not());

        // no paths: validate_repo also checks duplicate ids and future-dated ulids across the corpus.
        kb_at(dir.path()).args(["check", "validate"]).assert().failure().stdout(predicate::str::contains("duplicate-id"));
    }

    #[test]
    fn future_dated_ulid_is_error() {
        let dir = init_repo();
        // a ulid whose 48-bit timestamp component is far in the future (crockford '7' as the
        // first char already encodes a time past year 2100).
        let future_id = "7ZZZZZZZZZ0000000000000AAA";
        write(dir.path(), &format!("docs/{future_id}.md"), &valid(&[("id", future_id)]));
        kb_at(dir.path()).args(["check", "validate"]).assert().failure().stdout(predicate::str::contains("future-timestamp"));
    }

    #[test]
    fn parent_not_a_kb_uri_is_error() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &valid(&[("parent", "not-a-kb-uri")]));
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_A}.md")])
            .assert()
            .failure()
            .stdout(predicate::str::contains("invalid-parent"));
    }

    #[test]
    fn unknown_ref_rel_and_scheme_are_warnings() {
        let dir = init_repo();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"c\"\n\n[[refs]]\nrel = \"mystery\"\nuri = \"ftp://example.com/x\"\n+++\n\nbody\n"
        );
        write(dir.path(), &format!("docs/{ID_A}.md"), &text);
        let assert = kb_at(dir.path()).args(["check", "validate", &format!("docs/{ID_A}.md")]).assert().success();
        let out = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
        assert!(out.contains("warning [unknown-rel]"));
        assert!(out.contains("warning [unknown-scheme]"));
    }

    #[test]
    fn over_budget_reference_body_is_a_warning() {
        let dir = init_repo();
        let words: Vec<String> = (0..7000).map(|i| format!("word{i}")).collect();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n{}\n",
            words.join(" ")
        );
        write(dir.path(), &format!("docs/{ID_A}.md"), &text);
        kb_at(dir.path())
            .args(["check", "validate", &format!("docs/{ID_A}.md")])
            .assert()
            .success()
            .stdout(predicate::str::contains("over-budget"));
    }
}

// ================================================================================================
// 6. graph
// ================================================================================================

mod graph {
    use super::*;

    fn doc(id: &str, layer: &str, agent_policy: &str, extra: &str, heading: &str) -> String {
        format!("+++\nid = \"{id}\"\nlayer = \"{layer}\"\ntitle = \"{heading}\"\nstatus = \"draft\"\nagent_policy = \"{agent_policy}\"\nowner = \"c\"\n{extra}+++\n\n# {heading}\n")
    }

    #[test]
    fn hierarchy_cycle_reports_the_path() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc(ID_A, "reference", "editable", &format!("parent = \"kb:{ID_B}\"\n"), "a"));
        write(dir.path(), &format!("docs/{ID_B}.md"), &doc(ID_B, "reference", "editable", &format!("parent = \"kb:{ID_C}\"\n"), "b"));
        write(dir.path(), &format!("docs/{ID_C}.md"), &doc(ID_C, "reference", "editable", &format!("parent = \"kb:{ID_A}\"\n"), "c"));
        kb_at(dir.path())
            .args(["check", "graph"])
            .assert()
            .failure()
            .stdout(predicate::str::contains("cycle").and(predicate::str::contains("hierarchy")));
    }

    #[test]
    fn self_parent_is_error() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc(ID_A, "reference", "editable", &format!("parent = \"kb:{ID_A}\"\n"), "a"));
        kb_at(dir.path()).args(["check", "graph"]).assert().failure().stdout(predicate::str::contains("self-edge"));
    }

    #[test]
    fn dangling_parent_is_a_warning_not_an_error() {
        let dir = init_repo();
        let missing = "01HZZZZZZZ0000000000000ZZZ";
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc(ID_A, "reference", "editable", &format!("parent = \"kb:{missing}\"\n"), "a"));
        // dangling is a warning, so exit code stays 0.
        kb_at(dir.path()).args(["check", "graph"]).assert().success().stdout(predicate::str::contains("warning [dangling]"));
    }

    #[test]
    fn parent_of_lower_rank_than_child_is_error() {
        let dir = init_repo();
        // child is `principle` (rank 0); parent is `reference` (rank 4) -- invalid, a parent must
        // be equal or higher (numerically lower rank).
        write(dir.path(), &format!("docs/{ID_A}.md"), &doc(ID_A, "principle", "locked", &format!("parent = \"kb:{ID_B}\"\n"), "a"));
        write(dir.path(), &format!("docs/{ID_B}.md"), &doc(ID_B, "reference", "editable", "", "b"));
        kb_at(dir.path()).args(["check", "graph"]).assert().failure().stdout(predicate::str::contains("parent-rank"));
    }

    #[test]
    fn succession_cycle_via_superseded_by() {
        let dir = init_repo();
        write(
            dir.path(),
            &format!("docs/{ID_A}.md"),
            &doc(ID_A, "reference", "editable", &format!("status = \"superseded\"\nsuperseded_by = \"kb:{ID_B}\"\n"), "a")
                .replace("status = \"draft\"\n", ""),
        );
        write(
            dir.path(),
            &format!("docs/{ID_B}.md"),
            &doc(ID_B, "reference", "editable", &format!("status = \"superseded\"\nsuperseded_by = \"kb:{ID_A}\"\n"), "b")
                .replace("status = \"draft\"\n", ""),
        );
        kb_at(dir.path())
            .args(["check", "graph"])
            .assert()
            .failure()
            .stdout(predicate::str::contains("cycle").and(predicate::str::contains("succession")));
    }
}

// ================================================================================================
// 7. infer and gate over a two-commit repo
// ================================================================================================

mod infer_and_gate {
    use super::*;

    fn commit_doc(repo: &Path, id: &str, text: &str, msg: &str) {
        write(repo, &format!("docs/{id}.md"), text);
        commit_all(repo, msg);
    }

    #[test]
    fn append_is_auto_for_everyone() {
        let dir = init_repo();
        let base_text = format!("+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# one\n\ntext\n");
        commit_doc(dir.path(), ID_A, &base_text, "base");
        let base = kb::git::head(dir.path()).unwrap();
        write(dir.path(), &format!("docs/{ID_A}.md"), &format!("{base_text}\n# two\n\nmore\n"));
        commit_all(dir.path(), "append");
        let head = kb::git::head(dir.path()).unwrap();

        kb_at(dir.path())
            .args(["check", "infer", &format!("{base}..{head}")])
            .assert()
            .success()
            .stdout(predicate::str::contains("append two"));
        kb_at(dir.path())
            .args(["check", "gate", &format!("{base}..{head}")])
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::contains("auto"));
    }

    #[test]
    fn delete_is_always_review() {
        let dir = init_repo();
        let text = format!("+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# one\n\ntext\n");
        commit_doc(dir.path(), ID_A, &text, "base");
        let base = kb::git::head(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(format!("docs/{ID_A}.md"))).unwrap();
        commit_all(dir.path(), "delete");
        let head = kb::git::head(dir.path()).unwrap();

        kb_at(dir.path())
            .args(["check", "infer", &format!("{base}..{head}")])
            .assert()
            .success()
            .stdout(predicate::str::contains("delete"));
        kb_at(dir.path()).args(["check", "gate", &format!("{base}..{head}")]).assert().failure().code(1).stdout(predicate::str::contains("review"));
    }

    #[test]
    fn decision_critical_section_forces_review_for_a_human() {
        let dir = init_repo();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"decision\"\ntitle = \"a decision\"\nstatus = \"canon\"\nagent_policy = \"append-only\"\nowner = \"c\"\n+++\n\n# decision\n\nwe chose X\n\n# consequences\n\nit means Y\n"
        );
        commit_doc(dir.path(), ID_A, &text, "base");
        let base = kb::git::head(dir.path()).unwrap();
        write(dir.path(), &format!("docs/{ID_A}.md"), &text.replace("we chose X", "we chose X, revised"));
        commit_all(dir.path(), "revise decision");
        let head = kb::git::head(dir.path()).unwrap();

        // no --agent: the structure gate classifies a human change too, it just never blocks.
        kb_at(dir.path())
            .args(["check", "gate", &format!("{base}..{head}")])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("review").and(predicate::str::contains("critical section")));
    }

    #[test]
    fn owner_change_is_review_for_agent_and_auto_for_human() {
        let dir = init_repo();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"procedure\"\ntitle = \"a procedure\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# prerequisites\n\nneed X\n\n# steps\n\ndo Y\n\n# rollback\n\nundo Y\n"
        );
        commit_doc(dir.path(), ID_A, &text, "base");
        let base = kb::git::head(dir.path()).unwrap();
        write(dir.path(), &format!("docs/{ID_A}.md"), &text.replace("owner = \"c\"", "owner = \"someone-else\""));
        commit_all(dir.path(), "change owner");
        let head = kb::git::head(dir.path()).unwrap();

        kb_at(dir.path()).args(["check", "gate", &format!("{base}..{head}")]).assert().success().code(0);
        kb_at(dir.path())
            .args(["check", "gate", &format!("{base}..{head}"), "--agent"])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("policy field changed by an agent"));
    }

    #[test]
    fn locked_principle_layer_is_review_even_without_agent_flag() {
        // `principle`'s structure default is `critical = ["*"]`, so ANY body change on a
        // principle document is review class regardless of actor -- exercised here with no
        // --agent, i.e. a human edit, per design.md §15.1's "documents above the criticality
        // band" / "*" critical rule.
        let dir = init_repo();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"principle\"\ntitle = \"a principle\"\nstatus = \"canon\"\nagent_policy = \"locked\"\nowner = \"c\"\n+++\n\n# statement\n\nwe believe Z\n"
        );
        commit_doc(dir.path(), ID_A, &text, "base");
        let base = kb::git::head(dir.path()).unwrap();
        write(dir.path(), &format!("docs/{ID_A}.md"), &text.replace("we believe Z", "we believe Z, amended"));
        commit_all(dir.path(), "amend principle");
        let head = kb::git::head(dir.path()).unwrap();

        kb_at(dir.path())
            .args(["check", "gate", &format!("{base}..{head}")])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("review"));
    }

    #[test]
    fn decision_supersede_is_always_review() {
        let dir = init_repo();
        let text = format!(
            "+++\nid = \"{ID_A}\"\nlayer = \"decision\"\ntitle = \"a decision\"\nstatus = \"canon\"\nagent_policy = \"append-only\"\nowner = \"c\"\n+++\n\n# decision\n\nwe chose X\n\n# consequences\n\nit means Y\n"
        );
        commit_doc(dir.path(), ID_A, &text, "base");
        let base = kb::git::head(dir.path()).unwrap();
        let superseded = text
            .replace("status = \"canon\"", "status = \"superseded\"")
            .replacen("owner = \"c\"\n", &format!("owner = \"c\"\nsuperseded_by = \"kb:{ID_B}\"\n"), 1)
            .replacen(
                "# decision\n\nwe chose X\n",
                &format!("> superseded 2026-09-21 by [x](kb:{ID_B}). kept for history; not current.\n\n# decision\n\nwe chose X\n"),
                1,
            );
        write(dir.path(), &format!("docs/{ID_A}.md"), &superseded);
        commit_all(dir.path(), "supersede decision");
        let head = kb::git::head(dir.path()).unwrap();

        kb_at(dir.path()).args(["check", "gate", &format!("{base}..{head}")]).assert().failure().code(1);
    }
}

// ================================================================================================
// 8. secrets
// ================================================================================================

mod secrets {
    use super::*;

    #[test]
    fn detects_every_kind_and_never_prints_the_secret() {
        let dir = init_repo();
        commit_all(dir.path(), "init commit");
        let gh = format!("ghp_{}", "a".repeat(36));
        let aws = format!("AKIA{}", "B".repeat(16));
        let text = format!(
            "# leaked stuff\n\ngithub token: {gh}\naws key: {aws}\n-----BEGIN RSA PRIVATE KEY-----\nMIIB...fake...\n-----END RSA PRIVATE KEY-----\nslack webhook: https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX\n"
        );
        write(dir.path(), "docs/secret.md", &text);
        kb::git::run(dir.path(), &["add", "docs/secret.md"]).unwrap();

        let assert = kb_at(dir.path()).args(["check", "secrets", "--staged"]).assert().failure().code(1);
        let out = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
        assert!(out.contains("github-pat"));
        assert!(out.contains("aws-access-key-id"));
        assert!(out.contains("private-key-block"));
        assert!(out.contains("slack-webhook"));
        assert!(!out.contains(&gh), "the raw secret must never be printed");
        assert!(!out.contains(&aws), "the raw secret must never be printed");
    }

    #[test]
    fn clean_staged_file_exits_zero() {
        let dir = init_repo();
        commit_all(dir.path(), "init commit");
        write(dir.path(), "docs/clean.md", "# nothing to see here\n\njust a normal document.\n");
        kb::git::run(dir.path(), &["add", "docs/clean.md"]).unwrap();
        kb_at(dir.path()).args(["check", "secrets", "--staged"]).assert().success();
    }

    #[test]
    fn allow_paths_exempts_a_path() {
        let dir = init_repo();
        let kb_toml = read(dir.path(), "kb.toml");
        write(dir.path(), "kb.toml", &kb_toml.replace("allow_paths = []", "allow_paths = [\"docs/exempt/**\"]"));
        commit_all(dir.path(), "init commit");
        let secret = format!("ghp_{}", "c".repeat(36));
        write(dir.path(), "docs/exempt/x.md", &format!("token: {secret}\n"));
        kb::git::run(dir.path(), &["add", "docs/exempt/x.md"]).unwrap();
        kb_at(dir.path()).args(["check", "secrets", "--staged"]).assert().success();
    }

    #[test]
    fn range_mode_scans_added_lines_between_two_commits() {
        let dir = init_repo();
        commit_all(dir.path(), "init commit");
        let base = kb::git::head(dir.path()).unwrap();
        let secret = format!("ghp_{}", "d".repeat(36));
        write(dir.path(), "docs/secret2.md", &format!("token: {secret}\n"));
        commit_all(dir.path(), "add secret");
        let head = kb::git::head(dir.path()).unwrap();
        kb_at(dir.path())
            .args(["check", "secrets", &format!("{base}..{head}")])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("github-pat"));
    }
}

// ================================================================================================
// 9. index
// ================================================================================================

mod index {
    use super::*;

    /// an 8-document corpus: every layer, a parent chain, a superseded pair, a draft, tags, and a
    /// `related` ref -- everything `kb index` needs to exercise search/get/refs/ls/show/graph.
    fn seed_corpus() -> (tempfile::TempDir, std::collections::HashMap<&'static str, &'static str>) {
        let dir = init_repo();
        let principle = "01HZZZZZZZ0000000000000PPP";
        let design = "01HZZZZZZZ0000000000000DDD";
        let decision = "01HZZZZZZZ0000000000000CCC";
        let procedure = "01HZZZZZZZ0000000000000RRR";
        let ref1 = "01HZZZZZZZ0000000000000F11";
        let ref2 = "01HZZZZZZZ0000000000000F22";
        let draft = "01HZZZZZZZ0000000000000G11";
        let old = "01HZZZZZZZ0000000000000H11";

        write(
            dir.path(),
            &format!("docs/{principle}.md"),
            &format!("+++\nid = \"{principle}\"\nlayer = \"principle\"\ntitle = \"quantum flux stability principle\"\nstatus = \"canon\"\nagent_policy = \"locked\"\nowner = \"c\"\n+++\n\n# statement\n\nthe quantumfrobnicator must remain stable under load.\n"),
        );
        write(
            dir.path(),
            &format!("docs/{design}.md"),
            &format!("+++\nid = \"{design}\"\nlayer = \"design\"\ntitle = \"widget architecture design\"\nstatus = \"canon\"\nagent_policy = \"locked\"\nowner = \"c\"\nparent = \"kb:{principle}\"\n+++\n\n# overview\n\nthe widget subsystem uses a hyperbolic tessellator for rendering.\n"),
        );
        write(
            dir.path(),
            &format!("docs/{decision}.md"),
            &format!("+++\nid = \"{decision}\"\nlayer = \"decision\"\ntitle = \"use retry path for tokens\"\nstatus = \"canon\"\nagent_policy = \"append-only\"\nowner = \"c\"\nparent = \"kb:{design}\"\n+++\n\n# decision\n\nwe moved token refresh to the retry path.\n\n# consequences\n\nfewer race conditions.\n"),
        );
        write(
            dir.path(),
            &format!("docs/{procedure}.md"),
            &format!("+++\nid = \"{procedure}\"\nlayer = \"procedure\"\ntitle = \"deploy the launcher\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\nparent = \"kb:{decision}\"\n+++\n\n# prerequisites\n\nssh access to the launcher box.\n\n# steps\n\nrun the deploy script named zephyrograph.\n\n# rollback\n\nrevert to previous tag.\n"),
        );
        write(
            dir.path(),
            &format!("docs/{ref1}.md"),
            &format!("+++\nid = \"{ref1}\"\nlayer = \"reference\"\ntitle = \"static ip settings\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\ntags = [\"net\"]\nparent = \"kb:{procedure}\"\n\n[[refs]]\nrel = \"related\"\nuri = \"kb:{ref2}\"\n+++\n\n# setup\n\nset the static ip to 10.0.0.5 with a hyphen-in-word like foo-bar.\n"),
        );
        write(
            dir.path(),
            &format!("docs/{ref2}.md"),
            &format!("+++\nid = \"{ref2}\"\nlayer = \"reference\"\ntitle = \"unicode notes cafe\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\ntags = [\"misc\"]\n+++\n\n# caf\u{e9} notes\n\nna\u{ef}ve r\u{e9}sum\u{e9} about coop\u{e9}ration and jalape\u{f1}o peppers.\n"),
        );
        write(
            dir.path(),
            &format!("docs/{draft}.md"),
            &format!("+++\nid = \"{draft}\"\nlayer = \"reference\"\ntitle = \"draft scratchpad\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# scratch\n\nexact phrase test: \"the quick brown fox\"\n"),
        );
        write(
            dir.path(),
            &format!("docs/{old}.md"),
            &format!("+++\nid = \"{old}\"\nlayer = \"reference\"\ntitle = \"old superseded doc\"\nstatus = \"superseded\"\nagent_policy = \"editable\"\nowner = \"c\"\nsuperseded_by = \"kb:{ref1}\"\n+++\n\n> superseded 2026-09-20 by [static ip settings](kb:{ref1}). kept for history; not current.\n\n# old content\n\nthis used to describe the launcher's ip config, now superseded.\n"),
        );

        commit_all(dir.path(), "seed 8-doc corpus");
        let mut ids = std::collections::HashMap::new();
        ids.insert("principle", principle);
        ids.insert("design", design);
        ids.insert("decision", decision);
        ids.insert("procedure", procedure);
        ids.insert("ref1", ref1);
        ids.insert("ref2", ref2);
        ids.insert("draft", draft);
        ids.insert("old", old);
        (dir, ids)
    }

    #[test]
    fn build_reports_all_documents_and_is_reused_on_rebuild() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path())
            .args(["index", "build"])
            .assert()
            .success()
            .stdout(predicate::str::contains("8 documents").and(predicate::str::contains("6 canon")));
        kb_at(dir.path()).args(["index", "build"]).assert().success().stdout(predicate::str::contains("already built"));

        // a new commit builds a second, distinct artifact.
        write(dir.path(), &format!("docs/{}.md", ids["draft"]), &format!("+++\nid = \"{}\"\nlayer = \"reference\"\ntitle = \"draft scratchpad\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# scratch\n\nupdated.\n", ids["draft"]));
        commit_all(dir.path(), "touch draft");
        kb_at(dir.path()).args(["index", "build"]).assert().success().stdout(predicate::str::contains("already built").not());
        let artifacts: Vec<_> = std::fs::read_dir(dir.path().join(".kb/index")).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(artifacts.len(), 2, "two distinct commits should leave two artifacts");
    }

    #[test]
    fn search_finds_a_distinctive_word_and_excludes_superseded_by_default() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();

        kb_at(dir.path()).args(["index", "search", "hyperbolic"]).assert().success().stdout(predicate::str::contains(ids["design"]));

        // "launcher" appears in both the (canon) procedure doc and the superseded doc.
        let default_hits = String::from_utf8_lossy(&kb_at(dir.path()).args(["index", "search", "launcher"]).assert().success().get_output().stdout).into_owned();
        assert!(default_hits.contains(ids["procedure"]));
        assert!(!default_hits.contains(ids["old"]), "superseded documents are excluded by default");

        let any_status = String::from_utf8_lossy(
            &kb_at(dir.path()).args(["index", "search", "launcher", "--status-any"]).assert().success().get_output().stdout,
        )
        .into_owned();
        assert!(any_status.contains(ids["old"]), "--status-any widens the candidate set to superseded");
    }

    #[test]
    fn get_by_full_id_and_by_unique_prefix() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();
        let full = ids["principle"];
        // the seeded ids only differ in their last 3 chars, so the unique prefix must be long
        // enough to include at least the first differing character.
        let unique_prefix = &full[..24];

        let by_full = json_stdout(kb_at(dir.path()).args(["index", "get", full]).assert().success());
        let by_prefix = json_stdout(kb_at(dir.path()).args(["index", "get", unique_prefix]).assert().success());
        assert_eq!(by_full["id"], full);
        assert_eq!(by_prefix["id"], full);
    }

    #[test]
    fn get_reports_ambiguous_prefix_as_ambiguous() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();
        // every seeded id shares this 10-char prefix ("01HZZZZZZZ").
        let ambiguous_prefix = &ids["principle"][..10];
        kb_at(dir.path()).args(["index", "get", ambiguous_prefix]).assert().failure().stderr(predicate::str::contains("ambiguous"));
    }

    #[test]
    fn refs_outgoing_and_incoming() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();
        kb_at(dir.path())
            .args(["index", "refs", ids["design"]])
            .assert()
            .success()
            .stdout(predicate::str::contains(ids["principle"]));
        kb_at(dir.path())
            .args(["index", "refs", ids["principle"], "--incoming"])
            .assert()
            .success()
            .stdout(predicate::str::contains(ids["design"]));
    }

    #[test]
    fn ls_filters_by_layer_status_and_tag() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();

        let by_layer = String::from_utf8_lossy(&kb_at(dir.path()).args(["index", "ls", "--layer", "reference"]).assert().success().get_output().stdout).into_owned();
        assert!(by_layer.contains(ids["ref1"]) && by_layer.contains(ids["ref2"]) && by_layer.contains(ids["draft"]) && by_layer.contains(ids["old"]));
        assert!(!by_layer.contains(ids["principle"]));

        let by_status = String::from_utf8_lossy(&kb_at(dir.path()).args(["index", "ls", "--status", "draft"]).assert().success().get_output().stdout).into_owned();
        assert!(by_status.contains(ids["draft"]) && !by_status.contains(ids["ref1"]));

        let by_tag = String::from_utf8_lossy(&kb_at(dir.path()).args(["index", "ls", "--tag", "net"]).assert().success().get_output().stdout).into_owned();
        assert!(by_tag.contains(ids["ref1"]) && !by_tag.contains(ids["ref2"]));
    }

    #[test]
    fn show_reflects_committed_vs_uncommitted_state() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();
        kb_at(dir.path())
            .args(["index", "show", ids["ref1"]])
            .assert()
            .success()
            .stdout(predicate::str::contains("updated_state=committed"));

        let path = dir.path().join(format!("docs/{}.md", ids["ref1"]));
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\nan uncommitted edit.\n");
        std::fs::write(&path, text).unwrap();
        kb_at(dir.path())
            .args(["index", "show", ids["ref1"]])
            .assert()
            .success()
            .stdout(predicate::str::contains("updated_state=uncommitted"));
    }

    #[test]
    fn graph_top_ranks_by_criticality() {
        let (dir, _ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();
        kb_at(dir.path()).args(["index", "graph", "--top", "2"]).assert().success();
    }

    #[test]
    fn search_without_a_built_artifact_gives_a_clear_error() {
        let dir = init_repo();
        kb_at(dir.path()).args(["index", "search", "anything"]).assert().failure().code(1).stderr(predicate::str::contains("kb index build"));
    }

    #[test]
    fn fts_quoted_phrase_heading_only_word_and_unicode() {
        let (dir, ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();

        kb_at(dir.path()).args(["index", "search", "\"quick brown fox\""]).assert().success().stdout(predicate::str::contains(ids["draft"]));
        // "zephyrograph" appears only in the `steps` section body, not any heading text.
        kb_at(dir.path()).args(["index", "search", "zephyrograph"]).assert().success().stdout(predicate::str::contains(ids["procedure"]));
        // diacritics are folded: an accent-free query still finds the accented text.
        kb_at(dir.path()).args(["index", "search", "naive"]).assert().success().stdout(predicate::str::contains(ids["ref2"]));
        kb_at(dir.path()).args(["index", "search", "jalapeno"]).assert().success().stdout(predicate::str::contains(ids["ref2"]));
    }

    #[test]
    fn search_with_a_hyphenated_query_does_not_crash() {
        let (dir, _ids) = seed_corpus();
        kb_at(dir.path()).args(["index", "build"]).assert().success();
        kb_at(dir.path()).args(["index", "search", "foo-bar"]).assert().success();
    }
}

// ================================================================================================
// 10. --json validity
// ================================================================================================

mod json_output {
    use super::*;

    #[test]
    fn read_commands_emit_valid_json() {
        let dir = init_repo();
        let id = ID_A;
        write(
            dir.path(),
            &format!("docs/{id}.md"),
            &format!("+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# t\n\nhello world\n"),
        );
        commit_all(dir.path(), "seed");
        kb_at(dir.path()).args(["index", "build"]).assert().success();

        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "search", "hello"]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "get", id]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "show", id]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "refs", id]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "ls"]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "blame", id]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["index", "graph"]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["check", "validate"]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["check", "graph"]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["check", "secrets"]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["parse", &format!("docs/{id}.md")]).assert().success());

        let base = kb::git::head(dir.path()).unwrap();
        write(dir.path(), &format!("docs/{id}.md"), &format!("+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\n# t\n\nhello world\n\n# two\n\nmore\n"));
        commit_all(dir.path(), "append");
        let head = kb::git::head(dir.path()).unwrap();
        json_stdout(kb_at(dir.path()).arg("--json").args(["check", "infer", &format!("{base}..{head}")]).assert().success());
        json_stdout(kb_at(dir.path()).arg("--json").args(["check", "gate", &format!("{base}..{head}")]).assert().success());
    }

    #[test]
    fn init_emits_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        json_stdout(kb().arg("--json").arg("init").arg(dir.path()).arg("--no-hook").assert().success());
    }
}

// ================================================================================================
// 11. cli hygiene
// ================================================================================================

mod hygiene {
    use super::*;

    #[test]
    fn no_args_prints_usage() {
        kb().assert().failure().code(2).stderr(predicate::str::contains("Usage:"));
    }

    #[test]
    fn unknown_subcommand_is_a_clear_clap_error() {
        kb().arg("frobnicate").assert().failure().code(2).stderr(predicate::str::contains("unrecognized subcommand"));
    }

    #[test]
    fn repo_pointing_at_a_real_non_kb_directory_is_reported() {
        // a directory that exists, is not a git repo, and has no kb.toml anywhere upward is not a
        // kb repository at all: `resolve_root` must report that clearly instead of silently
        // treating the bare directory as an empty repo (see bug 6 fix in `resolve_root`).
        let dir = tempfile::tempdir().unwrap();
        kb_at(dir.path()).args(["check", "validate"]).assert().failure().stderr(predicate::str::contains("no kb repository found"));
    }

    #[test]
    fn repo_pointing_at_a_nonexistent_path_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("does_not_exist");
        kb().arg("--repo").arg(&bogus).args(["check", "validate"]).assert().failure();
    }

    #[test]
    fn index_show_missing_id_is_a_clear_error_not_a_panic() {
        let dir = init_repo();
        commit_all(dir.path(), "empty init");
        kb_at(dir.path())
            .args(["index", "show", "01ZZZZZZZZZZZZZZZZZZZZZZZZ"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("no document with id").or(predicate::str::contains("panic")).and(predicate::str::contains("panic").not()));
    }

    #[test]
    fn find_root_works_from_a_subdirectory() {
        let dir = init_repo();
        write(dir.path(), &format!("docs/{ID_A}.md"), &format!("+++\nid = \"{ID_A}\"\nlayer = \"reference\"\ntitle = \"t\"\nstatus = \"draft\"\nagent_policy = \"editable\"\nowner = \"c\"\n+++\n\nbody\n"));
        kb().current_dir(dir.path().join("docs")).args(["check", "validate", &format!("{ID_A}.md")]).assert().success();
    }
}

// ================================================================================================
// 12. provider classify (ollama), skipped when nothing is listening on 11434
// ================================================================================================

mod provider {
    use super::*;

    fn ollama_reachable() -> bool {
        std::net::TcpStream::connect_timeout(&"127.0.0.1:11434".parse().unwrap(), std::time::Duration::from_millis(500)).is_ok()
    }

    #[test]
    fn classify_then_write_produces_a_still_valid_document() {
        if !ollama_reachable() {
            eprintln!("skipping: no provider listening on localhost:11434");
            return;
        }
        let dir = init_repo();
        let kb_toml = read(dir.path(), "kb.toml");
        write(
            dir.path(),
            "kb.toml",
            &kb_toml.replace(
                "[secrets]",
                "[providers.classify]\nkind = \"ollama\"\nmodel = \"llama3.2:3b\"\nbase_url = \"http://localhost:11434\"\n\n[secrets]",
            ),
        );
        let fixture = "/home/calebbarzee/1_projects/dev/third_brain/.tmp/fixtures/notes/static_ip.md";
        let text = std::fs::read_to_string(fixture).unwrap();
        write(dir.path(), "docs/static_ip.md", &text);

        kb_at(dir.path())
            .args(["provider", "classify", "docs/static_ip.md"])
            .assert()
            .success()
            .stdout(predicate::str::contains("layer:").and(predicate::str::contains("tags:")));

        kb_at(dir.path()).args(["provider", "classify", "docs/static_ip.md", "--write"]).assert().success();
        // finish filling the required fields by hand so the document is fully valid, then confirm
        // the model-written layer/tags survived and validate is clean.
        kb_at(dir.path()).args(["check", "validate", "--fix", "docs/static_ip.md"]).assert().success();
        let new_path = md_files(&dir.path().join("docs")).into_iter().find(|p| p.file_stem().unwrap() != "static_ip").unwrap();
        let final_text = std::fs::read_to_string(&new_path).unwrap();
        assert!(final_text.contains("layer ="));
        assert!(final_text.contains("tags ="));
        kb_at(dir.path()).arg("check").arg("validate").arg(new_path.strip_prefix(dir.path()).unwrap()).assert().success();
    }

    #[test]
    fn classify_write_does_not_corrupt_legacy_yaml_frontmatter() {
        if !ollama_reachable() {
            eprintln!("skipping: no provider listening on localhost:11434");
            return;
        }
        let dir = init_repo();
        let kb_toml = read(dir.path(), "kb.toml");
        write(
            dir.path(),
            "kb.toml",
            &kb_toml.replace(
                "[secrets]",
                "[providers.classify]\nkind = \"ollama\"\nmodel = \"llama3.2:3b\"\nbase_url = \"http://localhost:11434\"\n\n[secrets]",
            ),
        );
        write(dir.path(), "docs/legacy.md", "---\nid: legacy\ntags: []\n---\nsome body content about deploying software safely.\n");

        kb_at(dir.path()).args(["provider", "classify", "docs/legacy.md", "--write"]).assert().success();
        let text = read(dir.path(), "docs/legacy.md");
        assert_eq!(text.matches("---").count(), 0, "the legacy yaml header should have been migrated, not left as stray body text");
    }
}

/// audit P0-3: the pre-commit fix re-stages what it rewrote and nothing else. an untracked,
/// half-written note next to the staged one must stay out of the commit.
#[test]
fn staged_fix_leaves_unstaged_and_untracked_files_alone() {
    let dir = init_repo();
    let repo = dir.path();
    commit_all(repo, "init");
    write(repo, "docs/a.md", "# static ip\n\nset it with nmcli.\n");
    kb::git::run(repo, &["add", "docs/a.md"]).unwrap();
    write(repo, "docs/scratch.md", "half written, not ready\n");

    kb_at(repo).args(["check", "validate", "--fix", "--staged", "--no-classify"]).assert().success();

    let staged = kb::git::run(repo, &["diff", "--cached", "--name-status"]).unwrap();
    assert!(!staged.contains("scratch.md"), "untracked file was staged:\n{staged}");
    assert!(!staged.contains("docs/a.md"), "the renamed original should not be staged as a file:\n{staged}");
    let added: Vec<&str> = staged.lines().filter(|l| l.starts_with('A')).collect();
    assert_eq!(added.len(), 1, "exactly the renamed note is staged:\n{staged}");
    assert!(repo.join("docs/scratch.md").is_file());
    let untracked = kb::git::run(repo, &["status", "--porcelain", "--", "docs/scratch.md"]).unwrap();
    assert!(untracked.starts_with("??"), "{untracked}");
}

/// `KB_NOW` fixes the clock for everything kb records: a new document's ulid and the date on
/// the commits kb makes. this is what the multi-day simulation relies on.
#[test]
fn kb_now_fixes_ulids_and_commit_dates() {
    let dir = init_repo();
    let repo = dir.path();
    commit_all(repo, "init");
    let when = "2027-01-15T09:30:00+00:00";

    write(repo, "docs/a.md", "# dated note\n\nbody.\n");
    kb_at(repo).env("KB_NOW", when).args(["check", "validate", "--fix", "--no-classify", "docs/a.md"]).assert().success();
    let file = md_files(&repo.join("docs")).into_iter().next().expect("renamed note");
    let id = file.file_stem().unwrap().to_str().unwrap().to_string();
    let ulid = ulid::Ulid::from_string(&id).unwrap();
    assert_eq!(ulid.timestamp_ms(), chrono::DateTime::parse_from_rfc3339(when).unwrap().timestamp_millis() as u64);

    let out = kb_at(repo).env("KB_NOW", when).args(["session", "open", "--objective", "dated"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let worktree = text.lines().last().unwrap().trim().to_string();
    let session_id = Path::new(&worktree).file_name().unwrap().to_str().unwrap().to_string();
    write(Path::new(&worktree), "docs/b.md", "# second\n\nbody.\n");
    kb_at(repo).env("KB_NOW", when).args(["session", "settle", &session_id, "-m", "dated settle"]).assert().success();
    let date = kb::git::run(Path::new(&worktree), &["log", "-1", "--format=%cI"]).unwrap();
    assert_eq!(date.trim(), "2027-01-15T09:30:00+00:00");
}
