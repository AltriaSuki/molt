//! `molt bench`: run, validate, report and list.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{ensure, Context};
use clap::{Args, Subcommand};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::agent::{Arm, Job, Molt};
use crate::grade;
use crate::record::{self, Setup};
use crate::report;
use crate::run::{self, Event, Plan, Settings, Stop};
use crate::task::{Language, Select, Split, Task};

/// Exit status when a benchmark stopped at its spending limit.
const STOPPED_AT_BUDGET: u8 = 3;
/// Names of variables whose values are left out of results.
const SECRET_NAMES: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"];

#[derive(Subcommand)]
pub enum Cmd {
    /// Run each arm on each task, grade every run with the task's hidden
    /// tests, and append the results to a file. Started again with the
    /// same results file, it runs only what is missing.
    #[command(after_help = "Exit status: 0 when every planned run is in the results file; 1 on an error, or when \
                            runs kept failing before reaching the model; 3 when it stopped at --max-usd; 130 when \
                            interrupted. Runs stopped by an interruption are not recorded.")]
    Run(Box<RunArgs>),
    /// Check that tasks are sound: the repo's tests pass before and after
    /// the reference solution, and the hidden check fails before and passes after.
    Validate(ValidateArgs),
    /// Sum up a results file: success rate, time to done and cost per task for each arm.
    Report(ReportArgs),
    /// List the tasks.
    List(TaskArgs),
}

#[derive(Args)]
pub struct TaskArgs {
    /// The directory of tasks.
    #[arg(long, default_value = "bench/tasks")]
    pub tasks: PathBuf,
    /// Only this task (repeatable).
    #[arg(long = "task", value_name = "ID")]
    pub ids: Vec<String>,
    /// Only tasks of this split: dev or heldout.
    #[arg(long)]
    pub split: Option<Split>,
    /// Only tasks in this language: python, javascript, rust or go.
    #[arg(long)]
    pub language: Option<Language>,
}

impl TaskArgs {
    fn load(&self) -> anyhow::Result<Vec<Task>> {
        Select { ids: self.ids.clone(), split: self.split, language: self.language }.load(&self.tasks)
    }
}

#[derive(Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub tasks: TaskArgs,
    /// An arm to run: molt (Molt as it ships), plain (one unverified attempt
    /// without memory), or NAME=OPTIONS for `molt do` with OPTIONS
    /// (repeatable). Default: molt and plain.
    #[arg(long = "arm", value_name = "ARM", value_parser = Arm::parse)]
    pub arms: Vec<Arm>,
    /// Runs of each arm on each task.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub trials: u32,
    /// The results file, JSON lines; created if missing, appended to otherwise.
    #[arg(long)]
    pub results: PathBuf,
    /// Where each run's logs go. Default: the results file's path with .logs in place of its extension.
    #[arg(long)]
    pub logs: Option<PathBuf>,
    /// Spending limit for this invocation, in US dollars. A run starts only
    /// if what was spent, plus --task-usd for it and for each run in flight,
    /// stays within it.
    #[arg(long, value_parser = usd, required_unless_present = "dry_run")]
    pub max_usd: Option<f64>,
    /// Spending limit of each run, passed to `molt do --budget-usd`. Molt
    /// checks it before each model call, so a run can pass it by one call.
    #[arg(long, default_value_t = 5.0, value_parser = usd)]
    pub task_usd: f64,
    /// Seconds each run may take before it is stopped and counted as failed.
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout_s: u64,
    /// Runs at a time. More is faster, but the runs then share the machine,
    /// which slows each one down and so changes the times measured.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=32))]
    pub jobs: u32,
    /// Model for every arm: opus, sonnet, haiku or a model id. Default: molt's.
    #[arg(long)]
    pub model: Option<String>,
    /// Effort for every arm: low, medium, high, xhigh or max. Default: molt's.
    #[arg(long, value_parser = effort)]
    pub effort: Option<String>,
    /// Model turns per attempt for every arm. Default: molt's.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_turns: Option<u32>,
    /// Set this variable for every run, such as ANTHROPIC_BASE_URL=... (repeatable).
    /// Values of names that look secret are left out of the results.
    #[arg(long = "env", value_name = "NAME=VALUE", value_parser = env_pair)]
    pub env: Vec<(String, String)>,
    /// Where each run's workspace and data dir are made, and removed after.
    /// It must be outside any git repository or project. Default: the temp directory.
    #[arg(long)]
    pub scratch: Option<PathBuf>,
    /// Keep each run's audit log, which holds every model call, with its logs.
    #[arg(long)]
    pub keep_audit: bool,
    /// Run again the runs that ended in an error, such as an API outage.
    #[arg(long)]
    pub retry_errors: bool,
    /// Show what would run and the most it could cost, and run nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// The molt executable to benchmark. Default: this one.
    #[arg(long)]
    pub molt: Option<PathBuf>,
}

#[derive(Args)]
pub struct ValidateArgs {
    #[command(flatten)]
    pub tasks: TaskArgs,
    /// Tasks validated at a time.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=32))]
    pub jobs: u32,
}

#[derive(Args)]
pub struct ReportArgs {
    /// The results file.
    pub results: PathBuf,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

fn usd(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v),
        _ => Err(format!("{s:?} is not a positive amount of US dollars")),
    }
}

fn effort(s: &str) -> Result<String, String> {
    s.parse::<molt_api::model::Effort>().map(|_| s.to_owned())
}

fn env_pair(s: &str) -> Result<(String, String), String> {
    let (name, value) = s.split_once('=').ok_or_else(|| format!("{s:?} is not NAME=VALUE"))?;
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!("{name:?} is not a variable name"));
    }
    if name.starts_with("MOLT_") {
        return Err(format!("{name}: give molt's settings as options, such as --model, so results record them"));
    }
    Ok((name.to_owned(), value.to_owned()))
}

pub async fn main(cmd: Cmd) -> anyhow::Result<ExitCode> {
    match cmd {
        Cmd::Run(args) => run_cmd(*args).await,
        Cmd::Validate(args) => validate_cmd(args).await,
        Cmd::Report(args) => {
            let records = record::load(&args.results)?;
            let r = report::report(&records);
            if args.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", report::markdown(&r));
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::List(args) => {
            for t in args.load()? {
                let s = &t.spec;
                println!(
                    "{:<24} {:<8} {:<11} {:<9} {:<7} {}",
                    t.id, s.split, s.language, s.kind, s.difficulty, s.title
                );
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_cmd(args: RunArgs) -> anyhow::Result<ExitCode> {
    let tasks = args.tasks.load()?;
    let arms = if args.arms.is_empty() {
        vec![Arm::builtin("molt").expect("built in"), Arm::builtin("plain").expect("built in")]
    } else {
        args.arms.clone()
    };
    let plan = Plan { tasks, arms, trials: args.trials };
    let exe = match &args.molt {
        Some(p) => p.canonicalize().with_context(|| format!("the molt executable {}", p.display()))?,
        None => std::env::current_exe().context("finding this executable")?,
    };
    let mut options = Vec::new();
    if let Some(model) = &args.model {
        options.extend(["--model".to_owned(), model.clone()]);
    }
    if let Some(effort) = &args.effort {
        options.extend(["--effort".to_owned(), effort.clone()]);
    }
    if let Some(turns) = args.max_turns {
        options.extend(["--max-turns".to_owned(), turns.to_string()]);
    }
    let setup = Setup {
        model: args.model.clone(),
        effort: args.effort.clone(),
        max_turns: args.max_turns,
        task_usd: args.task_usd,
        timeout_s: args.timeout_s,
        env: args.env.iter().map(|(k, v)| redacted(k, v)).collect(),
    };
    let logs = args.logs.clone().unwrap_or_else(|| args.results.with_extension("logs"));
    let settings = Settings {
        run_id: run_id(),
        setup,
        max_usd: args.max_usd.unwrap_or(f64::INFINITY),
        jobs: args.jobs as usize,
        scratch: args.scratch.clone().unwrap_or_else(std::env::temp_dir),
        logs,
        molt_version: env!("CARGO_PKG_VERSION").to_owned(),
        git_commit: git_commit(&args.tasks.tasks),
        retry_errors: args.retry_errors,
    };
    let molt = Molt { exe, options, env: args.env.clone(), keep_audit: args.keep_audit };

    if args.dry_run {
        dry_run(&plan, &settings, &args.results, &molt)?;
        return Ok(ExitCode::SUCCESS);
    }
    let has_key = std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|v| !v.is_empty())
        || args.env.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && !v.is_empty());
    ensure!(has_key, "molt do needs ANTHROPIC_API_KEY; set it, or pass it with --env");

    let cancel = CancellationToken::new();
    let on_signal = cancel.clone();
    let mut signals = [SignalKind::interrupt(), SignalKind::terminate(), SignalKind::hangup()]
        .into_iter()
        .map(signal)
        .collect::<std::io::Result<Vec<_>>>()?;
    tokio::spawn(async move {
        let [int, term, hup] = &mut signals[..] else { return };
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        eprintln!("stopping: the runs in flight are being stopped, and will not be recorded");
        on_signal.cancel();
    });

    let summary = run::run(&plan, &settings, &args.results, Arc::new(molt), cancel, progress).await?;
    let records = record::load(&args.results)?;
    print!("{}", report::markdown(&report::report(&records)));
    eprintln!(
        "\n{} runs recorded in {} (logs in {}); this invocation spent ${:.2}.",
        summary.recorded,
        args.results.display(),
        settings.logs.display(),
        summary.spent_usd
    );
    Ok(match summary.stop {
        None => ExitCode::SUCCESS,
        Some(Stop::Budget { left }) => {
            eprintln!(
                "Stopped at --max-usd with {left} runs not started. The same command with a higher \
                 --max-usd carries on from here."
            );
            ExitCode::from(STOPPED_AT_BUDGET)
        }
        Some(Stop::Interrupted) => {
            eprintln!("Interrupted. The same command carries on from here.");
            ExitCode::from(130)
        }
        Some(Stop::Failing(why)) => {
            eprintln!("Stopped: runs keep failing before they reach the model. The last one: {why}");
            ExitCode::FAILURE
        }
    })
}

fn progress(event: Event) {
    match event {
        Event::Planned { total, done } => {
            eprintln!("{total} runs planned, {done} already in the results file")
        }
        Event::Started { task, arm, trial } => eprintln!("start  {task} · {arm} · trial {}", trial + 1),
        Event::Finished { record: r, n, of, spent_usd } => {
            let verdict = match (&r.error, r.timed_out, r.passed, r.claimed_done) {
                (Some(e), ..) => format!("error: {e}"),
                (None, true, ..) => "stopped at the time limit".to_owned(),
                (None, false, true, _) => "solved".to_owned(),
                (None, false, false, true) => "failed, though it said it was done".to_owned(),
                (None, false, false, false) => "failed".to_owned(),
            };
            eprintln!(
                "[{n}/{of}] {} · {} · trial {}: {verdict} in {}s for ${:.2} (spent ${spent_usd:.2})",
                r.task,
                r.arm,
                r.trial + 1,
                r.wall_ms / 1000,
                r.cost_usd
            );
        }
        Event::Dropped { task, arm, trial, cost_usd } => {
            eprintln!("dropped {task} · {arm} · trial {}: interrupted after ${cost_usd:.2}", trial + 1)
        }
    }
}

fn dry_run(plan: &Plan, settings: &Settings, results: &Path, molt: &Molt) -> anyhow::Result<()> {
    let old = if results.exists() { record::load(results)? } else { Vec::new() };
    let done = run::already_done(plan, settings, &old)?;
    let total = plan.slots().len();
    let todo = plan.slots().into_iter().filter(|s| !done.contains(&run::key(plan, *s))).count();
    println!(
        "{} ({} × {} × {}); {} already in {}.",
        count(total, "run"),
        count(plan.tasks.len(), "task"),
        count(plan.arms.len(), "arm"),
        count(plan.trials as usize, "trial"),
        total - todo,
        results.display()
    );
    println!(
        "The {todo} to run could spend up to ${:.2} at ${:.2} each (plus at most one model call each past that).",
        todo as f64 * settings.setup.task_usd,
        settings.setup.task_usd
    );
    for arm in &plan.arms {
        let job = Job {
            task: &plan.tasks[0],
            arm,
            workspace: Path::new("WORKSPACE"),
            data_dir: Path::new("DATA_DIR"),
            logs: Path::new("LOGS"),
            budget_usd: settings.setup.task_usd,
            timeout: std::time::Duration::from_secs(settings.setup.timeout_s),
            cancel: CancellationToken::new(),
        };
        let mut args = molt.args(&job);
        args.pop();
        println!("{}: {} {} PROMPT", arm.name, molt.exe.display(), args.join(" "));
    }
    Ok(())
}

async fn validate_cmd(args: ValidateArgs) -> anyhow::Result<ExitCode> {
    let tasks = args.tasks.load()?;
    let limit = Arc::new(tokio::sync::Semaphore::new(args.jobs as usize));
    let mut set = JoinSet::new();
    for (i, task) in tasks.into_iter().enumerate() {
        let limit = limit.clone();
        set.spawn(async move {
            let _permit = limit.acquire_owned().await;
            (i, grade::validate(&task).await)
        });
    }
    let mut done = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (i, v) = joined.context("a validation panicked")?;
        println!("{}: {}", v.task, if v.ok() { "valid" } else { "INVALID" });
        for f in &v.findings {
            println!("  {} {}", if f.ok { "ok  " } else { "FAIL" }, f.what.replace('\n', "\n       "));
        }
        done.push((i, v.ok()));
    }
    let bad = done.iter().filter(|(_, ok)| !ok).count();
    if bad > 0 {
        println!("\n{bad} of {} tasks are invalid", done.len());
        return Ok(ExitCode::FAILURE);
    }
    println!("\nall {} tasks are valid", done.len());
    Ok(ExitCode::SUCCESS)
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// `NAME=VALUE`, or `NAME=<redacted>` when the name looks like a secret's.
fn redacted(name: &str, value: &str) -> String {
    let upper = name.to_ascii_uppercase();
    if SECRET_NAMES.iter().any(|s| upper.contains(s)) {
        format!("{name}=<redacted>")
    } else {
        format!("{name}={value}")
    }
}

/// Seconds since the epoch and the process id: unique enough to tell
/// invocations apart in one results file.
fn run_id() -> String {
    format!("{}-{}", run::now_ms() / 1000, std::process::id())
}

/// The commit the tasks were at, with `-dirty` when they had changes.
fn git_commit(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "--short=12", "HEAD"])?;
    let dirty = git(&["status", "--porcelain", "--", "."]).is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{commit}-dirty") } else { commit })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_stay_out_of_results() {
        assert_eq!(redacted("ANTHROPIC_API_KEY", "sk-1"), "ANTHROPIC_API_KEY=<redacted>");
        assert_eq!(redacted("my_token", "t"), "my_token=<redacted>");
        assert_eq!(redacted("ANTHROPIC_BASE_URL", "http://x"), "ANTHROPIC_BASE_URL=http://x");
    }

    #[test]
    fn options_are_checked_when_parsed() {
        assert!(usd("0").is_err() && usd("-1").is_err() && usd("inf").is_err() && usd("abc").is_err());
        assert_eq!(usd("2.5"), Ok(2.5));
        assert!(effort("high").is_ok() && effort("huge").is_err());
        assert_eq!(env_pair("A_B=x=y"), Ok(("A_B".into(), "x=y".into())));
        assert!(env_pair("1A=x").is_err() && env_pair("A").is_err());
        assert!(env_pair("MOLT_MODEL=opus").unwrap_err().contains("--model"));
    }
}
