//! every model call and every mcp tool call is stored in full in `state.sqlite`'s `transcripts`
//! table, with the trigger that started it. the model here is the `command` provider kind running
//! a shell script that prints a canned classify reply, so no live model is called.

use assert_cmd::Command;
use std::path::Path;

fn kb_at(repo: &Path) -> Command {
	let mut c = Command::cargo_bin("kb").unwrap();
	c.current_dir(repo).arg("--repo").arg(repo);
	c
}

fn repo_with_command_classifier() -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	let repo = dir.path();
	Command::cargo_bin("kb").unwrap().arg("init").arg(repo).arg("--no-hook").assert().success();
	kb::git::run(repo, &["config", "user.name", "test user"]).unwrap();
	kb::git::run(repo, &["config", "user.email", "test@example.com"]).unwrap();
	let script = repo.join(".kb-test-classify.sh");
	std::fs::write(&script, "#!/bin/sh\ncat > /dev/null\necho '{\"layer\": \"procedure\", \"tags\": [\"network\", \"jetson\"], \"confidence\": 0.9, \"reason\": \"network setup steps\"}'\n").unwrap();
	let mut perms = std::fs::metadata(&script).unwrap().permissions();
	std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
	std::fs::set_permissions(&script, perms).unwrap();
	let mut toml = std::fs::read_to_string(repo.join("kb.toml")).unwrap();
	toml.push_str(&format!("\n[providers.classify]\nkind = \"command\"\nbase_url = \"{}\"\n", script.display()));
	std::fs::write(repo.join("kb.toml"), toml).unwrap();
	kb::git::commit_all(repo, "init").unwrap();
	dir
}

#[test]
fn a_model_call_and_an_mcp_tool_call_are_both_transcribed() {
	let dir = repo_with_command_classifier();
	let repo = dir.path();
	std::fs::write(repo.join("docs/net.md"), "# static ip on the jetson\n\nuse nmcli to set 192.168.1.20.\n").unwrap();

	kb_at(repo).env("KB_TRIGGER", "hook:pre-commit").args(["check", "validate", "--fix", "docs/net.md"]).assert().success();
	kb::git::commit_all(repo, "add note").unwrap();
	kb_at(repo).args(["index", "build"]).assert().success();
	let requests = concat!(
		r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
		"\n",
		r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"query":"jetson"}}}"#,
		"\n"
	);
	kb_at(repo).arg("mcp").write_stdin(requests).assert().success();

	let out = kb_at(repo).args(["--json", "transcript", "list"]).output().unwrap();
	let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
	let rows = rows.as_array().unwrap();

	let classify = rows.iter().find(|r| r["role"] == "classify").expect("classify call transcribed");
	assert_eq!(classify["kind"], "llm");
	assert_eq!(classify["trigger"], "hook:pre-commit");
	let sent = classify["request"]["messages"][0]["content"].as_str().unwrap();
	assert!(sent.contains("nmcli to set 192.168.1.20"), "the full note body is in the request: {sent}");
	let reply = serde_json::to_string(&classify["response"]).unwrap();
	assert!(reply.contains("jetson"), "the full reply is stored: {reply}");

	let search = rows.iter().find(|r| r["kind"] == "mcp").expect("mcp tool call transcribed");
	assert_eq!(search["role"], "search");
	assert_eq!(search["trigger"], "mcp:stdio");
	assert_eq!(search["request"]["query"], "jetson");
	assert!(serde_json::to_string(&search["response"]).unwrap().contains("static ip"), "search results stored");

	let id = classify["id"].as_str().unwrap();
	let shown = kb_at(repo).args(["transcript", "show", id]).output().unwrap();
	let shown = String::from_utf8_lossy(&shown.stdout);
	assert!(shown.contains("--- user ---") && shown.contains("--- response ---"), "{shown}");
}

/// design.md §6: a classifier that fails every retry leaves the deterministic defaults and never
/// blocks the commit; each failed attempt is still transcribed.
#[test]
fn a_failing_classifier_does_not_block_validation() {
	let dir = repo_with_command_classifier();
	let repo = dir.path();
	std::fs::write(repo.join(".kb-test-classify.sh"), "#!/bin/sh\ncat > /dev/null\necho 'not json'\n").unwrap();
	std::fs::write(repo.join("docs/x.md"), "# a note\n\nbody.\n").unwrap();

	let out = kb_at(repo).env("KB_TRIGGER", "hook:pre-commit").args(["check", "validate", "--fix", "docs/x.md"]).output().unwrap();

	assert!(out.status.success(), "validate failed: {}", String::from_utf8_lossy(&out.stderr));
	let shown = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
	assert!(shown.contains("classify-failed"), "{shown}");
	let rows = kb_at(repo).args(["--json", "transcript", "list", "--role", "classify"]).output().unwrap();
	let rows: serde_json::Value = serde_json::from_slice(&rows.stdout).unwrap();
	assert!(rows.as_array().unwrap().len() >= 2, "every attempt is transcribed: {rows}");
}
