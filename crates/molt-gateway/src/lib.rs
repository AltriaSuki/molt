//! The model gateway: serves `model.complete` by calling the Anthropic
//! Messages API.
//!
//! It is the only process that holds the API key. It turns a
//! [`CompleteRequest`] into a Messages API request shaped for the chosen
//! model (thinking, effort, server-side fallbacks, prompt caching), retries
//! transient failures, and returns the content verbatim with usage and an
//! estimated cost.

use std::sync::Arc;
use std::time::Duration;

use molt_api::model::{CompleteRequest, CompleteResponse, Effort};
use molt_proto::RemoteError;
use molt_sdk::Service;

/// Gateway settings. [`Config::from_env`] documents the variables.
#[derive(Clone, Debug)]
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
    /// Per HTTP attempt.
    pub timeout: Duration,
    /// Retries after the first attempt for 408, 409, 429, 5xx and network errors.
    pub max_retries: u32,
    /// Requests handled at once.
    pub max_in_flight: usize,
    /// Opt into server-side fallbacks on models that support them.
    pub fallbacks: bool,
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
    pub fn from_env() -> anyhow::Result<Self> {
        todo!()
    }
}

/// The full model id for an alias (`opus`, `sonnet`, `haiku`); any other
/// string is returned unchanged.
pub fn resolve_model(name: &str) -> String {
    let _ = name;
    todo!()
}

pub struct Gateway {
    _cfg: Config,
}

impl Gateway {
    pub fn new(cfg: Config) -> anyhow::Result<Self> {
        let _ = cfg;
        todo!()
    }

    /// One Messages API call, with retries.
    pub async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, RemoteError> {
        let _ = req;
        todo!()
    }
}

/// Serve `model.complete` on `svc` until its link closes.
pub async fn serve(svc: Arc<Service>, gateway: Arc<Gateway>) {
    let _ = (svc, gateway);
    todo!()
}
