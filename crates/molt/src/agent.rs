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
use molt_api::model::{attempt_name, speaker};
use molt_api::planner::{
    self, AttemptStatus, Candidate, Outcome, Requirement, RunRequest, RunResponse, RunState, Verdict,
};
use molt_api::progress::{self, Progress};
use molt_kernel::Kernel;
use molt_proto::{Budget, CapId, Envelope, ErrorCode, Kind, ServiceId, Target, TraceId};
use molt_sdk::{CallOpts, SdkError, Service};
use molt_tools::SandboxPolicy;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::layers::Layered;
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
/// given (it must define the agent [`SERVICES`]), otherwise [`layered`] for
/// the canonical `workspace`, with the service executables next to the
/// running one. `profile` picks the file's, or the user config's, profile.
/// No file in the project is picked up as a config: one there could run any
/// command with the user's secrets, so its `.molt/config.toml` holds only
/// data. `data_dir` replaces the configured data dir; the default setup
/// otherwise uses `project`'s, or [`default_data_dir`] without one, and has
/// memory keep the notes about the project under its id and the user's
/// notes for every project next to the registry ([`Registry::user_db`]).
pub fn resolve_config(
    path: Option<&Path>,
    workspace: &Path,
    data_dir: Option<&Path>,
    project: Option<&Project>,
    profile: Option<&str>,
) -> anyhow::Result<Config> {
    if let Some(path) = path {
        let mut cfg = Layered::file(path, profile)?.config;
        let trusted = !path.canonicalize()?.starts_with(workspace);
        if !trusted && !cfg.hooks.is_empty() {
            eprintln!("lifecycle hooks in a project configuration are ignored; keep trusted hook configuration outside the project");
            cfg.hooks.clear();
        }
        install_hooks(&mut cfg)?;
        require_services(&cfg).with_context(|| format!("{} cannot run tasks", path.display()))?;
        if let Some(dir) = data_dir {
            cfg.kernel.data_dir = dir.to_path_buf();
        }
        return Ok(cfg);
    }
    let data_dir = chosen_data_dir(workspace, data_dir, project)?;
    // Checked before the directory is created, and again once it exists.
    let data_dir = resolved(&data_dir).with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &data_dir)?;
    let mut cfg = layered(workspace, &data_dir, project, profile)?.config;
    let credential = |name: &&str| {
        let set = |value: Option<&String>| value.is_some_and(|key| !key.trim().is_empty());
        set(std::env::var(name).ok().as_ref()) || set(cfg.service("model").and_then(|m| m.env.get(*name)))
    };
    if !["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY"].iter().any(credential) {
        bail!(
            "ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY is not set: export your provider credential, or pass --config with services of your own"
        );
    }
    install_hooks(&mut cfg)?;
    require_services(&cfg).context("the user config leaves the agent unable to run tasks")?;
    crate::lock::create_private_dir(&data_dir)?;
    let canonical = data_dir.canonicalize().with_context(|| format!("data dir {}", data_dir.display()))?;
    outside(workspace, &canonical)?;
    anyhow::ensure!(canonical == data_dir, "the data dir {} moved while molt started", data_dir.display());
    Ok(cfg)
}

/// Hand the planner the config's lifecycle hooks, once they are checked.
fn install_hooks(cfg: &mut Config) -> anyhow::Result<()> {
    molt_planner::hooks::validate(&cfg.hooks)?;
    let hooks = serde_json::to_string(&cfg.hooks)?;
    if let Some(planner) = cfg.service_mut("planner") {
        planner.env.insert("MOLT_HOOKS_JSON".into(), hooks);
    }
    Ok(())
}

/// `data_dir`, else `project`'s, else the default one of `workspace`.
fn chosen_data_dir(workspace: &Path, data_dir: Option<&Path>, project: Option<&Project>) -> anyhow::Result<PathBuf> {
    match (data_dir, project) {
        (Some(dir), _) => Ok(dir.to_path_buf()),
        (None, Some(project)) => Ok(project.data_dir.clone()),
        (None, None) => default_data_dir(workspace),
    }
}

/// The default setup for `workspace` and `project` with its data in
/// `data_dir`, and the user's config, `profile`, the project's data config
/// and the environment over it (see [`crate::layers`]).
pub fn layered(
    workspace: &Path,
    data_dir: &Path,
    project: Option<&Project>,
    profile: Option<&str>,
) -> anyhow::Result<Layered> {
    let mut defaults = Config::agent(workspace, data_dir, &bin_dir()?)?;
    if let Some(project) = project {
        defaults.for_project(&project.id, &project_paths(project, workspace))?;
        defaults.with_user_db(&Registry::user()?.user_db())?;
    }
    Layered::build(&defaults, workspace, profile, &|name| std::env::var(name).ok())
}

/// What `molt config show` prints: what [`resolve_config`] would run with,
/// with where each value comes from. Nothing is created.
pub fn show_config(
    path: Option<&Path>,
    workspace: &Path,
    data_dir: Option<&Path>,
    project: Option<&Project>,
    profile: Option<&str>,
) -> anyhow::Result<Layered> {
    if let Some(path) = path {
        let mut layered = Layered::file(path, profile)?;
        if let Some(dir) = data_dir {
            layered.config.kernel.data_dir = dir.to_path_buf();
        }
        return Ok(layered);
    }
    let data_dir = chosen_data_dir(workspace, data_dir, project)?;
    let data_dir = resolved(&data_dir).with_context(|| format!("data dir {}", data_dir.display()))?;
    layered(workspace, &data_dir, project, profile)
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

/// Bounded DNS diagnostics for the standard web service, before any model work.
pub async fn web_warnings(cfg: &Config) -> anyhow::Result<Vec<String>> {
    let Some(exec) = cfg.service("web").and_then(|service| service.exec.as_ref()) else { return Ok(Vec::new()) };
    if Path::new(&exec.command).file_name().is_none_or(|name| name != "molt-web") {
        return Ok(Vec::new());
    }
    let mut hosts = Vec::new();
    let mut resolver = "system";
    let mut args = exec.args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--allow" => {
                if let Some(value) = args.next() {
                    hosts.push(value.clone());
                }
            }
            "--resolver" => {
                if let Some(value) = args.next() {
                    resolver = value;
                }
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--allow=") {
                    hosts.push(value.to_owned());
                }
                if let Some(value) = arg.strip_prefix("--resolver=") {
                    resolver = value;
                }
            }
        }
    }
    anyhow::ensure!(!hosts.is_empty(), "the molt-web service needs at least one --allow host");
    let allow = molt_web::Allow::parse(&hosts).map_err(anyhow::Error::msg)?;
    molt_web::preflight(&allow, resolver).await
}

/// Whether `exec` starts Molt's own shell service, which the sandbox options are for.
fn standard_shell(exec: &molt_proto::Exec) -> bool {
    Path::new(&exec.command).file_name().is_some_and(|name| name == "molt-tools")
}

/// Whether the shell service runs commands in the sandbox: `None` for a
/// shell service other than the standard one, which says nothing about it.
pub fn shell_sandboxed(cfg: &Config) -> Option<bool> {
    let exec = cfg.service("shell")?.exec.as_ref().filter(|exec| standard_shell(exec))?;
    Some(!exec.args.iter().any(|arg| arg == "--no-sandbox"))
}

/// Have the standard shell's commands build offline from this user's
/// dependency cache (`--deps-cache KIND`; `cargo` so far).
pub fn deps_cache(cfg: &mut Config, kinds: &[String]) -> anyhow::Result<()> {
    if kinds.is_empty() {
        return Ok(());
    }
    let shell = cfg.service_mut("shell").context("--deps-cache needs the shell service")?;
    let exec = shell.exec.as_mut().filter(|exec| standard_shell(exec));
    let exec = exec.context("--deps-cache requires the molt-tools shell service")?;
    for kind in kinds {
        exec.args.extend(["--deps-cache".into(), kind.clone()]);
    }
    // The shell looks for the cache where Cargo would.
    if !shell.pass_env.iter().any(|name| name == "CARGO_HOME") {
        shell.pass_env.push("CARGO_HOME".into());
    }
    Ok(())
}

/// Hand the config's environment profiles to the standard shell, which
/// applies them with the built-in ones.
pub fn environments(cfg: &mut Config) -> anyhow::Result<()> {
    if cfg.environments.is_empty() {
        return Ok(());
    }
    let json = serde_json::to_string(&cfg.environments)?;
    let shell = cfg.service_mut("shell").context("[environment] profiles need the shell service")?;
    let exec = shell.exec.as_mut().filter(|exec| standard_shell(exec));
    let exec = exec.context("[environment] profiles need the molt-tools shell service")?;
    if !exec.args.iter().any(|arg| arg == "--no-sandbox") {
        exec.args.extend(["--environments".into(), json]);
    }
    Ok(())
}

/// What the commands of a run in `workspace` will be missing, from cheap
/// file checks, with the fix for each: nothing when the shell is unconfined
/// or has network. `cargo_home` is the user's Cargo home, whose cache the
/// Rust profile builds from when it has one.
pub fn dependency_warnings(cfg: &Config, workspace: &Path, cargo_home: Option<&Path>) -> Vec<String> {
    let Some(exec) = cfg.service("shell").and_then(|shell| shell.exec.as_ref()).filter(|exec| standard_shell(exec))
    else {
        return Vec::new();
    };
    let mut cargo = false;
    let mut args = exec.args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-sandbox" => return Vec::new(),
            "--deps-cache" => cargo |= args.next().is_some_and(|kind| kind == "cargo"),
            // The user arranged a Cargo home of their own.
            "--sandbox-env" => cargo |= args.next().is_some_and(|name| name == "CARGO_HOME"),
            "--sandbox-policy" => {
                let policy = args.next().and_then(|path| std::fs::read(path).ok());
                if let Some(policy) = policy.and_then(|bytes| serde_json::from_slice::<SandboxPolicy>(&bytes).ok()) {
                    if policy.network {
                        return Vec::new();
                    }
                    cargo |= policy.cargo_cache.is_some() || policy.environment.iter().any(|name| name == "CARGO_HOME");
                }
            }
            _ => {}
        }
    }
    let profiles = molt_tools::env_profile::profiles(&cfg.environments);
    if profiles.get("rust").is_some_and(|rust| rust.cargo_cache) {
        cargo |= cargo_home.is_some_and(|home| home.join("registry").is_dir());
    }
    missing_dependencies(workspace, cargo)
}

/// The dependencies of the project in `workspace` that commands without
/// network cannot get, as one line each with what to do. `cargo` is whether
/// they can build from a Cargo cache.
pub fn missing_dependencies(workspace: &Path, cargo: bool) -> Vec<String> {
    let read = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();
    let dir = |name: &str| workspace.join(name).is_dir();
    let mut missing = Vec::new();
    let remote_crates = read("Cargo.lock").is_some_and(|lock| {
        lock.lines().any(|line| {
            ["registry+", "sparse+", "git+"].iter().any(|kind| line.trim().starts_with(&format!("source = \"{kind}")))
        })
    });
    let vendored = [".cargo/config.toml", ".cargo/config"]
        .iter()
        .any(|name| read(name).is_some_and(|c| c.contains("replace-with")));
    if remote_crates && !vendored && !cargo {
        missing.push(
            "Rust: Cargo.lock lists crates from a registry or git, which commands cannot download without network. \
             Run `cargo fetch` in the project on this machine: commands build offline from your Cargo cache \
             (`molt env doctor` checks it)."
                .to_owned(),
        );
    }
    if read("go.sum").is_some_and(|sum| !sum.trim().is_empty()) && !workspace.join("vendor/modules.txt").is_file() {
        missing.push(
            "Go: go.sum lists modules and there is no vendor directory, and commands cannot download them without \
             network. Run `go mod vendor`, mount your module cache with --sandbox-policy, or choose --no-sandbox to \
             run commands unconfined."
                .to_owned(),
        );
    }
    let node = read("package.json").and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let node_deps = node.is_some_and(|package| {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|key| package[key].as_object().is_some_and(|deps| !deps.is_empty()))
    });
    if node_deps && !dir("node_modules") {
        missing.push(
            "Node: package.json lists dependencies and there is no node_modules, and commands cannot install them \
             without network. Run `npm install` in the project first (commands see node_modules read-only), or \
             choose --no-sandbox to run commands unconfined."
                .to_owned(),
        );
    }
    let requirements = read("requirements.txt")
        .is_some_and(|text| text.lines().map(str::trim).any(|line| !line.is_empty() && !line.starts_with('#')));
    let pyproject = read("pyproject.toml").and_then(|text| text.parse::<toml::Table>().ok()).is_some_and(|table| {
        table
            .get("project")
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_array())
            .is_some_and(|d| !d.is_empty())
    });
    let venv = [".venv", "venv"].iter().any(|name| workspace.join(name).join("pyvenv.cfg").is_file());
    if (requirements || pyproject) && !venv {
        missing.push(
            "Python: the project lists packages and has no virtual environment (.venv), and commands cannot install \
             them without network, so only packages installed system-wide are there. Create .venv in the project \
             and install into it first, or choose --no-sandbox to run commands unconfined."
                .to_owned(),
        );
    }
    missing
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
            let data_dir = chosen_data_dir(workspace, data_dir, project)?;
            if !data_dir.is_dir() {
                bail!(
                    "molt has not run in {} yet (no data dir at {}); memory starts with the first `molt do`",
                    workspace.display(),
                    data_dir.display()
                );
            }
            let data_dir = data_dir.canonicalize().with_context(|| format!("data dir {}", data_dir.display()))?;
            layered(workspace, &data_dir, project, None)?.config
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
    /// (`MOLT_MEMORY_MODEL`, else the model the run's attempts used).
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

/// Why memory does not learn from a finished run, if it does not: the run
/// failed before any model call was answered (a wrong key or URL, say), so
/// there is nothing to learn from; or, since learning is a model call too
/// and the run's budget covers it, the budget is spent or what the run
/// spent is not all known.
fn skip_learning(resp: &RunResponse, budget_usd: Option<f64>) -> Option<String> {
    if resp.outcome == molt_api::planner::Outcome::Failed && resp.usage == molt_api::model::Usage::default() {
        return Some("skipped: the run failed before any model call was answered".to_owned());
    }
    if budget_usd.is_some_and(|b| resp.cost_usd >= b) {
        return Some("skipped: the budget is exhausted".to_owned());
    }
    match resp.uncounted_calls {
        0 => None,
        1 => Some("skipped: model settlement is incomplete (1 call with unknown cost)".to_owned()),
        n => Some(format!("skipped: model settlement is incomplete ({n} calls with unknown cost)")),
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
    // Learning is the workflow's last stage, and this side runs it. A
    // workflow defined in the planner's own configuration is not known
    // here: it learns as asked.
    let learns = |name: Option<&str>| match name {
        Some(molt_api::workflow::AUTO) => true,
        name => molt_api::workflow::builtin(name).is_none_or(|w| w.stages.learn.is_some()),
    };
    let learn = learn.filter(|_| learns(req.workflow.as_deref()));
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
            // A run that named `auto` learns as the workflow picked for it does.
            let learn = learn.filter(|_| ran.resp.workflow.is_none() || learns(ran.resp.workflow.as_deref()));
            let learned = match (learn, ran.memory) {
                (None, _) | (Some(_), None) => None,
                (Some(_), Some(Err(e))) => Some(Err(e)),
                (Some(_), Some(Ok(()))) if skip_learning(&ran.resp, budget_usd).is_some() => {
                    skip_learning(&ran.resp, budget_usd).map(Err)
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

/// One line about a stopped run: its id, when it stopped, what it spent,
/// its attempts and its task.
pub fn stopped_run(saved: &molt_planner::checkpoint::Saved) -> String {
    let ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| (now.as_millis() as u64).saturating_sub(saved.saved_ms) / 60_000);
    let attempts: Vec<String> = saved
        .attempts
        .iter()
        .map(|a| match &a.end {
            Some(end) => format!("{}: {:?} after {} turns", a.index + 1, end.status, a.turns).to_lowercase(),
            None => format!("{}: stopped after {} turns", a.index + 1, a.turns),
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
            let fork = name.to_str().map(|n| n.strip_suffix(".json").or(n.strip_suffix(".written")).unwrap_or(n));
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

/// The names in `scratch` that belong to forks: `fork-<id>`, `fork-<id>.json` and `fork-<id>.written`.
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
        environments: Default::default(),
        confirm: cfg.confirm.clone(),
        tui: cfg.tui.clone(),
        hooks: cfg.hooks.clone(),
        profiles: cfg.profiles.clone(),
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
    // Likewise a command's output comes from the shell, in the trace of the command.
    if matches!(event, Progress::Output { .. })
        && (msg.from.as_ref().map(ServiceId::as_str) != Some("shell") || msg.trace_id.as_str() != event.run())
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
        Progress::ModelRetry { attempt, role, call, failure, delay_ms, retry, retries, .. } => {
            let actor = speaker(*role, *attempt);
            let line = molt_api::progress::retry_line(failure, Duration::from_millis(*delay_ms), *retry, *retries);
            format!("{actor}, call {call}: {line}")
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
        Progress::CheckReady { command: None, rubric, .. } if !rubric.is_empty() => {
            let numbered: Vec<String> = rubric.iter().enumerate().map(|(i, c)| format!("{}. {c}", i + 1)).collect();
            format!("no command checks this task; a model judges the result against: {}", numbered.join(" "))
        }
        Progress::CheckReady { command: None, designed: true, .. } => {
            "no automated check fits this task; the result will be unverified".to_owned()
        }
        Progress::CheckReady { command: None, designed: false, .. } => {
            "running without a done-check, as asked; the result will be unverified".to_owned()
        }
        Progress::AttemptStarted { attempt, .. } => format!("{}: started", attempt_name(*attempt)),
        Progress::ToolCall { attempt, detail, .. } => format!("{}: {detail}", attempt_name(*attempt)),
        Progress::CheckRan { attempt, passed: true, judged: true, .. } => {
            format!("{}: the judge found every criterion met", attempt_name(*attempt))
        }
        Progress::CheckRan { attempt, passed: false, judged: true, tail, .. } => {
            format!("{}: judged: {tail}", attempt_name(*attempt))
        }
        Progress::CheckRan { attempt, passed: true, .. } => format!("{}: check passed", attempt_name(*attempt)),
        Progress::CheckRan { attempt, passed: false, exit_code: Some(code), .. } => {
            format!("{}: check failed (exit code {code})", attempt_name(*attempt))
        }
        Progress::CheckRan { attempt, passed: false, exit_code: None, .. } => {
            format!("{}: check failed (killed or timed out)", attempt_name(*attempt))
        }
        Progress::AttemptFinished { attempt, status, .. } => {
            format!("{}: {}", attempt_name(*attempt), status_name(*status))
        }
        Progress::Note { message, .. } => message.clone(),
        Progress::Routed { workflow, reason, .. } => routed_line(workflow, reason, "--workflow verify"),
        Progress::Required { requirement, .. } => requirement_line(requirement),
        Progress::Adopted { attempt, version, turn, .. } => {
            format!("{}: follows requirements v{version} from turn {turn}", attempt_name(*attempt))
        }
        Progress::State { state: RunState::Running, .. } => "going on".to_owned(),
        Progress::State { state: RunState::Paused, .. } => "paused".to_owned(),
        Progress::State { state: RunState::Pausing, waiting, .. } => {
            format!("pausing; waiting for {}", waiting.join(", "))
        }
        Progress::Asked { attempt, question, text, options, .. } => {
            let mut line = format!("{} asks (question {question}): {text}", attempt_name(*attempt));
            if !options.is_empty() {
                let numbered: Vec<String> =
                    options.iter().enumerate().map(|(i, o)| format!("{}. {o}", i + 1)).collect();
                line.push_str(&format!(" [{}]", numbered.join(" ")));
            }
            line
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
        Progress::Output { text, .. } => text.trim_end().to_owned(),
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
    match c.answers {
        Some(question) => {
            format!("requirements v{}: answer to question {question}: {}", requirement.version, parts.join("; "))
        }
        None => format!("requirements v{}: {}", requirement.version, parts.join("; ")),
    }
}

fn model_label(context: &molt_api::progress::ModelCall) -> String {
    let actor = speaker(context.role, context.attempt);
    format!("{actor}, turn {}, call {}", context.turn, context.call)
}

/// The result of a run as a few lines for the terminal, made [`printable`].
pub fn report(resp: &RunResponse) -> String {
    report_at(resp, None)
}

/// Report a result with the resolved output directory, including virtual projects.
/// What the planner picked for a run that named `auto`, and how to have
/// the full workflow instead (`full`, in the caller's words).
pub fn routed_line(workflow: &str, reason: &str, full: &str) -> String {
    let why = if reason.is_empty() { String::new() } else { format!(" ({reason})") };
    if workflow == molt_api::workflow::VERIFY {
        format!("handled as verify{why}")
    } else {
        format!("handled as {workflow}{why} · {full} for the full workflow")
    }
}

pub fn report_at(resp: &RunResponse, workspace: Option<&str>) -> String {
    let answer = resp.workflow.as_deref() == Some(molt_api::workflow::ASK) && resp.outcome == Outcome::Unverified;
    let mut lines = vec![match &resp.dispute {
        Some(_) => format!("outcome: {}, the check was disputed", outcome_name(resp.outcome)),
        None if answer => "outcome: unverified answer".to_owned(),
        None => format!("outcome: {}", outcome_name(resp.outcome)),
    }];
    if let Some(level) = resp.verification {
        lines.push(format!("verification: {level}"));
    }
    if let Some(check) = &resp.check {
        let by = if check.designed { " (designed by the planner)" } else { "" };
        lines.push(format!("check: {}{by}", check.command));
    }
    if !resp.rubric.is_empty() {
        lines.push("criteria (judged by a model, no command checked them):".to_owned());
        for (i, c) in resp.rubric.iter().enumerate() {
            match resp.judgement.iter().find(|f| f.criterion == c.text) {
                Some(f) => {
                    let mark = if f.met { "met" } else { "NOT met" };
                    lines.push(format!("  {}. [{mark}] {}", i + 1, c.text));
                    if !f.reason.trim().is_empty() {
                        lines.push(format!("     {}", f.reason.trim()));
                    }
                }
                None => lines.push(format!("  {}. {}", i + 1, c.text)),
            }
        }
    }
    if resp.changes.is_empty() {
        lines.push("changes: none".to_owned());
    } else {
        lines.push("changes:".to_owned());
        lines.extend(resp.changes.iter().map(|c| format!("  {:<9}{}", change_name(c.kind), c.path)));
    }
    if let Some(line) = resp.left_out.as_ref().and_then(left_out_line) {
        lines.push(line);
    }
    if let Some(review) = &resp.review {
        lines.push(format!("review: {}", verdict_name(review.verdict)));
        lines.extend(review.findings.iter().map(|f| format!("  - {f}")));
    }
    lines.extend(selection_lines(resp));
    if resp.applied {
        lines.push(format!("applied to {}", workspace.unwrap_or("the workspace")));
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

/// The build artifacts a result leaves out, in one line: how many, and the
/// first few. `None` when there were none.
pub fn left_out_line(left_out: &molt_api::fs::LeftOut) -> Option<String> {
    let count = left_out.count;
    if count == 0 {
        return None;
    }
    let what = if count == 1 { "build artifact" } else { "build artifacts" };
    let mut line = format!("left out {count} {what} the commands made: {}", left_out.paths.join(", "));
    let more = count.saturating_sub(left_out.paths.len() as u64);
    if more > 0 {
        line.push_str(&format!(", and {more} more"));
    }
    Some(line)
}

/// How the winner was picked among several passing attempts: one line per
/// candidate, best ranked first. Empty when there was no comparison.
pub fn selection_lines(resp: &RunResponse) -> Vec<String> {
    let Some(selection) = &resp.selection else { return Vec::new() };
    let mut lines = vec![format!("picked among {} passing attempts:", selection.candidates.len())];
    for c in &selection.candidates {
        let mark = if resp.winner == Some(c.attempt) { "*" } else { " " };
        lines.push(format!(" {mark} {}: {}", attempt_name(c.attempt), candidate_scores(c)));
    }
    lines
}

/// What a passing candidate was compared on, in a few words.
pub fn candidate_scores(c: &Candidate) -> String {
    let mut parts = Vec::new();
    if let Some(met) = c.criteria_met {
        parts.push(format!("{met} criteria met"));
    }
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
            if !r.skill_drafts.is_empty() {
                parts.push(format!("drafted {} project skills; use molt skill to evaluate", r.skill_drafts.len()));
            }
            let what = if parts.is_empty() {
                format!("learned nothing new ({})", r.skipped.as_deref().unwrap_or("nothing worth keeping"))
            } else {
                parts.join(", ")
            };
            let cost = match r.uncounted_calls {
                0 => format!("cost ${:.4}", r.cost_usd),
                n => format!("cost ${:.4} known, {n} calls without a known price", r.cost_usd),
            };
            let mut line = format!("memory: {what}; {cost} ({} tokens)", r.usage.total());
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
        Outcome::Judged => "passed (judged by a model)",
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
    use molt_api::model::{Role, Usage};
    use molt_api::planner::{CheckSpec, Constraint, Review};
    use serde_json::json;

    use super::*;

    #[test]
    fn the_preflight_names_each_kind_of_dependency_commands_cannot_get_and_how_to_fix_it() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        assert!(missing_dependencies(ws, false).is_empty(), "an empty project needs nothing");
        let lock = "[[package]]\nname = \"memchr\"\nversion = \"2.7.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        std::fs::write(ws.join("Cargo.lock"), lock).unwrap();
        std::fs::write(ws.join("go.sum"), "golang.org/x/text v0.3.0 h1:abc=\n").unwrap();
        std::fs::write(ws.join("package.json"), r#"{"dependencies": {"left-pad": "1"}}"#).unwrap();
        std::fs::write(ws.join("requirements.txt"), "# pinned\nrequests==2.0\n").unwrap();
        let missing = missing_dependencies(ws, false);
        assert_eq!(missing.len(), 4, "{missing:#?}");
        assert!(missing[0].starts_with("Rust:") && missing[0].contains("cargo fetch"), "{}", missing[0]);
        assert!(missing[1].starts_with("Go:") && missing[1].contains("go mod vendor"), "{}", missing[1]);
        assert!(missing[2].starts_with("Node:") && missing[2].contains("npm install"), "{}", missing[2]);
        assert!(missing[3].starts_with("Python:") && missing[3].contains(".venv"), "{}", missing[3]);
        // With the cache, or each kind's dependencies in the project, nothing is missing.
        std::fs::create_dir_all(ws.join("vendor")).unwrap();
        std::fs::write(ws.join("vendor/modules.txt"), "").unwrap();
        std::fs::create_dir(ws.join("node_modules")).unwrap();
        std::fs::create_dir(ws.join(".venv")).unwrap();
        std::fs::write(ws.join(".venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        assert!(missing_dependencies(ws, true).is_empty());
        assert_eq!(missing_dependencies(ws, false).len(), 1);
        // Path-only crates need nothing.
        std::fs::write(ws.join("Cargo.lock"), "[[package]]\nname = \"local\"\nversion = \"0.1.0\"\n").unwrap();
        assert!(missing_dependencies(ws, false).is_empty());
    }

    #[test]
    fn the_preflight_is_quiet_when_commands_are_unconfined_and_counts_the_cargo_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("project");
        std::fs::create_dir(&ws).unwrap();
        std::fs::write(ws.join("Cargo.lock"), "source = \"sparse+https://index.crates.io/\"\n").unwrap();
        let mut cfg = Config::agent(&ws, &tmp.path().join("data"), Path::new("/opt/molt/bin")).unwrap();
        let cargo_home = tmp.path().join("cargo");
        assert_eq!(dependency_warnings(&cfg, &ws, Some(&cargo_home)).len(), 1, "no Cargo cache there");
        let mut cached = cfg.clone();
        deps_cache(&mut cached, &["cargo".into()]).unwrap();
        assert!(dependency_warnings(&cached, &ws, None).is_empty());
        assert!(cached.service("shell").unwrap().pass_env.iter().any(|name| name == "CARGO_HOME"));
        // The Rust profile builds from the user's Cargo cache by default, unless the user's profile says not to.
        std::fs::create_dir_all(cargo_home.join("registry")).unwrap();
        assert!(dependency_warnings(&cfg, &ws, Some(&cargo_home)).is_empty());
        let mut own = cfg.clone();
        own.environments.insert("rust".into(), molt_tools::EnvProfile::default());
        assert_eq!(dependency_warnings(&own, &ws, Some(&cargo_home)).len(), 1);
        cfg.service_mut("shell").unwrap().exec.as_mut().unwrap().args.push("--no-sandbox".into());
        assert!(dependency_warnings(&cfg, &ws, None).is_empty());
    }

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
        let trusted = resolve_config(Some(&outside), &workspace, None, None, None).unwrap();
        assert_eq!(trusted.hooks.len(), 1);
        assert!(trusted.service("planner").unwrap().env["MOLT_HOOKS_JSON"].contains("before_apply"));
        let inside = workspace.join("config.toml");
        std::fs::write(&inside, config).unwrap();
        let ignored = resolve_config(Some(&inside), &workspace, None, None, None).unwrap();
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
    fn learning_whose_price_is_unknown_says_its_cost_is_a_subtotal() {
        let response = |uncounted_calls| ConsolidateResponse {
            skill_drafts: Vec::new(),
            added: Vec::new(),
            reinforced: Vec::new(),
            contradicted: Vec::new(),
            skipped: Some("the episode held nothing worth keeping".into()),
            proposed: Vec::new(),
            usage: Usage::default(),
            cost_usd: 0.01,
            uncounted_calls,
        };
        assert!(learned(&Ok(response(0))).contains("; cost $0.0100 ("));
        assert!(learned(&Ok(response(2))).contains("cost $0.0100 known, 2 calls without a known price"));
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
            std::fs::write(work.join(format!("{id}.written")), "\"src/lib.rs\"\n").unwrap();
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
            [
                ".molt",
                "fork-000000000001",
                "fork-000000000001.json",
                "fork-000000000003",
                "fork-000000000003.json",
                "fork-000000000003.written"
            ]
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
    fn a_question_and_its_answer_are_one_line_each() {
        let asked = Progress::Asked {
            run: "r".into(),
            attempt: 1,
            question: 2,
            text: "Tabs or spaces?".into(),
            options: vec!["tabs".into(), "spaces".into()],
        };
        assert_eq!(describe(&asked), "attempt 2 asks (question 2): Tabs or spaces? [1. tabs 2. spaces]");
        let constraint = Constraint { text: "tabs".into(), answers: Some(2), ..Default::default() };
        let answer = Requirement { version: 3, constraint, at_ms: 0 };
        assert_eq!(requirement_line(&answer), "requirements v3: answer to question 2: tabs");
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
        assert_eq!(describe(&call), "attempt 1: run echo \\u{1b}[2Khi");
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
                role: None,
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
                role: None,
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
            role: None,
            turn: 2,
            call: "call-a".into(),
            seq: 1,
        };
        let preview = describe(&Progress::ModelText { context: context.clone(), text: "\x1b[2Khi".into() });
        assert_eq!(preview, "attempt 2, turn 2, call call-a preview: \\u{1b}[2Khi");
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
                rubric: vec![],
            },
            Progress::CheckReady { run: run.clone(), command: None, files: vec![], designed: true, rubric: vec![] },
            Progress::CheckReady { run: run.clone(), command: None, files: vec![], designed: false, rubric: vec![] },
            Progress::ToolCall {
                run: run.clone(),
                attempt: 1,
                turn: 3,
                tool: "read_file".into(),
                detail: "read a.rs".into(),
            },
            Progress::CheckRan {
                run: run.clone(),
                attempt: 0,
                passed: false,
                exit_code: Some(1),
                tail: "boom".into(),
                judged: false,
            },
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
                "attempt 2: read a.rs",
                "attempt 1: check failed (exit code 1)",
                "attempt 2: cancelled",
            ]
        );
    }

    #[test]
    fn an_answer_is_reported_as_unverified_and_a_pick_says_how_to_have_the_full_workflow() {
        let resp: RunResponse = serde_json::from_value(serde_json::json!({
            "outcome": "unverified", "summary": "It parses JSON.", "changes": [], "patch": "", "applied": false,
            "attempts": [], "usage": {}, "cost_usd": 0.0, "verification": "none", "workflow": "ask"
        }))
        .unwrap();
        assert!(report(&resp).starts_with("outcome: unverified answer\nverification: none\n"), "{}", report(&resp));
        let quick = RunResponse { workflow: Some("quick".into()), ..resp };
        assert!(report(&quick).starts_with("outcome: unverified\n"));
        let event = Progress::Routed { run: "r".into(), workflow: "quick".into(), reason: "a comment".into() };
        assert_eq!(describe(&event), "handled as quick (a comment) · --workflow verify for the full workflow");
        assert_eq!(routed_line("verify", "", "/verify"), "handled as verify");
    }

    #[test]
    fn the_report_says_how_the_result_was_verified_when_the_planner_does() {
        let resp: RunResponse = serde_json::from_value(serde_json::json!({
            "outcome": "unverified", "summary": "", "changes": [], "patch": "", "applied": false,
            "attempts": [], "usage": {}, "cost_usd": 0.0, "verification": "none"
        }))
        .unwrap();
        assert!(report(&resp).starts_with("outcome: unverified\nverification: none\n"), "{}", report(&resp));
        let older = RunResponse { verification: None, ..resp };
        assert!(!report(&older).contains("verification"), "{}", report(&older));
    }

    #[test]
    fn learning_is_skipped_with_the_reason_that_applies() {
        let resp = |outcome, input_tokens, cost_usd, uncounted_calls| RunResponse {
            outcome,
            check: None,
            winner: None,
            summary: String::new(),
            changes: vec![],
            left_out: None,
            patch: String::new(),
            patch_truncated: false,
            applied: false,
            fork: None,
            review: None,
            attempts: vec![],
            usage: Usage { input_tokens, ..Usage::default() },
            cost_usd,
            uncounted_calls,
            checkpoint: None,
            requirements: vec![],
            selection: None,
            rubric: vec![],
            judgement: vec![],
            dispute: None,
            verification: None,
            workflow: None,
            conversation: Vec::new(),
        };
        // A rejected key or a wrong URL: every call refused, nothing to learn from.
        let refused = resp(Outcome::Failed, 0, 0.0, 0);
        assert_eq!(
            skip_learning(&refused, None).unwrap(),
            "skipped: the run failed before any model call was answered"
        );
        // A host that cannot be reached leaves the calls' cost unknown, but the run's failure is the reason.
        let unreachable = resp(Outcome::Failed, 0, 0.0, 1);
        assert!(skip_learning(&unreachable, None).unwrap().contains("the run failed"));
        let spent = resp(Outcome::Failed, 100, 0.5, 0);
        assert_eq!(skip_learning(&spent, Some(0.5)).unwrap(), "skipped: the budget is exhausted");
        let unsettled = resp(Outcome::Passed, 100, 0.1, 2);
        assert_eq!(
            skip_learning(&unsettled, Some(1.0)).unwrap(),
            "skipped: model settlement is incomplete (2 calls with unknown cost)"
        );
        assert_eq!(skip_learning(&resp(Outcome::Failed, 100, 0.1, 0), Some(1.0)), None, "a failed run is learned from");
        assert_eq!(skip_learning(&resp(Outcome::Passed, 100, 0.1, 0), None), None);
    }

    #[test]
    fn a_retry_is_one_line_naming_its_attempt() {
        let retry = Progress::ModelRetry {
            run: String::new(),
            attempt: Some(1),
            role: None,
            call: "c1".into(),
            failure: "model API 502".into(),
            delay_ms: 60_000,
            retry: 2,
            retries: 4,
        };
        assert_eq!(describe(&retry), "attempt 2, call c1: model API 502, retrying in 60s (2/4)");
        let reviewer = Progress::ModelRetry {
            run: String::new(),
            attempt: None,
            role: Some(Role::Reviewer),
            call: "c1".into(),
            failure: "model API 502".into(),
            delay_ms: 60_000,
            retry: 2,
            retries: 4,
        };
        assert_eq!(describe(&reviewer), "reviewer, call c1: model API 502, retrying in 60s (2/4)");
    }

    #[test]
    fn stream_lines_name_the_reviewer_and_the_judge_not_the_designer() {
        let context = |role, attempt| molt_api::progress::ModelCall {
            run: "run".into(),
            attempt,
            role,
            turn: 3,
            call: "msg_1".into(),
            seq: 1,
        };
        let text =
            |role, attempt| describe(&Progress::ModelText { context: context(role, attempt), text: "hi".into() });
        assert_eq!(text(Some(Role::Reviewer), None), "reviewer, turn 3, call msg_1 preview: hi");
        assert_eq!(text(Some(Role::Judge), None), "judge, turn 3, call msg_1 preview: hi");
        assert_eq!(text(Some(Role::Designer), None), "designer, turn 3, call msg_1 preview: hi");
        assert_eq!(text(Some(Role::Attempt), Some(0)), "attempt 1, turn 3, call msg_1 preview: hi");
        // From an older gateway that names no role.
        assert_eq!(text(None, Some(0)), "attempt 1, turn 3, call msg_1 preview: hi");
        assert_eq!(text(None, None), "planner, turn 3, call msg_1 preview: hi");
        let started = describe(&Progress::ModelStarted { context: context(Some(Role::Reviewer), None) });
        assert_eq!(started, "reviewer, turn 3, call msg_1: model started (cost pending)");
    }

    #[test]
    fn report_lines() {
        let mut resp = RunResponse {
            outcome: Outcome::Passed,
            check: Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: false, suite: None }),
            winner: Some(0),
            summary: "Fixed the parser.\n".into(),
            changes: vec![Change { path: "src/lib.rs".into(), kind: ChangeKind::Modified }],
            left_out: None,
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
            rubric: vec![],
            judgement: vec![],
            dispute: None,
            verification: None,
            workflow: None,
            conversation: Vec::new(),
        };
        assert_eq!(
            report(&resp),
            "outcome: passed\ncheck: cargo test\nchanges:\n  modified src/lib.rs\napplied to the workspace\n\
             known cost: $0.0120 (1200 tokens)\n\nFixed the parser.\n"
        );
        assert!(report_at(&resp, Some("/user-data/molt/projects/trip/files"))
            .contains("applied to /user-data/molt/projects/trip/files\n"));
        resp.left_out = Some(molt_api::fs::LeftOut {
            count: 7,
            paths: ["target-x/CACHEDIR.TAG", "target-x/debug/app"].map(String::from).to_vec(),
        });
        assert!(report(&resp).contains(
            "  modified src/lib.rs\nleft out 7 build artifacts the commands made: target-x/CACHEDIR.TAG, \
             target-x/debug/app, and 5 more\napplied"
        ));
        resp.left_out = None;
        resp.applied = false;
        resp.fork = Some("/cache/work/fork_1".into());
        resp.uncounted_calls = 2;
        assert!(report(&resp).contains("not applied; the result is in /cache/work/fork_1\n"));
        assert!(
            report(&resp).contains("known cost: $0.0120 (1200 tokens), 2 model calls have unknown or unsettled cost\n")
        );
        resp.uncounted_calls = 0;
        resp.review =
            Some(Review { verdict: Verdict::Reject, findings: vec!["src/lib.rs:3 returns 42.".into()], details: None });
        assert!(
            report(&resp).contains("review: reject\n  - src/lib.rs:3 returns 42.\nnot applied"),
            "{}",
            report(&resp)
        );
        resp.review = None;
        let candidate = |attempt, suite_passed, lines_changed, verdict| planner::Candidate {
            attempt,
            criteria_met: None,
            suite_passed,
            lines_changed,
            files_changed: 1,
            truncated: false,
            verdict,
        };
        resp.selection = Some(Box::new(planner::Selection {
            rule: "fewer lines".into(),
            candidates: vec![candidate(0, Some(true), 3, Some(Verdict::Approve)), candidate(1, Some(false), 1, None)],
        }));
        assert!(
            report(&resp).contains(
                "picked among 2 passing attempts:\n * attempt 1: suite passed, 3 lines in 1 files, review: approve\n   \
                 attempt 2: suite failed, 1 lines in 1 files\nnot applied"
            ),
            "{}",
            report(&resp)
        );
        resp.selection = None;
        resp.outcome = Outcome::Judged;
        resp.rubric = vec![
            planner::Criterion { text: "Cites sources.".into(), command: None },
            planner::Criterion { text: "Under 500 words.".into(), command: Some("wc".into()) },
        ];
        resp.judgement = vec![
            planner::Finding {
                criterion: "Cites sources.".into(),
                met: true,
                reason: "Each claim links.".into(),
                by_command: false,
            },
            planner::Finding {
                criterion: "Under 500 words.".into(),
                met: true,
                reason: String::new(),
                by_command: true,
            },
        ];
        let shown = report(&resp);
        assert!(shown.starts_with("outcome: passed (judged by a model)\n"), "{shown}");
        assert!(
            shown.contains(
                "criteria (judged by a model, no command checked them):\n  1. [met] Cites sources.\n     Each claim \
                 links.\n  2. [met] Under 500 words.\n"
            ),
            "{shown}"
        );
        resp.outcome = Outcome::Passed;
        resp.rubric.clear();
        resp.judgement.clear();
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
