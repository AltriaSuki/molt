use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use molt::agent::{self, Interrupted};
use molt::{Config, Secrets};
use molt_api::memory::{self, ForgetRequest, ForgetResponse, MapRequest, RecallRequest};
use molt_api::model::Effort;
use molt_api::planner::{Outcome, RunRequest};
use molt_proto::ServiceId;

/// The config `molt run`, `molt audit` and `molt nats-config` read when none is given.
const DEFAULT_CONFIG: &str = "molt.toml";

#[derive(Parser)]
#[command(name = "molt", version, about = "Molt kernel daemon and tools")]
struct Cli {
    /// Path to molt.toml [default: molt.toml]. `molt do` and `molt memory`
    /// read one only when this is given.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the kernel and the configured services; stop on Ctrl-C, SIGTERM or SIGHUP.
    Run,
    /// Carry out a task in a workspace with the agent services, verified by a done-check.
    #[command(
        after_help = "Exit status: 0 when the run passed (or finished unverified) and its changes are applied, \
                            or kept as --no-apply asks; 1 when no attempt passed, or on an error; 2 for a usage error; \
                            3 when the run passed but its changes could not be applied and are kept in the fork the \
                            report names; 128+N when stopped by signal N (130 for Ctrl-C, 129 for SIGHUP, 143 for SIGTERM)."
    )]
    Do(DoArgs),
    /// See and correct what memory holds about a project.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
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
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: Option<u32>,
    /// Spending limit for the whole run, learning included, in US dollars.
    /// It is checked before each model call, so the last one can go past it.
    #[arg(long, value_parser = usd)]
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
    /// Pass this variable from your environment to the commands the agent
    /// runs, the check included (repeatable). They get only PATH, HOME, the
    /// locale and a few more otherwise.
    #[arg(long, value_name = "NAME", value_parser = env_name)]
    pass_env: Vec<String>,
    /// Run without memory: no project map or notes for the model, and
    /// nothing learned from the run.
    #[arg(long)]
    no_memory: bool,
    /// Use memory, but do not learn from this run (learning is one more
    /// model call after the run).
    #[arg(long)]
    no_learn: bool,
}

/// Where a project's memory is.
#[derive(Args)]
struct MemoryAt {
    /// The project.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// The data dir `molt do` used there. Default: ~/.cache/molt/<project>-<hash>.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// List the notes about the project, strongest first, or those matching WORDS.
    Show {
        #[command(flatten)]
        at: MemoryAt,
        words: Vec<String>,
        /// Most notes to list (at most 50).
        #[arg(short, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=50))]
        n: u32,
        /// Print the notes as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Forget a note. It is kept as a tombstone with the reason, and the
    /// forget is recorded in the audit log.
    Forget {
        #[command(flatten)]
        at: MemoryAt,
        /// The note's id, as `molt memory show` prints it.
        id: String,
        /// Why it is wrong.
        #[arg(long)]
        reason: String,
    },
    /// Print the map of the project's code that a run starts with, for WORDS
    /// (a task) or for the project as a whole.
    Map {
        #[command(flatten)]
        at: MemoryAt,
        words: Vec<String>,
        /// Size of the map, in estimated tokens (at most 32000).
        #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u32).range(1..=32_000))]
        tokens: u32,
    },
}

/// A positive amount of US dollars, refused here rather than by the planner
/// once every service has started.
fn usd(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(usd),
        _ => Err(format!("{text:?} is not a positive amount")),
    }
}

/// A variable `--pass-env` may name. The shell service keeps MOLT_* and
/// ANTHROPIC_* variables from its commands, so passing one would only put a
/// secret in its process.
fn env_name(name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err(format!("{name:?} is not a variable name"));
    }
    if name.starts_with("MOLT_") || name.starts_with("ANTHROPIC_") {
        return Err("commands never see MOLT_* or ANTHROPIC_* variables".into());
    }
    Ok(name.to_owned())
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
    // `molt do` and `molt memory` print their own progress; kernel logs would bury it.
    molt::init_tracing(if matches!(cli.cmd, Cmd::Do(_) | Cmd::Memory { .. }) { "warn" } else { "info" });
    let config = || Config::load(cli.config.as_deref().unwrap_or(Path::new(DEFAULT_CONFIG)));
    let audit_path = |p: Option<PathBuf>| -> anyhow::Result<PathBuf> {
        match p {
            Some(p) => Ok(p),
            None => Ok(config()?.kernel.data_dir.join("audit.jsonl")),
        }
    };
    match cli.cmd {
        Cmd::Run => {
            let stop = molt::stop_signal()?;
            let cfg = config()?;
            let running = molt::start(&cfg).await?;
            tracing::info!(services = cfg.services.len(), "kernel running; Ctrl-C to stop");
            let signal = stop.await;
            tracing::info!(signal, "stopping");
            running.shutdown().await;
        }
        Cmd::Do(args) => return do_task(cli.config.as_deref(), args).await,
        Cmd::Memory { cmd } => memory_cmd(cli.config.as_deref(), cmd).await?,
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

async fn do_task(config: Option<&Path>, args: DoArgs) -> anyhow::Result<ExitCode> {
    // From the start, so that no signal leaves services running.
    let stop = molt::stop_signal()?;
    let workspace = args.workspace.canonicalize().with_context(|| format!("workspace {}", args.workspace.display()))?;
    anyhow::ensure!(workspace.is_dir(), "the workspace {} is not a directory", workspace.display());
    let mut cfg = agent::resolve_config(config, &workspace, args.data_dir.as_deref())?;
    if let Some(path) = config {
        eprintln!("config: {}", agent::printable(&path.display().to_string()));
    }
    if let Some(shell) = cfg.service_mut("shell") {
        shell.pass_env.extend(args.pass_env);
    }
    if args.no_memory {
        cfg.services.retain(|s| s.name.as_str() != agent::MEMORY);
    }
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
    let apply = req.apply;

    let mut signal = None;
    let stopped = async { signal = Some(stop.await) };
    let progress = |event: &_| eprintln!("{}", agent::describe(event));
    let result = agent::run_task(&cfg, req, !args.no_learn, progress, stopped).await;
    let done = match result {
        Ok(done) => done,
        Err(e) if e.is::<Interrupted>() => {
            eprintln!("interrupted; the services are stopped");
            let status = signal.map_or(130, |n| 128 + n);
            return Ok(ExitCode::from(u8::try_from(status).unwrap_or(130)));
        }
        Err(e) => return Err(e),
    };
    let resp = done.run;
    if args.json {
        let mut out = serde_json::to_value(&resp)?;
        // What learning did and cost, next to the run's own spend.
        out["learning"] = done.learned.as_ref().map_or(serde_json::Value::Null, agent::learning_json);
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print!("{}", agent::report(&resp));
    }
    if let Some(learned) = &done.learned {
        eprintln!("{}", agent::learned(learned));
    }
    // A signal while memory learned from the run still ends it as stopped.
    if let Some(n) = signal {
        return Ok(ExitCode::from(u8::try_from(128 + n).unwrap_or(130)));
    }
    Ok(match resp.outcome {
        Outcome::Failed => ExitCode::FAILURE,
        // It passed, but the workspace does not have it: a script must not go on as if it did.
        Outcome::Passed | Outcome::Unverified if apply && !resp.applied && resp.fork.is_some() => ExitCode::from(3),
        Outcome::Passed | Outcome::Unverified => ExitCode::SUCCESS,
    })
}

async fn memory_cmd(config: Option<&Path>, cmd: MemoryCmd) -> anyhow::Result<()> {
    let at = match &cmd {
        MemoryCmd::Show { at, .. } | MemoryCmd::Forget { at, .. } | MemoryCmd::Map { at, .. } => at,
    };
    let workspace = at.workspace.canonicalize().with_context(|| format!("workspace {}", at.workspace.display()))?;
    let key = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    let cfg = agent::memory_config(config, &workspace, at.data_dir.as_deref())?;
    let open = || -> anyhow::Result<molt_memory::Db> {
        let db = agent::memory_db(&cfg).context("the memory service has no --db")?;
        anyhow::ensure!(db.is_file(), "memory has nothing yet ({} does not exist)", db.display());
        molt_memory::Db::open(&db)
    };
    match cmd {
        MemoryCmd::Show { words, n, json, .. } => {
            let req = RecallRequest { query: words.join(" "), workspace: Some(key), k: Some(n), ..Default::default() };
            let notes = molt_memory::recall(&open()?, &req).map_err(|e| anyhow::anyhow!(e.message))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&notes)?);
            } else if notes.is_empty() {
                eprintln!("no notes");
            } else {
                print!("{}", agent::printable(&show_notes(&notes)));
            }
        }
        MemoryCmd::Forget { id, reason, .. } => {
            let req = serde_json::to_value(ForgetRequest { id: id.clone(), reason })?;
            let reply: ForgetResponse = serde_json::from_value(agent::call_memory(&cfg, memory::FORGET, req).await?)?;
            anyhow::ensure!(reply.forgotten, "there is no note {id}, or it is already forgotten");
            println!("forgot {id}");
        }
        MemoryCmd::Map { words, tokens, .. } => {
            let db = open()?;
            molt_memory::project::index(&db, &workspace, None).map_err(|e| anyhow::anyhow!(e.message))?;
            let req = MapRequest { workspace: key, query: words.join(" "), max_tokens: Some(tokens) };
            let map = molt_memory::project::map(&db, &workspace, &req).map_err(|e| anyhow::anyhow!(e.message))?;
            print!("{}", agent::printable(&map.map));
            eprintln!("{} files, {} definitions, about {} tokens", map.files, map.symbols, map.tokens);
        }
    }
    Ok(())
}

/// Notes as `molt memory show` lists them: id, kind, confidence and how
/// often an episode bore the note out, then the text.
fn show_notes(notes: &[molt_api::memory::Recalled]) -> String {
    let mut out = String::new();
    for r in notes {
        let n = &r.note;
        let mut extra = Vec::new();
        if n.reinforced > 0 {
            extra.push(format!("confirmed {}x", n.reinforced));
        }
        if !n.conflicts.is_empty() {
            extra.push(format!("disputed by {}", n.conflicts.join(", ")));
        }
        let extra = if extra.is_empty() { String::new() } else { format!(" ({})", extra.join("; ")) };
        out.push_str(&format!("{}  {} {:.2}{extra}\n    {}\n", n.id, n.kind.as_str(), n.confidence, n.text));
    }
    out
}
