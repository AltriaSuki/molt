//! Pluggable transports for the Molt bus.
//!
//! A transport only moves envelopes between the kernel and services. It never
//! routes service-to-service: every service has exactly one authenticated
//! endpoint, and only the kernel can read from it or write to its inbox. That
//! is what keeps the kernel the single choke point whichever wire is in use.
//!
//! Two implementations ship from day one, behind the same [`Transport`] and
//! [`Link`] traits and the same conformance suite ([`testkit`]):
//!
//! - [`unix::UnixTransport`]: one Unix socket per service, for a single host.
//! - [`nats::NatsTransport`]: NATS subjects guarded by per-service server
//!   permissions, for many hosts.

use std::fmt;

use async_trait::async_trait;
use futures::stream::BoxStream;
use molt_proto::{Envelope, ServiceId};

#[cfg(feature = "nats")]
pub mod nats;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod unix;

/// Environment variables the supervisor sets for every service process.
pub const ENV_ADDRESS: &str = "MOLT_ADDRESS";
pub const ENV_SERVICE_ID: &str = "MOLT_SERVICE_ID";
pub const ENV_SECRET: &str = "MOLT_SECRET";

/// Largest envelope a transport accepts, in bytes.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("service {0} is not connected")]
    NotConnected(ServiceId),
    #[error("inbox of {0} is full")]
    Busy(ServiceId),
    #[error("authentication failed")]
    Auth,
    #[error("unsupported address {0:?}")]
    Address(String),
    #[error("transport closed")]
    Closed,
    #[error("encoding: {0}")]
    Codec(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// A message from a service, with the sender identity established by the
/// transport (never taken from the envelope).
#[derive(Debug, Clone)]
pub struct Inbound {
    pub from: ServiceId,
    pub msg: Envelope,
}

/// The credential a service presents to its endpoint. Debug output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn random() -> Self {
        Self(hex::encode(rand::random::<[u8; 32]>()))
    }

    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Comparison whose running time does not depend on where the inputs differ.
    pub fn matches(&self, presented: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), presented.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// The kernel's side of a transport.
///
/// Guarantees every implementation must keep (checked by [`testkit`]):
/// - messages from one sender arrive in the order sent, and messages the
///   kernel sends to one service arrive in the order sent;
/// - delivery is at most once: no duplicates, and a failed send is reported
///   or dropped, never retried behind the caller's back;
/// - [`Inbound::from`] is the authenticated endpoint, whatever the envelope says;
/// - a service can neither read another service's inbox nor write into
///   another service's endpoint.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Open the authenticated endpoint for `id`. The service must present
    /// `secret` to use it.
    async fn open(&self, id: &ServiceId, secret: &Secret) -> Result<(), TransportError>;

    /// Close the endpoint for `id` and disconnect the service.
    async fn close(&self, id: &ServiceId) -> Result<(), TransportError>;

    /// Deliver `msg` to the inbox of `to`. Must not queue without bound:
    /// when the inbox is full, fail with [`TransportError::Busy`] or drop.
    async fn send(&self, to: &ServiceId, msg: &Envelope) -> Result<(), TransportError>;

    /// The stream of messages from all services. Can be taken once.
    fn inbound(&self) -> Option<BoxStream<'static, Inbound>>;

    /// The address a service process uses to connect (see [`connect`]).
    fn address(&self) -> String;
}

/// A service's side of a transport: its single link to the kernel.
#[async_trait]
pub trait Link: Send + Sync + 'static {
    /// Send to the kernel, which routes the message on.
    async fn send(&self, msg: &Envelope) -> Result<(), TransportError>;

    /// Next message from the kernel; `None` once the link is closed.
    async fn recv(&self) -> Option<Envelope>;
}

/// Connect a service to the kernel at `address` (`unix:<dir>` or `nats://…`).
pub async fn connect(address: &str, id: &ServiceId, secret: &Secret) -> Result<Box<dyn Link>, TransportError> {
    if let Some(dir) = address.strip_prefix("unix:") {
        return Ok(Box::new(unix::UnixLink::connect(dir.as_ref(), id, secret).await?));
    }
    #[cfg(feature = "nats")]
    if address.starts_with("nats://") || address.starts_with("tls://") {
        return Ok(Box::new(nats::NatsLink::connect(address, id, secret).await?));
    }
    Err(TransportError::Address(address.to_owned()))
}

/// Connect using the environment the supervisor provides.
pub async fn connect_from_env() -> Result<(ServiceId, Box<dyn Link>), TransportError> {
    let var = |k: &str| std::env::var(k).map_err(|_| TransportError::Other(format!("{k} is not set")));
    let id = ServiceId::new(var(ENV_SERVICE_ID)?).map_err(|e| TransportError::Other(e.to_string()))?;
    let secret = Secret::from_raw(var(ENV_SECRET)?);
    let link = connect(&var(ENV_ADDRESS)?, &id, &secret).await?;
    Ok((id, link))
}
