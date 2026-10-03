use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use molt::{Config, Secrets};

#[derive(Parser)]
#[command(name = "molt", version, about = "Molt kernel daemon and tools")]
struct Cli {
    /// Path to molt.toml.
    #[arg(short, long, global = true, default_value = "molt.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the kernel and the configured services; stop on Ctrl-C.
    Run,
    /// Work with the audit log.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Create NATS secrets (if missing) and print the nats-server authorization block.
    NatsConfig,
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Check the hash chain of an audit log.
    Verify { path: Option<PathBuf> },
    /// Print the last entries as JSON lines.
    Tail {
        path: Option<PathBuf>,
        #[arg(short, default_value_t = 20)]
        n: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let config = || Config::load(&cli.config);
    let audit_path = |p: Option<PathBuf>| -> anyhow::Result<PathBuf> {
        match p {
            Some(p) => Ok(p),
            None => Ok(config()?.kernel.data_dir.join("audit.jsonl")),
        }
    };
    match cli.cmd {
        Cmd::Run => {
            let cfg = config()?;
            let kernel = molt::start(&cfg).await?;
            tracing::info!(services = cfg.services.len(), "kernel running; Ctrl-C to stop");
            tokio::signal::ctrl_c().await?;
            kernel.shutdown().await;
        }
        Cmd::Audit { cmd: AuditCmd::Verify { path } } => {
            let path = audit_path(path)?;
            let n =
                molt_kernel::audit::verify(&path).await.with_context(|| format!("{} is corrupt", path.display()))?;
            println!("{}: {n} entries, chain intact", path.display());
        }
        Cmd::Audit { cmd: AuditCmd::Tail { path, n } } => {
            let path = audit_path(path)?;
            for e in molt_kernel::audit::tail(&path, n).await? {
                println!("{}", serde_json::json!({ "seq": e.seq, "ts_ms": e.ts_ms, "event": e.event }));
            }
        }
        Cmd::NatsConfig => {
            let cfg = config()?;
            let ids: Vec<_> = cfg.services.iter().map(|s| s.name.clone()).collect();
            let secrets = Secrets::load_or_create(&cfg.secrets_path(), &ids)?;
            let services: Vec<_> = ids.iter().map(|id| (id.clone(), secrets.services[id.as_str()].clone())).collect();
            print!(
                "{}",
                molt_transport::nats::server_config(molt_transport::nats::DEFAULT_PREFIX, &secrets.kernel, &services)
            );
        }
    }
    Ok(())
}
