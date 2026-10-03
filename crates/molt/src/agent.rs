//! `molt do`: start the agent services, have the planner carry out one
//! task, and report how it went.
//!
//! The CLI joins the bus in process as [`CLI`]. It holds only `planner.run`
//! and `topic:progress`; the planner does the work with its own capabilities.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
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

/// The configuration `molt do` runs with: the file at `path` when one is
/// given (it must define the agent [`SERVICES`]), otherwise [`Config::agent`]
/// for the canonical `workspace`, with the service executables next to the
/// running one. No file is ever picked up on its own: one in the project
/// could run any command with the user's secrets. `data_dir` replaces the
/// configured data dir; the default setup otherwise uses [`default_data_dir`].
pub fn resolve_config(path: Option<&Path>, workspace: &Path, data_dir: Option<&Path>) -> anyhow::Result<Config> {
    if let Some(path) = path {
        let mut cfg = Config::load(path)?;
        require_services(&cfg).with_context(|| format!("{} cannot run tasks", path.display()))?;
        if let Some(dir) = data_dir {
            cfg.kernel.data_dir = dir.to_path_buf();
        }
        return Ok(cfg);
    }
    if std::env::var_os("ANTHROPIC_API_KEY").is_none_or(|key| key.is_empty()) {
        bail!(
            "ANTHROPIC_API_KEY is not set: export your Anthropic API key, or pass --config with services of your own"
        );
    }
    let data_dir = match data_dir {
        Some(dir) => dir.to_path_buf(),
        None => default_data_dir(workspace)?,
    };
    // Checked before the directory is created, and again once it exists.
    let data_dir = resolved(&data_dir).with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &data_dir)?;
    crate::lock::create_private_dir(&data_dir)?;
    let data_dir = data_dir.canonicalize().with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &data_dir)?;
    let exe = std::env::current_exe().context("finding the molt executable")?;
    let bin_dir = exe.parent().context("the molt executable has no parent directory")?;
    Config::agent(workspace, &data_dir, bin_dir)
}

/// `path` made absolute, with symlinks and `..` resolved as far as it exists.
/// In the rest, which creating it would make plain directories, `..` is
/// resolved by name.
fn resolved(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    for known in path.ancestors() {
        let Ok(mut real) = known.canonicalize() else { continue };
        for part in path.components().skip(known.components().count()) {
            match part {
                Component::ParentDir => {
                    real.pop();
                }
                Component::Normal(name) => real.push(name),
                Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            }
        }
        return Ok(real);
    }
    Ok(path)
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
/// `~/.cache`, `name` the workspace's directory name (shortened or left out
/// when the cache path is long) and `hash` the first 8 hex digits of the
/// SHA-256 of its canonical path.
pub fn default_data_dir(workspace: &Path) -> anyhow::Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").filter(|home| !home.is_empty()).map(|home| Path::new(&home).join(".cache"))
        })
        .context("neither XDG_CACHE_HOME nor HOME is set; pass --data-dir")?;
    Ok(data_dir_in(&cache.join("molt"), workspace))
}

/// `<parent>/<name>-<hash>`, the name shortened or left out so that the
/// sockets of a run fit [`MAX_SOCKET_PATH`](crate::MAX_SOCKET_PATH).
fn data_dir_in(parent: &Path, workspace: &Path) -> PathBuf {
    let longest = SERVICES.iter().chain([&CLI]).map(|s| s.len()).max().unwrap_or_default();
    let sockets = "/sock/".len() + longest + ".sock".len();
    let room = crate::MAX_SOCKET_PATH.saturating_sub(parent.as_os_str().len() + "/-".len() + 8 + sockets);
    parent.join(data_dir_name(workspace, room.min(32)))
}

fn data_dir_name(workspace: &Path, max_name: usize) -> String {
    let hash = hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()));
    let name: String = workspace
        .file_name()
        .map_or("root".into(), |n| n.to_string_lossy())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .take(max_name)
        .collect();
    if name.is_empty() {
        hash[..8].to_owned()
    } else {
        format!("{name}-{}", &hash[..8])
    }
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
/// abandoned with [`Interrupted`]. The forks the run leaves behind, other
/// than the one its result names, are then removed (see [`RunForks`]).
pub async fn run_task(
    cfg: &Config,
    req: RunRequest,
    on_progress: impl FnMut(&Progress),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<RunResponse> {
    require_services(cfg)?;
    let running = crate::start(cfg).await?;
    // Before planner.run, the only caller that makes forks.
    let forks = RunForks::before(cfg);
    let result = tokio::select! {
        // A Ctrl-C in a terminal also reaches the services, and the run may
        // fail from their exit at the same moment: the interruption wins.
        biased;
        () = stop => Err(Interrupted.into()),
        result = drive(&running, req, on_progress) => result,
    };
    running.shutdown().await;
    if let Some(forks) = forks {
        forks.remove_new(result.as_ref().ok().and_then(|resp| resp.fork.as_deref()));
    }
    result
}

/// The forks in the `fs` service's scratch dir when a run starts. A planner
/// stopped mid-run (interrupted, crashed, killed) never drops its forks,
/// each a full copy of the project. Only a scratch dir inside the data dir is
/// looked after: the data dir is locked to this run, so whatever appears
/// there during it is the run's own.
struct RunForks {
    scratch: PathBuf,
    before: HashSet<OsString>,
}

impl RunForks {
    fn before(cfg: &Config) -> Option<Self> {
        let args = &cfg.service("fs")?.exec.as_ref()?.args;
        let scratch = std::path::absolute(args.iter().skip_while(|a| *a != "--scratch").nth(1)?).ok()?;
        let data_dir = std::path::absolute(&cfg.kernel.data_dir).ok()?;
        let plain = !scratch.components().any(|c| c == Component::ParentDir);
        (plain && scratch.starts_with(data_dir)).then(|| Self { before: fork_entries(&scratch), scratch })
    }

    /// Remove the forks made since [`RunForks::before`], except `keep`.
    fn remove_new(self, keep: Option<&str>) {
        let keep = keep.and_then(|fork| Path::new(fork).file_name()).and_then(|name| name.to_str());
        for name in fork_entries(&self.scratch).difference(&self.before) {
            let fork = name.to_str().map(|n| n.strip_suffix(".json").unwrap_or(n));
            if keep.is_some_and(|keep| fork == Some(keep)) {
                continue;
            }
            let path = self.scratch.join(name);
            let removed = match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
                _ => std::fs::remove_file(&path),
            };
            if let Err(e) = removed {
                tracing::warn!("could not remove {}, left by the run: {e}", path.display());
            }
        }
    }
}

/// The names in `scratch` that belong to forks: `fork-<id>` and `fork-<id>.json`.
fn fork_entries(scratch: &Path) -> HashSet<OsString> {
    let Ok(entries) = std::fs::read_dir(scratch) else { return HashSet::new() };
    entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|n| n.as_encoded_bytes().starts_with(b"fork-"))
        .collect()
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
/// or wait out its whole deadline. Once the supervisor gives up on the
/// service, the wait ends with how its last run ended, which otherwise only
/// the audit log has.
async fn wait_until_up(kernel: &Kernel, cli: &Service, service: &str, deadline: Instant) -> anyhow::Result<()> {
    let id = ServiceId::new(service)?;
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
                let status = kernel.service_status(&id);
                let last = status.last_exit.map(|exit| format!(" (last exit: {exit})")).unwrap_or_default();
                if let Some(restarts) = status.gave_up {
                    break Err(anyhow!(
                        "the {service} service did not come up: gave up after {restarts} restarts{last}"
                    ));
                }
                if Instant::now() >= deadline {
                    break Err(anyhow!("the {service} service did not come up within {STARTUP:?}: {e}{last}"));
                }
                tokio::time::sleep(RETRY).await;
            }
            Err(e) => break Err(anyhow::Error::new(e).context(format!("probing the {service} service"))),
        }
    };
    kernel.revoke(&cap).await?;
    result
}

/// `planner.run`, sent once. [`wait_until_up`] has shown that the planner is
/// up, so any error now, `unavailable` from its crash included, may come
/// after it got the request, and sending it again would start the run over.
async fn call_planner(cli: &Service, req: &RunRequest, trace: &TraceId) -> anyhow::Result<Value> {
    let opts = CallOpts { cap: None, budget: Budget::new(0, RUN_MS, 0), trace: Some(trace.clone()) };
    cli.call(planner::RUN, serde_json::to_value(req)?, opts).await.context("planner.run failed")
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

/// One short line for the terminal, made [`printable`].
pub fn describe(event: &Progress) -> String {
    printable(&match event {
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
    })
}

/// The result of a run as a few lines for the terminal, made [`printable`].
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
    let mut cost = format!("cost: ${:.4} ({} tokens)", resp.cost_usd, resp.usage.total());
    if resp.uncounted_calls > 0 {
        cost.push_str(&format!(", not counting {} calls of cancelled attempts left unanswered", resp.uncounted_calls));
    }
    lines.push(cost);
    let summary = resp.summary.trim();
    if !summary.is_empty() {
        lines.push(String::new());
        lines.push(summary.to_owned());
    }
    printable(&(lines.join("\n") + "\n"))
}

/// `text` with each control character but newline and tab escaped (ESC as
/// `\u{1b}`). What the model writes and the file names its commands make
/// reach the terminal, and an escape sequence there could rewrite or hide
/// what the user sees, or set the clipboard.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
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
        let name = |path: &str| data_dir_name(Path::new(path), 32);
        let a = name("/home/u/my project");
        assert!(a.starts_with("my_project-"), "{a}");
        assert_eq!(a.len(), "my_project-".len() + 8);
        assert_eq!(a, name("/home/u/my project"));
        assert_ne!(a, name("/srv/my project"));
        assert!(name("/").starts_with("root-"));
        assert_eq!(name(&format!("/w/{}", "x".repeat(200))).len(), 32 + 1 + 8);
        assert_eq!(data_dir_name(Path::new("/home/u/my project"), 3), format!("my_-{}", &a[a.len() - 8..]));
        assert_eq!(data_dir_name(Path::new("/home/u/my project"), 0), a[a.len() - 8..]);
    }

    #[test]
    fn the_default_data_dir_leaves_room_for_the_sockets() {
        let socket = |data_dir: &Path| data_dir.join("sock").join("planner.sock").as_os_str().len();
        let workspace = Path::new("/home/u/my-company-monorepo-service-api");
        let short = data_dir_in(Path::new("/home/u/.cache/molt"), workspace);
        assert!(short.ends_with(data_dir_name(workspace, 32)), "{}", short.display());
        for len in [40, 60, 70, 76] {
            let parent = PathBuf::from(format!("/{}", "c".repeat(len - 1)));
            let dir = data_dir_in(&parent, workspace);
            assert!(socket(&dir) <= crate::MAX_SOCKET_PATH, "{}", dir.display());
            assert!(dir.starts_with(&parent));
        }
    }

    #[test]
    fn resolved_follows_symlinks_and_dot_dots_before_anything_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("deep/real")).unwrap();
        std::os::unix::fs::symlink(base.join("deep/real"), base.join("link")).unwrap();
        assert_eq!(resolved(&base.join("link/../x/y")).unwrap(), base.join("deep/x/y"));
        assert_eq!(resolved(&base.join("new/../other/../ws/state")).unwrap(), base.join("ws/state"));
        assert_eq!(resolved(&base.join("deep/./real")).unwrap(), base.join("deep/real"));
        assert!(!base.join("new").exists() && !base.join("ws").exists());
    }

    #[test]
    fn a_run_removes_the_forks_it_left_except_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let work = data.join("work");
        std::fs::create_dir_all(work.join("fork-000000000001")).unwrap();
        std::fs::write(work.join("fork-000000000001.json"), "{}").unwrap();
        let cfg = Config::agent(Path::new("/w"), &data, Path::new("/b")).unwrap();
        let forks = RunForks::before(&cfg).expect("the default scratch is inside the data dir");
        for id in ["fork-000000000002", "fork-000000000003"] {
            std::fs::create_dir_all(work.join(id).join("src")).unwrap();
            std::fs::write(work.join(id).join("src/lib.rs"), "").unwrap();
            std::fs::write(work.join(format!("{id}.json")), "{}").unwrap();
        }
        forks.remove_new(Some(work.join("fork-000000000003").to_str().unwrap()));
        let mut left: Vec<_> =
            std::fs::read_dir(&work).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(
            left,
            ["fork-000000000001", "fork-000000000001.json", "fork-000000000003", "fork-000000000003.json"]
        );

        // A scratch dir outside the data dir may be shared with other runs.
        let mut shared = cfg.clone();
        let fs = shared.service_mut("fs").unwrap().exec.as_mut().unwrap();
        fs.args = vec!["fs".into(), "--root".into(), "/w".into(), "--scratch".into(), "/tmp/shared".into()];
        assert!(RunForks::before(&shared).is_none());
        let fs = shared.service_mut("fs").unwrap().exec.as_mut().unwrap();
        fs.args[4] = data.join("../elsewhere").to_str().unwrap().to_owned();
        assert!(RunForks::before(&shared).is_none());
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        assert_eq!(printable("ok\tline\nnext"), "ok\tline\nnext");
        assert_eq!(
            printable("a\x1b]52;c;Y3VybA==\x07b\x1b[2K\r\u{9b}31m\x7f"),
            "a\\u{1b}]52;c;Y3VybA==\\u{7}b\\u{1b}[2K\\r\\u{9b}31m\\u{7f}"
        );
        let call = Progress::ToolCall {
            run: String::new(),
            attempt: 0,
            turn: 1,
            tool: "run".into(),
            detail: "run echo \x1b[2Khi".into(),
        };
        assert_eq!(describe(&call), "attempt 0: run echo \\u{1b}[2Khi");
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
            patch_truncated: false,
            applied: true,
            fork: None,
            attempts: vec![],
            usage: Usage { input_tokens: 1000, output_tokens: 200, ..Usage::default() },
            cost_usd: 0.012,
            uncounted_calls: 0,
        };
        assert_eq!(
            report(&resp),
            "outcome: passed\ncheck: cargo test\nchanges:\n  modified src/lib.rs\napplied to the workspace\n\
             cost: $0.0120 (1200 tokens)\n\nFixed the parser.\n"
        );
        resp.applied = false;
        resp.fork = Some("/cache/work/fork_1".into());
        resp.uncounted_calls = 2;
        assert!(report(&resp).contains("not applied; the result is in /cache/work/fork_1\n"));
        assert!(report(&resp)
            .contains("cost: $0.0120 (1200 tokens), not counting 2 calls of cancelled attempts left unanswered\n"));
        resp.uncounted_calls = 0;
        resp.changes = vec![Change { path: "\x1b[1Aevil".into(), kind: ChangeKind::Added }];
        resp.summary = "Done.\x1b]0;title\x07\n".into();
        let shown = report(&resp);
        assert!(shown.contains("  added    \\u{1b}[1Aevil\n"), "{shown}");
        assert!(shown.ends_with("\nDone.\\u{1b}]0;title\\u{7}\n"), "{shown}");
        resp.changes.clear();
        resp.summary.clear();
        assert!(report(&resp).ends_with(
            "changes: none\nnot applied; the result is in /cache/work/fork_1\ncost: $0.0120 (1200 tokens)\n"
        ));
    }
}
