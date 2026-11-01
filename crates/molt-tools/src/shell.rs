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
use molt_api::shell::{OutputEnd, OutputStream, RunRequest, RunResponse, SavedOutput};
use molt_proto::{ErrorCode, RemoteError, TraceId};
use molt_sdk::Service;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cargo_cache::CargoCache;
use crate::error::failed;
use crate::network::Networks;
use crate::outputs::{self, Asker, Origin};
use crate::resources::{self, Admission};
use crate::sandbox::LocalNet;
use crate::{build_dir, sandbox, Roots, SandboxPolicy};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const HEAD_BYTES: usize = 8 * 1024;
const TAIL_BYTES: usize = 24 * 1024;
/// How long output may keep arriving after the command exited and its group was killed.
pub(crate) const GRACE: Duration = Duration::from_millis(500);
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
    /// Starts to fail as if the host had no process to spare, in tests.
    #[cfg(test)]
    short: usize,
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

#[allow(clippy::too_many_arguments)] // The service's parts a command uses, each borrowed for the call.
pub(crate) async fn run(
    roots: &Arc<Roots>,
    program: &Path,
    groups: &Arc<Groups>,
    admission: &Admission,
    sandbox: Option<(&SandboxPolicy, Option<Arc<CargoCache>>)>,
    networks: &Arc<Networks>,
    req: RunRequest,
    cancel: CancellationToken,
    out: Out,
) -> Result<RunResponse, RemoteError> {
    let Out { sink, save } = out;
    let (policy, cargo) = match sandbox {
        Some((policy, cargo)) => (Some(policy), cargo),
        None => (None, None),
    };
    // A command waiting for its turn holds nothing else, not even a build directory.
    let _turn = admission.enter(&cancel).await?;
    // Held until the command is done, so no other command builds into its
    // directory, and the processes of its workspace stay reachable.
    let (ws, build, net) = acquire(roots, policy, cargo, networks, &req.workspace, "shell.run").await?;
    if cancel.is_cancelled() {
        return Err(RemoteError { code: ErrorCode::Cancelled, message: "command cancelled before start".into() });
    }
    let limit = Duration::from_millis(req.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS));
    if req.stdin.as_ref().is_some_and(|input| input.len() > molt_api::shell::MAX_STDIN) {
        let message = format!("stdin is over {} bytes: give the command less input", molt_api::shell::MAX_STDIN);
        return Err(RemoteError { code: ErrorCode::Invalid, message });
    }

    let save = save.map(|(store, asker)| (store, Arc::new(asker.origin(&ws, &req.workspace, &req.command))));
    let started = Instant::now();
    let mut tries = 0;
    let (status, timed_out, cancelled, stdout, stderr) = loop {
        tries += 1;
        let (mut cmd, guard) = command(roots, program, policy, &ws, &build, net.as_deref(), &req.command)?;
        let keep = (sink.clone(), save.clone());
        let ran = once(&mut cmd, policy, guard, groups, &cancel, req.stdin.clone(), keep, limit).await;
        let (resource, said) = match ran {
            Once::Ran { status, timed_out, cancelled, stdout, stderr } => {
                break (status, timed_out, cancelled, stdout, stderr)
            }
            Once::NotStarted { resource, said } => (resource, said),
            Once::Refused(e) => return Err(e),
        };
        let Some(delay) = resources::BACKOFF.get(tries - 1) else {
            return Err(resources::short_of(resource, &said, tries, policy));
        };
        tracing::debug!(resource = resource.name(), %said, "command could not start; trying again");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(RemoteError {
                    code: ErrorCode::Cancelled,
                    message: "command cancelled while it waited to start again; it never started".into(),
                });
            }
            _ = tokio::time::sleep(*delay) => {}
        }
    };
    let duration = started.elapsed();
    drop((build, net));
    let status = status.map_err(|e| failed(format!("waiting for the command: {e}")))?;
    let end = OutputEnd { exit_code: status.code(), signal: status.signal(), timed_out, cancelled };
    let (stdout, out_cut, out_saved) = stdout.finish(end);
    let (stderr, err_cut, err_saved) = stderr.finish(end);
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
        outputs: out_saved.into_iter().chain(err_saved).collect(),
    })
}

/// Where a command's outputs are saved, and whose they are.
type SaveTo = (Arc<outputs::Store>, Arc<Origin>);

/// How one start of a command went.
enum Once {
    Ran {
        status: std::io::Result<std::process::ExitStatus>,
        timed_out: bool,
        cancelled: bool,
        stdout: Capture,
        stderr: Capture,
    },
    /// It provably never started, for want of `resource`: it may be started again.
    NotStarted { resource: resources::Resource, said: String },
    /// It did not start, and starting it again would not help.
    Refused(RemoteError),
}

/// Start `cmd` once, with `stdin`, and wait for it, at most `limit`.
#[allow(clippy::too_many_arguments)] // One start's parts, each borrowed or moved for it.
async fn once(
    cmd: &mut Command,
    policy: Option<&SandboxPolicy>,
    guard: Option<sandbox::Guard>,
    groups: &Arc<Groups>,
    cancel: &CancellationToken,
    stdin: Option<String>,
    (sink, save): (Option<Sink>, Option<SaveTo>),
    limit: Duration,
) -> Once {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let info = guard.as_ref().map(|guard| guard.info.clone());
    let (mut child, group) = match Group::try_spawn(cmd, groups, cancel, info) {
        Ok(spawned) => spawned,
        Err(Spawn::Short(resource, said)) => return Once::NotStarted { resource, said },
        Err(Spawn::Error(e)) => return Once::Refused(e),
    };

    let saving =
        |stream| Capture { save: save.clone().map(|(store, origin)| (store, origin, stream)), ..Capture::default() };
    let stdout = Arc::new(Mutex::new(saving(OutputStream::Stdout)));
    let stderr = Arc::new(Mutex::new(saving(OutputStream::Stderr)));
    // JoinSet aborts its readers when this request future is dropped, too.
    let mut readers = tokio::task::JoinSet::new();
    // A sandbox that could not start says so on stderr, streamed like any output: the reply tells.
    readers.spawn(drain(child.stdout.take(), stdout.clone(), sink.clone().map(|s| (s, false))));
    readers.spawn(drain(child.stderr.take(), stderr.clone(), sink.map(|s| (s, true))));
    // Written on its own, so a command that reads none of it cannot hold up
    // the call; closed when written, or when the request ends.
    let mut writer = tokio::task::JoinSet::new();
    if let (Some(mut pipe), Some(input)) = (child.stdin.take(), stdin) {
        writer.spawn(async move {
            use tokio::io::AsyncWriteExt;
            // A command that exits without reading it all closes the pipe: that is its choice.
            let _ = pipe.write_all(input.as_bytes()).await;
        });
    }

    // Prefer an already-finished command in a cancellation/completion tie.
    let (status, timed_out, cancelled) = tokio::select! {
        biased;
        status = child.wait() => (status, false, false),
        _ = cancel.cancelled() => (terminate(&group, &mut child).await, false, true),
        _ = tokio::time::sleep(limit) => (terminate(&group, &mut child).await, true, false),
    };
    group.kill();
    writer.abort_all();
    let _ = tokio::time::timeout(GRACE, async { while readers.join_next().await.is_some() {} }).await;
    // Whatever escaped the group (a daemon that called setsid) may still hold a pipe.
    readers.abort_all();
    let (stdout, stderr) = (take(&stdout), take(&stderr));
    if let (Some(guard), Ok(status)) = (&guard, &status) {
        if !timed_out && !cancelled && status.code() == Some(1) {
            let (out, err) = (stdout.peek(), stderr.peek());
            let started = guard.started();
            // Once the sandbox's namespaces exist, a namespace that cannot be created is one the
            // command asked for (a bwrap of its own, which the sandbox refuses): its output.
            let inner = started && err.to_lowercase().contains("new namespace");
            match resources::setup_failure(&out, &err).filter(|_| !inner) {
                // Before its namespaces, the sandbox never ran the command.
                Some(resources::Setup::Short(resource)) if !started => {
                    return Once::NotStarted { resource, said: err.trim().to_owned() };
                }
                // After them it failed before the command too, but that is not proven: not started again.
                Some(resources::Setup::Short(resource)) => {
                    return Once::Refused(resources::short_of(resource, &err, 1, policy));
                }
                Some(setup) => return Once::Refused(resources::no_isolation(setup, &err)),
                None => {}
            }
        }
    }
    Once::Ran { status, timed_out, cancelled, stdout, stderr }
}

/// Where a command's output goes besides its reply: published as it comes,
/// and saved whole once the reply has to cut it.
#[derive(Default)]
pub(crate) struct Out {
    pub sink: Option<Sink>,
    pub save: Option<(Arc<outputs::Store>, Asker)>,
}

/// `command` under `program -c` in the canonical workspace `ws`, in the
/// sandbox `policy` describes and on `net` when it has one (keep the guard
/// while it runs) or unconfined
/// without the service's secrets, in a process group of its own, with its
/// output piped. Its standard input is left for the caller to set.
pub(crate) fn command(
    roots: &Roots,
    program: &Path,
    policy: Option<&SandboxPolicy>,
    ws: &Path,
    build: &build_dir::BuildDir,
    net: Option<&LocalNet>,
    command: &str,
) -> Result<(Command, Option<sandbox::Guard>), RemoteError> {
    let (mut cmd, guard) = match policy {
        Some(policy) => {
            let (cmd, guard) =
                policy.prepare(roots, ws, program, build, net).map_err(|e| failed(format!("sandbox: {e:#}")))?;
            (cmd, Some(guard))
        }
        None => (Command::new(program), None),
    };
    cmd.arg("-c").arg(command).current_dir(ws).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
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
    Ok((cmd, guard))
}

/// What a command in `workspace` holds while it runs: the canonical
/// workspace, its build directory, and the network it shares with what else
/// runs there, if it shares one.
pub(crate) async fn acquire(
    roots: &Arc<Roots>,
    policy: Option<&SandboxPolicy>,
    cargo: Option<Arc<CargoCache>>,
    networks: &Arc<Networks>,
    workspace: &str,
    method: &str,
) -> Result<(PathBuf, build_dir::BuildDir, Option<Arc<LocalNet>>), RemoteError> {
    let (roots, ws, networks) = (roots.clone(), workspace.to_owned(), networks.clone());
    let policy = policy.filter(|policy| policy.shares_local_network()).cloned();
    tokio::task::spawn_blocking(move || {
        let ws = roots.workspace(&ws)?;
        let build = build_dir::acquire(&roots, &ws, cargo.as_ref())?;
        let net = match &policy {
            Some(policy) => networks.join(policy, &ws)?,
            None => None,
        };
        Ok::<_, RemoteError>((ws, build, net))
    })
    .await
    .map_err(|e| failed(format!("{method} failed: {e}")))?
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

pub(crate) async fn terminate(
    group: &Group,
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
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

/// Why a command did not start.
enum Spawn {
    /// The host was short of a resource: it never started, and may be started again.
    Short(resources::Resource, String),
    Error(RemoteError),
}

/// A command's process group, listed in [`Groups`] while it lives. Killed
/// when dropped too, so a cancelled request does not leave its command running.
pub(crate) struct Group {
    // A pgid of 0 would mean our own group.
    pgid: Option<libc::pid_t>,
    groups: Arc<Groups>,
    info: Option<Arc<std::fs::File>>,
}

impl Group {
    pub(crate) fn spawn(
        command: &mut Command,
        groups: &Arc<Groups>,
        cancel: &CancellationToken,
        info: Option<Arc<std::fs::File>>,
    ) -> Result<(tokio::process::Child, Self), RemoteError> {
        Self::try_spawn(command, groups, cancel, info).map_err(|e| match e {
            Spawn::Short(resource, said) => resources::short_of(resource, &said, 1, None),
            Spawn::Error(e) => e,
        })
    }

    /// [`Group::spawn`], telling a start the host could not afford from other failures.
    fn try_spawn(
        command: &mut Command,
        groups: &Arc<Groups>,
        cancel: &CancellationToken,
        info: Option<Arc<std::fs::File>>,
    ) -> Result<(tokio::process::Child, Self), Spawn> {
        // Admission, spawning and registration share the shutdown lock.
        // Shutdown therefore either sees this child or prevents it starting.
        let mut state = lock(&groups.0);
        if state.stopping {
            let message = "shell service is shutting down".into();
            return Err(Spawn::Error(RemoteError { code: ErrorCode::Unavailable, message }));
        }
        if cancel.is_cancelled() {
            let message = "command cancelled before start".into();
            return Err(Spawn::Error(RemoteError { code: ErrorCode::Cancelled, message }));
        }
        #[cfg(test)]
        if state.short > 0 {
            state.short -= 1;
            let said = "could not start the command: Resource temporarily unavailable (os error 11)".into();
            return Err(Spawn::Short(resources::Resource::Processes, said));
        }
        let child = command.spawn().map_err(|e| match e.raw_os_error().and_then(resources::Resource::of_errno) {
            Some(resource) => Spawn::Short(resource, format!("could not start the command: {e}")),
            None if info.is_some() && e.kind() == std::io::ErrorKind::NotFound => Spawn::Error(
                resources::no_isolation(resources::Setup::Unsupported, &format!("could not start /usr/bin/bwrap: {e}")),
            ),
            None => Spawn::Error(failed(format!("could not start command: {e}"))),
        })?;
        let pgid = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()).filter(|&p| p > 0);
        if let Some(pgid) = pgid {
            state.running.insert(pgid, info.clone());
        }
        Ok((child, Self { pgid, groups: groups.clone(), info }))
    }

    pub(crate) fn term(&self) {
        if let Some(pgid) = self.pgid {
            term_group(pgid, self.info.as_deref());
        }
    }

    pub(crate) fn kill(&self) {
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

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn take(capture: &Mutex<Capture>) -> Capture {
    std::mem::take(&mut *lock(capture))
}

/// The first [`HEAD_BYTES`] and last [`TAIL_BYTES`] of a stream, and all
/// of it saved once that is not all of it.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    /// Where to save it, until it is.
    save: Option<(Arc<outputs::Store>, Arc<Origin>, OutputStream)>,
    /// Boxed: a hash state and a file are more than the rest of a capture.
    saving: Option<Box<outputs::Writer>>,
}

impl Capture {
    fn push(&mut self, mut data: &[u8]) {
        match &mut self.saving {
            Some(writer) => writer.write(data),
            // Nothing was dropped yet: what is kept is all of it.
            None if self.total + data.len() as u64 > (HEAD_BYTES + TAIL_BYTES) as u64 => {
                if let Some((store, origin, stream)) = self.save.take() {
                    let mut first = self.head.clone();
                    first.extend(&self.tail);
                    first.extend_from_slice(data);
                    self.saving = store.create(origin, stream, &first).map(Box::new);
                }
            }
            None => {}
        }
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

    /// What it holds so far, as text, without the marker.
    fn peek(&self) -> String {
        let mut bytes = self.head.clone();
        bytes.extend(self.tail.iter());
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The text, with a marker where bytes were dropped, whether any were,
    /// and where all of it was saved, closed with how the command ended.
    fn finish(mut self, end: OutputEnd) -> (String, bool, Option<SavedOutput>) {
        let saved = self.saving.take().map(|mut writer| {
            writer.close(Some(end));
            writer.handle().saved()
        });
        let omitted = self.total - (self.head.len() + self.tail.len()) as u64;
        let mut bytes = std::mem::take(&mut self.head);
        if omitted > 0 {
            bytes.extend_from_slice(format!("\n[... {omitted} bytes omitted ...]\n").as_bytes());
        }
        bytes.extend(std::mem::take(&mut self.tail));
        (String::from_utf8_lossy(&bytes).into_owned(), omitted > 0, saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Host {
        _tmp: tempfile::TempDir,
        roots: Arc<Roots>,
        ws: PathBuf,
        groups: Arc<Groups>,
        admission: Admission,
    }

    impl Host {
        /// Unconfined commands in `ws`, the first `short` of whose starts fail for want of processes.
        fn new(short: usize) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            std::fs::create_dir_all(root.join("ws")).unwrap();
            let roots = Roots { root: root.clone(), scratch: tmp.path().join("scratch") }.canonical().unwrap();
            let groups = Arc::new(Groups::default());
            lock(&groups.0).short = short;
            let ws = roots.root.join("ws");
            Self { _tmp: tmp, roots: Arc::new(roots), ws, groups, admission: Admission::new(1, 1) }
        }

        async fn run(&self, command: &str, cancel: CancellationToken) -> Result<RunResponse, RemoteError> {
            let req = RunRequest {
                workspace: "ws".into(),
                command: command.into(),
                timeout_ms: None,
                stream: false,
                stdin: None,
            };
            let networks = Arc::default();
            run(&self.roots, &find_shell(), &self.groups, &self.admission, None, &networks, req, cancel, Out::default())
                .await
        }
    }

    #[tokio::test]
    async fn a_start_the_host_could_not_afford_is_made_again_after_a_bounded_wait() {
        // Two starts fail, the third goes: the command runs once.
        let host = Host::new(2);
        let started = Instant::now();
        let ran = host.run("echo ran >> once", CancellationToken::new()).await.unwrap();
        assert_eq!(ran.exit_code, Some(0), "{ran:?}");
        assert!(started.elapsed() >= BACKOFF_FIRST_TWO, "it waited before each new start");
        assert_eq!(std::fs::read_to_string(host.ws.join("once")).unwrap(), "ran\n");
        assert_eq!(lock(&host.groups.0).short, 0);

        // Never: refused after five starts, naming the resource, and it never ran.
        let host = Host::new(usize::MAX);
        let started = Instant::now();
        let e = host.run("touch ran", CancellationToken::new()).await.unwrap_err();
        let waited = started.elapsed();
        assert_eq!(e.code, ErrorCode::Busy);
        assert!(e.message.starts_with("short of host resources: processes: could not start"), "{}", e.message);
        assert!(e.message.contains("(tried 5 times)") && e.message.contains("This user runs "), "{}", e.message);
        assert!(waited >= resources::BACKOFF.iter().sum::<Duration>() && waited < Duration::from_secs(5), "{waited:?}");
        assert_eq!(lock(&host.groups.0).short, usize::MAX - 5, "five starts, no more");
        assert!(!host.ws.join("ran").exists());
        assert_eq!(host.admission.running(), 0, "its turn went back");

        // Cancelled while it waits to start again: it stops at once and never runs.
        let host = Host::new(usize::MAX);
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let (e, ()) = tokio::join!(host.run("touch ran", cancel.clone()), async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        });
        let e = e.unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled, "{}", e.message);
        assert!(started.elapsed() < Duration::from_millis(1000), "{:?}", started.elapsed());
        assert_eq!(lock(&host.groups.0).short, usize::MAX - 2);
        assert!(!host.ws.join("ran").exists());
    }

    /// The first two waits of [`resources::BACKOFF`].
    const BACKOFF_FIRST_TWO: Duration = Duration::from_millis(300);

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
        assert_eq!(c.finish(OutputEnd::default()), ("short".to_owned(), false, None));

        let mut c = Capture::default();
        c.push(&vec![b'h'; HEAD_BYTES]);
        for _ in 0..10 {
            c.push(&vec![b'm'; 10_000]);
        }
        c.push(&vec![b't'; TAIL_BYTES]);
        let (text, cut, _) = c.finish(OutputEnd::default());
        assert!(cut);
        assert!(text.starts_with(&"h".repeat(HEAD_BYTES)));
        assert!(text.ends_with(&"t".repeat(TAIL_BYTES)));
        assert!(text.contains("[... 100000 bytes omitted ...]"));
    }
}
