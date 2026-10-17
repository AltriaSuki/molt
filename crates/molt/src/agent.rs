//! `molt do`: start the agent services, have the planner carry out one
//! task, and report how it went.
//!
//! The CLI joins the bus in process as [`CLI`]. It holds only `planner.run`,
//! `topic:progress` and, to have memory learn from the finished run,
//! `memory.consolidate`; the planner does the work with its own capabilities.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use molt_api::fs::ChangeKind;
use molt_api::memory::{self, ConsolidateRequest, ConsolidateResponse};
use molt_api::planner::{
    self, AttemptStatus, Candidate, Outcome, Requirement, RunRequest, RunResponse, RunState, Verdict,
};
use molt_api::progress::{self, Progress};
use molt_kernel::Kernel;
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, ServiceId, Target, TraceId};
use molt_sdk::{CallOpts, SdkError, Service};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::project::{Project, Registry};
use crate::{Config, Running};

/// The identity `molt do` joins the bus with.
pub const CLI: &str = "cli";

/// The services a run needs.
pub const SERVICES: [&str; 4] = ["model", "fs", "shell", "planner"];
/// The service a run uses, and learns into, when the config has it.
pub const MEMORY: &str = "memory";

/// How long services get to come up.
pub(crate) const STARTUP: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_millis(50);
/// A method no service has: a live service answers it with `invalid`.
const PROBE: &str = "ping";
/// A live service answers a probe at once. On NATS a probe sent before the
/// service has subscribed is lost, and only this deadline ends it.
const PROBE_MS: u64 = 500;
/// Reply deadline for a whole run; the planner's own limits end it sooner.
const RUN_MS: u64 = 6 * 3600 * 1000;
/// Reply deadline for learning from a run.
const LEARN_MS: u64 = 600_000;
/// How long memory gets to show it still answers before learning starts.
const LEARN_PROBE: Duration = Duration::from_secs(10);

/// The run was stopped before it finished.
#[derive(Debug, thiserror::Error)]
#[error("interrupted")]
pub struct Interrupted;

/// The configuration `molt do` runs with: the file at `path` when one is
/// given (it must define the agent [`SERVICES`]), otherwise [`Config::agent`]
/// for the canonical `workspace`, with the service executables next to the
/// running one. No file is ever picked up on its own: one in the project
/// could run any command with the user's secrets. `data_dir` replaces the
/// configured data dir; the default setup otherwise uses `project`'s, or
/// [`default_data_dir`] without one, and has memory keep the notes about the
/// project under its id and the user's notes for every project next to the
/// registry ([`Registry::user_db`]).
pub fn resolve_config(
    path: Option<&Path>,
    workspace: &Path,
    data_dir: Option<&Path>,
    project: Option<&Project>,
) -> anyhow::Result<Config> {
    if let Some(path) = path {
        let mut cfg = Config::load(path)?;
        let trusted = !path.canonicalize()?.starts_with(workspace);
        if !trusted && !cfg.hooks.is_empty() {
            eprintln!("lifecycle hooks in a project configuration are ignored; keep trusted hook configuration outside the project");
            cfg.hooks.clear();
        }
        molt_planner::hooks::validate(&cfg.hooks)?;
        let hooks = serde_json::to_string(&cfg.hooks)?;
        if let Some(planner) = cfg.service_mut("planner") {
            planner.env.insert("MOLT_HOOKS_JSON".into(), hooks);
        }
        require_services(&cfg).with_context(|| format!("{} cannot run tasks", path.display()))?;
        if let Some(dir) = data_dir {
            cfg.kernel.data_dir = dir.to_path_buf();
        }
        return Ok(cfg);
    }
    if ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .all(|name| std::env::var_os(name).is_none_or(|key| key.is_empty()))
    {
        bail!(
            "ANTHROPIC_API_KEY or OPENAI_API_KEY is not set: export your provider API key, or pass --config with services of your own"
        );
    }
    let data_dir = match (data_dir, project) {
        (Some(dir), _) => dir.to_path_buf(),
        (None, Some(project)) => project.data_dir.clone(),
        (None, None) => default_data_dir(workspace)?,
    };
    // Checked before the directory is created, and again once it exists.
    let data_dir = resolved(&data_dir).with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &data_dir)?;
    crate::lock::create_private_dir(&data_dir)?;
    let data_dir = data_dir.canonicalize().with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &data_dir)?;
    let mut cfg = Config::agent(workspace, &data_dir, &bin_dir()?)?;
    if let Some(project) = project {
        cfg.for_project(&project.id, &project_paths(project, workspace))?;
        cfg.with_user_db(&Registry::user()?.user_db())?;
    }
    Ok(cfg)
}

/// The workspace to work in and its project: the project `named` (by id or
/// name) and its directory, or else the canonical `workspace` and the
/// project it is a directory of, registered now if it is in none. A custom
/// config, or a host without a registry, has no project.
pub fn locate(
    named: Option<&str>,
    workspace: &Path,
    custom_config: bool,
) -> anyhow::Result<(PathBuf, Option<Project>)> {
    if let Some(named) = named {
        let registry = Registry::user()?;
        let project = registry.find(named)?;
        return Ok((registry.workspace(&project)?, Some(project)));
    }
    let workspace = workspace.canonicalize().with_context(|| format!("workspace {}", workspace.display()))?;
    anyhow::ensure!(workspace.is_dir(), "the workspace {} is not a directory", workspace.display());
    if custom_config {
        return Ok((workspace, None));
    }
    let registry = match Registry::user() {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!("{e:#}; notes are kept by the workspace's path");
            return Ok((workspace, None));
        }
    };
    let project = registry.for_path(&workspace, || default_data_dir(&workspace))?;
    Ok((workspace, Some(project)))
}

/// The directories memory keeps `project`'s notes for: its own, or the one it
/// works in when it has none.
fn project_paths(project: &Project, workspace: &Path) -> Vec<PathBuf> {
    if project.paths.is_empty() {
        vec![workspace.to_path_buf()]
    } else {
        project.paths.clone()
    }
}

/// The data dir of a project made with `molt project new`, as
/// [`default_data_dir`] names them, from its name and id.
pub fn project_data_dir(name: &str, id: &str) -> anyhow::Result<PathBuf> {
    let slug: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    default_data_dir(&Path::new("/").join(id).join(slug.trim_matches('_')))
}

/// [`Config::allow_web`] with the services next to this executable.
pub fn allow_web(cfg: &mut Config, hosts: &[String]) -> anyhow::Result<()> {
    if hosts.is_empty() {
        return Ok(());
    }
    cfg.allow_web(hosts, &bin_dir()?)
}

/// The directory of the running executable, where the services are.
fn bin_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the molt executable")?;
    Ok(exe.parent().context("the molt executable has no parent directory")?.to_path_buf())
}

/// The configuration `molt memory` works with: the file at `path` when one
/// is given (with `data_dir` in place of its own, when given), otherwise the
/// default setup of `molt do` for the canonical `workspace`, which must have
/// run there before (nothing is created).
pub fn memory_config(
    path: Option<&Path>,
    workspace: &Path,
    data_dir: Option<&Path>,
    project: Option<&Project>,
) -> anyhow::Result<Config> {
    let cfg = match path {
        Some(path) => {
            let mut cfg = Config::load(path)?;
            if let Some(dir) = data_dir {
                cfg.kernel.data_dir = dir.to_path_buf();
            }
            cfg
        }
        None => {
            let data_dir = match (data_dir, project) {
                (Some(dir), _) => dir.to_path_buf(),
                (None, Some(project)) => project.data_dir.clone(),
                (None, None) => default_data_dir(workspace)?,
            };
            if !data_dir.is_dir() {
                bail!(
                    "molt has not run in {} yet (no data dir at {}); memory starts with the first `molt do`",
                    workspace.display(),
                    data_dir.display()
                );
            }
            let data_dir = data_dir.canonicalize().with_context(|| format!("data dir {}", data_dir.display()))?;
            let mut cfg = Config::agent(workspace, &data_dir, &bin_dir()?)?;
            if let Some(project) = project {
                cfg.for_project(&project.id, &project_paths(project, workspace))?;
                cfg.with_user_db(&Registry::user()?.user_db())?;
            }
            cfg
        }
    };
    cfg.service(MEMORY).context("the config has no memory service")?;
    Ok(cfg)
}

/// The database the memory service of `cfg` keeps, from its `--db`
/// argument (a relative one is relative to the current directory, as it is
/// for the service).
pub fn memory_db(cfg: &Config) -> Option<PathBuf> {
    let args = &cfg.service(MEMORY)?.exec.as_ref()?.args;
    args.iter().skip_while(|a| *a != "--db").nth(1).map(PathBuf::from)
}

/// The directories the memory service's project model leaves out (its
/// `--skip` arguments), canonical where they exist.
pub fn memory_skips(cfg: &Config) -> Vec<PathBuf> {
    let Some(args) = cfg.service(MEMORY).and_then(|s| s.exec.as_ref()).map(|e| &e.args) else { return Vec::new() };
    args.windows(2)
        .filter(|pair| pair[0] == "--skip")
        .filter_map(|pair| Path::new(&pair[1]).canonicalize().ok())
        .collect()
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
    let longest = SERVICES.iter().chain([&CLI, &MEMORY]).map(|s| s.len()).max().unwrap_or_default();
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

/// How memory learns from a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Learn {
    /// The model memory learns with; `None` for memory's own default
    /// (`MOLT_MEMORY_MODEL`, else sonnet).
    pub model: Option<String>,
}

/// The model memory learns with, from what the user asked for: a model,
/// [`SAME_MODEL`] for the attempts' model, or nothing for memory's default.
pub fn memory_model(asked: Option<&str>, attempts: Option<&str>) -> Option<String> {
    match asked.map(str::trim).filter(|m| !m.is_empty()) {
        Some(SAME_MODEL) => attempts.map(str::to_owned),
        other => other.map(str::to_owned),
    }
}

/// Asks memory to learn with the model the attempts used.
pub const SAME_MODEL: &str = "same";

/// What a run produced.
#[derive(Debug)]
pub struct Done {
    pub run: RunResponse,
    /// What memory learned from the run; `None` when it was not asked to
    /// learn (learning is off, or the config has no memory service). An
    /// error when it could not: memory is down, the model failed, or the
    /// run was interrupted while memory was learning.
    pub learned: Option<Result<ConsolidateResponse, String>>,
}

/// Carry out one task: start the kernel and the services of `cfg`, ask the
/// planner to run `req`, hand this run's progress events to `on_progress`,
/// then, with `learn` and a memory service in `cfg`, have memory learn from
/// the run, and shut everything down again. If `stop` resolves before the
/// planner answers, the run is abandoned with [`Interrupted`]; if it
/// resolves while memory learns, only the learning is. The forks the run
/// leaves behind, other than the one its result names, are then removed
/// (see [`RunForks`]).
pub async fn run_task(
    cfg: &Config,
    req: RunRequest,
    learn: Option<Learn>,
    on_progress: impl FnMut(&Progress),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<Done> {
    run_task_as(cfg, req, TraceId::random(), learn, on_progress, stop).await
}

/// [`run_task`] under the run id `trace`: a resumed run goes on under its
/// own. A checkpointed run that stops before it finishes keeps every fork
/// it made, for the resume.
pub async fn run_task_as(
    cfg: &Config,
    req: RunRequest,
    trace: TraceId,
    learn: Option<Learn>,
    mut on_progress: impl FnMut(&Progress),
    stop: impl Future<Output = ()>,
) -> anyhow::Result<Done> {
    require_services(cfg)?;
    if req.stream {
        require_stream_capability(cfg)?;
    }
    let running = crate::start(cfg).await?;
    // Before planner.run, the only caller that makes forks.
    let forks = RunForks::before(cfg);
    let workspace = req.workspace.clone();
    let budget_usd = req.budget_usd;
    let checkpointed = req.checkpoint || req.resume.is_some();
    tokio::pin!(stop);
    let result = tokio::select! {
        // A Ctrl-C in a terminal also reaches the services, and the run may
        // fail from their exit at the same moment: the interruption wins.
        biased;
        () = &mut stop => Err(Interrupted.into()),
        result = drive(&running, cfg, req, trace, &mut on_progress) => result,
    };
    let done = match result {
        Ok(ran) => {
            let learned = match (learn, ran.memory) {
                (None, _) | (Some(_), None) => None,
                (Some(_), Some(Err(e))) => Some(Err(e)),
                // Learning is a model call too: the run's budget covers it.
                (Some(_), Some(Ok(())))
                    if (ran.resp.uncounted_calls > 0 || budget_usd.is_some_and(|b| ran.resp.cost_usd >= b)) =>
                {
                    Some(Err("skipped: the budget is exhausted or model settlement is incomplete".to_owned()))
                }
                (Some(learn), Some(Ok(()))) => {
                    on_progress(&Progress::Note {
                        run: ran.trace.to_string(),
                        message: "learning from the run".into(),
                    });
                    let learn = async {
                        // Memory may have stopped during the run; a call to it then waits out its whole deadline.
                        let kernel = running.kernel();
                        wait_until_up(kernel, &ran.cli, MEMORY, Instant::now() + LEARN_PROBE)
                            .await
                            .map_err(|e| format!("{e:#}"))?;
                        consolidate(kernel, &ran.cli, &ran.trace, &workspace, learn.model).await
                    };
                    tokio::select! {
                        biased;
                        () = &mut stop => Some(Err("interrupted; what it spent is not counted".to_owned())),
                        learned = learn => Some(learned),
                    }
                }
            };
            Ok(Done { run: ran.resp, learned })
        }
        Err(e) => Err(e),
    };
    running.shutdown().await;
    // A foreground-group signal can make a service exit before Tokio has
    // delivered the same signal to our listener. Recheck after shutdown,
    // while preserving successful runs and never polling a completed stop.
    let done = if done.as_ref().is_err_and(|error| !error.is::<Interrupted>()) {
        tokio::select! {
            biased;
            () = &mut stop => Err(Interrupted.into()),
            () = std::future::ready(()) => done,
        }
    } else {
        done
    };
    let kept = checkpointed && done.as_ref().map_or(true, |d| d.run.checkpoint.is_some());
    if let Some(forks) = forks.filter(|_| !kept) {
        let keep = done.as_ref().ok().and_then(|d| d.run.fork.as_deref());
        forks.remove_new(keep.as_slice());
    }
    done
}

/// Where the planner of `cfg` keeps run checkpoints: its `--state` argument.
pub fn checkpoint_dir(cfg: &Config) -> Option<PathBuf> {
    let args = &cfg.service("planner")?.exec.as_ref()?.args;
    args.iter().skip_while(|a| *a != "--state").nth(1).map(PathBuf::from)
}

fn require_stream_capability(cfg: &Config) -> anyhow::Result<()> {
    let target: Target = format!("topic:publish:{}", progress::TOPIC).parse()?;
    let allowed =
        cfg.service("model").is_some_and(|s| s.requests.iter().any(|r| r.target.covers(&target) && r.budget.calls > 0));
    if !allowed {
        bail!("--stream requires the model service to request a topic:publish:progress capability (legacy topic:progress also permits publication) with a nonzero calls budget");
    }
    Ok(())
}

/// The forks in the `fs` service's scratch dir when a run starts. A planner
/// stopped mid-run (interrupted, crashed, killed) never drops its forks,
/// each a full copy of the project. Only a scratch dir inside the data dir is
/// looked after: the data dir is locked to this run, so whatever appears
/// there during it is the run's own.
pub(crate) struct RunForks {
    scratch: PathBuf,
    before: HashSet<OsString>,
}

impl RunForks {
    pub(crate) fn before(cfg: &Config) -> Option<Self> {
        let args = &cfg.service("fs")?.exec.as_ref()?.args;
        let scratch = std::path::absolute(args.iter().skip_while(|a| *a != "--scratch").nth(1)?).ok()?;
        let data_dir = std::path::absolute(&cfg.kernel.data_dir).ok()?;
        let plain = !scratch.components().any(|c| c == Component::ParentDir);
        (plain && scratch.starts_with(data_dir)).then(|| Self { before: fork_entries(&scratch), scratch })
    }

    /// Remove the forks made since [`RunForks::before`], except those in `keep`.
    pub(crate) fn remove_new(self, keep: &[&str]) {
        let keep: Vec<&str> =
            keep.iter().filter_map(|fork| Path::new(fork).file_name()).filter_map(|name| name.to_str()).collect();
        for name in fork_entries(&self.scratch).difference(&self.before) {
            let fork = name.to_str().map(|n| n.strip_suffix(".json").unwrap_or(n));
            if fork.is_some_and(|fork| keep.contains(&fork)) {
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

/// A run the planner answered.
struct Ran {
    cli: Service,
    trace: TraceId,
    resp: RunResponse,
    /// `None` without a memory service; an error when it did not come up.
    memory: Option<Result<(), String>>,
}

async fn drive(
    running: &Running,
    cfg: &Config,
    req: RunRequest,
    trace: TraceId,
    mut on_progress: impl FnMut(&Progress),
) -> anyhow::Result<Ran> {
    let kernel = running.kernel();
    let cli = join_bus(running).await?;
    let deadline = Instant::now() + STARTUP;
    for service in SERVICES {
        wait_until_up(kernel, &cli, service, deadline).await?;
    }
    // A run goes ahead without memory: the planner finds it unavailable.
    let memory = match cfg.service(MEMORY) {
        None => None,
        Some(_) => Some(wait_until_up(kernel, &cli, MEMORY, deadline).await.map_err(|e| format!("{e:#}"))),
    };
    if let Some(Err(e)) = &memory {
        let message = format!("running without memory: {e}");
        on_progress(&Progress::Note { run: trace.to_string(), message });
    }
    grant(kernel, &cli, planner::RUN, Budget::new(0, 0, 1_000)).await?;
    grant(kernel, &cli, &format!("topic:subscribe:{}", progress::TOPIC), Budget::new(0, 0, 1_000)).await?;
    cli.subscribe(progress::TOPIC).await?;

    let mut preview = PreviewFilter::default();
    let reply = {
        let call = call_planner(&cli, &req, &trace);
        tokio::pin!(call);
        loop {
            tokio::select! {
                // Events go first. The link hands over the events published
                // before the reply ahead of it, so none is left unshown.
                biased;
                Some(msg) = cli.next() => {
                    if let Some(event) = progress_of(&msg, trace.as_str()) {
                        if let Some(gap) = preview.accept(&event) {
                            if gap {
                                on_progress(&Progress::Note { run: trace.to_string(), message: "some model preview events were omitted; the final reply remains complete".into() });
                            }
                            on_progress(&event);
                        }
                    }
                }
                reply = &mut call => break reply?,
            }
        }
    };
    let resp = serde_json::from_value(reply).context("the planner's result is not a planner.run response")?;
    Ok(Ran { cli, trace, resp, memory })
}

/// Have memory learn from the run `episode` in `workspace`. The call has a
/// trace of its own, so it does not become part of the episode.
async fn consolidate(
    kernel: &Kernel,
    cli: &Service,
    episode: &TraceId,
    workspace: &str,
    model: Option<String>,
) -> Result<ConsolidateResponse, String> {
    grant(kernel, cli, memory::CONSOLIDATE, Budget::new(0, 0, 10)).await.map_err(|e| format!("{e:#}"))?;
    let req = ConsolidateRequest { episode: episode.to_string(), workspace: workspace.to_owned(), model };
    let payload = serde_json::to_value(req).map_err(|e| e.to_string())?;
    let opts = CallOpts { cap: None, budget: Budget::new(0, LEARN_MS, 0), trace: None };
    let reply = cli.call(memory::CONSOLIDATE, payload, opts).await.map_err(|e| e.to_string())?;
    serde_json::from_value(reply).map_err(|e| format!("memory's answer is not a consolidate response: {e}"))
}

/// Call `target` on the memory service of `cfg`, started on its own: what
/// `molt memory forget` uses, so that the change is in the audit log.
pub async fn call_memory(cfg: &Config, target: &str, payload: Value) -> anyhow::Result<Value> {
    let service = cfg.service(MEMORY).context("the config has no memory service")?.clone();
    let alone = Config {
        kernel: cfg.kernel.clone(),
        services: vec![service],
        confirm: cfg.confirm.clone(),
        hooks: cfg.hooks.clone(),
    };
    let running = crate::start(&alone).await?;
    let result = async {
        let kernel = running.kernel();
        let cli = join_bus(&running).await?;
        wait_until_up(kernel, &cli, MEMORY, Instant::now() + STARTUP).await?;
        grant(kernel, &cli, target, Budget::new(0, 0, 1)).await?;
        let opts = CallOpts { cap: None, budget: Budget::new(0, 60_000, 0), trace: None };
        cli.call(target, payload, opts).await.with_context(|| format!("{target} failed"))
    }
    .await;
    running.shutdown().await;
    result
}

/// Register [`CLI`] with the kernel and connect to it like a service would.
pub(crate) async fn join_bus(running: &Running) -> anyhow::Result<Service> {
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

pub(crate) async fn grant(kernel: &Kernel, cli: &Service, target: &str, budget: Budget) -> anyhow::Result<CapId> {
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
pub(crate) async fn wait_until_up(
    kernel: &Kernel,
    cli: &Service,
    service: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
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
pub(crate) fn progress_of(msg: &Envelope, run: &str) -> Option<Progress> {
    progress_event(msg).filter(|event| event.run() == run)
}

/// Authenticate model previews before either front end sees them. Ordinary
/// planner events carry their run in the payload; their publication request
/// may have its own trace, so do not impose a model-call trace on those.
pub(crate) fn progress_event(msg: &Envelope) -> Option<Progress> {
    let on_topic = matches!(&msg.to, Target::Topic { name } if name == progress::TOPIC);
    if msg.kind != Kind::Event || !on_topic {
        return None;
    }
    let event: Progress = serde_json::from_value(msg.payload.clone()).ok()?;
    // The kernel stamps the sender. A service with topic access cannot
    // impersonate the gateway by choosing a model event payload.
    if event.model_call().is_some()
        && (msg.from.as_ref().map(ServiceId::as_str) != Some("model") || msg.trace_id.as_str() != event.run())
    {
        return None;
    }
    Some(event)
}

#[derive(Default)]
struct PreviewFilter {
    calls: HashMap<String, (u64, bool)>,
    finished: HashSet<u32>,
}

impl PreviewFilter {
    /// None drops duplicates or previews of finished attempts. Some(true)
    /// announces a gap in best-effort progress, without repeating any text.
    fn accept(&mut self, event: &Progress) -> Option<bool> {
        if let Progress::AttemptFinished { attempt, .. } = event {
            self.finished.insert(*attempt);
        }
        let Some(context) = event.model_call() else { return Some(false) };
        if context.attempt.is_some_and(|a| self.finished.contains(&a)) {
            return None;
        }
        let previous = self.calls.get(&context.call).copied();
        if previous.is_some_and(|(seq, closed)| closed || context.seq <= seq) {
            return None;
        }
        let gap = match previous {
            Some((seq, _)) => context.seq != seq + 1,
            None => context.seq != 0,
        };
        self.calls.insert(context.call.clone(), (context.seq, matches!(event, Progress::ModelFinished { .. })));
        Some(gap)
    }
}

/// One short line for the terminal, made [`printable`].
pub fn describe(event: &Progress) -> String {
    printable(&match event {
        Progress::ModelStarted { context } => format!("{}: model started (cost pending)", model_label(context)),
        Progress::ModelText { context, text } => format!("{} preview: {text}", model_label(context)),
        Progress::ModelFinished { context, usage, cost_usd, error } => {
            let cost = cost_usd.map_or_else(|| "cost unknown".into(), |cost| format!("cost ${cost:.4}"));
            let tokens = usage.map_or_else(String::new, |u| format!(", {} tokens", u.total()));
            let result = error.as_ref().map_or_else(|| "model finished".into(), |e| format!("model failed: {e}"));
            format!("{}: {result} ({cost}{tokens})", model_label(context))
        }
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
        Progress::CheckReady { command: None, designed: true, .. } => {
            "no automated check fits this task; the result will be unverified".to_owned()
        }
        Progress::CheckReady { command: None, designed: false, .. } => {
            "running without a done-check, as asked; the result will be unverified".to_owned()
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
        Progress::Required { requirement, .. } => requirement_line(requirement),
        Progress::Adopted { attempt, version, turn, .. } => {
            format!("attempt {attempt}: follows requirements v{version} from turn {turn}")
        }
        Progress::State { state: RunState::Running, .. } => "going on".to_owned(),
        Progress::State { state: RunState::Paused, .. } => "paused".to_owned(),
        Progress::State { state: RunState::Pausing, waiting, .. } => {
            format!("pausing; waiting for {}", waiting.join(", "))
        }
        Progress::Recalled { run, notes, map_tokens } => {
            let mut parts = Vec::new();
            if let Some(tokens) = map_tokens {
                parts.push(format!("a project map of about {tokens} tokens"));
            }
            match notes.len() {
                0 => {}
                1 => parts.push("1 note from earlier tasks".to_owned()),
                n => parts.push(format!("{n} notes from earlier tasks")),
            }
            let mut line = format!("memory: {}", parts.join(" and "));
            if !notes.is_empty() {
                line.push_str(&format!("; inspect their snapshot with molt memory used {run}"));
            }
            line
        }
    })
}

/// A new version of a run's requirements as one line.
pub fn requirement_line(requirement: &Requirement) -> String {
    let c = &requirement.constraint;
    let mut parts = Vec::new();
    if !c.text.is_empty() {
        parts.push(c.text.clone());
    }
    if !c.protect.is_empty() {
        parts.push(format!("leave {} unchanged", c.protect.join(", ")));
    }
    if let Some(check) = &c.check {
        parts.push(format!("check: {check}"));
    }
    format!("requirements v{}: {}", requirement.version, parts.join("; "))
}

fn model_label(context: &molt_api::progress::ModelCall) -> String {
    let actor = context.attempt.map_or_else(|| "designer".into(), |a| format!("attempt {a}"));
    format!("{actor}, turn {}, call {}", context.turn, context.call)
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
    if let Some(review) = &resp.review {
        lines.push(format!("review: {}", verdict_name(review.verdict)));
        lines.extend(review.findings.iter().map(|f| format!("  - {f}")));
    }
    lines.extend(selection_lines(resp));
    if resp.applied {
        lines.push("applied to the workspace".to_owned());
    } else if let Some(fork) = &resp.fork {
        lines.push(format!("not applied; the result is in {fork}"));
    }
    let mut cost = format!("known cost: ${:.4} ({} tokens)", resp.cost_usd, resp.usage.total());
    if resp.uncounted_calls > 0 {
        cost.push_str(&format!(", {} model calls have unknown or unsettled cost", resp.uncounted_calls));
    }
    lines.push(cost);
    let summary = resp.summary.trim();
    if !summary.is_empty() {
        lines.push(String::new());
        lines.push(summary.to_owned());
    }
    printable(&(lines.join("\n") + "\n"))
}

/// How the winner was picked among several passing attempts: one line per
/// candidate, best ranked first. Empty when there was no comparison.
pub fn selection_lines(resp: &RunResponse) -> Vec<String> {
    let Some(selection) = &resp.selection else { return Vec::new() };
    let mut lines = vec![format!("picked among {} passing attempts:", selection.candidates.len())];
    for c in &selection.candidates {
        let mark = if resp.winner == Some(c.attempt) { "*" } else { " " };
        lines.push(format!(" {mark} attempt {}: {}", c.attempt, candidate_scores(c)));
    }
    lines
}

/// What a passing candidate was compared on, in a few words.
pub fn candidate_scores(c: &Candidate) -> String {
    let mut parts = Vec::new();
    match c.suite_passed {
        Some(true) => parts.push("suite passed".to_owned()),
        Some(false) => parts.push("suite failed".to_owned()),
        None => {}
    }
    let at_least = if c.truncated { "≥" } else { "" };
    parts.push(format!("{at_least}{} lines in {} files", c.lines_changed, c.files_changed));
    if let Some(verdict) = c.verdict {
        parts.push(format!("review: {}", verdict_name(verdict)));
    }
    parts.join(", ")
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "approve",
        Verdict::Concern => "concern",
        Verdict::Reject => "reject",
    }
}

/// What memory learned from a run, as one line for the terminal, made
/// [`printable`].
/// What learning did, for `molt do --json`: memory's answer, or `{"error": ...}`.
pub fn learning_json(learned: &Result<ConsolidateResponse, String>) -> Value {
    match learned {
        Ok(r) => serde_json::to_value(r).unwrap_or(Value::Null),
        Err(e) => serde_json::json!({ "error": e }),
    }
}

pub fn learned(learned: &Result<ConsolidateResponse, String>) -> String {
    let line = match learned {
        Err(e) => format!("memory: learned nothing: {e}"),
        Ok(r) => {
            let mut parts = Vec::new();
            let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
            if !r.added.is_empty() {
                parts.push(format!("learned {}", count(r.added.len(), "note", "notes")));
            }
            if !r.reinforced.is_empty() {
                parts.push(format!("confirmed {}", count(r.reinforced.len(), "note", "notes")));
            }
            if !r.contradicted.is_empty() {
                parts.push(format!("disputed {}", count(r.contradicted.len(), "note", "notes")));
            }
            let what = if parts.is_empty() {
                format!("learned nothing new ({})", r.skipped.as_deref().unwrap_or("nothing worth keeping"))
            } else {
                parts.join(", ")
            };
            let mut line = format!("memory: {what}; cost ${:.4} ({} tokens)", r.cost_usd, r.usage.total());
            for note in &r.added {
                line.push_str(&format!("\n  + [{}] {}", note.kind.as_str(), note.text));
            }
            line
        }
    };
    printable(&line)
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
    use molt_api::planner::{CheckSpec, Review};
    use serde_json::json;

    use super::*;

    #[test]
    fn only_configuration_outside_the_project_can_install_hooks() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let services = SERVICES
            .iter()
            .map(|name| format!("[[service]]\nname = \"{name}\"\nexec = {{ command = \"/bin/true\" }}\n"))
            .collect::<String>();
        let config = format!("{services}\n[[hooks]]\nevent = \"before_apply\"\ncommand = \"true\"\nblocking = true\n");
        let outside = root.path().join("config.toml");
        std::fs::write(&outside, &config).unwrap();
        let trusted = resolve_config(Some(&outside), &workspace, None, None).unwrap();
        assert_eq!(trusted.hooks.len(), 1);
        assert!(trusted.service("planner").unwrap().env["MOLT_HOOKS_JSON"].contains("before_apply"));
        let inside = workspace.join("config.toml");
        std::fs::write(&inside, config).unwrap();
        let ignored = resolve_config(Some(&inside), &workspace, None, None).unwrap();
        assert!(ignored.hooks.is_empty());
        assert_eq!(ignored.service("planner").unwrap().env["MOLT_HOOKS_JSON"], "[]");
    }

    #[test]
    fn memory_learns_with_the_model_asked_for_or_the_attempts_one() {
        assert_eq!(memory_model(None, Some("opus")), None);
        assert_eq!(memory_model(Some(" "), Some("opus")), None);
        assert_eq!(memory_model(Some("haiku"), Some("opus")).as_deref(), Some("haiku"));
        assert_eq!(memory_model(Some("same"), Some("opus")).as_deref(), Some("opus"));
        assert_eq!(memory_model(Some("same"), None), None);
    }

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
        std::fs::create_dir_all(work.join(".molt/applies/apply-0000000000000001")).unwrap();
        std::fs::write(work.join(".molt/applies/apply-0000000000000001/journal.json"), "recovery material").unwrap();
        forks.remove_new(&[work.join("fork-000000000003").to_str().unwrap()]);
        assert_eq!(
            std::fs::read_to_string(work.join(".molt/applies/apply-0000000000000001/journal.json")).unwrap(),
            "recovery material"
        );
        let mut left: Vec<_> =
            std::fs::read_dir(&work).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(
            left,
            [".molt", "fork-000000000001", "fork-000000000001.json", "fork-000000000003", "fork-000000000003.json"]
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
        // Memory is not required: a run goes ahead without it.
        let mut no_memory = Config::agent(Path::new("/w"), Path::new("/d"), Path::new("/b")).unwrap();
        no_memory.services.retain(|s| s.name.as_str() != MEMORY);
        require_services(&no_memory).unwrap();
        assert!(err.contains("missing the services model, fs, shell, planner"), "{err}");
        let mut cfg = Config::agent(Path::new("/w"), Path::new("/d"), Path::new("/b")).unwrap();
        require_services(&cfg).unwrap();
        cfg.services.retain(|s| s.name.as_str() != "shell");
        let err = require_services(&cfg).unwrap_err().to_string();
        assert!(err.contains("missing the services shell "), "{err}");
    }

    #[tokio::test]
    async fn streaming_refuses_a_custom_setup_without_progress_permission_before_starting() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut cfg = Config::agent(Path::new("/w"), &data, Path::new("/b")).unwrap();
        cfg.service_mut("model").unwrap().requests.clear();
        let request = RunRequest { stream: true, ..RunRequest::new("task", "/w") };
        let error = run_task(&cfg, request, None, |_| {}, std::future::pending()).await.unwrap_err();
        assert!(error.to_string().contains("topic:progress"));
        assert!(!data.exists());
    }

    #[test]
    fn streaming_accepts_legacy_publication_but_not_a_subscription_grant() {
        let mut cfg = Config::agent(Path::new("/w"), Path::new("/d"), Path::new("/b")).unwrap();
        for (right, allowed) in
            [("topic:publish:progress", true), ("topic:progress", true), ("topic:subscribe:progress", false)]
        {
            cfg.service_mut("model").unwrap().requests[0].target = right.parse().unwrap();
            assert_eq!(require_stream_capability(&cfg).is_ok(), allowed);
        }
        cfg.service_mut("model").unwrap().requests[0].budget.calls = 0;
        assert!(require_stream_capability(&cfg).is_err());
    }

    #[test]
    fn only_this_runs_progress_is_shown() {
        let event = |run: &str| {
            let payload = json!({ "event": "attempt_started", "run": run, "attempt": 1 });
            Envelope::event(TraceId::random(), progress::TOPIC, CapId::random(), payload).unwrap()
        };
        assert_eq!(
            progress_of(&event("trace_a"), "trace_a"),
            Some(Progress::AttemptStarted { run: "trace_a".into(), attempt: 1, fork: None })
        );
        assert_eq!(progress_of(&event("trace_b"), "trace_a"), None);
        // The session's shared filter must preserve planner publications
        // whose envelope trace differs from the run in their payload.
        assert_eq!(
            progress_event(&event("trace_a")),
            Some(Progress::AttemptStarted { run: "trace_a".into(), attempt: 1, fork: None })
        );
        let mut other = event("trace_a");
        other.to = "topic:builds".parse().unwrap();
        assert_eq!(progress_of(&other, "trace_a"), None);
    }

    #[test]
    fn model_previews_require_the_gateway_and_the_matching_trace() {
        let trace = TraceId::random();
        let event = Progress::ModelText {
            context: molt_api::progress::ModelCall {
                run: trace.to_string(),
                attempt: Some(1),
                turn: 2,
                call: "call-a".into(),
                seq: 1,
            },
            text: "hi".into(),
        };
        let mut msg =
            Envelope::event(trace.clone(), progress::TOPIC, CapId::random(), serde_json::to_value(&event).unwrap())
                .unwrap();
        msg.from = Some(ServiceId::new("planner").unwrap());
        assert_eq!(progress_of(&msg, trace.as_str()), None);
        msg.from = Some(ServiceId::new("model").unwrap());
        assert_eq!(progress_of(&msg, trace.as_str()), Some(event));
        msg.trace_id = TraceId::random();
        assert_eq!(progress_of(&msg, trace.as_str()), None);
    }

    #[test]
    fn previews_keep_calls_separate_detect_gaps_and_ignore_cancelled_attempts() {
        let mut filter = PreviewFilter::default();
        let text = |call: &str, seq| Progress::ModelText {
            context: molt_api::progress::ModelCall {
                run: "run".into(),
                attempt: Some(1),
                turn: 2,
                call: call.into(),
                seq,
            },
            text: "hi".into(),
        };
        assert_eq!(filter.accept(&text("a", 1)), Some(true));
        assert_eq!(filter.accept(&text("a", 1)), None);
        assert_eq!(filter.accept(&text("a", 0)), None);
        assert_eq!(filter.accept(&text("a", 2)), Some(false));
        assert_eq!(filter.accept(&text("b", 0)), Some(false));
        assert_eq!(filter.accept(&text("b", 2)), Some(true));
        let context = text("a", 3).model_call().unwrap().clone();
        assert_eq!(
            filter.accept(&Progress::ModelFinished {
                context,
                usage: None,
                cost_usd: None,
                error: Some("broken stream".into())
            }),
            Some(false)
        );
        assert_eq!(filter.accept(&text("a", 4)), None);
        filter.accept(&Progress::AttemptFinished { run: "run".into(), attempt: 1, status: AttemptStatus::Cancelled });
        assert_eq!(filter.accept(&text("b", 3)), None);
    }

    #[test]
    fn preview_display_escapes_control_characters_and_shows_unknown_cost() {
        let context = molt_api::progress::ModelCall {
            run: "run".into(),
            attempt: Some(1),
            turn: 2,
            call: "call-a".into(),
            seq: 1,
        };
        let preview = describe(&Progress::ModelText { context: context.clone(), text: "\x1b[2Khi".into() });
        assert_eq!(preview, "attempt 1, turn 2, call call-a preview: \\u{1b}[2Khi");
        let finished = describe(&Progress::ModelFinished {
            context,
            usage: None,
            cost_usd: None,
            error: Some("interrupted".into()),
        });
        assert!(finished.contains("cost unknown"));
        assert!(!finished.contains("$0.0000"));
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
            Progress::CheckReady { run: run.clone(), command: None, files: vec![], designed: false },
            Progress::ToolCall {
                run: run.clone(),
                attempt: 1,
                turn: 3,
                tool: "read_file".into(),
                detail: "read a.rs".into(),
            },
            Progress::CheckRan { run: run.clone(), attempt: 0, passed: false, exit_code: Some(1), tail: "boom".into() },
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
                "running without a done-check, as asked; the result will be unverified",
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
            check: Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: false, suite: None }),
            winner: Some(0),
            summary: "Fixed the parser.\n".into(),
            changes: vec![Change { path: "src/lib.rs".into(), kind: ChangeKind::Modified }],
            patch: String::new(),
            patch_truncated: false,
            applied: true,
            fork: None,
            review: None,
            attempts: vec![],
            usage: Usage { input_tokens: 1000, output_tokens: 200, ..Usage::default() },
            cost_usd: 0.012,
            uncounted_calls: 0,
            checkpoint: None,
            requirements: vec![],
            selection: None,
        };
        assert_eq!(
            report(&resp),
            "outcome: passed\ncheck: cargo test\nchanges:\n  modified src/lib.rs\napplied to the workspace\n\
             known cost: $0.0120 (1200 tokens)\n\nFixed the parser.\n"
        );
        resp.applied = false;
        resp.fork = Some("/cache/work/fork_1".into());
        resp.uncounted_calls = 2;
        assert!(report(&resp).contains("not applied; the result is in /cache/work/fork_1\n"));
        assert!(
            report(&resp).contains("known cost: $0.0120 (1200 tokens), 2 model calls have unknown or unsettled cost\n")
        );
        resp.uncounted_calls = 0;
        resp.review = Some(Review { verdict: Verdict::Reject, findings: vec!["src/lib.rs:3 returns 42.".into()] });
        assert!(
            report(&resp).contains("review: reject\n  - src/lib.rs:3 returns 42.\nnot applied"),
            "{}",
            report(&resp)
        );
        resp.review = None;
        let candidate = |attempt, suite_passed, lines_changed, verdict| planner::Candidate {
            attempt,
            suite_passed,
            lines_changed,
            files_changed: 1,
            truncated: false,
            verdict,
        };
        resp.selection = Some(planner::Selection {
            rule: "fewer lines".into(),
            candidates: vec![candidate(0, Some(true), 3, Some(Verdict::Approve)), candidate(1, Some(false), 1, None)],
        });
        assert!(
            report(&resp).contains(
                "picked among 2 passing attempts:\n * attempt 0: suite passed, 3 lines in 1 files, review: approve\n   \
                 attempt 1: suite failed, 1 lines in 1 files\nnot applied"
            ),
            "{}",
            report(&resp)
        );
        resp.selection = None;
        resp.changes = vec![Change { path: "\x1b[1Aevil".into(), kind: ChangeKind::Added }];
        resp.summary = "Done.\x1b]0;title\x07\n".into();
        let shown = report(&resp);
        assert!(shown.contains("  added    \\u{1b}[1Aevil\n"), "{shown}");
        assert!(shown.ends_with("\nDone.\\u{1b}]0;title\\u{7}\n"), "{shown}");
        resp.changes.clear();
        resp.summary.clear();
        assert!(report(&resp).ends_with(
            "changes: none\nnot applied; the result is in /cache/work/fork_1\nknown cost: $0.0120 (1200 tokens)\n"
        ));
    }
}
