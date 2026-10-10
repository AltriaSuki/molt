//! The planner: serves `planner.run`.
//!
//! See [`molt_api::planner`] for what a run does. The planner talks to the
//! model and tools only through a [`Bus`], so its logic can be tested with
//! scripted fakes.

mod agent;
mod attempt;
mod ctx;
mod designer;
mod memory;
mod prompts;
mod runner;
mod tools;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context};
use async_trait::async_trait;
use molt_api::planner::{DesignResponse, RunRequest, RunResponse};
use molt_proto::{Budget, Envelope, ErrorCode, RemoteError, Target, TraceId};
use molt_sdk::{CallOpts, SdkError, Service};
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
    /// Size of the project map at the start of a run, in estimated tokens.
    pub map_tokens: u32,
    /// How long a finished run waits, while it applies the winner, for the
    /// replies to model calls that cancelled attempts left running, so their
    /// cost is counted. Calls still unanswered are reported as uncounted.
    pub late_reply_wait: Duration,
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
            map_tokens: 3_000,
            late_reply_wait: Duration::from_secs(2),
        }
    }
}

impl Config {
    /// [`Config::default`] overridden by `MOLT_PLANNER_MODEL`,
    /// `MOLT_MAX_TURNS`, `MOLT_MAX_CHECK_ROUNDS`, `MOLT_BUDGET_USD`,
    /// `MOLT_MAX_TOKENS`, `MOLT_MODEL_TIMEOUT_S`, `MOLT_CHECK_TIMEOUT_S` and
    /// `MOLT_MAP_TOKENS`.
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `MOLT_PLANNER_MODEL` | unset (the gateway's default) |
    /// | `MOLT_MAX_TURNS` | `50` |
    /// | `MOLT_MAX_CHECK_ROUNDS` | `3` |
    /// | `MOLT_BUDGET_USD` | `10` |
    /// | `MOLT_MAX_TOKENS` | `32000` |
    /// | `MOLT_MODEL_TIMEOUT_S` | `1200` |
    /// | `MOLT_CHECK_TIMEOUT_S` | `900` |
    /// | `MOLT_MAP_TOKENS` | `3000` (at most 32000) |
    ///
    /// An empty variable counts as unset. A value that does not parse is an
    /// error, and so is a zero: none of these limits has a "no limit" value.
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
        let small = |name: &str, default: u32| -> anyhow::Result<u32> {
            u32::try_from(number(name, default.into())?).with_context(|| format!("{name} is too large"))
        };
        let d = Self::default();
        let budget_usd = match get("MOLT_BUDGET_USD") {
            Some(v) => {
                let usd: f64 = v.parse().with_context(|| format!("MOLT_BUDGET_USD={v:?} is not a number"))?;
                ensure!(usd.is_finite() && usd > 0.0, "MOLT_BUDGET_USD={v:?} must be a positive amount");
                usd
            }
            None => d.budget_usd,
        };
        Ok(Self {
            default_model: get("MOLT_PLANNER_MODEL"),
            max_turns: small("MOLT_MAX_TURNS", d.max_turns)?,
            max_check_rounds: small("MOLT_MAX_CHECK_ROUNDS", d.max_check_rounds)?,
            budget_usd,
            max_tokens: small("MOLT_MAX_TOKENS", d.max_tokens)?,
            model_timeout: Duration::from_secs(number("MOLT_MODEL_TIMEOUT_S", d.model_timeout.as_secs())?),
            check_timeout: Duration::from_secs(number("MOLT_CHECK_TIMEOUT_S", d.check_timeout.as_secs())?),
            map_tokens: small("MOLT_MAP_TOKENS", d.map_tokens)?.min(32_000),
            max_concurrent_runs: d.max_concurrent_runs,
            late_reply_wait: d.late_reply_wait,
        })
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
        let opts = CallOpts { cap: None, budget, trace: Some(trace.clone()) };
        self.0.call(target, payload, opts).await.map_err(|e| match e {
            SdkError::Remote(e) => e,
            other => RemoteError { code: ErrorCode::Unavailable, message: format!("calling {target}: {other}") },
        })
    }

    async fn publish(&self, topic: &str, payload: Value) {
        if let Err(e) = self.0.publish(topic, payload).await {
            tracing::debug!(topic, error = %e, "could not publish an event");
        }
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
    runner::run(bus, cfg, req, trace).await
}

/// Design the done-check for a run without starting it (`planner.design`).
pub async fn design(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<DesignResponse, RemoteError> {
    runner::design(bus, cfg, req, trace).await
}

/// Serve `planner.run` and `planner.design` on `svc` until its link closes.
pub async fn serve(svc: Arc<Service>, cfg: Config) {
    let max_in_flight = cfg.max_concurrent_runs;
    let cfg = Arc::new(cfg);
    let bus: Arc<dyn Bus> = Arc::new(ServiceBus(svc.clone()));
    svc.serve_concurrent(max_in_flight, move |req| {
        let (bus, cfg) = (bus.clone(), cfg.clone());
        async move { handle(bus, cfg, req).await }
    })
    .await;
}

async fn handle(bus: Arc<dyn Bus>, cfg: Arc<Config>, req: Envelope) -> Result<Value, RemoteError> {
    let method = match &req.to {
        Target::Method { method, .. } => method.as_str(),
        _ => "",
    };
    let request = |payload: Value| -> Result<RunRequest, RemoteError> {
        serde_json::from_value(payload).map_err(|e| RemoteError {
            code: ErrorCode::Invalid,
            message: format!("bad planner.{method} request: {e}"),
        })
    };
    let encode = |response: Result<Value, serde_json::Error>| {
        response.map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("encoding the response: {e}") })
    };
    match method {
        "run" => {
            let response = run(bus, cfg, request(req.payload)?, req.trace_id).await?;
            encode(serde_json::to_value(response))
        }
        "design" => {
            let response = design(bus, cfg, request(req.payload)?, req.trace_id).await?;
            encode(serde_json::to_value(response))
        }
        other => Err(RemoteError { code: ErrorCode::Invalid, message: format!("no method {other:?}") }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use molt_proto::CapId;
    use serde_json::json;

    use super::*;

    fn from(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn env_defaults() {
        let cfg = from(&[]).unwrap();
        let d = Config::default();
        assert_eq!(cfg.default_model, None);
        assert_eq!(cfg.max_turns, d.max_turns);
        assert_eq!(cfg.max_check_rounds, d.max_check_rounds);
        assert_eq!(cfg.budget_usd, d.budget_usd);
        assert_eq!(cfg.max_tokens, d.max_tokens);
        assert_eq!(cfg.model_timeout, d.model_timeout);
        assert_eq!(cfg.check_timeout, d.check_timeout);
        assert_eq!(cfg.max_concurrent_runs, d.max_concurrent_runs);
        assert_eq!(cfg.map_tokens, 3_000);
    }

    #[test]
    fn env_overrides() {
        let cfg = from(&[
            ("MOLT_PLANNER_MODEL", "sonnet"),
            ("MOLT_MAX_TURNS", "7"),
            ("MOLT_MAX_CHECK_ROUNDS", "4"),
            ("MOLT_BUDGET_USD", "2.5"),
            ("MOLT_MAX_TOKENS", "1000"),
            ("MOLT_MODEL_TIMEOUT_S", "5"),
            ("MOLT_CHECK_TIMEOUT_S", "6"),
            ("MOLT_MAP_TOKENS", "99999"),
        ])
        .unwrap();
        assert_eq!(cfg.map_tokens, 32_000);
        assert_eq!(cfg.default_model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.max_turns, 7);
        assert_eq!(cfg.max_check_rounds, 4);
        assert_eq!(cfg.budget_usd, 2.5);
        assert_eq!(cfg.max_tokens, 1000);
        assert_eq!(cfg.model_timeout, Duration::from_secs(5));
        assert_eq!(cfg.check_timeout, Duration::from_secs(6));
    }

    #[test]
    fn env_errors() {
        assert!(from(&[("MOLT_MAX_TURNS", "many")]).is_err());
        // Zero is never "no limit": it would fail every run.
        for name in [
            "MOLT_MAX_TURNS",
            "MOLT_MAX_CHECK_ROUNDS",
            "MOLT_MAX_TOKENS",
            "MOLT_MODEL_TIMEOUT_S",
            "MOLT_CHECK_TIMEOUT_S",
            "MOLT_MAP_TOKENS",
        ] {
            let err = from(&[(name, "0")]).unwrap_err().to_string();
            assert!(err.contains(name), "{err}");
        }
        assert!(from(&[("MOLT_MAX_TOKENS", "99999999999")]).is_err());
        assert!(from(&[("MOLT_BUDGET_USD", "cheap")]).is_err());
        assert!(from(&[("MOLT_BUDGET_USD", "-1")]).is_err());
        assert!(from(&[("MOLT_BUDGET_USD", "NaN")]).is_err());
        assert!(from(&[("MOLT_CHECK_TIMEOUT_S", "1.5")]).is_err());
        // Empty means unset.
        assert_eq!(from(&[("MOLT_PLANNER_MODEL", " ")]).unwrap().default_model, None);
    }

    struct NoBus;

    #[async_trait]
    impl Bus for NoBus {
        async fn call(&self, target: &str, _: Value, _: Budget, _: &TraceId) -> Result<Value, RemoteError> {
            Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {target}") })
        }

        async fn publish(&self, _: &str, _: Value) {}
    }

    fn envelope(to: &str, payload: Value) -> Envelope {
        Envelope::request(TraceId::random(), to.parse().unwrap(), CapId::random(), payload)
    }

    #[tokio::test]
    async fn unknown_methods_and_bad_payloads_are_invalid() {
        let (bus, cfg): (Arc<dyn Bus>, _) = (Arc::new(NoBus), Arc::new(Config::default()));
        let err = handle(bus.clone(), cfg.clone(), envelope("planner.plan", json!({}))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains("no method \"plan\""), "{}", err.message);

        let err = handle(bus.clone(), cfg.clone(), envelope("planner.run", json!({ "task": 1 }))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);

        let bad = json!({ "task": "t", "workspace": "/w", "attempts": 9 });
        let err = handle(bus.clone(), cfg.clone(), envelope("planner.run", bad)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);

        // planner.design designs: a check given, or verify off, leaves it nothing to do.
        for given in [json!({ "check": "true" }), json!({ "verify": false })] {
            let mut payload = json!({ "task": "t", "workspace": "/w" });
            payload.as_object_mut().unwrap().extend(given.as_object().unwrap().clone());
            let err = handle(bus.clone(), cfg.clone(), envelope("planner.design", payload)).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{}", err.message);
        }

        // A valid request reaches the bus; the fork fails because nothing serves `fs`.
        let ok = json!({ "task": "t", "workspace": "/w" });
        let err = handle(bus, cfg, envelope("planner.run", ok)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
    }
}
