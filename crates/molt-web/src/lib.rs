//! The `web` service (see [`molt_api::web`]): `web.fetch` reads one page
//! from a host on the allowlist the service was started with.
//!
//! Every URL, the first and each redirect, must be `https` on the default
//! port, name an allowed host, and carry no credentials ([`Allow::check`]).
//! Names are resolved here and only public addresses are connected to
//! ([`public`]), so an allowed name that points at the user's machine or
//! network leads nowhere. No proxy is used. A page is read up to
//! [`MAX_BYTES`]; HTML becomes plain text ([`html::to_text`]), and other
//! types that are not text are refused.
//!
//! Nothing here holds a secret, and nothing fetched is run: the text goes
//! back to the caller, and through the kernel's audit log on its way.

mod allow;
pub mod html;
mod resolver;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use molt_api::web::{FetchRequest, FetchResponse};
use molt_proto::{ErrorCode, RemoteError, Target};
use molt_sdk::Service;
use reqwest::dns::{Name, Resolve, Resolving};
use reqwest::Url;
use serde_json::Value;

pub use allow::{public, Allow};
pub use resolver::{preflight, validate_resolver};

/// Most bytes of a response read.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Most bytes of text returned.
pub const MAX_TEXT: usize = 100_000;
/// Most redirects followed.
pub const MAX_REDIRECTS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One HTTP response, before redirects are followed.
#[derive(Clone, Debug, Default)]
pub struct Hop {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
    /// The body was longer than the limit and was cut.
    pub truncated: bool,
}

/// How one `GET` is made. [`Https`] is the real one.
#[async_trait]
pub trait Transport: Send + Sync {
    /// `GET url` without following redirects, reading at most `max_bytes` of the body.
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<Hop, String>;
}

/// Serves `web.*`.
pub struct Web {
    allow: Allow,
    transport: Box<dyn Transport>,
}

impl Web {
    /// The service for `allow`, fetching over the network.
    pub fn new(allow: Allow) -> anyhow::Result<Self> {
        Self::with_resolver(allow, "system")
    }

    pub fn with_resolver(allow: Allow, resolver: &str) -> anyhow::Result<Self> {
        Ok(Self::with_transport(allow, Box::new(Https::with_resolver(resolver)?)))
    }

    pub fn with_transport(allow: Allow, transport: Box<dyn Transport>) -> Self {
        Self { allow, transport }
    }

    /// Handle one `web` method with its request payload.
    pub async fn handle(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        match method.strip_prefix("web.").unwrap_or(method) {
            "fetch" => {
                let req: FetchRequest =
                    serde_json::from_value(payload).map_err(|e| invalid(format!("bad web.fetch request: {e}")))?;
                let reply = self.fetch(&req.url).await?;
                serde_json::to_value(reply).map_err(|e| failed(e.to_string()))
            }
            other => Err(invalid(format!("unknown method web.{other}"))),
        }
    }

    pub async fn fetch(&self, url: &str) -> Result<FetchResponse, RemoteError> {
        let mut url = Url::parse(url.trim()).map_err(|e| invalid(format!("{url:?} is not a URL: {e}")))?;
        url.set_fragment(None);
        self.allow.check(&url).map_err(invalid)?;
        let mut redirects = 0;
        loop {
            let hop = self.transport.get(&url, MAX_BYTES).await.map_err(|error| {
                if error.starts_with(molt_api::web::ENVIRONMENT_UNAVAILABLE) {
                    RemoteError { code: ErrorCode::Unavailable, message: error }
                } else {
                    failed(format!("fetching {url}: {error}"))
                }
            })?;
            match hop.location.as_deref().filter(|_| (300..400).contains(&hop.status)) {
                Some(location) => {
                    let mut next =
                        url.join(location).map_err(|e| failed(format!("{url} redirects to {location:?}: {e}")))?;
                    next.set_fragment(None);
                    self.allow.check(&next).map_err(|e| invalid(format!("{url} redirects to {next}, refused: {e}")))?;
                    redirects += 1;
                    if redirects > MAX_REDIRECTS {
                        return Err(failed(format!("{url} redirects more than {MAX_REDIRECTS} times")));
                    }
                    url = next;
                }
                None => return render(url, hop),
            }
        }
    }
}

/// The page `hop` holds, as text.
fn render(url: Url, hop: Hop) -> Result<FetchResponse, RemoteError> {
    let mime = hop.content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let html = mime == "text/html" || mime == "application/xhtml+xml";
    let textual = html
        || mime.is_empty()
        || mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(mime.as_str(), "application/json" | "application/xml" | "application/javascript");
    if !textual || hop.body.iter().take(8192).any(|b| *b == 0) {
        return Err(invalid(format!("{url} is {}, not text", if mime.is_empty() { "binary" } else { &mime })));
    }
    let raw = String::from_utf8_lossy(&hop.body);
    let mut text = if html { html::to_text(&raw, &url) } else { raw.into_owned() };
    let mut truncated = hop.truncated;
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    Ok(FetchResponse { url: url.to_string(), status: hop.status, content_type: hop.content_type, text, truncated })
}

/// [`Transport`] over HTTPS, connecting only to public addresses.
pub struct Https {
    resolver: resolver::Resolver,
}

impl Https {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_resolver("system")
    }

    pub fn with_resolver(spec: &str) -> anyhow::Result<Self> {
        Ok(Self { resolver: resolver::Resolver::new(spec)? })
    }
}

/// Every page destination is pinned first. An unexpected resolver fallback must fail closed.
struct NoFallback;
impl Resolve for NoFallback {
    fn resolve(&self, _name: Name) -> Resolving {
        Box::pin(async { Err("web destination was not pinned to public addresses".into()) })
    }
}

#[async_trait]
impl Transport for Https {
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<Hop, String> {
        let host = url.domain().ok_or("web destination must name a host")?.trim_end_matches('.');
        let addresses = self.resolver.lookup(host).await?;
        let mut destination = url.clone();
        destination.set_host(Some(host)).map_err(|_| "invalid web destination")?;
        let client = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(NoFallback))
            .resolve_to_addrs(host, &addresses)
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("molt-web/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| error.to_string())?;
        let mut resp = client.get(destination).send().await.map_err(|e| error_chain(&e))?;
        let header = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let location = header(reqwest::header::LOCATION);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
        let status = resp.status().as_u16();
        let (mut body, mut truncated) = (Vec::new(), false);
        while let Some(chunk) = resp.chunk().await.map_err(|e| error_chain(&e))? {
            let room = max_bytes - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Hop { status, location, content_type, body, truncated })
    }
}

/// An error with its causes, which say what actually went wrong (refused
/// address, TLS, timeout).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        out.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    out
}

/// Serve `web.*` on `svc` until its link closes.
pub async fn serve(svc: Arc<Service>, web: Arc<Web>) {
    svc.serve_concurrent(8, move |req| {
        let web = web.clone();
        async move {
            let method = match &req.to {
                Target::Method { method, .. } => method.clone(),
                other => return Err(invalid(format!("{other} is not a method"))),
            };
            web.handle(&method, req.payload).await
        }
    })
    .await
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Failed, message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RefusedTransport;
    #[async_trait]
    impl Transport for RefusedTransport {
        async fn get(&self, _url: &Url, _max_bytes: usize) -> Result<Hop, String> {
            Err(format!("{}fake-IP DNS; do not retry", molt_api::web::ENVIRONMENT_UNAVAILABLE))
        }
    }
    #[tokio::test]
    async fn a_dns_configuration_failure_is_a_structured_unavailable_reply() {
        let web = Web::with_transport(Allow::parse(&["docs.rs"]).unwrap(), Box::new(RefusedTransport));
        let error = web.fetch("https://docs.rs/example").await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);
        assert!(error.message.starts_with(molt_api::web::ENVIRONMENT_UNAVAILABLE));
    }

    #[tokio::test]
    async fn names_that_resolve_only_to_local_addresses_are_not_connected_to() {
        let err = resolver::Resolver::new("system").unwrap().lookup("localhost").await.unwrap_err();
        assert!(err.contains("not public") || err.contains("no address"), "{err}");
    }
}
