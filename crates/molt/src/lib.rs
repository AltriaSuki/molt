//! The `molt` daemon: reads `molt.toml`, starts the kernel on the chosen
//! transport, installs the configured services and supervises them. The
//! [`agent`] module runs one task through them for `molt do`.

pub mod acp;
pub mod agent;
pub mod agent_skills;
pub mod config;
pub mod confine;
pub mod doctor;
pub mod extensions;
pub mod git;
pub mod improve;
pub mod layers;
mod lock;
pub mod prepare;
pub mod project;
pub mod prompts;
pub mod protocol;
pub mod readiness;
pub mod runs;
pub mod serve;
pub mod session;
pub mod sessions;
pub mod tui;

use std::future::Future;
use std::sync::Arc;

use anyhow::{bail, Context};
use molt_kernel::supervisor::{Limits, RestartPolicy};
use molt_kernel::{Config as KernelConfig, Kernel};
use molt_transport::{Secret, Transport};
use tokio::signal::unix::{signal, SignalKind};

pub use config::{Config, Isolation, Secrets, ServiceSection, TransportKind};
pub use lock::{DataDirLock, LOCK_FILE};

/// The longest path a Unix socket address holds (`sun_path` less its NUL).
pub(crate) const MAX_SOCKET_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// Log to stderr, filtered by `RUST_LOG` or else `default` (e.g. `info`).
pub fn init_tracing(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

/// Resolves to the number of the first signal that should stop molt and the
/// services it started: SIGINT (Ctrl-C), SIGTERM (`kill`, `timeout`, a
/// service manager) or SIGHUP (the terminal closed). Each would end molt at
/// once otherwise, and on NATS its services would live on. Listening starts
/// when this is called.
pub fn stop_signal() -> std::io::Result<impl Future<Output = i32>> {
    let (int, term, hup) = (SignalKind::interrupt(), SignalKind::terminate(), SignalKind::hangup());
    let (mut on_int, mut on_term, mut on_hup) = (signal(int)?, signal(term)?, signal(hup)?);
    Ok(async move {
        let kind = tokio::select! {
            _ = on_int.recv() => int,
            _ = on_term.recv() => term,
            _ = on_hup.recv() => hup,
        };
        kind.as_raw_value()
    })
}

/// A kernel started by [`start`], its services, and the lock on its data dir.
pub struct Running {
    kernel: Kernel,
    secrets: Option<Secrets>,
    _lock: DataDirLock,
}

impl Running {
    pub fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// The NATS credentials, when the kernel runs on NATS.
    pub fn secrets(&self) -> Option<&Secrets> {
        self.secrets.as_ref()
    }

    /// Stop every service and the kernel, then release the data dir.
    pub async fn shutdown(self) {
        self.kernel.shutdown().await;
    }
}

/// Lock the data dir, start the kernel and launch every configured service.
pub async fn start(cfg: &Config) -> anyhow::Result<Running> {
    if cfg.kernel.transport == TransportKind::Unix {
        check_socket_paths(cfg)?;
    }
    let lock = DataDirLock::acquire(&cfg.kernel.data_dir)?;
    let mut kcfg = KernelConfig::new(&cfg.kernel.data_dir);
    kcfg.fsync = cfg.kernel.fsync;
    let (transport, secrets): (Arc<dyn Transport>, _) = match cfg.kernel.transport {
        TransportKind::Unix => (Arc::new(molt_transport::unix::UnixTransport::new(cfg.socket_dir())?), None),
        TransportKind::Nats => {
            let secrets = Secrets::load(&cfg.secrets_path()).context(
                "NATS needs pre-provisioned secrets: run `molt nats-config` and restart nats-server with its output",
            )?;
            let transport = molt_transport::nats::NatsTransport::connect(
                &cfg.kernel.nats_url,
                &secrets.kernel,
                molt_transport::nats::DEFAULT_PREFIX,
            )
            .await?;
            (Arc::new(transport), Some(secrets))
        }
    };
    let kernel = Kernel::start(kcfg, transport).await?;
    if let Err(e) = launch_all(&kernel, cfg, secrets.as_ref()).await {
        kernel.shutdown().await;
        return Err(e);
    }
    Ok(Running { kernel, secrets, _lock: lock })
}

/// Refuse a data dir whose socket paths would be too long, before anything
/// is created in it.
fn check_socket_paths(cfg: &Config) -> anyhow::Result<()> {
    let names = cfg.services.iter().map(|s| s.name.as_str()).chain([agent::CLI]);
    let Some(longest) = names.max_by_key(|name| name.len()) else { return Ok(()) };
    let path = cfg.socket_dir().join(format!("{longest}.sock"));
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        bail!(
            "the socket path {} would be {len} bytes, more than the {MAX_SOCKET_PATH} Unix sockets allow; \
             use a data dir with a shorter path (--data-dir for molt do)",
            path.display()
        );
    }
    Ok(())
}

async fn launch_all(kernel: &Kernel, cfg: &Config, secrets: Option<&Secrets>) -> anyhow::Result<()> {
    let mut cfg = cfg.clone();
    mcp_tools(&mut cfg.services).await;
    let cfg = &cfg;
    // The planner learns the services' tools from the same manifests the kernel installs.
    let tools = cfg.provided_tools()?;
    for svc in &cfg.services {
        let mut manifest = svc.manifest()?;
        if svc.isolation == Isolation::Sandbox {
            // Looked up here, as the user's configuration names it: the sandbox has no PATH to search.
            if let Some(exec) = manifest.exec.as_mut() {
                exec.command = resolve(&exec.command)?;
            }
        }
        kernel.install(manifest).await?;
        let secret: Option<Secret> = match secrets {
            Some(s) => Some(
                s.services
                    .get(svc.name.as_str())
                    .cloned()
                    .with_context(|| format!("no NATS secret for {}; rerun `molt nats-config`", svc.name))?,
            ),
            None => None,
        };
        let limits = Limits { memory_bytes: svc.memory_mb.map(|m| m * 1024 * 1024), cpu_secs: svc.cpu_secs };
        let restart = RestartPolicy { max_restarts: svc.max_restarts, ..RestartPolicy::default() };
        let mut env = svc.launch_env();
        if svc.name.as_str() == "planner" && !tools.is_empty() {
            env.push((molt_planner::TOOLS_ENV.into(), serde_json::to_string(&tools)?));
        }
        match svc.isolation {
            Isolation::Native => {
                kernel.launch_with_env(&svc.name, secret, limits, restart, env).await?;
            }
            // Never started any other way: an unavailable sandbox stops molt here.
            Isolation::Sandbox => {
                let backend = Arc::new(confine::LinuxSandbox::reading(mounts(svc, &cfg.kernel.data_dir)?));
                kernel.launch_confined(&svc.name, secret, limits, restart, env, backend).await?;
            }
        }
        tracing::info!(service = %svc.name, isolation = ?svc.isolation, "launched");
    }
    Ok(())
}

/// The host paths sandboxed `svc` reads, canonical: none may be the data
/// dir, hold it or lie in it, as the registry, the audit log and the other
/// services' state are there.
fn mounts(svc: &ServiceSection, data_dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let data = data_dir.canonicalize().with_context(|| format!("the data dir {}", data_dir.display()))?;
    let mut out = Vec::new();
    for path in &svc.mounts {
        let path =
            path.canonicalize().with_context(|| format!("service {}: the mount {}", svc.name, path.display()))?;
        anyhow::ensure!(
            !data.starts_with(&path) && !path.starts_with(&data),
            "service {}: the mount {} would show it the data dir {}, with the registry and the audit log",
            svc.name,
            path.display(),
            data.display()
        );
        out.push(path);
    }
    Ok(out)
}

/// A command named in the configuration, as an absolute path: itself when
/// it has a directory, else the first match on PATH.
fn resolve(command: &str) -> anyhow::Result<String> {
    let path = std::path::Path::new(command);
    if path.components().count() > 1 {
        anyhow::ensure!(path.is_absolute(), "the command {command} of a sandboxed service must be an absolute path");
        return Ok(command.to_owned());
    }
    let dirs = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&dirs)
        .map(|dir| dir.join(command))
        .find(|p| p.is_file() && p.is_absolute())
        .map(|p| p.to_string_lossy().into_owned())
        .with_context(|| format!("there is no {command} on PATH"))
}

/// How long an MCP bridge gets to list its server's tools.
const MCP_LIST: std::time::Duration = std::time::Duration::from_secs(90);

/// Give each MCP bridge service (`molt-mcp serve`) the declarations of its
/// server's tools, which `molt-mcp list` prints with the same arguments and
/// environment. A server that cannot list them is started without tools,
/// with a warning: one broken server does not stop the run.
async fn mcp_tools(services: &mut [ServiceSection]) {
    for svc in services.iter_mut().filter(|s| s.tools.is_empty()) {
        let Some(exec) = svc.exec.as_ref().filter(|e| {
            std::path::Path::new(&e.command).file_name().is_some_and(|n| n == "molt-mcp")
                && e.args.first().is_some_and(|a| a == "serve")
        }) else {
            continue;
        };
        let mut command = tokio::process::Command::new(&exec.command);
        command.env_clear();
        for key in molt_kernel::supervisor::BASELINE_ENV {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .arg("list")
            .args(&exec.args[1..])
            .envs(svc.launch_env())
            .env("RUST_LOG", "warn")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let listed = match tokio::time::timeout(MCP_LIST, command.output()).await {
            Err(_) => Err(format!("it did not list its tools within {} s", MCP_LIST.as_secs())),
            Ok(Err(e)) => Err(format!("molt-mcp did not start: {e}")),
            Ok(Ok(out)) if !out.status.success() => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            Ok(Ok(out)) => serde_json::from_slice::<serde_json::Value>(&out.stdout).map_err(|e| e.to_string()),
        };
        match listed {
            Ok(listing) => {
                for why in listing["left_out"].as_array().into_iter().flatten().filter_map(|w| w.as_str()) {
                    tracing::warn!(service = %svc.name, "an MCP tool is left out: {why}");
                }
                match serde_json::from_value(listing["tools"].clone()) {
                    Ok(tools) => svc.tools = tools,
                    Err(e) => tracing::warn!(service = %svc.name, "its tool list is malformed: {e}"),
                }
            }
            Err(why) => tracing::warn!(service = %svc.name, "its MCP server offers no tools in this run: {why}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_kernel_on_the_same_data_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.kernel.data_dir = dir.path().join("data");
        cfg.kernel.fsync = false;
        let running = start(&cfg).await.unwrap();
        let err = start(&cfg).await.err().unwrap().to_string();
        assert!(err.contains("already running"), "{err}");
        running.shutdown().await;
        start(&cfg).await.unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn a_data_dir_too_long_for_sockets_is_refused_before_anything_is_made() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.kernel.data_dir = dir.path().join("d".repeat(MAX_SOCKET_PATH));
        let err = start(&cfg).await.err().unwrap().to_string();
        assert!(err.contains("use a data dir with a shorter path"), "{err}");
        assert!(!cfg.kernel.data_dir.exists());
    }
}
