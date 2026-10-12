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
    /// The fs service's trusted scratch directory, for disposable fork symbols.
    #[arg(long)]
    forks: Option<PathBuf>,
    /// Trusted local word-vector JSON model. No file is downloaded automatically.
    #[arg(long, requires = "embedding_sha256")]
    embedding_model: Option<PathBuf>,
    #[arg(long, requires = "embedding_model")]
    embedding_sha256: Option<String>,
    #[arg(long, default_value_t = 0.5)]
    semantic_weight: f64,
    #[arg(long, default_value_t = 0.4)]
    semantic_min_similarity: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let cli = Cli::parse();
    let cfg = Config { skip: cli.skip, fork_root: cli.forks, ..Config::from_env()? };
    let mut db = Db::open(&cli.db)?;
    if let Some(path) = cli.embedding_model {
        anyhow::ensure!(
            !path.canonicalize()?.starts_with(cli.root.canonicalize()?),
            "embedding model must be outside the workspace"
        );
        let model = molt_memory::semantic::Model::load(
            &path,
            cli.embedding_sha256.as_deref().expect("clap requires hash"),
            cli.semantic_weight,
            cli.semantic_min_similarity,
        )?;
        db.enable_semantic(model)?;
    }
    let db = Arc::new(db);
    let svc = Arc::new(Service::connect_from_env().await?);
    let bus = Arc::new(ServiceBus(svc.clone()));
    let memory = Arc::new(Memory::new(db, &cli.root, cfg, Writer::of(&svc), bus)?);
    molt_memory::serve(svc, memory).await;
    Ok(())
}
