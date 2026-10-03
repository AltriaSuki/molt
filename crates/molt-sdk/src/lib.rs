//! Write Molt services.
//!
//! A [`Service`] wraps the service's single [`Link`] to the kernel. A
//! background task reads the link: replies are matched to the [`Service::call`]
//! waiting for them, and requests and events are queued for
//! [`Service::next`] (or [`Service::serve`]).
//!
//! ```no_run
//! # async fn demo() -> Result<(), molt_sdk::SdkError> {
//! let svc = molt_sdk::Service::connect_from_env().await?;
//! svc.serve(|req| async move { Ok(req.payload) }).await;
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, MsgId, RemoteError, ServiceId, Target, TraceId};
use molt_transport::{Link, TransportError};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Same as `molt_kernel::ENV_CAPS`; duplicated so services need not depend on the kernel.
pub const ENV_CAPS: &str = "MOLT_CAPS";

#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("no capability for {0}")]
    NoCapability(String),
    #[error("invalid target: {0}")]
    Target(String),
    #[error("the link to the kernel closed")]
    Closed,
    #[error("bad {ENV_CAPS}: {0}")]
    Env(String),
}

/// Options for one call. `Default` uses the service's own capability for the
/// target, a fresh trace and the kernel's default deadline.
#[derive(Clone, Debug, Default)]
pub struct CallOpts {
    pub cap: Option<CapId>,
    pub budget: Budget,
    pub trace: Option<TraceId>,
}

/// A request that has been sent and whose reply has not been awaited yet.
pub struct PendingReply {
    rx: oneshot::Receiver<Envelope>,
}

impl PendingReply {
    /// The reply payload, or the error the callee or the kernel sent back.
    pub async fn wait(self) -> Result<Value, SdkError> {
        let reply = self.rx.await.map_err(|_| SdkError::Closed)?;
        match reply.error() {
            Some(err) => Err(SdkError::Remote(err)),
            None => Ok(reply.payload),
        }
    }
}

type Waiters = Arc<Mutex<HashMap<MsgId, oneshot::Sender<Envelope>>>>;

pub struct Service {
    id: ServiceId,
    link: Arc<dyn Link>,
    caps: Mutex<HashMap<String, CapId>>,
    waiters: Waiters,
    incoming: tokio::sync::Mutex<mpsc::Receiver<Envelope>>,
    reader: JoinHandle<()>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Service {
    /// Connect with the address, id, secret and capabilities the supervisor
    /// put in the environment.
    pub async fn connect_from_env() -> Result<Self, SdkError> {
        let (id, link) = molt_transport::connect_from_env().await?;
        let caps = match std::env::var(ENV_CAPS) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| SdkError::Env(e.to_string()))?,
            Err(_) => HashMap::new(),
        };
        Ok(Self::new(id, link, caps))
    }

    pub fn new(id: ServiceId, link: Box<dyn Link>, caps: HashMap<String, CapId>) -> Self {
        let link: Arc<dyn Link> = Arc::from(link);
        let waiters: Waiters = Arc::default();
        let (tx, rx) = mpsc::channel(256);
        let reader = tokio::spawn(read_loop(link.clone(), waiters.clone(), tx));
        Self { id, link, caps: Mutex::new(caps), waiters, incoming: tokio::sync::Mutex::new(rx), reader }
    }

    pub fn id(&self) -> &ServiceId {
        &self.id
    }

    /// Remember a capability for `target` (e.g. one delegated to this service).
    pub fn add_cap(&self, target: &str, cap: CapId) {
        self.caps.lock().unwrap().insert(target.to_owned(), cap);
    }

    /// The capability this service holds for `target`, exact or `service.*`.
    pub fn cap_for(&self, target: &Target) -> Option<CapId> {
        let caps = self.caps.lock().unwrap();
        caps.get(&target.to_string()).cloned().or_else(|| {
            let service = target.service()?;
            caps.get(&format!("{service}.*")).cloned()
        })
    }

    /// Send a request and wait for its reply payload.
    pub async fn call(&self, to: &str, payload: Value, opts: CallOpts) -> Result<Value, SdkError> {
        self.request(to, payload, opts).await?.wait().await
    }

    /// Send a request now and return a handle to await its reply later.
    /// Requests sent one after another from one service reach the callee in
    /// that order, whenever their replies are awaited.
    pub async fn request(&self, to: &str, payload: Value, opts: CallOpts) -> Result<PendingReply, SdkError> {
        let target: Target = to.parse().map_err(|_| SdkError::Target(to.to_owned()))?;
        // Kernel methods authorize by their arguments, so they may go without a capability.
        let cap = match opts.cap {
            Some(c) => Some(c),
            None if target.service().is_some_and(|s| s.as_str() == molt_proto::KERNEL) => None,
            None => Some(self.cap_for(&target).ok_or_else(|| SdkError::NoCapability(to.to_owned()))?),
        };
        let mut msg =
            Envelope::request(opts.trace.unwrap_or_else(TraceId::random), target, CapId::from_raw(""), payload)
                .with_budget(opts.budget);
        msg.cap = cap;
        self.send_raw_request(msg).await
    }

    /// Send a fully built request and wait for the reply envelope.
    pub async fn send_request(&self, msg: Envelope) -> Result<Envelope, SdkError> {
        self.send_raw_request(msg).await?.rx.await.map_err(|_| SdkError::Closed)
    }

    async fn send_raw_request(&self, msg: Envelope) -> Result<PendingReply, SdkError> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap().insert(msg.id.clone(), tx);
        if let Err(e) = self.link.send(&msg).await {
            self.waiters.lock().unwrap().remove(&msg.id);
            return Err(e.into());
        }
        Ok(PendingReply { rx })
    }

    /// Call a kernel method (`ping`, `cap.delegate`, `subscribe`, `registry.*`).
    pub async fn kernel(&self, method: &str, cap: Option<CapId>, payload: Value) -> Result<Value, SdkError> {
        let opts = CallOpts { cap, ..Default::default() };
        self.call(&format!("{}.{method}", molt_proto::KERNEL), payload, opts).await
    }

    /// Give `to` a narrower slice of a capability this service holds.
    pub async fn delegate(
        &self,
        parent: &CapId,
        to: &ServiceId,
        target: &str,
        budget: Budget,
        ttl: Option<Duration>,
    ) -> Result<CapId, SdkError> {
        let args = json!({
            "parent": parent,
            "to": to,
            "target": target,
            "budget": budget,
            "ttl_ms": ttl.map(|d| d.as_millis() as u64),
        });
        let out = self.kernel("cap.delegate", None, args).await?;
        serde_json::from_value(out["cap"].clone()).map_err(|e| SdkError::Env(e.to_string()))
    }

    pub async fn subscribe(&self, topic: &str) -> Result<(), SdkError> {
        let target: Target = format!("topic:{topic}").parse().map_err(|_| SdkError::Target(topic.to_owned()))?;
        let cap = self.cap_for(&target).ok_or_else(|| SdkError::NoCapability(target.to_string()))?;
        self.kernel("subscribe", Some(cap), json!({ "topic": topic })).await.map(|_| ())
    }

    pub async fn publish(&self, topic: &str, payload: Value) -> Result<(), SdkError> {
        let target: Target = format!("topic:{topic}").parse().map_err(|_| SdkError::Target(topic.to_owned()))?;
        let cap = self.cap_for(&target).ok_or_else(|| SdkError::NoCapability(target.to_string()))?;
        let msg =
            Envelope::event(TraceId::random(), topic, cap, payload).map_err(|e| SdkError::Target(e.to_string()))?;
        Ok(self.link.send(&msg).await?)
    }

    /// Next request or event addressed to this service.
    pub async fn next(&self) -> Option<Envelope> {
        self.incoming.lock().await.recv().await
    }

    pub async fn reply(&self, req: &Envelope, payload: Value) -> Result<(), SdkError> {
        Ok(self.link.send(&req.reply(payload)).await?)
    }

    pub async fn reply_error(&self, req: &Envelope, code: ErrorCode, message: &str) -> Result<(), SdkError> {
        Ok(self.link.send(&req.error_reply(code, message)).await?)
    }

    /// Handle requests one at a time until the link closes. Events are passed
    /// to the handler too; their result is discarded.
    pub async fn serve<F, Fut>(&self, handler: F)
    where
        F: Fn(Envelope) -> Fut,
        Fut: Future<Output = Result<Value, RemoteError>>,
    {
        while let Some(msg) = self.next().await {
            let result = handler(msg.clone()).await;
            self.answer(&msg, result).await;
        }
    }

    /// Handle up to `max_in_flight` requests at once until the link closes.
    /// Each runs in its own task, so replies may go out in a different order
    /// than the requests came in. A handler that panics answers `failed`.
    /// Events are passed to the handler too; their result is discarded.
    pub async fn serve_concurrent<F, Fut>(self: &Arc<Self>, max_in_flight: usize, handler: F)
    where
        F: Fn(Envelope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, RemoteError>> + Send + 'static,
    {
        let slots = Arc::new(tokio::sync::Semaphore::new(max_in_flight.max(1)));
        let handler = Arc::new(handler);
        let mut tasks = tokio::task::JoinSet::new();
        while let Some(msg) = self.next().await {
            let Ok(slot) = slots.clone().acquire_owned().await else { break };
            let (svc, handler) = (self.clone(), handler.clone());
            tasks.spawn(async move {
                let _slot = slot;
                let work = tokio::spawn(handler(msg.clone()));
                let result = match work.await {
                    Ok(r) => r,
                    Err(e) => Err(RemoteError { code: ErrorCode::Failed, message: format!("the handler failed: {e}") }),
                };
                svc.answer(&msg, result).await;
            });
            while tasks.try_join_next().is_some() {}
        }
    }

    /// Send the reply for a handled message; events get none.
    async fn answer(&self, msg: &Envelope, result: Result<Value, RemoteError>) {
        if msg.kind != Kind::Request {
            return;
        }
        let sent = match result {
            Ok(v) => self.reply(msg, v).await,
            Err(e) => self.reply_error(msg, e.code, &e.message).await,
        };
        if let Err(e) = sent {
            tracing::warn!(error = %e, "could not send a reply");
        }
    }
}

async fn read_loop(link: Arc<dyn Link>, waiters: Waiters, incoming: mpsc::Sender<Envelope>) {
    while let Some(msg) = link.recv().await {
        if msg.kind == Kind::Reply {
            let waiter = msg.reply_to.as_ref().and_then(|id| waiters.lock().unwrap().remove(id));
            match waiter {
                Some(w) => {
                    let _ = w.send(msg);
                }
                None => tracing::debug!(reply_to = ?msg.reply_to, "reply with no waiting call"),
            }
        } else if incoming.send(msg).await.is_err() {
            break;
        }
    }
    // The link closed: wake every waiting call with an error.
    waiters.lock().unwrap().clear();
}
