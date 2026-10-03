//! NATS subjects guarded by per-service server permissions, for many hosts.
//!
//! Subjects, with the default prefix `molt`:
//! - `molt.in.<service>`: the service's endpoint. Only that service may
//!   publish here and only the kernel may subscribe.
//! - `molt.svc.<service>.inbox`: the service's inbox. Only the kernel may
//!   publish here and only that service may subscribe.
//!
//! The NATS server enforces those rules through its `authorization` block;
//! [`server_config`] generates it. Each service logs in with its id as the
//! user name and its [`Secret`] as the password. The kernel trusts the
//! subject a message arrived on, never the envelope, for the sender identity.
//!
//! Core NATS keeps per-publisher, per-subject order and delivers at most
//! once, which is exactly the guarantee the bus promises.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use async_nats::{Client, ConnectOptions, Subscriber};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use molt_proto::{Envelope, ServiceId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{Inbound, Link, Secret, Transport, TransportError};

pub const DEFAULT_PREFIX: &str = "molt";
const INBOUND_CAPACITY: usize = 1024;

fn endpoint_subject(prefix: &str, id: &ServiceId) -> String {
    format!("{prefix}.in.{id}")
}

fn inbox_subject(prefix: &str, id: &ServiceId) -> String {
    format!("{prefix}.svc.{id}.inbox")
}

fn other(e: impl std::fmt::Display) -> TransportError {
    TransportError::Other(e.to_string())
}

/// Generate the `authorization` block of a nats-server config that enforces
/// the bus rules for the kernel and each listed service.
pub fn server_config(prefix: &str, kernel_password: &Secret, services: &[(ServiceId, Secret)]) -> String {
    let mut out = String::from("authorization {\n  users = [\n");
    let _ = writeln!(
        out,
        "    {{ user: \"kernel\", password: \"{}\", permissions: {{ publish: [\"{prefix}.svc.*.inbox\"], subscribe: [\"{prefix}.in.*\"] }} }}",
        kernel_password.expose()
    );
    for (id, secret) in services {
        let _ = writeln!(
            out,
            "    {{ user: \"{id}\", password: \"{}\", permissions: {{ publish: [\"{}\"], subscribe: [\"{}\"] }} }}",
            secret.expose(),
            endpoint_subject(prefix, id),
            inbox_subject(prefix, id),
        );
    }
    out.push_str("  ]\n}\n");
    out
}

pub struct NatsTransport {
    url: String,
    prefix: String,
    client: Client,
    subscriptions: Mutex<HashMap<ServiceId, JoinHandle<()>>>,
    inbound_tx: mpsc::Sender<Inbound>,
    inbound_rx: Mutex<Option<mpsc::Receiver<Inbound>>>,
}

impl NatsTransport {
    /// Connect as the `kernel` user.
    pub async fn connect(url: &str, kernel_password: &Secret, prefix: &str) -> Result<Self, TransportError> {
        let client = ConnectOptions::with_user_and_password("kernel".into(), kernel_password.expose().into())
            .connect(url)
            .await
            .map_err(other)?;
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        Ok(Self {
            url: url.to_owned(),
            prefix: prefix.to_owned(),
            client,
            subscriptions: Mutex::default(),
            inbound_tx,
            inbound_rx: Mutex::new(Some(inbound_rx)),
        })
    }
}

#[async_trait]
impl Transport for NatsTransport {
    /// Subscribes to the service's endpoint subject. The secret is checked by
    /// the NATS server, which must list it (see [`server_config`]).
    async fn open(&self, id: &ServiceId, _secret: &Secret) -> Result<(), TransportError> {
        self.close(id).await?;
        let mut sub = self.client.subscribe(endpoint_subject(&self.prefix, id)).await.map_err(other)?;
        self.client.flush().await.map_err(other)?;
        let (id2, tx) = (id.clone(), self.inbound_tx.clone());
        let task = tokio::spawn(async move {
            while let Some(m) = sub.next().await {
                match serde_json::from_slice::<Envelope>(&m.payload) {
                    Ok(msg) => {
                        if tx.send(Inbound { from: id2.clone(), msg }).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(service = %id2, error = %e, "dropped a malformed message")
                    }
                }
            }
        });
        self.subscriptions.lock().unwrap().insert(id.clone(), task);
        Ok(())
    }

    async fn close(&self, id: &ServiceId) -> Result<(), TransportError> {
        if let Some(task) = self.subscriptions.lock().unwrap().remove(id) {
            task.abort();
        }
        Ok(())
    }

    async fn send(&self, to: &ServiceId, msg: &Envelope) -> Result<(), TransportError> {
        let body = serde_json::to_vec(msg)?;
        self.client.publish(inbox_subject(&self.prefix, to), body.into()).await.map_err(other)
    }

    fn inbound(&self) -> Option<BoxStream<'static, Inbound>> {
        let rx = self.inbound_rx.lock().unwrap().take()?;
        Some(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) }).boxed())
    }

    fn address(&self) -> String {
        self.url.clone()
    }
}

/// A service's NATS connection: publish to its endpoint, read its inbox.
pub struct NatsLink {
    client: Client,
    endpoint: String,
    inbox: tokio::sync::Mutex<Subscriber>,
}

impl NatsLink {
    pub async fn connect(url: &str, id: &ServiceId, secret: &Secret) -> Result<Self, TransportError> {
        Self::connect_with_prefix(url, id, secret, DEFAULT_PREFIX).await
    }

    pub async fn connect_with_prefix(
        url: &str,
        id: &ServiceId,
        secret: &Secret,
        prefix: &str,
    ) -> Result<Self, TransportError> {
        let client = ConnectOptions::with_user_and_password(id.to_string(), secret.expose().into())
            .connect(url)
            .await
            .map_err(|_| TransportError::Auth)?;
        let inbox = client.subscribe(inbox_subject(prefix, id)).await.map_err(other)?;
        client.flush().await.map_err(other)?;
        Ok(Self { client, endpoint: endpoint_subject(prefix, id), inbox: tokio::sync::Mutex::new(inbox) })
    }

    /// Publish raw bytes to any subject. Only for tests that prove the server
    /// refuses subjects outside the service's permissions.
    #[doc(hidden)]
    pub async fn publish_raw(&self, subject: String, body: Vec<u8>) -> Result<(), TransportError> {
        self.client.publish(subject, body.into()).await.map_err(other)?;
        self.client.flush().await.map_err(other)
    }

    #[doc(hidden)]
    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[async_trait]
impl Link for NatsLink {
    async fn send(&self, msg: &Envelope) -> Result<(), TransportError> {
        let body = serde_json::to_vec(msg)?;
        self.client.publish(self.endpoint.clone(), body.into()).await.map_err(other)
    }

    async fn recv(&self) -> Option<Envelope> {
        let mut inbox = self.inbox.lock().await;
        loop {
            let m = inbox.next().await?;
            match serde_json::from_slice(&m.payload) {
                Ok(msg) => return Some(msg),
                Err(e) => tracing::warn!(error = %e, "dropped a malformed message from the kernel"),
            }
        }
    }
}
