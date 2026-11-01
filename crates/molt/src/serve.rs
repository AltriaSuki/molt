//! `molt serve`: the agent services of one workspace, for clients that speak
//! the [`protocol`] (RFC 0004). The interface in the terminal is one: it
//! starts a server of its own on a pipe (`molt ui-backend`) and talks to it
//! only through the protocol. `molt serve` itself listens on a Unix socket
//! only its user can open, or speaks on its standard input and output.
//!
//! A connection opens at most one session at a time, and only one session
//! is open in a server: one kernel per data dir. Whatever a client asks is
//! what the interface could ask; the config, the workspace and the
//! capabilities come from the server's own start, never from a client.
//!
//! A connection may also hold a stored session (RFC 0006): its tasks, their
//! replies, the questions and the person's decisions go into that
//! session's journal, each before the request it records is answered, so a
//! later connection or process loads them again.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{ensure, Context};
use molt_api::planner::{Constraint, Control, RunRequest};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::project::Project;
use crate::protocol::{self as p, code, method, Error, Id, Notification, Response, ServerMessage};
use crate::session::Session;
use crate::sessions::{self, Decision, Entry, Sessions, Writer};
use crate::{agent, Config};

/// Where and how the services run: the flags `molt`, `molt ui` and `molt serve` share.
pub struct Options {
    pub config: Option<PathBuf>,
    pub workspace: PathBuf,
    /// A project of the registry, by id or name, in place of `workspace`.
    pub project: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub pass_env: Vec<String>,
    /// Dependency caches commands build from offline (`--deps-cache`).
    pub deps_cache: Vec<String>,
    /// Hosts the agent may read web pages from.
    pub allow_web: Vec<String>,
    pub web_resolver: Option<String>,
    /// The `[profile.NAME]` of the config to take defaults from.
    pub profile: Option<String>,
    /// The session the runs belong to (`--session`); without one, the
    /// interface names one for each time it is opened, and `molt serve` one
    /// for each client.
    pub session: Option<String>,
}

/// The config of `opts`, the workspace it is for and its project.
pub fn configure(opts: Options) -> anyhow::Result<(Config, String, Option<Project>)> {
    let (workspace, project) = agent::locate(opts.project.as_deref(), &opts.workspace, opts.config.is_some())?;
    let path = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    let mut cfg = agent::resolve_config(
        opts.config.as_deref(),
        &workspace,
        opts.data_dir.as_deref(),
        project.as_ref(),
        opts.profile.as_deref(),
    )?;
    if let Some(shell) = cfg.service_mut("shell") {
        shell.pass_env.extend(opts.pass_env);
    }
    agent::deps_cache(&mut cfg, &opts.deps_cache)?;
    agent::environments(&mut cfg)?;
    agent::mcp(&mut cfg)?;
    // `--pass-env` and the MCP bridges come after the config: neither may be given an endpoint's key.
    cfg.check_endpoint_keys(&|name| std::env::var(name).ok())?;
    agent::allow_web(&mut cfg, &opts.allow_web)?;
    if let Some(resolver) = &opts.web_resolver {
        cfg.web_resolver(resolver)?;
    }
    if std::env::var_os("RUST_LOG").is_none() {
        for svc in &mut cfg.services {
            svc.env.entry("RUST_LOG".into()).or_insert_with(|| "warn".into());
        }
    }
    Ok((cfg, path, project))
}

/// Progress events queued for a client that does not read; see [`Outbox`].
const QUEUED: usize = 64;
/// How long a closing connection waits for its last answers to be written.
const FLUSH: Duration = Duration::from_secs(1);
/// How long an interrupt waits before it lists a fork's changes again,
/// after the shell's grace for a stopped command (TERM, then KILL 500 ms on).
const RELIST_AFTER: Duration = Duration::from_secs(1);

pub struct Server {
    cfg: Arc<Config>,
    workspace: String,
    /// A connection has a session open or starting.
    in_use: Arc<AtomicBool>,
    /// The session of a run whose request names none; `None`: one of its
    /// own for each connection.
    session: Option<String>,
}

impl Server {
    pub fn new(cfg: Config, workspace: String) -> Arc<Self> {
        Self::named(cfg, workspace, None)
    }

    /// A server whose runs that name no session go into `session`.
    pub fn named(cfg: Config, workspace: String, session: Option<String>) -> Arc<Self> {
        Arc::new(Self { cfg: Arc::new(cfg), workspace, in_use: Arc::default(), session })
    }

    /// Serve one client on `input` and `output` until it goes, or `stop`.
    /// A client that goes without closing its session leaves every fork:
    /// it may have been waiting on one. On `stop`, the forks of results not
    /// applied or discarded are kept too.
    pub async fn connection<R, W>(self: &Arc<Self>, input: R, output: W, stop: CancellationToken) -> anyhow::Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let behind = Arc::new(AtomicUsize::new(0));
        let outbox = Outbox { tx, behind: behind.clone() };
        let mut writer = tokio::spawn(async move {
            let mut output = output;
            while let Some(Out::Message(msg)) = rx.recv().await {
                if matches!(msg, ServerMessage::Notification(_)) {
                    behind.fetch_sub(1, Ordering::SeqCst);
                }
                p::write_line(&mut output, &msg).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        // A reader of its own owns each whole line: selecting on the session
        // and `stop` must not cancel one half read.
        let (lines, mut incoming) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut input = tokio::io::BufReader::new(input);
            loop {
                let mut line = Vec::new();
                let read = p::read_line(&mut input, &mut line).await.map(|more| more.then_some(line));
                let ended = !matches!(read, Ok(Some(_)));
                if lines.send(read).is_err() || ended {
                    break;
                }
            }
        });
        let (sessions, mut started) = mpsc::unbounded_channel();
        let mut conn = Connection {
            run_session: self.session.clone().unwrap_or_else(crate::runs::new_session),
            server: self.clone(),
            tx: outbox.clone(),
            sessions,
            session: None,
            starting: None,
            initialized: false,
            work: Vec::new(),
            commands: HashMap::new(),
            before: None,
            forks: Arc::default(),
            stored: Sessions::new(&self.cfg.kernel.data_dir),
            held: Arc::default(),
        };
        let end = loop {
            tokio::select! {
                line = incoming.recv() => match line.unwrap_or(Ok(None)) {
                    Ok(Some(line)) => {
                        if let Some((id, keep)) = conn.handle(&line) {
                            conn.abort_work();
                            if conn.starting.is_some() {
                                if let Some((open, session)) = started.recv().await {
                                    conn.started(open, session);
                                }
                            }
                            conn.close(keep).await;
                            conn.respond(id, Ok(p::Empty {}));
                        }
                    }
                    Ok(None) => break End::Gone,
                    Err(e) => break End::Failed(e),
                },
                Some((open, session)) = started.recv() => conn.started(open, session),
                _ = stop.cancelled() => break End::Stopped,
            }
        };
        conn.abort_work();
        if conn.starting.is_some() {
            if let Some((open, session)) = started.recv().await {
                conn.started(open, session);
            }
        }
        match end {
            End::Stopped => {
                let held = conn.forks.lock().unwrap_or_else(PoisonError::into_inner).held();
                conn.close(held).await;
            }
            End::Gone | End::Failed(_) => {
                if let Some(session) = conn.session.take() {
                    session.abandon().await;
                    self.in_use.store(false, Ordering::SeqCst);
                }
            }
        }
        reader.abort();
        // The session is kept as it is; another connection may hold it now.
        held(&conn.held).take();
        drop(conn);
        // The last answers are written, unless the client is not reading.
        let _ = outbox.tx.send(Out::End);
        if tokio::time::timeout(FLUSH, &mut writer).await.is_err() {
            writer.abort();
        }
        match end {
            End::Failed(e) => Err(e).context("reading from the client"),
            End::Gone | End::Stopped => Ok(()),
        }
    }
}

/// Where answers and progress events wait for the writer. Past [`QUEUED`]
/// events waiting, more are dropped (their sequence numbers show the gap);
/// answers always wait, so they keep their settlement. A client that stops
/// reading cannot block the bus.
#[derive(Clone)]
struct Outbox {
    tx: UnboundedSender<Out>,
    /// Progress events queued and not written yet.
    behind: Arc<AtomicUsize>,
}

enum Out {
    Message(ServerMessage),
    /// The connection is over: what came before is written, then the writer stops.
    End,
}

impl Outbox {
    fn send(&self, msg: ServerMessage) {
        if matches!(msg, ServerMessage::Notification(_)) && self.behind.fetch_add(1, Ordering::SeqCst) >= QUEUED {
            self.behind.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        let _ = self.tx.send(Out::Message(msg));
    }
}

enum End {
    /// The client closed its side.
    Gone,
    /// What it sent could not be read as messages.
    Failed(std::io::Error),
    Stopped,
}

/// The forks this connection's session answered with, for what to keep when it ends.
#[derive(Default)]
struct Forks {
    /// Results answered with and neither applied nor discarded.
    results: BTreeSet<String>,
    /// Named by `result.keep`.
    kept: BTreeSet<String>,
}

impl Forks {
    fn held(&self) -> Vec<String> {
        self.results.union(&self.kept).cloned().collect()
    }
}

/// A call in flight: answered once, by the call or by what stops it.
struct Work {
    id: Option<Id>,
    answered: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// A session that came up, or why not, for the `session.open` of that id.
type Started = (Option<Id>, Result<Arc<Session>, String>);

/// The stored session a connection holds, if any (RFC 0006).
type Held = Arc<Mutex<Option<Writer>>>;

fn held(held: &Held) -> std::sync::MutexGuard<'_, Option<Writer>> {
    held.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Add what `entry` makes of the stored session `id` to its journal, if this
/// connection still holds it. A failure is logged: the work it records is
/// done, and its answer still goes to the client.
fn record(held_session: &Held, id: &str, entry: impl FnOnce(&sessions::Loaded) -> Option<Entry>) {
    let mut guard = held(held_session);
    let Some(writer) = guard.as_mut().filter(|w| w.id() == id) else { return };
    let Some(entry) = entry(writer.loaded()) else { return };
    if let Err(e) = writer.append(entry) {
        tracing::warn!("session {id} could not record what happened: {e:#}");
    }
}

/// The decision on `fork`, when the fork holds one of the stored session's results.
fn decided(fork: &str, decision: Decision, paths: Vec<String>) -> impl FnOnce(&sessions::Loaded) -> Option<Entry> + '_ {
    move |loaded| {
        loaded.results.iter().any(|r| r.fork == fork).then(|| Entry::Decided { fork: fork.to_owned(), decision, paths })
    }
}

/// A refusal to hold or remove a session: another client holds it, or why not.
fn refused(e: anyhow::Error) -> Error {
    let busy = e.downcast_ref::<sessions::Busy>().is_some();
    Error::new(if busy { code::BUSY } else { code::FAILED }, format!("{e:#}"))
}

struct Connection {
    server: Arc<Server>,
    /// The session of the runs this client asks for that name none.
    run_session: String,
    tx: Outbox,
    sessions: UnboundedSender<Started>,
    session: Option<Arc<Session>>,
    /// The `session.open` a session is starting for; it comes on `sessions`.
    starting: Option<Option<Id>>,
    initialized: bool,
    /// Calls in flight, stopped by `task.stop` and `session.close`.
    work: Vec<Work>,
    /// The person's commands running, by trace: what stops each.
    commands: HashMap<String, CancellationToken>,
    /// The work tree as it was when the task `.0` started, for `commit.make`.
    before: Option<(String, anyhow::Result<crate::git::Before>)>,
    forks: Arc<Mutex<Forks>>,
    /// The stored sessions of the data dir, and the one this connection holds.
    stored: Sessions,
    held: Held,
}

fn failed(e: impl std::fmt::Display) -> Error {
    Error::new(code::FAILED, e.to_string())
}

fn no_session() -> Error {
    Error::new(code::NO_SESSION, "the services are not up")
}

fn value(result: Result<impl Serialize, Error>) -> Result<Value, Error> {
    result.and_then(|v| serde_json::to_value(v).map_err(|e| Error::new(code::FAILED, e.to_string())))
}

fn params<T: DeserializeOwned>(params: Value) -> Result<T, Error> {
    // Methods without parameters take them left out, as `null` or as `{}`.
    let params = if params.is_null() { Value::Object(Default::default()) } else { params };
    serde_json::from_value(params).map_err(|e| Error::new(code::INVALID_PARAMS, format!("invalid params: {e}")))
}

impl Connection {
    fn respond(&self, id: Option<Id>, result: Result<impl Serialize, Error>) {
        respond(&self.tx, id, value(result));
    }

    /// Carry out the request `line`. A `session.close` comes back to the
    /// loop, which waits on what is starting before it closes.
    fn handle(&mut self, line: &[u8]) -> Option<(Option<Id>, Vec<String>)> {
        self.work.retain(|w| !w.task.is_finished());
        let message: Value = match serde_json::from_slice(line) {
            Ok(message) => message,
            Err(e) => {
                let error = Error::new(code::PARSE, format!("not JSON: {e}"));
                self.tx.send(ServerMessage::Response(Response::new(None, Err(error))));
                return None;
            }
        };
        let id = message.get("id").and_then(|id| serde_json::from_value::<Id>(id.clone()).ok());
        let request = match serde_json::from_value::<p::Request>(message) {
            Ok(request) if request.jsonrpc == "2.0" => request,
            Ok(_) => {
                let error = Error::new(code::INVALID_REQUEST, "only JSON-RPC 2.0 is spoken: say \"jsonrpc\": \"2.0\"");
                self.tx.send(ServerMessage::Response(Response::new(id, Err(error))));
                return None;
            }
            Err(e) => {
                let error = Error::new(code::INVALID_REQUEST, format!("not a request: {e}"));
                self.tx.send(ServerMessage::Response(Response::new(id, Err(error))));
                return None;
            }
        };
        let p::Request { id, method, params: args, .. } = request;
        if method != method::INITIALIZE && !self.initialized {
            self.respond(id, Err::<(), _>(Error::new(code::NOT_INITIALIZED, "call initialize first")));
            return None;
        }
        if let Err(error) = self.dispatch(id.clone(), &method, args) {
            match error {
                Dispatch::Close(keep) => return Some((id, keep)),
                Dispatch::Error(error) => self.respond(id, Err::<(), _>(error)),
            }
        }
        None
    }

    fn dispatch(&mut self, id: Option<Id>, name: &str, args: Value) -> Result<(), Dispatch> {
        match name {
            method::INITIALIZE => {
                let hello: p::Initialize = params(args)?;
                if hello.protocol != p::VERSION {
                    return Err(Dispatch::Error(Error::new(
                        code::INVALID_PARAMS,
                        format!(
                            "this server speaks version {} of the protocol, not {}: use a client of the same molt",
                            p::VERSION,
                            hello.protocol
                        ),
                    )));
                }
                self.initialized = true;
                self.respond(
                    id,
                    Ok(p::Initialized {
                        protocol: p::VERSION,
                        server: p::Client { name: "molt".into(), version: env!("CARGO_PKG_VERSION").into() },
                        workspace: self.server.workspace.clone(),
                        methods: method::ALL.iter().chain(&method::ADDED).map(|m| m.to_string()).collect(),
                    }),
                );
            }
            method::SESSION_OPEN => {
                let p::Empty {} = params(args)?;
                self.open(id);
            }
            method::SESSION_CLOSE => {
                let p::Keep { keep } = params(args)?;
                return Err(Dispatch::Close(keep));
            }
            method::TASK_DESIGN => {
                let p::Task { trace, mut request } = params(args)?;
                self.own(&mut request)?;
                self.call(id, move |s, _| async move {
                    let design = s.design(&request, &trace).await.map_err(|e| failed(format!("{e:#}")))?;
                    // Which check files would replace the workspace's own, for a client that stops there.
                    let root = Path::new(s.workspace());
                    let existing = design
                        .files
                        .iter()
                        .filter(|f| root.join(&f.path).symlink_metadata().is_ok())
                        .map(|f| f.path.clone())
                        .collect();
                    Ok(p::Designed { design, existing })
                });
            }
            method::TASK_SUBMIT => {
                let p::Task { trace, mut request } = params(args)?;
                self.own(&mut request)?;
                let stored = self.record_task(&trace, &mut request)?;
                self.call(id, move |s, forks| async move {
                    let resp = s.run(&request, &trace).await.map_err(|e| format!("{e:#}"));
                    if let Some((held_session, sid)) = &stored {
                        let run = trace.to_string();
                        record(held_session, sid, |_| {
                            Some(match &resp {
                                Ok(resp) => Entry::Reply(Box::new(sessions::Reply::of(&run, &request.task, resp))),
                                Err(e) => Entry::Failed { run: run.clone(), error: e.clone() },
                            })
                        });
                    }
                    let resp = resp.map_err(failed)?;
                    if let Some(fork) = &resp.fork {
                        forks.lock().unwrap_or_else(PoisonError::into_inner).results.insert(fork.clone());
                    }
                    Ok(resp)
                });
            }
            method::TASK_PLAN => {
                let p::Plan { trace, mut request } = params(args)?;
                self.own(&mut request.run)?;
                self.call(id, move |s, forks| async move {
                    let resp = s.plan(&request, &trace).await.map_err(|e| failed(format!("{e:#}")))?;
                    if let Some(fork) = resp.result.as_ref().and_then(|r| r.fork.as_ref()) {
                        forks.lock().unwrap_or_else(PoisonError::into_inner).results.insert(fork.clone());
                    }
                    Ok(resp)
                });
            }
            method::TASK_STEER => {
                let p::Steer { trace, constraint } = params(args)?;
                self.control(id, trace, Control::Constrain(constraint));
            }
            method::QUESTION_ANSWER => {
                let p::Answer { trace, question, text } = params(args)?;
                let answer = Constraint { text: text.clone(), answers: Some(question), ..Default::default() };
                let stored = self.held_id().map(|sid| (self.held.clone(), sid));
                self.call(id, move |s, _| async move {
                    let answered =
                        s.control(&trace, Control::Constrain(answer)).await.map_err(|e| failed(format!("{e:#}")))?;
                    if let Some((held_session, sid)) = &stored {
                        let run = trace.to_string();
                        record(held_session, sid, |loaded| {
                            loaded
                                .questions
                                .iter()
                                .any(|q| q.run == run && q.question == question)
                                .then(|| Entry::Answered { run: run.clone(), question, text })
                        });
                    }
                    Ok(answered)
                });
            }
            method::TASK_PAUSE => {
                let p::Run { trace } = params(args)?;
                self.control(id, trace, Control::Pause);
            }
            method::TASK_RESUME => {
                let p::Run { trace } = params(args)?;
                self.control(id, trace, Control::Resume);
            }
            method::TASK_STOP => {
                let stop: p::Stop = params(args)?;
                self.stop(id, stop);
            }
            method::TASK_LEARN => {
                let p::Learn { trace, model } = params(args)?;
                self.call(id, move |s, _| async move { s.learn(&trace, model).await.map_err(failed) });
            }
            method::RESULT_DIFF => {
                let p::Fork { fork } = params(args)?;
                self.call(id, move |s, _| async move { s.diff(&fork).await.map_err(|e| failed(format!("{e:#}"))) });
            }
            method::RESULT_APPLY => {
                let p::Apply { fork, paths } = params(args)?;
                let stored = self.held_id().map(|sid| (self.held.clone(), sid));
                self.call(id, move |s, forks| async move {
                    let merged = s.apply_only(&fork, &paths).await.map_err(|e| failed(format!("{e:#}")))?;
                    forks.lock().unwrap_or_else(PoisonError::into_inner).results.remove(&fork);
                    if let Some((held_session, sid)) = &stored {
                        record(held_session, sid, decided(&fork, Decision::Applied, paths));
                    }
                    Ok(merged)
                });
            }
            method::RESULT_RECHECK => {
                let p::Recheck { fork, only, check } = params(args)?;
                self.call(id, move |s, _| async move {
                    s.recheck(&fork, only.as_deref(), check.as_ref()).await.map_err(|e| failed(format!("{e:#}")))
                });
            }
            method::RESULT_KEEP => {
                let p::Fork { fork } = params(args)?;
                if let Some(sid) = self.held_id() {
                    record(&self.held, &sid, decided(&fork, Decision::Kept, Vec::new()));
                }
                self.forks.lock().unwrap_or_else(PoisonError::into_inner).kept.insert(fork);
                self.respond(id, Ok(p::Empty {}));
            }
            method::RESULT_DISCARD => {
                let p::Fork { fork } = params(args)?;
                let stored = self.held_id().map(|sid| (self.held.clone(), sid));
                self.call(id, move |s, forks| async move {
                    s.discard(&fork).await.map_err(|e| failed(format!("{e:#}")))?;
                    if let Some((held_session, sid)) = &stored {
                        record(held_session, sid, decided(&fork, Decision::Discarded, Vec::new()));
                    }
                    let mut forks = forks.lock().unwrap_or_else(PoisonError::into_inner);
                    forks.results.remove(&fork);
                    forks.kept.remove(&fork);
                    Ok(p::Empty {})
                });
            }
            method::COMMIT_PREPARE => {
                let p::Run { trace } = params(args)?;
                let before = crate::git::Before::look(Path::new(&self.server.workspace));
                self.before = Some((trace.to_string(), before));
                self.respond(id, Ok(p::Empty {}));
            }
            method::COMMIT_MAKE => {
                let p::Commit { trace, task, response } = params(args)?;
                let before = match self.before.take() {
                    Some((run, before)) if run == trace.as_str() => before,
                    other => {
                        self.before = other;
                        Err(anyhow::anyhow!("/commit was turned on after the task started"))
                    }
                };
                let tx = self.tx.clone();
                // Not among the work a stop ends: a commit half made would leave the index behind.
                tokio::spawn(async move {
                    let made =
                        tokio::task::spawn_blocking(move || before?.commit(&response, trace.as_str(), &task, None))
                            .await
                            .map_err(anyhow::Error::from)
                            .and_then(|made| made);
                    respond(&tx, id, value(made.map_err(|e| failed(format!("{e:#}")))));
                });
            }
            method::RUNS_LIST => {
                let p::Empty {} = params(args)?;
                let (tx, dir, workspace) =
                    (self.tx.clone(), agent::checkpoint_dir(&self.server.cfg), self.server.workspace.clone());
                tokio::task::spawn_blocking(move || {
                    let runs = dir.map_or_else(Vec::new, |dir| list_runs(&dir, &workspace));
                    respond(&tx, id, value(Ok::<_, Error>(runs)));
                });
            }
            method::RUNS_DROP => {
                let p::DropRun { run } = params(args)?;
                self.call(id, move |s, _| async move {
                    s.drop_run(&run).await.map_err(|e| failed(format!("{e:#}")))?;
                    Ok(p::Empty {})
                });
            }
            method::RUNS_UNDO => {
                let p::Undo { run } = params(args)?;
                let (session, tx) = (self.session.clone(), self.tx.clone());
                // A client waits for Git and memory before it quits. Like commits,
                // this is not among the work a stop ends.
                tokio::spawn(async move {
                    let result = match session {
                        Some(session) => session.undo(run).await.map_err(|e| failed(format!("{e:#}"))),
                        None => Err(no_session()),
                    };
                    respond(&tx, id, value(result));
                });
            }
            method::NOTES_LIST => {
                let p::Query { query } = params(args)?;
                self.call(id, move |s, _| async move {
                    s.inspect_notes(&query).await.map_err(|e| failed(format!("{e:#}")))
                });
            }
            method::NOTES_FORGET => {
                let p::NoteReason { id: note, reason } = params(args)?;
                self.call(id, move |s, _| async move {
                    let forgotten = s.forget(&note, &reason).await.map_err(|e| failed(format!("{e:#}")))?;
                    Ok(p::Forgot { forgotten })
                });
            }
            method::NOTES_REMEMBER => {
                let p::Remember { text } = params(args)?;
                self.call(id, move |s, _| async move {
                    let kept = s.remember(&text).await.map_err(|e| failed(format!("{e:#}")))?;
                    Ok(p::NoteId { id: kept.note.id })
                });
            }
            method::NOTES_PROMOTE => {
                let p::NoteReason { id: note, reason } = params(args)?;
                self.call(id, move |s, _| async move {
                    let copy = s.promote(&note, &reason).await.map_err(|e| failed(format!("{e:#}")))?;
                    Ok(p::NoteId { id: copy.id })
                });
            }
            method::NOTES_CHANGE => {
                let p::ChangeNote { review, text } = params(args)?;
                self.call(id, move |s, _| async move {
                    s.change_note(review, text).await.map_err(|e| failed(format!("{e:#}")))
                });
            }
            method::NOTES_USED => {
                let p::UsedNotes { run } = params(args)?;
                self.call(
                    id,
                    move |s, _| async move { s.used_notes(&run).await.map_err(|e| failed(format!("{e:#}"))) },
                );
            }
            method::COMMAND_RUN => {
                let p::Command { trace, command } = params(args)?;
                let stop = CancellationToken::new();
                self.commands.retain(|_, stop| !stop.is_cancelled());
                self.commands.insert(trace.to_string(), stop.clone());
                self.call(id, move |s, _| async move {
                    s.command(&command, &trace, stop).await.map_err(|e| failed(format!("{e:#}")))
                });
            }
            method::SESSIONS_LIST => {
                let p::ListSessions { archived } = params(args)?;
                let (tx, stored, workspace) = (self.tx.clone(), self.stored.clone(), self.server.workspace.clone());
                tokio::task::spawn_blocking(move || {
                    let mut listed = stored.list(&workspace);
                    listed.retain(|s| archived || !s.archived);
                    respond(&tx, id, value(Ok::<_, Error>(listed)));
                });
            }
            method::SESSIONS_NEW => {
                let p::NewSession { title, import } = params(args)?;
                let (tx, stored, held_session) = (self.tx.clone(), self.stored.clone(), self.held.clone());
                let (data, workspace) = (self.server.cfg.kernel.data_dir.clone(), self.server.workspace.clone());
                tokio::task::spawn_blocking(move || {
                    let made = (|| {
                        let saved = match import {
                            true => crate::tui::saved::load_for(&data, Path::new(&workspace))
                                .context("the interface's saved session could not be read, so no session was made")?,
                            false => None,
                        };
                        let mut writer = stored.create(&workspace, title)?;
                        if let Some(saved) = saved {
                            writer.append(Entry::Imported(sessions::imported(saved)))?;
                        }
                        Ok::<_, anyhow::Error>(writer)
                    })();
                    let result = made.map_err(|e| failed(format!("{e:#}"))).map(|writer| {
                        let session = writer.loaded().clone();
                        *held(&held_session) = Some(writer);
                        p::StoredSession { session, held: true }
                    });
                    respond(&tx, id, value(result));
                });
            }
            method::SESSIONS_LOAD => {
                let p::SessionId { id: sid } = params(args)?;
                if let Some(writer) = held(&self.held).as_ref().filter(|w| w.id() == sid) {
                    self.respond(id, Ok(p::StoredSession { session: writer.loaded().clone(), held: true }));
                    return Ok(());
                }
                let (tx, stored, held_session) = (self.tx.clone(), self.stored.clone(), self.held.clone());
                let workspace = self.server.workspace.clone();
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        ours(&stored, &sid, &workspace)?;
                        // Read first: an archived session is shown, not held.
                        let session = stored.load(&sid).map_err(|e| failed(format!("{e:#}")))?;
                        if session.archived {
                            return Ok(p::StoredSession { session, held: false });
                        }
                        let writer = stored.open(&sid).map_err(refused)?;
                        let session = writer.loaded().clone();
                        *held(&held_session) = Some(writer);
                        Ok(p::StoredSession { session, held: true })
                    })();
                    respond(&tx, id, value(result));
                });
            }
            method::SESSIONS_ARCHIVE => {
                let p::ArchiveSession { id: sid, restore } = params(args)?;
                let (tx, stored, held_session) = (self.tx.clone(), self.stored.clone(), self.held.clone());
                let workspace = self.server.workspace.clone();
                tokio::task::spawn_blocking(move || {
                    let entry = if restore { Entry::Restored } else { Entry::Archived };
                    let result = (|| {
                        let mut guard = held(&held_session);
                        if let Some(writer) = guard.as_mut().filter(|w| w.id() == sid) {
                            writer.append(entry).map_err(|e| failed(format!("{e:#}")))?;
                            // An archived session takes no more tasks: this connection lets it go.
                            if !restore {
                                guard.take();
                            }
                            return Ok(p::Empty {});
                        }
                        drop(guard);
                        ours(&stored, &sid, &workspace)?;
                        let mut writer = stored.open(&sid).map_err(refused)?;
                        if writer.loaded().archived != !restore {
                            writer.append(entry).map_err(|e| failed(format!("{e:#}")))?;
                        }
                        Ok(p::Empty {})
                    })();
                    respond(&tx, id, value(result));
                });
            }
            method::SESSIONS_DELETE => {
                let p::SessionId { id: sid } = params(args)?;
                let (tx, stored, held_session) = (self.tx.clone(), self.stored.clone(), self.held.clone());
                let workspace = self.server.workspace.clone();
                tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        ours(&stored, &sid, &workspace)?;
                        let mut guard = held(&held_session);
                        if guard.as_ref().is_some_and(|w| w.id() == sid) {
                            guard.take();
                        }
                        drop(guard);
                        stored.delete(&sid).map_err(refused)?;
                        Ok(p::Empty {})
                    })();
                    respond(&tx, id, value(result));
                });
            }
            method::COMMAND_STOP => {
                let p::Run { trace } = params(args)?;
                if let Some(stop) = self.commands.remove(trace.as_str()) {
                    stop.cancel();
                }
                self.respond(id, Ok(p::Empty {}));
            }
            other => {
                return Err(Dispatch::Error(Error::new(
                    code::METHOD_NOT_FOUND,
                    format!("there is no method {other:?}; initialize lists them"),
                )))
            }
        }
        Ok(())
    }

    /// A request is always about the server's workspace: the one it was
    /// started for, which the client cannot change.
    fn own(&self, req: &mut RunRequest) -> Result<(), Dispatch> {
        let ours = &self.server.workspace;
        let same = req.workspace.is_empty()
            || req.workspace == *ours
            || Path::new(&req.workspace).canonicalize().ok() == Path::new(ours).canonicalize().ok();
        if !same {
            return Err(Dispatch::Error(Error::new(
                code::INVALID_PARAMS,
                format!("the request is for {}, and this server works in {ours}", req.workspace),
            )));
        }
        req.workspace = ours.clone();
        req.session = match req.session.take() {
            None => Some(self.held_id().unwrap_or_else(|| self.run_session.clone())),
            Some(name) => Some(
                crate::runs::session_name(&name).map_err(|e| Dispatch::Error(Error::new(code::INVALID_PARAMS, e)))?,
            ),
        };
        Ok(())
    }

    /// The forks of the results the stored session waits on the person for:
    /// the session goes on after this connection, and so do they.
    fn awaiting(&self) -> Vec<String> {
        held(&self.held).as_ref().map_or_else(Vec::new, |w| w.loaded().results.iter().map(|r| r.fork.clone()).collect())
    }

    /// The id of the stored session this connection holds.
    fn held_id(&self) -> Option<String> {
        held(&self.held).as_ref().map(|w| w.id().to_owned())
    }

    /// Record the task of `trace` in the stored session this connection
    /// holds, before the planner is asked: once the task is answered as
    /// started, the session has it. The earlier tasks of the session are
    /// what the request is told of, when it names none of its own.
    fn record_task(
        &self,
        trace: &molt_proto::TraceId,
        req: &mut RunRequest,
    ) -> Result<Option<(Held, String)>, Dispatch> {
        if self.session.is_none() {
            return Ok(None);
        }
        let mut guard = held(&self.held);
        let Some(writer) = guard.as_mut() else { return Ok(None) };
        if req.earlier.is_empty() && req.resume.is_none() {
            req.earlier = writer.loaded().earlier.clone();
        }
        writer.append(Entry::Task { run: trace.to_string(), task: req.task.clone() }).map_err(|e| {
            Dispatch::Error(failed(format!("the session could not record the task, so it was not started: {e:#}")))
        })?;
        Ok(Some((self.held.clone(), writer.id().to_owned())))
    }

    fn open(&mut self, id: Option<Id>) {
        if self.session.is_some() || self.starting.is_some() {
            self.respond(id, Err::<(), _>(Error::new(code::STARTING, "the session is already open or starting")));
            return;
        }
        if self.server.in_use.swap(true, Ordering::SeqCst) {
            let error = Error::new(code::BUSY, "another client has the session open; close it there first");
            self.respond(id, Err::<(), _>(error));
            return;
        }
        self.starting = Some(id.clone());
        let (cfg, workspace, tx, sessions) =
            (self.server.cfg.clone(), self.server.workspace.clone(), self.tx.clone(), self.sessions.clone());
        let held_session = self.held.clone();
        tokio::spawn(async move {
            let (events, mut progress) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Some(event) = progress.recv().await {
                    // A question of a task of the stored session waits for the person there too.
                    if let molt_api::progress::Progress::Asked { run, question, text, options, .. } = &event {
                        let sid = held(&held_session).as_ref().map(|w| w.id().to_owned());
                        if let Some(sid) = sid {
                            record(&held_session, &sid, |loaded| {
                                loaded.unanswered.iter().any(|u| u.run == *run).then(|| Entry::Asked {
                                    run: run.clone(),
                                    question: *question,
                                    text: text.clone(),
                                    options: options.clone(),
                                })
                            });
                        }
                    }
                    let Ok(params) = serde_json::to_value(&event) else { continue };
                    let note = Notification { jsonrpc: "2.0".into(), method: method::PROGRESS.into(), params };
                    tx.send(ServerMessage::Notification(note));
                }
            });
            let session = Session::start(&cfg, &workspace, events).await;
            let _ = sessions.send((id, session.map(Arc::new).map_err(|e| format!("{e:#}"))));
        });
    }

    fn started(&mut self, id: Option<Id>, session: Result<Arc<Session>, String>) {
        self.starting = None;
        match session {
            Ok(session) => {
                let opened = p::SessionOpened {
                    versions: session.versions().to_vec(),
                    memory_problem: session.memory_problem(),
                    default_model: session.default_model().map(str::to_owned),
                };
                self.session = Some(session);
                self.respond(id, Ok(opened));
            }
            Err(e) => {
                self.server.in_use.store(false, Ordering::SeqCst);
                self.respond(id, Err::<(), _>(failed(e)));
            }
        }
    }

    fn control(&mut self, id: Option<Id>, trace: molt_proto::TraceId, action: Control) {
        self.call(id, move |s, _| async move { s.control(&trace, action).await.map_err(|e| failed(format!("{e:#}"))) });
    }

    /// Make a call on the session in a task of its own; it answers `id`.
    fn call<F, Fut, T>(&mut self, id: Option<Id>, work: F)
    where
        F: FnOnce(Arc<Session>, Arc<Mutex<Forks>>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, Error>> + Send + 'static,
        T: Serialize,
    {
        let Some(session) = self.session.clone() else {
            self.respond(id, Err::<(), _>(no_session()));
            return;
        };
        let (tx, forks, answered) = (self.tx.clone(), self.forks.clone(), Arc::new(AtomicBool::new(false)));
        let (reply, once) = (id.clone(), answered.clone());
        let task = tokio::spawn(async move {
            let result = value(work(session, forks).await);
            if !once.swap(true, Ordering::SeqCst) {
                respond(&tx, reply, result);
            }
        });
        self.work.push(Work { id, answered, task });
    }

    /// Stop every call in flight; each that had not answered answers that it was stopped.
    fn abort_work(&mut self) {
        for work in self.work.drain(..) {
            if !work.answered.swap(true, Ordering::SeqCst) {
                work.task.abort();
                let error = Error::new(code::CANCELLED, "stopped before it answered; what it did is unknown");
                respond(&self.tx, work.id, Err(error));
            }
        }
    }

    /// Stop the services and the kernel, keeping `keep` and the forks `result.keep` named.
    async fn close(&mut self, keep: Vec<String>) {
        if let Some(session) = self.session.take() {
            let kept = std::mem::take(&mut *self.forks.lock().unwrap_or_else(PoisonError::into_inner)).kept;
            let keep: BTreeSet<String> = keep.into_iter().chain(kept).chain(self.awaiting()).collect();
            session.shutdown(&keep.into_iter().collect::<Vec<_>>()).await;
            self.server.in_use.store(false, Ordering::SeqCst);
        }
    }

    fn stop(&mut self, id: Option<Id>, stop: p::Stop) {
        self.abort_work();
        let p::Stop { keep, attempts, check_files, drop_run } = stop;
        let tx = self.tx.clone();
        let Some(session) = self.session.take() else {
            let result = match self.starting {
                Some(_) => Err(Error::new(code::STARTING, "the session is still starting; nothing was stopped")),
                None => Ok(p::Stopped::default()),
            };
            self.respond(id, result);
            return;
        };
        let kept_before: Vec<String> = {
            let mut forks = self.forks.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *forks).kept.into_iter().chain(self.awaiting()).collect()
        };
        let in_use = self.server.in_use.clone();
        tokio::spawn(async move {
            let mut listed = Vec::new();
            for fork in &attempts {
                listed.push(session.changes(fork).await);
            }
            if listed.iter().any(Result::is_err) {
                // A command the attempt started may still be writing in its fork until the
                // shell has stopped it: look once more after that.
                tokio::time::sleep(RELIST_AFTER).await;
                for (fork, result) in attempts.iter().zip(listed.iter_mut()) {
                    if result.is_err() {
                        *result = session.changes(fork).await;
                    }
                }
            }
            let (mut kept, mut notes) = (Vec::new(), Vec::new());
            for (fork, result) in attempts.into_iter().zip(listed) {
                match leftover(&fork, result, &check_files) {
                    Leftover::Work => kept.push(fork),
                    Leftover::Empty => {
                        // The planner left it to us; a scratch dir outside the data dir is not
                        // cleaned up at shutdown.
                        if let Err(e) = session.discard(&fork).await {
                            tracing::warn!("could not drop {fork}: {e:#}");
                        }
                    }
                    Leftover::Unlisted(e) => {
                        tracing::warn!("could not list the changes in {fork}: {e}");
                        notes.push(format!(
                            "the changes in {fork} could not be listed, so it is kept in case it holds work: {e}"
                        ));
                        kept.push(fork);
                    }
                    Leftover::Gone(e) => {
                        tracing::warn!("could not list the changes in {fork}: {e}");
                        notes.push(format!("{fork} is gone, with whatever it held: {e}"));
                    }
                }
            }
            let all: Vec<String> = keep.into_iter().chain(kept_before).chain(kept.iter().cloned()).collect();
            session.shutdown_dropping(&all, drop_run.as_deref()).await;
            in_use.store(false, Ordering::SeqCst);
            respond(&tx, id, value(Ok::<_, Error>(p::Stopped { kept, notes })));
        });
    }
}

/// Session `id` is one of `workspace`'s: its journal's first line says so.
fn ours(stored: &Sessions, id: &str, workspace: &str) -> Result<(), Error> {
    match stored.workspace(id) {
        Some(theirs) if theirs == workspace => Ok(()),
        Some(theirs) => Err(Error::new(
            code::INVALID_PARAMS,
            format!("session {id} is of {theirs}, and this server works in {workspace}"),
        )),
        None => Err(failed(format!(
            "there is no session {id} here, or its journal cannot be read: sessions.list says which there are"
        ))),
    }
}

enum Dispatch {
    Error(Error),
    Close(Vec<String>),
}

impl From<Error> for Dispatch {
    fn from(error: Error) -> Self {
        Self::Error(error)
    }
}

/// Answer request `id`; a notification (no `id`) gets no answer.
fn respond(tx: &Outbox, id: Option<Id>, result: Result<Value, Error>) {
    if id.is_some() {
        tx.send(ServerMessage::Response(Response::new(id, result)));
    }
}

/// What an interrupted attempt left in its fork.
#[derive(Debug, PartialEq)]
enum Leftover {
    /// Changes beyond the check's own files: the attempt's work, to keep.
    Work,
    /// Nothing but the check's files.
    Empty,
    /// Its changes could not be listed, and the fork is still there: kept,
    /// since it may hold work.
    Unlisted(String),
    /// Its changes could not be listed because the fork is gone.
    Gone(String),
}

fn leftover(fork: &str, listed: anyhow::Result<Vec<String>>, check_files: &[String]) -> Leftover {
    match listed {
        Ok(paths) if paths.iter().any(|p| !check_files.contains(p)) => Leftover::Work,
        Ok(_) => Leftover::Empty,
        Err(e) if Path::new(fork).exists() => Leftover::Unlisted(format!("{e:#}")),
        Err(e) => Leftover::Gone(format!("{e:#}")),
    }
}

/// The runs of `workspace` that stopped with a checkpoint in `dir`, newest
/// first, without their conversations; one whose checkpoint cannot be read
/// comes with why.
fn list_runs(dir: &Path, workspace: &str) -> Vec<p::StoppedRun> {
    molt_planner::checkpoint::list(dir)
        .into_iter()
        .filter_map(|(run, saved)| match saved {
            Ok(mut saved) if saved.request.workspace == workspace => {
                let gone = saved
                    .attempts
                    .iter()
                    .filter(|a| a.end.is_none())
                    .filter_map(|a| a.fork.clone())
                    .filter(|fork| !Path::new(fork).is_dir())
                    .collect();
                for attempt in &mut saved.attempts {
                    attempt.messages = Vec::new();
                }
                Some(p::StoppedRun { run, saved: Ok(saved), gone })
            }
            Ok(_) => None,
            Err(e) => Some(p::StoppedRun { run, saved: Err(e), gone: Vec::new() }),
        })
        .collect()
}

/// Serve `server` on standard input and output until the client closes its
/// side or `stop`. What the services print goes to standard error, never
/// into the protocol.
pub async fn stdio(server: Arc<Server>, stop: impl Future<Output = i32> + Send + 'static) -> anyhow::Result<()> {
    let output = tokio::fs::File::from_std(protocol_output()?);
    stdio_with(server, tokio::io::stdin(), output, stop).await
}

/// [`stdio`] on `input` once read from, for the interface's own server.
pub(crate) async fn stdio_with(
    server: Arc<Server>,
    input: impl AsyncRead + Unpin + Send + 'static,
    output: impl AsyncWrite + Unpin + Send + 'static,
    stop: impl Future<Output = i32> + Send + 'static,
) -> anyhow::Result<()> {
    let token = CancellationToken::new();
    let stopping = token.clone();
    tokio::spawn(async move {
        stop.await;
        stopping.cancel();
    });
    server.connection(input, output, token).await
}

/// Services inherit stdout as their log, never the private protocol descriptor.
pub(crate) fn protocol_output() -> anyhow::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: duplicate this process's pipe with close-on-exec; only the
    // returned File owns the new descriptor. dup2 changes our stdout to our log.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(fd >= 0, "could not keep the protocol output");
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } >= 0,
        "could not separate service output from the protocol"
    );
    Ok(file)
}

/// Listen on the Unix socket `path` until `stop`, serving each client that
/// connects as this user. The socket is made with mode 0600 in a directory
/// no one else may enter, and a peer of another user is turned away too.
pub async fn listen(
    server: Arc<Server>,
    path: &Path,
    stop: impl Future<Output = i32> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind(path)?;
    eprintln!("molt serve: listening on {}", path.display());
    let token = CancellationToken::new();
    tokio::pin!(stop);
    let mut clients = tokio::task::JoinSet::new();
    // SAFETY: plain syscall.
    let uid = unsafe { libc::geteuid() };
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        tracing::warn!("could not accept a client: {e}");
                        continue;
                    }
                };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == uid => {}
                    Ok(cred) => {
                        tracing::warn!("refused a client of user {} on {}", cred.uid(), path.display());
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("refused a client whose user is unknown: {e}");
                        continue;
                    }
                }
                let (server, token) = (server.clone(), token.child_token());
                clients.spawn(async move {
                    let (input, output) = stream.into_split();
                    if let Err(e) = server.connection(input, output, token).await {
                        tracing::warn!("a client's connection ended: {e:#}");
                    }
                });
            }
            _ = &mut stop => break,
        }
    }
    drop(listener);
    let _ = std::fs::remove_file(path);
    token.cancel();
    while clients.join_next().await.is_some() {}
    Ok(())
}

/// The socket at `path`, made private. A socket left by a server that is
/// gone is replaced; one a server still answers on is not.
fn bind(path: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    ensure!(
        path.as_os_str().len() <= crate::MAX_SOCKET_PATH,
        "{} is too long for a socket ({} bytes at most): pass a shorter --socket",
        path.display(),
        crate::MAX_SOCKET_PATH
    );
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    crate::lock::create_private_dir(dir)?;
    let mode = std::fs::metadata(dir).with_context(|| format!("{}", dir.display()))?.permissions().mode();
    // Another user who may write there could put a socket of their own in its place.
    ensure!(
        mode & 0o022 == 0,
        "other users may write in {} (mode {:o}); make it private with chmod 700, or pass a --socket in a private \
         directory",
        dir.display(),
        mode & 0o777
    );
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            ensure!(
                std::os::unix::net::UnixStream::connect(path).is_err(),
                "a molt server is already listening on {}",
                path.display()
            );
            std::fs::remove_file(path).with_context(|| format!("removing the stale {}", path.display()))?;
        }
        Ok(_) => anyhow::bail!("{} exists and is not a socket; move it or pass another --socket", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
    }
    let listener = tokio::net::UnixListener::bind(path).with_context(|| format!("listening on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("making {} private", path.display()))?;
    Ok(listener)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};

    /// A client of a server with no services on `workspace`, over an in-memory pipe.
    pub(crate) struct Raw {
        out: tokio::io::WriteHalf<DuplexStream>,
        input: BufReader<tokio::io::ReadHalf<DuplexStream>>,
        pub served: JoinHandle<anyhow::Result<()>>,
    }

    impl Raw {
        pub(crate) fn new(workspace: &str) -> Self {
            Self::with(Config::default(), workspace)
        }

        pub(crate) fn with(cfg: Config, workspace: &str) -> Self {
            let (ours, theirs) = tokio::io::duplex(1 << 20);
            let (theirs_in, theirs_out) = tokio::io::split(theirs);
            let server = Server::new(cfg, workspace.to_owned());
            let served =
                tokio::spawn(async move { server.connection(theirs_in, theirs_out, CancellationToken::new()).await });
            let (input, out) = tokio::io::split(ours);
            Self { out, input: BufReader::new(input), served }
        }

        pub(crate) async fn send(&mut self, line: &str) {
            self.out.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        }

        pub(crate) async fn recv(&mut self) -> Value {
            let mut line = Vec::new();
            assert!(p::read_line(&mut self.input, &mut line).await.unwrap(), "the server hung up");
            serde_json::from_slice(&line).unwrap()
        }

        pub(crate) async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()).await;
            let reply = self.recv().await;
            assert_eq!(reply["id"], id, "{reply}");
            reply
        }
    }

    #[tokio::test]
    async fn a_client_must_say_its_version_first_and_bad_messages_are_answered_not_fatal() {
        let mut raw = Raw::new("/w");
        let refused = raw.call(1, "runs.list", json!({})).await;
        assert_eq!(refused["error"]["code"], code::NOT_INITIALIZED, "{refused}");
        let old = raw.call(2, "initialize", json!({"protocol": 0})).await;
        assert_eq!(old["error"]["code"], code::INVALID_PARAMS, "{old}");
        assert!(old["error"]["message"].as_str().unwrap().contains("version 1"), "{old}");
        let hello = raw.call(3, "initialize", json!({"protocol": 1, "client": {"name": "test"}})).await;
        assert_eq!(hello["result"]["protocol"], 1);
        assert_eq!(hello["result"]["workspace"], "/w");
        assert_eq!(hello["result"]["methods"].as_array().unwrap().len(), method::ALL.len() + method::ADDED.len());

        raw.send("not json").await;
        let parse = raw.recv().await;
        assert_eq!((parse["id"].clone(), parse["error"]["code"].clone()), (Value::Null, json!(code::PARSE)));
        raw.send(r#"{"jsonrpc":"1.0","id":"a","method":"runs.list"}"#).await;
        let wrong = raw.recv().await;
        assert_eq!((wrong["id"].clone(), wrong["error"]["code"].clone()), (json!("a"), json!(code::INVALID_REQUEST)));
        let unknown = raw.call(4, "kernel.grant", json!({})).await;
        assert_eq!(unknown["error"]["code"], code::METHOD_NOT_FOUND, "{unknown}");
        let bad = raw.call(5, "result.diff", json!({"path": "x"})).await;
        assert_eq!(bad["error"]["code"], code::INVALID_PARAMS, "{bad}");
        // A notification is carried out and answered with nothing: the next answer is the next request's.
        raw.send(r#"{"jsonrpc":"2.0","method":"command.stop","params":{"trace":"trace_x"}}"#).await;
        let none = raw.call(6, "result.diff", json!({"fork": "/f"})).await;
        assert_eq!(none["error"]["code"], code::NO_SESSION, "{none}");
        assert_eq!(none["error"]["message"], "the services are not up");
        let runs = raw.call(7, "runs.list", Value::Null).await;
        assert_eq!(runs["result"], json!([]), "a config without a planner keeps no checkpoints");
        let stopped = raw.call(8, "task.stop", json!({})).await;
        assert_eq!(stopped["result"], json!({"kept": [], "notes": []}), "nothing to stop");
        let closed = raw.call(9, "session.close", json!({"keep": []})).await;
        assert_eq!(closed["result"], json!({}));
    }

    #[tokio::test]
    async fn the_server_takes_every_request_of_version_1_as_written() {
        let data = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.kernel.data_dir = data.path().to_path_buf();
        let mut raw = Raw::with(cfg, "/home/me/project");
        raw.call(0, "initialize", json!({"protocol": 1})).await;
        let v1 =
            concat!(include_str!("testdata/protocol-v1.jsonl"), include_str!("testdata/protocol-v1-sessions.jsonl"));
        for (n, line) in v1.lines().enumerate() {
            let entry: Value = serde_json::from_str(line).unwrap();
            let Some(name) = entry["method"].as_str().filter(|m| *m != "initialize") else { continue };
            let reply = raw.call(n as u64 + 1, name, entry["params"].clone()).await;
            // Read and dispatched: with no services, at most it finds no session.
            let code = reply["error"]["code"].as_i64();
            assert!(
                !matches!(code, Some(code::METHOD_NOT_FOUND | code::INVALID_PARAMS | code::INVALID_REQUEST)),
                "{name}: {reply}"
            );
        }
    }

    #[tokio::test]
    async fn a_stored_session_is_held_by_one_client_outlives_its_connection_and_only_its_word_ends_it() {
        let data = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let ws = ws.to_str().unwrap();
        let mut cfg = Config::default();
        cfg.kernel.data_dir = data.path().to_path_buf();
        // What the interface saved of its last session here.
        let earlier = molt_api::planner::Earlier {
            task: "fix parser".into(),
            outcome: molt_api::planner::Outcome::Passed,
            summary: "Parsed.".into(),
            changes: vec![],
            applied: true,
        };
        let saved = crate::tui::saved::Saved { earlier: vec![earlier], spent_usd: 0.5, ..Default::default() };
        let store = crate::tui::state::Store::new(data.path(), Path::new(ws)).unwrap();
        crate::tui::saved::save(&store, &saved).unwrap();

        let mut a = Raw::with(cfg.clone(), ws);
        let hello = a.call(1, "initialize", json!({"protocol": 1})).await;
        assert!(hello["result"]["methods"].as_array().unwrap().contains(&json!("sessions.load")), "{hello}");
        let made = a.call(2, "sessions.new", json!({"title": "parser work", "import": true})).await;
        let session = &made["result"]["session"];
        assert_eq!((&made["result"]["held"], &session["title"]), (&json!(true), &json!("parser work")), "{made}");
        assert_eq!(
            (&session["earlier"][0]["task"], &session["spent"]["known_usd"]),
            (&json!("fix parser"), &json!(0.5))
        );
        let id = session["id"].as_str().unwrap().to_owned();
        assert!(crate::tui::saved::load(&store).unwrap().is_some(), "the interface's own file is left");

        let mut b = Raw::with(cfg.clone(), ws);
        b.call(1, "initialize", json!({"protocol": 1})).await;
        let busy = b.call(2, "sessions.load", json!({"id": id})).await;
        assert_eq!(busy["error"]["code"], code::BUSY, "{busy}");
        assert_eq!(b.call(3, "sessions.delete", json!({"id": id})).await["error"]["code"], code::BUSY);
        let listed = b.call(4, "sessions.list", json!({})).await;
        assert_eq!((&listed["result"][0]["id"], &listed["result"][0]["title"]), (&json!(id), &json!("parser work")));
        let mut other = Raw::with(cfg.clone(), "/elsewhere");
        other.call(1, "initialize", json!({"protocol": 1})).await;
        let theirs = other.call(2, "sessions.load", json!({"id": id})).await;
        assert_eq!(theirs["error"]["code"], code::INVALID_PARAMS, "{theirs}");
        assert_eq!(other.call(3, "sessions.list", json!({})).await["result"], json!([]));

        // The first client goes without a word: the session stays, and the next one holds it.
        let Raw { out, input, served } = a;
        drop((out, input));
        served.await.unwrap().unwrap();
        let loaded = b.call(5, "sessions.load", json!({"id": id})).await;
        assert_eq!(loaded["result"]["held"], true, "{loaded}");
        assert_eq!(loaded["result"]["session"]["earlier"][0]["task"], "fix parser");
        assert!(loaded["result"]["session"]["unanswered"].as_array().unwrap().is_empty(), "loading starts nothing");

        assert_eq!(b.call(6, "sessions.archive", json!({"id": id})).await["result"], json!({}));
        assert_eq!(b.call(7, "sessions.list", json!({})).await["result"], json!([]));
        assert_eq!(b.call(8, "sessions.list", json!({"archived": true})).await["result"][0]["archived"], true);
        let shown = b.call(9, "sessions.load", json!({"id": id})).await;
        assert_eq!((&shown["result"]["held"], &shown["result"]["session"]["archived"]), (&json!(false), &json!(true)));
        b.call(10, "sessions.archive", json!({"id": id, "restore": true})).await;
        assert_eq!(b.call(11, "sessions.load", json!({"id": id})).await["result"]["held"], true);
        assert_eq!(b.call(12, "sessions.delete", json!({"id": id})).await["result"], json!({}));
        assert_eq!(b.call(13, "sessions.list", json!({"archived": true})).await["result"], json!([]));
        assert_eq!(b.call(14, "sessions.load", json!({"id": id})).await["error"]["code"], code::FAILED);
    }

    #[tokio::test]
    async fn a_request_names_the_servers_workspace_or_none_and_never_another() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let mut raw = Raw::new(ws.to_str().unwrap());
        raw.call(1, "initialize", json!({"protocol": 1})).await;
        let other = tempfile::tempdir().unwrap();
        let req = RunRequest::new("t", other.path().to_str().unwrap());
        let refused = raw.call(2, "task.submit", json!({"trace": "trace_1", "request": req})).await;
        assert_eq!(refused["error"]["code"], code::INVALID_PARAMS, "{refused}");
        assert!(refused["error"]["message"].as_str().unwrap().contains("this server works in"), "{refused}");
        // Its own, or none, gets as far as the session, which is not open.
        for workspace in ["", ws.to_str().unwrap(), &format!("{}/.", ws.display())] {
            let req = RunRequest::new("t", workspace);
            let reply = raw.call(3, "task.submit", json!({"trace": "trace_1", "request": req})).await;
            assert_eq!(reply["error"]["code"], code::NO_SESSION, "{workspace}: {reply}");
        }
        // A session it names must be a name.
        let req = RunRequest { session: Some("a\u{1b}b".into()), ..RunRequest::new("t", "") };
        let refused = raw.call(4, "task.submit", json!({"trace": "trace_1", "request": req})).await;
        assert_eq!(refused["error"]["code"], code::INVALID_PARAMS, "{refused}");
    }

    #[tokio::test]
    async fn a_second_client_cannot_open_the_session_another_holds() {
        let server = Server::new(Config::default(), "/w".into());
        server.in_use.store(true, Ordering::SeqCst);
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (theirs_in, theirs_out) = tokio::io::split(theirs);
        tokio::spawn({
            let server = server.clone();
            async move { server.connection(theirs_in, theirs_out, CancellationToken::new()).await }
        });
        let (input, mut out) = tokio::io::split(ours);
        let mut input = BufReader::new(input);
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol":1}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"session.open"}"#,
        ] {
            out.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        }
        let mut line = Vec::new();
        p::read_line(&mut input, &mut line).await.unwrap();
        p::read_line(&mut input, &mut line).await.unwrap();
        let busy: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(busy["error"]["code"], code::BUSY, "{busy}");
        assert!(server.in_use.load(Ordering::SeqCst), "the other client's session is left alone");
    }

    #[tokio::test]
    async fn a_message_past_the_limit_ends_the_connection() {
        let mut raw = Raw::new("/w");
        let long = vec![b'x'; p::MAX_LINE + 1];
        raw.out.write_all(&long).await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(10), &mut raw.served).await.unwrap().unwrap();
        assert!(format!("{:#}", ended.unwrap_err()).contains("longer than 16 MiB"));
    }

    #[test]
    fn the_socket_is_private_a_live_one_is_not_taken_over_and_a_stale_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("serve.sock");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = bind(&path).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            let err = bind(&path).unwrap_err().to_string();
            assert!(err.contains("already listening"), "{err}");
            drop(listener);
            bind(&path).expect("a socket no server answers on is replaced");
        });
        let file = dir.path().join("plain");
        std::fs::write(&file, "").unwrap();
        assert!(bind(&file).unwrap_err().to_string().contains("is not a socket"));
        let open = tempfile::tempdir().unwrap();
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = bind(&open.path().join("serve.sock")).unwrap_err().to_string();
        assert!(err.contains("other users may write in"), "{err}");
        let long = dir.path().join("x".repeat(120));
        assert!(bind(&long).unwrap_err().to_string().contains("too long"));
    }

    /// A result that changed `path`, as a run answers.
    pub(crate) fn run_response(path: &str) -> molt_api::planner::RunResponse {
        use molt_api::fs::{Change, ChangeKind};
        use molt_api::planner::{Outcome, RunResponse};
        RunResponse {
            outcome: Outcome::Unverified,
            check: None,
            winner: Some(0),
            summary: "Write b".into(),
            changes: vec![Change { path: path.into(), kind: ChangeKind::Added }],
            left_out: None,
            patch: String::new(),
            patch_truncated: false,
            applied: true,
            fork: None,
            review: None,
            attempts: vec![],
            usage: Default::default(),
            cost_usd: 0.0,
            uncounted_calls: 0,
            checkpoint: None,
            requirements: vec![],
            selection: Default::default(),
            rubric: vec![],
            judgement: vec![],
            dispute: None,
            verification: None,
            workflow: None,
            conversation: Vec::new(),
            prompts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_server_commits_what_the_task_merged_against_the_tree_it_started_from() {
        use molt_proto::TraceId;
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
            git(d, &["config", key, value]);
        }
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "start"]);
        let mut raw = Raw::new(d.to_str().unwrap());
        raw.call(1, "initialize", json!({"protocol": 1})).await;
        let trace = TraceId::random();
        let prepared = raw.call(2, "commit.prepare", json!({"trace": trace})).await;
        assert_eq!(prepared["result"], json!({}));
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        let made = json!({"trace": trace, "task": "write b", "response": run_response("b.txt")});
        let made = raw.call(3, "commit.make", made).await;
        assert_eq!(made["result"]["subject"], "Write b", "{made}");
        assert_eq!(made["result"]["branch"], "main", "{made}");
        assert_eq!(git(d, &["show", "--name-only", "--format=", "HEAD"]), "b.txt\n");
        let trailer = git(d, &["log", "-1", "--format=%(trailers:key=Molt-Run,valueonly)"]);
        assert_eq!(trailer.trim(), trace.as_str());
        // Turned on after the task started: nothing was looked at for it.
        let late = json!({"trace": TraceId::random(), "task": "late", "response": run_response("b.txt")});
        let late = raw.call(4, "commit.make", late).await;
        assert_eq!(late["error"]["code"], code::FAILED, "{late}");
        assert!(late["error"]["message"].as_str().unwrap().contains("turned on after the task started"), "{late}");
    }

    /// Run `f` on a thread of its own as user and group `id`. The raw calls
    /// change only that thread, not the test process.
    fn as_user<T: Send + 'static>(id: libc::uid_t, f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(move || {
            // SAFETY: plain syscalls on this thread's own credentials.
            unsafe {
                assert_eq!(libc::syscall(libc::SYS_setresgid, id, id, id), 0);
                assert_eq!(libc::syscall(libc::SYS_setresuid, id, id, id), 0);
            }
            f()
        })
        .join()
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_servers_own_user_is_served_on_its_socket() {
        use std::io::{Read, Write};
        // SAFETY: plain syscall.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("notice: acting as another user needs root; the socket's own user is checked elsewhere");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("serve.sock");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let listening = tokio::spawn({
            let path = path.clone();
            async move {
                listen(Server::new(Config::default(), "/w".into()), &path, async { stopped.await.map_or(0, |()| 0) })
                    .await
            }
        });
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let hello = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocol\":1}}\n";
        let at = path.clone();
        let blocked = as_user(65534, move || std::os::unix::net::UnixStream::connect(&at).map(|_| ()));
        assert_eq!(
            blocked.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "the directory and socket keep others out"
        );
        // With the way opened to them, the server still turns them away by who they are.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let at = path.clone();
        let answered = as_user(65534, move || {
            let mut stream = std::os::unix::net::UnixStream::connect(&at).unwrap();
            // Served, it would get its answer and wait for more: a failure, not a hang.
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let _ = stream.write_all(hello.as_bytes());
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            answer
        });
        assert!(answered.is_empty(), "another user got an answer: {}", String::from_utf8_lossy(&answered));
        let at = path.clone();
        let ours = tokio::task::spawn_blocking(move || {
            let mut stream = std::os::unix::net::UnixStream::connect(&at).unwrap();
            stream.write_all(hello.as_bytes()).unwrap();
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line).unwrap();
            line
        })
        .await
        .unwrap();
        assert!(ours.contains("\"protocol\":1"), "{ours}");
        stop.send(()).unwrap();
        listening.await.unwrap().unwrap();
        assert!(!path.exists(), "the socket goes with the server");
    }

    #[test]
    fn an_interrupted_fork_whose_changes_cannot_be_listed_is_kept_while_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let fork = dir.path().to_str().unwrap();
        let check = vec!["tests/check.rs".to_owned()];
        let unreadable = || Err(anyhow::anyhow!("fs.diff failed: reading the fork: No such file or directory"));
        assert_eq!(
            leftover(fork, unreadable(), &check),
            Leftover::Unlisted("fs.diff failed: reading the fork: No such file or directory".into())
        );
        assert!(matches!(leftover("/nowhere/fork-gone", unreadable(), &check), Leftover::Gone(_)));
        assert_eq!(leftover(fork, Ok(vec!["src/lib.rs".into()]), &check), Leftover::Work);
        assert_eq!(leftover(fork, Ok(check.clone()), &check), Leftover::Empty, "the check's own files are no work");
    }

    #[test]
    fn stopped_runs_are_this_workspaces_say_which_forks_are_gone_and_name_damage() {
        use molt_planner::checkpoint::Saved;
        let dir = tempfile::tempdir().unwrap();
        let fork = dir.path().join("fork-kept");
        std::fs::create_dir(&fork).unwrap();
        let mut here = Saved::new("trace_here", RunRequest::new("t", "/w"));
        here.attempt(0).fork = Some(fork.to_str().unwrap().into());
        here.attempt(0).messages = vec![json!({"role": "user", "content": "long"})];
        here.attempt(1).fork = Some("/nowhere/fork-gone".into());
        let other = Saved::new("trace_other", RunRequest::new("t", "/elsewhere"));
        for saved in [&here, &other] {
            std::fs::write(dir.path().join(format!("{}.json", saved.run)), serde_json::to_vec(saved).unwrap()).unwrap();
        }
        std::fs::write(dir.path().join("trace_bad.json"), "{\"format\":1,").unwrap();
        let mut runs = list_runs(dir.path(), "/w");
        runs.sort_by(|a, b| a.run.cmp(&b.run));
        assert_eq!(runs.iter().map(|r| r.run.as_str()).collect::<Vec<_>>(), ["trace_bad", "trace_here"]);
        assert!(runs[0].saved.as_ref().unwrap_err().contains("damaged"));
        assert_eq!(runs[1].gone, ["/nowhere/fork-gone"]);
        assert!(runs[1].saved.as_ref().unwrap().attempts[0].messages.is_empty(), "conversations stay on disk");
    }
}
