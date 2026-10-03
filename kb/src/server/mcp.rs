//! model context protocol: json-rpc 2.0 over stdio (`kb mcp`) and over http (`POST /mcp`,
//! streamable-http shaped but without sse -- one request body in, one response body out).
//! design.md §18 ("mcp surface: search, get, refs, propose, run") plus the `answer` tool
//! (design.md §19, §27), backed by `crate::answer` (`src/answer/`).
//!
//! framing for the stdio transport lives in `server::framing`; this module is the json-rpc
//! method dispatch and tool catalog, shared by both transports.

use super::framing::{self, Framing};
use super::handlers;
use crate::config::Config;
use crate::index::SearchOpts;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

#[derive(Debug, Deserialize)]
struct RpcRequest {
	#[serde(default)]
	id: Option<Value>,
	method: String,
	#[serde(default)]
	params: Option<Value>,
}

fn ok_response(id: Value, result: Value) -> Value {
	json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err_response(id: Value, code: i64, message: &str) -> Value {
	json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// one json-rpc request in, at most one response out. `None` for a notification (no `id` on the
/// request -- per the json-rpc spec a notification never gets a reply, whatever its method) and
/// for `notifications/initialized` specifically, which the mcp protocol defines as reply-less
/// even in the unlikely case a client sent it with an `id`. a malformed body still gets a
/// `-32700 parse error` response, since the client needs to know something went wrong even
/// though there is no `id` to correlate it with.
pub fn handle_message(cfg: &Config, principal: &str, reader_groups: &[String], raw: &str) -> Option<String> {
	let request: RpcRequest = match serde_json::from_str(raw) {
		Ok(r) => r,
		Err(e) => return Some(err_response(Value::Null, -32700, &format!("parse error: {e}")).to_string()),
	};
	if request.method == "notifications/initialized" {
		return None;
	}
	let is_notification = request.id.is_none();
	let id = request.id.clone().unwrap_or(Value::Null);
	let response = match request.method.as_str() {
		"initialize" => ok_response(id, initialize_result()),
		"ping" => ok_response(id, json!({})),
		"tools/list" => ok_response(id, json!({"tools": tool_defs()})),
		"tools/call" => match handle_tools_call(cfg, principal, reader_groups, request.params) {
			Ok(v) => ok_response(id, v),
			Err(e) => err_response(id, -32000, &format!("{e:#}")),
		},
		other => err_response(id, -32601, &format!("method not found: {other}")),
	};
	if is_notification { None } else { Some(response.to_string()) }
}

fn initialize_result() -> Value {
	json!({
		"protocolVersion": "2025-06-18",
		"serverInfo": {"name": "kb", "version": env!("CARGO_PKG_VERSION")},
		"capabilities": {"tools": {}},
	})
}

fn tool_defs() -> Value {
	json!([
		{
			"name": "search",
			"description": "full-text search over canon and draft documents (design.md §18)",
			"inputSchema": {
				"type": "object",
				"properties": {
					"query": {"type": "string"},
					"limit": {"type": "integer", "minimum": 1},
					"include_captures": {"type": "boolean"},
					"status_any": {"type": "boolean"},
					"mode": {"type": "string", "enum": ["fts", "hybrid", "vec"]},
				},
				"required": ["query"],
			},
		},
		{
			"name": "get",
			"description": "frontmatter, derived block, body, and refs for one document or capture id",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
		},
		{
			"name": "refs",
			"description": "walk the reference graph from one document, outgoing or incoming, up to `depth` hops",
			"inputSchema": {
				"type": "object",
				"properties": {
					"id": {"type": "string"},
					"incoming": {"type": "boolean"},
					"depth": {"type": "integer", "minimum": 1},
				},
				"required": ["id"],
			},
		},
		{
			"name": "propose",
			"description": "curate a free-text proposal or a set of captures into operations inside a session",
			"inputSchema": {
				"type": "object",
				"properties": {
					"task": {"type": "string"},
					"text": {"type": "string"},
					"capture_ids": {"type": "array", "items": {"type": "string"}},
					"dry_run": {"type": "boolean"},
				},
				"required": ["task"],
			},
		},
		{
			"name": "run",
			"description": "a run log's event timeline, or a summary and a withheld count when the caller lacks access to the documents it touched",
			"inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
		},
		{
			"name": "answer",
			"description": "search, answer, and cite over the top matches for a question (design.md §27)",
			"inputSchema": {
				"type": "object",
				"properties": {"question": {"type": "string"}, "limit": {"type": "integer", "minimum": 1}},
				"required": ["question"],
			},
		},
	])
}

fn handle_tools_call(cfg: &Config, principal: &str, reader_groups: &[String], params: Option<Value>) -> Result<Value> {
	let params = params.context("tools/call requires params")?;
	let name = params.get("name").and_then(Value::as_str).context("params.name is required")?;
	let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
	let result = call_tool(cfg, principal, reader_groups, name, &args)?;
	let text = serde_json::to_string(&result)?;
	Ok(json!({"content": [{"type": "text", "text": text}]}))
}

/// run one tool and record the call, arguments and result in full, in the transcripts table.
fn call_tool(cfg: &Config, principal: &str, reader_groups: &[String], name: &str, args: &Value) -> Result<Value> {
	let started = crate::clock::now();
	let clock = std::time::Instant::now();
	let result = dispatch_tool(cfg, principal, reader_groups, name, args);
	let entry = crate::transcript::Entry {
		kind: "mcp",
		role: name,
		provider: None,
		model: None,
		actor: &format!("agent:mcp@{principal}"),
		request: args.clone(),
		response: result.as_ref().ok().cloned(),
		error: result.as_ref().err().map(|e| format!("{e:#}")),
		started,
		duration_ms: clock.elapsed().as_millis() as u64,
		input_tokens: None,
		output_tokens: None,
	};
	if let Err(e) = crate::transcript::record(&cfg.state_dir(), entry) {
		eprintln!("warning: could not record the mcp `{name}` transcript: {e:#}");
	}
	result
}

fn dispatch_tool(cfg: &Config, principal: &str, reader_groups: &[String], name: &str, args: &Value) -> Result<Value> {
	let reader = handlers::Reader::new(cfg, principal);
	match name {
		"search" => {
			let query = args.get("query").and_then(Value::as_str).context("search requires `query`")?;
			let opts = SearchOpts {
				limit: args.get("limit").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(5),
				status_any: args.get("status_any").and_then(Value::as_bool).unwrap_or(false),
				include_captures: args.get("include_captures").and_then(Value::as_bool).unwrap_or(false),
				mode: handlers::parse_search_mode(args.get("mode").and_then(Value::as_str)),
				explain: args.get("explain").and_then(Value::as_bool).unwrap_or(false),
			};
			handlers::search(cfg, &reader, query, &opts)
		}
		"get" => {
			let id = args.get("id").and_then(Value::as_str).context("get requires `id`")?;
			Ok(handlers::get(cfg, &reader, id)?.unwrap_or_else(|| json!({"error": format!("no document matches `{id}`")})))
		}
		"refs" => {
			let id = args.get("id").and_then(Value::as_str).context("refs requires `id`")?;
			let incoming = args.get("incoming").and_then(Value::as_bool).unwrap_or(false);
			let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(1) as u32;
			handlers::refs(cfg, &reader, id, incoming, depth)
		}
		"propose" => {
			let task = args.get("task").and_then(Value::as_str).context("propose requires `task`")?;
			let text = args.get("text").and_then(Value::as_str).map(str::to_string);
			let capture_ids: Vec<String> = args
				.get("capture_ids")
				.and_then(Value::as_array)
				.map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
				.unwrap_or_default();
			let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
			handlers::propose(cfg, principal, task, text, capture_ids, dry_run)
		}
		"run" => {
			let id = args.get("id").and_then(Value::as_str).context("run requires `id`")?;
			handlers::run_show(cfg, reader_groups, id)
		}
		"answer" => {
			let question = args.get("question").and_then(Value::as_str).context("answer requires `question`")?;
			let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
			handlers::answer(cfg, &reader, question, limit)
		}
		other => bail!("unknown tool `{other}`"),
	}
}

/// `kb mcp`: the stdio transport. framing is detected once from the first bytes on stdin and
/// used for the rest of the session (design.md §29 step 6; see `server::framing`'s doc comment
/// for why detection happens only once).
pub fn run_stdio(cfg: &Config) -> Result<()> {
	let principal = super::auth::local_principal(cfg);
	let reader_groups = cfg.groups_of(&principal);
	let stdin = std::io::stdin();
	let mut reader = stdin.lock();
	let framing = framing::detect_framing(&mut reader)?;
	let stdout = std::io::stdout();
	let mut writer = stdout.lock();
	run_stdio_with(cfg, &principal, &reader_groups, &mut reader, &mut writer, framing)
}

/// the loop `run_stdio` drives, taking the reader/writer/framing directly so tests can exercise
/// it against an in-memory buffer instead of real stdio.
pub fn run_stdio_with(
	cfg: &Config,
	principal: &str,
	reader_groups: &[String],
	reader: &mut impl BufRead,
	writer: &mut impl Write,
	framing: Framing,
) -> Result<()> {
	loop {
		let Some(msg) = framing::read_message(reader, framing)? else { break };
		if let Some(reply) = handle_message(cfg, principal, reader_groups, &msg) {
			framing::write_message(writer, framing, &reply)?;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::Config;

	fn seeded_cfg() -> (tempfile::TempDir, Config) {
		let dir = tempfile::tempdir().unwrap();
		crate::cli::init::init(dir.path(), false, false).unwrap();
		crate::git::run(dir.path(), &["config", "user.name", "test"]).unwrap();
		crate::git::run(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
		std::fs::write(
			dir.path().join("docs/01K5A7Q2XW3ZRCM8T6YB4VJH0A.md"),
			"+++\nid = \"01K5A7Q2XW3ZRCM8T6YB4VJH0A\"\nlayer = \"reference\"\ntitle = \"install steps\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# install steps\n\nrun the installer on linux.\n",
		)
		.unwrap();
		crate::git::run(dir.path(), &["add", "-A"]).unwrap();
		crate::git::commit_all(dir.path(), "seed").unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		crate::index::build(&cfg, None).unwrap();
		(dir, cfg)
	}

	#[test]
	fn initialize_returns_protocol_version_and_capabilities() {
		let (_dir, cfg) = seeded_cfg();
		let reply = handle_message(&cfg, "caleb", &[], r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap();
		let v: Value = serde_json::from_str(&reply).unwrap();
		assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
		assert_eq!(v["result"]["serverInfo"]["name"], "kb");
		assert!(v["result"]["capabilities"]["tools"].is_object());
	}

	#[test]
	fn notifications_initialized_gets_no_reply() {
		let (_dir, cfg) = seeded_cfg();
		let reply = handle_message(&cfg, "caleb", &[], r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
		assert!(reply.is_none());
	}

	#[test]
	fn tools_list_names_all_six_tools() {
		let (_dir, cfg) = seeded_cfg();
		let reply = handle_message(&cfg, "caleb", &[], r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
		let v: Value = serde_json::from_str(&reply).unwrap();
		let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
		for expected in ["search", "get", "refs", "propose", "run", "answer"] {
			assert!(names.contains(&expected), "missing tool `{expected}` in {names:?}");
		}
	}

	#[test]
	fn tools_call_search_finds_the_seeded_document() {
		let (_dir, cfg) = seeded_cfg();
		let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"installer"}}}"#;
		let reply = handle_message(&cfg, "caleb", &[], req).unwrap();
		let v: Value = serde_json::from_str(&reply).unwrap();
		let text = v["result"]["content"][0]["text"].as_str().unwrap();
		let hits: Value = serde_json::from_str(text).unwrap();
		assert!(hits.as_array().unwrap().iter().any(|h| h["doc_id"] == "01K5A7Q2XW3ZRCM8T6YB4VJH0A"), "{hits}");
	}

	#[test]
	fn unknown_method_is_method_not_found() {
		let (_dir, cfg) = seeded_cfg();
		let reply = handle_message(&cfg, "caleb", &[], r#"{"jsonrpc":"2.0","id":4,"method":"bogus"}"#).unwrap();
		let v: Value = serde_json::from_str(&reply).unwrap();
		assert_eq!(v["error"]["code"], -32601);
	}

	#[test]
	fn malformed_json_is_parse_error() {
		let (_dir, cfg) = seeded_cfg();
		let reply = handle_message(&cfg, "caleb", &[], "{not json").unwrap();
		let v: Value = serde_json::from_str(&reply).unwrap();
		assert_eq!(v["error"]["code"], -32700);
	}

	#[test]
	fn ping_replies_with_empty_result() {
		let (_dir, cfg) = seeded_cfg();
		let reply = handle_message(&cfg, "caleb", &[], r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#).unwrap();
		let v: Value = serde_json::from_str(&reply).unwrap();
		assert_eq!(v["result"], json!({}));
	}
}
