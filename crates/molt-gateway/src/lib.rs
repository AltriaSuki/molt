//! The model gateway: serves `model.complete` by calling the Anthropic
//! Messages API.
//!
//! It is the only process that holds the API key. It turns a
//! [`CompleteRequest`] into a Messages API request shaped for the chosen
//! model (thinking, effort, server-side fallbacks, prompt caching), retries
//! transient failures, and returns the content verbatim with usage and an
//! estimated cost.

mod api;
mod body;
mod profile;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context};
use molt_api::model::{CompleteRequest, CompleteResponse, Effort, Usage, STOP_REFUSAL};
use molt_proto::{Envelope, ErrorCode, RemoteError, Target};
use molt_sdk::Service;
use serde_json::Value;

use crate::profile::profile;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Kept back from a caller's deadline so the reply reaches it before the
/// kernel answers `timeout` in its place; at most a tenth of the deadline.
const REPLY_MARGIN: Duration = Duration::from_millis(500);

/// Gateway settings. [`Config::from_env`] documents the variables.
#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    /// Without a trailing slash, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    /// Model alias or id used when a request names none.
    pub default_model: String,
    /// `max_tokens` when a request sets none.
    pub max_tokens: u32,
    /// Effort when a request sets none; `None` uses each model's own default.
    pub effort: Option<Effort>,
    /// Per HTTP attempt; a caller's nearer deadline shortens it.
    pub timeout: Duration,
    /// Retries after the first attempt for 408, 409, 429, 5xx and network errors.
    pub max_retries: u32,
    /// Requests handled at once.
    pub max_in_flight: usize,
    /// Opt into server-side fallbacks on models that support them.
    pub fallbacks: bool,
    /// First retry delay when the API gives no `retry-after`; it doubles on
    /// each retry up to 30 s. One second outside tests.
    pub retry_base: Duration,
}

// Written out so that logging a config never prints the key.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("default_model", &self.default_model)
            .field("max_tokens", &self.max_tokens)
            .field("effort", &self.effort)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("max_in_flight", &self.max_in_flight)
            .field("fallbacks", &self.fallbacks)
            .field("retry_base", &self.retry_base)
            .finish()
    }
}

impl Config {
    /// Read settings from the environment:
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `ANTHROPIC_API_KEY` | required |
    /// | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
    /// | `MOLT_MODEL` | `opus` |
    /// | `MOLT_MAX_TOKENS` | `32000` |
    /// | `MOLT_EFFORT` | unset |
    /// | `MOLT_MODEL_TIMEOUT_S` | `1200` |
    /// | `MOLT_MODEL_RETRIES` | `4` |
    /// | `MOLT_MODEL_CONCURRENCY` | `16` |
    /// | `MOLT_FALLBACKS` | `1` (`0` turns them off) |
    ///
    /// An empty variable counts as unset. `MOLT_MAX_TOKENS`,
    /// `MOLT_MODEL_TIMEOUT_S` and `MOLT_MODEL_CONCURRENCY` must be above 0:
    /// a 0 there would fail every call, not lift the limit.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let parse = |name: &str, default: u64| -> anyhow::Result<u64> {
            get(name).map_or(Ok(default), |v| v.parse().with_context(|| format!("{name}={v:?} is not a number")))
        };
        let api_key = get("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
        let base_url = get("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let effort = match get("MOLT_EFFORT") {
            Some(v) => Some(Effort::from_str(&v).map_err(|e| anyhow!("MOLT_EFFORT: {e}"))?),
            None => None,
        };
        let fallbacks = match get("MOLT_FALLBACKS").as_deref() {
            None | Some("1" | "true" | "yes" | "on") => true,
            Some("0" | "false" | "no" | "off") => false,
            Some(other) => bail!("MOLT_FALLBACKS={other:?}: use 1 or 0"),
        };
        let positive = |name: &str, default: u64| -> anyhow::Result<u64> {
            let v = parse(name, default)?;
            ensure!(v > 0, "{name} must be above 0");
            Ok(v)
        };
        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            default_model: get("MOLT_MODEL").unwrap_or_else(|| "opus".into()),
            max_tokens: u32::try_from(positive("MOLT_MAX_TOKENS", 32_000)?).context("MOLT_MAX_TOKENS is too large")?,
            effort,
            timeout: Duration::from_secs(positive("MOLT_MODEL_TIMEOUT_S", 1200)?),
            max_retries: u32::try_from(parse("MOLT_MODEL_RETRIES", 4)?).context("MOLT_MODEL_RETRIES is too large")?,
            max_in_flight: usize::try_from(positive("MOLT_MODEL_CONCURRENCY", 16)?)
                .context("MOLT_MODEL_CONCURRENCY is too large")?,
            fallbacks,
            retry_base: Duration::from_secs(1),
        })
    }
}

/// The full model id for an alias (`opus`, `sonnet`, `haiku`); any other
/// string is returned unchanged.
pub fn resolve_model(name: &str) -> String {
    match name {
        "opus" => "claude-opus-5-5",
        "sonnet" => "claude-sonnet-5-5",
        "haiku" => "claude-haiku-4-5",
        other => other,
    }
    .to_owned()
}

/// Whether the gateway knows what `model` (an alias or a model id) costs.
/// Calls to a model it has no price for report no cost, so no spending
/// limit holds them back.
pub fn has_price(model: &str) -> bool {
    profile::profile(&resolve_model(model)).prices.is_some()
}

pub struct Gateway {
    cfg: Config,
    api: api::Api,
}

impl Gateway {
    pub fn new(cfg: Config) -> anyhow::Result<Self> {
        let api = api::Api::new(&cfg)?;
        Ok(Self { cfg, api })
    }

    /// One Messages API call, with retries.
    pub async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, RemoteError> {
        self.complete_within(req, None).await
    }

    /// [`Gateway::complete`] for a caller that waits at most `within`. Each
    /// attempt gets no more than the time left and no retry starts that could
    /// not finish in it; the last failure comes back instead, or `timeout`
    /// when the time ran out.
    pub async fn complete_within(
        &self,
        req: CompleteRequest,
        within: Option<Duration>,
    ) -> Result<CompleteResponse, RemoteError> {
        // A deadline too far off to represent is no deadline.
        let deadline = within.and_then(|d| Instant::now().checked_add(d));
        let named = req.model.as_deref().filter(|m| !m.trim().is_empty()).unwrap_or(&self.cfg.default_model);
        let model = resolve_model(named);
        let messages = req.messages.len();
        let call = body::build(&self.cfg, &model, req)?;
        tracing::debug!(
            %model,
            messages,
            max_tokens = call.max_tokens,
            fallbacks = call.fallbacks,
            "calling the Messages API"
        );

        let msg = self.api.create(&call.body, call.fallbacks, deadline).await?;
        let usage = Usage::from(msg.usage);
        // After a fallback another model answered, and its prices apply.
        let cost_usd = profile(&msg.model).prices.or(profile(&model).prices).map(|p| p.cost(&usage));
        if msg.stop_reason.as_deref() == Some(STOP_REFUSAL) {
            let category = msg.stop_details.as_ref().and_then(|d| d.get("category")).cloned().unwrap_or(Value::Null);
            tracing::info!(model = %msg.model, %category, "the model declined the request");
        }
        tracing::info!(
            requested = %model,
            model = %msg.model,
            stop_reason = msg.stop_reason.as_deref().unwrap_or(""),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_write_tokens = usage.cache_creation_input_tokens,
            cache_read_tokens = usage.cache_read_input_tokens,
            ?cost_usd,
            "model call"
        );
        Ok(CompleteResponse {
            id: msg.id,
            model: msg.model,
            content: msg.content,
            stop_reason: msg.stop_reason,
            stop_details: msg.stop_details,
            usage,
            cost_usd,
        })
    }
}

/// Serve `model.complete` on `svc` until its link closes. A request's
/// `budget.ms` is how long its caller waits (0: no deadline), and the call
/// stops in time to answer within it.
pub async fn serve(svc: Arc<Service>, gateway: Arc<Gateway>) {
    let max_in_flight = gateway.cfg.max_in_flight;
    svc.serve_concurrent(max_in_flight, move |req| {
        let gateway = gateway.clone();
        async move { handle(&gateway, req).await }
    })
    .await;
}

async fn handle(gateway: &Gateway, req: Envelope) -> Result<Value, RemoteError> {
    let method = match &req.to {
        Target::Method { method, .. } => method.as_str(),
        _ => "",
    };
    match method {
        "complete" => {
            let within = reply_window(req.budget.ms);
            let request: CompleteRequest = serde_json::from_value(req.payload).map_err(|e| RemoteError {
                code: ErrorCode::Invalid,
                message: format!("bad model.complete request: {e}"),
            })?;
            let response = gateway.complete_within(request, within).await?;
            serde_json::to_value(response)
                .map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("encoding the response: {e}") })
        }
        other => Err(RemoteError { code: ErrorCode::Invalid, message: format!("no method {other:?}") }),
    }
}

/// How long to work on a request whose caller waits `ms` (0: no deadline).
fn reply_window(ms: u64) -> Option<Duration> {
    let wait = Duration::from_millis(ms);
    (ms > 0).then(|| wait - REPLY_MARGIN.min(wait / 10))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use molt_api::model::user_text;
    use molt_proto::{Budget, CapId, TraceId};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn from(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn env_defaults() {
        let cfg = from(&[("ANTHROPIC_API_KEY", "sk-test")]).unwrap();
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.base_url, "https://api.anthropic.com");
        assert_eq!(cfg.default_model, "opus");
        assert_eq!(cfg.max_tokens, 32_000);
        assert_eq!(cfg.effort, None);
        assert_eq!(cfg.timeout, Duration::from_secs(1200));
        assert_eq!(cfg.max_retries, 4);
        assert_eq!(cfg.max_in_flight, 16);
        assert!(cfg.fallbacks);
        assert_eq!(cfg.retry_base, Duration::from_secs(1));
    }

    #[test]
    fn env_overrides() {
        let cfg = from(&[
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9/"),
            ("MOLT_MODEL", "haiku"),
            ("MOLT_MAX_TOKENS", "1000"),
            ("MOLT_EFFORT", "xhigh"),
            ("MOLT_MODEL_TIMEOUT_S", "5"),
            ("MOLT_MODEL_RETRIES", "0"),
            ("MOLT_MODEL_CONCURRENCY", "2"),
            ("MOLT_FALLBACKS", "0"),
        ])
        .unwrap();
        assert_eq!(cfg.base_url, "http://127.0.0.1:9");
        assert_eq!(cfg.default_model, "haiku");
        assert_eq!(cfg.max_tokens, 1000);
        assert_eq!(cfg.effort, Some(Effort::Xhigh));
        assert_eq!(cfg.timeout, Duration::from_secs(5));
        assert_eq!(cfg.max_retries, 0);
        assert_eq!(cfg.max_in_flight, 2);
        assert!(!cfg.fallbacks);
    }

    #[test]
    fn env_errors() {
        assert!(from(&[]).is_err());
        assert!(from(&[("ANTHROPIC_API_KEY", "  ")]).is_err());
        assert!(from(&[("ANTHROPIC_API_KEY", "k"), ("MOLT_EFFORT", "huge")]).is_err());
        assert!(from(&[("ANTHROPIC_API_KEY", "k"), ("MOLT_MAX_TOKENS", "lots")]).is_err());
        assert!(from(&[("ANTHROPIC_API_KEY", "k"), ("MOLT_FALLBACKS", "maybe")]).is_err());
        for name in ["MOLT_MAX_TOKENS", "MOLT_MODEL_TIMEOUT_S", "MOLT_MODEL_CONCURRENCY"] {
            let err = from(&[("ANTHROPIC_API_KEY", "k"), (name, "0")]).unwrap_err();
            assert!(err.to_string().contains(name), "{err}");
        }
        // Empty means unset.
        assert_eq!(from(&[("ANTHROPIC_API_KEY", "k"), ("MOLT_EFFORT", "")]).unwrap().effort, None);
    }

    #[test]
    fn debug_hides_the_key() {
        let cfg = from(&[("ANTHROPIC_API_KEY", "sk-secret")]).unwrap();
        assert!(!format!("{cfg:?}").contains("sk-secret"));
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(resolve_model("opus"), "claude-opus-5-5");
        assert_eq!(resolve_model("sonnet"), "claude-sonnet-5-5");
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5");
        assert_eq!(resolve_model("claude-opus-5"), "claude-opus-5");
        assert_eq!(resolve_model("Opus"), "Opus");
    }

    fn gateway() -> Gateway {
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k"), ("ANTHROPIC_BASE_URL", "http://127.0.0.1:1")]).unwrap();
        cfg.max_retries = 0;
        Gateway::new(cfg).unwrap()
    }

    fn envelope(to: &str, payload: Value) -> Envelope {
        Envelope::request(TraceId::random(), to.parse().unwrap(), CapId::random(), payload)
    }

    #[tokio::test]
    async fn unknown_methods_and_bad_payloads_are_invalid() {
        let gw = gateway();
        let err = handle(&gw, envelope("model.stream", json!({}))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains("no method \"stream\""), "{}", err.message);

        let err = handle(&gw, envelope("model.complete", json!({ "messages": "hi" }))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);

        let err = handle(&gw, envelope("model.complete", json!({ "messages": [] }))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn the_reply_window_keeps_a_margin_before_the_callers_deadline() {
        assert_eq!(reply_window(0), None);
        assert_eq!(reply_window(1), Some(Duration::from_micros(900)));
        assert_eq!(reply_window(2_000), Some(Duration::from_millis(1_800)));
        assert_eq!(reply_window(1_200_000), Some(Duration::from_millis(1_199_500)));
        assert!(reply_window(u64::MAX).is_some());
    }

    #[tokio::test]
    async fn the_callers_deadline_bounds_the_call() {
        let server = MockServer::start().await;
        let slow = ResponseTemplate::new(200).set_delay(Duration::from_secs(3)).set_body_json(json!({
            "id": "msg_01",
            "model": "claude-opus-5-5",
            "content": [],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }));
        Mock::given(method("POST")).and(path("/v1/messages")).respond_with(slow).mount(&server).await;
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k"), ("MOLT_MODEL_RETRIES", "2")]).unwrap();
        cfg.base_url = server.uri();
        let gw = Gateway::new(cfg).unwrap();

        let payload = serde_json::to_value(CompleteRequest { messages: vec![user_text("hi")], ..Default::default() });
        let req = envelope("model.complete", payload.unwrap()).with_budget(Budget::new(0, 300, 0));
        // Without the deadline the call would wait out the 3 s answer and succeed.
        let err = tokio::time::timeout(Duration::from_secs(2), handle(&gw, req)).await.unwrap().unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout, "{}", err.message);
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "no retry past the deadline");
    }

    #[test]
    fn a_bad_base_url_fails_construction() {
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        cfg.base_url = "not a url".into();
        assert!(Gateway::new(cfg).is_err());
    }
}
