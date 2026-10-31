#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use molt_kernel::{Config, Kernel};
use molt_proto::{Budget, ErrorCode, ServiceId, Target};
use molt_sdk::{CallOpts, SdkError, Service};
use molt_transport::unix::{UnixLink, UnixTransport};

pub struct World {
    pub dir: tempfile::TempDir,
    pub sock: PathBuf,
    pub kernel: Kernel,
}

pub fn sid(s: &str) -> ServiceId {
    ServiceId::new(s).unwrap()
}

pub fn target(s: &str) -> Target {
    s.parse().unwrap()
}

impl World {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sock");
        let transport = Arc::new(UnixTransport::new(&sock).unwrap());
        let mut config = Config::new(dir.path().join("data"));
        config.fsync = false;
        config.default_deadline = Duration::from_secs(5);
        let kernel = Kernel::start(config, transport).await.unwrap();
        Self { dir, sock, kernel }
    }

    /// Register a service and connect to it as that service.
    pub async fn join(&self, name: &str) -> Service {
        let id = sid(name);
        let secret = self.kernel.register(&id).await.unwrap();
        let link = UnixLink::connect(&self.sock, &id, &secret).await.unwrap();
        let svc = Service::new(id, Box::new(link), HashMap::new());
        svc.kernel("ping", None, serde_json::Value::Null).await.expect("ping the kernel");
        svc
    }

    pub async fn grant(&self, holder: &Service, t: &str, budget: Budget) -> molt_proto::CapId {
        let cap = self.kernel.grant(holder.id(), target(t), budget, None).await.unwrap();
        holder.add_cap(t, cap.clone());
        cap
    }
}

pub fn with_cap(cap: &molt_proto::CapId) -> CallOpts {
    CallOpts { cap: Some(cap.clone()), ..Default::default() }
}

/// The error code of a failed call; panics if the call succeeded.
pub fn code(r: Result<serde_json::Value, SdkError>) -> ErrorCode {
    match r {
        Err(SdkError::Remote(e)) => e.code,
        other => panic!("expected a remote error, got {other:?}"),
    }
}

/// Answer every request by echoing its payload, until the service is dropped.
pub fn echo(svc: Service) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { svc.serve(|req| async move { Ok(req.payload) }).await })
}

/// A backend for the kernel's own rules on confined services, which only
/// says whether it is available: it confines nothing, so it starts nothing
/// (the services it is used for have no `exec`).
#[derive(Default)]
pub struct FakeBackend {
    pub unavailable: std::sync::atomic::AtomicBool,
}

impl molt_kernel::supervisor::Backend for FakeBackend {
    fn name(&self) -> &str {
        "fake"
    }

    fn probe(&self) -> Result<(), String> {
        match self.unavailable.load(std::sync::atomic::Ordering::SeqCst) {
            true => Err("no namespaces here".into()),
            false => Ok(()),
        }
    }

    fn command(
        &self,
        _: &molt_kernel::supervisor::ConfinedStart<'_>,
    ) -> Result<(tokio::process::Command, Box<dyn molt_kernel::supervisor::Contained>), String> {
        Err("the fake backend starts nothing".into())
    }
}

impl World {
    /// Make the live version of `name` run confined by `backend`.
    pub async fn confine(&self, name: &str, backend: &Arc<FakeBackend>) {
        let restart = molt_kernel::supervisor::RestartPolicy::default();
        let backend: Arc<dyn molt_kernel::supervisor::Backend> = backend.clone();
        self.kernel.launch_confined(&sid(name), None, Default::default(), restart, Vec::new(), backend).await.unwrap();
    }
}
