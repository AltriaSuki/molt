//! The model gateway: serves `model.complete` through explicit Anthropic
//! Messages and OpenAI Chat Completions adapters.
//!
//! It is the only process that holds provider credentials. It turns a
//! [`CompleteRequest`] into a Messages API request shaped for the chosen
//! model (thinking, effort, server-side fallbacks, prompt caching), retries
//! transient failures, and returns the content verbatim with usage and an
//! estimated cost.

mod api;
mod body;
pub mod catalog;
mod openai;
pub mod prices;
pub mod probe;
mod profile;
pub mod provider;
mod stream;

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context};
use molt_api::model::{CompleteRequest, CompleteResponse, Effort, Usage, STOP_REFUSAL};
use molt_api::progress::{self, ModelCall, Progress};
use molt_proto::{Envelope, ErrorCode, RemoteError, Target};
use molt_sdk::Service;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::prices::PriceTable;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Kept back from a caller's deadline so the reply reaches it before the
/// kernel answers `timeout` in its place; at most a tenth of the deadline.
const REPLY_MARGIN: Duration = Duration::from_millis(500);

/// Gateway settings. [`Config::from_env`] documents the variables.
#[derive(Clone)]
pub struct Config {
    pub api_key: String,
    /// Anthropic Bearer authentication, used only when the API key is absent.
    pub auth_token: Option<String>,
    /// Provider routing and the optional second provider, held only here.
    pub providers: provider::Providers,
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
    /// What each model costs; see [`prices`].
    pub prices: Arc<PriceTable>,
    /// What `molt models probe` found each model does on its endpoint; see [`probe`].
    pub probed: Arc<probe::Records>,
}

// Written out so that logging a config never prints credentials.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &"<redacted>")
            .field("auth_token", &"<redacted>")
            .field("providers", &self.providers)
            .field("base_url", &self.base_url)
            .field("default_model", &self.default_model)
            .field("max_tokens", &self.max_tokens)
            .field("effort", &self.effort)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("max_in_flight", &self.max_in_flight)
            .field("fallbacks", &self.fallbacks)
            .field("retry_base", &self.retry_base)
            .field("prices", &self.prices.sources())
            .field("probed", &self.probed.models.len())
            .finish()
    }
}

impl Config {
    /// Read settings from the environment:
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `ANTHROPIC_API_KEY` | preferred Anthropic credential |
    /// | `ANTHROPIC_AUTH_TOKEN` | Bearer fallback when the API key is unset |
    /// | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
    /// | `MOLT_MODEL` | `opus` |
    /// | `MOLT_MAX_TOKENS` | `32000` |
    /// | `MOLT_EFFORT` | unset |
    /// | `MOLT_MODEL_TIMEOUT_S` | `1200` |
    /// | `MOLT_MODEL_RETRIES` | `4` |
    /// | `MOLT_MODEL_CONCURRENCY` | `16` |
    /// | `MOLT_FALLBACKS` | `1` (`0` turns them off) |
    /// | `MOLT_MODEL_PRICES`, `MOLT_PRICES_FILE` | see [`prices`] |
    /// | `MOLT_MODELS_FILE` | see [`probe::records_path`] |
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
        let providers = provider::Providers::from_vars(&get)?;
        let api_key = get("ANTHROPIC_API_KEY").unwrap_or_default();
        let auth_token = get("ANTHROPIC_AUTH_TOKEN");
        ensure!(
            !api_key.is_empty() || auth_token.is_some() || providers.openai_key.is_some(),
            "ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY is not set"
        );
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
            providers,
            api_key,
            auth_token,
            base_url: base_url.trim_end_matches('/').to_owned(),
            default_model: default_model(&get),
            max_tokens: u32::try_from(positive("MOLT_MAX_TOKENS", 32_000)?).context("MOLT_MAX_TOKENS is too large")?,
            effort,
            timeout: Duration::from_secs(positive("MOLT_MODEL_TIMEOUT_S", 1200)?),
            max_retries: u32::try_from(parse("MOLT_MODEL_RETRIES", 4)?).context("MOLT_MODEL_RETRIES is too large")?,
            max_in_flight: usize::try_from(positive("MOLT_MODEL_CONCURRENCY", 16)?)
                .context("MOLT_MODEL_CONCURRENCY is too large")?,
            fallbacks,
            retry_base: Duration::from_secs(1),
            prices: Arc::new(PriceTable::from_vars(&var)?),
            probed: Arc::new(probe::Records::from_vars(&var)?),
        })
    }
}

/// The full model id for an alias (`opus`, `sonnet`, `haiku`); any other
/// string is returned unchanged.
/// `MOLT_MODEL`, else the default provider's own default.
fn default_model(get: &impl Fn(&str) -> Option<String>) -> String {
    get("MOLT_MODEL").unwrap_or_else(|| {
        if get("MOLT_PROVIDER").as_deref() == Some("openai") {
            "openai/gpt-4.1".into()
        } else {
            "opus".into()
        }
    })
}

/// The model id that a call naming `named`, or naming no model, goes to,
/// as a gateway started with the environment `var` resolves it: aliases
/// expanded and the provider prefix taken off. `None` when that environment
/// would not resolve it (a malformed alias table, an empty model id). This
/// lets an interface name the model before any call is made.
pub fn resolved_model(var: impl Fn(&str) -> Option<String>, named: Option<&str>) -> Option<String> {
    let get = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let providers = provider::Providers::from_vars(&get).ok()?;
    let named = named.map(str::trim).filter(|m| !m.is_empty()).map_or_else(|| default_model(&get), str::to_owned);
    providers.resolve(&named).ok().map(|(_, id)| id)
}

pub fn resolve_model(name: &str) -> String {
    match name {
        "opus" => "claude-opus-5-5",
        "sonnet" => "claude-sonnet-5-5",
        "haiku" => "claude-haiku-4-5",
        other => other,
    }
    .to_owned()
}

/// Whether `prices` has what `model` (an alias or a model id) costs.
/// Calls to a model with no price report no cost, so no spending limit
/// holds them back.
pub fn has_price(prices: &PriceTable, model: &str) -> bool {
    prices.lookup(&resolve_model(model)).is_some()
}

pub struct Gateway {
    cfg: Config,
    anthropic: Option<api::Api>,
    openai: Option<api::Api>,
}

impl Gateway {
    pub fn new(cfg: Config) -> anyhow::Result<Self> {
        if !cfg.api_key.is_empty() && cfg.auth_token.as_ref().is_some_and(|token| !token.is_empty()) {
            tracing::warn!(
                "both ANTHROPIC_API_KEY and ANTHROPIC_AUTH_TOKEN are set; using ANTHROPIC_API_KEY (x-api-key)"
            );
        }
        let anthropic = (!cfg.api_key.is_empty() || cfg.auth_token.as_ref().is_some_and(|token| !token.is_empty()))
            .then(|| api::Api::new(&cfg))
            .transpose()?;
        let openai = cfg.providers.openai_key.as_ref().map(|key| api::Api::openai(&cfg, key)).transpose()?;
        ensure!(
            anthropic.is_some() || openai.is_some(),
            "no model provider credential: set ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY"
        );
        Ok(Self { cfg, anthropic, openai })
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
        self.complete_observed(req, within, api::Observer::default()).await
    }

    /// Stream text previews to a bounded observer while keeping tool input,
    /// signatures, usage and the final message in the authoritative reply.
    /// The observer must not block; serving over the bus uses a bounded queue.
    pub async fn complete_streamed(
        &self,
        req: CompleteRequest,
        within: Option<Duration>,
        mut on_text: impl FnMut(String) + Send,
    ) -> Result<CompleteResponse, RemoteError> {
        if req.stream.is_none() {
            return Err(RemoteError { code: ErrorCode::Invalid, message: "stream context is required".into() });
        }
        self.complete_observed(req, within, api::Observer { text: Some(&mut on_text), retry: None }).await
    }

    async fn complete_observed(
        &self,
        req: CompleteRequest,
        within: Option<Duration>,
        observer: api::Observer<'_>,
    ) -> Result<CompleteResponse, RemoteError> {
        // A deadline too far off to represent is no deadline.
        let deadline = within.and_then(|d| Instant::now().checked_add(d));
        let named = req.model.as_deref().filter(|m| !m.trim().is_empty()).unwrap_or(&self.cfg.default_model).to_owned();
        let (provider, model) = self.cfg.providers.resolve(&named)?;
        let api = self.api(provider).ok_or_else(|| RemoteError {
            code: ErrorCode::Invalid,
            message: format!("no credential configured for {provider}"),
        })?;
        let messages = req.messages.len();
        let probed = self.cfg.probed.lookup(provider, self.base_url(provider), &model);
        let mut req = req;
        if req.stream.is_some() && probed.is_some_and(|p| p.streaming == Some(false)) {
            // The reply stays whole; only its previews are lost.
            tracing::debug!(%model, "not streaming: `molt models probe` found the endpoint does not stream");
            req.stream = None;
        }
        let call = match provider {
            provider::Provider::Anthropic => body::build(&self.cfg, &model, req, probed)?,
            provider::Provider::Openai => openai::build(&self.cfg, &model, req, probed)?,
        };
        tracing::debug!(
            %provider,
            %model,
            messages,
            max_tokens = call.max_tokens,
            fallbacks = call.fallbacks,
            "calling the Messages API"
        );

        let mut msg = api.create(&call.body, call.fallbacks, deadline, observer).await.map_err(|mut e| {
            if let Some(hint) = self.alias_hint(&named, &e) {
                e.message.push_str(&hint);
            }
            e
        })?;
        if !msg.usage.known() {
            return Err(RemoteError {
                code: ErrorCode::Failed,
                message: "model finished without token usage; usage and cost are unknown".into(),
            });
        }
        if call.schema_tool {
            probe::tool_as_answer(&mut msg.content, &mut msg.stop_reason);
        }
        let usage = Usage::from(msg.usage);
        // After a fallback or a router's choice another model answered, and its prices apply.
        let priced =
            self.cfg.prices.lookup(&msg.model).or_else(|| {
                (provider == provider::Provider::Anthropic).then(|| self.cfg.prices.lookup(&model)).flatten()
            });
        if priced.is_none() {
            let mut looked_for = prices::names(&msg.model);
            for name in prices::names(&model) {
                if !looked_for.contains(&name) {
                    looked_for.push(name);
                }
            }
            tracing::warn!(
                ?looked_for,
                "no price for the model: the call's cost is unknown; run `molt prices update` or set MOLT_MODEL_PRICES"
            );
        }
        let cost_usd = priced.as_ref().map(|(p, _)| p.cost(&usage));
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
            price_source = priced.as_ref().map(|(_, s)| s.source.as_str()),
            price_entry = priced.as_ref().map(|(_, s)| s.entry.as_str()),
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
            price_source: priced.map(|(_, s)| s),
        })
    }
}

impl Gateway {
    fn api(&self, provider: provider::Provider) -> Option<&api::Api> {
        match provider {
            provider::Provider::Anthropic => self.anthropic.as_ref(),
            provider::Provider::Openai => self.openai.as_ref(),
        }
    }

    /// For a refused call that names a model which is no alias but is one
    /// typo from one, such as `sonet`: which alias was likely meant.
    fn alias_hint(&self, named: &str, error: &RemoteError) -> Option<String> {
        if error.code != ErrorCode::Invalid || !error.message.to_ascii_lowercase().contains("model") {
            return None;
        }
        let bare = named.strip_prefix("anthropic/").unwrap_or(named);
        let aliases =
            ["opus", "sonnet", "haiku"].into_iter().chain(self.cfg.providers.aliases.keys().map(String::as_str));
        let best = closest_alias(bare, aliases)?;
        Some(format!("; {named:?} is not a model alias: did you mean {best:?}?"))
    }
}

/// The alias at most two edits from `name` (and closer than any other), when
/// `name` is not itself one.
fn closest_alias<'a>(name: &str, aliases: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for alias in aliases {
        if name == alias {
            return None;
        }
        let d = edit_distance(&lower, &alias.to_ascii_lowercase());
        if best.is_none_or(|(b, _)| d < b) {
            best = Some((d, alias));
        }
    }
    best.filter(|&(d, _)| d <= 2.min(lower.chars().count() / 2)).map(|(_, a)| a)
}

/// Levenshtein distance over characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let next = (diagonal + usize::from(ca != cb)).min(row[j] + 1).min(row[j + 1] + 1);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Serve `model.complete` on `svc` until its link closes. A request's
/// `budget.ms` is how long its caller waits (0: no deadline), and the call
/// stops in time to answer within it.
pub async fn serve(svc: Arc<Service>, gateway: Arc<Gateway>) {
    let max_in_flight = gateway.cfg.max_in_flight;
    let events = svc.clone();
    svc.serve_cancellable(max_in_flight, move |req, cancel| {
        let gateway = gateway.clone();
        let events = events.clone();
        async move { handle_observed(&gateway, req, Some(Events::Bus(events)), cancel).await }
    })
    .await;
}

#[cfg(test)]
async fn handle(gateway: &Gateway, req: Envelope) -> Result<Value, RemoteError> {
    handle_observed(gateway, req, None, CancellationToken::new()).await
}

/// Where a streamed call's progress goes: the bus when serving, a list in
/// tests.
#[derive(Clone)]
enum Events {
    Bus(Arc<Service>),
    #[cfg(test)]
    Collect(Arc<Mutex<Vec<Progress>>>),
    #[cfg(test)]
    CancelWhilePublishing {
        seen: Arc<Mutex<Vec<Progress>>>,
        cancel: CancellationToken,
        finish: bool,
    },
}

/// A streamed call that has published `ModelStarted` and not yet
/// `ModelFinished`, so a cancellation can close it.
struct Open {
    context: ModelCall,
    publisher: Option<Publisher>,
}

/// The preview publishing task, stopped when dropped so no preview follows
/// the call's `ModelFinished`.
struct Publisher(tokio::task::JoinHandle<()>);

impl Publisher {
    async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn handle_observed(
    gateway: &Gateway,
    req: Envelope,
    events: Option<Events>,
    cancel: CancellationToken,
) -> Result<Value, RemoteError> {
    let method = match &req.to {
        Target::Method { method, .. } => method.as_str(),
        _ => "",
    };
    match method {
        "complete" => {
            let started = Instant::now();
            let within = reply_window(req.budget.ms);
            let call_id = req.id.clone();
            if cancel.is_cancelled() {
                return Ok(cancelled_settlement(call_id, false));
            }
            let request: CompleteRequest = serde_json::from_value(req.payload).map_err(|e| RemoteError {
                code: ErrorCode::Invalid,
                message: format!("bad model.complete request: {e}"),
            })?;
            let open: Mutex<Option<Open>> = Mutex::new(None);
            let streamed = request.stream.clone().zip(events.clone());
            // Retries are announced for every call made over the bus, so a
            // long wait on a failing API is visible without the log.
            let attempt = request.stream.as_ref().and_then(|s| s.attempt);
            let role = request.role;
            let mut on_retry = events.clone().map(|events| {
                let (trace, call) = (req.trace_id.clone(), call_id.to_string());
                move |retry: api::Retry| {
                    let event = Progress::ModelRetry {
                        run: trace.to_string(),
                        attempt,
                        role,
                        call: call.clone(),
                        failure: retry.failure,
                        delay_ms: u64::try_from(retry.delay.as_millis()).unwrap_or(u64::MAX),
                        retry: retry.retry,
                        retries: retry.retries,
                    };
                    let (events, trace) = (events.clone(), trace.clone());
                    tokio::spawn(async move { publish(&events, &trace, event).await });
                }
            });
            let call = async {
                if let Some((context, events)) = streamed {
                    let call = ModelCall {
                        run: req.trace_id.to_string(),
                        attempt: context.attempt,
                        role,
                        turn: context.turn,
                        call: call_id.to_string(),
                        seq: 0,
                    };
                    // Register before the first publication can yield to cancellation.
                    *lock(&open) = Some(Open { context: call.clone(), publisher: None });
                    publish(&events, &req.trace_id, Progress::ModelStarted { context: call.clone() }).await;
                    // A slow event consumer cannot block the model response or
                    // build an unbounded queue. Missing previews produce seq gaps.
                    let (tx, mut rx) = mpsc::channel::<(u64, String)>(32);
                    let publisher = Publisher({
                        let events = events.clone();
                        let trace = req.trace_id.clone();
                        let context = call.clone();
                        tokio::spawn(async move {
                            while let Some((seq, text)) = rx.recv().await {
                                let mut context = context.clone();
                                context.seq = seq;
                                publish(&events, &trace, Progress::ModelText { context, text }).await;
                            }
                        })
                    });
                    *lock(&open) = Some(Open { context: call, publisher: Some(publisher) });
                    let mut on_text = |text| {
                        if let Some(open) = lock(&open).as_mut() {
                            open.context.seq += 1;
                            let _ = tx.try_send((open.context.seq, text));
                        }
                    };
                    let retry = on_retry.as_mut().map(|f| f as &mut (dyn FnMut(api::Retry) + Send));
                    let observer = api::Observer { text: Some(&mut on_text), retry };
                    let response = gateway
                        .complete_observed(
                            request,
                            within.map(|limit| limit.saturating_sub(started.elapsed())),
                            observer,
                        )
                        .await;
                    drop(tx);
                    response
                } else {
                    let retry = on_retry.as_mut().map(|f| f as &mut (dyn FnMut(api::Retry) + Send));
                    gateway.complete_observed(request, within, api::Observer { text: None, retry }).await
                }
            };
            let response = tokio::select! {
                biased;
                response = call => Some(response),
                _ = cancel.cancelled() => None,
            };
            // Settle progress outside the cancellable model future. Cancellation
            // must not drop a final publication after ownership has been taken.
            let unfinished = lock(&open).take();
            if let (Some(Open { mut context, publisher }), Some(events)) = (unfinished, events) {
                if let Some(mut publisher) = publisher {
                    let drain =
                        within.map_or(Duration::from_millis(400), |limit| limit.saturating_sub(started.elapsed()));
                    if response.is_none() || tokio::time::timeout(drain, &mut publisher.0).await.is_err() {
                        publisher.stop().await;
                    }
                }
                context.seq += 1;
                let (usage, cost_usd, error) = match &response {
                    Some(Ok(resp)) => (
                        Some(resp.usage),
                        resp.cost_usd,
                        resp.is_refusal().then(|| "the model declined; preview is not a result".into()),
                    ),
                    Some(Err(e)) => (None, None, Some(e.to_string())),
                    None => (None, None, Some("cancelled".into())),
                };
                publish(&events, &req.trace_id, Progress::ModelFinished { context, usage, cost_usd, error }).await;
            }
            let Some(response) = response else {
                return Ok(cancelled_settlement(call_id, true));
            };
            serde_json::to_value(response?)
                .map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("encoding the response: {e}") })
        }
        other => Err(RemoteError { code: ErrorCode::Invalid, message: format!("no method {other:?}") }),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cancelled_settlement(call: molt_proto::MsgId, may_have_started: bool) -> Value {
    // Anthropic Messages has no cancellation acknowledgement. Dropping HTTP
    // stops local work only; never invent zero usage for a started request.
    json!({
        "error": {"code": "cancelled", "message": "local request stopped; remote cancellation and billing remain unknown"},
        "settlement": {"request_id": call, "local_stopped": true,
            "remote_cancel_confirmed": null, "may_have_started": may_have_started,
            "usage": null, "cost_usd": null}
    })
}

async fn publish(events: &Events, trace: &molt_proto::TraceId, event: Progress) {
    match events {
        Events::Bus(service) => {
            if let Ok(payload) = serde_json::to_value(event) {
                // Progress is best effort, even when the transport's outgoing queue
                // is full. This bound also keeps draining 32 previews within the
                // gateway's normal reply margin.
                let _ = tokio::time::timeout(
                    Duration::from_millis(10),
                    service.publish_traced(progress::TOPIC, payload, trace.clone()),
                )
                .await;
            }
        }
        #[cfg(test)]
        Events::Collect(seen) => lock(seen).push(event),
        #[cfg(test)]
        Events::CancelWhilePublishing { seen, cancel, finish } => {
            if *finish && matches!(event, Progress::ModelFinished { .. }) {
                cancel.cancel();
                tokio::task::yield_now().await;
            }
            let started = matches!(event, Progress::ModelStarted { .. });
            lock(seen).push(event);
            if !*finish && started {
                cancel.cancel();
                tokio::task::yield_now().await;
            }
        }
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
    fn auth_token_is_a_credential_and_empty_variables_are_unset() {
        let cfg = from(&[("ANTHROPIC_AUTH_TOKEN", " token-test "), ("ANTHROPIC_API_KEY", "  ")]).unwrap();
        assert!(cfg.api_key.is_empty());
        assert_eq!(cfg.auth_token.as_deref(), Some("token-test"));
        let error = from(&[("ANTHROPIC_AUTH_TOKEN", "  ")]).unwrap_err().to_string();
        for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY"] {
            assert!(error.contains(name), "{error}");
        }
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
        let cfg = from(&[("ANTHROPIC_API_KEY", "sk-secret"), ("ANTHROPIC_AUTH_TOKEN", "token-secret")]).unwrap();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("sk-secret") && !debug.contains("token-secret"));
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(resolve_model("opus"), "claude-opus-5-5");
        assert_eq!(resolve_model("sonnet"), "claude-sonnet-5-5");
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5");
        assert_eq!(resolve_model("claude-opus-5"), "claude-opus-5");
        assert_eq!(resolve_model("Opus"), "Opus");
    }

    #[test]
    fn the_default_model_is_named_as_a_call_without_one_would_resolve_it() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_owned())
        };
        assert_eq!(resolved_model(env(&[]), None).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(resolved_model(env(&[("MOLT_MODEL", "grok-4.5")]), None).as_deref(), Some("grok-4.5"));
        assert_eq!(resolved_model(env(&[("MOLT_MODEL", " ")]), Some("haiku")).as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(resolved_model(env(&[("MOLT_PROVIDER", "openai")]), None).as_deref(), Some("gpt-4.1"));
        let aliased = env(&[("MOLT_MODEL", "fast"), ("MOLT_MODEL_ALIASES", r#"{"fast":"openai/gpt-4.1-mini"}"#)]);
        assert_eq!(resolved_model(aliased, None).as_deref(), Some("gpt-4.1-mini"));
        assert_eq!(resolved_model(env(&[("MOLT_MODEL_ALIASES", "not json")]), None), None);
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
    #[tokio::test]
    async fn cancellation_stops_local_http_without_inventing_usage_or_retrying() {
        for (provider, endpoint) in [("anthropic", "/v1/messages"), ("openai", "/v1/chat/completions")] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
                .mount(&server)
                .await;
            let mut cfg = from(&[
                ("ANTHROPIC_API_KEY", "k"),
                ("OPENAI_API_KEY", "o"),
                ("MOLT_PROVIDER", provider),
                ("MOLT_MODEL_RETRIES", "4"),
            ])
            .unwrap();
            cfg.base_url = server.uri();
            cfg.providers.openai_base_url = server.uri();
            let gateway = Gateway::new(cfg).unwrap();
            let request = envelope(
                "model.complete",
                serde_json::to_value(CompleteRequest { messages: vec![user_text("hi")], ..Default::default() })
                    .unwrap(),
            );
            let id = request.id.clone();
            let cancel = CancellationToken::new();
            let work = handle_observed(&gateway, request, None, cancel.clone());
            tokio::pin!(work);
            tokio::select! {
                result = &mut work => panic!("HTTP finished early: {result:?}"),
                _ = async { while server.received_requests().await.unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }} => {}
            }
            cancel.cancel();
            let result = tokio::time::timeout(Duration::from_secs(1), work).await.unwrap().unwrap();
            assert_eq!(result["error"]["code"], "cancelled");
            assert_eq!(result["settlement"]["request_id"], json!(id));
            assert_eq!(result["settlement"]["local_stopped"], true);
            assert!(result["settlement"]["remote_cancel_confirmed"].is_null());
            assert!(result["settlement"]["usage"].is_null());
            assert!(result["settlement"]["cost_usd"].is_null());
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn a_streamed_call_cancelled_midway_still_finishes_with_unknown_usage() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let sse = |value: Value| format!("event: {}\ndata: {value}\n\n", value["type"].as_str().unwrap());
        let first =
            sse(json!({"type":"message_start","message":{
                "id":"msg", "model":"claude-opus-5-5", "role":"assistant", "type":"message", "content":[],
                "usage":{"input_tokens":10,"output_tokens":1}
            }})) + &sse(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}))
                + &sse(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut chunk = [0; 2048];
                let n = socket.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..n]);
            }
            let header =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n";
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(first.as_bytes()).await.unwrap();
            // Never finish: only the cancellation ends the call.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        cfg.base_url = url;
        let gateway = Gateway::new(cfg).unwrap();
        let request = CompleteRequest {
            messages: vec![user_text("hi")],
            stream: Some(molt_api::model::StreamContext { attempt: Some(2), turn: 3 }),
            ..Default::default()
        };
        let request = envelope("model.complete", serde_json::to_value(request).unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let work = handle_observed(&gateway, request, Some(Events::Collect(seen.clone())), cancel.clone());
        tokio::pin!(work);
        let has_text = || lock(&seen).iter().any(|e| matches!(e, Progress::ModelText { .. }));
        tokio::select! {
            result = &mut work => panic!("the call finished early: {result:?}"),
            _ = async { while !has_text() { tokio::time::sleep(Duration::from_millis(5)).await } } => {}
        }
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), work).await.unwrap().unwrap();
        assert_eq!(result["error"]["code"], "cancelled");
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.abort();

        let seen = lock(&seen).clone();
        assert!(matches!(seen.first(), Some(Progress::ModelStarted { .. })), "{seen:?}");
        let Some(Progress::ModelFinished { context, usage, cost_usd, error }) = seen.last() else {
            panic!("the events do not end with ModelFinished: {seen:?}");
        };
        assert_eq!((context.attempt, context.turn), (Some(2), 3));
        assert_eq!((usage, cost_usd, error.as_deref()), (&None, &None, Some("cancelled")));
        let finished = seen.iter().filter(|e| matches!(e, Progress::ModelFinished { .. })).count();
        assert_eq!(finished, 1, "{seen:?}");
        let seqs: Vec<u64> = seen.iter().filter_map(|e| e.model_call().map(|c| c.seq)).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq goes up: {seqs:?}");
    }

    #[tokio::test]
    async fn a_streamed_call_that_completes_reports_its_usage_once() {
        let server = MockServer::start().await;
        let body = [
            json!({"type":"message_start","message":{"id":"msg","model":"claude-opus-5-5","role":"assistant",
                "type":"message","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ]
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        cfg.base_url = server.uri();
        let gateway = Gateway::new(cfg).unwrap();
        let request = CompleteRequest {
            messages: vec![user_text("hi")],
            stream: Some(molt_api::model::StreamContext { attempt: None, turn: 1 }),
            role: Some(molt_api::model::Role::Reviewer),
            ..Default::default()
        };
        let request = envelope("model.complete", serde_json::to_value(request).unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let result = handle_observed(&gateway, request, Some(Events::Collect(seen.clone())), CancellationToken::new())
            .await
            .unwrap();
        assert!(result.get("error").is_none(), "{result}");
        let seen = lock(&seen).clone();
        // Every event names the caller's role, so a preview says it is the reviewer's.
        assert!(seen.iter().all(|e| e.speaker().as_deref() == Some("reviewer")), "{seen:?}");
        let kinds: Vec<&str> = seen
            .iter()
            .map(|e| match e {
                Progress::ModelStarted { .. } => "started",
                Progress::ModelText { .. } => "text",
                Progress::ModelFinished { .. } => "finished",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["started", "text", "finished"]);
        let Some(Progress::ModelFinished { usage, error, .. }) = seen.last() else { unreachable!() };
        assert_eq!((usage.map(|u| u.output_tokens), error), (Some(4), &None));
    }

    #[tokio::test]
    async fn cancellation_while_publishing_started_or_finished_still_closes_once() {
        let server = MockServer::start().await;
        let body = [
            json!({"type":"message_start","message":{"id":"msg","model":"claude-opus-5-5","role":"assistant",
                "type":"message","content":[],"usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ]
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        cfg.base_url = server.uri();
        let gateway = Gateway::new(cfg).unwrap();
        let request = CompleteRequest {
            messages: vec![user_text("hi")],
            stream: Some(molt_api::model::StreamContext { attempt: Some(0), turn: 1 }),
            ..Default::default()
        };
        let request = envelope("model.complete", serde_json::to_value(request).unwrap());
        for finish in [false, true] {
            let request = request.clone();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let cancel = CancellationToken::new();
            let result = handle_observed(
                &gateway,
                request,
                Some(Events::CancelWhilePublishing { seen: seen.clone(), cancel: cancel.clone(), finish }),
                cancel,
            )
            .await
            .unwrap();
            if finish {
                assert!(result.get("error").is_none(), "{result}");
            } else {
                assert_eq!(result["error"]["code"], "cancelled");
            }
            let seen = lock(&seen);
            assert!(matches!(seen.first(), Some(Progress::ModelStarted { .. })));
            assert!(matches!(seen.last(), Some(Progress::ModelFinished { .. })), "{seen:?}");
            assert_eq!(seen.iter().filter(|e| matches!(e, Progress::ModelFinished { .. })).count(), 1);
        }
    }

    #[test]
    fn a_name_one_or_two_edits_from_an_alias_suggests_it() {
        let aliases = || ["opus", "sonnet", "haiku", "fast"].into_iter();
        for (name, best) in [
            ("sonet", Some("sonnet")),
            ("Sonnet", Some("sonnet")),
            ("opsu", Some("opus")),
            ("haiko", Some("haiku")),
            ("sonnet", None),
            ("fast", None),
            ("grok-4.5", None),
            ("claude-sonnet-5-5", None),
            ("op", None),
        ] {
            assert_eq!(closest_alias(name, aliases()), best, "{name}");
        }
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[tokio::test]
    async fn each_retry_is_announced_before_its_wait_with_its_number() {
        let server = MockServer::start().await;
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        cfg.base_url = server.uri();
        let gateway = Gateway::new(cfg).unwrap();
        let sse = [
            json!({"type":"message_start","message":{"id":"msg","model":"claude-opus-5-5","role":"assistant",
                "type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ]
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect::<String>();
        let plain = json!({
            "id": "msg", "model": "claude-opus-5-5", "content": [],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        // Announced with or without previews, with the attempt when the caller says.
        let attempt_stream = Some(molt_api::model::StreamContext { attempt: Some(1), turn: 2 });
        use molt_api::model::Role;
        for (stream, role) in [(None, Some(Role::Judge)), (attempt_stream, Some(Role::Attempt)), (None, None)] {
            server.reset().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(502).insert_header("retry-after", "0"))
                .up_to_n_times(2)
                .mount(&server)
                .await;
            let answer = match stream {
                Some(_) => ResponseTemplate::new(200).set_body_raw(sse.clone(), "text/event-stream"),
                None => ResponseTemplate::new(200).set_body_json(plain.clone()),
            };
            Mock::given(method("POST")).and(path("/v1/messages")).respond_with(answer).mount(&server).await;
            let attempt = stream.as_ref().and_then(|s| s.attempt);
            let request = CompleteRequest { messages: vec![user_text("hi")], stream, role, ..Default::default() };
            let request = envelope("model.complete", serde_json::to_value(request).unwrap());
            let (run, call) = (request.trace_id.to_string(), request.id.to_string());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let events = Some(Events::Collect(seen.clone()));
            let result = handle_observed(&gateway, request, events, CancellationToken::new()).await.unwrap();
            assert!(result.get("error").is_none(), "{result}");
            tokio::task::yield_now().await;
            let retries: Vec<Progress> =
                lock(&seen).iter().filter(|e| matches!(e, Progress::ModelRetry { .. })).cloned().collect();
            let expected = |retry| Progress::ModelRetry {
                run: run.clone(),
                attempt,
                role,
                call: call.clone(),
                failure: "model API 502".into(),
                delay_ms: 0,
                retry,
                retries: 4,
            };
            assert_eq!(retries, [expected(1), expected(2)]);
        }
    }

    #[tokio::test]
    async fn missing_usage_is_unknown_and_is_not_priced_as_zero() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg", "model": "claude-opus-5-5", "content": []
            })))
            .mount(&server)
            .await;
        let mut cfg = from(&[("ANTHROPIC_API_KEY", "k")]).unwrap();
        cfg.base_url = server.uri();
        let gateway = Gateway::new(cfg).unwrap();
        let error = gateway
            .complete(CompleteRequest { messages: vec![user_text("hi")], ..Default::default() })
            .await
            .unwrap_err();
        assert!(error.message.contains("usage and cost are unknown"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
