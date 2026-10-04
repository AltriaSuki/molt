//! The dispatcher that ties the kernel parts together.
//!
//! Every message from a service flows through [`Inner::dispatch`], one at a
//! time and in arrival order:
//!
//! 1. stamp the sender with the identity the transport authenticated;
//! 2. verify the capability and charge its budget;
//! 3. submit the message to the audit log;
//! 4. hand it to the receiver's bounded mailbox, whose delivery task waits for
//!    the audit entry to be durable before writing it to the transport.
//!
//! So nothing is delivered that is not already on the record, and messages
//! between any two services keep their order. Replies are routed by the
//! request they answer, which only the original callee may reply to.
//!
//! The kernel's own methods (`kernel.*`) are answered by the kernel. A read
//! of the audit log (`kernel.audit.read`) runs off the dispatcher, so a long
//! scan holds up no other message, and its reply is recorded as the range of
//! entries it returned rather than copied into the log a second time.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use futures::StreamExt;
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, Manifest, MsgId, ServiceId, Target, VersionId};
use molt_transport::{Inbound, Secret, Transport, TransportError};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::audit::{self, AuditError, AuditEvent, AuditLog, Receipt};
use crate::caps::CapTable;
use crate::registry::{Authority, Registry, RegistryError};
use crate::supervisor::{self, Limits, RestartPolicy, Supervisor};

/// Environment variable holding the service's capabilities as a JSON object
/// of `target -> cap id`.
pub const ENV_CAPS: &str = "MOLT_CAPS";

/// Environment variable holding the registry version the service was
/// launched as, which it records as the author of what it writes.
pub const ENV_VERSION: &str = "MOLT_VERSION";

/// How long a reply waits for space in a full mailbox before it is dropped.
const REPLY_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    /// Holds `audit.jsonl` and `registry/`.
    pub data_dir: std::path::PathBuf,
    /// Sync every audit batch to disk. Turn off only in tests.
    pub fsync: bool,
    /// Reply deadline for requests that do not set `budget.ms`.
    pub default_deadline: Duration,
    /// Messages the kernel queues per service before senders get `Busy`.
    pub mailbox: usize,
}

impl Config {
    pub fn new(data_dir: impl Into<std::path::PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), fsync: true, default_deadline: Duration::from_secs(30), mailbox: 256 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error(transparent)]
    Audit(#[from] AuditError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("{0} is not registered")]
    UnknownService(ServiceId),
    #[error("{0} has no live version")]
    NoLiveVersion(ServiceId),
    #[error("the transport's inbound stream was already taken")]
    InboundTaken,
}

/// What the supervisor has reported about a service since it was last
/// launched, for a caller that waits for it to come up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceStatus {
    /// How its last run ended, e.g. `exit status: 1` or `spawn failed: ...`.
    pub last_exit: Option<String>,
    /// Set once the supervisor stopped restarting it: the restarts it made.
    pub gave_up: Option<u32>,
}

struct Pending {
    requester: ServiceId,
    callee: ServiceId,
    deadline: Instant,
    request: Envelope,
}

/// A service's bounded queue and the task draining it into the transport.
type Mailbox = (mpsc::Sender<Delivery>, JoinHandle<()>);

struct Delivery {
    receipt: Receipt,
    msg: Envelope,
}

struct Inner {
    transport: Arc<dyn Transport>,
    caps: CapTable,
    audit: AuditLog,
    registry: Registry,
    supervisor: Supervisor,
    config: Config,
    me: ServiceId,
    services: Mutex<HashMap<ServiceId, Secret>>,
    mailboxes: Mutex<HashMap<ServiceId, Mailbox>>,
    pending: Mutex<HashMap<MsgId, Pending>>,
    topics: Mutex<HashMap<String, BTreeSet<ServiceId>>>,
    status: Mutex<HashMap<ServiceId, ServiceStatus>>,
    /// Handed to delivery tasks so they do not keep the kernel alive.
    weak: Weak<Inner>,
}

/// A running kernel. Dropping it stops the dispatcher; call
/// [`Kernel::shutdown`] to also stop supervised services and finish writing
/// the audit log.
pub struct Kernel {
    inner: Arc<Inner>,
    tasks: Vec<JoinHandle<()>>,
    /// Logs the supervisor's events. Shutdown drains it, so the services'
    /// last exits are on the record.
    events: Option<(oneshot::Sender<()>, JoinHandle<()>)>,
}

/// What a launched service was given.
#[derive(Debug, Clone)]
pub struct Launched {
    pub version: VersionId,
    pub caps: HashMap<String, CapId>,
}

impl Kernel {
    pub async fn start(config: Config, transport: Arc<dyn Transport>) -> Result<Self, KernelError> {
        let audit = AuditLog::open(config.data_dir.join("audit.jsonl"), config.fsync).await?;
        let registry = Registry::open(config.data_dir.join("registry"))?;
        let inbound = transport.inbound().ok_or(KernelError::InboundTaken)?;
        let (sup_tx, sup_rx) = mpsc::channel(256);
        let inner = Arc::new_cyclic(|weak| Inner {
            weak: weak.clone(),
            transport,
            caps: CapTable::new(),
            audit,
            registry,
            supervisor: Supervisor::new(sup_tx),
            config,
            me: ServiceId::kernel(),
            services: Mutex::default(),
            mailboxes: Mutex::default(),
            pending: Mutex::default(),
            topics: Mutex::default(),
            status: Mutex::default(),
        });
        let tasks = vec![tokio::spawn(dispatcher(inner.clone(), inbound)), tokio::spawn(reaper(inner.clone()))];
        let (close_events, closing) = oneshot::channel();
        let events = tokio::spawn(supervisor_events(inner.clone(), sup_rx, closing));
        Ok(Self { inner, tasks, events: Some((close_events, events)) })
    }

    /// Open an endpoint for `id` with a fresh secret and return the secret.
    pub async fn register(&self, id: &ServiceId) -> Result<Secret, KernelError> {
        let secret = Secret::random();
        self.register_with_secret(id, secret.clone()).await?;
        Ok(secret)
    }

    /// Open an endpoint for `id` with a known secret (NATS deployments
    /// provision secrets in the server config ahead of time).
    pub async fn register_with_secret(&self, id: &ServiceId, secret: Secret) -> Result<(), KernelError> {
        self.inner.register(id, secret).await
    }

    /// Grant a capability from nothing. This is the human/configuration path;
    /// services can only delegate what they already hold.
    pub async fn grant(
        &self,
        holder: &ServiceId,
        target: Target,
        budget: Budget,
        ttl: Option<Duration>,
    ) -> Result<CapId, KernelError> {
        let cap = self.inner.caps.grant(holder.clone(), target.clone(), budget, ttl);
        self.inner
            .audit
            .append(AuditEvent::CapGranted {
                cap: cap.id.clone(),
                holder: holder.clone(),
                target,
                budget,
                parent: None,
            })
            .await?;
        Ok(cap.id.clone())
    }

    pub async fn revoke(&self, cap: &CapId) -> Result<bool, KernelError> {
        let found = self.inner.caps.revoke(cap);
        if found {
            self.inner.audit.append(AuditEvent::CapRevoked { cap: cap.clone() }).await?;
        }
        Ok(found)
    }

    /// Store a manifest and make it live, as a human.
    pub async fn install(&self, manifest: Manifest) -> Result<VersionId, KernelError> {
        let service = manifest.name.clone();
        let version = self.inner.registry.store(manifest)?;
        self.inner
            .audit
            .append(AuditEvent::VersionStored {
                service: service.clone(),
                version: version.clone(),
                by: "human".into(),
            })
            .await?;
        let live = self.inner.registry.live(&service);
        if live.as_ref() != Some(&version) {
            self.promote(&service, live.as_ref(), &version, &Authority::Human).await?;
        }
        Ok(version)
    }

    pub async fn promote(
        &self,
        service: &ServiceId,
        expected: Option<&VersionId>,
        version: &VersionId,
        by: &Authority,
    ) -> Result<Option<VersionId>, KernelError> {
        let previous = self.inner.registry.promote(service, expected, version, by)?;
        self.inner
            .audit
            .append(AuditEvent::VersionPromoted {
                service: service.clone(),
                from: previous.clone(),
                to: version.clone(),
                by: by.to_string(),
            })
            .await?;
        Ok(previous)
    }

    /// Start the live version of `service` under the supervisor: open its
    /// endpoint, grant the capabilities its manifest requests, and pass the
    /// address, secret and capabilities to the process environment.
    pub async fn launch(
        &self,
        service: &ServiceId,
        secret: Option<Secret>,
        limits: Limits,
        restart: RestartPolicy,
    ) -> Result<Launched, KernelError> {
        self.launch_with_env(service, secret, limits, restart, Vec::new()).await
    }

    /// [`Kernel::launch`] with extra environment variables for the process,
    /// such as an API key only this service may hold. They cannot override
    /// the bus address, identity, secret or capabilities.
    pub async fn launch_with_env(
        &self,
        service: &ServiceId,
        secret: Option<Secret>,
        limits: Limits,
        restart: RestartPolicy,
        extra_env: Vec<(String, String)>,
    ) -> Result<Launched, KernelError> {
        let version = self.inner.registry.live(service).ok_or_else(|| KernelError::NoLiveVersion(service.clone()))?;
        let manifest =
            self.inner.registry.manifest(&version).ok_or_else(|| KernelError::NoLiveVersion(service.clone()))?;
        let secret = secret.unwrap_or_else(Secret::random);
        self.inner.register(service, secret.clone()).await?;
        for old in self.inner.caps.revoke_held_by(service) {
            self.inner.audit.append(AuditEvent::CapRevoked { cap: old }).await?;
        }
        let mut caps = HashMap::new();
        for req in &manifest.requests {
            let cap = self.grant(service, req.target.clone(), req.budget, None).await?;
            caps.insert(req.target.to_string(), cap);
        }
        if let Some(exec) = manifest.exec.clone() {
            let mut env = extra_env;
            env.extend([
                (molt_transport::ENV_ADDRESS.to_owned(), self.inner.transport.address()),
                (molt_transport::ENV_SERVICE_ID.to_owned(), service.to_string()),
                (molt_transport::ENV_SECRET.to_owned(), secret.expose().to_owned()),
                (ENV_CAPS.to_owned(), serde_json::to_string(&caps).unwrap()),
                (ENV_VERSION.to_owned(), version.to_string()),
            ]);
            self.inner.status.lock().unwrap().remove(service);
            self.inner.supervisor.start(supervisor::Spec { id: service.clone(), exec, env, limits, restart });
        }
        Ok(Launched { version, caps })
    }

    pub async fn stop(&self, service: &ServiceId) {
        self.inner.supervisor.stop(service).await;
    }

    /// What the supervisor has reported about `service` since its last
    /// launch. The audit log has the whole history.
    pub fn service_status(&self, service: &ServiceId) -> ServiceStatus {
        self.inner.status.lock().unwrap().get(service).cloned().unwrap_or_default()
    }

    pub fn caps(&self) -> &CapTable {
        &self.inner.caps
    }

    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    pub fn audit_path(&self) -> &std::path::Path {
        self.inner.audit.path()
    }

    /// The address services connect to (see `molt_transport::connect`), for
    /// a client that joins the bus from inside this process.
    pub fn address(&self) -> String {
        self.inner.transport.address()
    }

    /// Requests still waiting for a reply.
    pub fn pending_count(&self) -> usize {
        self.inner.pending.lock().unwrap().len()
    }

    /// Stop every service, then the kernel. When it returns, everything the
    /// kernel logged, the services' exits included, is written and the audit
    /// writer has stopped.
    pub async fn shutdown(mut self) {
        self.inner.supervisor.stop_all().await;
        for t in &self.tasks {
            t.abort();
        }
        if let Some((close, events)) = self.events.take() {
            let _ = close.send(());
            let _ = events.await;
        }
        self.inner.audit.close().await;
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        for t in self.tasks.iter().chain(self.events.as_ref().map(|(_, t)| t)) {
            t.abort();
        }
        for (_, (_, task)) in self.inner.mailboxes.lock().unwrap().drain() {
            task.abort();
        }
    }
}

async fn dispatcher(inner: Arc<Inner>, mut inbound: BoxStream<'static, Inbound>) {
    while let Some(Inbound { from, mut msg }) = inbound.next().await {
        msg.from = Some(from.clone());
        inner.dispatch(from, msg).await;
    }
}

async fn reaper(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    loop {
        tick.tick().await;
        let now = Instant::now();
        let expired: Vec<MsgId> =
            inner.pending.lock().unwrap().iter().filter(|(_, p)| p.deadline <= now).map(|(id, _)| id.clone()).collect();
        for id in expired {
            inner.fail_pending(&id, ErrorCode::Timeout, "no reply before the deadline").await;
        }
    }
}

/// Log the supervisor's events. Once `closing` fires, log the ones already
/// queued and stop.
async fn supervisor_events(
    inner: Arc<Inner>,
    mut rx: mpsc::Receiver<supervisor::Event>,
    mut closing: oneshot::Receiver<()>,
) {
    let mut open = true;
    loop {
        let ev = tokio::select! {
            ev = rx.recv() => ev,
            _ = &mut closing, if open => {
                // recv() still hands over what is queued, then returns None.
                rx.close();
                open = false;
                continue;
            }
        };
        let Some(ev) = ev else { return };
        let event = match ev {
            supervisor::Event::Started { service, pid } => AuditEvent::ServiceStarted { service, pid },
            supervisor::Event::Exited { service, status } => {
                let waiting: Vec<MsgId> = inner
                    .pending
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, p)| p.callee == service)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in waiting {
                    inner.fail_pending(&id, ErrorCode::Unavailable, "the service exited").await;
                }
                inner.status.lock().unwrap().entry(service.clone()).or_default().last_exit = Some(status.clone());
                AuditEvent::ServiceExited { service, status }
            }
            supervisor::Event::GaveUp { service, restarts } => {
                inner.status.lock().unwrap().entry(service.clone()).or_default().gave_up = Some(restarts);
                AuditEvent::ServiceGaveUp { service, restarts }
            }
        };
        if inner.audit.append(event).await.is_err() {
            return;
        }
    }
}

async fn deliver_loop(inner: Weak<Inner>, to: ServiceId, mut rx: mpsc::Receiver<Delivery>) {
    while let Some(Delivery { receipt, msg }) = rx.recv().await {
        // Nothing is delivered before it is on the record.
        if receipt.durable().await.is_err() {
            return;
        }
        let Some(inner) = inner.upgrade() else { return };
        let give_up = Instant::now() + Duration::from_secs(5);
        loop {
            match inner.transport.send(&to, &msg).await {
                Ok(()) => break,
                Err(TransportError::Busy(_)) if Instant::now() < give_up => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(e) => {
                    tracing::debug!(service = %to, error = %e, "delivery failed");
                    if msg.kind == Kind::Request {
                        inner.fail_pending(&msg.id, ErrorCode::Unavailable, &e.to_string()).await;
                    }
                    break;
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct DelegateArgs {
    parent: CapId,
    to: ServiceId,
    target: Target,
    budget: Budget,
    #[serde(default)]
    ttl_ms: Option<u64>,
}

#[derive(Deserialize)]
struct SubscribeArgs {
    topic: String,
}

#[derive(Deserialize)]
struct ProposeArgs {
    manifest: Manifest,
}

#[derive(Deserialize)]
struct PromoteArgs {
    service: ServiceId,
    #[serde(default)]
    expected: Option<VersionId>,
    version: VersionId,
}

impl Inner {
    async fn register(&self, id: &ServiceId, secret: Secret) -> Result<(), KernelError> {
        self.transport.open(id, &secret).await?;
        self.services.lock().unwrap().insert(id.clone(), secret);
        {
            let mut boxes = self.mailboxes.lock().unwrap();
            if !boxes.contains_key(id) {
                let (tx, rx) = mpsc::channel(self.config.mailbox);
                let task = tokio::spawn(deliver_loop(self.weak.clone(), id.clone(), rx));
                boxes.insert(id.clone(), (tx, task));
            }
        }
        self.audit
            .append(AuditEvent::ServiceRegistered { service: id.clone(), version: self.registry.live(id) })
            .await?;
        Ok(())
    }

    async fn dispatch(&self, from: ServiceId, msg: Envelope) {
        match msg.kind {
            Kind::Request => self.on_request(from, msg).await,
            Kind::Event => self.on_event(from, msg).await,
            Kind::Reply => self.on_reply(from, msg).await,
        }
    }

    async fn deny(&self, from: &ServiceId, msg: &Envelope, code: ErrorCode, reason: String) {
        let denied = AuditEvent::Denied {
            from: from.clone(),
            msg: msg.id.clone(),
            to: msg.to.clone(),
            code,
            reason: reason.clone(),
        };
        if self.audit.submit(denied).await.is_err() {
            return;
        }
        if msg.kind != Kind::Reply {
            let mut reply = msg.error_reply(code, reason);
            reply.from = Some(self.me.clone());
            let _ = self.route(from, reply).await;
        }
    }

    /// Log `msg` and queue it for `to`. A full mailbox refuses requests and
    /// events with `Busy` (and nothing is logged). Replies are never refused:
    /// a caller is waiting on them, so they wait for mailbox space instead.
    async fn route(&self, to: &ServiceId, msg: Envelope) -> Result<(), ErrorCode> {
        self.route_as(to, msg, None).await
    }

    /// [`Inner::route`], logging `record` in place of the message when given.
    async fn route_as(&self, to: &ServiceId, msg: Envelope, record: Option<AuditEvent>) -> Result<(), ErrorCode> {
        let record = record.unwrap_or_else(|| AuditEvent::Message { envelope: msg.clone() });
        let tx = self.mailboxes.lock().unwrap().get(to).map(|(tx, _)| tx.clone()).ok_or(ErrorCode::Unavailable)?;
        let permit = match tx.try_reserve() {
            Ok(p) => p,
            Err(mpsc::error::TrySendError::Full(())) if msg.kind == Kind::Reply => {
                let (audit, to, tx) = (self.audit.clone(), to.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(REPLY_WAIT, tx.reserve_owned()).await {
                        Ok(Ok(permit)) => {
                            if let Ok(receipt) = audit.submit(record).await {
                                permit.send(Delivery { receipt, msg });
                            }
                        }
                        _ => {
                            tracing::warn!(service = %to, "dropped a reply: the mailbox stayed full")
                        }
                    }
                });
                return Ok(());
            }
            Err(mpsc::error::TrySendError::Full(())) => return Err(ErrorCode::Busy),
            Err(mpsc::error::TrySendError::Closed(())) => return Err(ErrorCode::Unavailable),
        };
        let receipt = self.audit.submit(record).await.map_err(|_| ErrorCode::Unavailable)?;
        permit.send(Delivery { receipt, msg });
        Ok(())
    }

    async fn fail_pending(&self, id: &MsgId, code: ErrorCode, reason: &str) {
        let Some(p) = self.pending.lock().unwrap().remove(id) else {
            return;
        };
        let mut reply = p.request.error_reply(code, reason);
        reply.from = Some(self.me.clone());
        let _ = self.route(&p.requester, reply).await;
    }

    async fn on_request(&self, from: ServiceId, msg: Envelope) {
        let Target::Method { service, method } = &msg.to else {
            return self.deny(&from, &msg, ErrorCode::Invalid, "a request must target service.method".into()).await;
        };
        if *service == self.me {
            let method = method.clone();
            return self.kernel_call(from, &method, msg).await;
        }
        let cap = match self.caps.verify(msg.cap.as_ref(), &from, &msg.to) {
            Ok(c) => c,
            Err(e) => return self.deny(&from, &msg, ErrorCode::Denied, e.to_string()).await,
        };
        if cap.max_ms != 0 && msg.budget.ms > cap.max_ms {
            return self
                .deny(&from, &msg, ErrorCode::Denied, "deadline is longer than the capability allows".into())
                .await;
        }
        if !self.services.lock().unwrap().contains_key(service) {
            return self.deny(&from, &msg, ErrorCode::Unavailable, format!("{service} is not registered")).await;
        }
        if let Err(e) = self.caps.charge(&cap, msg.budget.tokens, 1) {
            return self.deny(&from, &msg, ErrorCode::OverBudget, e.to_string()).await;
        }
        let mut ms = if msg.budget.ms == 0 { self.config.default_deadline.as_millis() as u64 } else { msg.budget.ms };
        if cap.max_ms != 0 {
            ms = ms.min(cap.max_ms);
        }
        let callee = service.clone();
        self.pending.lock().unwrap().insert(
            msg.id.clone(),
            Pending {
                requester: from.clone(),
                callee: callee.clone(),
                deadline: Instant::now() + Duration::from_millis(ms),
                request: msg.clone(),
            },
        );
        if let Err(code) = self.route(&callee, msg.clone()).await {
            self.pending.lock().unwrap().remove(&msg.id);
            self.caps.refund(&cap, msg.budget.tokens, 1);
            self.deny(&from, &msg, code, format!("could not queue for {callee}")).await;
        }
    }

    async fn on_event(&self, from: ServiceId, msg: Envelope) {
        let Target::Topic { name } = &msg.to else {
            return self.deny(&from, &msg, ErrorCode::Invalid, "an event must target topic:<name>".into()).await;
        };
        let cap = match self.caps.verify(msg.cap.as_ref(), &from, &msg.to) {
            Ok(c) => c,
            Err(e) => return self.deny(&from, &msg, ErrorCode::Denied, e.to_string()).await,
        };
        if let Err(e) = self.caps.charge(&cap, msg.budget.tokens, 1) {
            return self.deny(&from, &msg, ErrorCode::OverBudget, e.to_string()).await;
        }
        let subscribers: Vec<ServiceId> = self
            .topics
            .lock()
            .unwrap()
            .get(name)
            .map(|s| s.iter().filter(|s| **s != from).cloned().collect())
            .unwrap_or_default();
        for sub in subscribers {
            // Events are at most once: a full mailbox drops the event for that subscriber.
            let _ = self.route(&sub, msg.clone()).await;
        }
    }

    async fn on_reply(&self, from: ServiceId, msg: Envelope) {
        let Some(answers) = msg.reply_to.clone() else {
            return self.deny(&from, &msg, ErrorCode::Invalid, "a reply must name reply_to".into()).await;
        };
        let pending = {
            let mut pending = self.pending.lock().unwrap();
            match pending.get(&answers) {
                Some(p) if p.callee == from => pending.remove(&answers),
                _ => None,
            }
        };
        match pending {
            Some(p) => {
                let _ = self.route(&p.requester, msg).await;
            }
            None => {
                self.deny(&from, &msg, ErrorCode::Denied, "no request of yours is waiting for this reply".into()).await
            }
        }
    }

    async fn kernel_call(&self, from: ServiceId, method: &str, msg: Envelope) {
        if self.audit.submit(AuditEvent::Message { envelope: msg.clone() }).await.is_err() {
            return;
        }
        if method == "audit.read" {
            return self.audit_read(from, msg).await;
        }
        let result = self.kernel_method(&from, method, &msg).await;
        match result {
            Ok(payload) => {
                let mut reply = msg.reply(payload);
                reply.from = Some(self.me.clone());
                let _ = self.route(&from, reply).await;
            }
            Err((code, reason)) => self.deny(&from, &msg, code, reason).await,
        }
    }

    /// Check the capability and arguments of a `kernel.audit.read`, then
    /// read the page and reply from a task of its own.
    async fn audit_read(&self, from: ServiceId, msg: Envelope) {
        let cap = match self.caps.verify(msg.cap.as_ref(), &from, &msg.to) {
            Ok(c) => c,
            Err(e) => return self.deny(&from, &msg, ErrorCode::Denied, e.to_string()).await,
        };
        let req: molt_proto::audit::ReadRequest = match serde_json::from_value(msg.payload.clone()) {
            Ok(r) => r,
            Err(e) => return self.deny(&from, &msg, ErrorCode::Invalid, e.to_string()).await,
        };
        if let Err(e) = self.caps.charge(&cap, 0, 1) {
            return self.deny(&from, &msg, ErrorCode::OverBudget, e.to_string()).await;
        }
        let (weak, path) = (self.weak.clone(), self.audit.path().to_owned());
        tokio::spawn(async move {
            let read =
                tokio::task::spawn_blocking(move || audit::read_trace(&path, &req).map(|page| (req, page))).await;
            let Some(inner) = weak.upgrade() else { return };
            let (req, page) = match read {
                Ok(Ok(done)) => done,
                Ok(Err(e @ AuditError::Cursor(_))) => {
                    return inner.deny(&from, &msg, ErrorCode::Invalid, e.to_string()).await;
                }
                Ok(Err(e)) => return inner.deny(&from, &msg, ErrorCode::Failed, e.to_string()).await,
                Err(e) => return inner.deny(&from, &msg, ErrorCode::Failed, format!("the read failed: {e}")).await,
            };
            let payload = match serde_json::to_value(&page) {
                Ok(p) => p,
                Err(e) => return inner.deny(&from, &msg, ErrorCode::Failed, e.to_string()).await,
            };
            let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(payload.to_string().as_bytes()));
            let mut reply = msg.reply(payload);
            reply.from = Some(inner.me.clone());
            let record = AuditEvent::AuditRead {
                by: from.clone(),
                reply: reply.id.clone(),
                reply_to: msg.id.clone(),
                trace: req.trace,
                entries: page.entries.len() as u64,
                first_seq: page.entries.first().map(|e| e.seq),
                last_seq: page.entries.last().map(|e| e.seq),
                sha256: digest,
            };
            let _ = inner.route_as(&from, reply, Some(record)).await;
        });
    }

    async fn kernel_method(
        &self,
        from: &ServiceId,
        method: &str,
        msg: &Envelope,
    ) -> Result<Value, (ErrorCode, String)> {
        let invalid = |e: serde_json::Error| (ErrorCode::Invalid, e.to_string());
        let denied = |e: &dyn std::fmt::Display| (ErrorCode::Denied, e.to_string());
        match method {
            "ping" => Ok(json!({ "pong": true })),
            "cap.delegate" => {
                // Authority comes from holding the parent capability, checked by delegate().
                let a: DelegateArgs = serde_json::from_value(msg.payload.clone()).map_err(invalid)?;
                if !self.services.lock().unwrap().contains_key(&a.to) {
                    return Err((ErrorCode::Invalid, format!("{} is not registered", a.to)));
                }
                let cap = self
                    .caps
                    .delegate(
                        &a.parent,
                        from,
                        a.to.clone(),
                        a.target.clone(),
                        a.budget,
                        a.ttl_ms.map(Duration::from_millis),
                    )
                    .map_err(|e| denied(&e))?;
                let granted = AuditEvent::CapGranted {
                    cap: cap.id.clone(),
                    holder: a.to,
                    target: a.target,
                    budget: a.budget,
                    parent: Some(a.parent),
                };
                self.audit.submit(granted).await.map_err(|e| denied(&e))?;
                Ok(json!({ "cap": cap.id }))
            }
            "subscribe" => {
                let a: SubscribeArgs = serde_json::from_value(msg.payload.clone()).map_err(invalid)?;
                let topic = Target::Topic { name: a.topic.clone() };
                self.caps.verify(msg.cap.as_ref(), from, &topic).map_err(|e| denied(&e))?;
                self.topics.lock().unwrap().entry(a.topic).or_default().insert(from.clone());
                Ok(json!({ "subscribed": true }))
            }
            "registry.propose" => {
                self.caps.verify(msg.cap.as_ref(), from, &msg.to).map_err(|e| denied(&e))?;
                let a: ProposeArgs = serde_json::from_value(msg.payload.clone()).map_err(invalid)?;
                let service = a.manifest.name.clone();
                let version = self.registry.store(a.manifest).map_err(|e| denied(&e))?;
                let stored =
                    AuditEvent::VersionStored { service, version: version.clone(), by: format!("gate:{from}") };
                self.audit.submit(stored).await.map_err(|e| denied(&e))?;
                Ok(json!({ "version": version }))
            }
            "registry.promote" => {
                self.caps.verify(msg.cap.as_ref(), from, &msg.to).map_err(|e| denied(&e))?;
                let a: PromoteArgs = serde_json::from_value(msg.payload.clone()).map_err(invalid)?;
                let by = Authority::Gate(from.clone());
                let previous =
                    self.registry.promote(&a.service, a.expected.as_ref(), &a.version, &by).map_err(|e| denied(&e))?;
                let promoted = AuditEvent::VersionPromoted {
                    service: a.service,
                    from: previous.clone(),
                    to: a.version,
                    by: by.to_string(),
                };
                self.audit.submit(promoted).await.map_err(|e| denied(&e))?;
                Ok(json!({ "previous": previous }))
            }
            other => Err((ErrorCode::Invalid, format!("the kernel has no method {other:?}"))),
        }
    }
}
