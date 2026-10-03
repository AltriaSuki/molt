//! The file and shell services, started by the kernel:
//! `molt-tools fs --root DIR --scratch DIR` serves `fs.*` and
//! `molt-tools shell --root DIR --scratch DIR` serves `shell.*`.
//!
//! The shell service kills the commands it is running before it exits on
//! SIGTERM (the supervisor stopping it) or SIGINT (Ctrl-C in the terminal
//! molt runs in); they would outlive it otherwise.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use molt_sdk::Service;
use molt_tools::{Fs, Roots, Shell};
use tokio::signal::unix::{signal, SignalKind};

#[derive(Parser)]
#[command(name = "molt-tools", version, about = "Molt's file and shell services")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve `fs.*`: files, forks, diffs and merges.
    Fs(RootArgs),
    /// Serve `shell.*`: commands run in a workspace.
    Shell(RootArgs),
}

#[derive(Args)]
struct RootArgs {
    /// Workspaces must be inside this directory.
    #[arg(long)]
    root: PathBuf,
    /// Forks are created here. Keep it outside the projects under `root`.
    #[arg(long)]
    scratch: PathBuf,
}

impl RootArgs {
    fn roots(self) -> Roots {
        Roots { root: self.root, scratch: self.scratch }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    match Cli::parse().cmd {
        Cmd::Fs(args) => {
            let fs = Arc::new(Fs::new(args.roots())?);
            let svc = Arc::new(Service::connect_from_env().await?);
            molt_tools::serve_fs(svc, fs).await;
        }
        Cmd::Shell(args) => {
            let shell = Arc::new(Shell::new(args.roots())?);
            let mut term = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let svc = Arc::new(Service::connect_from_env().await?);
            tokio::select! {
                () = molt_tools::serve_shell(svc, shell.clone()) => {}
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }
            shell.kill_all();
        }
    }
    Ok(())
}
