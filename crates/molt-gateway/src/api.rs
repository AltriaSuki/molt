//! The HTTP side: one `POST /v1/messages` with retries, and the mapping of
//! its failures onto bus error codes.

use std::error::Error as _;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::Context;
use molt_api::model::Usage;
use molt_proto::{ErrorCode, RemoteError};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{Auth, Endpoint, EndpointProtocol, Provider};
use crate::stream::{Accumulator, Decoder};
use crate::Config;

const API_VERSION: &str = "2023-06-01";
/// The beta for `fallbacks: "default"`. The array form needs a different
/// header, and pairing either header with the other form is a 400.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The least time a retry needs before the caller's deadline; a Messages API
/// call rarely answers sooner, and one cut short is wasted.
const MIN_ATTEMPT: Duration = Duration::from_secs(1);
/// How much of an unparseable error body goes into the error message.
const DETAIL_CHARS: usize = 300;

/// The parts of a Messages API response the gateway returns.
#[derive(Debug, Deserialize)]
pub(crate) struct Message {
    pub id: String,
    pub model: String,
    pub content: Vec<Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<Value>,
    #[serde(default)]
    pub usage: ApiUsage,
}

/// Usage as the API sends it: the cache counts may be `null`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ApiUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl ApiUsage {
    pub fn known(&self) -> bool {
        self.input_tokens.is_some() && self.output_tokens.is_some()
    }
}

impl From<ApiUsage> for Usage {
    fn from(u: ApiUsage) -> Self {
        Usage {
            input_tokens: u.input_tokens.unwrap_or(0),
            output_tokens: u.output_tokens.unwrap_or(0),
            cache_creation_input_tokens: u.cache_creation_input_tokens.unwrap_or(0),
            cache_read_input_tokens: u.cache_read_input_tokens.unwrap_or(0),
        }
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

/// Why one attempt failed.
#[derive(Debug)]
enum Failure {
    Status {
        status: StatusCode,
        retry_after: Option<Duration>,
        detail: String,
    },
    Timeout,
    /// `connect`: the connection itself failed, so nothing was sent.
    Transport {
        message: String,
        connect: bool,
    },
    /// A success status with a body that is not a Messages API response.
    BadBody(String),
    /// Once a successful streaming response begins, never retry it: some
    /// previews may already have been displayed and billing is uncertain.
    Stream(RemoteError),
}

impl Failure {
    fn retryable(&self) -> bool {
        match self {
            Failure::Status { status, .. } => matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error(),
            Failure::Timeout | Failure::Transport { .. } => true,
            Failure::BadBody(_) | Failure::Stream(_) => false,
        }
    }

    /// The API may have done (and billed) the work. An error status comes
    /// with no usage and a refused connection sent nothing; a timeout, a cut
    /// connection or a broken body leave it unknown.
    fn may_have_billed(&self) -> bool {
        match self {
            Failure::Status { .. } => false,
            Failure::Transport { connect, .. } => !connect,
            Failure::Timeout | Failure::BadBody(_) | Failure::Stream(_) => true,
        }
    }

    /// What failed, in a few words, for a progress event.
    fn short(&self) -> String {
        match self {
            Failure::Status { status, .. } => format!("model API {}", status.as_u16()),
            Failure::Timeout => "model API timed out".into(),
            Failure::Transport { .. } => "could not reach the model API".into(),
            Failure::BadBody(_) | Failure::Stream(_) => "model API sent a bad response".into(),
        }
    }

    /// `timeout` is the last attempt's; `cut` says the caller's deadline set it.
    fn into_error(self, attempts: u32, timeout: Duration, cut: bool) -> RemoteError {
        let tries = if attempts > 1 { format!(" ({attempts} attempts)") } else { String::new() };
        let (code, message) = match self {
            Failure::Stream(error) => (error.code, error.message),
            Failure::Status { status, detail, .. } => {
                let code = match status.as_u16() {
                    400 | 404 | 413 | 422 => ErrorCode::Invalid,
                    401 | 403 => ErrorCode::Denied,
                    408 => ErrorCode::Timeout,
                    409 | 429 | 529 => ErrorCode::Busy,
                    s if s >= 500 => ErrorCode::Unavailable,
                    _ => ErrorCode::Failed,
                };
                (code, format!("the Messages API returned {}{detail}{tries}", status.as_u16()))
            }
            Failure::Timeout if cut => {
                (ErrorCode::Timeout, format!("the Messages API did not answer before the caller's deadline{tries}"))
            }
            Failure::Timeout => {
                (ErrorCode::Timeout, format!("the Messages API did not answer within {timeout:?}{tries}"))
            }
            Failure::Transport { message, .. } => {
                (ErrorCode::Unavailable, format!("could not reach the Messages API: {message}{tries}"))
            }
            Failure::BadBody(e) => (ErrorCode::Failed, format!("the Messages API sent an unreadable response: {e}")),
        };
        RemoteError { code, message }
    }
}

/// A retry the gateway is about to make, for progress.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Retry {
    /// What failed, in a few words: `model API 502`.
    pub failure: String,
    pub delay: Duration,
    /// This retry's number, from 1, and the most retries there will be.
    pub retry: u32,
    pub retries: u32,
}

/// Who hears about a call while it is under way: streamed text, and each
/// retry before its wait. Neither may block.
#[derive(Default)]
pub(crate) struct Observer<'a> {
    pub text: Option<&'a mut (dyn FnMut(String) + Send)>,
    pub retry: Option<&'a mut (dyn FnMut(Retry) + Send)>,
}

/// The settings an error message names, so it can say what to fix.
struct Names {
    base_url: String,
    path: &'static str,
    credential: String,
}

/// The protocol one call speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wire {
    Messages,
    ChatCompletions,
    Responses,
}

impl Wire {
    fn name(self) -> &'static str {
        match self {
            Self::Messages => "Messages API",
            Self::ChatCompletions => "OpenAI Chat Completions API",
            Self::Responses => "OpenAI Responses API",
        }
    }
}

/// A client for `POST {base_url}/v1/messages`, or for OpenAI's
/// `/v1/chat/completions` and `/v1/responses`.
pub(crate) struct Api {
    provider: Provider,
    names: Names,
    /// OpenAI's `/v1/responses`, beside `url` for Chat Completions.
    responses: Option<Url>,
    /// The base URL ends in `/v1`, which a 404 most likely comes from.
    doubled_v1: bool,
    http: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    timeout: Duration,
    max_retries: u32,
    retry_base: Duration,
}

impl Api {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !cfg.api_key.is_empty() || cfg.auth_token.as_ref().is_some_and(|token| !token.is_empty()),
            "no Anthropic credential: set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN"
        );
        // Appended rather than joined, so a base URL with a path prefix keeps it.
        let url: Url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'))
            .parse()
            .with_context(|| format!("bad base URL {:?}", cfg.base_url))?;
        // No redirects: credentials must not be re-sent to a host named by a 307.
        let mut builder = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(concat!("molt-gateway/", env!("CARGO_PKG_VERSION")));
        // Tests point the gateway at a local mock server, and a configured HTTPS_PROXY must not swallow that.
        if is_loopback(&url) {
            builder = builder.no_proxy();
        }
        let http = builder.build().context("building the HTTP client")?;

        let mut headers = HeaderMap::new();
        if !cfg.api_key.is_empty() {
            let mut key = HeaderValue::from_str(&cfg.api_key).context("the API key is not a valid header value")?;
            key.set_sensitive(true);
            headers.insert("x-api-key", key);
        } else if let Some(token) = &cfg.auth_token {
            let mut token = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("the Anthropic auth token is not a valid header value")?;
            token.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, token);
        }
        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(Self {
            provider: Provider::Anthropic,
            names: Names {
                base_url: "ANTHROPIC_BASE_URL".into(),
                path: "/v1/messages",
                credential: "ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN".into(),
            },
            doubled_v1: ends_in_v1(&cfg.base_url),
            http,
            url,
            responses: None,
            headers,
            timeout: cfg.timeout,
            max_retries: cfg.max_retries,
            retry_base: cfg.retry_base,
        })
    }

    pub fn openai(cfg: &Config, key: &str) -> anyhow::Result<Self> {
        let origin = cfg.providers.openai_base_url.trim_end_matches('/');
        let url: Url = format!("{origin}/v1/chat/completions").parse().context("bad OPENAI_BASE_URL")?;
        let responses: Url = format!("{origin}/v1/responses").parse().context("bad OPENAI_BASE_URL")?;
        anyhow::ensure!(
            url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
            "OPENAI_BASE_URL cannot contain credentials, query or fragment"
        );
        anyhow::ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url)),
            "OpenAI credentials require HTTPS except on loopback"
        );
        let mut builder = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("molt-gateway/", env!("CARGO_PKG_VERSION")));
        if is_loopback(&url) {
            builder = builder.no_proxy();
        }
        let http = builder.build()?;
        let mut key = HeaderValue::from_str(&format!("Bearer {key}")).context("invalid OpenAI authorization header")?;
        key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, key);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(Self {
            provider: Provider::Openai,
            names: Names {
                base_url: "OPENAI_BASE_URL".into(),
                path: "/v1/chat/completions",
                credential: "OPENAI_API_KEY".into(),
            },
            doubled_v1: ends_in_v1(&cfg.providers.openai_base_url),
            http,
            url,
            responses: Some(responses),
            headers,
            timeout: cfg.timeout,
            max_retries: cfg.max_retries,
            retry_base: cfg.retry_base,
        })
    }

    /// A client for the user's endpoint `name`, with `key` from its
    /// `key_env`. The endpoint is checked first (see [`Endpoint::check`]).
    pub fn endpoint(cfg: &Config, name: &str, endpoint: &Endpoint, key: &str) -> anyhow::Result<Self> {
        endpoint.check(name).map_err(anyhow::Error::msg)?;
        let origin = endpoint.origin();
        let url: Url = format!("{origin}{}", endpoint.protocol.path()).parse()?;
        let mut builder = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(concat!("molt-gateway/", env!("CARGO_PKG_VERSION")));
        if is_loopback(&url) {
            builder = builder.no_proxy();
        }
        let http = builder.build().context("building the HTTP client")?;
        let invalid = || format!("endpoint {name}: the key in {} is not a valid header value", endpoint.key_env);
        let mut headers = HeaderMap::new();
        match endpoint.auth() {
            Auth::XApiKey => {
                let mut key = HeaderValue::from_str(key).with_context(invalid)?;
                key.set_sensitive(true);
                headers.insert("x-api-key", key);
            }
            Auth::Bearer => {
                let mut key = HeaderValue::from_str(&format!("Bearer {key}")).with_context(invalid)?;
                key.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, key);
            }
        }
        if endpoint.protocol == EndpointProtocol::Anthropic {
            headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Each OpenAI protocol has its own endpoint, so both URLs are the one path.
        let responses = (endpoint.protocol == EndpointProtocol::Responses).then(|| url.clone());
        Ok(Self {
            provider: endpoint.protocol.provider(),
            names: Names {
                base_url: format!("endpoint {name}'s base_url"),
                path: endpoint.protocol.path(),
                credential: format!("{} (endpoint {name}'s key_env)", endpoint.key_env),
            },
            doubled_v1: false,
            http,
            url,
            responses,
            headers,
            timeout: cfg.timeout,
            max_retries: cfg.max_retries,
            retry_base: cfg.retry_base,
        })
    }

    /// Send `body`, retrying transient failures. Before `deadline`, when there
    /// is one: each attempt gets at most the time left, and a retry that could
    /// not finish in time is not started, since the caller has stopped waiting
    /// by then and the API bills a call whether or not anyone reads it.
    pub async fn create(
        &self,
        body: &Value,
        fallbacks: bool,
        deadline: Option<Instant>,
        mut observer: Observer<'_>,
    ) -> Result<Message, RemoteError> {
        let wire = match self.provider {
            Provider::Anthropic => Wire::Messages,
            Provider::Openai if crate::responses::is_responses(body) => Wire::Responses,
            Provider::Openai => Wire::ChatCompletions,
        };
        if wire == Wire::Messages {
            crate::responses::refuse_on_messages(body)?;
        }
        let streaming = body["stream"] == true;
        let bytes = serde_json::to_vec(body)
            .map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("encoding the request: {e}") })?;
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let mut retries = 0;
        // An earlier attempt the API may have billed, which the last one's
        // refusal does not make free.
        let mut billed_before = false;
        loop {
            let timeout = left().map_or(self.timeout, |left| left.min(self.timeout));
            if timeout.is_zero() {
                let message = "the caller's deadline passed before the Messages API was called".to_owned();
                return Err(RemoteError { code: ErrorCode::Timeout, message });
            }
            let failure =
                match self.attempt(wire, bytes.clone(), fallbacks, timeout, streaming, &mut observer.text).await {
                    Ok(msg) => return Ok(msg),
                    Err(f) => f,
                };
            let cut = timeout < self.timeout;
            if !failure.retryable() || retries >= self.max_retries {
                return Err(self.give_up(wire, failure, retries + 1, timeout, cut, billed_before));
            }
            let delay = match &failure {
                Failure::Status { retry_after: Some(d), .. } => *d,
                _ => backoff(self.retry_base, retries),
            };
            if left().is_some_and(|left| left < delay + MIN_ATTEMPT) {
                tracing::warn!(
                    ?failure,
                    ?delay,
                    "Messages API call failed; no time to retry before the caller's deadline"
                );
                return Err(self.give_up(wire, failure, retries + 1, timeout, cut, billed_before));
            }
            tracing::warn!(?failure, retry = retries + 1, ?delay, "Messages API call failed; retrying");
            if let Some(on_retry) = observer.retry.as_mut() {
                on_retry(Retry { failure: failure.short(), delay, retry: retries + 1, retries: self.max_retries });
            }
            billed_before |= failure.may_have_billed();
            tokio::time::sleep(delay).await;
            retries += 1;
        }
    }

    /// The error for the call's last failure, saying what to fix where the
    /// status points at a setting. A refusal (`denied`, `invalid`) means
    /// nothing was billed, which callers count on, so it is reported as
    /// `failed` when an earlier attempt may have been.
    fn give_up(
        &self,
        wire: Wire,
        failure: Failure,
        attempts: u32,
        timeout: Duration,
        cut: bool,
        billed_before: bool,
    ) -> RemoteError {
        let status = match &failure {
            Failure::Status { status, .. } => Some(status.as_u16()),
            _ => None,
        };
        let mut error = failure.into_error(attempts, timeout, cut);
        error.message = error.message.replace(Wire::Messages.name(), wire.name());
        let names = &self.names;
        let path = if wire == Wire::Responses { "/v1/responses" } else { names.path };
        match status {
            Some(404) if self.doubled_v1 => error.message.push_str(&format!(
                "; {} ends in /v1 and molt adds {path} itself: remove the trailing /v1",
                names.base_url
            )),
            Some(401) => error.message.push_str(&format!("; the credential was rejected: check {}", names.credential)),
            _ => {}
        }
        if billed_before && matches!(error.code, ErrorCode::Denied | ErrorCode::Invalid) {
            error.code = ErrorCode::Failed;
            error.message.push_str("; an earlier attempt may have been billed, so the cost is unknown");
        }
        error
    }

    async fn attempt(
        &self,
        wire: Wire,
        body: Vec<u8>,
        fallbacks: bool,
        timeout: Duration,
        streaming: bool,
        on_text: &mut Option<&mut (dyn FnMut(String) + Send)>,
    ) -> Result<Message, Failure> {
        let url = if wire == Wire::Responses { self.responses.as_ref().unwrap_or(&self.url) } else { &self.url };
        let mut req = self.http.post(url.clone()).headers(self.headers.clone()).timeout(timeout).body(body);
        if fallbacks {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }
        let resp = req.send().await.map_err(transport)?;
        let status = resp.status();
        let retry_after = retry_after(resp.headers());
        let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let request_id = header("request-id");
        let location = header("location").filter(|_| status.is_redirection());
        if status.is_success() && streaming {
            return read_stream(resp, on_text, wire).await;
        }
        let bytes = resp.bytes().await.map_err(transport)?;
        if status.is_success() {
            let decode = match wire {
                Wire::Messages => return serde_json::from_slice(&bytes).map_err(|e| Failure::BadBody(e.to_string())),
                Wire::ChatCompletions => crate::openai::decode,
                Wire::Responses => crate::responses::decode,
            };
            return serde_json::from_slice(&bytes)
                .map_err(|e| Failure::BadBody(e.to_string()))
                .and_then(|v| decode(v).map_err(Failure::BadBody));
        }
        let mut detail = match serde_json::from_slice::<ErrorBody>(&bytes) {
            Ok(b) => format!(": {}: {}", b.error.kind, b.error.message),
            Err(_) if bytes.is_empty() => String::new(),
            Err(_) => format!(": {}", String::from_utf8_lossy(&bytes).chars().take(DETAIL_CHARS).collect::<String>()),
        };
        if let Some(to) = location {
            detail.push_str(&format!(" [redirect to {to} not followed]"));
        }
        if let Some(id) = request_id {
            detail.push_str(&format!(" [request-id {id}]"));
        }
        Err(Failure::Status { status, retry_after, detail })
    }
}

enum ResponseStream {
    Anthropic(Accumulator),
    Openai(crate::openai::Accumulator),
    Responses(crate::responses::Accumulator),
}
impl ResponseStream {
    fn new(wire: Wire) -> Self {
        match wire {
            Wire::Messages => Self::Anthropic(Accumulator::default()),
            Wire::ChatCompletions => Self::Openai(crate::openai::Accumulator::default()),
            Wire::Responses => Self::Responses(crate::responses::Accumulator::default()),
        }
    }
    fn event(&mut self, event: Value) -> Result<Option<String>, String> {
        match self {
            Self::Anthropic(s) => s.event(event),
            Self::Openai(s) => s.event(event),
            Self::Responses(s) => s.event(event),
        }
    }
    fn finish(self) -> Result<Message, String> {
        match self {
            Self::Anthropic(s) => s.finish(),
            Self::Openai(s) => s.finish(),
            Self::Responses(s) => s.finish(),
        }
    }
}

async fn read_stream(
    mut response: reqwest::Response,
    on_text: &mut Option<&mut (dyn FnMut(String) + Send)>,
    wire: Wire,
) -> Result<Message, Failure> {
    let bad = |e: String| Failure::BadBody(format!("invalid Messages API stream: {e}"));
    let content_type = response.headers().get(CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("");
    if content_type.split(';').next().unwrap_or("").trim() != "text/event-stream" {
        return Err(bad("expected text/event-stream".into()));
    }
    // Only Chat Completions ends its stream with `[DONE]`.
    let mut decoder =
        Decoder::for_provider(if wire == Wire::ChatCompletions { Provider::Openai } else { Provider::Anthropic });
    let mut accumulator = ResponseStream::new(wire);
    let mut preview = String::new();
    let mut last_flush = Instant::now() - Duration::from_secs(1);
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk.map_err(|e| {
                Failure::Stream(RemoteError {
                    code: if e.is_timeout() { ErrorCode::Timeout } else { ErrorCode::Unavailable },
                    message: "the Messages API stream was interrupted; partial content was not used and cost is unknown".into(),
                })
            })?,
            _ = tokio::time::sleep_until((last_flush + Duration::from_millis(100)).into()), if !preview.is_empty() => {
                flush_preview(&mut preview, on_text);
                last_flush = Instant::now();
                continue;
            }
        };
        let Some(chunk) = chunk else { break };
        for event in decoder.feed(&chunk).map_err(bad)? {
            if let Some(text) = accumulator.event(event).map_err(bad)? {
                preview.push_str(&text);
            }
            // Coalesce token-sized deltas. The first preview is immediate;
            // later ones are capped at 4 KiB and flushed at most every 100 ms
            // unless their size requires an earlier flush.
            if !preview.is_empty() && (preview.len() >= 4096 || last_flush.elapsed() >= Duration::from_millis(100)) {
                flush_preview(&mut preview, on_text);
                last_flush = Instant::now();
            }
        }
    }
    decoder.finish().map_err(bad)?;
    let message = accumulator.finish().map_err(bad)?;
    flush_preview(&mut preview, on_text);
    Ok(message)
}

fn flush_preview(preview: &mut String, on_text: &mut Option<&mut (dyn FnMut(String) + Send)>) {
    if let Some(on_text) = on_text.as_mut() {
        while !preview.is_empty() {
            let mut end = preview.len().min(4096);
            while !preview.is_char_boundary(end) {
                end -= 1;
            }
            on_text(preview.drain(..end).collect());
        }
    } else {
        preview.clear();
    }
}

fn transport(e: reqwest::Error) -> Failure {
    if e.is_timeout() {
        return Failure::Timeout;
    }
    // reqwest's own message is terse ("error sending request"); the cause is in the source chain.
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        message.push_str(": ");
        message.push_str(&s.to_string());
        source = s.source();
    }
    Failure::Transport { message, connect: e.is_connect() }
}

/// A base URL whose path already ends in `/v1`, to which `/v1/...` is added.
pub(crate) fn ends_in_v1(base_url: &str) -> bool {
    base_url.trim_end_matches('/').to_ascii_lowercase().ends_with("/v1")
}

/// A `retry-after` given in seconds, capped.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: f64 = headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs.min(MAX_RETRY_AFTER.as_secs_f64())))
}

/// Exponential backoff from `base` for the retry numbered `retry` (from 0),
/// capped, with +-25% jitter so parallel callers spread out.
fn backoff(base: Duration, retry: u32) -> Duration {
    let cap = MAX_BACKOFF.as_secs_f64();
    let exp = (base.as_secs_f64() * 2f64.powi(retry.min(30) as i32)).min(cap);
    Duration::from_secs_f64((exp * rand::random_range(0.75..=1.25)).min(cap))
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_recognised() {
        for url in
            ["http://127.0.0.1:8080", "http://localhost", "http://LOCALHOST:1", "http://[::1]:9", "http://127.1.2.3"]
        {
            assert!(is_loopback(&url.parse().unwrap()), "{url}");
        }
        for url in ["https://api.anthropic.com", "http://10.0.0.1", "http://localhost.example.com"] {
            assert!(!is_loopback(&url.parse().unwrap()), "{url}");
        }
    }

    #[test]
    fn backoff_doubles_with_jitter_and_is_capped() {
        let base = Duration::from_secs(1);
        for (retry, nominal) in [(0, 1.0), (1, 2.0), (2, 4.0), (3, 8.0)] {
            let d = backoff(base, retry).as_secs_f64();
            assert!(d >= nominal * 0.75 && d <= nominal * 1.25, "retry {retry}: {d}");
        }
        assert!(backoff(base, 10) <= MAX_BACKOFF);
        assert!(backoff(base, u32::MAX) <= MAX_BACKOFF);

        // The jitter is real: draws land on both sides of the nominal delay.
        let draws: Vec<f64> = (0..200).map(|_| backoff(base, 2).as_secs_f64()).collect();
        assert!(draws.iter().all(|d| (3.0..=5.0).contains(d)), "{draws:?}");
        assert!(draws.iter().any(|&d| d < 4.0) && draws.iter().any(|&d| d > 4.0), "{draws:?}");
    }

    #[test]
    fn retry_after_is_read_in_seconds_and_capped() {
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(2)));
        h.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
        assert_eq!(retry_after(&h), Some(MAX_RETRY_AFTER));
        h.insert(RETRY_AFTER, HeaderValue::from_static("1e300"));
        assert_eq!(retry_after(&h), Some(MAX_RETRY_AFTER));
        h.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(retry_after(&h), None);
        h.insert(RETRY_AFTER, HeaderValue::from_static("-1"));
        assert_eq!(retry_after(&h), None);
    }

    #[test]
    fn null_cache_counts_read_as_zero() {
        let msg: Message = serde_json::from_value(serde_json::json!({
            "id": "msg_1",
            "model": "m",
            "content": [],
            "stop_reason": null,
            "stop_details": null,
            "usage": {
                "input_tokens": 3,
                "output_tokens": 4,
                "cache_creation_input_tokens": null,
                "cache_read_input_tokens": null
            }
        }))
        .unwrap();
        let usage = Usage::from(msg.usage);
        assert_eq!(usage, Usage { input_tokens: 3, output_tokens: 4, ..Default::default() });
        assert_eq!(msg.stop_reason, None);
        assert_eq!(msg.stop_details, None);
    }
}
