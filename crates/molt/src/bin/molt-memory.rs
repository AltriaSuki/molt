//! The memory service (`memory.*`), started by the kernel:
//! `molt-memory --root DIR --db FILE [--skip DIR]... [--project ID --project-path DIR...] [--user-db FILE]`.
//! Its model settings come from the
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
    /// The id of the project in the user's registry: notes about its
    /// directories are kept under it.
    #[arg(long, requires = "project_path")]
    project: Option<String>,
    /// A directory of that project. May be given more than once.
    #[arg(long, requires = "project")]
    project_path: Vec<PathBuf>,
    /// The user's notes for every project, shared by all of them. Without
    /// it, notes for every project stay in `--db`.
    #[arg(long)]
    user_db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let cli = Cli::parse();
    let project = cli.project.map(|id| molt_memory::Project { id, paths: cli.project_path });
    let cfg = Config { skip: cli.skip, fork_root: cli.forks, project, ..Config::from_env()? };
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
    let mut memory = Memory::new(db, &cli.root, cfg, Writer::of(&svc), bus)?;
    if let Some(path) = &cli.user_db {
        memory = memory.with_user(Arc::new(Db::open(path)?));
    }
    let memory = Arc::new(memory);
    molt_memory::serve(svc, memory).await;
    Ok(())
}
