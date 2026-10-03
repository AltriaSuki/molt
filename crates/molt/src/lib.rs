//! The `molt` daemon: reads `molt.toml`, starts the kernel on the chosen
//! transport, installs the configured services and supervises them. The
//! [`agent`] module runs one task through them for `molt do`.

pub mod agent;
pub mod config;
mod lock;

use std::sync::Arc;

use anyhow::Context;
use molt_kernel::supervisor::{Limits, RestartPolicy};
use molt_kernel::{Config as KernelConfig, Kernel};
use molt_transport::{Secret, Transport};

pub use config::{Config, Secrets, ServiceSection, TransportKind};
pub use lock::{DataDirLock, LOCK_FILE};

/// Log to stderr, filtered by `RUST_LOG` or else `default` (e.g. `info`).
pub fn init_tracing(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
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

async fn launch_all(kernel: &Kernel, cfg: &Config, secrets: Option<&Secrets>) -> anyhow::Result<()> {
    for svc in &cfg.services {
        kernel.install(svc.manifest()?).await?;
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
        kernel.launch_with_env(&svc.name, secret, limits, restart, svc.launch_env()).await?;
        tracing::info!(service = %svc.name, "launched");
    }
    Ok(())
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
}
