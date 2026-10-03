//! bearer tokens for the http server. design.md §10: "a server keeps sha256 hashes of bearer
//! tokens in a `tokens` table in the state database, issued with `kb token issue <principal>`".
//!
//! a token is 32 random bytes read from `/dev/urandom`, printed once as a 64-character hex
//! string (hex is already a dependency of this crate and is as url-safe as base64url, so no new
//! dependency is added for this). only the sha256 of the token ever touches disk.

use crate::config::Config;
use crate::parse::frontmatter::sha256_hex;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::io::Read;

const SCHEMA: &str = r#"
create table if not exists tokens (
    token_sha256 text primary key,
    principal text not null,
    created_at text not null,
    last_used text,
    revoked integer not null default 0
);
create index if not exists tokens_principal on tokens(principal);
"#;

/// open (creating if needed) `<state>/state.sqlite` and make sure the `tokens` table exists.
/// shares the file with `store::catalog`'s own tables; each module only touches its own table.
pub fn open(cfg: &Config) -> Result<Connection> {
	let dir = cfg.state_dir();
	std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
	let path = dir.join("state.sqlite");
	let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
	conn.busy_timeout(std::time::Duration::from_millis(5000))?;
	conn.pragma_update(None, "journal_mode", "WAL")?;
	conn.execute_batch(SCHEMA).context("creating tokens table")?;
	Ok(conn)
}

/// 32 bytes from the kernel csprng, hex-encoded (64 characters).
fn random_token() -> Result<String> {
	let mut buf = [0u8; 32];
	let mut f = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
	f.read_exact(&mut buf).context("reading /dev/urandom")?;
	Ok(hex::encode(buf))
}

/// one row of `kb token list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenRow {
	pub token_sha256: String,
	pub principal: String,
	pub created_at: String,
	pub last_used: Option<String>,
	pub revoked: bool,
}

/// mint a fresh token for `principal`, store its sha256, and return the raw token. the raw value
/// is never stored or logged; the caller (`kb token issue`) prints it exactly once.
pub fn issue(cfg: &Config, principal: &str) -> Result<String> {
	let token = random_token()?;
	let hash = sha256_hex(token.as_bytes());
	let conn = open(cfg)?;
	let created_at = crate::clock::now().to_rfc3339();
	conn.execute(
		"insert into tokens (token_sha256, principal, created_at, last_used, revoked) values (?1, ?2, ?3, null, 0)",
		params![hash, principal, created_at],
	)
	.context("inserting token row")?;
	Ok(token)
}

pub fn list(cfg: &Config) -> Result<Vec<TokenRow>> {
	let conn = open(cfg)?;
	let mut stmt = conn.prepare("select token_sha256, principal, created_at, last_used, revoked from tokens order by created_at")?;
	let rows = stmt.query_map([], |r| {
		Ok(TokenRow { token_sha256: r.get(0)?, principal: r.get(1)?, created_at: r.get(2)?, last_used: r.get(3)?, revoked: r.get::<_, i64>(4)? != 0 })
	})?;
	Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// mark every token matching `target` revoked; `target` is either a prefix of a `token_sha256`
/// or an exact principal name. returns the number of rows changed; an error when nothing matched.
pub fn revoke(cfg: &Config, target: &str) -> Result<usize> {
	let conn = open(cfg)?;
	let n = conn
		.execute("update tokens set revoked = 1 where token_sha256 like ?1 or principal = ?2", params![format!("{target}%"), target])
		.context("revoking tokens")?;
	if n == 0 {
		bail!("no token matches `{target}` (by hash prefix or principal)");
	}
	Ok(n)
}

/// resolve a bearer token to its principal: the token must hash to a stored, non-revoked row.
/// touches `last_used` on a hit. `None` for an unknown or revoked token, never an error, so
/// callers turn every negative result into a plain 401.
pub fn verify(cfg: &Config, token: &str) -> Result<Option<String>> {
	let hash = sha256_hex(token.as_bytes());
	let conn = open(cfg)?;
	let principal: Option<String> =
		conn.query_row("select principal from tokens where token_sha256 = ?1 and revoked = 0", params![hash], |r| r.get(0)).optional()?;
	if let Some(p) = &principal {
		let now = crate::clock::now().to_rfc3339();
		conn.execute("update tokens set last_used = ?1 where token_sha256 = ?2", params![now, hash])?;
		return Ok(Some(p.clone()));
	}
	Ok(None)
}

/// how the server resolves a request to a principal. `Token` looks up every request's bearer
/// token; `NoAuth` (design.md §10, `kb serve --no-auth`) fixes every request to one principal,
/// computed once at startup from the git identity, for trusted single-user local use.
#[derive(Debug, Clone)]
pub enum AuthMode {
	Token,
	NoAuth(String),
}

/// the git identity's principal, for `--no-auth` single-user local mode: the configured
/// principal for that email in groups.toml, or the raw email/name when groups.toml does not
/// know it (single-user mode, design.md §10).
pub fn local_principal(cfg: &Config) -> String {
	let (name, email) = crate::git::identity(&cfg.root);
	if let Some(email) = &email
		&& let Some(p) = cfg.principal_for_email(email)
	{
		return p;
	}
	email.or(name).unwrap_or_else(|| "local".to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn issue_verify_and_revoke_by_principal() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let token = issue(&cfg, "caleb").unwrap();
		assert_eq!(token.len(), 64, "32 bytes hex-encoded");

		assert_eq!(verify(&cfg, &token).unwrap().as_deref(), Some("caleb"));
		assert_eq!(verify(&cfg, "not-a-real-token").unwrap(), None);

		let rows = list(&cfg).unwrap();
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].principal, "caleb");
		assert!(!rows[0].revoked);
		assert!(rows[0].last_used.is_some(), "verify should have touched last_used");

		let n = revoke(&cfg, "caleb").unwrap();
		assert_eq!(n, 1);
		assert_eq!(verify(&cfg, &token).unwrap(), None, "a revoked token no longer verifies");
	}

	#[test]
	fn revoke_by_hash_prefix() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let token = issue(&cfg, "svc-curate").unwrap();
		let rows = list(&cfg).unwrap();
		let prefix = &rows[0].token_sha256[..8];
		revoke(&cfg, prefix).unwrap();
		assert_eq!(verify(&cfg, &token).unwrap(), None);
	}

	#[test]
	fn revoke_unknown_target_errors() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		assert!(revoke(&cfg, "nobody").is_err());
	}

	#[test]
	fn two_issued_tokens_are_distinct() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let a = issue(&cfg, "a").unwrap();
		let b = issue(&cfg, "b").unwrap();
		assert_ne!(a, b);
	}
}
