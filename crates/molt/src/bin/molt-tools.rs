//! The file and shell services, started by the kernel:
//! `molt-tools fs --root DIR --scratch DIR` serves `fs.*` and
//! `molt-tools shell --root DIR --scratch DIR` serves `shell.*`.
//!
//! The shell service kills the commands it is running before it exits on
//! SIGTERM (the supervisor stopping it), SIGINT (Ctrl-C in the terminal molt
//! runs in), SIGHUP (that terminal closing) or SIGQUIT (`Ctrl-\`); they would
//! outlive it otherwise.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use molt_sdk::Service;
use molt_tools::{env_profile, EnvProfile, ExecutionPolicy, Fs, Roots, SandboxPolicy, Shell};

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
    /// Inspect, resume or undo durable workspace applies, without starting services.
    Apply(ApplyArgs),
}

#[derive(Args)]
struct ApplyArgs {
    #[command(flatten)]
    roots: RootArgs,
    #[command(subcommand)]
    action: ApplyAction,
}

#[derive(Subcommand)]
enum ApplyAction {
    /// List this workspace's durable operations and observed file states.
    List,
    /// Inspect one operation.
    Inspect { operation: String },
    /// Finish applying the same staged candidate, refusing later edits.
    Resume { operation: String },
    /// Restore the preimages, refusing later edits.
    Undo { operation: String },
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
    /// Build offline from this user's dependency cache: `cargo` mounts
    /// $CARGO_HOME/registry and git (default ~/.cargo) read-only.
    #[arg(long, value_name = "KIND", value_parser = ["cargo"], conflicts_with = "no_sandbox")]
    deps_cache: Vec<String>,
    /// The user's environment profiles, as JSON keyed by name, over the
    /// built-in ones; molt passes them from its trusted config.
    #[arg(long, value_name = "JSON", conflicts_with = "no_sandbox")]
    environments: Option<String>,
    /// Commands `shell.run` runs at once; more wait their turn. Default:
    /// the host's processors, and at least 4.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..=molt_tools::resources::MAX_JOBS as u64))]
    jobs: Option<u64>,
    /// Commands that may wait for a turn; past that one is refused at once.
    #[arg(long, value_name = "N", default_value_t = molt_tools::resources::DEFAULT_QUEUE as u64,
          value_parser = clap::value_parser!(u64).range(0..=molt_tools::resources::MAX_JOBS as u64))]
    queue: u64,
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
        Cmd::Apply(args) => {
            let workspace = args.roots.root.canonicalize()?.to_string_lossy().into_owned();
            let fs = Fs::new(args.roots.roots())?;
            let (method, payload) = match args.action {
                ApplyAction::List => ("applies", serde_json::json!({"workspace": workspace})),
                ApplyAction::Inspect { operation } => {
                    ("applies", serde_json::json!({"workspace": workspace, "operation": operation}))
                }
                ApplyAction::Resume { operation } => {
                    ("recover", serde_json::json!({"workspace": workspace, "operation": operation, "action": "resume"}))
                }
                ApplyAction::Undo { operation } => {
                    ("recover", serde_json::json!({"workspace": workspace, "operation": operation, "action": "undo"}))
                }
            };
            let result = fs.handle(method, payload).await.map_err(|e| anyhow::anyhow!(e.message))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Fs(args) => {
            let fs = Arc::new(Fs::new(args.roots())?);
            let svc = Arc::new(Service::connect_from_env().await?);
            molt_tools::serve_fs(svc, fs).await;
        }
        Cmd::Shell(args) => {
            let admission = match args.jobs {
                Some(jobs) => molt_tools::resources::Admission::new(jobs as usize, args.queue as usize),
                None => molt_tools::resources::Admission::default().with_queue(args.queue as usize),
            };
            tracing::info!(jobs = admission.jobs(), queue = admission.queue(), "shell admission");
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
                if args.deps_cache.iter().any(|kind| kind == "cargo") {
                    policy.cargo_cache = Some(molt_tools::host_cargo_home()?);
                }
                let user: BTreeMap<String, EnvProfile> = match &args.environments {
                    Some(json) => serde_json::from_str(json).context("--environments")?,
                    None => BTreeMap::new(),
                };
                let profiles = env_profile::profiles(&user);
                let root = roots.root.canonicalize()?;
                let var = |name: &str| std::env::var_os(name);
                for applied in env_profile::apply(&mut policy, &root, &[&root, &roots.scratch], &profiles, &var) {
                    tracing::info!(profile = %applied.name, "environment profile");
                    for note in applied.notes {
                        tracing::warn!(profile = %applied.name, "{note}");
                    }
                }
                ExecutionPolicy::Configured(policy)
            };
            let shell = Arc::new(Shell::with_policy(roots, policy)?.with_admission(admission));
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
