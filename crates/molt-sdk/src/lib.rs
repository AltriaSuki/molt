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
use std::time::{Duration, Instant};

use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, MsgId, RemoteError, ServiceId, Target, TraceId, MAX_DEPTH};
use molt_transport::{Link, TransportError};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Same as `molt_kernel::ENV_CAPS`; duplicated so services need not depend on the kernel.
pub const ENV_CAPS: &str = "MOLT_CAPS";

/// Same as `molt_kernel::ENV_VERSION`.
pub const ENV_VERSION: &str = "MOLT_VERSION";

/// Requests and events a service holds for [`Service::next`] before more
/// requests are answered `busy` and more events are dropped.
pub const QUEUE: usize = 256;

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
    id: MsgId,
    waiters: Waiters,
    /// When to stop waiting: a while after the request's deadline, by which
    /// the kernel answers every request it still knows of. A reply lost on
    /// the way (one the transport refused, say) ends the wait here.
    give_up: Option<Instant>,
}

/// How long past a request's deadline a caller still waits for the kernel's answer.
#[cfg(not(test))]
const REPLY_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const REPLY_GRACE: Duration = Duration::from_millis(50);

impl PendingReply {
    /// The reply payload, or the error the callee or the kernel sent back.
    pub async fn wait(self) -> Result<Value, SdkError> {
        let reply = match self.give_up {
            None => self.rx.await,
            Some(at) => match tokio::time::timeout_at(at.into(), self.rx).await {
                Ok(reply) => reply,
                Err(_) => {
                    self.waiters.lock().unwrap().remove(&self.id);
                    let message = "no reply arrived by the deadline; it was lost on the way".to_owned();
                    return Err(SdkError::Remote(RemoteError { code: ErrorCode::Timeout, message }));
                }
            },
        };
        let reply = reply.map_err(|_| SdkError::Closed)?;
        match reply.error() {
            Some(err) => Err(SdkError::Remote(err)),
            None => Ok(reply.payload),
        }
    }
}

type Waiters = Arc<Mutex<HashMap<MsgId, oneshot::Sender<Envelope>>>>;

pub struct Service {
    id: ServiceId,
    /// The registry version the kernel launched this service as.
    version: Option<String>,
    link: Arc<dyn Link>,
    caps: Mutex<HashMap<String, CapId>>,
    waiters: Waiters,
    /// Requests and events, with when each arrived.
    incoming: tokio::sync::Mutex<mpsc::Receiver<(Envelope, Instant)>>,
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
        let mut svc = Self::new(id, link, caps);
        svc.version = std::env::var(ENV_VERSION).ok().filter(|v| !v.is_empty());
        Ok(svc)
    }

    pub fn new(id: ServiceId, link: Box<dyn Link>, caps: HashMap<String, CapId>) -> Self {
        let link: Arc<dyn Link> = Arc::from(link);
        let waiters: Waiters = Arc::default();
        let (tx, rx) = mpsc::channel(QUEUE);
        let reader = tokio::spawn(read_loop(link.clone(), waiters.clone(), tx));
        Self { id, version: None, link, caps: Mutex::new(caps), waiters, incoming: tokio::sync::Mutex::new(rx), reader }
    }

    pub fn id(&self) -> &ServiceId {
        &self.id
    }

    /// The registry version this service was launched as, when the kernel
    /// launched it (see [`ENV_VERSION`]).
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Set the version [`Service::version`] reports, for a service joined
    /// to the bus some other way than [`Service::connect_from_env`].
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
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
        let give_up = (msg.budget.ms > 0).then(|| Instant::now() + Duration::from_millis(msg.budget.ms) + REPLY_GRACE);
        Ok(PendingReply { rx, id: msg.id, waiters: self.waiters.clone(), give_up })
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

    /// Next request or event addressed to this service. Up to [`QUEUE`] wait
    /// here; a request that finds the queue full is answered `busy`, and an
    /// event is dropped. The time a request waited here is taken off its
    /// `budget.ms`, so a handler sees what is left of its caller's deadline;
    /// one whose deadline passed while it waited is answered `timeout`
    /// instead of returned.
    pub async fn next(&self) -> Option<Envelope> {
        let mut incoming = self.incoming.lock().await;
        loop {
            let (mut msg, arrived) = incoming.recv().await?;
            if msg.kind != Kind::Request || msg.budget.ms == 0 {
                return Some(msg);
            }
            let waited = u64::try_from(arrived.elapsed().as_millis()).unwrap_or(u64::MAX);
            if waited < msg.budget.ms {
                msg.budget.ms -= waited;
                return Some(msg);
            }
            let reason = format!("the request waited {waited} ms for the service, past its deadline");
            if let Err(e) = self.reply_error(&msg, ErrorCode::Timeout, &reason).await {
                tracing::debug!(error = %e, "could not answer a request that timed out in the queue");
            }
        }
    }

    /// Answer `req`. A payload nested too deeply for the kernel to take is
    /// refused here, so the caller can be told instead of left waiting.
    pub async fn reply(&self, req: &Envelope, payload: Value) -> Result<(), SdkError> {
        // The envelope and its payload field add one level.
        let deep = molt_proto::depth(&payload) + 1;
        if deep > MAX_DEPTH {
            let e = format!("the reply is nested {deep} deep; the bus carries at most {MAX_DEPTH}");
            return Err(SdkError::Transport(TransportError::Other(e)));
        }
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
    /// than the requests came in. More requests wait in the service's queue
    /// until a slot is free, and that wait counts against their deadlines
    /// (see [`Service::next`]). A handler that panics, even before its future
    /// starts, answers `failed`. Events are passed to the handler too; their
    /// result is discarded.
    pub async fn serve_concurrent<F, Fut>(self: &Arc<Self>, max_in_flight: usize, handler: F)
    where
        F: Fn(Envelope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, RemoteError>> + Send + 'static,
    {
        let slots = Arc::new(tokio::sync::Semaphore::new(max_in_flight.max(1)));
        let handler = Arc::new(handler);
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let Ok(slot) = slots.clone().acquire_owned().await else { break };
            let Some(msg) = self.next().await else { break };
            let (svc, handler) = (self.clone(), handler.clone());
            tasks.spawn(async move {
                let _slot = slot;
                let request = msg.clone();
                let work = tokio::spawn(async move { handler(request).await });
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
            // A reply the transport refuses (too big, say) would leave the
            // caller waiting out its whole deadline; a short error reaches it.
            Ok(v) => match self.reply(msg, v).await {
                Err(e) => {
                    tracing::warn!(error = %e, "could not send a reply; sending an error instead");
                    self.reply_error(msg, ErrorCode::Failed, &format!("the reply could not be sent: {e}")).await
                }
                sent => sent,
            },
            Err(e) => self.reply_error(msg, e.code, &e.message).await,
        };
        if let Err(e) = sent {
            tracing::warn!(error = %e, "could not send a reply");
        }
    }
}

/// Route replies to their calls and queue everything else for [`Service::next`].
/// It never waits for queue space: a request handler waiting on a call would
/// then never see its reply, which arrives behind the queued requests.
async fn read_loop(link: Arc<dyn Link>, waiters: Waiters, incoming: mpsc::Sender<(Envelope, Instant)>) {
    while let Some(msg) = link.recv().await {
        if msg.kind == Kind::Reply {
            let waiter = msg.reply_to.as_ref().and_then(|id| waiters.lock().unwrap().remove(id));
            match waiter {
                Some(w) => {
                    let _ = w.send(msg);
                }
                None => tracing::debug!(reply_to = ?msg.reply_to, "reply with no waiting call"),
            }
            continue;
        }
        match incoming.try_send((msg, Instant::now())) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full((msg, _))) if msg.kind == Kind::Request => {
                let busy = msg
                    .error_reply(ErrorCode::Busy, format!("the service is busy: {QUEUE} requests are already waiting"));
                if let Err(e) = link.send(&busy).await {
                    tracing::warn!(error = %e, "could not answer a request the queue had no room for");
                }
            }
            Err(mpsc::error::TrySendError::Full((msg, _))) => {
                tracing::debug!(to = %msg.to, "dropped an event: the queue is full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
    // The link closed: wake every waiting call with an error.
    waiters.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;

    /// A link that refuses to send envelopes over `max` bytes, as a transport does.
    struct SmallLink {
        max: usize,
        inbox: tokio::sync::Mutex<mpsc::Receiver<Envelope>>,
        sent: mpsc::Sender<Envelope>,
    }

    #[async_trait]
    impl Link for SmallLink {
        async fn send(&self, msg: &Envelope) -> Result<(), TransportError> {
            let size = serde_json::to_vec(msg)?.len();
            if size > self.max {
                return Err(TransportError::Other(format!("{size} bytes is too big")));
            }
            self.sent.send(msg.clone()).await.map_err(|_| TransportError::Closed)
        }

        async fn recv(&self) -> Option<Envelope> {
            self.inbox.lock().await.recv().await
        }
    }

    #[tokio::test]
    async fn a_reply_too_deep_for_the_bus_becomes_an_error_reply() {
        let (to_service, inbox) = mpsc::channel(4);
        let (sent, mut from_service) = mpsc::channel(4);
        let link = SmallLink { max: usize::MAX, inbox: tokio::sync::Mutex::new(inbox), sent };
        let svc = Service::new(ServiceId::new("svc").unwrap(), Box::new(link), HashMap::new());
        let req = Envelope::request(TraceId::random(), "svc.deep".parse().unwrap(), CapId::random(), Value::Null);
        to_service.send(req.clone()).await.unwrap();
        drop(to_service);
        let deep: Value =
            serde_json::from_str(&format!("{}1{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH))).unwrap();
        svc.serve(move |_| {
            let deep = deep.clone();
            async move { Ok(deep) }
        })
        .await;
        drop(svc);
        let err = from_service.recv().await.expect("a reply").error().expect("an error reply");
        assert!(err.message.contains("nested"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_call_whose_reply_is_lost_ends_after_its_deadline() {
        let (_to_service, inbox) = mpsc::channel(4);
        let (sent, _from_service) = mpsc::channel(4);
        let link = SmallLink { max: usize::MAX, inbox: tokio::sync::Mutex::new(inbox), sent };
        let svc = Service::new(ServiceId::new("svc").unwrap(), Box::new(link), HashMap::new());
        let opts = CallOpts { budget: Budget::new(0, 20, 0), ..Default::default() };
        let started = Instant::now();
        let err = svc.call("kernel.audit.read", json!({}), opts).await.unwrap_err();
        let SdkError::Remote(err) = err else { panic!("{err:?}") };
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(started.elapsed() >= Duration::from_millis(70));
        assert!(svc.waiters.lock().unwrap().is_empty(), "the call is forgotten");
    }

    #[tokio::test]
    async fn a_reply_too_big_to_send_becomes_an_error_reply() {
        let (to_service, inbox) = mpsc::channel(4);
        let (sent, mut from_service) = mpsc::channel(4);
        let link = SmallLink { max: 1024, inbox: tokio::sync::Mutex::new(inbox), sent };
        let svc = Service::new(ServiceId::new("svc").unwrap(), Box::new(link), HashMap::new());
        let req = Envelope::request(TraceId::random(), "svc.big".parse().unwrap(), CapId::random(), Value::Null);
        to_service.send(req.clone()).await.unwrap();
        drop(to_service);
        svc.serve(|_| async { Ok(json!("x".repeat(4096))) }).await;
        drop(svc);

        let reply = from_service.recv().await.expect("a reply");
        assert_eq!(reply.reply_to.as_ref(), Some(&req.id));
        let err = reply.error().expect("an error reply");
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(err.message.contains("too big"), "{}", err.message);
    }
}
