//! `molt do`: start the agent services, have the planner carry out one
//! task, and report how it went.
//!
//! The CLI joins the bus in process as [`CLI`]. It holds only `planner.run`
//! and `topic:progress`; the planner does the work with its own capabilities.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use molt_api::fs::ChangeKind;
use molt_api::planner::{self, AttemptStatus, Outcome, RunRequest, RunResponse};
use molt_api::progress::{self, Progress};
use molt_kernel::Kernel;
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, ServiceId, Target, TraceId};
use molt_sdk::{CallOpts, SdkError, Service};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Config, Running};

/// The identity `molt do` joins the bus with.
pub const CLI: &str = "cli";

/// The services a run needs.
pub const SERVICES: [&str; 4] = ["model", "fs", "shell", "planner"];

/// How long services get to come up.
const STARTUP: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_millis(50);
/// A method no service has: a live service answers it with `invalid`.
const PROBE: &str = "ping";
/// A live service answers a probe at once. On NATS a probe sent before the
/// service has subscribed is lost, and only this deadline ends it.
const PROBE_MS: u64 = 500;
/// Reply deadline for a whole run; the planner's own limits end it sooner.
const RUN_MS: u64 = 6 * 3600 * 1000;

/// The run was stopped before it finished.
#[derive(Debug, thiserror::Error)]
#[error("interrupted")]
pub struct Interrupted;

/// The configuration `molt do` runs with: the file at `path` if there is one
/// (it must define the agent [`SERVICES`]), otherwise [`Config::agent`] for
/// the canonical `workspace`, with the service executables next to the
/// running one. `data_dir` replaces the configured data dir; the default
/// setup otherwise uses [`default_data_dir`].
pub fn resolve_config(path: &Path, workspace: &Path, data_dir: Option<&Path>) -> anyhow::Result<Config> {
    if path.exists() {
        let mut cfg = Config::load(path)?;
        require_services(&cfg).with_context(|| format!("{} cannot run tasks", path.display()))?;
        if let Some(dir) = data_dir {
            cfg.kernel.data_dir = dir.to_path_buf();
        }
        return Ok(cfg);
    }
    if std::env::var_os("ANTHROPIC_API_KEY").is_none_or(|key| key.is_empty()) {
        bail!(
            "ANTHROPIC_API_KEY is not set: export your Anthropic API key, or configure the services in {}",
            path.display()
        );
    }
    let data_dir = match data_dir {
        Some(dir) => std::path::absolute(dir)?,
        None => default_data_dir(workspace)?,
    };
    // Checked before the directory is created, and again with symlinks and `..` resolved.
    outside(workspace, &data_dir)?;
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let data_dir = data_dir.canonicalize().with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &data_dir)?;
    let exe = std::env::current_exe().context("finding the molt executable")?;
    let bin_dir = exe.parent().context("the molt executable has no parent directory")?;
    Config::agent(workspace, &data_dir, bin_dir)
}

fn outside(workspace: &Path, data_dir: &Path) -> anyhow::Result<()> {
    if data_dir.starts_with(workspace) {
        bail!(
            "the data dir {} is inside the workspace; forks there would find the project's own Cargo.toml and .git",
            data_dir.display()
        );
    }
    Ok(())
}

/// `<cache>/molt/<name>-<hash>`: the cache is `$XDG_CACHE_HOME` or
/// `~/.cache`, `name` the workspace's directory name and `hash` the first 8
/// hex digits of the SHA-256 of its canonical path.
pub fn default_data_dir(workspace: &Path) -> anyhow::Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").filter(|home| !home.is_empty()).map(|home| Path::new(&home).join(".cache"))
        })
        .context("neither XDG_CACHE_HOME nor HOME is set; pass --data-dir")?;
    Ok(cache.join("molt").join(data_dir_name(workspace)))
}

fn data_dir_name(workspace: &Path) -> String {
    let hash = hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()));
    // Short and plain, so socket paths stay under the platform's length limit.
    let name: String = workspace
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .take(32)
        .collect();
    let name = if name.is_empty() { "root" } else { &name };
    format!("{name}-{}", &hash[..8])
}

/// Check that `cfg` defines every service a run needs and leaves the name
/// [`CLI`] free.
pub fn require_services(cfg: &Config) -> anyhow::Result<()> {
    let missing: Vec<&str> = SERVICES.into_iter().filter(|name| cfg.service(name).is_none()).collect();
    if !missing.is_empty() {
        bail!("missing the services {} (a run needs {})", missing.join(", "), SERVICES.join(", "));
    }
    if cfg.service(CLI).is_some() {
        bail!("the service name {CLI:?} is reserved for `molt do`");
    }
    Ok(())
}

/// Carry out one task: start the kernel and the services of `cfg`, ask the
/// planner to run `req`, hand this run's progress events to `on_progress`,
/// and shut everything down again. If `stop` resolves first, the run is
/// abandoned with [`Interrupted`].
pub async fn run_task(
    cfg: &Config,
    req: RunRequest,
    on_progress: impl FnMut(&Progress),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<RunResponse> {
    require_services(cfg)?;
    let running = crate::start(cfg).await?;
    let result = tokio::select! {
        // A Ctrl-C in a terminal also reaches the services, and the run may
        // fail from their exit at the same moment: the interruption wins.
        biased;
        () = stop => Err(Interrupted.into()),
        result = drive(&running, req, on_progress) => result,
    };
    running.shutdown().await;
    result
}

async fn drive(
    running: &Running,
    req: RunRequest,
    mut on_progress: impl FnMut(&Progress),
) -> anyhow::Result<RunResponse> {
    let kernel = running.kernel();
    let cli = join_bus(running).await?;
    let deadline = Instant::now() + STARTUP;
    for service in SERVICES {
        wait_until_up(kernel, &cli, service, deadline).await?;
    }
    grant(kernel, &cli, planner::RUN, Budget::new(0, 0, 1_000)).await?;
    grant(kernel, &cli, &format!("topic:{}", progress::TOPIC), Budget::new(0, 0, 1_000)).await?;
    cli.subscribe(progress::TOPIC).await?;

    let trace = TraceId::random();
    let call = call_planner(&cli, &req, &trace);
    tokio::pin!(call);
    let reply = loop {
        tokio::select! {
            // Events go first. The link hands over the events published
            // before the reply ahead of it, so none is left unshown.
            biased;
            Some(msg) = cli.next() => {
                if let Some(event) = progress_of(&msg, trace.as_str()) {
                    on_progress(&event);
                }
            }
            reply = &mut call => break reply?,
        }
    };
    serde_json::from_value(reply).context("the planner's result is not a planner.run response")
}

/// Register [`CLI`] with the kernel and connect to it like a service would.
async fn join_bus(running: &Running) -> anyhow::Result<Service> {
    let kernel = running.kernel();
    let id = ServiceId::new(CLI)?;
    let secret = match running.secrets() {
        Some(secrets) => {
            let secret = secrets.services.get(CLI).cloned().with_context(|| {
                format!("no NATS secret for {CLI}; rerun `molt nats-config` and restart nats-server with its output")
            })?;
            kernel.register_with_secret(&id, secret.clone()).await?;
            secret
        }
        None => kernel.register(&id).await?,
    };
    let link = molt_transport::connect(&kernel.address(), &id, &secret).await.context("joining the bus")?;
    Ok(Service::new(id, link, HashMap::new()))
}

async fn grant(kernel: &Kernel, cli: &Service, target: &str, budget: Budget) -> anyhow::Result<CapId> {
    let cap = kernel.grant(cli.id(), target.parse()?, budget, None).await?;
    cli.add_cap(target, cap.clone());
    Ok(cap)
}

/// Wait until `service` answers on the bus. Until its process has connected,
/// the kernel answers `unavailable` (Unix sockets) or the probe times out
/// (NATS drops what nobody reads), and a planner call made then would fail
/// or wait out its whole deadline.
async fn wait_until_up(kernel: &Kernel, cli: &Service, service: &str, deadline: Instant) -> anyhow::Result<()> {
    let target = format!("{service}.{PROBE}");
    let cap = grant(kernel, cli, &target, Budget::new(0, 0, 100_000)).await?;
    let result = loop {
        let opts = CallOpts { cap: Some(cap.clone()), budget: Budget::new(0, PROBE_MS, 0), trace: None };
        match cli.call(&target, Value::Null, opts).await {
            Ok(_) => break Ok(()),
            Err(SdkError::Remote(e)) if matches!(e.code, ErrorCode::Invalid | ErrorCode::Failed) => break Ok(()),
            Err(SdkError::Remote(e))
                if matches!(e.code, ErrorCode::Unavailable | ErrorCode::Timeout | ErrorCode::Busy) =>
            {
                if Instant::now() >= deadline {
                    break Err(anyhow!("the {service} service did not come up within {STARTUP:?}: {e}"));
                }
                tokio::time::sleep(RETRY).await;
            }
            Err(e) => break Err(anyhow::Error::new(e).context(format!("probing the {service} service"))),
        }
    };
    kernel.revoke(&cap).await?;
    result
}

/// `planner.run`, retried while the planner is still starting.
async fn call_planner(cli: &Service, req: &RunRequest, trace: &TraceId) -> anyhow::Result<Value> {
    let payload = serde_json::to_value(req)?;
    let deadline = Instant::now() + STARTUP;
    loop {
        let opts = CallOpts { cap: None, budget: Budget::new(0, RUN_MS, 0), trace: Some(trace.clone()) };
        match cli.call(planner::RUN, payload.clone(), opts).await {
            Err(SdkError::Remote(e)) if e.code == ErrorCode::Unavailable && Instant::now() < deadline => {
                tracing::debug!(error = %e, "the planner is not ready; retrying");
                tokio::time::sleep(RETRY).await;
            }
            result => return result.context("planner.run failed"),
        }
    }
}

/// The progress event `msg` carries, if it is one of run `run`'s.
fn progress_of(msg: &Envelope, run: &str) -> Option<Progress> {
    let on_topic = matches!(&msg.to, Target::Topic { name } if name == progress::TOPIC);
    if msg.kind != Kind::Event || !on_topic {
        return None;
    }
    let event: Progress = serde_json::from_value(msg.payload.clone()).ok()?;
    (event.run() == run).then_some(event)
}

/// One short line for the terminal.
pub fn describe(event: &Progress) -> String {
    match event {
        Progress::CheckReady { command: Some(command), files, designed, .. } => {
            let mut line = format!("check: {command}");
            if *designed {
                line.push_str(" (designed by the planner");
                if !files.is_empty() {
                    line.push_str(&format!("; uses {}", files.join(", ")));
                }
                line.push(')');
            }
            line
        }
        Progress::CheckReady { command: None, .. } => {
            "no automated check fits this task; the result will be unverified".to_owned()
        }
        Progress::AttemptStarted { attempt, .. } => format!("attempt {attempt}: started"),
        Progress::ToolCall { attempt, detail, .. } => format!("attempt {attempt}: {detail}"),
        Progress::CheckRan { attempt, passed: true, .. } => format!("attempt {attempt}: check passed"),
        Progress::CheckRan { attempt, passed: false, exit_code: Some(code), .. } => {
            format!("attempt {attempt}: check failed (exit code {code})")
        }
        Progress::CheckRan { attempt, passed: false, exit_code: None, .. } => {
            format!("attempt {attempt}: check failed (killed or timed out)")
        }
        Progress::AttemptFinished { attempt, status, .. } => format!("attempt {attempt}: {}", status_name(*status)),
        Progress::Note { message, .. } => message.clone(),
    }
}

/// The result of a run as a few lines for the terminal.
pub fn report(resp: &RunResponse) -> String {
    let mut lines = vec![format!("outcome: {}", outcome_name(resp.outcome))];
    if let Some(check) = &resp.check {
        let by = if check.designed { " (designed by the planner)" } else { "" };
        lines.push(format!("check: {}{by}", check.command));
    }
    if resp.changes.is_empty() {
        lines.push("changes: none".to_owned());
    } else {
        lines.push("changes:".to_owned());
        lines.extend(resp.changes.iter().map(|c| format!("  {:<9}{}", change_name(c.kind), c.path)));
    }
    if resp.applied {
        lines.push("applied to the workspace".to_owned());
    } else if let Some(fork) = &resp.fork {
        lines.push(format!("not applied; the result is in {fork}"));
    }
    lines.push(format!("cost: ${:.4} ({} tokens)", resp.cost_usd, resp.usage.total()));
    let summary = resp.summary.trim();
    if !summary.is_empty() {
        lines.push(String::new());
        lines.push(summary.to_owned());
    }
    lines.join("\n") + "\n"
}

pub fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Passed => "passed",
        Outcome::Failed => "failed",
        Outcome::Unverified => "unverified",
    }
}

fn status_name(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Passed => "passed",
        AttemptStatus::Failed => "failed",
        AttemptStatus::Cancelled => "cancelled",
        AttemptStatus::Error => "stopped on an error",
    }
}

fn change_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

#[cfg(test)]
mod tests {
    use molt_api::fs::Change;
    use molt_api::model::Usage;
    use molt_api::planner::CheckSpec;
    use serde_json::json;

    use super::*;

    #[test]
    fn data_dir_names_are_short_plain_and_tied_to_the_path() {
        let a = data_dir_name(Path::new("/home/u/my project"));
        assert!(a.starts_with("my_project-"), "{a}");
        assert_eq!(a.len(), "my_project-".len() + 8);
        assert_eq!(a, data_dir_name(Path::new("/home/u/my project")));
        assert_ne!(a, data_dir_name(Path::new("/srv/my project")));
        assert!(data_dir_name(Path::new("/")).starts_with("root-"));
        let long = data_dir_name(&Path::new("/w").join("x".repeat(200)));
        assert_eq!(long.len(), 32 + 1 + 8);
    }

    #[test]
    fn a_config_without_the_agent_services_is_refused() {
        let err = require_services(&Config::default()).unwrap_err().to_string();
        assert!(err.contains("missing the services model, fs, shell, planner"), "{err}");
        let mut cfg = Config::agent(Path::new("/w"), Path::new("/d"), Path::new("/b")).unwrap();
        require_services(&cfg).unwrap();
        cfg.services.retain(|s| s.name.as_str() != "shell");
        let err = require_services(&cfg).unwrap_err().to_string();
        assert!(err.contains("missing the services shell "), "{err}");
    }

    #[test]
    fn only_this_runs_progress_is_shown() {
        let event = |run: &str| {
            let payload = json!({ "event": "attempt_started", "run": run, "attempt": 1 });
            Envelope::event(TraceId::random(), progress::TOPIC, CapId::random(), payload).unwrap()
        };
        assert_eq!(
            progress_of(&event("trace_a"), "trace_a"),
            Some(Progress::AttemptStarted { run: "trace_a".into(), attempt: 1 })
        );
        assert_eq!(progress_of(&event("trace_b"), "trace_a"), None);
        let mut other = event("trace_a");
        other.to = "topic:builds".parse().unwrap();
        assert_eq!(progress_of(&other, "trace_a"), None);
    }

    #[test]
    fn progress_lines() {
        let run = String::new();
        let lines = [
            Progress::CheckReady {
                run: run.clone(),
                command: Some("sh check.sh".into()),
                files: vec!["check.sh".into()],
                designed: true,
            },
            Progress::CheckReady { run: run.clone(), command: None, files: vec![], designed: true },
            Progress::ToolCall {
                run: run.clone(),
                attempt: 1,
                turn: 3,
                tool: "read_file".into(),
                detail: "read a.rs".into(),
            },
            Progress::CheckRan { run: run.clone(), attempt: 0, passed: false, exit_code: Some(1) },
            Progress::AttemptFinished { run, attempt: 1, status: AttemptStatus::Cancelled },
        ]
        .iter()
        .map(describe)
        .collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "check: sh check.sh (designed by the planner; uses check.sh)",
                "no automated check fits this task; the result will be unverified",
                "attempt 1: read a.rs",
                "attempt 0: check failed (exit code 1)",
                "attempt 1: cancelled",
            ]
        );
    }

    #[test]
    fn report_lines() {
        let mut resp = RunResponse {
            outcome: Outcome::Passed,
            check: Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: false }),
            winner: Some(0),
            summary: "Fixed the parser.\n".into(),
            changes: vec![Change { path: "src/lib.rs".into(), kind: ChangeKind::Modified }],
            patch: String::new(),
            applied: true,
            fork: None,
            attempts: vec![],
            usage: Usage { input_tokens: 1000, output_tokens: 200, ..Usage::default() },
            cost_usd: 0.012,
        };
        assert_eq!(
            report(&resp),
            "outcome: passed\ncheck: cargo test\nchanges:\n  modified src/lib.rs\napplied to the workspace\n\
             cost: $0.0120 (1200 tokens)\n\nFixed the parser.\n"
        );
        resp.applied = false;
        resp.fork = Some("/cache/work/fork_1".into());
        assert!(report(&resp).contains("not applied; the result is in /cache/work/fork_1\n"));
        resp.changes.clear();
        resp.summary.clear();
        assert!(report(&resp).ends_with(
            "changes: none\nnot applied; the result is in /cache/work/fork_1\ncost: $0.0120 (1200 tokens)\n"
        ));
    }
}
