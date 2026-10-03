//! design.md §23 (local and server) and §29 step 6: the http server behind `kb serve`, the
//! stdio and http mcp transport, and bearer tokens. "one binary, same code paths" -- this module
//! is additive: every handler calls straight into the same `index`, `curate`, `merge`, `runlog`,
//! and `store` functions the cli uses.
//!
//! - `auth`     bearer tokens: the `tokens` table, issue/list/revoke, request authentication.
//! - `framing`  `kb mcp` stdio message framing (newline-delimited or `Content-Length`).
//! - `handlers` business logic shared by `rest` and `mcp`: search, get, refs, propose, run, answer.
//! - `mcp`      the model context protocol: json-rpc 2.0 dispatch and tool catalog.
//! - `rest`     the http routes: the same five mcp tools, plus `/blob`, `/runs`, `/review`, the
//!   two webhooks, and `/mcp` itself (streamable-http shaped, no sse).

pub mod auth;
pub mod framing;
pub mod handlers;
pub mod mcp;
pub mod rest;

use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tiny_http::Server;

/// `kb serve`'s options (design.md §29 step 6).
#[derive(Debug, Clone, Default)]
pub struct ServeOpts {
	pub bind: String,
	pub no_auth: bool,
	pub tick_seconds: u64,
	/// serve reads only: search, get, refs, docs, answer, runs, review, blob. `propose`, the
	/// webhooks, and the mcp `propose` tool answer 403.
	pub read_only: bool,
}

/// size of the blocking request thread pool (task's constraint: "a small thread pool of 8").
const WORKER_THREADS: usize = 8;

/// a running server: worker threads plus the background tick thread. `addr()` gives the actual
/// bound address (useful when `bind` asked for an ephemeral port, `:0`); `shutdown` stops the
/// accept loop, waits for in-flight requests to finish, and joins every thread.
pub struct ServerHandle {
	addr: SocketAddr,
	shutdown_flag: Arc<AtomicBool>,
	server: Arc<Server>,
	workers: Vec<JoinHandle<()>>,
	tick_thread: Option<JoinHandle<()>>,
}

impl ServerHandle {
	pub fn addr(&self) -> SocketAddr {
		self.addr
	}

	/// signal shutdown, unblock the accept loop so it notices immediately, and join every
	/// thread. in-flight requests keep running to completion (`recv_timeout` only stops new
	/// ones from being picked up once the flag is set).
	pub fn shutdown(self) {
		self.shutdown_flag.store(true, Ordering::SeqCst);
		self.server.unblock();
		for w in self.workers {
			let _ = w.join();
		}
		if let Some(t) = self.tick_thread {
			let _ = t.join();
		}
	}
}

/// bind the server and spawn its worker and tick threads; returns as soon as the socket is bound,
/// so a caller (a test, or `serve_blocking` below) can read the actual address before anything
/// blocks. `--no-auth` (design.md §10) requires a loopback bind, checked here rather than
/// silently rebound, since a caller who typed a non-loopback address for `--no-auth` almost
/// certainly did not mean to expose an unauthenticated server.
pub fn start(cfg: Config, opts: ServeOpts) -> Result<ServerHandle> {
	let bind_addr: SocketAddr = opts.bind.parse().with_context(|| format!("invalid --bind address `{}`", opts.bind))?;
	if opts.no_auth && !bind_addr.ip().is_loopback() {
		bail!("--no-auth requires a loopback bind address (127.0.0.1 or ::1), got `{}`", opts.bind);
	}

	let server = Server::http(bind_addr).map_err(|e| anyhow::anyhow!("binding {}: {e}", opts.bind))?;
	let addr = server.server_addr().to_ip().context("server did not report an ip address")?;
	let server = Arc::new(server);

	let auth_mode = if opts.no_auth { auth::AuthMode::NoAuth(auth::local_principal(&cfg)) } else { auth::AuthMode::Token };

	let cfg = Arc::new(cfg);
	let shutdown_flag = Arc::new(AtomicBool::new(false));

	let mut workers = Vec::with_capacity(WORKER_THREADS);
	for _ in 0..WORKER_THREADS {
		let server = Arc::clone(&server);
		let cfg = Arc::clone(&cfg);
		let auth_mode = auth_mode.clone();
		let shutdown_flag = Arc::clone(&shutdown_flag);
		let read_only = opts.read_only;
		workers.push(std::thread::spawn(move || worker_loop(&server, &cfg, &auth_mode, read_only, &shutdown_flag)));
	}

	let tick_thread = if opts.tick_seconds > 0 {
		let cfg = Arc::clone(&cfg);
		let shutdown_flag = Arc::clone(&shutdown_flag);
		let interval = Duration::from_secs(opts.tick_seconds);
		Some(std::thread::spawn(move || tick_loop(&cfg, interval, &shutdown_flag)))
	} else {
		None
	};

	Ok(ServerHandle { addr, shutdown_flag, server, workers, tick_thread })
}

/// `kb serve`: start the server and block until ctrl-c, then shut down gracefully.
pub fn serve_blocking(cfg: Config, opts: ServeOpts) -> Result<()> {
	let bind = opts.bind.clone();
	let handle = start(cfg, opts)?;
	eprintln!("kb serve listening on http://{} (ctrl-c to stop)", handle.addr());
	let _ = bind;

	let stop = Arc::new(AtomicBool::new(false));
	{
		let stop = Arc::clone(&stop);
		ctrlc_handler(move || stop.store(true, Ordering::SeqCst))?;
	}
	while !stop.load(Ordering::SeqCst) {
		std::thread::sleep(Duration::from_millis(200));
	}
	eprintln!("kb serve shutting down");
	handle.shutdown();
	Ok(())
}

/// install a ctrl-c handler with nothing beyond the standard library: a `signal(2)` call for
/// `SIGINT` on unix, hand-declared (below) so this crate does not gain a dependency on the
/// `libc` crate just for one function. kept unix-only since this crate otherwise has no
/// platform-specific code; `kb serve` is not expected to run on windows yet.
#[cfg(unix)]
fn ctrlc_handler(on_signal: impl Fn() + Send + Sync + 'static) -> Result<()> {
	use std::sync::OnceLock;
	static HANDLER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
	// signal-safe-ish: the closure only ever stores to an AtomicBool, so running it directly
	// from the signal handler (rather than deferring to a self-pipe) is acceptable here.
	extern "C" fn trampoline(_sig: libc_sigint::c_int) {
		if let Some(f) = HANDLER.get() {
			f();
		}
	}
	HANDLER.set(Box::new(on_signal)).map_err(|_| anyhow::anyhow!("ctrl-c handler already installed"))?;
	unsafe {
		libc_sigint::signal(libc_sigint::SIGINT, trampoline);
	}
	Ok(())
}

#[cfg(not(unix))]
fn ctrlc_handler(_on_signal: impl Fn() + Send + Sync + 'static) -> Result<()> {
	bail!("graceful ctrl-c shutdown is only wired up on unix in this build")
}

/// the one libc symbol `ctrlc_handler` needs, hand-declared so this crate does not gain a
/// dependency on the `libc` crate just for `signal(2)`. `Handler` matches C's
/// `void (*)(int)`, the type `signal`'s own prototype (`void (*signal(int, void (*)(int)))(int)`)
/// takes and returns.
#[cfg(unix)]
#[allow(non_camel_case_types)]
mod libc_sigint {
	pub type c_int = i32;
	pub type Handler = extern "C" fn(c_int);
	pub const SIGINT: c_int = 2;
	unsafe extern "C" {
		pub fn signal(signum: c_int, handler: Handler) -> Handler;
	}
}

fn worker_loop(server: &Server, cfg: &Config, auth_mode: &auth::AuthMode, read_only: bool, shutdown_flag: &AtomicBool) {
	while !shutdown_flag.load(Ordering::SeqCst) {
		match server.recv_timeout(Duration::from_millis(200)) {
			Ok(Some(request)) => {
				let start = Instant::now();
				let method = request.method().as_str().to_string();
				let path = request.url().split('?').next().unwrap_or("").to_string();
				// a panic in one handler must not take this worker thread with it; the dropped
				// request is answered 500 by tiny_http.
				let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rest::handle(cfg, auth_mode, read_only, request)));
				let ms = start.elapsed().as_millis();
				match handled {
					Ok((status, principal)) => eprintln!("{method} {path} {principal} {status} {ms}ms"),
					Err(_) => eprintln!("{method} {path} - 500 {ms}ms (handler panicked)"),
				}
			}
			Ok(None) => {} // recv_timeout elapsed with nothing waiting; loop and re-check shutdown
			Err(e) => {
				eprintln!("kb serve: error receiving a request: {e}");
			}
		}
	}
}

fn tick_loop(cfg: &Config, interval: Duration, shutdown_flag: &AtomicBool) {
	let mut last = Instant::now();
	while !shutdown_flag.load(Ordering::SeqCst) {
		if last.elapsed() >= interval {
			if let Err(e) = crate::merge::tick::tick(cfg) {
				eprintln!("kb serve: reconciliation tick failed: {e:#}");
			}
			last = Instant::now();
		}
		std::thread::sleep(Duration::from_millis(200));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn no_auth_rejects_a_non_loopback_bind() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let opts = ServeOpts { bind: "0.0.0.0:0".to_string(), no_auth: true, ..ServeOpts::default() };
		assert!(start(cfg, opts).is_err());
	}

	#[test]
	fn start_binds_an_ephemeral_port_and_shuts_down() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = Config::load(dir.path()).unwrap();
		let opts = ServeOpts { bind: "127.0.0.1:0".to_string(), no_auth: true, ..ServeOpts::default() };
		let handle = start(cfg, opts).unwrap();
		assert_ne!(handle.addr().port(), 0);
		handle.shutdown();
	}
}
