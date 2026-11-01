//! `molt acp`: Molt as an agent of the Agent Client Protocol, for editors
//! that speak it, such as Zed and JetBrains IDEs (docs/acp.md).
//!
//! The editor starts `molt acp` and speaks ACP (JSON-RPC 2.0, one message a
//! line) on its standard input and output. For each folder the editor opens
//! a session in, the adapter runs a server of the client protocol
//! ([`crate::serve`], RFC 0004) for that folder alone and talks to it only
//! through the protocol: it can do what any client of `molt serve` can, and
//! nothing more. A result is kept in its fork, and applied only when the
//! person picks "Apply" in the editor's permission prompt for it.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use molt_api::fs::ChangeKind;
use molt_api::model::Role;
use molt_api::planner::tools::{TodoItem, TodoStatus};
use molt_api::planner::{AttemptStatus, Earlier, Outcome, RunRequest, RunResponse};
use molt_api::progress::Progress;
use molt_api::workflow;
use molt_proto::TraceId;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::config::TuiSection;
use crate::protocol::{self as p, method, Id, ServerMessage};
use crate::serve;

/// The ACP version this adapter speaks.
pub const PROTOCOL_VERSION: u16 = 1;

/// Earlier tasks of a session given to the next one, as the interface does.
const EARLIER_TASKS: usize = 5;
const EARLIER_SUMMARY_BYTES: usize = 4000;
/// The most files of a result shown as diffs in the apply prompt; the
/// rest are named in its text.
const DIFF_FILES: usize = 50;
/// A file larger than this is named in the apply prompt, not shown.
const DIFF_FILE_BYTES: u64 = 512 * 1024;
/// How long the end of the input waits for the tasks in flight to stop.
const SHUTDOWN: Duration = Duration::from_secs(30);

/// ACP's method names.
mod acp_method {
    pub const INITIALIZE: &str = "initialize";
    pub const AUTHENTICATE: &str = "authenticate";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_CANCEL: &str = "session/cancel";
    pub const SESSION_UPDATE: &str = "session/update";
    pub const REQUEST_PERMISSION: &str = "session/request_permission";
}

/// JSON-RPC's error codes and ACP's own. The client protocol's codes are
/// never passed through: ACP gives -32000 another meaning.
mod code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const RESOURCE_NOT_FOUND: i64 = -32002;
}

/// The option ids of the apply prompt.
const APPLY: &str = "apply";
const KEEP: &str = "keep";
const DISCARD: &str = "discard";
/// The option of a question that lets the attempt decide.
const DECIDE: &str = "decide";
const DECIDE_ANSWER: &str = "No answer from the person: decide yourself, and say in your summary what you assumed.";

/// The flags `molt acp` passes on to each folder's server, as `molt serve`
/// takes them. No data dir: each folder has its own.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub config: Option<PathBuf>,
    pub profile: Option<String>,
    pub pass_env: Vec<String>,
    pub deps_cache: Vec<String>,
    pub allow_web: Vec<String>,
    pub web_resolver: Option<String>,
}

impl Options {
    fn serve(&self, workspace: PathBuf) -> serve::Options {
        serve::Options {
            config: self.config.clone(),
            workspace,
            project: None,
            data_dir: None,
            pass_env: self.pass_env.clone(),
            deps_cache: self.deps_cache.clone(),
            allow_web: self.allow_web.clone(),
            web_resolver: self.web_resolver.clone(),
            profile: self.profile.clone(),
            // Each ACP session names its own runs' session.
            session: None,
        }
    }
}

/// Speak ACP on standard input and output until the editor closes its side
/// or `stop`. What the services print goes to standard error.
pub async fn stdio(opts: Options, stop: impl Future<Output = i32> + Send + 'static) -> anyhow::Result<()> {
    let output = tokio::fs::File::from_std(serve::protocol_output()?);
    run(opts, tokio::io::stdin(), output, stop).await
}

/// Speak ACP on `input` and `output`. When the input ends or `stop`, a task
/// in flight is stopped as `session/cancel` stops it, keeping its work, and
/// every folder's services stop, keeping the results not applied.
pub async fn run(
    opts: Options,
    input: impl AsyncRead + Unpin + Send + 'static,
    output: impl AsyncWrite + Unpin + Send + 'static,
    stop: impl Future<Output = i32> + Send + 'static,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(msg) = rx.recv().await {
            p::write_line(&mut output, &msg).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let agent = Arc::new(Agent {
        opts,
        editor: Editor { tx, next: Arc::default(), pending: Arc::default() },
        sessions: Mutex::default(),
        servers: tokio::sync::Mutex::default(),
        stop: CancellationToken::new(),
        initialized: AtomicBool::new(false),
    });
    // A reader of its own owns each whole line, as the server's does.
    let (lines, mut incoming) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        let mut input = BufReader::new(input);
        loop {
            let mut line = Vec::new();
            let read = p::read_line(&mut input, &mut line).await.map(|more| more.then_some(line));
            let ended = !matches!(read, Ok(Some(_)));
            if lines.send(read).is_err() || ended {
                break;
            }
        }
    });
    let mut work = JoinSet::new();
    tokio::pin!(stop);
    let ended = loop {
        tokio::select! {
            line = incoming.recv() => match line.unwrap_or(Ok(None)) {
                Ok(Some(line)) => agent.handle(&line, &mut work),
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            },
            Some(_) = work.join_next(), if !work.is_empty() => {}
            _ = &mut stop => break Ok(()),
        }
    };
    reader.abort();
    agent.shutdown(&mut work).await;
    drop(agent);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    ended.context("reading from the editor")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(code::INTERNAL, message)
    }
}

/// What a request's parameters read as, or `INVALID_PARAMS`.
fn params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { Value::Object(Default::default()) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError::params(format!("invalid params: {e}")))
}

/// The editor's side: messages to it, and its answers to the requests the
/// adapter makes of it.
#[derive(Clone)]
struct Editor {
    tx: mpsc::UnboundedSender<Value>,
    next: Arc<AtomicU64>,
    pending: Waiting,
}

/// The adapter's requests the editor has not answered yet, by id.
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

impl Editor {
    fn respond(&self, id: Option<Value>, result: Result<Value, RpcError>) {
        // A notification gets no answer, not even an error.
        let Some(id) = id else { return };
        let msg = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        let _ = self.tx.send(msg);
    }

    fn update(&self, session: &str, update: Update) {
        let params = json!({"sessionId": session, "update": update});
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "method": acp_method::SESSION_UPDATE, "params": params}));
    }

    async fn request(&self, method: &str, params: impl Serialize) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.tx.send(msg).is_err() {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(RpcError::internal("the editor is gone"));
        }
        rx.await.unwrap_or_else(|_| Err(RpcError::internal("the editor is gone")))
    }

    /// The editor's answer to one of our requests.
    fn answered(&self, msg: Value) {
        let Some(id) = msg.get("id").and_then(Value::as_u64) else { return };
        let Some(tx) = self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) else { return };
        let answer = match (msg.get("result"), msg.get("error")) {
            (_, Some(error)) => Err(serde_json::from_value(error.clone())
                .unwrap_or_else(|_| RpcError::internal("the editor answered with an error"))),
            (Some(result), None) => Ok(result.clone()),
            (None, None) => Err(RpcError::internal("the editor's answer has neither a result nor an error")),
        };
        let _ = tx.send(answer);
    }
}

struct Agent {
    opts: Options,
    editor: Editor,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// A server for each folder, by its canonical path; sessions in one
    /// folder share it.
    servers: tokio::sync::Mutex<HashMap<PathBuf, Arc<Molt>>>,
    /// Ends every server's connection.
    stop: CancellationToken,
    initialized: AtomicBool,
}

/// One ACP session: a conversation in a folder.
struct Session {
    id: String,
    molt: Arc<Molt>,
    state: Mutex<Turns>,
}

#[derive(Default)]
struct Turns {
    /// What stops the turn running, if one is.
    running: Option<CancellationToken>,
    earlier: Vec<Earlier>,
}

impl Agent {
    fn handle(self: &Arc<Self>, line: &[u8], work: &mut JoinSet<()>) {
        let msg: Value = match serde_json::from_slice(line) {
            Ok(msg) => msg,
            Err(e) => {
                return self
                    .editor
                    .respond(Some(Value::Null), Err(RpcError::new(code::PARSE, format!("not JSON: {e}"))))
            }
        };
        let id = msg.get("id").filter(|id| !id.is_null()).cloned();
        let Some(name) = msg.get("method").and_then(Value::as_str).map(str::to_owned) else {
            if id.is_some() && (msg.get("result").is_some() || msg.get("error").is_some()) {
                self.editor.answered(msg);
            } else {
                let error = RpcError::new(code::INVALID_REQUEST, "not a JSON-RPC 2.0 request or response");
                self.editor.respond(Some(id.unwrap_or(Value::Null)), Err(error));
            }
            return;
        };
        let args = msg.get("params").cloned().unwrap_or(Value::Null);
        if name != acp_method::INITIALIZE && !self.initialized.load(Ordering::SeqCst) {
            return self.editor.respond(id, Err(RpcError::new(code::INVALID_REQUEST, "call initialize first")));
        }
        match name.as_str() {
            acp_method::INITIALIZE => {
                let answer = self.initialize(args);
                self.editor.respond(id, answer);
            }
            // No method is offered: the model's key is the user's own, in their environment.
            acp_method::AUTHENTICATE => self.editor.respond(id, Ok(json!({}))),
            acp_method::SESSION_NEW => {
                let agent = self.clone();
                work.spawn(async move {
                    let answer = agent.new_session(args).await;
                    agent.editor.respond(id, answer);
                });
            }
            acp_method::SESSION_PROMPT => {
                let agent = self.clone();
                work.spawn(async move {
                    let answer = agent.prompt(args).await;
                    agent.editor.respond(id, answer);
                });
            }
            acp_method::SESSION_CANCEL => {
                let answer = self.cancel(args);
                self.editor.respond(id, answer.map(|()| Value::Null));
            }
            other => {
                let error = RpcError::new(code::METHOD_NOT_FOUND, format!("molt acp has no method {other:?}"));
                self.editor.respond(id, Err(error));
            }
        }
    }

    fn initialize(&self, args: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Hello {
            protocol_version: u16,
        }
        // Whatever the editor speaks, the answer names the version this adapter
        // speaks; an editor that does not speak it closes the connection.
        let Hello { protocol_version } = params(args)?;
        if protocol_version != PROTOCOL_VERSION {
            tracing::info!("the editor speaks ACP version {protocol_version}; molt speaks {PROTOCOL_VERSION}");
        }
        self.initialized.store(true, Ordering::SeqCst);
        Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": false, "sse": false},
            },
            "authMethods": [],
            "agentInfo": {"name": "molt", "title": "Molt", "version": env!("CARGO_PKG_VERSION")},
        }))
    }

    async fn new_session(&self, args: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct New {
            cwd: String,
            /// The editor's MCP servers: not used, since a tool the agent runs
            /// comes only from the user's own config.
            #[serde(default)]
            mcp_servers: Vec<Value>,
        }
        let New { cwd, mcp_servers } = params(args)?;
        let cwd = PathBuf::from(cwd);
        if !cwd.is_absolute() {
            return Err(RpcError::params(format!("cwd must be an absolute path, not {}", cwd.display())));
        }
        if !mcp_servers.is_empty() {
            tracing::warn!(
                "the editor offered {} MCP servers; molt uses only those of its own config",
                mcp_servers.len()
            );
        }
        let molt = self.server(&cwd).await?;
        molt.open().await?;
        let id = format!("sess_{:032x}", rand::random::<u128>());
        let session = Arc::new(Session { id: id.clone(), molt, state: Mutex::default() });
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner).insert(id.clone(), session);
        Ok(json!({"sessionId": id}))
    }

    /// The server of the folder `cwd`, started the first time.
    async fn server(&self, cwd: &Path) -> Result<Arc<Molt>, RpcError> {
        let folder = cwd.canonicalize().map_err(|e| RpcError::params(format!("cwd {}: {e}", cwd.display())))?;
        if !folder.is_dir() {
            return Err(RpcError::params(format!("cwd {} is not a directory", cwd.display())));
        }
        let mut servers = self.servers.lock().await;
        if let Some(molt) = servers.get(&folder) {
            return Ok(molt.clone());
        }
        let molt = Molt::start(self.opts.serve(folder.clone()), self.stop.child_token())
            .await
            .map_err(|e| RpcError::internal(format!("{e:#}")))?;
        servers.insert(folder, molt.clone());
        Ok(molt)
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, RpcError> {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions
            .get(id)
            .cloned()
            .ok_or_else(|| RpcError::new(code::RESOURCE_NOT_FOUND, format!("there is no session {id:?}")))
    }

    fn cancel(&self, args: Value) -> Result<(), RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Cancel {
            session_id: String,
        }
        let Cancel { session_id } = params(args)?;
        let session = self.session(&session_id)?;
        if let Some(stop) = &session.state.lock().unwrap_or_else(PoisonError::into_inner).running {
            stop.cancel();
        }
        Ok(())
    }

    async fn prompt(&self, args: Value) -> Result<Value, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Prompt {
            session_id: String,
            prompt: Vec<Value>,
        }
        let Prompt { session_id, prompt } = params(args)?;
        let session = self.session(&session_id)?;
        let (task, files) = task_of(&prompt, Path::new(&session.molt.workspace))?;
        if task.trim().is_empty() {
            return Err(RpcError::params("the prompt has no text: say what to do"));
        }
        // One task at a time in a folder, as in the interface.
        let Ok(_turn) = session.molt.busy.try_lock() else {
            return Err(RpcError::new(
                code::INVALID_REQUEST,
                "a task is running in this folder, in this or another thread: wait for it or stop it",
            ));
        };
        let stop = CancellationToken::new();
        let earlier = {
            let mut state = session.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.running = Some(stop.clone());
            state.earlier.clone()
        };
        let turn = Turn { editor: &self.editor, session: &session, stop: &stop };
        let ended = turn.run(task, files, earlier).await;
        session.state.lock().unwrap_or_else(PoisonError::into_inner).running = None;
        ended.map(|reason| json!({"stopReason": reason}))
    }

    /// Stop what runs, keeping its work, then every folder's services.
    async fn shutdown(&self, work: &mut JoinSet<()>) {
        let sessions: Vec<Arc<Session>> =
            self.sessions.lock().unwrap_or_else(PoisonError::into_inner).values().cloned().collect();
        for session in &sessions {
            if let Some(stop) = &session.state.lock().unwrap_or_else(PoisonError::into_inner).running {
                stop.cancel();
            }
        }
        if tokio::time::timeout(SHUTDOWN, async { while work.join_next().await.is_some() {} }).await.is_err() {
            work.abort_all();
        }
        self.stop.cancel();
        let servers: Vec<Arc<Molt>> = self.servers.lock().await.values().cloned().collect();
        for molt in servers {
            let served = molt.served.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(served) = served {
                match tokio::time::timeout(SHUTDOWN, served).await {
                    Ok(Ok(Err(e))) => tracing::warn!("the server of {} ended: {e:#}", molt.workspace),
                    Ok(_) => {}
                    Err(_) => tracing::warn!("the services of {} did not stop in time", molt.workspace),
                }
            }
        }
    }
}

/// A folder's server of the client protocol, and the adapter's connection to it.
struct Molt {
    workspace: String,
    /// The task defaults of `[tui]` (and of `--profile`), which a task here runs with.
    defaults: TuiSection,
    rpc: Rpc,
    /// Held by the task running in the folder.
    busy: tokio::sync::Mutex<()>,
    /// The protocol session, once open; `None` again after a stop closed it.
    opened: tokio::sync::Mutex<Option<p::SessionOpened>>,
    served: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

impl Molt {
    async fn start(opts: serve::Options, stop: CancellationToken) -> anyhow::Result<Arc<Self>> {
        let (cfg, workspace, _) = tokio::task::spawn_blocking(move || serve::configure(opts)).await??;
        let defaults = cfg.tui.clone();
        let server = serve::Server::new(cfg, workspace.clone());
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (theirs_in, theirs_out) = tokio::io::split(theirs);
        let served = tokio::spawn(async move { server.connection(theirs_in, theirs_out, stop).await });
        let (input, output) = tokio::io::split(ours);
        let rpc = Rpc::over(input, output);
        let hello = p::Initialize {
            protocol: p::VERSION,
            client: p::Client { name: "molt acp".into(), version: env!("CARGO_PKG_VERSION").into() },
        };
        rpc.call(method::INITIALIZE, hello).await.map_err(|e| anyhow::anyhow!("the server refused: {}", e.message))?;
        Ok(Arc::new(Self {
            workspace,
            defaults,
            rpc,
            busy: tokio::sync::Mutex::new(()),
            opened: tokio::sync::Mutex::new(None),
            served: Mutex::new(Some(served)),
        }))
    }

    /// The protocol session, opened if it is not.
    async fn open(&self) -> Result<p::SessionOpened, RpcError> {
        let mut opened = self.opened.lock().await;
        if let Some(opened) = &*opened {
            return Ok(opened.clone());
        }
        let answer = self.rpc.call(method::SESSION_OPEN, p::Empty {}).await;
        let session: p::SessionOpened = answer
            .and_then(|v| serde_json::from_value(v).map_err(|e| p::Error::new(p::code::FAILED, e.to_string())))
            .map_err(|e| {
                RpcError::internal(format!("the services of {} did not start: {}", self.workspace, e.message))
            })?;
        *opened = Some(session.clone());
        Ok(session)
    }
}

type Answer = oneshot::Sender<Result<Value, p::Error>>;

/// A client of the protocol on one connection: requests answered as they
/// end, and each run's progress events to whoever follows it.
struct Rpc {
    out: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    next: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, Answer>>>,
    runs: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Progress>>>>,
    gone: Arc<AtomicBool>,
}

impl Rpc {
    fn over(input: impl AsyncRead + Unpin + Send + 'static, output: impl AsyncWrite + Unpin + Send + 'static) -> Self {
        let pending: Arc<Mutex<HashMap<u64, Answer>>> = Arc::default();
        let runs: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Progress>>>> = Arc::default();
        let gone: Arc<AtomicBool> = Arc::default();
        let (answers, events, ended) = (pending.clone(), runs.clone(), gone.clone());
        tokio::spawn(async move {
            let mut input = BufReader::new(input);
            let mut line = Vec::new();
            while let Ok(true) = p::read_line(&mut input, &mut line).await {
                match serde_json::from_slice(&line) {
                    Ok(ServerMessage::Response(response)) => {
                        let Some(Id::Number(id)) = response.id else { continue };
                        let answer = answers.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                        if let Some(answer) = answer {
                            let _ = answer.send(response.outcome());
                        }
                    }
                    Ok(ServerMessage::Notification(note)) if note.method == method::PROGRESS => {
                        // An event this build does not know is left out.
                        let Ok(event) = serde_json::from_value::<Progress>(note.params) else { continue };
                        if let Some(run) = events.lock().unwrap_or_else(PoisonError::into_inner).get(event.run()) {
                            let _ = run.send(event);
                        }
                    }
                    Ok(ServerMessage::Notification(_)) => {}
                    Err(e) => tracing::warn!("the server sent what is not a message: {e}"),
                }
            }
            ended.store(true, Ordering::SeqCst);
            let left: Vec<Answer> =
                answers.lock().unwrap_or_else(PoisonError::into_inner).drain().map(|(_, answer)| answer).collect();
            for answer in left {
                let _ = answer.send(Err(gone_error()));
            }
        });
        Self { out: tokio::sync::Mutex::new(Box::new(output)), next: AtomicU64::new(1), pending, runs, gone }
    }

    async fn call(&self, method: &str, params: impl Serialize) -> Result<Value, p::Error> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let request = p::Request::new(Id::Number(id), method, params)
            .map_err(|e| p::Error::new(p::code::FAILED, e.to_string()))?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, tx);
        // The reader marks the end before it answers what waits, so a call
        // put in after that sees it here.
        if self.gone.load(Ordering::SeqCst) || p::write_line(&mut *self.out.lock().await, &request).await.is_err() {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(gone_error());
        }
        rx.await.unwrap_or_else(|_| Err(gone_error()))
    }

    async fn call_as<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T, p::Error> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| p::Error::new(p::code::FAILED, format!("{method} answered: {e}")))
    }

    fn follow(&self, trace: &TraceId) -> mpsc::UnboundedReceiver<Progress> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.runs.lock().unwrap_or_else(PoisonError::into_inner).insert(trace.to_string(), tx);
        rx
    }

    fn unfollow(&self, trace: &TraceId) {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner).remove(trace.as_str());
    }
}

fn gone_error() -> p::Error {
    p::Error::new(p::code::FAILED, "the server is gone")
}

/// The task a prompt asks for, and the files of the folder it points at.
/// Text is the task; a file of the folder, linked or embedded whole, is
/// named in it as `@path` and given to the task as the interface's `@path`
/// is; a selection, or a file from elsewhere, goes into the task as text.
fn task_of(prompt: &[Value], workspace: &Path) -> Result<(String, Vec<String>), RpcError> {
    let mut task = String::new();
    let mut files: Vec<String> = Vec::new();
    let mut mention = |task: &mut String, rel: String| {
        task.push('@');
        task.push_str(&rel);
        if !files.contains(&rel) {
            files.push(rel);
        }
    };
    for block in prompt {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => task.push_str(block.get("text").and_then(Value::as_str).unwrap_or_default()),
            Some("resource_link") => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                match in_folder(uri, workspace) {
                    Some((rel, None)) => mention(&mut task, rel),
                    _ => task.push_str(uri),
                }
            }
            Some("resource") => {
                let resource = block.get("resource").cloned().unwrap_or_default();
                let uri = resource.get("uri").and_then(Value::as_str).unwrap_or_default();
                let text = resource.get("text").and_then(Value::as_str);
                match (in_folder(uri, workspace), text) {
                    (Some((rel, None)), _) => mention(&mut task, rel),
                    (place, Some(text)) => {
                        let name = match place {
                            Some((rel, Some(part))) => format!("{rel}#{part}"),
                            _ => uri.to_owned(),
                        };
                        task.push_str(&format!("\n\n{name}:\n```\n{text}\n```\n"));
                    }
                    (_, None) => task.push_str(uri),
                }
            }
            Some(other @ ("image" | "audio")) => {
                return Err(RpcError::params(format!("molt takes no {other} in a prompt, only text and files")))
            }
            other => return Err(RpcError::params(format!("a prompt block of type {other:?} is not one molt reads"))),
        }
    }
    Ok((task, files))
}

/// The path in `workspace` that `uri` names, and the part of the file after
/// its `#`, when it is a `file:` URI of a path inside it.
fn in_folder(uri: &str, workspace: &Path) -> Option<(String, Option<String>)> {
    let rest = uri.strip_prefix("file://")?;
    let (rest, part) = match rest.split_once('#') {
        Some((rest, part)) => (rest, Some(part.to_owned())),
        None => (rest, None),
    };
    let rest = rest.split_once('?').map_or(rest, |(rest, _)| rest);
    // `file://host/path` names another host; only `file:///path` and `file://localhost/path` are local.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let path = PathBuf::from(percent_decode(rest)?);
    let rel = path.strip_prefix(workspace).ok()?;
    let normal = rel.components().all(|c| matches!(c, Component::Normal(_)));
    let rel = rel.to_str().filter(|r| normal && !r.is_empty())?.to_owned();
    Some((rel, part))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// What a session update tells the editor.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case", rename_all_fields = "camelCase")]
enum Update {
    AgentMessageChunk {
        content: Block,
    },
    AgentThoughtChunk {
        content: Block,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: Kind,
        status: Status,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolContent>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        locations: Vec<Location>,
    },
    #[serde(rename = "tool_call_update")]
    ToolCallChanged {
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<Status>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ToolContent>>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
}

/// A text content block.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Block {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

fn text(text: impl Into<String>) -> Block {
    Block { kind: "text", text: text.into() }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
enum ToolContent {
    Content {
        content: Block,
    },
    Diff {
        path: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        new_text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Location {
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Read,
    Edit,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct PlanEntry {
    content: String,
    priority: &'static str,
    status: Status,
}

/// The kind of tool call a tool of the attempts is, for the editor's icon.
fn kind_of(tool: &str) -> Kind {
    use molt_api::planner::tools as t;
    match tool {
        t::READ_FILE | t::LIST_FILES | t::LOAD_SKILL => Kind::Read,
        t::WRITE_FILE | t::EDIT_FILE => Kind::Edit,
        t::SEARCH | t::FIND_SYMBOL | t::RECALL => Kind::Search,
        t::RUN | t::START_PROCESS => Kind::Execute,
        t::FETCH_URL => Kind::Fetch,
        t::EXPLORE => Kind::Think,
        _ => Kind::Other,
    }
}

/// The steps of a run's workflow, as the editor shows a plan, then the
/// steps of each attempt's own list (#264).
#[derive(Debug)]
struct Plan {
    entries: Vec<PlanEntry>,
    work: usize,
    review: Option<usize>,
    apply: Option<usize>,
    /// Each attempt's own list, as it last set it. Kept apart from the
    /// workflow's steps: a list all completed never moves the run on.
    todos: BTreeMap<u32, Vec<TodoItem>>,
}

impl Plan {
    /// A plan of no steps of the workflow, for a run that was not routed.
    fn lists_only() -> Self {
        Self { entries: Vec::new(), work: 0, review: None, apply: None, todos: BTreeMap::new() }
    }

    fn of(name: &str) -> Self {
        let mut entries = Vec::new();
        let mut step = |content: &str| {
            entries.push(PlanEntry { content: content.into(), priority: "medium", status: Status::Pending });
            entries.len() - 1
        };
        let Some(flow) = workflow::builtin(Some(name)) else {
            // A workflow of the planner's own config: only what every one has.
            let work = step("Do the task");
            let apply = Some(step("Apply the result once you allow it"));
            let mut plan = Self { entries, work, review: None, apply, todos: BTreeMap::new() };
            plan.entries[0].status = Status::InProgress;
            return plan;
        };
        if flow.stages.design.is_some() {
            step("Design the check the result must pass");
        }
        let work = step(if flow.read_only() { "Answer from the project" } else { "Make the change" });
        let review = flow.stages.review.is_some().then(|| step("Review the result"));
        let apply = (!flow.read_only()).then(|| step("Apply the result once you allow it"));
        let mut plan = Self { entries, work, review, apply, todos: BTreeMap::new() };
        plan.entries[0].status = Status::InProgress;
        plan
    }

    /// Step `at` is under way: every step before it is done.
    fn reach(&mut self, at: usize) -> bool {
        let mut changed = false;
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let status = match i.cmp(&at) {
                std::cmp::Ordering::Less => Status::Completed,
                std::cmp::Ordering::Equal => Status::InProgress,
                std::cmp::Ordering::Greater => continue,
            };
            if entry.status != status && entry.status != Status::Completed {
                entry.status = status;
                changed = true;
            }
        }
        changed
    }

    fn done(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            changed |= entry.status != Status::Completed;
            entry.status = Status::Completed;
        }
        changed
    }

    fn update(&self) -> Update {
        let mut entries = self.entries.clone();
        let several = self.todos.len() > 1;
        for (attempt, items) in &self.todos {
            for item in items {
                let (status, dropped) = match item.status {
                    TodoStatus::Pending => (Status::Pending, false),
                    TodoStatus::InProgress => (Status::InProgress, false),
                    TodoStatus::Completed => (Status::Completed, false),
                    // A plan entry is pending, in progress or completed: a dropped step says so in its words.
                    TodoStatus::Cancelled => (Status::Completed, true),
                };
                let mut content =
                    if several { format!("Attempt {}: {}", attempt + 1, item.content) } else { item.content.clone() };
                if dropped {
                    content.push_str(" (dropped)");
                }
                entries.push(PlanEntry { content, priority: "low", status });
            }
        }
        Update::Plan { entries }
    }
}

/// What the editor is shown of a run, from its progress events.
struct View {
    trace: String,
    workspace: PathBuf,
    plan: Option<Plan>,
    /// The tool call of each attempt still going: it ends when the attempt's
    /// next one starts, or the attempt does.
    open: BTreeMap<u32, String>,
    calls: u32,
    /// Each attempt's fork, which a stop looks at to keep its work.
    forks: BTreeMap<u32, String>,
    check_files: Vec<String>,
    /// Whose model text the thoughts show now.
    speaker: Option<String>,
}

impl View {
    fn new(trace: &TraceId, workspace: &str) -> Self {
        Self {
            trace: trace.to_string(),
            workspace: PathBuf::from(workspace),
            plan: None,
            open: BTreeMap::new(),
            calls: 0,
            forks: BTreeMap::new(),
            check_files: Vec::new(),
            speaker: None,
        }
    }

    fn thought(text_: impl Into<String>) -> Update {
        Update::AgentThoughtChunk { content: text(text_) }
    }

    fn reach(&mut self, step: impl Fn(&Plan) -> Option<usize>, out: &mut Vec<Update>) {
        if let Some(plan) = &mut self.plan {
            if let Some(at) = step(plan) {
                if plan.reach(at) {
                    out.push(plan.update());
                }
            }
        }
    }

    /// End the tool call attempt `attempt` has going.
    fn close(&mut self, attempt: u32, status: Status, out: &mut Vec<Update>) {
        if let Some(id) = self.open.remove(&attempt) {
            out.push(Update::ToolCallChanged { tool_call_id: id, status: Some(status), content: None });
        }
    }

    /// The updates that tell the editor of `event`. A question is not one of
    /// them: the turn asks it.
    fn event(&mut self, event: &Progress) -> Vec<Update> {
        let mut out = Vec::new();
        match event {
            Progress::Routed { workflow, reason, .. } => {
                let plan = Plan::of(workflow);
                out.push(plan.update());
                self.plan = Some(plan);
                let why = if reason.is_empty() { String::new() } else { format!(": {reason}") };
                out.push(Self::thought(format!("Workflow {workflow}{why}\n")));
            }
            Progress::Recalled { notes, .. } if !notes.is_empty() => {
                out.push(Self::thought(format!("Memory gave {} notes on this project.\n", notes.len())));
            }
            Progress::CheckReady { command, files, rubric, .. } => {
                self.check_files.clone_from(files);
                self.reach(|p| Some(p.work), &mut out);
                let line = match command {
                    Some(command) => format!("The check: `{command}`\n"),
                    None if !rubric.is_empty() => format!("The result is judged by {} criteria.\n", rubric.len()),
                    None => "No check applies: the result is not verified.\n".to_owned(),
                };
                out.push(Self::thought(line));
            }
            Progress::AttemptStarted { attempt, fork, .. } => {
                if let Some(fork) = fork {
                    self.forks.insert(*attempt, fork.clone());
                }
                self.reach(|p| Some(p.work), &mut out);
            }
            Progress::ToolCall { attempt, tool, detail, .. } => {
                self.close(*attempt, Status::Completed, &mut out);
                if tool == molt_api::planner::tools::ASK_USER {
                    return out;
                }
                self.calls += 1;
                let id = format!("{}/{}/{}", self.trace, attempt, self.calls);
                let title = if detail.is_empty() { tool.clone() } else { detail.clone() };
                let title = if self.forks.len() > 1 { format!("{title} (attempt {})", attempt + 1) } else { title };
                let kind = kind_of(tool);
                let locations = match kind {
                    Kind::Read | Kind::Edit => detail
                        .split_once(' ')
                        .map(|(_, path)| Path::new(path))
                        .filter(|path| path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)))
                        .map(|path| vec![Location { path: self.workspace.join(path) }])
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                out.push(Update::ToolCall {
                    tool_call_id: id.clone(),
                    title,
                    kind,
                    status: Status::InProgress,
                    content: Vec::new(),
                    locations,
                });
                self.open.insert(*attempt, id);
            }
            Progress::CheckRan { attempt, passed, exit_code, tail, judged, .. } => {
                self.close(*attempt, Status::Completed, &mut out);
                self.calls += 1;
                let what = if *judged { "Judge the result" } else { "Run the check" };
                let title =
                    if self.forks.len() > 1 { format!("{what} (attempt {})", attempt + 1) } else { what.to_owned() };
                let said = match (passed, exit_code) {
                    (true, _) => "passed".to_owned(),
                    (false, Some(code)) => format!("failed with exit code {code}"),
                    (false, None) => "failed".to_owned(),
                };
                let said = if tail.is_empty() { said } else { format!("{said}:\n```\n{tail}\n```") };
                out.push(Update::ToolCall {
                    tool_call_id: format!("{}/{}/{}", self.trace, attempt, self.calls),
                    title,
                    kind: Kind::Execute,
                    status: if *passed { Status::Completed } else { Status::Failed },
                    content: vec![ToolContent::Content { content: text(said) }],
                    locations: Vec::new(),
                });
            }
            Progress::Todo { attempt, items, .. } => {
                let plan = self.plan.get_or_insert_with(Plan::lists_only);
                plan.todos.insert(*attempt, items.clone());
                out.push(plan.update());
            }
            Progress::AttemptFinished { attempt, status, .. } => {
                self.close(*attempt, Status::Completed, &mut out);
                // The list of an attempt that did not pass no longer says what the run does.
                if *status != AttemptStatus::Passed {
                    if let Some(plan) = self.plan.as_mut() {
                        if plan.todos.remove(attempt).is_some() {
                            out.push(plan.update());
                        }
                    }
                }
                let said = match status {
                    AttemptStatus::Passed => "is done",
                    AttemptStatus::Failed => "did not pass",
                    AttemptStatus::Cancelled => "was stopped",
                    AttemptStatus::Error => "ended in an error",
                };
                out.push(Self::thought(format!("\nAttempt {} {said}.\n", attempt + 1)));
            }
            Progress::ModelStarted { context } => {
                if context.role == Some(Role::Reviewer) {
                    self.reach(|p| p.review, &mut out);
                }
            }
            Progress::ModelText { context, text: said } => {
                let speaker = molt_api::model::speaker(context.role, context.attempt);
                if self.speaker.as_ref() != Some(&speaker) {
                    out.push(Self::thought(format!("\n[{speaker}] ")));
                    self.speaker = Some(speaker);
                }
                out.push(Self::thought(said.clone()));
            }
            Progress::ModelRetry { failure, delay_ms, retry, retries, .. } => {
                let line = molt_api::progress::retry_line(failure, Duration::from_millis(*delay_ms), *retry, *retries);
                out.push(Self::thought(format!("\n{line}\n")));
            }
            Progress::Note { message, .. } => out.push(Self::thought(format!("\n{message}\n"))),
            _ => {}
        }
        out
    }

    /// The tool calls still going when the run stopped end as failed.
    fn stopped(&mut self) -> Vec<Update> {
        let mut out = Vec::new();
        for attempt in self.open.keys().copied().collect::<Vec<_>>() {
            self.close(attempt, Status::Failed, &mut out);
        }
        out
    }
}

/// What the person picked in the apply prompt.
#[derive(Debug, PartialEq)]
enum Decision {
    Apply,
    Keep,
    Discard,
    /// The prompt was cancelled, or answered with no option of ours.
    Unanswered,
}

/// The person's pick in a permission prompt: the option id, or `None` when
/// the editor cancelled it.
fn picked(answer: &Value) -> Option<&str> {
    let outcome = answer.get("outcome")?;
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => outcome.get("optionId").and_then(Value::as_str),
        _ => None,
    }
}

fn decision(answer: &Value) -> Decision {
    match picked(answer) {
        Some(APPLY) => Decision::Apply,
        Some(KEEP) => Decision::Keep,
        Some(DISCARD) => Decision::Discard,
        _ => Decision::Unanswered,
    }
}

/// A file's text when it can be shown: a regular file, not too large, in UTF-8.
fn shown(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > DIFF_FILE_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

/// The apply prompt's content: a diff for each file of the result that can
/// be shown, the workspace's text against the fork's, and a list of the
/// changes, with the patch for whatever is not shown.
fn apply_content(workspace: &Path, fork: &Path, resp: &RunResponse) -> Vec<ToolContent> {
    let mut diffs = Vec::new();
    let mut unshown = false;
    for change in &resp.changes {
        let rel = Path::new(&change.path);
        if diffs.len() >= DIFF_FILES || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            unshown = true;
            continue;
        }
        let old = match change.kind {
            ChangeKind::Added => Some(None),
            ChangeKind::Modified | ChangeKind::Deleted => shown(&workspace.join(rel)).map(Some),
        };
        let new = match change.kind {
            ChangeKind::Deleted => Some(String::new()),
            ChangeKind::Added | ChangeKind::Modified => shown(&fork.join(rel)),
        };
        match (old, new) {
            (Some(old_text), Some(new_text)) => {
                diffs.push(ToolContent::Diff { path: workspace.join(rel), old_text, new_text })
            }
            _ => unshown = true,
        }
    }
    let mut list: String = resp
        .changes
        .iter()
        .map(|c| {
            let kind = match c.kind {
                ChangeKind::Added => "added",
                ChangeKind::Modified => "modified",
                ChangeKind::Deleted => "deleted",
            };
            format!("- {kind} `{}`\n", c.path)
        })
        .collect();
    if let Some(line) = resp.left_out.as_ref().and_then(crate::agent::left_out_line) {
        list.push_str(&format!("\n{line}\n"));
    }
    if unshown && !resp.patch.is_empty() {
        let cut = if resp.patch_truncated { "\n(the patch is cut short)" } else { "" };
        list.push_str(&format!("\n```diff\n{}\n```{cut}\n", resp.patch.trim_end()));
    }
    let mut content = vec![ToolContent::Content { content: text(list) }];
    content.extend(diffs);
    content
}

/// What a finished run says, after its summary.
fn verdict(resp: &RunResponse) -> String {
    let outcome = match resp.outcome {
        Outcome::Passed => match &resp.check {
            Some(check) => format!("Passed its check `{}`.", check.command),
            None => "Passed its check.".to_owned(),
        },
        Outcome::Judged => "Met every criterion, as an independent judge found.".to_owned(),
        Outcome::Unverified if resp.workflow.as_deref() == Some(workflow::ASK) => "An answer, not verified.".to_owned(),
        Outcome::Unverified => "Not verified: no check ran on it.".to_owned(),
        Outcome::Failed => "No attempt passed.".to_owned(),
    };
    let cost = match resp.uncounted_calls {
        0 => format!("cost ${:.4}", resp.cost_usd),
        n => format!("cost ${:.4} known, and {n} model calls whose cost is not known", resp.cost_usd),
    };
    format!("{outcome} ({cost})")
}

fn bounded(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// One prompt of a session, from the task to the person's answer on its result.
struct Turn<'a> {
    editor: &'a Editor,
    session: &'a Session,
    stop: &'a CancellationToken,
}

impl Turn<'_> {
    fn say(&self, update: Update) {
        self.editor.update(&self.session.id, update);
    }

    fn message(&self, said: impl Into<String>) {
        self.say(Update::AgentMessageChunk { content: text(said) });
    }

    async fn run(&self, task: String, files: Vec<String>, earlier: Vec<Earlier>) -> Result<&'static str, RpcError> {
        let molt = &self.session.molt;
        let opened = molt.open().await?;
        if self.stop.is_cancelled() {
            return Ok("cancelled");
        }
        let defaults = &molt.defaults;
        let mut req = RunRequest::new(task.clone(), "");
        req.files = files;
        // The person in the editor answers the attempts' questions, and confirms the apply.
        req.interactive = true;
        req.apply = false;
        // A stop looks at what the attempts changed itself, as the interface's does.
        req.keep_cancelled_forks = true;
        req.workflow = Some(workflow::AUTO.to_owned());
        req.earlier = earlier;
        // The editor's session is the runs' session.
        req.session = Some(self.session.id.clone());
        req.stream = defaults.stream.unwrap_or(true);
        if let Some(n) = defaults.attempts {
            req.attempts = n;
        }
        req.model = defaults.model.clone();
        req.effort = defaults.effort;
        req.budget_usd = defaults.budget;
        req.check = defaults.check.clone();

        let trace = TraceId::random();
        let mut events = molt.rpc.follow(&trace);
        let mut view = View::new(&trace, &molt.workspace);
        let submit =
            molt.rpc.call_as::<RunResponse>(method::TASK_SUBMIT, p::Task { trace: trace.clone(), request: req });
        tokio::pin!(submit);
        let mut questions = JoinSet::new();
        let mut stopping: Option<JoinHandle<Result<p::Stopped, p::Error>>> = None;
        let answer = loop {
            tokio::select! {
                Some(event) = events.recv() => self.show(&mut view, &trace, event, &mut questions),
                answer = &mut submit => break answer,
                _ = self.stop.cancelled(), if stopping.is_none() => {
                    let stop = p::Stop {
                        attempts: view.forks.values().cloned().collect(),
                        check_files: view.check_files.clone(),
                        ..Default::default()
                    };
                    let session = self.session.molt.clone();
                    stopping = Some(tokio::spawn(async move { session.rpc.call_as(method::TASK_STOP, stop).await }));
                }
                Some(_) = questions.join_next(), if !questions.is_empty() => {}
            }
        };
        questions.abort_all();
        // Events published before the answer may still be on their way.
        while let Ok(event) = events.try_recv() {
            self.show(&mut view, &trace, event, &mut questions);
        }
        questions.abort_all();
        molt.rpc.unfollow(&trace);
        if let Some(stopping) = stopping {
            let stopped = stopping.await.unwrap_or_else(|e| Err(p::Error::new(p::code::FAILED, e.to_string())));
            // The stop closed the protocol session; the next task opens another.
            *molt.opened.lock().await = None;
            for update in view.stopped() {
                self.say(update);
            }
            self.message(match stopped {
                Ok(p::Stopped { kept, notes }) => {
                    let mut said = match kept.as_slice() {
                        [] => "Stopped. No attempt had changed a file yet.".to_owned(),
                        kept => format!(
                            "Stopped. What the attempts did is kept, and nothing was applied: {}. `molt apply FORK` \
                             in this folder applies one.",
                            kept.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
                        ),
                    };
                    for note in notes {
                        said.push_str(&format!("\n{note}"));
                    }
                    said
                }
                Err(e) => format!("Stopped, but the services did not say what they kept: {}", e.message),
            });
            return Ok("cancelled");
        }
        let resp = match answer {
            Ok(resp) => resp,
            Err(e) if e.code == p::code::CANCELLED => return Ok("cancelled"),
            Err(e) => return Err(RpcError::internal(e.message)),
        };
        self.remember(&task, &resp);
        let reason = self.deliver(&mut view, &resp).await;
        self.learn(&trace, &opened, &resp);
        Ok(reason)
    }

    /// Show `event`; a question is asked in a task of its own, so the run's
    /// events go on while the person thinks.
    fn show(&self, view: &mut View, trace: &TraceId, event: Progress, questions: &mut JoinSet<()>) {
        if let Progress::Asked { question, text: asked, options, .. } = event {
            let (editor, session, rpc_of) = (self.editor.clone(), self.session.id.clone(), self.session.molt.clone());
            let trace = trace.clone();
            questions.spawn(async move {
                ask(&editor, &session, &rpc_of.rpc, &trace, question, &asked, &options).await;
            });
            return;
        }
        for update in view.event(&event) {
            self.say(update);
        }
    }

    fn remember(&self, task: &str, resp: &RunResponse) {
        let mut state = self.session.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.earlier.push(Earlier {
            task: task.to_owned(),
            outcome: resp.outcome,
            summary: bounded(resp.summary.trim(), EARLIER_SUMMARY_BYTES).to_owned(),
            changes: resp.changes.iter().map(|c| c.path.clone()).collect(),
            applied: resp.applied,
        });
        let over = state.earlier.len().saturating_sub(EARLIER_TASKS);
        state.earlier.drain(..over);
    }

    /// Tell the editor what the run did and, for a result to apply, ask the
    /// person: only "Apply" applies it.
    async fn deliver(&self, view: &mut View, resp: &RunResponse) -> &'static str {
        let rpc = &self.session.molt.rpc;
        let mut said = resp.summary.trim().to_owned();
        if !said.is_empty() {
            said.push_str("\n\n");
        }
        said.push_str(&verdict(resp));
        self.message(said);
        let Some(fork) = resp.fork.clone().filter(|_| !resp.applied) else {
            if view.plan.as_mut().is_some_and(Plan::done) {
                self.say(view.plan.as_ref().unwrap().update());
            }
            return "end_turn";
        };
        let keep = |why: &str| {
            let fork = fork.clone();
            let why = why.to_owned();
            async move {
                if let Err(e) = rpc.call(method::RESULT_KEEP, p::Fork { fork: fork.clone() }).await {
                    tracing::warn!("could not keep {fork}: {}", e.message);
                }
                format!("{why} The result is kept in `{fork}`: `molt apply {fork}` in this folder applies it.")
            }
        };
        if resp.changes.is_empty() {
            let _ = rpc.call(method::RESULT_DISCARD, p::Fork { fork }).await;
            if view.plan.as_mut().is_some_and(Plan::done) {
                self.say(view.plan.as_ref().unwrap().update());
            }
            return "end_turn";
        }
        if resp.outcome == Outcome::Failed {
            self.message(format!("\n\n{}", keep("Nothing was applied.").await));
            return "end_turn";
        }
        let apply_step = view.plan.as_ref().and_then(|p| p.apply);
        if let Some(at) = apply_step {
            let plan = view.plan.as_mut().unwrap();
            plan.reach(at);
            self.say(plan.update());
        }
        let id = format!("{}/apply", view.trace);
        let content = apply_content(&view.workspace, Path::new(&fork), resp);
        let title = match resp.changes.len() {
            1 => format!("Apply the change to {}", resp.changes[0].path),
            n => format!("Apply the changes to {n} files"),
        };
        self.say(Update::ToolCall {
            tool_call_id: id.clone(),
            title: title.clone(),
            kind: Kind::Edit,
            status: Status::Pending,
            content: content.clone(),
            locations: resp
                .changes
                .iter()
                .filter(|c| Path::new(&c.path).components().all(|c| matches!(c, Component::Normal(_))))
                .map(|c| Location { path: view.workspace.join(&c.path) })
                .collect(),
        });
        let options = json!([
            {"optionId": APPLY, "name": "Apply", "kind": "allow_once"},
            {"optionId": KEEP, "name": "Keep it aside", "kind": "reject_once"},
            {"optionId": DISCARD, "name": "Discard", "kind": "reject_once"},
        ]);
        let ask = json!({
            "sessionId": self.session.id,
            "toolCall": {"toolCallId": id, "title": title, "kind": Kind::Edit, "status": Status::Pending, "content": content},
            "options": options,
        });
        let asked = self.editor.request(acp_method::REQUEST_PERMISSION, ask);
        let decision = tokio::select! {
            answer = asked => match answer {
                Ok(answer) => decision(&answer),
                Err(e) => {
                    tracing::warn!("the editor did not answer the apply prompt: {}", e.message);
                    Decision::Unanswered
                }
            },
            _ = self.stop.cancelled() => Decision::Unanswered,
        };
        let finish = |status: Status, said: String| Update::ToolCallChanged {
            tool_call_id: id.clone(),
            status: Some(status),
            content: Some(vec![ToolContent::Content { content: text(said) }]),
        };
        match decision {
            Decision::Apply => match rpc
                .call(method::RESULT_APPLY, p::Apply { fork: fork.clone(), paths: Vec::new() })
                .await
            {
                Ok(_) => {
                    let n = resp.changes.len();
                    self.say(finish(Status::Completed, format!("Applied {n} file{}.", if n == 1 { "" } else { "s" })));
                    if view.plan.as_mut().is_some_and(Plan::done) {
                        self.say(view.plan.as_ref().unwrap().update());
                    }
                }
                Err(e) => {
                    let kept = keep(&format!("It could not be applied: {}.", e.message)).await;
                    self.say(finish(Status::Failed, kept));
                }
            },
            Decision::Discard => {
                let said = match rpc.call(method::RESULT_DISCARD, p::Fork { fork: fork.clone() }).await {
                    Ok(_) => "Discarded; nothing was applied.".to_owned(),
                    Err(e) => format!("Not applied, and it could not be discarded: {}", e.message),
                };
                self.say(finish(Status::Failed, said));
            }
            Decision::Keep => self.say(finish(Status::Failed, keep("Not applied.").await)),
            Decision::Unanswered => {
                self.say(finish(Status::Failed, keep("Not applied.").await));
                if self.stop.is_cancelled() {
                    return "cancelled";
                }
            }
        }
        "end_turn"
    }

    /// Have memory learn from the run, as the interface does, without waiting for it.
    fn learn(&self, trace: &TraceId, opened: &p::SessionOpened, resp: &RunResponse) {
        let learns = workflow::builtin(resp.workflow.as_deref().or(Some(workflow::VERIFY)))
            .is_none_or(|w| w.stages.learn.is_some());
        let defaults = &self.session.molt.defaults;
        if !learns || opened.memory_problem.is_some() || crate::agent::skip_learning(resp, defaults.budget).is_some() {
            return;
        }
        let model = crate::agent::memory_model(defaults.memory_model.as_deref(), defaults.model.as_deref());
        let (molt, trace) = (self.session.molt.clone(), trace.clone());
        tokio::spawn(async move {
            if let Err(e) = molt.rpc.call(method::TASK_LEARN, p::Learn { trace, model }).await {
                tracing::warn!("memory did not learn from the run: {}", e.message);
            }
        });
    }
}

/// Ask the person question `question` of run `trace` in the editor, and
/// give the attempt their answer. A question offers its options, and one
/// that lets the attempt decide; ACP has no free-text answer yet.
async fn ask(
    editor: &Editor,
    session: &str,
    rpc: &Rpc,
    trace: &TraceId,
    question: u32,
    asked: &str,
    choices: &[String],
) {
    let id = format!("{trace}/ask/{question}");
    let update = Update::ToolCall {
        tool_call_id: id.clone(),
        title: asked.to_owned(),
        kind: Kind::Think,
        status: Status::Pending,
        content: Vec::new(),
        locations: Vec::new(),
    };
    editor.update(session, update);
    let mut options: Vec<Value> = choices
        .iter()
        .enumerate()
        .map(|(i, choice)| json!({"optionId": format!("option/{i}"), "name": choice, "kind": "allow_once"}))
        .collect();
    options.push(json!({"optionId": DECIDE, "name": "Let it decide", "kind": "reject_once"}));
    let params = json!({
        "sessionId": session,
        "toolCall": {"toolCallId": id, "title": asked, "kind": Kind::Think, "status": Status::Pending},
        "options": options,
    });
    let answer = match editor.request(acp_method::REQUEST_PERMISSION, params).await {
        Ok(answer) => answer,
        Err(e) => {
            tracing::warn!("the editor did not answer a question: {}", e.message);
            return;
        }
    };
    let text_ = match picked(&answer) {
        Some(DECIDE) => DECIDE_ANSWER.to_owned(),
        Some(option) => {
            match option.strip_prefix("option/").and_then(|i| i.parse::<usize>().ok()).and_then(|i| choices.get(i)) {
                Some(choice) => choice.clone(),
                None => DECIDE_ANSWER.to_owned(),
            }
        }
        // Cancelled: the run is being stopped.
        None => return,
    };
    let answered =
        rpc.call(method::QUESTION_ANSWER, p::Answer { trace: trace.clone(), question, text: text_.clone() }).await;
    let (status, said) = match answered {
        Ok(_) => (Status::Completed, format!("Answer: {text_}")),
        Err(e) => (Status::Failed, format!("The answer did not reach the attempt: {}", e.message)),
    };
    editor.update(
        session,
        Update::ToolCallChanged {
            tool_call_id: id,
            status: Some(status),
            content: Some(vec![ToolContent::Content { content: text(said) }]),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    /// An editor of `molt acp` over an in-memory pipe, with no services.
    struct Client {
        out: tokio::io::WriteHalf<DuplexStream>,
        input: BufReader<tokio::io::ReadHalf<DuplexStream>>,
        ran: JoinHandle<anyhow::Result<()>>,
    }

    impl Client {
        fn new() -> Self {
            let (ours, theirs) = tokio::io::duplex(1 << 20);
            let (theirs_in, theirs_out) = tokio::io::split(theirs);
            let ran = tokio::spawn(run(Options::default(), theirs_in, theirs_out, std::future::pending()));
            let (input, out) = tokio::io::split(ours);
            Self { out, input: BufReader::new(input), ran }
        }

        async fn send(&mut self, msg: Value) {
            self.out.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
        }

        async fn recv(&mut self) -> Value {
            let mut line = Vec::new();
            assert!(p::read_line(&mut self.input, &mut line).await.unwrap(), "molt acp hung up");
            serde_json::from_slice(&line).unwrap()
        }

        async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
            let reply = self.recv().await;
            assert_eq!(reply["id"], id, "{reply}");
            reply
        }
    }

    #[tokio::test]
    async fn an_editor_is_told_what_molt_speaks_and_requests_it_cannot_take_are_answered_not_fatal() {
        let mut editor = Client::new();
        let early = editor.call(1, "session/new", json!({"cwd": "/", "mcpServers": []})).await;
        assert_eq!(early["error"]["code"], code::INVALID_REQUEST, "{early}");
        let hello = editor.call(2, "initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
        let hello = &hello["result"];
        assert_eq!(hello["protocolVersion"], 1);
        assert_eq!(hello["agentCapabilities"]["loadSession"], false);
        assert_eq!(hello["agentCapabilities"]["promptCapabilities"]["image"], false);
        assert_eq!(hello["authMethods"], json!([]));
        assert_eq!(hello["agentInfo"]["name"], "molt");
        // A newer editor is told the version molt speaks, and decides.
        let newer = editor.call(3, "initialize", json!({"protocolVersion": 7})).await;
        assert_eq!(newer["result"]["protocolVersion"], 1);

        editor.send(json!("not a request")).await;
        assert_eq!(editor.recv().await["error"]["code"], code::INVALID_REQUEST);
        editor.out.write_all(b"{not json\n").await.unwrap();
        assert_eq!(editor.recv().await["error"]["code"], code::PARSE);
        let unknown = editor.call(4, "session/load", json!({"sessionId": "s", "cwd": "/", "mcpServers": []})).await;
        assert_eq!(unknown["error"]["code"], code::METHOD_NOT_FOUND, "{unknown}");
        let relative = editor.call(5, "session/new", json!({"cwd": "project", "mcpServers": []})).await;
        assert_eq!(relative["error"]["code"], code::INVALID_PARAMS, "{relative}");
        assert!(relative["error"]["message"].as_str().unwrap().contains("absolute"), "{relative}");
        let missing = editor.call(6, "session/new", json!({"cwd": "/nowhere/at/all", "mcpServers": []})).await;
        assert_eq!(missing["error"]["code"], code::INVALID_PARAMS, "{missing}");
        let file = tempfile::NamedTempFile::new().unwrap();
        let not_dir = editor.call(7, "session/new", json!({"cwd": file.path(), "mcpServers": []})).await;
        assert_eq!(not_dir["error"]["code"], code::INVALID_PARAMS, "{not_dir}");
        let text = json!([{"type": "text", "text": "hi"}]);
        let nobody = editor.call(8, "session/prompt", json!({"sessionId": "sess_x", "prompt": text})).await;
        assert_eq!(nobody["error"]["code"], code::RESOURCE_NOT_FOUND, "{nobody}");
        // A cancel is a notification: unknown or not, nothing comes back for it.
        editor.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "sess_x"}})).await;
        let auth = editor.call(9, "authenticate", json!({"methodId": "none"})).await;
        assert_eq!(auth["result"], json!({}));

        let Client { out, input, ran } = editor;
        drop((out, input));
        let ended = tokio::time::timeout(Duration::from_secs(10), ran).await.unwrap().unwrap();
        ended.unwrap();
    }

    #[test]
    fn a_prompt_is_the_task_and_the_folders_files_it_points_at_go_with_it() {
        let ws = Path::new("/home/me/project");
        let prompt = vec![
            json!({"type": "text", "text": "Fix "}),
            json!({"type": "resource_link", "uri": "file:///home/me/project/src/main%20file.rs", "name": "main file.rs"}),
            json!({"type": "text", "text": " like "}),
            json!({"type": "resource", "resource": {"uri": "file:///home/me/project/src/lib.rs", "text": "fn a() {}"}}),
            json!({"type": "resource", "resource": {"uri": "file:///home/me/project/src/lib.rs#L1:2", "text": "fn b() {}"}}),
            json!({"type": "resource_link", "uri": "file:///etc/passwd", "name": "passwd"}),
            json!({"type": "resource_link", "uri": "file:///home/me/project/../other/x", "name": "x"}),
            json!({"type": "resource_link", "uri": "file:///home/me/project/src/lib.rs", "name": "lib.rs"}),
        ];
        let (task, files) = task_of(&prompt, ws).unwrap();
        assert_eq!(files, ["src/main file.rs", "src/lib.rs"], "each once, and only the folder's own");
        assert!(
            task.starts_with("Fix @src/main file.rs like @src/lib.rs\n\nsrc/lib.rs#L1:2:\n```\nfn b() {}\n```"),
            "{task}"
        );
        assert!(task.contains("file:///etc/passwd") && task.contains("file:///home/me/project/../other/x"), "{task}");
        assert!(task.ends_with("@src/lib.rs"), "{task}");

        let image = vec![json!({"type": "image", "data": "", "mimeType": "image/png"})];
        assert_eq!(task_of(&image, ws).unwrap_err().code, code::INVALID_PARAMS);
        assert_eq!(in_folder("file://elsewhere/home/me/project/a", ws), None, "another host's file");
        assert_eq!(in_folder("file://localhost/home/me/project/a?x=1", ws), Some(("a".into(), None)));
        assert_eq!(in_folder("file:///home/me/project", ws), None, "the folder itself is no file");
        assert_eq!(in_folder("https://example.com/home/me/project/a", ws), None);
    }

    fn progress(event: Value) -> Progress {
        serde_json::from_value(event).unwrap()
    }

    #[test]
    fn a_runs_events_become_its_plan_its_tool_calls_with_their_status_and_its_thoughts() {
        let trace = TraceId::from_raw("trace_1");
        let mut view = View::new(&trace, "/w");
        let routed = view
            .event(&progress(json!({"event": "routed", "run": "trace_1", "workflow": "verify", "reason": "a change"})));
        let Update::Plan { entries } = &routed[0] else { panic!("{routed:?}") };
        let steps: Vec<(&str, Status)> = entries.iter().map(|e| (e.content.as_str(), e.status)).collect();
        assert_eq!(
            steps,
            [
                ("Design the check the result must pass", Status::InProgress),
                ("Make the change", Status::Pending),
                ("Review the result", Status::Pending),
                ("Apply the result once you allow it", Status::Pending),
            ]
        );
        let ready = view.event(&progress(json!({"event": "check_ready", "run": "trace_1", "command": "sh check.sh", "files": ["check.sh"], "designed": true})));
        let Update::Plan { entries } = &ready[0] else { panic!("{ready:?}") };
        assert_eq!((entries[0].status, entries[1].status), (Status::Completed, Status::InProgress));
        assert_eq!(view.check_files, ["check.sh"]);
        view.event(&progress(
            json!({"event": "attempt_started", "run": "trace_1", "attempt": 0, "fork": "/d/work/f0"}),
        ));
        let write = view.event(&progress(json!({"event": "tool_call", "run": "trace_1", "attempt": 0, "turn": 1, "tool": "write_file", "detail": "write greeting.txt"})));
        assert_eq!(
            write,
            [Update::ToolCall {
                tool_call_id: "trace_1/0/1".into(),
                title: "write greeting.txt".into(),
                kind: Kind::Edit,
                status: Status::InProgress,
                content: vec![],
                locations: vec![Location { path: "/w/greeting.txt".into() }],
            }]
        );
        let wire = serde_json::to_value(&write[0]).unwrap();
        assert_eq!(wire["sessionUpdate"], "tool_call");
        assert_eq!(wire["toolCallId"], "trace_1/0/1");
        assert_eq!(wire["status"], "in_progress");
        let run = view.event(&progress(json!({"event": "tool_call", "run": "trace_1", "attempt": 0, "turn": 2, "tool": "run", "detail": "run ../../etc"})));
        assert_eq!(
            run[0],
            Update::ToolCallChanged {
                tool_call_id: "trace_1/0/1".into(),
                status: Some(Status::Completed),
                content: None
            },
            "the next call ends the last"
        );
        let Update::ToolCall { kind, locations, .. } = &run[1] else { panic!("{run:?}") };
        assert_eq!((*kind, locations.len()), (Kind::Execute, 0));
        let checked = view.event(&progress(json!({"event": "check_ran", "run": "trace_1", "attempt": 0, "passed": false, "exit_code": 1, "tail": "no hello"})));
        assert_eq!(
            checked[0],
            Update::ToolCallChanged {
                tool_call_id: "trace_1/0/2".into(),
                status: Some(Status::Completed),
                content: None
            }
        );
        let Update::ToolCall { status, content, .. } = &checked[1] else { panic!("{checked:?}") };
        assert_eq!(*status, Status::Failed);
        assert_eq!(content, &[ToolContent::Content { content: text("failed with exit code 1:\n```\nno hello\n```") }]);
        view.event(&progress(
            json!({"event": "attempt_started", "run": "trace_1", "attempt": 1, "fork": "/d/work/f1"}),
        ));
        let second = view.event(&progress(json!({"event": "tool_call", "run": "trace_1", "attempt": 1, "turn": 1, "tool": "read_file", "detail": "read a.txt"})));
        let Update::ToolCall { title, .. } = &second[0] else { panic!("{second:?}") };
        assert_eq!(title, "read a.txt (attempt 2)", "with attempts side by side, each says whose");
        assert_eq!(view.forks.values().collect::<Vec<_>>(), ["/d/work/f0", "/d/work/f1"]);
        let text_ = view.event(&progress(json!({"event": "model_text", "run": "trace_1", "attempt": 1, "role": "attempt", "turn": 2, "call": "c", "seq": 1, "text": "Looking"})));
        assert_eq!(text_, [View::thought("\n[attempt 2] "), View::thought("Looking")]);
        let more = view.event(&progress(json!({"event": "model_text", "run": "trace_1", "attempt": 1, "role": "attempt", "turn": 2, "call": "c", "seq": 2, "text": " around"})));
        assert_eq!(more, [View::thought(" around")]);
        let review = view.event(&progress(json!({"event": "model_started", "run": "trace_1", "attempt": null, "role": "reviewer", "turn": 1, "call": "r", "seq": 0})));
        let Update::Plan { entries } = &review[0] else { panic!("{review:?}") };
        assert_eq!((entries[1].status, entries[2].status), (Status::Completed, Status::InProgress));
        // An ask_user call is the turn's to show, as a question.
        let ask = view.event(&progress(json!({"event": "tool_call", "run": "trace_1", "attempt": 1, "turn": 3, "tool": "ask_user", "detail": "ask the user: which?"})));
        assert_eq!(ask.len(), 1, "only the end of the call before it: {ask:?}");
        let finished = view
            .event(&progress(json!({"event": "attempt_finished", "run": "trace_1", "attempt": 0, "status": "passed"})));
        assert_eq!(finished, [View::thought("\nAttempt 1 is done.\n")], "its last call ended with its check");
        view.event(&progress(json!({"event": "tool_call", "run": "trace_1", "attempt": 1, "turn": 4, "tool": "run", "detail": "run sleep 9"})));
        assert_eq!(
            view.stopped(),
            [Update::ToolCallChanged {
                tool_call_id: "trace_1/1/5".into(),
                status: Some(Status::Failed),
                content: None
            }],
            "a stop ends what is still going as failed"
        );
    }

    #[test]
    fn each_attempts_own_list_follows_the_workflows_steps_and_never_moves_them_on() {
        let trace = TraceId::from_raw("trace_1");
        let mut view = View::new(&trace, "/w");
        let entries = |updates: &[Update]| -> Vec<(String, Status, &'static str)> {
            let Some(Update::Plan { entries }) = updates.iter().rfind(|u| matches!(u, Update::Plan { .. })) else {
                panic!("{updates:?}")
            };
            entries.iter().map(|e| (e.content.clone(), e.status, e.priority)).collect()
        };
        // A run that was not routed has only the lists.
        let alone = View::new(&trace, "/w").event(&progress(json!({"event": "todo", "run": "trace_1", "attempt": 0,
            "items": [{"content": "read the lexer", "status": "in_progress"}]})));
        assert_eq!(entries(&alone), [("read the lexer".to_owned(), Status::InProgress, "low")]);

        view.event(&progress(json!({"event": "routed", "run": "trace_1", "workflow": "quick"})));
        let listed = view.event(&progress(json!({"event": "todo", "run": "trace_1", "attempt": 0, "items": [
            {"content": "read the lexer", "status": "completed"},
            {"content": "fix it", "status": "completed"},
            {"content": "old idea", "status": "cancelled"},
        ]})));
        let workflow = entries(&listed).len() - 3;
        let shown = entries(&listed);
        assert_eq!(
            shown[workflow..],
            [
                ("read the lexer".to_owned(), Status::Completed, "low"),
                ("fix it".to_owned(), Status::Completed, "low"),
                ("old idea (dropped)".to_owned(), Status::Completed, "low"),
            ]
        );
        // Every step of the list done: the workflow is where it was.
        assert!(shown[..workflow].iter().skip(1).all(|(_, status, _)| *status == Status::Pending), "{shown:?}");
        let second = view.event(&progress(json!({"event": "todo", "run": "trace_1", "attempt": 1,
            "items": [{"content": "try the parser", "status": "pending"}]})));
        let shown = entries(&second);
        assert_eq!(shown[workflow].0, "Attempt 1: read the lexer", "with two lists, each says whose");
        assert_eq!(shown.last().unwrap().0, "Attempt 2: try the parser");
        // The list of an attempt that did not pass goes; one that passed stays.
        let failed = view
            .event(&progress(json!({"event": "attempt_finished", "run": "trace_1", "attempt": 0, "status": "failed"})));
        assert_eq!(entries(&failed)[workflow..], [("try the parser".to_owned(), Status::Pending, "low")]);
        let passed = view
            .event(&progress(json!({"event": "attempt_finished", "run": "trace_1", "attempt": 1, "status": "passed"})));
        assert!(!passed.iter().any(|u| matches!(u, Update::Plan { .. })), "{passed:?}");
    }

    #[test]
    fn only_the_apply_option_applies_a_result() {
        let selected = |id: &str| json!({"outcome": {"outcome": "selected", "optionId": id}});
        assert_eq!(decision(&selected("apply")), Decision::Apply);
        assert_eq!(decision(&selected("keep")), Decision::Keep);
        assert_eq!(decision(&selected("discard")), Decision::Discard);
        for other in [
            selected("allow"),
            selected("Apply"),
            selected("option/0"),
            json!({"outcome": {"outcome": "cancelled"}}),
            json!({}),
            json!({"optionId": "apply"}),
        ] {
            assert_eq!(decision(&other), Decision::Unanswered, "{other}");
        }
    }

    #[test]
    fn the_apply_prompt_shows_each_file_as_it_is_and_as_the_result_has_it() {
        use molt_api::fs::Change;
        let (ws, fork) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(ws.path().join("a.txt"), "old\n").unwrap();
        std::fs::write(fork.path().join("a.txt"), "new\n").unwrap();
        std::fs::write(fork.path().join("b.txt"), "added\n").unwrap();
        std::fs::write(ws.path().join("c.txt"), "gone\n").unwrap();
        std::fs::write(fork.path().join("bin"), [0xff, 0xfe]).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", fork.path().join("link")).unwrap();
        let mut resp = crate::serve::tests::run_response("a.txt");
        resp.changes = [
            ("a.txt", ChangeKind::Modified),
            ("b.txt", ChangeKind::Added),
            ("c.txt", ChangeKind::Deleted),
            ("bin", ChangeKind::Added),
            ("link", ChangeKind::Added),
            ("../escape", ChangeKind::Added),
        ]
        .into_iter()
        .map(|(path, kind)| Change { path: path.into(), kind })
        .collect();
        resp.patch = "PATCH".into();
        let content = apply_content(ws.path(), fork.path(), &resp);
        let diffs: Vec<_> = content.iter().filter(|c| matches!(c, ToolContent::Diff { .. })).collect();
        assert_eq!(
            diffs,
            [
                &ToolContent::Diff {
                    path: ws.path().join("a.txt"),
                    old_text: Some("old\n".into()),
                    new_text: "new\n".into()
                },
                &ToolContent::Diff { path: ws.path().join("b.txt"), old_text: None, new_text: "added\n".into() },
                &ToolContent::Diff {
                    path: ws.path().join("c.txt"),
                    old_text: Some("gone\n".into()),
                    new_text: String::new()
                },
            ],
            "binary files, links and paths out of the folder are not read"
        );
        let ToolContent::Content { content: Block { text: list, .. } } = &content[0] else { panic!("{content:?}") };
        assert!(list.contains("- modified `a.txt`") && list.contains("- added `../escape`"), "{list}");
        assert!(list.contains("```diff\nPATCH\n```"), "what is not shown is in the patch: {list}");
        let wire = serde_json::to_value(&content[2]).unwrap();
        assert_eq!(wire, json!({"type": "diff", "path": ws.path().join("b.txt"), "newText": "added\n"}));
    }
}
