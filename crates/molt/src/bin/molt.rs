use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use molt::agent::{self, Interrupted};
use molt::{Config, Secrets};
use molt_api::model::Effort;
use molt_api::planner::{Outcome, RunRequest};
use molt_proto::ServiceId;

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
    /// Carry out a task in a workspace with the agent services, verified by a done-check.
    Do(DoArgs),
    /// Work with the audit log.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Create NATS secrets (if missing) and print the nats-server settings for them.
    NatsConfig,
}

#[derive(Args)]
struct DoArgs {
    /// What to do, in plain words.
    task: String,
    /// The project to work on.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Shell command, run in the workspace, that exits 0 once the task is done.
    /// Without one, the planner designs a check first.
    #[arg(long)]
    check: Option<String>,
    /// Parallel attempts.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=8))]
    attempts: u32,
    /// Model for the attempts: opus, sonnet, haiku or a model id.
    #[arg(long)]
    model: Option<String>,
    /// How hard the model thinks: low, medium, high, xhigh or max.
    #[arg(long)]
    effort: Option<Effort>,
    /// Model turns per attempt.
    #[arg(long)]
    max_turns: Option<u32>,
    /// Spending limit for the whole run, in US dollars.
    #[arg(long)]
    budget_usd: Option<f64>,
    /// Keep the result in its fork instead of applying it to the workspace.
    #[arg(long)]
    no_apply: bool,
    /// Print the result as JSON.
    #[arg(long)]
    json: bool,
    /// Where kernel state and forks go. Default: ~/.cache/molt/<project>-<hash>.
    #[arg(long)]
    data_dir: Option<PathBuf>,
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
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    // `molt do` prints its own progress; kernel logs would bury it.
    molt::init_tracing(if matches!(cli.cmd, Cmd::Do(_)) { "warn" } else { "info" });
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
            let running = molt::start(&cfg).await?;
            tracing::info!(services = cfg.services.len(), "kernel running; Ctrl-C to stop");
            tokio::signal::ctrl_c().await?;
            running.shutdown().await;
        }
        Cmd::Do(args) => return do_task(&cli.config, args).await,
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
            let mut ids: Vec<ServiceId> = cfg.services.iter().map(|s| s.name.clone()).collect();
            let secrets = Secrets::load_or_create(&cfg.secrets_path(), &ids)?;
            let cli_id = ServiceId::new(agent::CLI)?;
            if !ids.contains(&cli_id) {
                ids.push(cli_id);
            }
            let mut services = Vec::with_capacity(ids.len());
            for id in ids {
                let secret =
                    secrets.services.get(id.as_str()).cloned().with_context(|| format!("no secret for {id}"))?;
                services.push((id, secret));
            }
            print!(
                "{}",
                molt_transport::nats::server_config(molt_transport::nats::DEFAULT_PREFIX, &secrets.kernel, &services)
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn do_task(config: &Path, args: DoArgs) -> anyhow::Result<ExitCode> {
    let workspace = args.workspace.canonicalize().with_context(|| format!("workspace {}", args.workspace.display()))?;
    anyhow::ensure!(workspace.is_dir(), "the workspace {} is not a directory", workspace.display());
    let mut cfg = agent::resolve_config(config, &workspace, args.data_dir.as_deref())?;
    if std::env::var_os("RUST_LOG").is_none() {
        for svc in &mut cfg.services {
            svc.env.entry("RUST_LOG".into()).or_insert_with(|| "warn".into());
        }
    }
    let mut req = RunRequest::new(args.task, workspace.to_str().context("the workspace path is not UTF-8")?);
    req.check = args.check;
    req.attempts = args.attempts;
    req.model = args.model;
    req.effort = args.effort;
    req.max_turns = args.max_turns;
    req.budget_usd = args.budget_usd;
    req.apply = !args.no_apply;

    let ctrl_c = async {
        // Without a signal handler, wait forever rather than stop at once.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let resp = match agent::run_task(&cfg, req, |event| eprintln!("{}", agent::describe(event)), ctrl_c).await {
        Ok(resp) => resp,
        Err(e) if e.is::<Interrupted>() => {
            eprintln!("interrupted; the services are stopped");
            return Ok(ExitCode::from(130));
        }
        Err(e) => return Err(e),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print!("{}", agent::report(&resp));
    }
    Ok(match resp.outcome {
        Outcome::Passed | Outcome::Unverified => ExitCode::SUCCESS,
        Outcome::Failed => ExitCode::FAILURE,
    })
}
