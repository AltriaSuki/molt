//! The memory service: serves `memory.*`.
//!
//! See [`molt_api::memory`] for what it holds and the calls it answers.
//! Everything lives in one SQLite database ([`Db`]). The service reaches
//! the audit log and the model gateway only through a [`Bus`], so its logic
//! can be tested with scripted fakes.

mod consolidate;
pub mod db;
mod digest;
mod error;
mod notes;
pub mod project;
mod review;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context};
use async_trait::async_trait;
use molt_api::fs::{self as fs_api, FilesChanged};
use molt_api::memory::{
    ConsolidateRequest, CorrectRequest, ForgetRequest, ForgetResponse, IndexRequest, MapRequest, RecallRequest,
    RecallResponse, RememberRequest, RememberResponse, RetractRequest, RetractResponse, ReviewRequest, SymbolsRequest,
};
use molt_api::model::Effort;
use molt_proto::{Budget, Envelope, ErrorCode, Kind, RemoteError, Target, TraceId};
use molt_sdk::{CallOpts, SdkError, Service};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

pub use db::Db;

use crate::consolidate::{Deadline, Learner};
use crate::error::{failed, invalid};
use crate::notes::NewNote;

/// Molt's data directory, which no workspace may be inside.
const DATA_DIR: &str = ".molt";

/// Memory settings. [`Config::from_env`] documents the variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// Model that reads finished episodes, unless a request names one.
    pub model: String,
    pub effort: Effort,
    /// `max_tokens` of that model call.
    pub max_tokens: u32,
    /// Deadline for that model call.
    pub model_timeout: Duration,
    /// Requests handled at once.
    pub max_concurrent: usize,
    /// Directories the project model leaves out: Molt's data directory,
    /// with the `fs` service's forks in it, when it is inside the root.
    pub skip: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "sonnet".into(),
            effort: Effort::Low,
            max_tokens: 8_000,
            model_timeout: Duration::from_secs(300),
            max_concurrent: 8,
            skip: Vec::new(),
        }
    }
}

impl Config {
    /// [`Config::default`] overridden by:
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `MOLT_MEMORY_MODEL` | `sonnet` |
    /// | `MOLT_MEMORY_EFFORT` | `low` |
    /// | `MOLT_MEMORY_MAX_TOKENS` | `8000` |
    /// | `MOLT_MEMORY_MODEL_TIMEOUT_S` | `300` |
    ///
    /// An empty variable counts as unset; a value that does not parse, or a
    /// zero, is an error.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let number = |name: &str, default: u64| -> anyhow::Result<u64> {
            let Some(v) = get(name) else { return Ok(default) };
            let n: u64 = v.parse().with_context(|| format!("{name}={v:?} is not a number"))?;
            ensure!(n > 0, "{name}={v:?} must be at least 1");
            Ok(n)
        };
        let d = Self::default();
        let effort = match get("MOLT_MEMORY_EFFORT") {
            Some(v) => v.parse().map_err(|e: String| anyhow::anyhow!("MOLT_MEMORY_EFFORT: {e}"))?,
            None => d.effort,
        };
        Ok(Self {
            model: get("MOLT_MEMORY_MODEL").unwrap_or(d.model),
            effort,
            max_tokens: u32::try_from(number("MOLT_MEMORY_MAX_TOKENS", d.max_tokens.into())?)
                .context("MOLT_MEMORY_MAX_TOKENS is too large")?,
            model_timeout: Duration::from_secs(number("MOLT_MEMORY_MODEL_TIMEOUT_S", d.model_timeout.as_secs())?),
            max_concurrent: d.max_concurrent,
            skip: d.skip,
        })
    }
}

/// Who writes the notes the service learns: the service as the kernel
/// knows it, and its version. Rolling that version back retracts them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writer {
    pub service: String,
    pub version: String,
}

impl Writer {
    /// This service's id, and the version the kernel launched it at (`dev`
    /// when it was started some other way).
    pub fn of(svc: &Service) -> Self {
        Self { service: svc.id().to_string(), version: svc.version().unwrap_or("dev").to_owned() }
    }
}

/// How memory reaches other services and the kernel.
#[async_trait]
pub trait Bus: Send + Sync + 'static {
    /// Call `target` (e.g. `kernel.audit.read`) and wait for the reply payload.
    async fn call(&self, target: &str, payload: Value, budget: Budget, trace: &TraceId) -> Result<Value, RemoteError>;
}

/// [`Bus`] over a real service link. Every call carries the capability the
/// service holds for its target, kernel methods included.
pub struct ServiceBus(pub Arc<Service>);

#[async_trait]
impl Bus for ServiceBus {
    async fn call(&self, target: &str, payload: Value, budget: Budget, trace: &TraceId) -> Result<Value, RemoteError> {
        let cap = target.parse::<Target>().ok().and_then(|t| self.0.cap_for(&t));
        let opts = CallOpts { cap, budget, trace: Some(trace.clone()) };
        self.0.call(target, payload, opts).await.map_err(|e| match e {
            SdkError::Remote(e) => e,
            other => RemoteError { code: ErrorCode::Unavailable, message: format!("calling {target}: {other}") },
        })
    }
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn parse<T: DeserializeOwned>(method: &str, payload: Value) -> Result<T, RemoteError> {
    serde_json::from_value(payload).map_err(|e| invalid(format!("bad memory.{method} request: {e}")))
}

fn reply<T: Serialize>(value: T) -> Result<Value, RemoteError> {
    serde_json::to_value(value).map_err(|e| failed(format!("encoding the reply: {e}")))
}

/// Local recall for trusted host tools. Checks dependencies and records
/// review state in the database; use the service for kernel-audited calls.
/// `req.workspace` must be canonical. Capturing requires a service request.
pub fn recall(db: &Db, req: &RecallRequest) -> Result<Vec<molt_api::memory::Recalled>, RemoteError> {
    review::recall(db, req, None, now_ms())
}

/// Read immutable context/tool records for a canonical project and run.
pub fn recall_snapshots(
    db: &Db,
    workspace: &str,
    run: &str,
) -> Result<Vec<molt_api::memory::RecallSnapshot>, RemoteError> {
    review::snapshots(db, workspace, run)
}

/// The memory service's state.
pub struct Memory {
    db: Arc<Db>,
    /// Canonical; every workspace must be inside it.
    root: PathBuf,
    cfg: Config,
    writer: Writer,
    bus: Arc<dyn Bus>,
    /// Index updates run one at a time, so two never parse the same files.
    indexing: tokio::sync::Mutex<()>,
    /// Files `fs` changed that are waiting to be indexed again.
    changes: std::sync::Mutex<Waiting>,
}

/// Changed files waiting to be indexed again, by workspace. While one event
/// handler works through them (`draining`), the others only add to them, so
/// a slow index never has events pile up behind it.
#[derive(Default)]
struct Waiting {
    paths: BTreeMap<PathBuf, BTreeSet<String>>,
    draining: bool,
}

impl Memory {
    /// Memory over `db`, for workspaces inside `root`.
    pub fn new(db: Arc<Db>, root: &Path, cfg: Config, writer: Writer, bus: Arc<dyn Bus>) -> anyhow::Result<Self> {
        let root = root.canonicalize().with_context(|| format!("root {}", root.display()))?;
        let mut cfg = cfg;
        // The walk sees canonical paths; a directory not made yet cannot hold forks.
        cfg.skip = cfg.skip.iter().filter_map(|d| d.canonicalize().ok()).collect();
        Ok(Self {
            db,
            root,
            cfg,
            writer,
            bus,
            indexing: tokio::sync::Mutex::new(()),
            changes: std::sync::Mutex::default(),
        })
    }

    /// The canonical directory a request's `workspace` names: an absolute
    /// path or one relative to the root, which must be a directory inside it.
    fn workspace(&self, workspace: &str) -> Result<PathBuf, RemoteError> {
        if workspace.contains('\0') {
            return Err(invalid("workspace contains a NUL byte"));
        }
        let path = match self.root.join(workspace).canonicalize() {
            Ok(p) => p,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                return Err(invalid(format!("workspace {workspace} does not exist")))
            }
            Err(e) => return Err(failed(format!("workspace {workspace}: {e}"))),
        };
        let Ok(inside) = path.strip_prefix(&self.root) else {
            return Err(invalid(format!("workspace {workspace} is outside the directories memory may use")));
        };
        if inside.components().any(|c| c == Component::Normal(DATA_DIR.as_ref())) {
            return Err(invalid(format!("workspace {workspace} is inside {DATA_DIR}, Molt's own data directory")));
        }
        if !path.is_dir() {
            return Err(invalid(format!("workspace {workspace} is not a directory")));
        }
        Ok(path)
    }

    fn workspace_key(&self, workspace: &str) -> Result<String, RemoteError> {
        let path = self.workspace(workspace)?;
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid(format!("workspace {} is not valid UTF-8", path.display())))
    }

    /// Run `f` on the database off the async runtime.
    async fn blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Db) -> Result<R, RemoteError> + Send + 'static,
    ) -> Result<R, RemoteError> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db)).await.map_err(|e| failed(format!("memory failed: {e}")))?
    }

    /// Bring the model of the canonical `ws` up to date, one update at a time.
    async fn index(&self, ws: PathBuf, paths: Option<Vec<String>>) -> Result<Value, RemoteError> {
        let _turn = self.indexing.lock().await;
        let skip = self.cfg.skip.clone();
        reply(self.blocking(move |db| project::index_skipping(db, &ws, paths.as_deref(), &skip)).await?)
    }

    /// Handle one request or event addressed to the service.
    pub async fn handle(&self, req: Envelope) -> Result<Value, RemoteError> {
        if req.kind == Kind::Event {
            if let Target::Topic { name } = &req.to {
                if name == fs_api::CHANGED {
                    self.files_changed(req.payload, req.trace_id.to_string()).await;
                }
            }
            return Ok(Value::Null);
        }
        let method = match &req.to {
            Target::Method { method, .. } => method.clone(),
            other => return Err(invalid(format!("memory does not serve {other}"))),
        };
        let now = now_ms();
        match method.as_str() {
            "remember" => {
                let r: RememberRequest = parse(&method, req.payload)?;
                let service = req.from.as_ref().ok_or_else(|| invalid("the request has no sender"))?.to_string();
                let workspace = r.workspace.as_deref().map(|w| self.workspace_key(w)).transpose()?;
                let new = NewNote {
                    kind: r.kind,
                    text: r.text,
                    workspace,
                    confidence: r.confidence.unwrap_or(notes::DEFAULT_CONFIDENCE),
                    provenance: molt_api::memory::Provenance {
                        trace: r.trace,
                        events: r.events,
                        service,
                        version: r.version,
                    },
                };
                let (note, reinforced) = self.blocking(move |db| notes::remember(db, new, now)).await?;
                reply(RememberResponse { note, reinforced })
            }
            "recall" => {
                let mut r: RecallRequest = parse(&method, req.payload)?;
                r.workspace = r.workspace.as_deref().map(|w| self.workspace_key(w)).transpose()?;
                if r.capture.is_some() && req.from.as_ref().map(|s| s.as_str()) != Some("planner") {
                    return Err(invalid("only the kernel-identified planner may capture run context"));
                }
                let run = req.trace_id.to_string();
                let call = req.id.to_string();
                let notes = self.blocking(move |db| review::recall(db, &r, Some((&run, &call)), now)).await?;
                reply(RecallResponse { notes })
            }
            "review" | "correct" => {
                if req.from.as_ref().map(|s| s.as_str()) != Some("cli") {
                    return Err(invalid("only the kernel-identified CLI may review or correct notes"));
                }
                let (mut r, correction) = if method == "correct" {
                    let r: CorrectRequest = parse(&method, req.payload)?;
                    (r.review, Some(r.text))
                } else {
                    (parse::<ReviewRequest>(&method, req.payload)?, None)
                };
                r.workspace = self.workspace_key(&r.workspace)?;
                if r.depends_on
                    .iter()
                    .any(|p| self.cfg.skip.iter().any(|skip| Path::new(&r.workspace).join(p).starts_with(skip)))
                {
                    return Err(invalid("dependencies cannot name Molt's own data directory"));
                }
                let provenance = molt_api::memory::Provenance {
                    trace: req.trace_id.to_string(),
                    events: vec![req.id.to_string()],
                    service: "cli".into(),
                    version: "user".into(),
                };
                reply(self.blocking(move |db| review::review(db, &r, correction.as_deref(), provenance, now)).await?)
            }
            "forget" => {
                let r: ForgetRequest = parse(&method, req.payload)?;
                let trace = req.trace_id.to_string();
                let forgotten = self.blocking(move |db| notes::forget(db, &r.id, &r.reason, &trace, now)).await?;
                reply(ForgetResponse { forgotten })
            }
            "retract" => {
                let r: RetractRequest = parse(&method, req.payload)?;
                let trace = req.trace_id.to_string();
                let done =
                    self.blocking(move |db| notes::retract(db, &r.service, &r.version, &r.reason, &trace, now)).await?;
                reply(RetractResponse { retracted: done.retracted, adjusted: done.adjusted })
            }
            "consolidate" => {
                let r: ConsolidateRequest = parse(&method, req.payload)?;
                let ws = self.workspace(&r.workspace)?;
                let deadline = Deadline::after_ms(req.budget.ms);
                let learner = Learner { bus: self.bus.as_ref(), db: &self.db, cfg: &self.cfg, writer: &self.writer };
                reply(consolidate::consolidate(&learner, r, &ws, &req.trace_id, deadline).await?)
            }
            "index" => {
                let r: IndexRequest = parse(&method, req.payload)?;
                let ws = self.workspace(&r.workspace)?;
                self.index(ws, r.paths).await
            }
            "map" => {
                let r: MapRequest = parse(&method, req.payload)?;
                let ws = self.workspace(&r.workspace)?;
                reply(self.blocking(move |db| project::map(db, &ws, &r)).await?)
            }
            "symbols" => {
                let r: SymbolsRequest = parse(&method, req.payload)?;
                let ws = self.workspace(&r.workspace)?;
                reply(self.blocking(move |db| project::symbols(db, &ws, &r)).await?)
            }
            other => Err(invalid(format!("unknown method memory.{other}"))),
        }
    }

    /// Re-index the files the `fs` service changed, in a workspace that
    /// already has a model; the others are indexed when first asked for.
    async fn files_changed(&self, payload: Value, trace: String) {
        let Ok(changed) = serde_json::from_value::<FilesChanged>(payload) else { return };
        let Ok(ws) = self.workspace(&changed.workspace) else { return };
        let Some(key) = ws.to_str().map(str::to_owned) else { return };
        let review_key = key.clone();
        if let Err(e) = self.blocking(move |db| review::refresh(db, &review_key, &trace, now_ms())).await {
            tracing::debug!(error = %e.message, "could not check note dependencies");
        }
        let modelled = self
            .blocking(move |db| {
                db.with(|conn| conn.query_row("SELECT 1 FROM projects WHERE root = ?1", [key], |_| Ok(())))
                    .map(|()| true)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(false),
                        e => Err(error::db(e)),
                    })
            })
            .await;
        if !matches!(modelled, Ok(true)) || changed.paths.is_empty() {
            return;
        }
        {
            let mut waiting = self.changes.lock().unwrap_or_else(PoisonError::into_inner);
            waiting.paths.entry(ws).or_default().extend(changed.paths);
            if waiting.draining {
                return;
            }
            waiting.draining = true;
        }
        // Should this handler be dropped part way, the next event drains.
        let mut draining = Draining { changes: &self.changes, done: false };
        loop {
            let batch = {
                let mut waiting = self.changes.lock().unwrap_or_else(PoisonError::into_inner);
                if waiting.paths.is_empty() {
                    // In the same turn of the lock, so no event is left waiting.
                    waiting.draining = false;
                    draining.done = true;
                    return;
                }
                std::mem::take(&mut waiting.paths)
            };
            for (ws, paths) in batch {
                if let Err(e) = self.index(ws, Some(paths.into_iter().collect())).await {
                    tracing::debug!(error = %e.message, "could not re-index changed files");
                }
            }
        }
    }
}

/// Marks the changes as no longer being drained if the drain stops before it is `done`.
struct Draining<'a> {
    changes: &'a std::sync::Mutex<Waiting>,
    done: bool,
}

impl Drop for Draining<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.changes.lock().unwrap_or_else(PoisonError::into_inner).draining = false;
        }
    }
}

/// Serve `memory.*` on `svc` until its link closes. When `svc` holds a
/// capability for `topic:fs.changed`, the files the `fs` service reports
/// changed are re-indexed as they change.
pub async fn serve(svc: Arc<Service>, memory: Arc<Memory>) {
    let topic: Target = format!("topic:{}", fs_api::CHANGED).parse().expect("a valid topic");
    if svc.cap_for(&topic).is_some() {
        if let Err(e) = svc.subscribe(fs_api::CHANGED).await {
            tracing::warn!(error = %e, "could not subscribe to changed files; the project model updates when indexed");
        }
    }
    let max_concurrent = memory.cfg.max_concurrent;
    svc.serve_concurrent(max_concurrent, move |req| {
        let memory = memory.clone();
        async move { memory.handle(req).await }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn from(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn settings_come_from_the_environment() {
        let d = from(&[]).unwrap();
        assert_eq!((d.model.as_str(), d.effort, d.max_tokens), ("sonnet", Effort::Low, 8_000));
        let c = from(&[
            ("MOLT_MEMORY_MODEL", "haiku"),
            ("MOLT_MEMORY_EFFORT", "medium"),
            ("MOLT_MEMORY_MAX_TOKENS", "2000"),
            ("MOLT_MEMORY_MODEL_TIMEOUT_S", "9"),
        ])
        .unwrap();
        assert_eq!((c.model.as_str(), c.effort, c.max_tokens), ("haiku", Effort::Medium, 2_000));
        assert_eq!(c.model_timeout, Duration::from_secs(9));
        assert_eq!(from(&[("MOLT_MEMORY_MODEL", " ")]).unwrap().model, "sonnet");
        assert!(from(&[("MOLT_MEMORY_EFFORT", "huge")]).is_err());
        assert!(from(&[("MOLT_MEMORY_MAX_TOKENS", "0")]).is_err());
        assert!(from(&[("MOLT_MEMORY_MODEL_TIMEOUT_S", "soon")]).is_err());
    }
}
