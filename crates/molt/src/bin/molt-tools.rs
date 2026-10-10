//! The file and shell services, started by the kernel:
//! `molt-tools fs --root DIR --scratch DIR` serves `fs.*` and
//! `molt-tools shell --root DIR --scratch DIR` serves `shell.*`.
//!
//! The shell service kills the commands it is running before it exits on
//! SIGTERM (the supervisor stopping it), SIGINT (Ctrl-C in the terminal molt
//! runs in), SIGHUP (that terminal closing) or SIGQUIT (`Ctrl-\`); they would
//! outlive it otherwise.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use molt_sdk::Service;
use molt_tools::{ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};

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
    Shell(ShellArgs),
}

#[derive(Args)]
struct ShellArgs {
    #[command(flatten)]
    roots: RootArgs,
    /// Explicitly run with the host user's files and network.
    #[arg(long, conflicts_with = "sandbox_policy")]
    no_sandbox: bool,
    /// Trusted JSON policy outside the project and scratch roots.
    #[arg(long)]
    sandbox_policy: Option<PathBuf>,
    /// Explicit environment names allowed inside the sandbox (repeatable).
    #[arg(long)]
    sandbox_env: Vec<String>,
}

#[derive(Args)]
struct RootArgs {
    /// Workspaces must be inside this directory.
    #[arg(long)]
    root: PathBuf,
    /// Forks are created here. Keep it outside the projects under `root`. It
    /// must be private: it is created with mode 0700 if missing, and refused
    /// if it is a symlink, another user's, or writable by others.
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
            let roots = args.roots.roots();
            let policy = if args.no_sandbox {
                ExecutionPolicy::Unconfined
            } else {
                let mut policy = match args.sandbox_policy {
                    Some(path) => SandboxPolicy::load(
                        &path,
                        &Roots {
                            root: roots.root.canonicalize()?,
                            scratch: roots.scratch.canonicalize().unwrap_or_else(|_| roots.scratch.clone()),
                        },
                    )?,
                    None => SandboxPolicy::default(),
                };
                policy.environment.extend(args.sandbox_env);
                ExecutionPolicy::Configured(policy)
            };
            let shell = Arc::new(Shell::with_policy(roots, policy)?);
            let stop = molt_tools::stop_signal()?;
            let svc = Arc::new(Service::connect_from_env().await?);
            let serving = molt_tools::serve_shell(svc, shell.clone());
            tokio::pin!(serving);
            tokio::select! {
                () = &mut serving => {}
                () = stop => { shell.shutdown().await; }
            }
            shell.kill_all();
        }
    }
    Ok(())
}
