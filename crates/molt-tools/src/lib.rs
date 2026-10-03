//! The `fs` and `shell` services.
//!
//! Both confine every request to their [`Roots`]: a workspace must be a
//! directory inside `root` or `scratch`, and every path inside a request must
//! stay inside its workspace. Forks are created in `scratch`.

use std::path::PathBuf;
use std::sync::Arc;

use molt_proto::RemoteError;
use molt_sdk::Service;
use serde_json::Value;

/// Where the services may work.
#[derive(Clone, Debug)]
pub struct Roots {
    /// Workspaces the user works in live under here.
    pub root: PathBuf,
    /// Forks are created here. Keep it outside `root`'s projects, so tools
    /// that search parent directories (cargo, git) do not find the original.
    pub scratch: PathBuf,
}

/// Serves `fs.*`.
pub struct Fs {
    _roots: Roots,
}

impl Fs {
    /// Canonicalizes the roots; `scratch` is created if missing.
    pub fn new(roots: Roots) -> anyhow::Result<Self> {
        let _ = roots;
        todo!()
    }

    /// Handle one `fs` method (`read`, `write`, ...) with its request payload.
    pub async fn handle(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        let _ = (method, payload);
        todo!()
    }
}

/// Serves `shell.*`.
pub struct Shell {
    _roots: Roots,
}

impl Shell {
    pub fn new(roots: Roots) -> anyhow::Result<Self> {
        let _ = roots;
        todo!()
    }

    /// Handle one `shell` method (`run`) with its request payload.
    pub async fn handle(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        let _ = (method, payload);
        todo!()
    }
}

/// Serve `fs.*` on `svc` until its link closes.
pub async fn serve_fs(svc: Arc<Service>, fs: Arc<Fs>) {
    let _ = (svc, fs);
    todo!()
}

/// Serve `shell.*` on `svc` until its link closes.
pub async fn serve_shell(svc: Arc<Service>, shell: Arc<Shell>) {
    let _ = (svc, shell);
    todo!()
}
