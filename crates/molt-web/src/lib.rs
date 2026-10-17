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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use molt_api::web::{FetchRequest, FetchResponse};
use molt_proto::{ErrorCode, RemoteError, Target};
use molt_sdk::Service;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::Url;
use serde_json::Value;

pub use allow::{public, Allow};

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
        Ok(Self::with_transport(allow, Box::new(Https::new()?)))
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
            let hop = self.transport.get(&url, MAX_BYTES).await.map_err(|e| failed(format!("fetching {url}: {e}")))?;
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
    client: reqwest::Client,
}

impl Https {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .https_only(true)
            // A proxy would resolve names itself, past the address check.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(PublicOnly))
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("molt-web/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Transport for Https {
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<Hop, String> {
        let mut resp = self.client.get(url.clone()).send().await.map_err(|e| error_chain(&e))?;
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

/// Resolves names with the system resolver, keeping only public addresses.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let kept: Vec<SocketAddr> = found.iter().copied().filter(|a| public(a.ip())).collect();
            if kept.is_empty() {
                let err: Box<dyn std::error::Error + Send + Sync> = if found.is_empty() {
                    format!("{host} has no address").into()
                } else {
                    format!("{host} resolves only to addresses that are not public").into()
                };
                return Err(err);
            }
            Ok(Box::new(kept.into_iter()) as Addrs)
        })
    }
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

    #[tokio::test]
    async fn names_that_resolve_only_to_local_addresses_are_not_connected_to() {
        let name: Name = "localhost".parse().unwrap();
        let err = match PublicOnly.resolve(name).await {
            Ok(addrs) => panic!("resolved to {:?}", addrs.collect::<Vec<_>>()),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not public") || err.contains("no address"), "{err}");
    }
}
