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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use molt_api::progress::{self, Progress};
use molt_api::shell::{RunRequest, RunResponse};
use molt_proto::{ErrorCode, RemoteError, TraceId};
use molt_sdk::Service;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cargo_cache::CargoCache;
use crate::error::failed;
use crate::{build_dir, sandbox, Roots, SandboxPolicy};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const HEAD_BYTES: usize = 8 * 1024;
const TAIL_BYTES: usize = 24 * 1024;
/// How long output may keep arriving after the command exited and its group was killed.
const GRACE: Duration = Duration::from_millis(500);
/// Removed from the command's environment: the service's bus secret and API keys.
const HIDDEN_PREFIXES: [&[u8]; 3] = [b"MOLT_", b"ANTHROPIC_", b"OPENAI_"];
/// Output a streamed command publishes at most; the reply has the rest of its start and end.
const STREAM_BYTES: usize = 1 << 20;
/// A line longer than this is published in pieces.
const STREAM_LINE: usize = 8 * 1024;
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
    sandbox: Option<(&SandboxPolicy, Option<Arc<CargoCache>>)>,
    req: RunRequest,
    cancel: CancellationToken,
    sink: Option<Sink>,
) -> Result<RunResponse, RemoteError> {
    let (policy, cargo) = match sandbox {
        Some((policy, cargo)) => (Some(policy), cargo),
        None => (None, None),
    };
    // Held until the command is done, so no other command builds into its directory.
    let (ws, build) = {
        let (roots, ws) = (roots.clone(), req.workspace.clone());
        tokio::task::spawn_blocking(move || {
            let ws = roots.workspace(&ws)?;
            let build = build_dir::acquire(&roots, &ws, cargo.as_ref())?;
            Ok::<_, RemoteError>((ws, build))
        })
        .await
        .map_err(|e| failed(format!("shell.run failed: {e}")))??
    };
    if cancel.is_cancelled() {
        return Err(RemoteError { code: ErrorCode::Cancelled, message: "command cancelled before start".into() });
    }
    let limit = Duration::from_millis(req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS));

    let (mut cmd, guard) = match policy {
        Some(policy) => {
            let (cmd, guard) =
                policy.prepare(roots, &ws, program, &build).map_err(|e| failed(format!("sandbox: {e:#}")))?;
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
        // Unconfined, the same private directories, unless the user's
        // environment already chose where builds go.
        for (name, value) in build.env() {
            if std::env::var_os(name).is_none() {
                cmd.env(name, value);
            }
        }
    }
    let started = Instant::now();
    let (mut child, group) = Group::spawn(&mut cmd, groups, &cancel, guard.as_ref().map(|guard| guard.info.clone()))?;

    let stdout = Arc::new(Mutex::new(Capture::default()));
    let stderr = Arc::new(Mutex::new(Capture::default()));
    // JoinSet aborts its readers when this request future is dropped, too.
    let mut readers = tokio::task::JoinSet::new();
    readers.spawn(drain(child.stdout.take(), stdout.clone(), sink.clone().map(|s| (s, false))));
    readers.spawn(drain(child.stderr.take(), stderr.clone(), sink.map(|s| (s, true))));

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

    drop(build);
    let (stdout, out_cut) = take(&stdout).finish();
    let (stderr, err_cut) = take(&stderr).finish();
    let success = status.code() == Some(0) && !timed_out && !cancelled;
    let offline_failure =
        policy.is_some_and(|policy| !policy.network) && !success && (looks_offline(&stdout) || looks_offline(&stderr));
    Ok(RunResponse {
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        cancelled,
        stdout,
        stderr,
        truncated: out_cut || err_cut,
        duration_ms: duration.as_millis() as u64,
        offline_failure,
    })
}

/// What package managers and network clients print when they cannot reach
/// the network or a registry, lowercased.
const OFFLINE_SIGNS: &[&str] = &[
    // cargo
    "as a dependency of package",
    "no matching package named",
    "crates.io index",
    "failed to download",
    "--offline was specified",
    "you're using offline mode",
    // curl, git, and resolvers in general
    "could not resolve host",
    "temporary failure in name resolution",
    "network is unreachable",
    "network unreachable",
    // npm
    "enotfound",
    "eai_again",
    // pip
    "failed to establish a new connection",
    "no matching distribution found",
    // go
    "dial tcp",
];

/// Whether `output` shows a download that failed for want of network.
fn looks_offline(output: &str) -> bool {
    let output = output.to_lowercase();
    OFFLINE_SIGNS.iter().any(|sign| output.contains(sign))
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

async fn drain(pipe: Option<impl AsyncRead + Unpin>, into: Arc<Mutex<Capture>>, sink: Option<(Sink, bool)>) {
    let Some(mut pipe) = pipe else { return };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                lock(&into).push(&buf[..n]);
                if let Some((sink, stderr)) = &sink {
                    sink.send(*stderr, &buf[..n]);
                }
            }
        }
    }
}

/// Where a streamed command's output goes as it is read, to be published.
#[derive(Clone)]
pub(crate) struct Sink {
    tx: mpsc::UnboundedSender<(bool, Vec<u8>)>,
    /// Bytes handed on so far, by both streams: past [`STREAM_BYTES`] no more are.
    sent: Arc<AtomicUsize>,
}

impl Sink {
    fn send(&self, stderr: bool, bytes: &[u8]) {
        // The chunk that crosses the limit still goes, so the publisher sees the output was cut.
        if self.sent.fetch_add(bytes.len(), Ordering::Relaxed) < STREAM_BYTES {
            let _ = self.tx.send((stderr, bytes.to_vec()));
        }
    }
}

/// A sink for one command's output, and the task that publishes what it
/// gets on `topic:progress` as [`Progress::Output`] events traced `trace`.
/// The task ends once every clone of the sink is dropped and what is left
/// is published.
pub(crate) fn stream(svc: Arc<Service>, trace: TraceId) -> (Sink, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let sink = Sink { tx, sent: Arc::default() };
    (sink, tokio::spawn(publish(svc, trace, rx)))
}

async fn publish(svc: Arc<Service>, trace: TraceId, mut chunks: mpsc::UnboundedReceiver<(bool, Vec<u8>)>) {
    let mut out = Publisher { svc, run: trace.to_string(), trace, seq: 0, sent: 0, cut: false };
    // Partial lines of stdout and stderr.
    let mut partial = [Vec::new(), Vec::new()];
    while let Some(first) = chunks.recv().await {
        if out.cut {
            continue;
        }
        // Whatever else has arrived goes in the same events.
        let mut texts: Vec<(bool, Vec<u8>)> = Vec::new();
        let mut next = Some(first);
        while let Some((stderr, bytes)) = next.take().or_else(|| chunks.try_recv().ok()) {
            let buf = &mut partial[usize::from(stderr)];
            buf.extend_from_slice(&bytes);
            let end = match buf.iter().rposition(|&b| b == b'\n') {
                Some(at) => at + 1,
                None if buf.len() >= STREAM_LINE => buf.len(),
                None => continue,
            };
            let text: Vec<u8> = buf.drain(..end).collect();
            match texts.last_mut() {
                Some((last, joined)) if *last == stderr => joined.extend(text),
                _ => texts.push((stderr, text)),
            }
        }
        for (stderr, text) in texts {
            if !out.send(stderr, &text).await {
                break;
            }
        }
    }
    for (stderr, rest) in [(false, &partial[0]), (true, &partial[1])] {
        if !out.cut && !rest.is_empty() && !out.send(stderr, rest).await {
            break;
        }
    }
}

struct Publisher {
    svc: Arc<Service>,
    trace: TraceId,
    run: String,
    seq: u64,
    sent: usize,
    cut: bool,
}

impl Publisher {
    /// Publish `text`, or the cut when it would go past [`STREAM_BYTES`]; false once cut.
    async fn send(&mut self, stderr: bool, text: &[u8]) -> bool {
        self.sent += text.len();
        self.cut = self.sent > STREAM_BYTES;
        let text = if self.cut { String::new() } else { String::from_utf8_lossy(text).into_owned() };
        let event = Progress::Output { run: self.run.clone(), seq: self.seq, text, stderr, cut: self.cut };
        self.seq += 1;
        match serde_json::to_value(event) {
            Ok(event) => {
                if let Err(e) = self.svc.publish_traced(progress::TOPIC, event, self.trace.clone()).await {
                    tracing::debug!(error = %e, "could not publish command output");
                }
            }
            Err(e) => tracing::debug!(error = %e, "could not encode command output"),
        }
        !self.cut
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
    fn registry_and_network_failures_are_recognized_and_other_failures_are_not() {
        for output in [
            "error: failed to get `memchr` as a dependency of package `lines v0.1.0`",
            "error: no matching package named `memchr` found\nlocation searched: crates.io index",
            "fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com",
            "npm ERR! code ENOTFOUND\nnpm ERR! errno ENOTFOUND",
            "WARNING: Retrying ... Failed to establish a new connection: [Errno -3] Temporary failure",
            "go: github.com/x/y@v1.0.0: Get \"https://proxy.golang.org/...\": dial tcp: lookup proxy.golang.org",
        ] {
            assert!(looks_offline(output), "{output}");
        }
        assert!(!looks_offline("test tests::adds ... FAILED\nassertion failed: left == right"));
    }

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
