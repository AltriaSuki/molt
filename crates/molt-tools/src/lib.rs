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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use molt_api::fs::{self as api, FilesChanged};
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
        self.handle_reporting(method, payload).await.0
    }

    /// [`Fs::handle`], and the files the call changed in a workspace that is
    /// not a fork, for [`api::CHANGED`]. A merge that fails partway still
    /// reports the files it wrote.
    pub async fn handle_reporting(
        &self,
        method: &str,
        payload: Value,
    ) -> (Result<Value, RemoteError>, Option<FilesChanged>) {
        let method = method.strip_prefix("fs.").unwrap_or(method);
        if method == "merge" {
            return self.merge(payload).await;
        }
        match self.handle_other(method, payload).await {
            Ok((reply, changed)) => (Ok(reply), changed),
            Err(e) => (Err(e), None),
        }
    }

    async fn merge(&self, payload: Value) -> (Result<Value, RemoteError>, Option<FilesChanged>) {
        let req: api::MergeRequest = match parse("fs", "merge", payload) {
            Ok(r) => r,
            Err(e) => return (Err(e), None),
        };
        let (roots, forks) = (self.roots.clone(), self.forks.clone());
        let joined = tokio::task::spawn_blocking(move || {
            let base = fork::base(&roots, &req.fork);
            let mut written = Vec::new();
            let result = fork::merge(&roots, &forks, req, &mut written);
            let changed = base
                .filter(|base| !base.starts_with(&roots.scratch) && !written.is_empty())
                .map(|base| FilesChanged { workspace: base.to_string_lossy().into_owned(), paths: written });
            (result, changed)
        })
        .await;
        match joined {
            Ok((result, changed)) => {
                (result.and_then(|r| serde_json::to_value(r).map_err(|e| failed(e.to_string()))), changed)
            }
            Err(e) => (Err(failed(format!("fs.merge failed: {e}"))), None),
        }
    }

    async fn handle_other(&self, method: &str, payload: Value) -> Result<(Value, Option<FilesChanged>), RemoteError> {
        let roots = self.roots.clone();
        let forks = self.forks.clone();
        match method {
            "read" => blocking(method, payload, move |req| files::read(&roots, req).map(quiet)).await,
            "write" => {
                blocking(method, payload, move |req: api::WriteRequest| {
                    let (workspace, path) = (req.workspace.clone(), req.path.clone());
                    let reply = files::write(&roots, req)?;
                    Ok((reply, files::changed(&roots, &workspace, &path)))
                })
                .await
            }
            "edit" => {
                blocking(method, payload, move |req: api::EditRequest| {
                    let (workspace, path) = (req.workspace.clone(), req.path.clone());
                    let reply = files::edit(&roots, req)?;
                    Ok((reply, files::changed(&roots, &workspace, &path)))
                })
                .await
            }
            "list" => blocking(method, payload, move |req| files::list(&roots, req).map(quiet)).await,
            "search" => blocking(method, payload, move |req| files::search(&roots, req).map(quiet)).await,
            "fork" => blocking(method, payload, move |req| fork::fork(&roots, req).map(quiet)).await,
            "diff" => blocking(method, payload, move |req| fork::diff(&roots, req).map(quiet)).await,
            "drop" => blocking(method, payload, move |req| fork::drop_fork(&roots, &forks, req).map(quiet)).await,
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

/// The regular files of the canonical workspace directory `ws`, as absolute
/// paths, by the rules list, search and fork follow (see the `walk` module):
/// ignored files, `.git` and `.molt` are left out, and symlinks are neither
/// followed nor returned. Directories `left_out` names (given their absolute
/// path) are not entered either. Entries that cannot be read are skipped.
pub fn workspace_files<F>(ws: &Path, left_out: F) -> impl Iterator<Item = PathBuf>
where
    F: Fn(&Path) -> bool + Send + Sync + 'static,
{
    walk::files(ws, left_out).map(ignore::DirEntry::into_path)
}

/// True when `path` is one of [`workspace_files`]`(ws, left_out)`. Cheaper
/// than a walk of the whole workspace, since only the directories on the
/// way to `path` are read.
pub fn is_workspace_file<F>(ws: &Path, path: &Path, left_out: F) -> bool
where
    F: Fn(&Path) -> bool + Send + Sync + 'static,
{
    walk::reaches_file(ws, path, left_out)
}

/// Serve `fs.*` on `svc` until its link closes. When `svc` holds a
/// capability for `topic:fs.changed`, the files each write, edit and merge
/// changed outside the forks are published there.
pub async fn serve_fs(svc: Arc<Service>, fs: Arc<Fs>) {
    let topic: Target = format!("topic:{}", api::CHANGED).parse().expect("a valid topic");
    let report = svc.cap_for(&topic).is_some();
    let publisher = svc.clone();
    svc.serve_concurrent(16, move |req| {
        let (fs, publisher) = (fs.clone(), publisher.clone());
        async move {
            let (reply, changed) = fs.handle_reporting(&method_of(&req)?, req.payload).await;
            if let Some(changed) = changed.filter(|_| report) {
                match serde_json::to_value(changed) {
                    Ok(event) => {
                        if let Err(e) = publisher.publish(api::CHANGED, event).await {
                            tracing::debug!(error = %e, "could not publish changed files");
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "could not encode changed files"),
                }
            }
            reply
        }
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

/// A reply that changed no files.
fn quiet<T>(reply: T) -> (T, Option<FilesChanged>) {
    (reply, None)
}

/// Decode the request, run `work` on the blocking pool and encode its reply.
async fn blocking<Req, Rep>(
    method: &str,
    payload: Value,
    work: impl FnOnce(Req) -> Result<(Rep, Option<FilesChanged>), RemoteError> + Send + 'static,
) -> Result<(Value, Option<FilesChanged>), RemoteError>
where
    Req: DeserializeOwned + Send + 'static,
    Rep: Serialize + Send + 'static,
{
    let req = parse("fs", method, payload)?;
    let (reply, changed) = tokio::task::spawn_blocking(move || work(req))
        .await
        .map_err(|e| failed(format!("fs.{method} failed: {e}")))??;
    Ok((serde_json::to_value(reply).map_err(|e| failed(e.to_string()))?, changed))
}
