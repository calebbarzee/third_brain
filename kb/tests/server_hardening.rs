//! regression tests for the 2026-09-28 audit's server findings: `/blob` path traversal (P1-1), a
//! multibyte blob address hanging workers (P1-2), cross-site requests under `--no-auth` (P1-4),
//! and unbounded request bodies (P1-5). requests go over a raw tcp socket so headers and percent
//! encodings reach the server exactly as written.

use kb::config::Config;
use kb::server::{self, ServeOpts};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn start_no_auth() -> (tempfile::TempDir, server::ServerHandle) {
    let dir = tempfile::tempdir().unwrap();
    kb::cli::init::init(dir.path(), false, false).unwrap();
    kb::git::run(dir.path(), &["config", "user.name", "test user"]).unwrap();
    kb::git::run(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
    std::fs::create_dir_all(dir.path().join(".kb/store/blobs")).unwrap();
    let cfg = Config::load(dir.path()).unwrap();
    let handle = server::start(cfg, ServeOpts { bind: "127.0.0.1:0".to_string(), no_auth: true, tick_seconds: 0 }).unwrap();
    (dir, handle)
}

/// send `head` (request line plus headers, no trailing blank line) and `body`; return the status
/// code and the response text.
fn raw(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut request = format!("{head}\r\nConnection: close\r\n\r\n").into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request).unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, text)
}

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}", addr.port()), b"")
}

#[test]
fn blob_paths_cannot_leave_the_store() {
    let (dir, handle) = start_no_auth();
    let addr = handle.addr();
    std::fs::write(dir.path().join(".kb/secret.txt"), "do not serve").unwrap();
    for path in ["/blob/%2Fetc%2Fhostname", "/blob/..%2Fsecret.txt", "/blob/..%2F..%2Fkb.toml", "/blob/..%2F..%2F.kb%2Fsecret.txt"] {
        let (status, text) = get(addr, path);
        assert_eq!(status, 404, "{path}: {text}");
        assert!(!text.contains("do not serve") && !text.contains("[store]"), "{path} leaked a file");
    }
    handle.shutdown();
}

#[test]
fn multibyte_blob_addresses_do_not_exhaust_workers() {
    let (_dir, handle) = start_no_auth();
    let addr = handle.addr();
    for _ in 0..12 {
        let (status, _) = get(addr, "/blob/%E2%82%ACabc");
        assert_eq!(status, 404);
    }
    assert_eq!(get(addr, "/health").0, 200, "server stopped answering");
    handle.shutdown();
}

#[test]
fn no_auth_refuses_cross_site_requests() {
    let (_dir, handle) = start_no_auth();
    let addr = handle.addr();
    let port = addr.port();

    let (status, _) = raw(addr, "GET /health HTTP/1.1\r\nHost: evil.example", b"");
    assert_eq!(status, 403, "a rebound dns name must be refused");

    let (status, _) = raw(addr, &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example"), b"");
    assert_eq!(status, 403, "a foreign origin must be refused");

    let body = br#"{"task":"t","text":"x","dry_run":true}"#;
    let (status, _) = raw(addr, &format!("POST /propose HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: text/plain\r\nContent-Length: {}", body.len()), body);
    assert_eq!(status, 403, "a simple cross-site form post must be refused");

    let (status, _) = raw(addr, &format!("GET /health HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}"), b"");
    assert_eq!(status, 200);
    let (status, _) = raw(addr, &format!("GET /health HTTP/1.1\r\nHost: [::1]:{port}"), b"");
    assert_eq!(status, 200);
    handle.shutdown();
}

#[test]
fn oversized_bodies_are_refused() {
    let (_dir, handle) = start_no_auth();
    let addr = handle.addr();
    let (status, _) = raw(
        addr,
        &format!("POST /webhook/source/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}", addr.port(), 50 * 1024 * 1024),
        b"",
    );
    assert_eq!(status, 413);
    handle.shutdown();
}

/// audit P1-3: with principals listed, a document whose `access.read` names a group is visible
/// only to that group's members, on every read path.
#[test]
fn access_read_is_enforced_for_search_get_and_docs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    kb::cli::init::init(root, false, false).unwrap();
    kb::git::run(root, &["config", "user.name", "test user"]).unwrap();
    kb::git::run(root, &["config", "user.email", "caleb@example.com"]).unwrap();
    std::fs::write(
        root.join("groups.toml"),
        "[principals.caleb]\nkind = \"human\"\nemails = [\"caleb@example.com\"]\ngroups = [\"secret-team\"]\n\n[principals.ana]\nkind = \"human\"\nemails = [\"ana@example.com\"]\ngroups = []\n\n[groups.secret-team]\nmembers = [\"caleb\"]\n",
    )
    .unwrap();
    let secret = "01K5A7Q2XW3ZRCM8T6YB4VJH5S";
    let public = "01K5A7Q2XW3ZRCM8T6YB4VJH5P";
    std::fs::write(
        root.join("docs").join(format!("{secret}.md")),
        format!("+++\nid = \"{secret}\"\nlayer = \"reference\"\ntitle = \"launch codes\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n\n[access]\nread = [\"secret-team\"]\nwrite = [\"owner\"]\n+++\n\n# launch codes\n\nzebra protocol details.\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("docs").join(format!("{public}.md")),
        format!("+++\nid = \"{public}\"\nlayer = \"reference\"\ntitle = \"zebra overview\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n# zebra overview\n\nzebra public notes.\n"),
    )
    .unwrap();
    kb::git::commit_all(root, "seed").unwrap();
    let cfg = Config::load(root).unwrap();
    kb::index::build(&cfg, None).unwrap();
    let handle = server::start(cfg.clone(), ServeOpts { bind: "127.0.0.1:0".to_string(), no_auth: false, tick_seconds: 0 }).unwrap();
    let addr = handle.addr();
    let ana = kb::server::auth::issue(&cfg, "ana").unwrap();
    let caleb = kb::server::auth::issue(&cfg, "caleb").unwrap();
    let as_user = |token: &str, path: &str| raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}", addr.port()), b"");

    let (status, body) = as_user(&ana, "/search?q=zebra");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(public) && !body.contains(secret), "ana's search leaked the restricted document:\n{body}");
    assert_eq!(as_user(&ana, &format!("/doc/{secret}")).0, 404);
    assert!(!as_user(&ana, "/docs").1.contains(secret));

    let (_, body) = as_user(&caleb, "/search?q=zebra");
    assert!(body.contains(secret), "a group member must see the restricted document:\n{body}");
    assert_eq!(as_user(&caleb, &format!("/doc/{secret}")).0, 200);
    handle.shutdown();
}
