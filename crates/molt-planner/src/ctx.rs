//! What every part of one run shares: the bus, the limits, the spend so far,
//! the forks still alive, and typed helpers for the calls a run makes.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use molt_api::fs;
use molt_api::model::{self, CompleteRequest, CompleteResponse, Effort, Usage};
use molt_api::planner::RunRequest;
use molt_api::progress::{self, Progress};
use molt_proto::{Budget, ErrorCode, RemoteError, TraceId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;

use crate::{Bus, Config};

/// Deadline for reading, writing, listing and searching files.
pub(crate) const FILE_MS: u64 = 60_000;
/// Deadline for fork, diff, merge and drop, which copy or walk whole trees.
pub(crate) const TREE_MS: u64 = 600_000;
/// Added to a command's own timeout for the reply deadline, so the shell
/// service can report the timeout itself.
pub(crate) const SHELL_GRACE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Spend {
    pub usage: Usage,
    pub cost_usd: f64,
}

impl Spend {
    pub fn add(&mut self, resp: &CompleteResponse) {
        self.usage.add(&resp.usage);
        // Unknown prices count as free; the token totals still show the spend.
        self.cost_usd += resp.cost_usd.unwrap_or(0.0);
    }
}

/// The run's spend and its model calls still unanswered, kept together so a
/// reply is always counted in exactly one of them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tally {
    pub spend: Spend,
    pub pending: u32,
}

pub(crate) struct Ctx {
    bus: Arc<dyn Bus>,
    pub cfg: Arc<Config>,
    pub trace: TraceId,
    pub task: String,
    pub workspace: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub max_turns: u32,
    pub max_check_rounds: u32,
    pub budget_usd: f64,
    tally: watch::Sender<Tally>,
    /// Forks created and not yet dropped or merged, so none outlives the run by accident.
    forks: Mutex<Vec<String>>,
}

/// A conversation that only ever grows. `system` and `tools` stay the same
/// for its whole life: thinking blocks are bound to the exact prefix, and an
/// unchanged prefix keeps the prompt cache warm.
pub(crate) struct Conversation {
    system: &'static str,
    tools: Arc<Vec<Value>>,
    messages: Vec<Value>,
}

impl Conversation {
    pub fn new(system: &'static str, tools: Arc<Vec<Value>>, first: Value) -> Self {
        Self { system, tools, messages: vec![first] }
    }

    pub fn push(&mut self, message: Value) {
        self.messages.push(message);
    }
}

impl Ctx {
    /// Limits come from the request where it sets them and from `cfg` otherwise.
    pub fn new(bus: Arc<dyn Bus>, cfg: Arc<Config>, trace: TraceId, req: &RunRequest) -> Self {
        let model = req.model.clone().filter(|m| !m.trim().is_empty()).or_else(|| cfg.default_model.clone());
        Self {
            bus,
            trace,
            task: req.task.clone(),
            workspace: req.workspace.clone(),
            model,
            effort: req.effort,
            max_turns: req.max_turns.unwrap_or(cfg.max_turns),
            max_check_rounds: req.max_check_rounds.unwrap_or(cfg.max_check_rounds),
            budget_usd: req.budget_usd.unwrap_or(cfg.budget_usd),
            cfg,
            tally: watch::Sender::new(Tally::default()),
            forks: Mutex::default(),
        }
    }

    pub fn run_id(&self) -> String {
        self.trace.to_string()
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        target: &str,
        payload: impl Serialize,
        budget: Budget,
    ) -> Result<T, RemoteError> {
        let payload = serde_json::to_value(payload).map_err(|e| RemoteError {
            code: ErrorCode::Invalid,
            message: format!("encoding a {target} request: {e}"),
        })?;
        let reply = self.bus.call(target, payload, budget, &self.trace).await?;
        serde_json::from_value(reply)
            .map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("bad {target} reply: {e}") })
    }

    pub async fn progress(&self, event: Progress) {
        match serde_json::to_value(&event) {
            Ok(v) => self.bus.publish(progress::TOPIC, v).await,
            Err(e) => tracing::debug!(error = %e, "could not encode a progress event"),
        }
    }

    pub async fn note(&self, message: impl Into<String>) {
        self.progress(Progress::Note { run: self.run_id(), message: message.into() }).await;
    }

    /// Run totals over the designer and every attempt, and the model calls not answered yet.
    pub fn tally(&self) -> Tally {
        *self.tally.borrow()
    }

    pub fn over_budget(&self) -> bool {
        self.tally().spend.cost_usd >= self.budget_usd
    }

    /// [`Ctx::tally`] once every model call is answered, or after `limit`.
    pub async fn settle(&self, limit: Duration) -> Tally {
        let mut tally = self.tally.subscribe();
        // A timeout leaves the calls still pending in the tally.
        let _ = tokio::time::timeout(limit, tally.wait_for(|t| t.pending == 0)).await;
        self.tally()
    }

    /// One model call; its usage counts against the run. The call runs in its
    /// own task: the gateway bills it whether or not the caller still waits,
    /// so a caller that stops waiting leaves it pending until the reply lands.
    pub async fn complete(self: &Arc<Self>, conv: &Conversation) -> Result<CompleteResponse, RemoteError> {
        let req = CompleteRequest {
            model: self.model.clone(),
            system: Some(conv.system.to_owned()),
            messages: conv.messages.clone(),
            tools: conv.tools.as_ref().clone(),
            max_tokens: Some(self.cfg.max_tokens),
            effort: self.effort,
            output_schema: None,
        };
        let ms = u64::try_from(self.cfg.model_timeout.as_millis()).unwrap_or(u64::MAX);
        let budget = Budget::new(self.cfg.max_tokens.into(), ms, 0);
        self.tally.send_modify(|t| t.pending += 1);
        let ctx = self.clone();
        let call = tokio::spawn(async move {
            let resp = ctx.call::<CompleteResponse>(model::COMPLETE, req, budget).await;
            ctx.tally.send_modify(|t| {
                t.pending -= 1;
                if let Ok(resp) = &resp {
                    t.spend.add(resp);
                }
            });
            resp
        });
        call.await.unwrap_or_else(|e| {
            Err(RemoteError { code: ErrorCode::Failed, message: format!("the model call's task failed: {e}") })
        })
    }

    pub async fn fork(&self) -> Result<String, RemoteError> {
        let req = fs::ForkRequest { workspace: self.workspace.clone() };
        let resp: fs::ForkResponse = self.call(fs::FORK, req, Budget::new(0, TREE_MS, 0)).await?;
        self.forks.lock().unwrap_or_else(PoisonError::into_inner).push(resp.fork.clone());
        Ok(resp.fork)
    }

    /// Best effort: a fork that cannot be dropped is logged and left behind.
    pub async fn drop_fork(&self, fork: &str) {
        self.forget_fork(fork);
        let req = fs::DropRequest { fork: fork.to_owned() };
        if let Err(e) = self.call::<fs::DropResponse>(fs::DROP, req, Budget::new(0, TREE_MS, 0)).await {
            tracing::warn!(fork, error = %e, "could not drop a fork");
        }
    }

    /// Drop every fork still alive except `keep`.
    pub async fn drop_forks_except(&self, keep: Option<&str>) {
        let alive = self.forks.lock().unwrap_or_else(PoisonError::into_inner).clone();
        for fork in alive.iter().filter(|f| Some(f.as_str()) != keep) {
            self.drop_fork(fork).await;
        }
    }

    /// The fork is gone or handed to the caller; the run no longer owns it.
    pub fn forget_fork(&self, fork: &str) {
        self.forks.lock().unwrap_or_else(PoisonError::into_inner).retain(|f| f != fork);
    }

    pub async fn diff(&self, fork: &str) -> Result<fs::DiffResponse, RemoteError> {
        self.call(fs::DIFF, fs::DiffRequest { fork: fork.to_owned() }, Budget::new(0, TREE_MS, 0)).await
    }

    /// Merge a fork into the workspace, then drop it. The drop is a separate
    /// best-effort call, so a fork that cannot be deleted does not make a
    /// merge that succeeded look failed.
    pub async fn merge(&self, fork: &str) -> Result<fs::MergeResponse, RemoteError> {
        let req = fs::MergeRequest { fork: fork.to_owned(), drop: false };
        let resp = self.call(fs::MERGE, req, Budget::new(0, TREE_MS, 0)).await?;
        self.drop_fork(fork).await;
        Ok(resp)
    }

    pub async fn write(&self, workspace: &str, path: &str, content: &str) -> Result<fs::WriteResponse, RemoteError> {
        let req =
            fs::WriteRequest { workspace: workspace.to_owned(), path: path.to_owned(), content: content.to_owned() };
        self.call(fs::WRITE, req, Budget::new(0, FILE_MS, 0)).await
    }

    pub async fn read(
        &self,
        workspace: &str,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<fs::ReadResponse, RemoteError> {
        let req = fs::ReadRequest { workspace: workspace.to_owned(), path: path.to_owned(), offset, limit };
        self.call(fs::READ, req, Budget::new(0, FILE_MS, 0)).await
    }

    /// A whole file, paging through reads the service cuts short.
    pub async fn read_all(&self, workspace: &str, path: &str) -> Result<String, RemoteError> {
        let mut content = String::new();
        let mut offset = 1;
        loop {
            let page = self.read(workspace, path, Some(offset), None).await?;
            // More lines follow a cut page, so its last line ends in a newline unless the service cut it.
            if page.truncated && !page.content.is_empty() && !page.content.ends_with('\n') {
                let message = format!("{path} has a line too long to read whole");
                return Err(RemoteError { code: ErrorCode::Failed, message });
            }
            content.push_str(&page.content);
            if !page.truncated || page.lines == 0 {
                return Ok(content);
            }
            offset = page.first_line + page.lines;
        }
    }
}
