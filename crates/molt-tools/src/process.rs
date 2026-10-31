//! `shell.start`, `shell.read`, `shell.write` and `shell.stop`: a command
//! that runs on after the call that started it, such as a dev server or an
//! interactive program (#110).
//!
//! A process starts as `shell.run`'s commands do: the same sandbox, build
//! directory, environment and process group, registered in the same
//! [`Groups`] so stopping the service stops it too. A task of its own waits
//! for it to exit, for its lifetime to end, for a stop, or for its
//! workspace to go away (a dropped fork), and then kills its whole group.
//! What it prints is kept in a ring of [`KEPT`] bytes; each read takes what
//! came since the last. Once it printed more than the ring holds, all of it
//! is saved too (see `outputs`), so what fell out of the ring can still be
//! read with `shell.output`.
//!
//! A process belongs to the caller and the workspace that started it, and
//! every other call must name both: an id from another attempt's fork, or
//! another service, finds nothing. The id is random, so it cannot be
//! guessed either.

use std::collections::{HashMap, VecDeque};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use molt_api::shell::{
    OutputEnd, OutputStream, ProcessState, ProcessStatus, ReadRequest, StartRequest, StopRequest, StopResponse,
    WriteRequest, MAX_INPUT, MAX_READY,
};
use molt_proto::{ErrorCode, RemoteError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::cargo_cache::CargoCache;
use crate::error::{failed, invalid};
use crate::outputs::{self, Asker, Origin};
use crate::shell::{self, lock, Group, Groups};
use crate::{build_dir, Roots, SandboxPolicy};

/// How long a process may run when the start does not say.
const DEFAULT_LIFETIME_MS: u64 = 30 * 60 * 1000;
/// The longest a process may run: as long as one `shell.run` command.
const MAX_LIFETIME_MS: u64 = 60 * 60 * 1000;
/// How long a start waits for the process when it says nothing else.
const DEFAULT_WAIT_MS: u64 = 1_000;
/// The longest a start waits for its `ready` text.
const MAX_START_WAIT_MS: u64 = 120_000;
/// The longest a read waits for output.
const MAX_READ_WAIT_MS: u64 = 30_000;
/// Processes running at once in one workspace, for one caller.
pub(crate) const MAX_PER_WORKSPACE: usize = 4;
/// Processes running at once in the service. With the eight commands it
/// runs at once, that leaves a build directory slot for each (see `build_dir`).
pub(crate) const MAX_RUNNING: usize = 8;
/// Processes the service remembers, ended ones included; the oldest ended
/// one is forgotten first.
const MAX_KEPT: usize = 32;
/// How long an ended process is remembered when nobody stops it.
const ENDED_KEPT: Duration = Duration::from_secs(10 * 60);
/// Output kept per process: the latest bytes.
pub(crate) const KEPT: usize = 1 << 20;
/// The most output one answer carries: its first [`HEAD`] and last [`TAIL`] bytes.
const HEAD: usize = 8 * 1024;
const TAIL: usize = 24 * 1024;
/// How long a write may wait for a process to take its input.
const WRITE_MS: u64 = 5_000;
/// How often a process's workspace is looked for.
const GONE_EVERY: Duration = Duration::from_secs(1);

/// The processes the service started.
#[derive(Default)]
pub(crate) struct Processes {
    table: Mutex<HashMap<String, Arc<Process>>>,
}

/// Who may use a process: the caller the kernel named, and the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    caller: Option<String>,
    /// The canonical workspace.
    ws: PathBuf,
}

struct Process {
    id: String,
    owner: Owner,
    /// The workspace as the start named it, which still matches once the
    /// directory is gone.
    given: String,
    command: String,
    started: Instant,
    log: Mutex<Log>,
    /// Woken when output arrives and when it ends.
    changed: Notify,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    /// Asks its task to stop it.
    stop: CancellationToken,
    /// Cancelled once it ended and its task cleaned up.
    ended: CancellationToken,
}

#[derive(Default)]
struct Log {
    /// The latest output, ending at `total`.
    kept: VecDeque<u8>,
    total: u64,
    /// Where the next read starts.
    read: u64,
    /// The text that means it is ready, while it has not appeared.
    ready: Option<Vec<u8>>,
    /// Whether it appeared; none without a ready text.
    is_ready: Option<bool>,
    end: Option<End>,
    ended_at: Option<Instant>,
    /// Where to save all of it, until the ring is about to drop some.
    save: Option<(Arc<outputs::Store>, Arc<Origin>)>,
    saving: Option<outputs::Writer>,
}

#[derive(Clone, Copy)]
struct End {
    status: ProcessStatus,
    exit_code: Option<i32>,
    signal: Option<i32>,
}

impl Log {
    fn push(&mut self, bytes: &[u8]) {
        if let Some(ready) = &self.ready {
            // Look in what came with the bytes before it too, so the text may be split across reads.
            let back = (ready.len() - 1).min(self.kept.len());
            let mut window: Vec<u8> = self.kept.range(self.kept.len() - back..).copied().collect();
            window.extend_from_slice(bytes);
            if window.windows(ready.len()).any(|w| w == ready.as_slice()) {
                self.ready = None;
                self.is_ready = Some(true);
            }
        }
        match &mut self.saving {
            Some(writer) => writer.write(bytes),
            // Nothing fell out of the ring yet: it holds all of it.
            None if self.total + bytes.len() as u64 > KEPT as u64 => {
                if let Some((store, origin)) = self.save.take() {
                    let mut first: Vec<u8> = self.kept.iter().copied().collect();
                    first.extend_from_slice(bytes);
                    self.saving = store.create(origin, OutputStream::Combined, &first);
                }
            }
            None => {}
        }
        self.total += bytes.len() as u64;
        self.kept.extend(bytes);
        let excess = self.kept.len().saturating_sub(KEPT);
        self.kept.drain(..excess);
    }

    fn unread(&self) -> bool {
        self.read < self.total
    }

    /// What came since the last read, which it now is.
    fn take(&mut self) -> (String, u64, u64, u64) {
        let offset = self.read;
        let first = self.total - self.kept.len() as u64;
        let lost = first.saturating_sub(offset);
        let from = self.kept.len() - (self.total - offset.max(first)) as usize;
        let new: Vec<u8> = self.kept.range(from..).copied().collect();
        self.read = self.total;
        if new.len() <= HEAD + TAIL {
            return (String::from_utf8_lossy(&new).into_owned(), offset, self.total, lost);
        }
        let cut = (new.len() - HEAD - TAIL) as u64;
        let mut text = String::from_utf8_lossy(&new[..HEAD]).into_owned();
        text.push_str(&format!("\n[... {cut} bytes omitted ...]\n"));
        text.push_str(&String::from_utf8_lossy(&new[new.len() - TAIL..]));
        (text, offset, self.total, lost + cut)
    }
}

impl Process {
    /// Its state, with the output since the last read.
    fn state(&self) -> ProcessState {
        let mut log = lock(&self.log);
        let (output, offset, next, omitted) = log.take();
        let end = log.end;
        ProcessState {
            id: self.id.clone(),
            command: self.command.clone(),
            status: end.map_or(ProcessStatus::Running, |e| e.status),
            exit_code: end.and_then(|e| e.exit_code),
            signal: end.and_then(|e| e.signal),
            ready: log.is_ready,
            output,
            offset,
            next,
            omitted,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            saved: log.saving.as_ref().map(|writer| writer.handle().saved()),
        }
    }

    fn running(&self) -> bool {
        lock(&self.log).end.is_none()
    }

    /// Wait until `done` says so, it ends, `wait` passes or `cancel` fires.
    async fn wait_for(&self, wait: Duration, cancel: &CancellationToken, done: impl Fn(&Log) -> bool) {
        let until = tokio::time::Instant::now() + wait;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let log = lock(&self.log);
                if log.end.is_some() || done(&log) {
                    return;
                }
            }
            tokio::select! {
                _ = &mut changed => {}
                _ = tokio::time::sleep_until(until) => return,
                _ = cancel.cancelled() => return,
            }
        }
    }

    /// Stop it, and wait for its task to clean up.
    async fn halt(&self) {
        self.stop.cancel();
        // Its task kills the group after a grace, so this is a bound, not a wait it should reach.
        let _ = tokio::time::timeout(shell::GRACE * 6, self.ended.cancelled()).await;
    }
}

impl Processes {
    /// Stop every process: the service is stopping.
    pub(crate) fn stop_all(&self) {
        for process in lock(&self.table).values() {
            process.stop.cancel();
        }
    }

    /// The process `id` that `caller` started in `workspace`.
    fn find(
        &self,
        roots: &Roots,
        caller: Option<&str>,
        workspace: &str,
        id: &str,
    ) -> Result<Arc<Process>, RemoteError> {
        let ws = roots.workspace(workspace).ok();
        lock(&self.table)
            .get(id)
            .filter(|p| {
                p.owner.caller.as_deref() == caller && (p.given == workspace || Some(&p.owner.ws) == ws.as_ref())
            })
            .cloned()
            .ok_or_else(|| {
                invalid(format!(
                    "there is no process {id} in this workspace: it was stopped and forgotten, or was never started \
                     here; start it again"
                ))
            })
    }

    /// Forget ended processes nobody stopped once they are old, and the
    /// oldest ended ones while too many are kept.
    fn prune(table: &mut HashMap<String, Arc<Process>>) {
        table.retain(|_, p| lock(&p.log).ended_at.is_none_or(|at| at.elapsed() < ENDED_KEPT));
        while table.len() >= MAX_KEPT {
            let oldest = table
                .values()
                .filter_map(|p| lock(&p.log).ended_at.map(|at| (at, p.id.clone())))
                .min()
                .map(|(_, id)| id);
            match oldest {
                Some(id) => table.remove(&id),
                None => break,
            };
        }
    }
}

/// The service's side of the four methods.
pub(crate) struct Ctx<'a> {
    pub roots: &'a Arc<Roots>,
    pub program: &'a Path,
    pub groups: &'a Arc<Groups>,
    pub processes: &'a Arc<Processes>,
    pub sandbox: Option<(&'a SandboxPolicy, Option<Arc<CargoCache>>)>,
    /// The caller the kernel named; none when the service is called directly.
    pub caller: Option<&'a str>,
    /// Where output is saved, and who asks.
    pub outputs: Option<(&'a Arc<outputs::Store>, &'a Asker)>,
}

pub(crate) async fn start(
    cx: Ctx<'_>,
    req: StartRequest,
    cancel: CancellationToken,
) -> Result<ProcessState, RemoteError> {
    if req.command.trim().is_empty() {
        return Err(invalid("command is empty: give the command to start"));
    }
    let ready = req.ready.filter(|r| !r.is_empty());
    if ready.as_ref().is_some_and(|r| r.len() > MAX_READY) {
        return Err(invalid(format!("ready is over {MAX_READY} bytes: give a short piece of the line it prints")));
    }
    let (policy, cargo) = match cx.sandbox {
        Some((policy, cargo)) => (Some(policy), cargo),
        None => (None, None),
    };
    // Its build directory is its own until it ends, as a command's is.
    let (ws, build) = {
        let (roots, ws) = (cx.roots.clone(), req.workspace.clone());
        tokio::task::spawn_blocking(move || {
            let ws = roots.workspace(&ws)?;
            let build = build_dir::acquire(&roots, &ws, cargo.as_ref())?;
            Ok::<_, RemoteError>((ws, build))
        })
        .await
        .map_err(|e| failed(format!("shell.start failed: {e}")))??
    };
    let owner = Owner { caller: cx.caller.map(str::to_owned), ws: ws.clone() };
    let lifetime = Duration::from_millis(req.lifetime_ms.unwrap_or(DEFAULT_LIFETIME_MS).clamp(1, MAX_LIFETIME_MS));
    let (mut cmd, guard) = shell::command(cx.roots, cx.program, policy, &ws, &build, &req.command)?;
    cmd.stdin(Stdio::piped());

    let process = {
        // Counted and added under one lock, so two starts cannot both take the last place.
        let mut table = lock(&cx.processes.table);
        Processes::prune(&mut table);
        let running: Vec<&Arc<Process>> = table.values().filter(|p| p.running()).collect();
        if running.iter().filter(|p| p.owner == owner).count() >= MAX_PER_WORKSPACE {
            return Err(invalid(format!(
                "{MAX_PER_WORKSPACE} processes already run in this workspace: stop one before starting another"
            )));
        }
        if running.len() >= MAX_RUNNING || table.len() >= MAX_KEPT {
            return Err(invalid(format!(
                "the shell service already runs {} processes, as many as it may: stop one, or wait for one to end",
                running.len()
            )));
        }
        let (mut child, group) = Group::spawn(&mut cmd, cx.groups, &cancel, guard.as_ref().map(|g| g.info.clone()))?;
        let id = format!("proc_{:032x}", rand::random::<u128>());
        let is_ready = ready.as_ref().map(|_| false);
        let save =
            cx.outputs.map(|(store, asker)| (store.clone(), Arc::new(asker.origin(&ws, &req.workspace, &req.command))));
        let log = Log { ready: ready.map(String::into_bytes), is_ready, save, ..Log::default() };
        let process = Arc::new(Process {
            id: id.clone(),
            owner,
            given: req.workspace.clone(),
            command: req.command.clone(),
            started: Instant::now(),
            log: Mutex::new(log),
            changed: Notify::new(),
            stdin: tokio::sync::Mutex::new(child.stdin.take()),
            stop: CancellationToken::new(),
            ended: CancellationToken::new(),
        });
        table.insert(id, process.clone());
        let outputs = (child.stdout.take(), child.stderr.take());
        tokio::spawn(supervise(process.clone(), child, group, outputs, lifetime, ws, (guard, build)));
        process
    };

    let wait = Duration::from_millis(req.wait_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_START_WAIT_MS));
    process.wait_for(wait, &cancel, |log| log.is_ready == Some(true)).await;
    if cancel.is_cancelled() {
        // Nobody was told its id, so nobody else could stop it.
        process.halt().await;
        lock(&cx.processes.table).remove(&process.id);
        return Err(RemoteError { code: ErrorCode::Cancelled, message: "shell.start cancelled".into() });
    }
    Ok(process.state())
}

pub(crate) async fn read(
    cx: Ctx<'_>,
    req: ReadRequest,
    cancel: CancellationToken,
) -> Result<ProcessState, RemoteError> {
    let process = cx.processes.find(cx.roots, cx.caller, &req.workspace, &req.id)?;
    let wait = Duration::from_millis(req.wait_ms.unwrap_or(0).min(MAX_READ_WAIT_MS));
    if !wait.is_zero() {
        process.wait_for(wait, &cancel, Log::unread).await;
    }
    Ok(process.state())
}

pub(crate) async fn write(cx: Ctx<'_>, req: WriteRequest) -> Result<ProcessState, RemoteError> {
    if req.input.len() > MAX_INPUT {
        return Err(invalid(format!("input is over {MAX_INPUT} bytes: write it in smaller pieces")));
    }
    let process = cx.processes.find(cx.roots, cx.caller, &req.workspace, &req.id)?;
    let ended = |p: &Process| {
        let state = lock(&p.log).end;
        state.map(|end| match (end.status, end.exit_code, end.signal) {
            (ProcessStatus::Exited, Some(code), _) => format!("exited with code {code}"),
            (ProcessStatus::Exited, None, Some(signal)) => format!("was killed by signal {signal}"),
            (ProcessStatus::Expired, ..) => "ran out of time and was stopped".to_owned(),
            _ => "was stopped".to_owned(),
        })
    };
    if let Some(how) = ended(&process) {
        return Err(invalid(format!("process {} {how}: it takes no more input", req.id)));
    }
    let mut stdin = process.stdin.lock().await;
    let Some(pipe) = stdin.as_mut() else {
        return Err(invalid(format!("process {}'s input was closed: it takes no more", req.id)));
    };
    let written = tokio::time::timeout(Duration::from_millis(WRITE_MS), async {
        pipe.write_all(req.input.as_bytes()).await?;
        pipe.flush().await
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            stdin.take();
            let how = ended(&process).unwrap_or_else(|| "closed its input".to_owned());
            return Err(invalid(format!("process {} {how}: the input was not all written", req.id)));
        }
        Err(_) => {
            return Err(failed(format!(
                "process {} did not take the input within {} s: it is not reading it",
                req.id,
                WRITE_MS / 1000
            )))
        }
    }
    if req.close {
        stdin.take();
    }
    drop(stdin);
    Ok(process.state())
}

pub(crate) async fn stop(cx: Ctx<'_>, req: StopRequest) -> Result<StopResponse, RemoteError> {
    let Some(id) = req.id else {
        // All of the workspace's, which are forgotten too: the caller is done with it.
        let ws = cx.roots.workspace(&req.workspace).ok();
        let caller = cx.caller.map(str::to_owned);
        let mine: Vec<Arc<Process>> = {
            let mut table = lock(&cx.processes.table);
            let ids: Vec<String> = table
                .values()
                .filter(|p| p.owner.caller == caller && (p.given == req.workspace || Some(&p.owner.ws) == ws.as_ref()))
                .map(|p| p.id.clone())
                .collect();
            ids.iter().filter_map(|id| table.remove(id)).collect()
        };
        for process in &mine {
            process.stop.cancel();
        }
        for process in &mine {
            process.halt().await;
        }
        return Ok(StopResponse { stopped: mine.iter().map(|p| p.state()).collect() });
    };
    // Stopping one that already ended answers as the first stop did.
    let process = cx.processes.find(cx.roots, cx.caller, &req.workspace, &id)?;
    process.halt().await;
    Ok(StopResponse { stopped: vec![process.state()] })
}

/// Wait for the process to end or be stopped, then kill its group and say
/// how it ended. `_held` is its sandbox guard and build directory.
async fn supervise(
    process: Arc<Process>,
    mut child: tokio::process::Child,
    group: Group,
    outputs: (Option<tokio::process::ChildStdout>, Option<tokio::process::ChildStderr>),
    lifetime: Duration,
    ws: PathBuf,
    _held: (Option<crate::sandbox::Guard>, build_dir::BuildDir),
) {
    let mut readers = tokio::task::JoinSet::new();
    readers.spawn(drain(outputs.0, process.clone()));
    readers.spawn(drain(outputs.1, process.clone()));
    let gone = async {
        let mut every = tokio::time::interval(GONE_EVERY);
        loop {
            every.tick().await;
            if !ws.is_dir() {
                return;
            }
        }
    };
    let (status, how) = tokio::select! {
        biased;
        // Asked to stop, it was stopped, even when the signal that ended it came first.
        _ = process.stop.cancelled() => (shell::terminate(&group, &mut child).await, ProcessStatus::Stopped),
        status = child.wait() => (status, ProcessStatus::Exited),
        _ = tokio::time::sleep(lifetime) => (shell::terminate(&group, &mut child).await, ProcessStatus::Expired),
        _ = gone => (shell::terminate(&group, &mut child).await, ProcessStatus::Stopped),
    };
    // Whatever it started goes with it, as after a command.
    group.kill();
    let _ = tokio::time::timeout(shell::GRACE, async { while readers.join_next().await.is_some() {} }).await;
    readers.abort_all();
    process.stdin.lock().await.take();
    let (exit_code, signal) = match (&status, how) {
        (Ok(status), ProcessStatus::Exited) => (status.code(), status.signal()),
        _ => (None, None),
    };
    if let Err(e) = &status {
        tracing::warn!(process = %process.id, error = %e, "could not wait for a process");
    }
    {
        let mut log = lock(&process.log);
        log.end = Some(End { status: how, exit_code, signal });
        log.ended_at = Some(Instant::now());
        let end = OutputEnd {
            exit_code,
            signal,
            timed_out: how == ProcessStatus::Expired,
            cancelled: how == ProcessStatus::Stopped,
        };
        if let Some(writer) = &mut log.saving {
            writer.close(Some(end));
        }
    }
    drop(group);
    process.changed.notify_waiters();
    process.ended.cancel();
}

async fn drain(pipe: Option<impl AsyncRead + Unpin>, process: Arc<Process>) {
    let Some(mut pipe) = pipe else { return };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                lock(&process.log).push(&buf[..n]);
                process.changed.notify_waiters();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_takes_what_came_since_the_last_and_says_what_it_lost() {
        let mut log = Log::default();
        log.push(b"one\n");
        assert_eq!(log.take(), ("one\n".to_owned(), 0, 4, 0));
        assert_eq!(log.take(), (String::new(), 4, 4, 0));
        // More than is kept: the oldest is lost, and the rest is cut to its head and tail.
        log.push(&vec![b'x'; KEPT + 10]);
        let (text, offset, next, omitted) = log.take();
        assert_eq!((offset, next), (4, 4 + KEPT as u64 + 10));
        assert_eq!(omitted, 10 + (KEPT - HEAD - TAIL) as u64);
        assert!(
            text.contains(&format!("[... {} bytes omitted ...]", KEPT - HEAD - TAIL)),
            "{}",
            &text[HEAD..HEAD + 40]
        );
        assert!(text.len() < HEAD + TAIL + 64);
    }

    #[test]
    fn the_ready_text_is_found_even_when_it_arrives_in_pieces() {
        let mut log = Log { ready: Some(b"listening on".to_vec()), is_ready: Some(false), ..Log::default() };
        log.push(b"starting\nlisten");
        assert_eq!(log.is_ready, Some(false));
        log.push(b"ing on :8080\n");
        assert_eq!(log.is_ready, Some(true));
    }
}
