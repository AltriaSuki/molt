//! The `fs` and `shell` services.
//!
//! Both confine every request to their [`Roots`]: a workspace must be a
//! directory inside `root` or `scratch`, and every path inside a request must
//! stay inside its workspace. Nothing inside a `.molt` directory may be used,
//! since Molt's data directory may live in `root`; only forks may sit below
//! one, when `scratch` does. Forks are created in `scratch`.

mod error;
mod files;
mod fork;
mod paths;
mod shell;
mod walk;

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use molt_proto::{Envelope, RemoteError, Target};
use molt_sdk::Service;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::signal::unix::{signal, SignalKind};

use crate::error::{failed, invalid};

/// Where the services may work.
#[derive(Clone, Debug)]
pub struct Roots {
    /// Workspaces the user works in live under here.
    pub root: PathBuf,
    /// Forks are created here. Keep it outside `root`'s projects, so tools
    /// that search parent directories (cargo, git) do not find the original.
    /// It must be private to this user: it is created with mode 0700, and a
    /// symlink, another user's directory or one others can write to is refused.
    pub scratch: PathBuf,
}

/// Serves `fs.*`.
pub struct Fs {
    roots: Arc<Roots>,
    /// Held by merge and drop, so two merges into one workspace cannot
    /// interleave their conflict checks and writes.
    forks: Arc<Mutex<()>>,
}

impl Fs {
    /// Canonicalizes the roots; `scratch` is created if missing, and checked.
    pub fn new(roots: Roots) -> anyhow::Result<Self> {
        Ok(Self { roots: Arc::new(roots.canonical()?), forks: Arc::default() })
    }

    /// Handle one `fs` method (`read`, `write`, ...) with its request payload.
    pub async fn handle(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        let method = method.strip_prefix("fs.").unwrap_or(method);
        let roots = self.roots.clone();
        let forks = self.forks.clone();
        match method {
            "read" => blocking(method, payload, move |req| files::read(&roots, req)).await,
            "write" => blocking(method, payload, move |req| files::write(&roots, req)).await,
            "edit" => blocking(method, payload, move |req| files::edit(&roots, req)).await,
            "list" => blocking(method, payload, move |req| files::list(&roots, req)).await,
            "search" => blocking(method, payload, move |req| files::search(&roots, req)).await,
            "fork" => blocking(method, payload, move |req| fork::fork(&roots, req)).await,
            "diff" => blocking(method, payload, move |req| fork::diff(&roots, req)).await,
            "merge" => blocking(method, payload, move |req| fork::merge(&roots, &forks, req)).await,
            "drop" => blocking(method, payload, move |req| fork::drop_fork(&roots, &forks, req)).await,
            _ => Err(invalid(format!("unknown method fs.{method}"))),
        }
    }
}

/// Serves `shell.*`.
pub struct Shell {
    roots: Arc<Roots>,
    /// `bash`, or `sh` when there is no bash on `PATH`.
    program: PathBuf,
    running: Arc<shell::Groups>,
}

impl Shell {
    /// Canonicalizes the roots (`scratch` is created if missing, and checked) and finds the shell to run commands with.
    pub fn new(roots: Roots) -> anyhow::Result<Self> {
        Ok(Self { roots: Arc::new(roots.canonical()?), program: shell::find_shell(), running: Arc::default() })
    }

    /// Kill every command still running, with everything it started. Call
    /// it before the service exits: each command has a process group of its
    /// own, which neither a signal to the service nor its exit reaches.
    pub fn kill_all(&self) {
        self.running.kill_all();
    }

    /// Handle one `shell` method (`run`) with its request payload.
    pub async fn handle(&self, method: &str, payload: Value) -> Result<Value, RemoteError> {
        let method = method.strip_prefix("shell.").unwrap_or(method);
        match method {
            "run" => {
                let req = parse("shell", method, payload)?;
                let reply = shell::run(&self.roots, &self.program, &self.running, req).await?;
                serde_json::to_value(reply).map_err(|e| failed(e.to_string()))
            }
            _ => Err(invalid(format!("unknown method shell.{method}"))),
        }
    }
}

/// Resolves on the first signal that should stop the shell service, which
/// must then [`Shell::kill_all`]: SIGTERM (the supervisor), SIGINT (Ctrl-C),
/// SIGHUP (the terminal closed) or SIGQUIT (`Ctrl-\`). Each of them would
/// end the process at once otherwise. Listening starts when this is called.
pub fn stop_signal() -> std::io::Result<impl Future<Output = ()>> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut quit = signal(SignalKind::quit())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = hup.recv() => {}
            _ = quit.recv() => {}
        }
    })
}

/// Serve `fs.*` on `svc` until its link closes.
pub async fn serve_fs(svc: Arc<Service>, fs: Arc<Fs>) {
    svc.serve_concurrent(16, move |req| {
        let fs = fs.clone();
        async move { fs.handle(&method_of(&req)?, req.payload).await }
    })
    .await
}

/// Serve `shell.*` on `svc` until its link closes.
pub async fn serve_shell(svc: Arc<Service>, shell: Arc<Shell>) {
    svc.serve_concurrent(8, move |req| {
        let shell = shell.clone();
        async move { shell.handle(&method_of(&req)?, req.payload).await }
    })
    .await
}

fn method_of(req: &Envelope) -> Result<String, RemoteError> {
    match &req.to {
        Target::Method { method, .. } => Ok(method.clone()),
        other => Err(invalid(format!("{other} is not a method"))),
    }
}

fn parse<T: DeserializeOwned>(service: &str, method: &str, payload: Value) -> Result<T, RemoteError> {
    serde_json::from_value(payload).map_err(|e| invalid(format!("bad {service}.{method} request: {e}")))
}

/// Decode the request, run `work` on the blocking pool and encode its reply.
async fn blocking<Req, Rep>(
    method: &str,
    payload: Value,
    work: impl FnOnce(Req) -> Result<Rep, RemoteError> + Send + 'static,
) -> Result<Value, RemoteError>
where
    Req: DeserializeOwned + Send + 'static,
    Rep: Serialize + Send + 'static,
{
    let req = parse("fs", method, payload)?;
    let reply = tokio::task::spawn_blocking(move || work(req))
        .await
        .map_err(|e| failed(format!("fs.{method} failed: {e}")))??;
    serde_json::to_value(reply).map_err(|e| failed(e.to_string()))
}
