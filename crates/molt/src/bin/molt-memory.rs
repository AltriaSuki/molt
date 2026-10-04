//! The memory service (`memory.*`), started by the kernel:
//! `molt-memory --root DIR --db FILE [--skip DIR]...`. Its model settings come from the
//! environment; see `molt_memory::Config::from_env`.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use molt_memory::{Config, Db, Memory, ServiceBus, Writer};
use molt_sdk::Service;

#[derive(Parser)]
#[command(name = "molt-memory", version, about = "Molt's memory: notes and the project model")]
struct Cli {
    /// Workspaces must be inside this directory.
    #[arg(long)]
    root: PathBuf,
    /// The database file. It is created private (mode 0600) if missing.
    #[arg(long)]
    db: PathBuf,
    /// A directory the project model leaves out, such as Molt's data
    /// directory with the forks in it. May be given more than once.
    #[arg(long)]
    skip: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let cli = Cli::parse();
    let cfg = Config { skip: cli.skip, ..Config::from_env()? };
    let db = Arc::new(Db::open(&cli.db)?);
    let svc = Arc::new(Service::connect_from_env().await?);
    let bus = Arc::new(ServiceBus(svc.clone()));
    let memory = Arc::new(Memory::new(db, &cli.root, cfg, Writer::of(&svc), bus)?);
    molt_memory::serve(svc, memory).await;
    Ok(())
}
