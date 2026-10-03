//! The planner: serves `planner.run`.
//!
//! See [`molt_api::planner`] for what a run does. The planner talks to the
//! model and tools only through a [`Bus`], so its logic can be tested with
//! scripted fakes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use molt_api::planner::{RunRequest, RunResponse};
use molt_proto::{Budget, RemoteError, TraceId};
use molt_sdk::Service;
use serde_json::Value;

/// Planner settings. [`Config::from_env`] documents the variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// Model for runs that name none; `None` leaves the choice to the gateway.
    pub default_model: Option<String>,
    pub max_turns: u32,
    pub max_check_rounds: u32,
    pub budget_usd: f64,
    /// `max_tokens` for each model call.
    pub max_tokens: u32,
    /// Deadline for one model call.
    pub model_timeout: Duration,
    /// Deadline for one done-check run.
    pub check_timeout: Duration,
    /// Runs handled at once.
    pub max_concurrent_runs: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_model: None,
            max_turns: 50,
            max_check_rounds: 3,
            budget_usd: 10.0,
            max_tokens: 32_000,
            model_timeout: Duration::from_secs(1200),
            check_timeout: Duration::from_secs(900),
            max_concurrent_runs: 4,
        }
    }
}

impl Config {
    /// [`Config::default`] overridden by `MOLT_PLANNER_MODEL`,
    /// `MOLT_MAX_TURNS`, `MOLT_MAX_CHECK_ROUNDS`, `MOLT_BUDGET_USD`,
    /// `MOLT_MAX_TOKENS`, `MOLT_MODEL_TIMEOUT_S` and `MOLT_CHECK_TIMEOUT_S`.
    pub fn from_env() -> anyhow::Result<Self> {
        todo!()
    }
}

/// How the planner reaches other services.
#[async_trait]
pub trait Bus: Send + Sync + 'static {
    /// Call `target` (e.g. `fs.read`) and wait for the reply payload.
    async fn call(&self, target: &str, payload: Value, budget: Budget, trace: &TraceId) -> Result<Value, RemoteError>;
    /// Publish an event; failures are ignored.
    async fn publish(&self, topic: &str, payload: Value);
}

/// [`Bus`] over a real service link.
pub struct ServiceBus(pub Arc<Service>);

#[async_trait]
impl Bus for ServiceBus {
    async fn call(&self, target: &str, payload: Value, budget: Budget, trace: &TraceId) -> Result<Value, RemoteError> {
        let _ = (target, payload, budget, trace);
        todo!()
    }

    async fn publish(&self, topic: &str, payload: Value) {
        let _ = (topic, payload);
        todo!()
    }
}

/// Carry out one run. `trace` ties every call of the run together in the
/// audit log and names the run in progress events.
pub async fn run(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<RunResponse, RemoteError> {
    let _ = (bus, cfg, req, trace);
    todo!()
}

/// Serve `planner.run` on `svc` until its link closes.
pub async fn serve(svc: Arc<Service>, cfg: Config) {
    let _ = (svc, cfg);
    todo!()
}
