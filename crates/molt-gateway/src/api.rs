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
    Transport(String),
    /// A success status with a body that is not a Messages API response.
    BadBody(String),
}

impl Failure {
    fn retryable(&self) -> bool {
        match self {
            Failure::Status { status, .. } => matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error(),
            Failure::Timeout | Failure::Transport(_) => true,
            Failure::BadBody(_) => false,
        }
    }

    /// `timeout` is the last attempt's; `cut` says the caller's deadline set it.
    fn into_error(self, attempts: u32, timeout: Duration, cut: bool) -> RemoteError {
        let tries = if attempts > 1 { format!(" ({attempts} attempts)") } else { String::new() };
        let (code, message) = match self {
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
            Failure::Transport(e) => (ErrorCode::Unavailable, format!("could not reach the Messages API: {e}{tries}")),
            Failure::BadBody(e) => (ErrorCode::Failed, format!("the Messages API sent an unreadable response: {e}")),
        };
        RemoteError { code, message }
    }
}

/// A client for `POST {base_url}/v1/messages`.
pub(crate) struct Api {
    http: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    timeout: Duration,
    max_retries: u32,
    retry_base: Duration,
}

impl Api {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        anyhow::ensure!(!cfg.api_key.is_empty(), "no Anthropic API key");
        // Appended rather than joined, so a base URL with a path prefix keeps it.
        let url: Url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'))
            .parse()
            .with_context(|| format!("bad base URL {:?}", cfg.base_url))?;
        // No redirects: reqwest would re-send x-api-key to whatever host a 307 names.
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

        let mut key = HeaderValue::from_str(&cfg.api_key).context("the API key is not a valid header value")?;
        key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", key);
        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(Self { http, url, headers, timeout: cfg.timeout, max_retries: cfg.max_retries, retry_base: cfg.retry_base })
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
    ) -> Result<Message, RemoteError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("encoding the request: {e}") })?;
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let mut retries = 0;
        loop {
            let timeout = left().map_or(self.timeout, |left| left.min(self.timeout));
            if timeout.is_zero() {
                let message = "the caller's deadline passed before the Messages API was called".to_owned();
                return Err(RemoteError { code: ErrorCode::Timeout, message });
            }
            let failure = match self.attempt(bytes.clone(), fallbacks, timeout).await {
                Ok(msg) => return Ok(msg),
                Err(f) => f,
            };
            let cut = timeout < self.timeout;
            if !failure.retryable() || retries >= self.max_retries {
                return Err(failure.into_error(retries + 1, timeout, cut));
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
                return Err(failure.into_error(retries + 1, timeout, cut));
            }
            tracing::warn!(?failure, retry = retries + 1, ?delay, "Messages API call failed; retrying");
            tokio::time::sleep(delay).await;
            retries += 1;
        }
    }

    async fn attempt(&self, body: Vec<u8>, fallbacks: bool, timeout: Duration) -> Result<Message, Failure> {
        let mut req = self.http.post(self.url.clone()).headers(self.headers.clone()).timeout(timeout).body(body);
        if fallbacks {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }
        let resp = req.send().await.map_err(transport)?;
        let status = resp.status();
        let retry_after = retry_after(resp.headers());
        let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let request_id = header("request-id");
        let location = header("location").filter(|_| status.is_redirection());
        let bytes = resp.bytes().await.map_err(transport)?;
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|e| Failure::BadBody(e.to_string()));
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
    Failure::Transport(message)
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

fn is_loopback(url: &Url) -> bool {
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
