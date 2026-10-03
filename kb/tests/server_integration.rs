//! end-to-end test of the http server: a temp kb repo with 4 documents and a built index, `kb
//! serve` started in-process on an ephemeral port, then real http requests (`reqwest::blocking`,
//! already a dependency) against health, search, doc, docs, review, mcp (over http), auth
//! failure modes, the run-log permission check, and the source webhook. design.md §18, §16, §19,
//! §23, and this crate's build-step task ("start `kb serve` in-process on an ephemeral port").

use kb::config::Config;
use kb::git;
use kb::runlog::{RunLog, RunStart, RunStatus};
use kb::server::{self, ServeOpts, auth};
use std::path::Path;

const ID_A: &str = "01HZZZZZZZ0000000000000SA1";
const ID_B: &str = "01HZZZZZZZ0000000000000SA2";
const ID_C: &str = "01HZZZZZZZ0000000000000SA3";
const ID_D: &str = "01HZZZZZZZ0000000000000SA4";

fn write_doc(root: &Path, id: &str, layer: &str, status: &str, title: &str, body: &str) {
	let text = format!(
		"+++\nid = \"{id}\"\nlayer = \"{layer}\"\ntitle = \"{title}\"\nstatus = \"{status}\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# {title}\n\n{body}\n"
	);
	std::fs::write(root.join("docs").join(format!("{id}.md")), text).unwrap();
}

/// a repo with the init skeleton, 4 committed documents, and a built index.
fn seed_repo() -> (tempfile::TempDir, Config) {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path();
	kb::cli::init::init(root, false, false).unwrap();
	git::run(root, &["config", "user.name", "test user"]).unwrap();
	git::run(root, &["config", "user.email", "caleb@example.com"]).unwrap();

	write_doc(root, ID_A, "reference", "canon", "install steps", "run the installer on linux.");
	write_doc(root, ID_B, "reference", "canon", "root guide", "start here for an overview.");
	write_doc(root, ID_C, "procedure", "canon", "rotation schedule", "rotate the credentials monthly.");
	write_doc(root, ID_D, "reference", "draft", "orphan draft", "not referenced by anything yet.");
	git::commit_all(root, "seed 4 documents").unwrap();

	let cfg = Config::load(root).unwrap();
	kb::index::build(&cfg, None).unwrap();
	(dir, cfg)
}

fn start_server(cfg: &Config) -> server::ServerHandle {
	server::start(cfg.clone(), ServeOpts { bind: "127.0.0.1:0".to_string(), no_auth: false, ..ServeOpts::default() }).unwrap()
}

fn client() -> reqwest::blocking::Client {
	reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap()
}

#[test]
fn health_search_doc_docs_review_and_mcp_over_http() {
	let (_dir, cfg) = seed_repo();
	let handle = start_server(&cfg);
	let base = format!("http://{}", handle.addr());
	let token = auth::issue(&cfg, "caleb").unwrap();
	let c = client();

	// --- GET /health ---
	let resp = c.get(format!("{base}/health")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);
	let body: serde_json::Value = resp.json().unwrap();
	assert_eq!(body["status"], "ok");

	// --- GET /search ---
	let resp = c.get(format!("{base}/search?q=installer")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);
	let hits: serde_json::Value = resp.json().unwrap();
	assert!(hits.as_array().unwrap().iter().any(|h| h["doc_id"] == ID_A), "{hits}");

	// --- GET /doc/<id> ---
	let resp = c.get(format!("{base}/doc/{ID_A}")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);
	let doc: serde_json::Value = resp.json().unwrap();
	assert_eq!(doc["id"], ID_A);

	// a doc that does not exist is a 404, not a 500.
	let resp = c.get(format!("{base}/doc/01HZZZZZZZ0000000000000000")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 404);

	// --- GET /doc/<id>/refs ---
	let resp = c.get(format!("{base}/doc/{ID_A}/refs")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);

	// --- GET /docs ---
	let resp = c.get(format!("{base}/docs?status=canon")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);
	let rows: serde_json::Value = resp.json().unwrap();
	let ids: Vec<&str> = rows.as_array().unwrap().iter().filter_map(|r| r["id"].as_str()).collect();
	assert!(ids.contains(&ID_A) && ids.contains(&ID_B) && ids.contains(&ID_C), "{rows}");
	assert!(!ids.contains(&ID_D), "the draft should not show up under status=canon: {rows}");

	// --- GET /review (empty queue, but the route itself must work) ---
	let resp = c.get(format!("{base}/review")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);

	// --- POST /mcp: initialize ---
	let resp = c
		.post(format!("{base}/mcp"))
		.bearer_auth(&token)
		.json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
		.send()
		.unwrap();
	assert_eq!(resp.status(), 200);
	let rpc: serde_json::Value = resp.json().unwrap();
	assert_eq!(rpc["result"]["protocolVersion"], "2025-06-18");

	// --- POST /mcp: tools/call search ---
	let resp = c
		.post(format!("{base}/mcp"))
		.bearer_auth(&token)
		.json(&serde_json::json!({
			"jsonrpc": "2.0", "id": 2, "method": "tools/call",
			"params": {"name": "search", "arguments": {"query": "rotation"}},
		}))
		.send()
		.unwrap();
	assert_eq!(resp.status(), 200);
	let rpc: serde_json::Value = resp.json().unwrap();
	let text = rpc["result"]["content"][0]["text"].as_str().unwrap();
	let hits: serde_json::Value = serde_json::from_str(text).unwrap();
	assert!(hits.as_array().unwrap().iter().any(|h| h["doc_id"] == ID_C), "{hits}");

	handle.shutdown();
}

#[test]
fn unauthenticated_and_revoked_tokens_are_401() {
	let (_dir, cfg) = seed_repo();
	let handle = start_server(&cfg);
	let base = format!("http://{}", handle.addr());
	let c = client();

	// no Authorization header at all.
	let resp = c.get(format!("{base}/health")).send().unwrap();
	assert_eq!(resp.status(), 401);

	// a bogus bearer token.
	let resp = c.get(format!("{base}/health")).bearer_auth("not-a-real-token").send().unwrap();
	assert_eq!(resp.status(), 401);

	// a real token, then revoked.
	let token = auth::issue(&cfg, "temp-user").unwrap();
	let resp = c.get(format!("{base}/health")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);
	auth::revoke(&cfg, "temp-user").unwrap();
	let resp = c.get(format!("{base}/health")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 401);

	handle.shutdown();
}

#[test]
fn run_access_control_withholds_documents_the_caller_cannot_read() {
	let (_dir, cfg) = seed_repo();
	let handle = start_server(&cfg);
	let base = format!("http://{}", handle.addr());
	let token = auth::issue(&cfg, "caleb").unwrap();
	let c = client();

	// a run whose touched documents are restricted to a group "caleb" (with no groups.toml
	// entry) does not belong to.
	let mut log = RunLog::create(&cfg, RunStart { actor: "agent:curate@svc".into(), principal: Some("svc".into()), ..Default::default() }).unwrap();
	let run_id = log.run_id().to_string();
	log.op(kb::model::Operation::Append, ID_A, None, None, Some("afterhash"), None).unwrap();
	log.run_end(RunStatus::Ok, None, None, None, &[], &["restricted-group".to_string()]).unwrap();
	log.seal(&cfg).unwrap();

	let resp = c.get(format!("{base}/run/{run_id}")).bearer_auth(&token).send().unwrap();
	assert_eq!(resp.status(), 200);
	let body: serde_json::Value = resp.json().unwrap();
	assert!(body.get("events").is_none(), "events should be withheld: {body}");
	assert_eq!(body["withheld_documents"], 1);
	assert_eq!(body["status"], "ok");

	handle.shutdown();
}

#[test]
fn webhook_source_stores_a_capture() {
	let (_dir, cfg) = seed_repo();
	let handle = start_server(&cfg);
	let base = format!("http://{}", handle.addr());
	let token = auth::issue(&cfg, "caleb").unwrap();
	let c = client();

	let resp = c
		.post(format!("{base}/webhook/source/test"))
		.bearer_auth(&token)
		.header("content-type", "application/json")
		.body(r#"{"hello": "world"}"#)
		.send()
		.unwrap();
	assert_eq!(resp.status(), 200);
	let body: serde_json::Value = resp.json().unwrap();
	assert_eq!(body["source"], "webhook:test");

	let captures = kb::store::catalog::list(&cfg, None, Some("webhook:test"), 10).unwrap();
	assert_eq!(captures.len(), 1);
	assert_eq!(captures[0].media_type, "application/json");

	handle.shutdown();
}

#[test]
fn no_auth_mode_requires_a_loopback_bind() {
	let (_dir, cfg) = seed_repo();
	let opts = ServeOpts { bind: "0.0.0.0:0".to_string(), no_auth: true, ..ServeOpts::default() };
	assert!(server::start(cfg, opts).is_err());
}
