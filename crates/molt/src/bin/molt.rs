use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use molt::agent::{self, Interrupted};
use molt::{git, Config, Secrets};
use molt_api::fs::SyntaxCheck;
use molt_api::memory::{
    self, CorrectRequest, ForgetRequest, ForgetResponse, Layer, MapRequest, NoteKind, PromoteRequest, PromoteResponse,
    RecallRequest, RecallResponse, RememberRequest, RememberResponse, RetractResponse, ReviewRequest, ReviewResponse,
    UndoRequest,
};
use molt_api::model::Effort;
use molt_api::planner::{Outcome, Resume, RunRequest, Select};
use molt_planner::checkpoint;
use molt_proto::{ServiceId, TraceId};

/// The config `molt run`, `molt audit` and `molt nats-config` read when none is given.
const DEFAULT_CONFIG: &str = "molt.toml";

#[derive(Parser)]
#[command(name = "molt", version, about = "Molt: a self-improving agent. Without a subcommand, opens its interface.")]
struct Cli {
    /// Path to molt.toml [default: molt.toml]. `molt do` and `molt memory`
    /// read one only when this is given.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the interface: tasks one after another in a workspace, the
    /// check shown before the attempts start, and what memory holds.
    Ui(UiArgs),
    #[command(hide = true)]
    UiBackend,
    /// Start the kernel and the configured services; stop on Ctrl-C, SIGTERM or SIGHUP.
    Run,
    /// Carry out a task in a workspace with the agent services, verified by a done-check.
    #[command(
        after_help = "Exit status: 0 when the run passed (or finished unverified) and its changes are applied, \
                            or kept as --no-apply asks; 1 when no attempt passed, or on an error; 2 for a usage error; \
                            3 when the run passed but its changes could not be applied and are kept in the fork the \
                            report names; 4 when they are applied but --commit could not commit them; 128+N when \
                            stopped by signal N (130 for Ctrl-C, 129 for SIGHUP, 143 for SIGTERM)."
    )]
    Do(DoArgs),
    /// Go on with a run of `molt do --checkpoint` that stopped before it
    /// finished, from its last checkpoint. Its attempts keep their forks and
    /// conversations, and the cost so far counts against its budget.
    Resume(ResumeArgs),
    /// Take back the commit `molt do --commit` made for a run, with a new
    /// commit that puts its files back, and have memory no longer count
    /// what the run showed.
    Undo(UndoArgs),
    /// See and correct what memory holds about a project.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Inspect, independently evaluate and manage declarative project skills.
    Skill(SkillArgs),
    /// The projects molt knows: each keeps its notes under a stable id,
    /// whatever directories it has, or none.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Benchmark Molt against a plain agent loop on coding tasks graded by hidden tests.
    Bench {
        #[command(subcommand)]
        cmd: molt_bench::cli::Cmd,
    },
    /// See and update what each model costs.
    Prices {
        #[command(subcommand)]
        cmd: PricesCmd,
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
    /// Shell command, run in the workspace, that exits 0 once the task is done.
    /// Without one, the planner designs a check first.
    #[arg(long)]
    check: Option<String>,
    /// Run without a done-check: none is designed, one attempt does the task,
    /// and its result is applied unverified.
    #[arg(long, conflicts_with = "check")]
    no_check: bool,
    /// Parallel attempts.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=8))]
    attempts: u32,
    /// How to pick among attempts that pass: `first` takes the first and
    /// cancels the rest; `wait:<seconds>` gives the others that long to pass
    /// too, and `all` lets every attempt finish, then the passing ones are
    /// compared (project suite, diff size, review).
    #[arg(long, default_value = "first", value_name = "first|wait:<seconds>|all")]
    select: Select,
    /// Model for the attempts: opus, sonnet, haiku or a model id.
    #[arg(long)]
    model: Option<String>,
    /// How hard the model thinks: low, medium, high, xhigh or max.
    #[arg(long)]
    effort: Option<Effort>,
    /// Model turns per attempt.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: Option<u32>,
    /// What the model's file writes and edits do about syntax errors they
    /// introduce: report them with the result, reject the change, or off.
    /// Default: the planner's (report).
    #[arg(long, value_name = "MODE", value_parser = syntax_check)]
    syntax_check: Option<SyntaxCheck>,
    /// Model for the judge of a task no command can check, which decides
    /// the result against criteria the planner sets: opus, sonnet, haiku or a
    /// model id. Default: the attempts' model.
    #[arg(long, value_name = "MODEL")]
    judge_model: Option<String>,
    /// Save a checkpoint as the run goes, so that a run stopped before it
    /// finishes (interrupted, out of budget, killed) can go on with `molt resume`.
    #[arg(long)]
    checkpoint: bool,
    #[command(flatten)]
    common: RunArgs,
}

/// How a run of `molt do` or `molt resume` goes.
#[derive(Args)]
struct RunArgs {
    /// The project to work on.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// A project of `molt project list`, by id or name, in place of
    /// --workspace: its directory, or its own private one when it has none.
    #[arg(long, conflicts_with = "workspace")]
    project: Option<String>,
    /// Spending limit for the whole run, learning included, in US dollars.
    /// It is checked before each model call, so the last one can go past it.
    /// For `molt resume`, it replaces the stopped run's, which counts what
    /// was spent before.
    #[arg(long, value_parser = usd)]
    budget_usd: Option<f64>,
    /// Keep the result in its fork instead of applying it to the workspace.
    #[arg(long)]
    no_apply: bool,
    /// Commit the applied result to git: exactly the files the run merged,
    /// with a `Molt-Run:` trailer naming the run. What you have staged stays
    /// staged and out of the commit. `molt undo` takes it back.
    #[arg(long, conflicts_with = "no_apply")]
    commit: bool,
    /// With --commit, commit on a new branch NAME, made from the current
    /// commit and switched to.
    #[arg(long, value_name = "NAME", requires = "commit")]
    branch: Option<String>,
    /// Print the result as JSON.
    #[arg(long)]
    json: bool,
    /// Show model text previews on stderr while replies are generated.
    #[arg(long)]
    stream: bool,
    /// Where kernel state and forks go. Default: ~/.cache/molt/<project>-<hash>.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Pass this variable from your environment to the commands the agent
    /// runs, the check included (repeatable). They get only PATH, HOME, the
    /// locale and a few more otherwise.
    #[arg(long, value_name = "NAME", value_parser = env_name)]
    pass_env: Vec<String>,
    /// Explicitly run commands with the host user's files and network.
    #[arg(long, conflicts_with = "sandbox_policy")]
    no_sandbox: bool,
    /// Trusted shell sandbox JSON policy outside the project and scratch.
    #[arg(long)]
    sandbox_policy: Option<PathBuf>,
    /// Let the agent read web pages from this host (repeatable): `docs.rs`,
    /// or `*.python.org` for the hosts below it. Pages are fetched over
    /// https by a service of their own and recorded in the audit log;
    /// commands still have no network.
    #[arg(long, value_name = "HOST")]
    allow_web: Vec<String>,
    /// Run without memory: no project map or notes for the model, and
    /// nothing learned from the run.
    #[arg(long)]
    no_memory: bool,
    /// Use memory, but do not learn from this run (learning is one more
    /// model call after the run).
    #[arg(long)]
    no_learn: bool,
    /// Do not have a reviewer look at a passing result before it is applied
    /// (the review is more model calls, and holds back what it questions).
    #[arg(long)]
    no_review: bool,
    /// Model memory learns from the run with: opus, sonnet, haiku, a model
    /// id, or `same` for the attempts' model. Default: MOLT_MEMORY_MODEL,
    /// else sonnet.
    #[arg(long, value_name = "MODEL", conflicts_with_all = ["no_learn", "no_memory"])]
    memory_model: Option<String>,
}

#[derive(Args)]
struct ResumeArgs {
    /// The stopped run to go on with, as `--list` shows it. Default: the one
    /// in the workspace that stopped last.
    run: Option<String>,
    /// Leave the stopped attempts as they are and start new ones, with the
    /// same check and the cost so far.
    #[arg(long)]
    retry: bool,
    /// List the stopped runs of the workspace that can be resumed.
    #[arg(long)]
    list: bool,
    #[command(flatten)]
    common: RunArgs,
}

#[derive(Args, Default)]
struct UiArgs {
    /// The project to work on.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// A project of `molt project list`, by id or name, in place of --workspace.
    #[arg(long, conflicts_with = "workspace")]
    project: Option<String>,
    /// Where kernel state and forks go. Default: ~/.cache/molt/<project>-<hash>.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Pass this variable from your environment to the commands the agent
    /// runs (repeatable).
    #[arg(long, value_name = "NAME", value_parser = env_name)]
    pass_env: Vec<String>,
    /// Let the agent read web pages from this host (repeatable): `docs.rs`,
    /// or `*.python.org` for the hosts below it.
    #[arg(long, value_name = "HOST")]
    allow_web: Vec<String>,
}

#[derive(Args)]
struct UndoArgs {
    /// The run to take back, as its commit's `Molt-Run:` trailer names it.
    /// Default: the newest run on the current branch not taken back yet.
    run: Option<String>,
    #[command(flatten)]
    at: MemoryAt,
}

/// Where a project's memory is.
#[derive(Args)]
struct MemoryAt {
    /// The project.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// A project of `molt project list`, by id or name, in place of --workspace.
    #[arg(long, conflicts_with = "workspace")]
    project: Option<String>,
    /// The data dir `molt do` used there. Default: ~/.cache/molt/<project>-<hash>.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Args)]
struct SkillArgs {
    #[command(flatten)]
    at: MemoryAt,
    #[command(subcommand)]
    cmd: SkillCmd,
}

#[derive(Subcommand)]
enum SkillCmd {
    List,
    /// Draft a workflow from a JSON spec and a verified source run.
    Draft {
        file: PathBuf,
        #[arg(long)]
        run: String,
    },
    /// Record an independent audited run; failures remain in the evidence.
    Evaluate {
        id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        run: String,
    },
    /// Activate only after two successful independent audited evaluations.
    Promote {
        id: String,
        #[arg(long)]
        revision: u64,
    },
    Disable {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Select an older evaluated revision, disabled until explicitly promoted.
    Rollback {
        id: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        reason: String,
    },
    Used {
        run: String,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Inspect the exact notes and recall reasons returned during RUN.
    Used {
        #[command(flatten)]
        at: MemoryAt,
        run: String,
        #[arg(long)]
        json: bool,
    },
    /// Confirm a note, replace its file dependencies, or mark it temporary.
    Review(ReviewArgs),
    /// Replace a note while preserving its original evidence and reason.
    Correct {
        #[command(flatten)]
        review: ReviewArgs,
        text: String,
    },
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
    /// Write a note down yourself: about the project, or with --global
    /// about you, for every project.
    Remember {
        #[command(flatten)]
        at: MemoryAt,
        /// One self-contained sentence, such as "Answer in Chinese."
        text: String,
        /// Keep it in your own layer, recalled in every project.
        #[arg(long)]
        global: bool,
        /// fact, convention, decision, preference or lesson.
        #[arg(long, default_value = "preference")]
        kind: NoteKind,
    },
    /// Copy a project note to your own layer, recalled in every project.
    /// Learning only proposes this ("proposed for every project" in show).
    Promote {
        #[command(flatten)]
        at: MemoryAt,
        /// The note's id, as `molt memory show` prints it.
        id: String,
        /// Why it holds in every project.
        #[arg(long)]
        reason: String,
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

#[derive(Args)]
struct ReviewArgs {
    #[command(flatten)]
    at: MemoryAt,
    id: String,
    /// Revision printed by memory show; refuses concurrent changes.
    #[arg(long)]
    rev: u64,
    #[arg(long)]
    reason: String,
    /// Replace the dependency list with these project-relative files. Repeatable.
    #[arg(long)]
    depends_on: Vec<String>,
    /// Mark this as temporary experience, to be verified before reuse.
    #[arg(long)]
    temporary: bool,
}

/// A positive amount of US dollars, refused here rather than by the planner
/// once every service has started.
fn usd(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(usd),
        _ => Err(format!("{text:?} is not a positive amount")),
    }
}

fn syntax_check(text: &str) -> Result<SyntaxCheck, String> {
    match text {
        "report" => Ok(SyntaxCheck::Report),
        "reject" => Ok(SyntaxCheck::Reject),
        "off" => Ok(SyntaxCheck::Off),
        _ => Err(format!("{text:?} is not a mode: use report, reject or off")),
    }
}

/// A variable `--pass-env` may name. The shell service keeps MOLT_* and
/// ANTHROPIC_* variables from its commands, so passing one would only put a
/// secret in its process.
fn env_name(name: &str) -> Result<String, String> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err(format!("{name:?} is not a variable name"));
    }
    if name.starts_with("MOLT_") || name.starts_with("ANTHROPIC_") || name.starts_with("OPENAI_") {
        return Err("commands never see MOLT_*, ANTHROPIC_* or OPENAI_* variables".into());
    }
    Ok(name.to_owned())
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Register a project called NAME, for the directory --path or, without
    /// one, for work with no folder (it works in a private one of its own).
    New {
        name: String,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// List the projects, with their ids, directories and data dirs.
    List,
    /// Print the absolute working directory of a project, for cd or finding its output.
    Path {
        /// The project's id or name.
        project: String,
    },
    /// Tell molt a project's directory moved to NEW_DIR. Its notes, audit
    /// log and stopped runs stay with it.
    Move {
        /// The project's id or name.
        project: String,
        new_dir: PathBuf,
        /// The directory that moved, when the project has several.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Add DIR, such as another clone, to a project: it shares the project's
    /// notes and data dir.
    Add {
        /// The project's id or name.
        project: String,
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum PricesCmd {
    /// Download a price list, check it, and make it the snapshot runs use.
    /// Nothing is replaced when the download does not check out.
    Update {
        /// litellm (the widest coverage) or openrouter (OpenRouter's own prices).
        #[arg(long, default_value = "litellm")]
        source: molt_gateway::prices::Source,
        /// Download from here instead of the source's own address.
        #[arg(long)]
        url: Option<String>,
        /// Write the snapshot here. Default: ~/.local/share/molt/prices.json
        /// (in XDG_DATA_HOME when set), or MOLT_PRICES_FILE.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Show where prices come from, or what MODEL costs and where that price came from.
    Show {
        /// opus, sonnet, haiku or a model id.
        model: Option<String>,
    },
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
    let cmd = cli.cmd.unwrap_or_else(|| Cmd::Ui(UiArgs { workspace: ".".into(), ..UiArgs::default() }));
    // `molt do`, `molt memory`, `molt bench` and the interface print their own progress; kernel logs would bury it.
    molt::init_tracing(
        if matches!(
            cmd,
            Cmd::Do(_)
                | Cmd::Resume(_)
                | Cmd::Undo(_)
                | Cmd::Memory { .. }
                | Cmd::Bench { .. }
                | Cmd::Prices { .. }
                | Cmd::Ui(_)
        ) {
            "warn"
        } else {
            "info"
        },
    );
    let config = || Config::load(cli.config.as_deref().unwrap_or(Path::new(DEFAULT_CONFIG)));
    let audit_path = |p: Option<PathBuf>| -> anyhow::Result<PathBuf> {
        match p {
            Some(p) => Ok(p),
            None => Ok(config()?.kernel.data_dir.join("audit.jsonl")),
        }
    };
    match cmd {
        Cmd::Ui(args) => {
            let opts = molt::tui::Options {
                config: cli.config,
                workspace: args.workspace,
                project: args.project,
                data_dir: args.data_dir,
                pass_env: args.pass_env,
                allow_web: args.allow_web,
            };
            for fork in molt::tui::run(opts).await? {
                eprintln!("kept: {}", agent::printable(&fork));
            }
        }
        Cmd::Skill(args) => {
            use molt_api::skills::{Action, Request, MANAGE};
            let config = cli.config.as_deref();
            let (workspace, project) = agent::locate(args.at.project.as_deref(), &args.at.workspace, config.is_some())?;
            let cfg = agent::memory_config(config, &workspace, args.at.data_dir.as_deref(), project.as_ref())?;
            let action = match args.cmd {
                SkillCmd::List => Action::List,
                SkillCmd::Draft { file, run } => {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    std::fs::File::open(file)?.take(65537).read_to_end(&mut bytes)?;
                    anyhow::ensure!(bytes.len() <= 65536, "skill specification exceeds 64 KiB");
                    Action::Draft { spec: serde_json::from_slice(&bytes)?, run }
                }
                SkillCmd::Evaluate { id, revision, run } => Action::Evaluate { id, revision, run },
                SkillCmd::Promote { id, revision } => Action::Promote { id, revision },
                SkillCmd::Disable { id, reason } => Action::Disable { id, reason },
                SkillCmd::Rollback { id, revision, reason } => Action::Rollback { id, revision, reason },
                SkillCmd::Used { run } => Action::Used { run },
            };
            let request = Request { workspace: workspace.to_str().context("workspace is not UTF-8")?.into(), action };
            let response = agent::call_memory(&cfg, MANAGE, serde_json::to_value(request)?).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Cmd::UiBackend => molt::tui::serve_backend().await?,
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
        Cmd::Resume(args) => return resume_task(cli.config.as_deref(), args).await,
        Cmd::Undo(args) => return undo_cmd(cli.config.as_deref(), args).await,
        Cmd::Memory { cmd } => memory_cmd(cli.config.as_deref(), cmd).await?,
        Cmd::Project { cmd } => project_cmd(cmd)?,
        Cmd::Bench { cmd } => return molt_bench::cli::main(cmd).await,
        Cmd::Prices { cmd } => return prices_cmd(cmd).await,
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

async fn prices_cmd(cmd: PricesCmd) -> anyhow::Result<ExitCode> {
    use molt_gateway::prices::{self, PriceSource, PriceTable, Snapshot};

    let var = |name: &str| std::env::var(name).ok();
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let origin = |s: &PriceSource| {
        let mut text = s.source.clone();
        if let Some(url) = &s.url {
            text.push_str(&format!(" from {url}"));
        }
        if let Some(sha) = &s.sha256 {
            text.push_str(&format!(", sha256 {}", &sha[..sha.len().min(16)]));
        }
        if let Some(ms) = s.fetched_at_ms {
            let days = now_ms.saturating_sub(u128::from(ms)) / 86_400_000;
            text.push_str(&format!(", fetched {days} days ago"));
        }
        text
    };
    match cmd {
        PricesCmd::Update { source, url, output } => {
            let path = match output {
                Some(path) => path,
                None => prices::snapshot_path(var).context("neither XDG_DATA_HOME nor HOME is set; pass --output")?,
            };
            let new = prices::download(source, url.as_deref()).await?;
            let old = std::fs::read_to_string(&path).ok().and_then(|text| Snapshot::parse(&text).ok());
            let changed = old.unwrap_or_else(Snapshot::bundled).big_changes(&new);
            if !changed.is_empty() {
                let shown: Vec<&str> = changed.iter().take(10).map(String::as_str).collect();
                let more = if changed.len() > shown.len() { ", …" } else { "" };
                eprintln!(
                    "note: {} models changed their input or output price more than twofold: {}{more}",
                    changed.len(),
                    shown.join(", ")
                );
            }
            new.write(&path)?;
            let source = PriceSource {
                source: new.source.clone(),
                url: Some(new.url.clone()),
                sha256: Some(new.sha256.clone()),
                ..Default::default()
            };
            println!("{}: {} models, {}", path.display(), new.models.len(), origin(&source));
            println!(
                "Runs started from now on use these prices; one already running keeps the prices it started with."
            );
        }
        PricesCmd::Show { model: None } => {
            let table = PriceTable::from_env()?;
            if let Some(path) = prices::snapshot_path(var).filter(|p| p.exists()) {
                println!("snapshot: {}", path.display());
            } else {
                println!("snapshot: the one built into this release (`molt prices update` downloads a newer one)");
            }
            for (source, models) in table.sources() {
                println!("{models} models: {}", origin(&source));
            }
        }
        PricesCmd::Show { model: Some(model) } => {
            let table = PriceTable::from_env()?;
            let id = molt_gateway::resolve_model(&model);
            let Some((p, source)) = table.lookup(&id) else {
                eprintln!(
                    "no price for {id} (looked for {}): its calls' cost is unknown, so a run with it stops after \
                     the first call. Run `molt prices update`, or set {}={id}=input,output in US dollars per \
                     million tokens",
                    prices::names(&id).join(", "),
                    prices::OVERRIDE_VAR
                );
                return Ok(ExitCode::FAILURE);
            };
            // Rounded to a billionth of a dollar, which hides float noise such as 0.35000000000000003.
            let usd = |v: f64| (v * 1e9).round() / 1e9;
            println!(
                "{id}: per million tokens ${} input, ${} output, ${} cache write, ${} cache read",
                usd(p.input),
                usd(p.output),
                usd(p.cache_write),
                usd(p.cache_read)
            );
            println!("entry {} in {}", source.entry, origin(&source));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cfg_scratch(args: &[String]) -> anyhow::Result<PathBuf> {
    let path =
        args.windows(2).find(|pair| pair[0] == "--scratch").context("molt-tools shell must declare --scratch")?;
    let path = PathBuf::from(&path[1]);
    Ok(path.canonicalize().unwrap_or(path))
}

/// The config and canonical workspace a run of `molt do` or `molt resume` uses.
fn prepare(config: Option<&Path>, args: &RunArgs) -> anyhow::Result<(Config, String)> {
    let (workspace, project) = agent::locate(args.project.as_deref(), &args.workspace, config.is_some())?;
    let mut cfg = agent::resolve_config(config, &workspace, args.data_dir.as_deref(), project.as_ref())?;
    if let Some(path) = config {
        eprintln!("config: {}", agent::printable(&path.display().to_string()));
    }
    if let Some(shell) = cfg.service_mut("shell") {
        let standard = shell
            .exec
            .as_ref()
            .is_some_and(|exec| Path::new(&exec.command).file_name().is_some_and(|name| name == "molt-tools"));
        anyhow::ensure!(
            standard || (!args.no_sandbox && args.sandbox_policy.is_none() && args.pass_env.is_empty()),
            "shell policy flags require the molt-tools shell service"
        );
        if let Some(exec) = shell.exec.as_mut().filter(|_| standard) {
            if args.no_sandbox {
                exec.args.push("--no-sandbox".into());
            }
            if let Some(path) = &args.sandbox_policy {
                let roots = molt_tools::Roots { root: workspace.clone(), scratch: cfg_scratch(&exec.args)? };
                let policy = molt_tools::SandboxPolicy::load(path, &roots)?;
                shell.pass_env.extend(policy.environment);
                exec.args.extend(["--sandbox-policy".into(), path.canonicalize()?.to_string_lossy().into_owned()]);
            }
            for name in &args.pass_env {
                exec.args.extend(["--sandbox-env".into(), name.clone()]);
            }
        }
        shell.pass_env.extend(args.pass_env.iter().cloned());
    }
    if args.no_memory {
        cfg.services.retain(|s| s.name.as_str() != agent::MEMORY);
    }
    agent::allow_web(&mut cfg, &args.allow_web)?;
    if std::env::var_os("RUST_LOG").is_none() {
        for svc in &mut cfg.services {
            svc.env.entry("RUST_LOG".into()).or_insert_with(|| "warn".into());
        }
    }
    let workspace = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    Ok((cfg, workspace))
}

async fn do_task(config: Option<&Path>, args: DoArgs) -> anyhow::Result<ExitCode> {
    // From the start, so that no signal leaves services running.
    let stop = molt::stop_signal()?;
    let (cfg, workspace) = prepare(config, &args.common)?;
    let mut req = RunRequest::new(args.task, workspace);
    req.check = args.check;
    req.verify = !args.no_check;
    req.attempts = args.attempts;
    req.select = args.select;
    req.model = args.model;
    req.effort = args.effort;
    req.max_turns = args.max_turns;
    req.syntax_check = args.syntax_check;
    req.judge_model = args.judge_model;
    req.budget_usd = args.common.budget_usd;
    req.apply = !args.common.no_apply;
    req.stream = args.common.stream;
    req.checkpoint = args.checkpoint;
    req.review = !args.common.no_review;
    execute(&cfg, req, TraceId::random(), &args.common, stop).await
}

async fn resume_task(config: Option<&Path>, args: ResumeArgs) -> anyhow::Result<ExitCode> {
    let stop = molt::stop_signal()?;
    let (cfg, workspace) = prepare(config, &args.common)?;
    let dir = agent::checkpoint_dir(&cfg).context("this config's planner keeps no checkpoints (it has no --state)")?;
    let here: Vec<checkpoint::Saved> = checkpoint::list(&dir)
        .into_iter()
        .filter_map(|(run, saved)| match saved {
            Ok(saved) => Some(saved),
            Err(e) => {
                eprintln!("{run}: {e}");
                None
            }
        })
        .filter(|saved| saved.request.workspace == workspace)
        .collect();
    if args.list {
        if here.is_empty() {
            eprintln!("no stopped run to resume in {}", agent::printable(&workspace));
        }
        for saved in &here {
            println!("{}", agent::printable(&stopped_run(saved)));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let run = match args.run {
        Some(run) => run,
        None => here
            .first()
            .map(|saved| saved.run.clone())
            .with_context(|| format!("no stopped run to resume in {}", agent::printable(&workspace)))?,
    };
    let saved = checkpoint::load(&dir, &run).map_err(|e| anyhow::anyhow!(e))?;
    let mut req = saved.request;
    req.workspace = workspace;
    req.resume = Some(Resume { run: run.clone(), retry: args.retry });
    req.checkpoint = true;
    req.apply = !args.common.no_apply;
    req.stream = args.common.stream;
    req.budget_usd = args.common.budget_usd.or(req.budget_usd);
    eprintln!("resuming run {run}: {}", agent::printable(req.task.lines().next().unwrap_or_default()));
    execute(&cfg, req, TraceId::from_raw(&run), &args.common, stop).await
}

/// One line about a stopped run: its id, when it stopped, what it spent,
/// its attempts and its task.
fn stopped_run(saved: &checkpoint::Saved) -> String {
    let ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| (now.as_millis() as u64).saturating_sub(saved.saved_ms) / 60_000);
    let attempts: Vec<String> = saved
        .attempts
        .iter()
        .map(|a| match &a.end {
            Some(end) => format!("{}: {:?} after {} turns", a.index, end.status, a.turns).to_lowercase(),
            None => format!("{}: stopped after {} turns", a.index, a.turns),
        })
        .collect();
    let task: String = saved.request.task.lines().next().unwrap_or_default().chars().take(60).collect();
    format!(
        "{}  {ago} min ago  ${:.2}  [{}]  {task}",
        saved.run,
        saved.spend.cost_usd,
        if attempts.is_empty() { "no attempt yet".to_owned() } else { attempts.join(", ") }
    )
}

/// Carry out `req` under the run id `trace` and report it.
async fn execute(
    cfg: &Config,
    req: RunRequest,
    trace: TraceId,
    args: &RunArgs,
    stop: impl std::future::Future<Output = i32>,
) -> anyhow::Result<ExitCode> {
    let workspace = req.workspace.clone();
    let apply = req.apply;
    let checkpointed = req.checkpoint;
    let run = trace.to_string();
    let task = req.task.clone();
    // Looked at before the run, so that its own changes are told apart from the user's.
    let target = match args.commit.then(|| commit_target(&req.workspace, args.branch.as_deref())).transpose()? {
        Some(Err(e)) => {
            eprintln!("{}", agent::printable(&format!("note: {e:#}; the result will be applied but not committed")));
            Some(Err(e))
        }
        other => other,
    };
    let mut signal = None;
    let stopped = async { signal = Some(stop.await) };
    let progress = |event: &_| eprintln!("{}", agent::describe(event));
    let learn = (!args.no_learn)
        .then(|| agent::Learn { model: agent::memory_model(args.memory_model.as_deref(), req.model.as_deref()) });
    let result = agent::run_task_as(cfg, req, trace, learn, progress, stopped).await;
    let done = match result {
        Ok(done) => done,
        Err(e) if e.is::<Interrupted>() => {
            eprintln!("interrupted; the services are stopped");
            if checkpointed {
                eprintln!("the run's checkpoint and forks are kept: go on with `molt resume {run}`");
            }
            let status = signal.map_or(130, |n| 128 + n);
            return Ok(ExitCode::from(u8::try_from(status).unwrap_or(130)));
        }
        Err(e) => return Err(e),
    };
    let resp = done.run;
    let committed = match target {
        Some(target) if resp.applied => {
            Some(target.and_then(|before| before.commit(&resp, &run, &task, args.branch.as_deref())))
        }
        _ => None,
    };
    if args.json {
        let mut out = serde_json::to_value(&resp)?;
        out["run"] = serde_json::json!(run);
        out["workspace"] = serde_json::json!(workspace);
        if let Some(committed) = &committed {
            out["commit"] = match committed {
                Ok(Some(c)) => serde_json::json!({ "sha": c.sha, "branch": c.branch, "ignored": c.ignored }),
                Ok(None) => serde_json::Value::Null,
                Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
            };
        }
        // What learning did and cost, next to the run's own spend.
        out["learning"] = done.learned.as_ref().map_or(serde_json::Value::Null, agent::learning_json);
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print!("{}", agent::report_at(&resp, Some(&workspace)));
    }
    if let Some(learned) = &done.learned {
        eprintln!("{}", agent::learned(learned));
    }
    let not_committed = match &committed {
        None => false,
        Some(Ok(None)) => {
            eprintln!("nothing to commit: the workspace's commit has these files as they are");
            false
        }
        Some(Ok(Some(c))) => {
            let on = c.branch.as_deref().map_or_else(|| "a detached HEAD".to_owned(), |b| format!("branch {b}"));
            eprintln!("{}", agent::printable(&format!("committed {} on {on}: {}", &c.sha[..12], c.subject)));
            if !c.ignored.is_empty() {
                eprintln!("{}", agent::printable(&format!("left out, as git ignores them: {}", c.ignored.join(", "))));
            }
            false
        }
        Some(Err(e)) => {
            eprintln!(
                "{}",
                agent::printable(&format!("not committed: {e:#}. The result is applied; commit it yourself"))
            );
            true
        }
    };
    // A signal while memory learned from the run still ends it as stopped.
    if let Some(n) = signal {
        return Ok(ExitCode::from(u8::try_from(128 + n).unwrap_or(130)));
    }
    Ok(match resp.outcome {
        Outcome::Failed => ExitCode::FAILURE,
        // It passed, but the workspace does not have it: a script must not go on as if it did.
        Outcome::Passed | Outcome::Unverified | Outcome::Judged if apply && !resp.applied && resp.fork.is_some() => {
            ExitCode::from(3)
        }
        Outcome::Passed | Outcome::Unverified | Outcome::Judged if not_committed => ExitCode::from(4),
        Outcome::Passed | Outcome::Unverified | Outcome::Judged => ExitCode::SUCCESS,
    })
}

/// The work tree `molt do --commit` commits in, looked at before the run;
/// an inner error when the workspace is in none (the run still goes ahead
/// and is applied). A `--branch` git would refuse is an error before
/// anything starts.
fn commit_target(workspace: &str, branch: Option<&str>) -> anyhow::Result<anyhow::Result<git::Before>> {
    let before = match git::Before::look(Path::new(workspace)) {
        Ok(before) => before,
        Err(e) => return Ok(Err(e.context("--commit"))),
    };
    if let Some(name) = branch {
        before.repo().check_new_branch(name)?;
    }
    Ok(Ok(before))
}

async fn undo_cmd(config: Option<&Path>, args: UndoArgs) -> anyhow::Result<ExitCode> {
    let (workspace, project) = agent::locate(args.at.project.as_deref(), &args.at.workspace, config.is_some())?;
    let repo = git::Repo::open(&workspace)?;
    let run = match repo.undo(args.run.as_deref())? {
        git::Undo::Reverted { run, commit, sha } => {
            println!("undid run {run}: reverted {} in {}", &commit[..12], &sha[..12]);
            run
        }
        git::Undo::Already { run, sha } => {
            println!("run {run} was undone already, in {}", &sha[..12]);
            run
        }
    };
    let cfg = agent::memory_config(config, &workspace, args.at.data_dir.as_deref(), project.as_ref())?;
    if !agent::memory_db(&cfg).is_some_and(|db| db.is_file()) {
        eprintln!("memory has nothing about this project, so nothing to take back there");
        return Ok(ExitCode::SUCCESS);
    }
    let req = UndoRequest { run: run.clone(), reason: "the user undid the run with `molt undo`".into() };
    match agent::call_memory(&cfg, memory::UNDO, serde_json::to_value(req)?).await {
        Ok(reply) => {
            let reply: RetractResponse = serde_json::from_value(reply)?;
            eprintln!(
                "memory no longer counts the run: {} notes only it showed forgotten, {} more worked out again",
                reply.retracted, reply.adjusted
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("the commit is taken back, but memory could not be told: {e:#}. Run `molt undo {run}` again");
            Ok(ExitCode::FAILURE)
        }
    }
}

async fn memory_cmd(config: Option<&Path>, cmd: MemoryCmd) -> anyhow::Result<()> {
    let at = match &cmd {
        MemoryCmd::Show { at, .. }
        | MemoryCmd::Forget { at, .. }
        | MemoryCmd::Remember { at, .. }
        | MemoryCmd::Promote { at, .. }
        | MemoryCmd::Map { at, .. }
        | MemoryCmd::Used { at, .. } => at,
        MemoryCmd::Review(r) | MemoryCmd::Correct { review: r, .. } => &r.at,
    };
    let (workspace, project) = agent::locate(at.project.as_deref(), &at.workspace, config.is_some())?;
    let path = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    // Notes about a project are kept under its id; the service maps the path to it.
    let key = project.as_ref().map_or_else(|| path.clone(), |p| p.id.clone());
    let cfg = agent::memory_config(config, &workspace, at.data_dir.as_deref(), project.as_ref())?;
    let open = || -> anyhow::Result<molt_memory::Db> {
        let db = agent::memory_db(&cfg).context("the memory service has no --db")?;
        anyhow::ensure!(db.is_file(), "memory has nothing yet ({} does not exist)", db.display());
        let db = molt_memory::Db::open(&db)?;
        if let Some(project) = &project {
            let paths = if project.paths.is_empty() { vec![workspace.clone()] } else { project.paths.clone() };
            molt_memory::adopt(&db, &molt_memory::Project { id: project.id.clone(), paths })?;
        }
        Ok(db)
    };
    match cmd {
        MemoryCmd::Show { words, n, json, .. } => {
            let req = RecallRequest {
                query: words.join(" "),
                workspace: Some(path),
                k: Some(n),
                include_review: true,
                ..Default::default()
            };
            // Dependency checks can change note state: keep those checks
            // on the audited service path too, rather than editing offline.
            // A project with no notes of its own yet still has the user's.
            if project.is_none() || agent::memory_db(&cfg).is_some_and(|db| db.is_file()) {
                drop(open()?);
            }
            let response: RecallResponse =
                serde_json::from_value(agent::call_memory(&cfg, memory::RECALL, serde_json::to_value(req)?).await?)?;
            let notes = response.notes;
            if json {
                println!("{}", serde_json::to_string_pretty(&notes)?);
            } else if notes.is_empty() {
                eprintln!("no notes");
            } else {
                print!("{}", agent::printable(&show_notes(&notes)));
            }
        }
        MemoryCmd::Used { run, json, .. } => {
            let snapshots =
                molt_memory::recall_snapshots(&open()?, &key, &run).map_err(|e| anyhow::anyhow!(e.message))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else if snapshots.is_empty() {
                eprintln!("no recorded memory context for run {run} in this project");
            } else {
                for snapshot in snapshots {
                    let heading = format!(
                        "run {}, call {} ({})\nquery: {}\n",
                        snapshot.run, snapshot.call, snapshot.purpose, snapshot.query
                    );
                    print!("{}", agent::printable(&heading));
                    if snapshot.notes.is_empty() {
                        println!("no notes were returned");
                    } else {
                        print!("{}", agent::printable(&show_notes(&snapshot.notes)));
                    }
                }
            }
        }
        MemoryCmd::Review(r) => change_note(&cfg, path, r, None).await?,
        MemoryCmd::Correct { review, text } => change_note(&cfg, path, review, Some(text)).await?,
        MemoryCmd::Remember { text, global, kind, .. } => {
            let req = RememberRequest {
                kind,
                text,
                workspace: (!global).then_some(path),
                confidence: Some(0.95),
                trace: TraceId::random().to_string(),
                events: Vec::new(),
                version: "user".into(),
            };
            let reply: RememberResponse =
                serde_json::from_value(agent::call_memory(&cfg, memory::REMEMBER, serde_json::to_value(req)?).await?)?;
            let layer = if reply.note.workspace.is_none() { "for every project" } else { "for this project" };
            let verb = if reply.reinforced { "already knew" } else { "remembered" };
            println!("{}", agent::printable(&format!("{verb} {} {layer}: {}", reply.note.id, reply.note.text)));
        }
        MemoryCmd::Promote { id, reason, .. } => {
            let req = serde_json::to_value(PromoteRequest { workspace: path, id, reason })?;
            let reply: PromoteResponse = serde_json::from_value(agent::call_memory(&cfg, memory::PROMOTE, req).await?)?;
            println!("{}", agent::printable(&format!("{} for every project: {}", reply.note.id, reply.note.text)));
        }
        MemoryCmd::Forget { id, reason, .. } => {
            let req = serde_json::to_value(ForgetRequest { id: id.clone(), reason })?;
            let reply: ForgetResponse = serde_json::from_value(agent::call_memory(&cfg, memory::FORGET, req).await?)?;
            anyhow::ensure!(reply.forgotten, "there is no note {id}, or it is already forgotten");
            println!("forgot {id}");
        }
        MemoryCmd::Map { words, tokens, .. } => {
            let db = open()?;
            let skip = agent::memory_skips(&cfg);
            molt_memory::project::index_skipping(&db, &workspace, None, &skip)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            let req = MapRequest { workspace: path, query: words.join(" "), max_tokens: Some(tokens) };
            let map = molt_memory::project::map(&db, &workspace, &req).map_err(|e| anyhow::anyhow!(e.message))?;
            print!("{}", agent::printable(&map.map));
            eprintln!("{} files, {} definitions, about {} tokens", map.files, map.symbols, map.tokens);
        }
    }
    Ok(())
}

fn project_cmd(cmd: ProjectCmd) -> anyhow::Result<()> {
    let registry = molt::project::Registry::user()?;
    let dir = |d: &Path| -> anyhow::Result<PathBuf> {
        let d = d.canonicalize().with_context(|| format!("{}", d.display()))?;
        anyhow::ensure!(d.is_dir(), "{} is not a directory", d.display());
        Ok(d)
    };
    let show = |p: &molt::project::Project| -> anyhow::Result<()> {
        let dirs = if p.paths.is_empty() {
            "no folder".to_owned()
        } else {
            p.paths.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
        };
        let files = if p.paths.is_empty() {
            format!("\n    files: {}", registry.workspace(p)?.display())
        } else {
            String::new()
        };
        println!(
            "{}",
            agent::printable(&format!("{}  {}  {dirs}{files}\n    data: {}", p.id, p.name, p.data_dir.display()))
        );
        Ok(())
    };
    match cmd {
        ProjectCmd::New { name, path } => {
            let path = path.as_deref().map(dir).transpose()?;
            show(&registry.create(&name, path.as_deref(), |id| agent::project_data_dir(&name, id))?)?;
        }
        ProjectCmd::List => {
            let projects = registry.list()?;
            if projects.is_empty() {
                eprintln!("no projects yet; one is registered the first time molt runs in a directory");
            }
            for project in &projects {
                show(project)?;
            }
        }
        ProjectCmd::Path { project } => {
            println!("{}", registry.workspace(&registry.find(&project)?)?.display());
        }
        ProjectCmd::Move { project, new_dir, from } => {
            show(&registry.move_path(&project, &dir(&new_dir)?, from.as_deref())?)?;
        }
        ProjectCmd::Add { project, dir: d } => show(&registry.add_path(&project, &dir(&d)?)?)?,
    }
    Ok(())
}

async fn change_note(cfg: &Config, workspace: String, r: ReviewArgs, text: Option<String>) -> anyhow::Result<()> {
    let req = ReviewRequest {
        workspace,
        id: r.id,
        expected_rev: r.rev,
        reason: r.reason,
        depends_on: r.depends_on,
        temporary: r.temporary,
    };
    let (target, payload) = match text {
        Some(text) => (memory::CORRECT, serde_json::to_value(CorrectRequest { review: req, text })?),
        None => (memory::REVIEW, serde_json::to_value(req)?),
    };
    let reply: ReviewResponse = serde_json::from_value(agent::call_memory(cfg, target, payload).await?)?;
    println!("{}", agent::printable(&format!("{} revision {}: {}", reply.note.id, reply.note.rev, reply.note.text)));
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
        if r.layer == Layer::User {
            extra.push("yours, for every project".into());
        }
        if r.details.everywhere {
            extra.push("proposed for every project; `molt memory promote` it if so".into());
        }
        if let Some(to) = &r.details.promoted_to {
            extra.push(format!("promoted as {to}"));
        }
        if r.details.temporary {
            extra.push("temporary experience; verify before reuse".into());
        }
        if !r.details.needs_review.is_empty() {
            extra.push(format!("needs review: {}", r.details.needs_review.join(", ")));
        }
        let extra = if extra.is_empty() { String::new() } else { format!(" ({})", extra.join("; ")) };
        out.push_str(&format!(
            "{} rev {}  {} {:.2}{extra}\n    {}\n",
            n.id,
            n.rev,
            n.kind.as_str(),
            n.confidence,
            n.text
        ));
        out.push_str(&format!(
            "    source: {}/{} run {} events {}\n",
            n.provenance.service,
            n.provenance.version,
            n.provenance.trace,
            n.provenance.events.join(", ")
        ));
        out.push_str(&format!(
            "    recall: score {:.3}, confidence {:.3}, recency {:.3}, keywords {}\n",
            r.score,
            r.reason.confidence,
            r.reason.recency,
            r.reason.keywords.as_deref().unwrap_or("none (confidence and recency)")
        ));
        if !r.details.dependencies.is_empty() {
            out.push_str(&format!(
                "    depends on: {}\n",
                r.details.dependencies.iter().map(|d| d.path.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if !r.details.reason.is_empty() {
            out.push_str(&format!("    review reason: {}\n", r.details.reason));
        }
    }
    out
}
