//! `shell.run`: one command under `bash -c` in a workspace.
//!
//! The command gets its own process group, so the whole tree it starts can
//! be killed at once: on timeout, and again as soon as the command itself
//! exits, so a background child cannot hold the output pipes open and hang
//! the call. A group of its own also means that nothing else stops it, so
//! the service keeps a list of them and kills them all when it is stopped.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use molt_api::shell::{RunRequest, RunResponse};
use molt_proto::{ErrorCode, RemoteError};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::error::failed;
use crate::{sandbox, Roots, SandboxPolicy};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const HEAD_BYTES: usize = 8 * 1024;
const TAIL_BYTES: usize = 24 * 1024;
/// How long output may keep arriving after the command exited and its group was killed.
const GRACE: Duration = Duration::from_millis(500);
/// Removed from the command's environment: the service's bus secret and API keys.
const HIDDEN_PREFIXES: [&[u8]; 2] = [b"MOLT_", b"ANTHROPIC_"];
/// Keep tools from paging, prompting or printing colour codes.
pub(crate) const FIXED_ENV: [(&str, &str); 6] = [
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("CI", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

/// `bash` from `PATH`, else `sh`.
pub(crate) fn find_shell() -> PathBuf {
    find_in_path("bash").or_else(|| find_in_path("sh")).unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The process groups of the commands running now.
#[derive(Default)]
pub(crate) struct Groups(Mutex<GroupState>);

#[derive(Default)]
struct GroupState {
    stopping: bool,
    running: HashMap<libc::pid_t, Option<Arc<std::fs::File>>>,
}

impl Groups {
    pub fn terminate_all(&self) {
        let mut state = lock(&self.0);
        state.stopping = true;
        for (&pgid, info) in &state.running {
            term_group(pgid, info.as_deref());
        }
    }

    pub fn kill_all(&self) {
        for &pgid in lock(&self.0).running.keys() {
            killpg(pgid);
        }
    }
}

pub(crate) async fn run(
    roots: &Arc<Roots>,
    program: &Path,
    groups: &Arc<Groups>,
    policy: Option<&SandboxPolicy>,
    req: RunRequest,
    cancel: CancellationToken,
) -> Result<RunResponse, RemoteError> {
    let ws = {
        let (roots, ws) = (roots.clone(), req.workspace.clone());
        tokio::task::spawn_blocking(move || roots.workspace(&ws))
            .await
            .map_err(|e| failed(format!("shell.run failed: {e}")))??
    };
    if cancel.is_cancelled() {
        return Err(RemoteError { code: ErrorCode::Cancelled, message: "command cancelled before start".into() });
    }
    let limit = Duration::from_millis(req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS));

    let (mut cmd, guard) = match policy {
        Some(policy) => {
            let (cmd, guard) = policy.prepare(roots, &ws, program).map_err(|e| failed(format!("sandbox: {e:#}")))?;
            (cmd, Some(guard))
        }
        None => (Command::new(program), None),
    };
    cmd.arg("-c")
        .arg(&req.command)
        .current_dir(&ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if policy.is_none() {
        cmd.env_clear().envs(child_env());
    }
    let started = Instant::now();
    let (mut child, group) = Group::spawn(&mut cmd, groups, &cancel, guard.as_ref().map(|guard| guard.info.clone()))?;

    let stdout = Arc::new(Mutex::new(Capture::default()));
    let stderr = Arc::new(Mutex::new(Capture::default()));
    // JoinSet aborts its readers when this request future is dropped, too.
    let mut readers = tokio::task::JoinSet::new();
    readers.spawn(drain(child.stdout.take(), stdout.clone()));
    readers.spawn(drain(child.stderr.take(), stderr.clone()));

    // Prefer an already-finished command in a cancellation/completion tie.
    let (status, timed_out, cancelled) = tokio::select! {
        biased;
        status = child.wait() => (status, false, false),
        _ = cancel.cancelled() => (terminate(&group, &mut child).await, false, true),
        _ = tokio::time::sleep(limit) => (terminate(&group, &mut child).await, true, false),
    };
    let duration = started.elapsed();
    group.kill();
    let status = status.map_err(|e| failed(format!("waiting for the command: {e}")))?;
    let _ = tokio::time::timeout(GRACE, async { while readers.join_next().await.is_some() {} }).await;
    // Whatever escaped the group (a daemon that called setsid) may still hold a pipe.
    readers.abort_all();

    let (stdout, out_cut) = take(&stdout).finish();
    let (stderr, err_cut) = take(&stderr).finish();
    Ok(RunResponse {
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        cancelled,
        stdout,
        stderr,
        truncated: out_cut || err_cut,
        duration_ms: duration.as_millis() as u64,
    })
}

async fn terminate(group: &Group, child: &mut tokio::process::Child) -> std::io::Result<std::process::ExitStatus> {
    group.term();
    // Keep the leader unreaped during grace so its process-group id cannot be reused.
    tokio::time::sleep(GRACE).await;
    group.kill();
    child.wait().await
}

/// The service's environment without its secrets, plus [`FIXED_ENV`].
fn child_env() -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !HIDDEN_PREFIXES.iter().any(|p| name.as_bytes().starts_with(p)))
        .collect();
    env.extend(FIXED_ENV.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))));
    env
}

/// A command's process group, listed in [`Groups`] while it lives. Killed
/// when dropped too, so a cancelled request does not leave its command running.
struct Group {
    // A pgid of 0 would mean our own group.
    pgid: Option<libc::pid_t>,
    groups: Arc<Groups>,
    info: Option<Arc<std::fs::File>>,
}

impl Group {
    fn spawn(
        command: &mut Command,
        groups: &Arc<Groups>,
        cancel: &CancellationToken,
        info: Option<Arc<std::fs::File>>,
    ) -> Result<(tokio::process::Child, Self), RemoteError> {
        // Admission, spawning and registration share the shutdown lock.
        // Shutdown therefore either sees this child or prevents it starting.
        let mut state = lock(&groups.0);
        if state.stopping {
            return Err(RemoteError { code: ErrorCode::Unavailable, message: "shell service is shutting down".into() });
        }
        if cancel.is_cancelled() {
            return Err(RemoteError { code: ErrorCode::Cancelled, message: "command cancelled before start".into() });
        }
        let child = command.spawn().map_err(|e| failed(format!("could not start command: {e}")))?;
        let pgid = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()).filter(|&p| p > 0);
        if let Some(pgid) = pgid {
            state.running.insert(pgid, info.clone());
        }
        Ok((child, Self { pgid, groups: groups.clone(), info }))
    }

    fn term(&self) {
        if let Some(pgid) = self.pgid {
            term_group(pgid, self.info.as_deref());
        }
    }

    fn kill(&self) {
        if let Some(pgid) = self.pgid {
            killpg(pgid);
        }
    }
}

fn term_group(pgid: libc::pid_t, info: Option<&std::fs::File>) {
    let pgid = match info {
        None => Some(pgid),
        Some(info) => sandbox::namespace_group(info),
    };
    if let Some(pgid) = pgid {
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
        if let Some(pgid) = self.pgid {
            lock(&self.groups.0).running.remove(&pgid);
        }
    }
}

fn killpg(pgid: libc::pid_t) {
    // SAFETY: killpg only sends a signal; an empty group yields ESRCH, which is fine.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

async fn drain(pipe: Option<impl AsyncRead + Unpin>, into: Arc<Mutex<Capture>>) {
    let Some(mut pipe) = pipe else { return };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => lock(&into).push(&buf[..n]),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn take(capture: &Mutex<Capture>) -> Capture {
    std::mem::take(&mut *lock(capture))
}

/// The first [`HEAD_BYTES`] and last [`TAIL_BYTES`] of a stream.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl Capture {
    fn push(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.head.len() < HEAD_BYTES {
            let n = (HEAD_BYTES - self.head.len()).min(data.len());
            self.head.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
        self.tail.extend(data);
        let excess = self.tail.len().saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
    }

    /// The text, with a marker where bytes were dropped, and whether any were.
    fn finish(self) -> (String, bool) {
        let omitted = self.total - (self.head.len() + self.tail.len()) as u64;
        let mut bytes = self.head;
        if omitted > 0 {
            bytes.extend_from_slice(format!("\n[... {omitted} bytes omitted ...]\n").as_bytes());
        }
        bytes.extend(self.tail);
        (String::from_utf8_lossy(&bytes).into_owned(), omitted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_head_and_tail() {
        let mut c = Capture::default();
        c.push(b"short");
        assert_eq!(c.finish(), ("short".to_owned(), false));

        let mut c = Capture::default();
        c.push(&vec![b'h'; HEAD_BYTES]);
        for _ in 0..10 {
            c.push(&vec![b'm'; 10_000]);
        }
        c.push(&vec![b't'; TAIL_BYTES]);
        let (text, cut) = c.finish();
        assert!(cut);
        assert!(text.starts_with(&"h".repeat(HEAD_BYTES)));
        assert!(text.ends_with(&"t".repeat(TAIL_BYTES)));
        assert!(text.contains("[... 100000 bytes omitted ...]"));
    }
}
