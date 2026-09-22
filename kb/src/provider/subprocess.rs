//! shared child-process plumbing for the `claude-code` and `command` adapters: spawn, feed
//! stdin, wait with a hard timeout (killing the child past it), and collect stdout/stderr
//! without risking a pipe-buffer deadlock. not a provider itself.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct ChildOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
}

/// `KB_PROVIDER_TIMEOUT_SECS` overrides (default 600), same env var `super::http_client` reads.
pub(crate) fn timeout_from_env() -> Duration {
    let secs = std::env::var("KB_PROVIDER_TIMEOUT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600u64);
    Duration::from_secs(secs)
}

/// run `program args...`, writing `stdin_data` to its stdin and closing it, then wait up to
/// `timeout`, killing the child and erroring if it is still running past that. stdout and
/// stderr are drained on background threads as they arrive, so a chatty child cannot deadlock
/// this by filling a pipe buffer while nobody is polling `try_wait`.
pub(crate) fn run_with_timeout(program: &str, args: &[String], stdin_data: &str, timeout: Duration) -> Result<ChildOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{program}`; is it installed and on PATH?"))?;

    let mut stdin_pipe = child.stdin.take().expect("stdin was piped");
    let stdin_data = stdin_data.to_string();
    std::thread::spawn(move || {
        // a closed reader (the child exited early) makes this write fail; nothing to do about
        // it here, the exit status and stderr below carry the real diagnosis.
        let _ = stdin_pipe.write_all(stdin_data.as_bytes());
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("polling child process")? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("`{program}` timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    Ok(ChildOutput { stdout, stderr, status })
}
