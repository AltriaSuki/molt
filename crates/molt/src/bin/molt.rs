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
use molt_api::plan::PlanRequest;
use molt_api::planner::{Criterion, Outcome, Resume, RunRequest, Schedule, Select};
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
    /// Use the `[profile.NAME]` defaults of the user config (or of the
    /// --config file): model, effort, attempts, budget and the rest of `[tui]`.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,
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
    /// Keep the services of a workspace up for clients of the client
    /// protocol (docs/serve.md): on a Unix socket only you can open, or on
    /// standard input and output with --stdio. The interface is one such client.
    Serve(ServeArgs),
    /// Work for an editor that speaks the Agent Client Protocol, such as Zed
    /// or a JetBrains IDE (docs/acp.md): the editor starts it and talks to it
    /// on standard input and output. A result is applied only when you allow it there.
    Acp(AcpArgs),
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
    /// Answer a question about a workspace: one attempt with tools that only
    /// read, in the workspace itself. Nothing is changed, and the answer is
    /// unverified.
    Ask(AskArgs),
    /// Go on with a run of `molt do --checkpoint` that stopped before it
    /// finished, from its last checkpoint. Its attempts keep their forks and
    /// conversations, and the cost so far counts against its budget.
    Resume(ResumeArgs),
    /// List the runs of a project from its audit log, oldest first: what each
    /// was asked, how it ended, what it cost and took, what memory learned
    /// from it and where its result went. `molt runs show RUN` shows one run.
    Runs(RunsArgs),
    /// Take back the commit `molt do --commit` made for a run, with a new
    /// commit that puts its files back, and have memory no longer count
    /// what the run showed.
    Undo(UndoArgs),
    /// Apply a result kept in its fork, such as one of `molt do --no-apply`,
    /// or only some of its files. With --check, the check runs again first,
    /// on just what is applied; a fork you edited by hand is checked the same way.
    Apply(ApplyArgs),
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
    /// The environment a workspace's commands get in the sandbox.
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
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
    /// See the models molt knows, and find out what one does on its endpoint.
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Work with the audit log.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
    /// Create NATS secrets (if missing) and print the nats-server settings for them.
    NatsConfig,
    /// See the configuration a run in a workspace uses.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// See the system prompts runs use, and promote a protected one of your
    /// prompt pack (~/.config/molt/prompts/).
    Prompts {
        #[command(subcommand)]
        cmd: PromptsCmd,
    },
    /// Install, approve, disable and remove extension packs (docs/extensions.md):
    /// MCP servers, hooks, skills and commands bundled in a directory or an
    /// archive with a molt-extension.toml.
    Ext {
        #[command(subcommand)]
        cmd: ExtCmd,
    },
}

#[derive(Subcommand)]
enum ExtCmd {
    /// Install a pack from a directory or a .tar, .tar.gz or .tgz archive,
    /// or update the installed one of its name. It shows what the pack
    /// would get and installs it only once you approve, at a terminal or
    /// with --yes; an update that asks for nothing more needs no approval.
    /// Nothing the pack holds is run. A Claude Code plugin is imported.
    Add {
        path: PathBuf,
        /// Approve what the pack would get without being asked, as from a script.
        #[arg(long)]
        yes: bool,
    },
    /// List the installed packs.
    List,
    /// Show where a pack is and what you approved it for.
    Show { name: String },
    /// Stop starting a pack's servers, hooks, skills and commands, and keep it installed.
    Disable { name: String },
    /// Start a disabled pack again, as you approved it.
    Enable { name: String },
    /// Uninstall a pack: its files, its approval and its servers' cached tool lists.
    Remove { name: String },
}

#[derive(Subcommand)]
enum PromptsCmd {
    /// List every prompt with its version (the first digits of its SHA-256),
    /// whether it is built in or yours, and whether a protected one of yours
    /// is promoted; then the pack's roles and workflows.
    List,
    /// Print a prompt's text: your pack's file, else the built-in one.
    Show {
        name: String,
        /// The built-in text, whatever your pack holds.
        #[arg(long)]
        builtin: bool,
    },
    /// Promote your pack's NAME.md as it is now. The reviewer's, the judge's
    /// and the vetter's prompts, and any prompt a role of theirs speaks
    /// with, take effect only once promoted, and again after each change.
    /// It asks you, at a terminal, to type the name back.
    Promote { name: String },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the configuration in effect: the built-in setup with the user
    /// config (~/.config/molt/config.toml), the --profile, the project's
    /// .molt/config.toml and the environment over it, or the --config file.
    /// Secret-looking variables are redacted.
    Show {
        /// Print one `key = value` line per value, with the layer it comes from.
        #[arg(long)]
        origin: bool,
        #[command(flatten)]
        at: MemoryAt,
    },
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
    /// A criterion the result must meet, for a task no command can check,
    /// such as writing (repeatable): an independent judge decides each one,
    /// and no check is designed.
    #[arg(long, value_name = "TEXT", value_parser = criterion, conflicts_with_all = ["check", "no_check"])]
    criterion: Vec<String>,
    /// Parallel attempts [default: `[tui] attempts` of the config, else 2].
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=8))]
    attempts: Option<u32>,
    /// How to pick among attempts that pass: `first` takes the first and
    /// cancels the rest; `wait:<seconds>` gives the others that long to pass
    /// too, and `all` lets every attempt finish, then the passing ones are
    /// compared (project suite, diff size, review).
    #[arg(long, default_value = "first", value_name = "first|wait:<seconds>|all")]
    select: Select,
    /// How the attempts start: `fixed` starts --attempts at once; `adaptive`
    /// starts one, on the cheapest of MOLT_ESCALATION_MODELS, and adds more,
    /// up to --attempts, when the evidence and the budget call for it.
    /// Default: the planner's MOLT_SCHEDULE, else fixed.
    #[arg(long, value_name = "fixed|adaptive")]
    schedule: Option<Schedule>,
    /// The models an adaptive schedule escalates through, cheapest first,
    /// separated by commas, such as haiku,sonnet,opus. Default: the
    /// planner's MOLT_ESCALATION_MODELS, else none: every attempt keeps
    /// the run's model.
    #[arg(long, value_name = "MODELS", value_delimiter = ',', requires = "schedule")]
    escalate: Vec<String>,
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
    /// The workflow to run: verify (design or take a check, race the
    /// attempts, review), quick (one attempt, the given check or none, the
    /// fixed review rules), ask (answer, change nothing), auto (the planner
    /// picks one of those; never ask with --check), or one the planner's
    /// configuration defines. Default: verify.
    #[arg(long, value_name = "NAME")]
    workflow: Option<String>,
    /// Carry the task out as ordered steps, each a run with its own
    /// designed check, the next going on from the last one's result; then
    /// the overall check (--check, or one designed for the whole task) runs
    /// on the result. The planner plans the steps, and they run as planned.
    /// One budget covers every step. See docs/plan.md.
    #[arg(long, conflicts_with_all = ["checkpoint", "criterion"])]
    plan: bool,
    /// A step of the plan, in order (repeatable), to run instead of the
    /// planned ones: one goal sentence each. Implies --plan.
    #[arg(long, value_name = "TEXT", conflicts_with_all = ["checkpoint", "criterion"])]
    step: Vec<String>,
    #[command(flatten)]
    common: RunArgs,
}

#[derive(Args)]
struct AskArgs {
    /// The question, in plain words.
    question: String,
    /// Model for the answer: opus, sonnet, haiku or a model id.
    #[arg(long)]
    model: Option<String>,
    /// How hard the model thinks: low, medium, high, xhigh or max.
    #[arg(long)]
    effort: Option<Effort>,
    /// Model turns for the answer.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: Option<u32>,
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
    /// The most designing the check may spend, in US dollars, out of the
    /// run's budget (at most all of it). Default: a quarter of the run's
    /// budget, or the share MOLT_DESIGN_BUDGET_SHARE sets. Checked before each
    /// model call, like the run's budget.
    #[arg(long, value_name = "USD", value_parser = usd)]
    design_budget_usd: Option<f64>,
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
    /// The session the run belongs to, a name that groups the runs of one
    /// piece of work in `molt runs --session`. For `molt resume`, it
    /// replaces the stopped run's.
    #[arg(long, value_name = "NAME", value_parser = session_name)]
    session: Option<String>,
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
    /// Let commands build offline from your dependency cache. `cargo`
    /// mounts $CARGO_HOME/registry and git (default ~/.cargo) read-only and
    /// gives commands a private CARGO_HOME over them, with CARGO_NET_OFFLINE.
    #[arg(long, value_name = "KIND", value_parser = ["cargo"], conflicts_with = "no_sandbox")]
    deps_cache: Vec<String>,
    /// Let the agent read web pages from this host (repeatable): `docs.rs`,
    /// or `*.python.org` for the hosts below it. Pages are fetched over
    /// https by a service of their own and recorded in the audit log;
    /// commands still have no network.
    #[arg(long, value_name = "HOST")]
    allow_web: Vec<String>,
    /// DNS for web pages: system (default), or doh:https://PUBLIC_IP/dns-query
    /// using a DNS JSON HTTPS endpoint such as Cloudflare's 1.1.1.1.
    #[arg(long, value_name = "RESOLVER")]
    web_resolver: Option<String>,
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
    /// else the attempts' model.
    #[arg(long, value_name = "MODEL", conflicts_with_all = ["no_learn", "no_memory"])]
    memory_model: Option<String>,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct RunsArgs {
    #[command(subcommand)]
    cmd: Option<RunsCmd>,
    /// Group the runs by session; with a NAME, list only that session's runs.
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    session: Option<String>,
    /// Only the newest N runs.
    #[arg(short = 'n', long, value_name = "N")]
    last: Option<usize>,
    /// Print the runs as JSON.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    at: MemoryAt,
}

#[derive(Subcommand)]
enum RunsCmd {
    /// One run: its task and check, each attempt, the files it changed,
    /// the notes memory gave it and the notes memory learned from it.
    Show {
        /// The run's id, or enough of its start to tell it from the others.
        run: String,
        /// Print the run, its request and its reply as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        at: MemoryAt,
    },
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
    /// Let commands build offline from your dependency cache: `cargo` mounts
    /// $CARGO_HOME/registry and git (default ~/.cargo) read-only.
    #[arg(long, value_name = "KIND", value_parser = ["cargo"])]
    deps_cache: Vec<String>,
    /// Let the agent read web pages from this host (repeatable): `docs.rs`,
    /// or `*.python.org` for the hosts below it.
    #[arg(long, value_name = "HOST")]
    allow_web: Vec<String>,
    /// DNS for web pages: system (default), or doh:https://PUBLIC_IP/dns-query
    /// using a DNS JSON HTTPS endpoint such as Cloudflare's 1.1.1.1.
    #[arg(long, value_name = "RESOLVER")]
    web_resolver: Option<String>,
    /// The session the runs belong to (`molt runs --session`). Default: a
    /// new one each time the interface is opened, or for each client of
    /// `molt serve`.
    #[arg(long, value_name = "NAME", value_parser = session_name)]
    session: Option<String>,
}

#[derive(Args)]
struct ServeArgs {
    #[command(flatten)]
    at: UiArgs,
    /// The socket to listen on. Default: serve.sock in the data dir.
    #[arg(long, conflicts_with = "stdio")]
    socket: Option<PathBuf>,
    /// Serve one client on standard input and output instead of a socket.
    #[arg(long)]
    stdio: bool,
}

#[derive(Args)]
struct AcpArgs {
    /// Pass this variable from your environment to the commands the agent
    /// runs (repeatable).
    #[arg(long, value_name = "NAME", value_parser = env_name)]
    pass_env: Vec<String>,
    /// Let commands build offline from your dependency cache: `cargo` mounts
    /// $CARGO_HOME/registry and git (default ~/.cargo) read-only.
    #[arg(long, value_name = "KIND", value_parser = ["cargo"])]
    deps_cache: Vec<String>,
    /// Let the agent read web pages from this host (repeatable): `docs.rs`,
    /// or `*.python.org` for the hosts below it.
    #[arg(long, value_name = "HOST")]
    allow_web: Vec<String>,
    /// DNS for web pages: system (default), or doh:https://PUBLIC_IP/dns-query
    /// using a DNS JSON HTTPS endpoint such as Cloudflare's 1.1.1.1.
    #[arg(long, value_name = "RESOLVER")]
    web_resolver: Option<String>,
}

#[derive(Args)]
struct UndoArgs {
    /// The run to take back, as its commit's `Molt-Run:` trailer names it.
    /// Default: the newest run on the current branch not taken back yet.
    run: Option<String>,
    #[command(flatten)]
    at: MemoryAt,
}

#[derive(Args)]
#[command(after_help = "Exit status: 0 when the result is applied; 1 when the check failed on it, or on an error, \
                        and then nothing is applied and the fork stays; 2 for a usage error.")]
#[command(group(clap::ArgGroup::new("verify").args(["check", "no_check"])))]
struct ApplyArgs {
    /// The fork the result is in, as the report of the run names it.
    fork: String,
    /// Apply only this file of the result (repeatable). The rest is not
    /// applied, and the fork is dropped. Give --check or --no-check with it:
    /// part of a result is not what was checked.
    #[arg(long, value_name = "PATH", requires = "verify")]
    only: Vec<String>,
    /// Run this check first, in a fork of the workspace with only what
    /// would be applied, and apply nothing if it fails.
    #[arg(long, value_name = "CMD")]
    check: Option<String>,
    /// A file of the result the check needs, which is put beside the chosen
    /// files for the check even when it is not applied (repeatable).
    #[arg(long, value_name = "PATH", requires = "check")]
    check_file: Vec<String>,
    /// Apply the chosen files without running a check on them.
    #[arg(long, conflicts_with = "check")]
    no_check: bool,
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
enum EnvCmd {
    /// Say which environment profiles apply to the workspace, run their
    /// probes in the sandbox as a run's commands would, and give a fix for
    /// each that fails. Exits 1 when one fails.
    Doctor {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// The trusted shell sandbox JSON policy runs use, if any.
        #[arg(long)]
        sandbox_policy: Option<PathBuf>,
    },
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
    /// Write a promoted skill as an Agent Skills SKILL.md: to stdout, or to
    /// DIR/NAME/SKILL.md (for example ~/.config/molt/skills). Drafts and
    /// disabled skills are not exported.
    Export {
        id: String,
        #[arg(long)]
        out: Option<PathBuf>,
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

fn criterion(text: &str) -> Result<String, String> {
    match text.trim() {
        "" => Err("a criterion is empty: say in one sentence what the result must satisfy".into()),
        text => Ok(text.to_owned()),
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

/// A name `--session` may give: see [`molt::runs::session_name`].
fn session_name(name: &str) -> Result<String, String> {
    molt::runs::session_name(name)
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
enum ModelsCmd {
    /// Show the default model, the aliases and what probes found; or, for
    /// MODEL, where its calls go, what it does and what it costs. Nothing
    /// is called.
    Show {
        /// opus, sonnet, haiku, an alias or a model id.
        model: Option<String>,
    },
    /// Find out what MODEL does on its endpoint: whether it calls tools,
    /// streams, keeps to a JSON schema (output_schema) and takes an effort.
    /// It makes up to four small calls, billed as usual, so it says what it
    /// will call and asks first; nothing else ever probes. What it finds is
    /// kept in ~/.local/share/molt/models.json (or MOLT_MODELS_FILE), and
    /// the gateway goes by it from the next run on.
    Probe {
        /// opus, sonnet, haiku, an alias or a model id.
        model: String,
        /// Make the calls without asking, as from a script.
        #[arg(long)]
        yes: bool,
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

fn serve_options(config: Option<PathBuf>, profile: Option<String>, args: UiArgs) -> molt::serve::Options {
    molt::serve::Options {
        config,
        workspace: args.workspace,
        project: args.project,
        data_dir: args.data_dir,
        pass_env: args.pass_env,
        deps_cache: args.deps_cache,
        allow_web: args.allow_web,
        web_resolver: args.web_resolver,
        profile,
        session: args.session,
    }
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
                | Cmd::Ask(_)
                | Cmd::Resume(_)
                | Cmd::Runs(_)
                | Cmd::Undo(_)
                | Cmd::Apply(_)
                | Cmd::Memory { .. }
                | Cmd::Bench { .. }
                | Cmd::Prices { .. }
                | Cmd::Models { .. }
                | Cmd::Ui(_)
                | Cmd::Serve(_)
                | Cmd::Acp(_)
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
            for fork in molt::tui::run(serve_options(cli.config, cli.profile, args)).await? {
                eprintln!("kept: {}", agent::printable(&fork));
            }
        }
        Cmd::Skill(args) => {
            use molt_api::skills::{Action, Request, MANAGE};
            let config = cli.config.as_deref();
            let (workspace, project) = agent::locate(args.at.project.as_deref(), &args.at.workspace, config.is_some())?;
            let cfg = agent::memory_config(config, &workspace, args.at.data_dir.as_deref(), project.as_ref())?;
            let export_to = match &args.cmd {
                SkillCmd::Export { out, .. } => Some(out.clone()),
                _ => None,
            };
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
                SkillCmd::Export { id, .. } => Action::Export { id },
            };
            let request = Request { workspace: workspace.to_str().context("workspace is not UTF-8")?.into(), action };
            let response = agent::call_memory(&cfg, MANAGE, serde_json::to_value(request)?).await?;
            if let Some(out) = export_to {
                let skill: molt_api::skills::Skill =
                    serde_json::from_value(response).context("memory's exported skill does not read")?;
                let exported = molt::agent_skills::export(&skill);
                match out {
                    Some(dir) => println!("{}", molt::agent_skills::write(&exported, &dir)?.display()),
                    None => print!("{}", exported.render()),
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
        }
        Cmd::UiBackend => molt::tui::serve_backend().await?,
        Cmd::Serve(args) => {
            let stop = molt::stop_signal()?;
            let session = args.at.session.clone();
            let (cfg, workspace, _) = molt::serve::configure(serve_options(cli.config, cli.profile, args.at))?;
            let socket = args.socket.unwrap_or_else(|| cfg.kernel.data_dir.join(molt::protocol::SOCKET));
            let server = molt::serve::Server::named(cfg, workspace, session);
            if args.stdio {
                molt::serve::stdio(server, stop).await?;
            } else {
                molt::serve::listen(server, &socket, stop).await?;
            }
        }
        Cmd::Acp(args) => {
            let stop = molt::stop_signal()?;
            let opts = molt::acp::Options {
                config: cli.config,
                profile: cli.profile,
                pass_env: args.pass_env,
                deps_cache: args.deps_cache,
                allow_web: args.allow_web,
                web_resolver: args.web_resolver,
            };
            molt::acp::stdio(opts, stop).await?;
        }
        Cmd::Run => {
            let stop = molt::stop_signal()?;
            let mut cfg = config()?;
            agent::mcp(&mut cfg)?;
            let running = molt::start(&cfg).await?;
            tracing::info!(services = cfg.services.len(), "kernel running; Ctrl-C to stop");
            let signal = stop.await;
            tracing::info!(signal, "stopping");
            running.shutdown().await;
        }
        Cmd::Do(args) => return do_task(cli.config.as_deref(), cli.profile.as_deref(), args).await,
        Cmd::Ask(args) => return ask(cli.config.as_deref(), cli.profile.as_deref(), args).await,
        Cmd::Resume(args) => return resume_task(cli.config.as_deref(), cli.profile.as_deref(), args).await,
        Cmd::Runs(args) => return runs_cmd(cli.config.as_deref(), args),
        Cmd::Undo(args) => return undo_cmd(cli.config.as_deref(), args).await,
        Cmd::Apply(args) => return apply_cmd(cli.config.as_deref(), args).await,
        Cmd::Memory { cmd } => memory_cmd(cli.config.as_deref(), cmd).await?,
        Cmd::Project { cmd } => project_cmd(cmd)?,
        Cmd::Env { cmd: EnvCmd::Doctor { workspace, sandbox_policy } } => {
            let workspace = workspace.canonicalize().with_context(|| format!("workspace {}", workspace.display()))?;
            // The profiles of the --config file if one is given, else the user config's.
            let user = match cli.config.as_deref() {
                Some(path) => Config::load(path)?.environments,
                None => match molt::layers::user_path(&|name| std::env::var(name).ok()) {
                    Some(path) if path.is_file() => Config::load(&path)?.environments,
                    _ => Default::default(),
                },
            };
            let policy = match sandbox_policy {
                Some(path) => {
                    let scratch = std::env::temp_dir().join("molt-env-doctor");
                    molt_tools::SandboxPolicy::load(&path, &molt_tools::Roots { root: workspace.clone(), scratch })?
                }
                None => molt_tools::SandboxPolicy::default(),
            };
            let (report, ok) = molt::doctor::run(&workspace, &user, policy).await?;
            print!("{report}");
            if !ok {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Bench { cmd } => return molt_bench::cli::main(cmd).await,
        Cmd::Prices { cmd } => return prices_cmd(cmd).await,
        Cmd::Models { cmd } => return models_cmd(cmd).await,
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
        Cmd::Config { cmd: ConfigCmd::Show { origin, at } } => {
            let config = cli.config.as_deref();
            let (workspace, project) = agent::locate(at.project.as_deref(), &at.workspace, config.is_some())?;
            let layered = agent::show_config(
                config,
                &workspace,
                at.data_dir.as_deref(),
                project.as_ref(),
                cli.profile.as_deref(),
            )?;
            print!("{}", layered.show(origin)?);
        }
        Cmd::Prompts { cmd } => prompts_cmd(cmd)?,
        Cmd::Ext { cmd } => return ext_cmd(cmd),
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
fn prepare(config: Option<&Path>, profile: Option<&str>, args: &RunArgs) -> anyhow::Result<(Config, String)> {
    let (workspace, project) = agent::locate(args.project.as_deref(), &args.workspace, config.is_some())?;
    let mut cfg = agent::resolve_config(config, &workspace, args.data_dir.as_deref(), project.as_ref(), profile)?;
    if let Some(path) = config {
        eprintln!("config: {}", agent::printable(&path.display().to_string()));
    }
    if let Some(shell) = cfg.service_mut("shell") {
        let standard = shell
            .exec
            .as_ref()
            .is_some_and(|exec| Path::new(&exec.command).file_name().is_some_and(|name| name == "molt-tools"));
        anyhow::ensure!(
            standard
                || (!args.no_sandbox
                    && args.sandbox_policy.is_none()
                    && args.pass_env.is_empty()
                    && args.deps_cache.is_empty()),
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
    agent::deps_cache(&mut cfg, &args.deps_cache)?;
    agent::environments(&mut cfg)?;
    agent::mcp(&mut cfg)?;
    if args.no_memory {
        cfg.services.retain(|s| s.name.as_str() != agent::MEMORY);
    }
    agent::allow_web(&mut cfg, &args.allow_web)?;
    if let Some(resolver) = &args.web_resolver {
        cfg.web_resolver(resolver)?;
    }
    if std::env::var_os("RUST_LOG").is_none() {
        for svc in &mut cfg.services {
            svc.env.entry("RUST_LOG".into()).or_insert_with(|| "warn".into());
        }
    }
    let workspace = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    Ok((cfg, workspace))
}

async fn do_task(config: Option<&Path>, profile: Option<&str>, args: DoArgs) -> anyhow::Result<ExitCode> {
    // From the start, so that no signal leaves services running.
    let stop = molt::stop_signal()?;
    let (cfg, workspace) = prepare(config, profile, &args.common)?;
    let mut req = RunRequest::new(args.task, workspace);
    // The configured check, unless one is given or none is wanted.
    let configured = (!args.no_check && args.criterion.is_empty()).then(|| cfg.tui.check.clone()).flatten();
    if let (None, Some(check)) = (&args.check, &configured) {
        eprintln!("check: {} (from the config; --check replaces it)", agent::printable(check));
    }
    req.check = args.check.or(configured);
    req.verify = !args.no_check;
    req.rubric = args.criterion.into_iter().map(|text| Criterion { text, command: None }).collect();
    req.attempts = args.attempts.or(cfg.tui.attempts).unwrap_or(2);
    req.select = args.select;
    req.schedule = args.schedule;
    req.escalation_models = args.escalate.into_iter().map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()).collect();
    req.model = args.model.or_else(|| cfg.tui.model.clone());
    req.effort = args.effort.or(cfg.tui.effort);
    req.max_turns = args.max_turns;
    req.syntax_check = args.syntax_check;
    req.judge_model = args.judge_model;
    req.budget_usd = args.common.budget_usd.or(cfg.tui.budget);
    req.design_budget_usd = args.common.design_budget_usd;
    req.apply = !args.common.no_apply;
    req.stream = args.common.stream;
    req.checkpoint = args.checkpoint;
    req.review = !args.common.no_review;
    req.workflow = args.workflow.filter(|w| !w.trim().is_empty());
    req.session = args.common.session.clone();
    let plan = (args.plan || !args.step.is_empty()).then_some(args.step);
    execute_as(&cfg, req, plan, TraceId::random(), &args.common, stop).await
}

async fn ask(config: Option<&Path>, profile: Option<&str>, args: AskArgs) -> anyhow::Result<ExitCode> {
    let stop = molt::stop_signal()?;
    let (cfg, workspace) = prepare(config, profile, &args.common)?;
    let mut req = RunRequest::new(args.question, workspace);
    req.workflow = Some(molt_api::workflow::ASK.to_owned());
    req.attempts = 1;
    req.model = args.model.or_else(|| cfg.tui.model.clone());
    req.effort = args.effort.or(cfg.tui.effort);
    req.max_turns = args.max_turns;
    req.budget_usd = args.common.budget_usd.or(cfg.tui.budget);
    req.apply = !args.common.no_apply;
    req.stream = args.common.stream;
    req.session = args.common.session.clone();
    execute(&cfg, req, TraceId::random(), &args.common, stop).await
}

async fn resume_task(config: Option<&Path>, profile: Option<&str>, args: ResumeArgs) -> anyhow::Result<ExitCode> {
    let stop = molt::stop_signal()?;
    let (cfg, workspace) = prepare(config, profile, &args.common)?;
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
            println!("{}", agent::printable(&agent::stopped_run(saved)));
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
    req.design_budget_usd = args.common.design_budget_usd.or(req.design_budget_usd);
    req.session = args.common.session.clone().or(req.session);
    eprintln!("resuming run {run}: {}", agent::printable(req.task.lines().next().unwrap_or_default()));
    execute(&cfg, req, TraceId::from_raw(&run), &args.common, stop).await
}

/// Carry out `req` under the run id `trace` and report it.
async fn execute(
    cfg: &Config,
    req: RunRequest,
    trace: TraceId,
    args: &RunArgs,
    stop: impl std::future::Future<Output = i32>,
) -> anyhow::Result<ExitCode> {
    execute_as(cfg, req, None, trace, args, stop).await
}

/// [`execute`], or with `plan`, `req` as a plan of those steps, or of the
/// planned ones when there are none.
async fn execute_as(
    cfg: &Config,
    req: RunRequest,
    plan: Option<Vec<String>>,
    trace: TraceId,
    args: &RunArgs,
    stop: impl std::future::Future<Output = i32>,
) -> anyhow::Result<ExitCode> {
    let workspace = req.workspace.clone();
    let apply = req.apply;
    for warning in agent::web_warnings(cfg).await? {
        eprintln!("{}", agent::printable(&format!("web DNS preflight: {warning}")));
    }
    for missing in agent::dependency_warnings(cfg, Path::new(&workspace), molt_tools::host_cargo_home().ok().as_deref())
    {
        eprintln!("{}", agent::printable(&format!("dependency preflight: {missing}")));
    }
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
    let result = match plan {
        Some(steps) => {
            let req = PlanRequest { steps, ..PlanRequest::new(req) };
            agent::plan_task_as(cfg, req, trace, learn, progress, stopped).await
        }
        None => agent::run_task_as(cfg, req, trace, learn, progress, stopped).await,
    };
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
        // The model conversation is for a task that goes on from this one, in the interface; it is long.
        if let Some(out) = out.as_object_mut() {
            out.remove("conversation");
        }
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
        // A plan: its steps, and each step's run; the rest is the plan's result.
        if let Some(plan) = &done.plan {
            out["plan"] = serde_json::to_value(plan)?;
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if let Some(plan) = &done.plan {
            print!("{}", agent::plan_report(plan));
        }
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
    let cfg = agent::memory_config(config, &workspace, args.at.data_dir.as_deref(), project.as_ref())?;
    // Refuse the common TUI-lock case before touching Git. Keep the lock through
    // the revert; a later notification failure is still reported as partial.
    let lock = match molt::DataDirLock::acquire(&cfg.kernel.data_dir) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!(
                "{}",
                agent::printable(&format!(
                    "undo did not change Git: {error:#}. If the interface is open, use /undo{} there",
                    args.run.as_ref().map_or(String::new(), |run| format!(" {run}"))
                ))
            );
            return Ok(ExitCode::from(4));
        }
    };
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
    drop(lock);
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
            eprintln!("{}", agent::printable(&format!("the commit is taken back, but memory could not be told: {e:#}. Run `molt undo {run}` again; if the interface is open, use /undo {run} there")));
            Ok(ExitCode::from(4))
        }
    }
}

async fn apply_cmd(config: Option<&Path>, args: ApplyArgs) -> anyhow::Result<ExitCode> {
    let (workspace, project) = agent::locate(args.at.project.as_deref(), &args.at.workspace, config.is_some())?;
    let cfg = agent::resolve_config(config, &workspace, args.at.data_dir.as_deref(), project.as_ref(), None)?;
    let workspace = workspace.to_str().context("the workspace path is not UTF-8")?.to_owned();
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let session = molt::session::Session::start(&cfg, &workspace, events).await?;
    let result = apply_kept(&session, &args).await;
    session.shutdown(std::slice::from_ref(&args.fork)).await;
    result
}

async fn apply_kept(session: &molt::session::Session, args: &ApplyArgs) -> anyhow::Result<ExitCode> {
    let fork = &args.fork;
    let mut paths = args.only.clone();
    if args.check.is_some() || !args.only.is_empty() {
        let check = args.check.clone().map(|command| molt_api::planner::CheckSpec {
            command,
            files: args.check_file.clone(),
            designed: false,
            suite: None,
        });
        let only = (!args.only.is_empty()).then_some(&args.only[..]);
        let found = session.recheck(fork, only, check.as_ref()).await?;
        for rule in &found.rules {
            eprintln!("{}", agent::printable(&format!("the review's fixed rules: {rule}")));
        }
        let chosen = found.diff.changes.len();
        match &found.check {
            Some(ran) if !ran.success() => {
                let status = ran.exit_code.map_or_else(|| "it was stopped".to_owned(), |c| format!("exit {c}"));
                eprintln!(
                    "{}",
                    agent::printable(&format!(
                        "the check failed on the {chosen} chosen files ({status}); nothing is applied, and the \
                         result stays in {fork}"
                    ))
                );
                let output = format!("{}{}", ran.stdout, ran.stderr);
                eprint!("{}", agent::printable(&output));
                return Ok(ExitCode::FAILURE);
            }
            Some(_) => eprintln!("the check passed on the {chosen} chosen files"),
            None => eprintln!("not checked: --no-check"),
        }
        if !args.only.is_empty() {
            paths = found.diff.changes.iter().map(|c| c.path.clone()).collect();
        }
    }
    let merged = session.apply_only(fork, &paths).await?;
    let names: Vec<&str> = merged.changes.iter().map(|c| c.path.as_str()).collect();
    println!("{}", agent::printable(&format!("applied {} files: {}", names.len(), names.join(", "))));
    Ok(ExitCode::SUCCESS)
}

/// The data dir `molt do` uses where `at` says, and the workspace.
fn runs_at(config: Option<&Path>, at: &MemoryAt) -> anyhow::Result<(PathBuf, PathBuf, Option<molt::project::Project>)> {
    let (workspace, project) = agent::locate(at.project.as_deref(), &at.workspace, config.is_some())?;
    let dir = agent::data_dir_for(config, &workspace, at.data_dir.as_deref(), project.as_ref())?;
    Ok((dir, workspace, project))
}

fn runs_cmd(config: Option<&Path>, args: RunsArgs) -> anyhow::Result<ExitCode> {
    use molt::runs;
    if let Some(RunsCmd::Show { run, json, at }) = args.cmd {
        return show_run(config, &run, json, &at);
    }
    let (dir, workspace, _) = runs_at(config, &args.at)?;
    let mut listed = runs::list(&dir)?;
    let grouped = match args.session.as_deref() {
        None => false,
        Some("") => true,
        Some(name) => {
            let name = runs::session_name(name).map_err(anyhow::Error::msg)?;
            listed.retain(|r| r.session.as_deref() == Some(name.as_str()));
            false
        }
    };
    if let Some(n) = args.last {
        listed.drain(..listed.len().saturating_sub(n));
    }
    if listed.is_empty() && !args.json {
        let what = match args.session.as_deref() {
            Some(name) if !name.is_empty() => format!("no runs of session {name}"),
            _ => "no runs".to_owned(),
        };
        eprintln!("{}", agent::printable(&format!("{what} in {} (data dir {})", workspace.display(), dir.display())));
        return Ok(ExitCode::SUCCESS);
    }
    if grouped {
        let sessions = runs::sessions(listed);
        if args.json {
            println!("{}", serde_json::to_string_pretty(&sessions)?);
            return Ok(ExitCode::SUCCESS);
        }
        for (i, session) in sessions.iter().enumerate() {
            if i > 0 {
                println!();
            }
            let name = session.session.as_deref().map_or_else(|| "no session".to_owned(), |s| format!("session {s}"));
            let count = match session.runs.len() {
                1 => "1 run".to_owned(),
                n => format!("{n} runs"),
            };
            let unknown = match session.unknown_cost {
                0 => String::new(),
                n => format!(" known, {n} runs' cost not known"),
            };
            let heading = format!(
                "{name} · {count} · ${:.4}{unknown} · from {} UTC",
                session.cost_usd,
                runs::when(session.started_ms)
            );
            println!("{}", agent::printable(&heading));
            for line in runs::table(&session.runs).lines() {
                println!("  {}", agent::printable(line));
            }
        }
    } else if args.json {
        println!("{}", serde_json::to_string_pretty(&listed)?);
    } else {
        print!("{}", agent::printable(&runs::table(&listed)));
        if listed.iter().any(|r| r.resumable) {
            eprintln!("*: stopped with a checkpoint; `molt resume RUN` goes on with it");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn show_run(config: Option<&Path>, id: &str, json: bool, at: &MemoryAt) -> anyhow::Result<ExitCode> {
    use molt::runs;
    let (dir, workspace, project) = runs_at(config, at)?;
    let detail = runs::detail(&dir, id)?;
    let run = &detail.run;
    // The notes memory gave the run, as `molt memory used` has them; none when memory has nothing here.
    let recalled = agent::memory_config(config, &workspace, at.data_dir.as_deref(), project.as_ref())
        .ok()
        .and_then(|cfg| open_memory(&cfg, &workspace, project.as_ref()).ok())
        .and_then(|db| {
            let key = project.as_ref().map_or_else(|| workspace.to_string_lossy().into_owned(), |p| p.id.clone());
            molt_memory::recall_snapshots(&db, &key, &run.run).ok()
        })
        .unwrap_or_default();
    if json {
        let mut response = serde_json::to_value(&detail.response)?;
        if let Some(out) = response.as_object_mut() {
            out.remove("conversation");
        }
        let out = serde_json::json!({
            "run": run,
            "request": detail.request,
            "response": response,
            "plan": detail.plan,
            "recalled": recalled,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }
    let mut lines = vec![format!("run {}", run.run)];
    let task = detail.request.as_ref().map_or(run.task.as_str(), |r| r.task.as_str());
    lines.push("task:".to_owned());
    lines.extend(task.lines().map(|l| format!("  {l}")));
    let mut when = format!("started {} UTC, took {}", runs::when(run.started_ms), runs::duration(run.duration_ms));
    if let Some(session) = &run.session {
        when.push_str(&format!(", session {session}"));
    }
    if let Some(workflow) = &run.workflow {
        when.push_str(&format!(", workflow {workflow}"));
    }
    lines.push(when);
    let mut result = format!("result: {}", run.result.name());
    if let Some(level) = run.verification {
        result.push_str(&format!(" (verification: {level})"));
    }
    result.push_str(&format!(", cost {}", runs::cost(run.cost_usd, run.uncounted_calls)));
    lines.push(result);
    if run.resumable {
        lines.push(format!("stopped with a checkpoint: `molt resume {}` goes on with it", run.run));
    }
    let check = detail.response.as_ref().and_then(|r| r.check.as_ref()).map(|c| (c.command.clone(), c.designed));
    let check = check.or_else(|| detail.request.as_ref().and_then(|r| r.check.clone()).map(|c| (c, false)));
    match check {
        Some((command, designed)) => {
            lines.push(format!("check: {command}{}", if designed { " (designed by the planner)" } else { "" }))
        }
        None => {
            let rubric = detail.response.as_ref().map(|r| &r.rubric).filter(|r| !r.is_empty());
            let rubric = rubric.or(detail.request.as_ref().map(|r| &r.rubric)).filter(|r| !r.is_empty());
            match rubric {
                Some(rubric) => {
                    lines.push("criteria:".to_owned());
                    lines.extend(rubric.iter().map(|c| format!("  - {}", c.text)));
                }
                None if detail.request.as_ref().is_some_and(|r| !r.verify) => lines.push("check: none".to_owned()),
                None => {}
            }
        }
    }
    if let Some(resp) = &detail.response {
        if !resp.attempts.is_empty() {
            lines.push("attempts:".to_owned());
        }
        for a in &resp.attempts {
            let winner = if resp.winner == Some(a.index) { ", its result was taken" } else { "" };
            let mut line = format!(
                "  {}: {} after {} turns and {} check runs, ${:.4}{winner}",
                a.index + 1,
                agent::status_name(a.status),
                a.turns,
                a.check_runs,
                a.cost_usd
            );
            if let Some(note) = a.note.lines().find(|l| !l.trim().is_empty()) {
                line.push_str(&format!("\n     {}", note.trim()));
            }
            lines.push(line);
        }
        if resp.changes.is_empty() {
            lines.push("changes: none".to_owned());
        } else {
            lines.push("changes:".to_owned());
            lines.extend(resp.changes.iter().map(|c| format!("  {:<9}{}", agent::change_name(c.kind), c.path)));
        }
    }
    match (run.applied, &run.fork) {
        (runs::Applied::Workspace, _) => lines.push(format!("applied to {}", run.workspace)),
        (runs::Applied::Kept, Some(fork)) => lines.push(format!("not applied; the result was kept in {fork}")),
        _ => {}
    }
    if !run.summary.trim().is_empty() {
        let summary = detail.response.as_ref().map_or(run.summary.as_str(), |r| r.summary.as_str());
        lines.push("summary:".to_owned());
        lines.extend(summary.trim().lines().map(|l| format!("  {l}")));
    }
    let given: usize = recalled.iter().map(|s| s.notes.len()).sum();
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    lines.push(format!(
        "memory gave it: {} in {}",
        count(given, "note", "notes"),
        count(recalled.len(), "context", "contexts")
    ));
    let mut out = agent::printable(&(lines.join("\n") + "\n"));
    for snapshot in &recalled {
        if !snapshot.notes.is_empty() {
            out.push_str(&agent::printable(&format!("  for {}: {}\n", snapshot.purpose, snapshot.query)));
            for line in show_notes(&snapshot.notes).lines() {
                out.push_str(&format!("    {}\n", agent::printable(line)));
            }
        }
    }
    let learned = match &run.learning {
        None => "memory learned: not asked to learn from it".to_owned(),
        Some(runs::Learning::Unanswered) => "memory learned: it was asked, and did not answer".to_owned(),
        Some(runs::Learning::Failed { error }) => format!("memory learned: nothing; {error}"),
        Some(runs::Learning::Answered { notes, reinforced, contradicted, skipped, cost_usd, uncounted_calls }) => {
            let mut line = format!(
                "memory learned: {} new, {reinforced} confirmed, {contradicted} disputed, cost {}",
                notes.len(),
                runs::cost(Some(*cost_usd), *uncounted_calls)
            );
            if let Some(why) = skipped.as_deref().filter(|_| notes.is_empty()) {
                line.push_str(&format!(" ({why})"));
            }
            for note in notes {
                line.push_str(&format!("\n  + [{}] {} ({})", note.kind.as_str(), note.text, note.id));
            }
            line
        }
    };
    out.push_str(&agent::printable(&(learned + "\n")));
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

/// Memory's database for `workspace`, its project's notes adopted.
fn open_memory(
    cfg: &Config,
    workspace: &Path,
    project: Option<&molt::project::Project>,
) -> anyhow::Result<molt_memory::Db> {
    let db = agent::memory_db(cfg).context("the memory service has no --db")?;
    anyhow::ensure!(db.is_file(), "memory has nothing yet ({} does not exist)", db.display());
    let db = molt_memory::Db::open(&db)?;
    if let Some(project) = project {
        let paths = if project.paths.is_empty() { vec![workspace.to_path_buf()] } else { project.paths.clone() };
        molt_memory::adopt(&db, &molt_memory::Project { id: project.id.clone(), paths })?;
    }
    Ok(db)
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
    let open = || open_memory(&cfg, &workspace, project.as_ref());
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

async fn models_cmd(cmd: ModelsCmd) -> anyhow::Result<ExitCode> {
    use std::io::{BufRead, IsTerminal, Write};

    use molt_gateway::catalog::{finding, Catalog};
    use molt_gateway::probe::{self, Records};

    // The gateway's own variables, as the default setup starts it here: what the user config sets for it
    // (its `[[service]] model` env, `[models.endpoints]`), else this environment.
    let workspace = std::env::current_dir()?;
    let set = molt::agent::gateway_env(&workspace)?;
    let var = |name: &str| set.get(name).cloned().or_else(|| std::env::var(name).ok());
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let ago = |ms: u64| {
        let days = now_ms.saturating_sub(u128::from(ms)) / 86_400_000;
        if days == 0 {
            "today".to_owned()
        } else {
            format!("{days} days ago")
        }
    };
    let found = |p: &probe::Probed| {
        format!(
            "tools {}, streaming {}, output_schema {}, effort {}, probed {}",
            finding(p.tools),
            finding(p.streaming),
            finding(p.output_schema),
            finding(p.effort),
            ago(p.probed_at_ms)
        )
    };
    match cmd {
        ModelsCmd::Show { model: None } => {
            let catalog = Catalog::from_vars(var)?;
            let default = catalog.entry(&catalog.default_model).map(|e| format!(" ({}/{})", e.provider, e.model));
            println!("default model: {}{}", catalog.default_model, default.unwrap_or_default());
            let aliases: Vec<String> = catalog.aliases.iter().map(|(a, t)| format!("{a} = {t}")).collect();
            println!("aliases: {}", aliases.join(", "));
            if !catalog.declared.is_empty() {
                println!("declared OpenAI models: {}", catalog.declared.join(", "));
            }
            if !catalog.endpoints.is_empty() {
                println!("endpoints:");
                for line in &catalog.endpoints {
                    println!("  {line}");
                }
            }
            match probe::records_path(var) {
                Some(path) if !catalog.probed.models.is_empty() => {
                    println!("probed, in {}:", path.display());
                    for p in &catalog.probed.models {
                        println!("  {}/{} at {}: {}", p.provider, p.model, p.base_url, found(p));
                    }
                }
                _ => println!("probed: none yet (`molt models probe MODEL` finds out what a model does)"),
            }
        }
        ModelsCmd::Show { model: Some(model) } => {
            let catalog = Catalog::from_vars(var)?;
            let entry = catalog.entry(&model).map_err(|e| anyhow::anyhow!("{model}: {}", e.message))?;
            match &entry.endpoint {
                Some(name) => println!("{model}: {} on endpoint {name} at {}", entry.model, entry.base_url),
                None => println!("{model}: {}/{} at {}", entry.provider, entry.model, entry.base_url),
            }
            println!("  {}", entry.known);
            match &entry.probed {
                Some(p) => println!("  probed: {}", found(p)),
                None => println!("  probed: not on this endpoint (`molt models probe {model}` finds out)"),
            }
            match &entry.price {
                Some((p, source)) => println!(
                    "  price per million tokens: ${} input, ${} output (entry {} in {})",
                    (p.input * 1e9).round() / 1e9,
                    (p.output * 1e9).round() / 1e9,
                    source.entry,
                    source.source
                ),
                None => println!("  price: unknown, so a run stops after its first call (see `molt prices show`)"),
            }
        }
        ModelsCmd::Probe { model, yes } => {
            let gateway = molt_gateway::Gateway::new(probe::probing(molt_gateway::Config::from_vars(var)?))?;
            let plan = gateway.probe_plan(&model).map_err(|e| anyhow::anyhow!("{model}: {}", e.message))?;
            let path =
                probe::records_path(var).context("neither XDG_DATA_HOME nor HOME is set; set MOLT_MODELS_FILE")?;
            eprintln!(
                "Probing {} at {} makes up to {} calls, one at a time, with no retries and at most {} output \
                 tokens each. They are billed as usual:",
                plan.qualified(),
                plan.base_url,
                plan.checks.len(),
                probe::MAX_TOKENS
            );
            for check in &plan.checks {
                eprintln!("  - {}", check.describe());
            }
            for skipped in &plan.skipped {
                eprintln!("  not probed: {skipped}");
            }
            if plan.checks.is_empty() {
                eprintln!("There is nothing to probe.");
                return Ok(ExitCode::SUCCESS);
            }
            if !yes {
                // Spending money is a person's word: a script says so with --yes.
                anyhow::ensure!(
                    std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
                    "molt models probe asks before it calls the model; pass --yes to make the calls without asking"
                );
                eprint!("Make these calls? [y/N] ");
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    eprintln!("Nothing was called.");
                    return Ok(ExitCode::FAILURE);
                }
            }
            let report = gateway.probe(&plan, u64::try_from(now_ms).unwrap_or(u64::MAX)).await;
            for f in &report.findings {
                println!("{}: {} ({})", f.check.describe(), finding(f.works), f.detail);
            }
            let cost = match report.probed.cost_usd {
                Some(usd) => format!("${usd:.4}"),
                None => "unknown (some calls have no known price or may have been billed)".into(),
            };
            println!("{} calls, cost {cost}", report.calls);
            if let Some(why) = &report.stopped {
                eprintln!("The probe stopped: {why}. Nothing was recorded; fix that and probe again.");
                return Ok(ExitCode::FAILURE);
            }
            let mut records = Records::from_vars(var)?;
            records.put(report.probed);
            records.write(&path)?;
            println!(
                "Recorded in {}. Runs started from now on go by it; one already running keeps what it started with.",
                path.display()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn prompts_cmd(cmd: PromptsCmd) -> anyhow::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let dir = molt::prompts::dir(&|name| std::env::var(name).ok());
    match cmd {
        PromptsCmd::List => print!("{}", molt::prompts::list(dir.as_deref())?),
        PromptsCmd::Show { name, builtin } => print!("{}", molt::prompts::show(dir.as_deref(), &name, builtin)?),
        PromptsCmd::Promote { name } => {
            let dir = dir.context("there is no prompt pack: HOME is not set, or MOLT_PROMPTS_DIR is none")?;
            // A promotion is a person's word: a script or an agent has no terminal to type it at.
            anyhow::ensure!(
                std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
                "molt prompts promote asks a person to confirm at a terminal; run it there, not from a script"
            );
            let mut confirm = |question: &str| -> anyhow::Result<String> {
                eprint!("{question}");
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                Ok(line)
            };
            molt::prompts::promote(&dir, &name, &mut std::io::stderr(), &mut confirm)?;
        }
    }
    Ok(())
}

fn ext_cmd(cmd: ExtCmd) -> anyhow::Result<ExitCode> {
    use molt::extensions::{self, Added, Approve};
    use std::io::{BufRead, IsTerminal, Write};

    /// Asks at a terminal, takes --yes, and otherwise refuses.
    struct Ask {
        yes: bool,
    }
    impl Approve for Ask {
        fn approve(&mut self, text: &str) -> anyhow::Result<bool> {
            eprint!("{text}");
            if self.yes {
                eprintln!("Approved with --yes.");
                return Ok(true);
            }
            // An approval is a person's word: without a terminal there is no one to give it.
            if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
                eprintln!("Nothing was installed: approve it at a terminal, or pass --yes.");
                return Ok(false);
            }
            eprint!("Install it? [y/N] ");
            std::io::stderr().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let yes = matches!(line.trim(), "y" | "Y" | "yes");
            if !yes {
                eprintln!("Nothing was installed.");
            }
            Ok(yes)
        }
        fn say(&mut self, text: &str) {
            eprintln!("{text}");
        }
    }

    let places = extensions::Places::from_env()?;
    match cmd {
        ExtCmd::Add { path, yes } => match extensions::add(&places, &path, &mut Ask { yes })? {
            Added::Installed => println!("installed; it starts with the next run (molt ext disable NAME turns it off)"),
            Added::Updated => println!("updated"),
            Added::Unchanged => {}
            Added::Refused => return Ok(ExitCode::FAILURE),
        },
        ExtCmd::List => print!("{}", extensions::list(&places)?),
        ExtCmd::Show { name } => print!("{}", extensions::show(&places, &name)?),
        ExtCmd::Disable { name } => {
            extensions::set_enabled(&places, &name, false)?;
            println!("extension {name} is disabled; molt ext enable {name} starts it again");
        }
        ExtCmd::Enable { name } => {
            extensions::set_enabled(&places, &name, true)?;
            println!("extension {name} is enabled");
        }
        ExtCmd::Remove { name } => {
            for path in extensions::remove(&places, &name)? {
                println!("removed {}", path.display());
            }
            println!("extension {name} is uninstalled");
        }
    }
    Ok(ExitCode::SUCCESS)
}
