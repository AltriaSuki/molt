//! The `molt` daemon: reads `molt.toml`, starts the kernel on the chosen
//! transport, installs the configured services and supervises them.

pub mod config;

use std::sync::Arc;

use anyhow::{bail, Context};
use molt_kernel::supervisor::{Limits, RestartPolicy};
use molt_kernel::{Config as KernelConfig, Kernel};
use molt_transport::{Secret, Transport};

pub use config::{Config, Secrets, TransportKind};

/// Start the kernel and every configured service. Returns the running kernel.
pub async fn start(cfg: &Config) -> anyhow::Result<Kernel> {
    let mut kcfg = KernelConfig::new(&cfg.kernel.data_dir);
    kcfg.fsync = cfg.kernel.fsync;
    let secrets = match cfg.kernel.transport {
        TransportKind::Unix => None,
        TransportKind::Nats => Some(Secrets::load(&cfg.secrets_path()).context(
            "NATS needs pre-provisioned secrets: run `molt nats-config` and restart nats-server with its output",
        )?),
    };
    let transport: Arc<dyn Transport> = match cfg.kernel.transport {
        TransportKind::Unix => Arc::new(molt_transport::unix::UnixTransport::new(cfg.socket_dir())?),
        TransportKind::Nats => {
            let s = secrets.as_ref().unwrap();
            Arc::new(
                molt_transport::nats::NatsTransport::connect(
                    &cfg.kernel.nats_url,
                    &s.kernel,
                    molt_transport::nats::DEFAULT_PREFIX,
                )
                .await?,
            )
        }
    };
    let kernel = Kernel::start(kcfg, transport).await?;
    for svc in &cfg.services {
        let manifest = svc.manifest()?;
        kernel.install(manifest).await?;
        let secret: Option<Secret> = match &secrets {
            Some(s) => match s.services.get(svc.name.as_str()) {
                Some(secret) => Some(secret.clone()),
                None => bail!("no NATS secret for {}; rerun `molt nats-config`", svc.name),
            },
            None => None,
        };
        let limits = Limits { memory_bytes: svc.memory_mb.map(|m| m * 1024 * 1024), cpu_secs: svc.cpu_secs };
        let restart = RestartPolicy { max_restarts: svc.max_restarts, ..RestartPolicy::default() };
        kernel.launch(&svc.name, secret, limits, restart).await?;
        tracing::info!(service = %svc.name, "launched");
    }
    Ok(kernel)
}
