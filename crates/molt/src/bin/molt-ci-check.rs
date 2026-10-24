//! Run a CI baseline in the same isolated fs fork and shell backend as attempts.
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use molt_api::{fs, shell};
use molt_tools::{Fs, Roots, Shell};

#[derive(Parser)]
struct Args {
    command: String,
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long, default_value_t = 120_000)]
    timeout_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = args.workspace.canonicalize()?;
    let scratch = tempfile::tempdir()?;
    let roots = Roots { root: root.clone(), scratch: scratch.path().join("forks") };
    let fs = Fs::new(roots.clone())?;
    let shell = Arc::new(Shell::new(roots)?);
    let fork =
        fs.handle(fs::FORK, serde_json::json!({"workspace": root})).await.map_err(|e| anyhow::anyhow!(e.message))?;
    let fork: fs::ForkResponse = serde_json::from_value(fork)?;
    let req = shell::RunRequest {
        workspace: fork.fork.clone(),
        command: args.command,
        timeout_ms: Some(args.timeout_ms),
        stream: false,
    };
    let result = shell.handle(shell::RUN, serde_json::to_value(req)?).await;
    shell.shutdown().await;
    let _ = fs.handle(fs::DROP, serde_json::json!({"fork": fork.fork})).await;
    let result = result.map_err(|e| anyhow::anyhow!(e.message)).context("isolated CI check")?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
