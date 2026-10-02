//! the rest surface: design.md §18 ("mcp surface: search, get, refs, propose, run. rest exposes
//! the same five plus `GET /blob/<sha256>`"), §19 (webhooks), §22 (the tick), §16 (run
//! permissions). every response body is json; `GET /blob/<sha256>` is the one exception, which
//! streams the recorded media type.
//!
//! route handling reads the request, calls into `server::handlers` (shared with `server::mcp`),
//! and shapes the http-specific envelope (status codes, query strings, the blob content type).
//! `server::mod` owns the thread pool and per-request logging that call `handle` below.
//!
//! `tiny_http::Request::respond` consumes `self`, so every handler here takes `Request` by value
//! and is the one place that finally calls `respond`; nothing hands a request back out.

use super::auth::AuthMode;
use super::handlers;
use crate::config::Config;
use crate::index::{LsFilter, SearchOpts};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tiny_http::{Header, Request, Response};

/// handle one request end to end: authenticate, route, respond. returns `(status, principal)`
/// for the caller's one-line-per-request log (`"-"` when authentication itself failed).
pub fn handle(cfg: &Config, mode: &AuthMode, request: Request) -> (u16, String) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let (path, query) = split_query(&url);

    if request.body_length().is_some_and(|n| n > MAX_BODY_BYTES) {
        let status = respond_json(request, 413, json!({"error": format!("request body over {MAX_BODY_BYTES} bytes")}));
        return (status, "-".to_string());
    }
    if let AuthMode::NoAuth(_) = mode
        && let Err(reason) = check_loopback_request(&method, &request)
    {
        let status = respond_json(request, 403, json!({"error": reason}));
        return (status, "-".to_string());
    }

    let principal = match authenticate(cfg, mode, &request) {
        Some(p) => p,
        None => {
            let status = respond_json(request, 401, json!({"error": "unauthorized"}));
            return (status, "-".to_string());
        }
    };

    let status = route(cfg, &principal, &method, &path, &query, request);
    (status, principal)
}

/// largest request body read; a larger declared body is refused with 413, and a chunked body is
/// cut at this size (and then fails to parse).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

/// the host part of a `Host` header or an origin url: `localhost:7777` and `http://localhost:7777`
/// both give `localhost`; `[::1]:7777` gives `[::1]`.
fn host_part(value: &str) -> &str {
    let rest = value.split_once("://").map_or(value, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or(authority)
    } else {
        authority.split(':').next().unwrap_or(authority)
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// `--no-auth` serves anyone who can reach 127.0.0.1, which includes web pages in the user's own
/// browser. a page cannot forge `Host` past dns rebinding checks or send `Content-Type:
/// application/json` cross-origin without a preflight this server never answers, so: the `Host`
/// must be loopback, an `Origin`, when sent, must be loopback, and a POST must be json.
fn check_loopback_request(method: &str, request: &Request) -> std::result::Result<(), String> {
    let host = header(request, "host").ok_or("missing Host header")?;
    if !is_loopback_host(host_part(host)) {
        return Err(format!("Host `{host}` is not a loopback name; --no-auth serves only localhost"));
    }
    if let Some(origin) = header(request, "origin")
        && !is_loopback_host(host_part(origin))
    {
        return Err(format!("Origin `{origin}` is not a loopback origin"));
    }
    if method == "POST" && !header(request, "content-type").is_some_and(|c| c.trim_start().to_ascii_lowercase().starts_with("application/json")) {
        return Err("POST requires Content-Type: application/json".to_string());
    }
    Ok(())
}

/// `Authorization: Bearer <token>` resolved through `auth::verify`, or (in `--no-auth` mode) the
/// fixed local git-identity principal for every request, no header required.
fn authenticate(cfg: &Config, mode: &AuthMode, request: &Request) -> Option<String> {
    match mode {
        AuthMode::NoAuth(principal) => Some(principal.clone()),
        AuthMode::Token => {
            let header = request.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("authorization"))?;
            let token = header.value.as_str().strip_prefix("Bearer ")?;
            super::auth::verify(cfg, token).ok().flatten()
        }
    }
}

fn route(cfg: &Config, principal: &str, method: &str, path: &str, query: &HashMap<String, String>, request: Request) -> u16 {
    let decoded: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(percent_decode).collect();
    let seg: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let reader = handlers::Reader::new(cfg, principal);
    match (method, seg.as_slice()) {
        ("GET", ["health"]) => respond_json(request, 200, json!({"status": "ok"})),
        ("GET", ["search"]) => respond_search(cfg, &reader, query, request),
        ("GET", ["doc", id]) => respond_get(cfg, &reader, id, request),
        ("GET", ["doc", id, "refs"]) => respond_refs(cfg, &reader, id, query, request),
        ("GET", ["docs"]) => respond_docs(cfg, &reader, query, request),
        ("POST", ["propose"]) => respond_propose(cfg, principal, request),
        ("POST", ["answer"]) => respond_answer(cfg, &reader, request),
        ("GET", ["runs"]) => respond_runs(cfg, query, request),
        ("GET", ["run", id]) => respond_run(cfg, principal, id, request),
        ("GET", ["blob", sha]) => respond_blob(cfg, &reader, sha, request),
        ("GET", ["review"]) => respond_review(cfg, request),
        ("POST", ["webhook", "main-advanced"]) => respond_webhook_tick(cfg, request),
        ("POST", ["webhook", "source", name]) => respond_webhook_source(cfg, name, request),
        ("POST", ["mcp"]) => respond_mcp(cfg, principal, request),
        _ => respond_json(request, 404, json!({"error": format!("no route for {method} {path}")})),
    }
}

fn respond_search(cfg: &Config, reader: &handlers::Reader, query: &HashMap<String, String>, request: Request) -> u16 {
    let Some(q) = query.get("q") else {
        return respond_json(request, 400, json!({"error": "missing required query parameter `q`"}));
    };
    let opts = SearchOpts {
        limit: query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(5),
        status_any: query.get("status_any").map(|v| v == "true" || v == "1").unwrap_or(false),
        include_captures: query.get("include_captures").map(|v| v == "true" || v == "1").unwrap_or(false),
        mode: handlers::parse_search_mode(query.get("mode").map(String::as_str)),
        explain: query.get("explain").map(|v| v == "true" || v == "1").unwrap_or(false),
    };
    match handlers::search(cfg, reader, q, &opts) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 500, json!({"error": format!("{e:#}")})),
    }
}

fn respond_get(cfg: &Config, reader: &handlers::Reader, id: &str, request: Request) -> u16 {
    match handlers::get(cfg, reader, id) {
        Ok(Some(v)) => respond_json(request, 200, v),
        Ok(None) => respond_json(request, 404, json!({"error": format!("no document matches `{id}`")})),
        Err(e) => respond_json(request, 400, json!({"error": format!("{e:#}")})),
    }
}

fn respond_refs(cfg: &Config, reader: &handlers::Reader, id: &str, query: &HashMap<String, String>, request: Request) -> u16 {
    let incoming = query.get("incoming").map(|v| v == "true" || v == "1").unwrap_or(false);
    let depth = query.get("depth").and_then(|v| v.parse().ok()).unwrap_or(1);
    match handlers::refs(cfg, reader, id, incoming, depth) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 400, json!({"error": format!("{e:#}")})),
    }
}

fn respond_docs(cfg: &Config, reader: &handlers::Reader, query: &HashMap<String, String>, request: Request) -> u16 {
    let filter =
        LsFilter { layer: query.get("layer").cloned(), status: query.get("status").cloned(), tag: query.get("tag").cloned(), kind: query.get("kind").cloned() };
    match handlers::docs_list(cfg, reader, &filter) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 500, json!({"error": format!("{e:#}")})),
    }
}

#[derive(serde::Deserialize)]
struct ProposeBody {
    task: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    capture_ids: Vec<String>,
    #[serde(default)]
    dry_run: bool,
}

fn respond_propose(cfg: &Config, principal: &str, mut request: Request) -> u16 {
    let body = read_body(&mut request);
    let parsed: Result<ProposeBody, _> = serde_json::from_slice(&body);
    match parsed {
        Ok(b) => match handlers::propose(cfg, principal, &b.task, b.text, b.capture_ids, b.dry_run) {
            Ok(v) => respond_json(request, 200, v),
            Err(e) => respond_json(request, 400, json!({"error": format!("{e:#}")})),
        },
        Err(e) => respond_json(request, 400, json!({"error": format!("invalid json body: {e}")})),
    }
}

#[derive(serde::Deserialize)]
struct AnswerBody {
    question: String,
    #[serde(default = "default_answer_limit")]
    limit: usize,
}
fn default_answer_limit() -> usize {
    5
}

fn respond_answer(cfg: &Config, reader: &handlers::Reader, mut request: Request) -> u16 {
    let body = read_body(&mut request);
    let parsed: Result<AnswerBody, _> = serde_json::from_slice(&body);
    match parsed {
        Ok(b) => match handlers::answer(cfg, reader, &b.question, b.limit) {
            Ok(v) => respond_json(request, 200, v),
            Err(e) => respond_json(request, 500, json!({"error": format!("{e:#}")})),
        },
        Err(e) => respond_json(request, 400, json!({"error": format!("invalid json body: {e}")})),
    }
}

fn respond_runs(cfg: &Config, query: &HashMap<String, String>, request: Request) -> u16 {
    let since = query.get("since").and_then(|s| parse_duration(s));
    match handlers::runs_list(cfg, since, query.get("status").map(String::as_str)) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 500, json!({"error": format!("{e:#}")})),
    }
}

fn respond_run(cfg: &Config, principal: &str, id: &str, request: Request) -> u16 {
    let reader_groups = cfg.groups_of(principal);
    match handlers::run_show(cfg, &reader_groups, id) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 404, json!({"error": format!("{e:#}")})),
    }
}

fn respond_blob(cfg: &Config, reader: &handlers::Reader, sha: &str, request: Request) -> u16 {
    if !crate::store::blob::is_sha256_hex(sha) || !handlers::blob_readable(cfg, reader, sha).unwrap_or(false) {
        return respond_json(request, 404, json!({"error": format!("no blob `{sha}`")}));
    }
    let store = match crate::store::blob::open_store(cfg) {
        Ok(s) => s,
        Err(e) => return respond_json(request, 500, json!({"error": format!("{e:#}")})),
    };
    match store.get(sha) {
        Ok(Some(bytes)) => {
            let media_type = crate::store::blob::media_type_of(&crate::store::blob::store_root(cfg).join("blobs"), sha)
                .unwrap_or_else(|| "application/octet-stream".to_string());
            respond_bytes(request, 200, bytes, &media_type)
        }
        Ok(None) => respond_json(request, 404, json!({"error": format!("no blob `{sha}`")})),
        Err(e) => respond_json(request, 500, json!({"error": format!("{e:#}")})),
    }
}

fn respond_review(cfg: &Config, request: Request) -> u16 {
    match handlers::review_list(cfg) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 500, json!({"error": format!("{e:#}")})),
    }
}

/// `POST /webhook/main-advanced`: any json body (a git host's push webhook, or a post-receive
/// hook posting here). runs `merge::tick::tick` in a background thread and answers 202 right
/// away rather than waiting for the tick, which may rebuild the index (design.md §22).
fn respond_webhook_tick(cfg: &Config, mut request: Request) -> u16 {
    let _body = read_body(&mut request); // the payload's content does not affect the tick; just drained
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::merge::tick::tick(&cfg) {
            eprintln!("webhook-triggered tick failed: {e:#}");
        }
    });
    respond_json(request, 202, json!({"status": "accepted"}))
}

fn respond_webhook_source(cfg: &Config, name: &str, mut request: Request) -> u16 {
    let body = read_body(&mut request);
    match handlers::webhook_capture(cfg, name, &body) {
        Ok(v) => respond_json(request, 200, v),
        Err(e) => respond_json(request, 400, json!({"error": format!("{e:#}")})),
    }
}

fn respond_mcp(cfg: &Config, principal: &str, mut request: Request) -> u16 {
    let body = read_body(&mut request);
    let raw = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return respond_json(request, 400, json!({"error": "request body was not valid utf-8"})),
    };
    let reader_groups = cfg.groups_of(principal);
    match super::mcp::handle_message(cfg, principal, &reader_groups, raw) {
        Some(reply) => {
            let value: Value = serde_json::from_str(&reply).unwrap_or(Value::Null);
            respond_json(request, 200, value)
        }
        // a notification (e.g. `notifications/initialized`) has no json-rpc reply; the http
        // transport still needs to close the request out, so an empty 204 stands in for it.
        None => respond_empty(request, 204),
    }
}

// ---------------------------------------------------------------------------------------------
// response helpers (each consumes `request`, since `Request::respond` takes `self` by value)
// ---------------------------------------------------------------------------------------------

fn respond_json(request: Request, status: u16, value: Value) -> u16 {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    let response = Response::from_data(body).with_status_code(status).with_header(header);
    let _ = request.respond(response);
    status
}

fn respond_bytes(request: Request, status: u16, bytes: Vec<u8>, content_type: &str) -> u16 {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
    let response = Response::from_data(bytes).with_status_code(status).with_header(header);
    let _ = request.respond(response);
    status
}

fn respond_empty(request: Request, status: u16) -> u16 {
    let response = Response::from_data(Vec::<u8>::new()).with_status_code(status);
    let _ = request.respond(response);
    status
}

fn read_body(request: &mut Request) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut std::io::Read::take(request.as_reader(), MAX_BODY_BYTES as u64), &mut buf);
    buf
}

// ---------------------------------------------------------------------------------------------
// query string parsing (no new dependency: reqwest's client-side encoder is the only percent
// encoder already in the dependency tree, and it is not reachable from the server side)
// ---------------------------------------------------------------------------------------------

fn split_query(raw_url: &str) -> (String, HashMap<String, String>) {
    match raw_url.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (raw_url.to_string(), HashMap::new()),
    }
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() && bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3_600,
        "d" => n * 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_query_separates_path_and_params() {
        let (path, q) = split_query("/search?q=hello%20world&limit=5");
        assert_eq!(path, "/search");
        assert_eq!(q.get("q").map(String::as_str), Some("hello world"));
        assert_eq!(q.get("limit").map(String::as_str), Some("5"));
    }

    #[test]
    fn split_query_with_no_query_string() {
        let (path, q) = split_query("/health");
        assert_eq!(path, "/health");
        assert!(q.is_empty());
    }

    #[test]
    fn percent_decode_handles_plus_and_hex() {
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("24h"), Some(Duration::from_secs(24 * 3_600)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("bogus"), None);
    }
}
