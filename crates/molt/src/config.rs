//! `molt.toml`.
//!
//! ```toml
//! [kernel]
//! data_dir = ".molt"
//! transport = "unix"        # or "nats"
//! nats_url = "nats://127.0.0.1:4222"
//!
//! [[service]]
//! name = "echo"
//! tier = "mutable"
//! exec = { command = "molt-echo" }
//! provides = ["echo.*"]
//! requests = [{ target = "topic:builds", budget = { calls = 100 } }]
//! memory_mb = 512
//! env = { GREETING = "hi" }           # set for the process
//! pass_env = ["ECHO_PREFIX"]          # copied from molt's environment when set
//!
//! [[service.tools]]                   # a tool the planner's model may be offered (RFC 0002)
//! name = "echo_back"
//! description = "Repeat the input back."
//! target = "echo.echo"                # a method the service provides
//! read_only = true
//! input_schema = { type = "object", properties = { text = { type = "string" } } }
//!
//! [confirm]                           # when the interface's confirm mode stops
//! max_lines = 300
//! max_files = 10
//! ask_paths = [".github/", "Cargo.toml"]
//! always_ask = false
//!
//! [environment.python-venv]           # replaces the built-in profile of this name
//! detect = [".venv/pyvenv.cfg"]
//! env = { VIRTUAL_ENV = "$WORKSPACE/.venv", PATH = "$WORKSPACE/.venv/bin:$PATH" }
//! read_only = ["/opt/python3.12"]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use molt_proto::tool::{ProvidedTool, ToolDecl};
use molt_proto::{Budget, CapRequest, Exec, Manifest, ServiceId, Target, Tier};
use molt_transport::Secret;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    #[default]
    Unix,
    Nats,
}

fn default_data_dir() -> PathBuf {
    ".molt".into()
}

fn default_nats_url() -> String {
    "nats://127.0.0.1:4222".into()
}

fn yes() -> bool {
    true
}

fn five() -> u32 {
    5
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelSection {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub transport: TransportKind,
    #[serde(default = "default_nats_url")]
    pub nats_url: String,
    #[serde(default = "yes")]
    pub fsync: bool,
}

impl Default for KernelSection {
    fn default() -> Self {
        Self { data_dir: default_data_dir(), transport: TransportKind::Unix, nats_url: default_nats_url(), fsync: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSection {
    pub name: ServiceId,
    #[serde(default = "mutable")]
    pub tier: Tier,
    #[serde(default)]
    pub exec: Option<Exec>,
    #[serde(default)]
    pub provides: Vec<Target>,
    #[serde(default)]
    pub requests: Vec<CapRequest>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub cpu_secs: Option<u64>,
    #[serde(default = "five")]
    pub max_restarts: u32,
    /// Variables set for the process. The supervisor starts services with a
    /// scrubbed environment, so this and `pass_env` are how a service gets
    /// its settings and secrets.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Variables copied from the molt process's environment when it has them.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Tools the service offers the planner's model, each on a method it
    /// provides. Declaring one grants nothing: the planner offers it only
    /// when it holds a capability for the method (RFC 0002).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDecl>,
    /// How its process runs (see `docs/services.md`): `native`, as a
    /// trusted host process, or `sandbox`, as a candidate an agent may
    /// replace.
    #[serde(default, skip_serializing_if = "Isolation::is_native")]
    pub isolation: Isolation,
    /// Host paths a sandboxed service may read, mounted read-only at their
    /// own paths. Never the data dir or a directory holding it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<PathBuf>,
}

/// How a service's process runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// A host process with the user's files, network and rights, whatever
    /// its capabilities on the bus. Trusted code only: a gate can never
    /// promote a version of it.
    #[default]
    Native,
    /// A candidate in the Linux sandbox, with only its granted capabilities
    /// and private state. Never run any other way.
    Sandbox,
}

impl Isolation {
    fn is_native(&self) -> bool {
        *self == Isolation::Native
    }
}

fn mutable() -> Tier {
    Tier::Mutable
}

impl ServiceSection {
    /// A service with no capabilities, limits or environment.
    pub fn new(name: ServiceId, tier: Tier, exec: Exec, provides: Vec<Target>) -> Self {
        Self {
            name,
            tier,
            exec: Some(exec),
            provides,
            requests: Vec::new(),
            memory_mb: None,
            cpu_secs: None,
            max_restarts: five(),
            env: BTreeMap::new(),
            pass_env: Vec::new(),
            tools: Vec::new(),
            isolation: Isolation::Native,
            mounts: Vec::new(),
        }
    }

    /// What is wrong with a sandboxed service's settings, if anything: it
    /// needs a command, and its variables cannot name Molt's own settings,
    /// the model credentials or what the sandbox sets.
    pub(crate) fn validate_isolation(&self) -> anyhow::Result<()> {
        if self.isolation != Isolation::Sandbox {
            anyhow::ensure!(
                self.mounts.is_empty(),
                "service {}: mounts are for a sandboxed service; a native one reads what you can",
                self.name
            );
            return Ok(());
        }
        anyhow::ensure!(self.mounts.len() <= 32, "service {}: at most 32 mounts", self.name);
        if let Some(path) = self.mounts.iter().find(|p| !p.is_absolute()) {
            anyhow::bail!("service {}: the mount {} must be an absolute path", self.name, path.display());
        }
        anyhow::ensure!(self.exec.is_some(), "service {}: a sandboxed service needs exec", self.name);
        for name in self.env.keys().chain(&self.pass_env) {
            anyhow::ensure!(
                !["MOLT_", "ANTHROPIC_", "OPENAI_"].iter().any(|p| name.starts_with(p))
                    && !["HOME", "PATH", "TMPDIR"].contains(&name.as_str()),
                "service {}: a sandboxed service cannot be given {name}: Molt's settings, the model credentials \
                 and HOME, PATH and TMPDIR stay out of its sandbox",
                self.name
            );
        }
        Ok(())
    }

    pub fn manifest(&self) -> anyhow::Result<Manifest> {
        Ok(Manifest {
            name: self.name.clone(),
            tier: self.tier,
            parent: None,
            provides: self.provides.clone(),
            requests: self.requests.clone(),
            exec: self.exec.clone(),
            tools: self.tools.clone(),
        })
    }

    /// The extra environment to launch the service with: each `pass_env`
    /// variable the molt process has, then `env`, which wins.
    pub fn launch_env(&self) -> Vec<(String, String)> {
        self.launch_env_from(|name| std::env::var(name).ok())
    }

    pub(crate) fn launch_env_from(&self, var: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        let passed = self
            .pass_env
            .iter()
            .filter(|name| !self.env.contains_key(*name))
            .filter_map(|name| Some((name.clone(), var(name)?)));
        passed.chain(self.env.iter().map(|(k, v)| (k.clone(), v.clone()))).collect()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub kernel: KernelSection,
    #[serde(default, rename = "service")]
    pub services: Vec<ServiceSection>,
    /// Environment profiles of the user's, by name: each adds one or
    /// replaces the built-in profile of its name (see
    /// `molt_tools::env_profile`). They reach the standard shell service.
    #[serde(default, rename = "environment", skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, molt_tools::EnvProfile>,
    #[serde(default)]
    pub confirm: ConfirmSection,
    #[serde(default)]
    pub tui: TuiSection,
    #[serde(default)]
    pub hooks: Vec<molt_planner::hooks::Hook>,
    /// The enabled extension packs with command templates for the
    /// interface, by name and directory. Not a key of a file: only the
    /// user's layer sets it, from the packs they approved.
    #[serde(skip)]
    pub extensions: Vec<(String, PathBuf)>,
    /// Sets of `[tui]` defaults, one picked with `--profile NAME`.
    #[serde(default, rename = "profile", skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, TuiSection>,
    /// MCP servers by name, each bridged as service `mcp-NAME` (see
    /// [`Config::install_mcp`] and `docs/mcp.md`). Only trusted
    /// configuration has them: never a project's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, molt_mcp::McpServer>,
    #[serde(default, skip_serializing_if = "ModelsSection::is_empty")]
    pub models: ModelsSection,
    /// `[improve]`: the gate `molt improve` promotes a prompt through
    /// (`docs/improve.md`). Only the user's configuration sets it: a
    /// project's holds no such key, and the improver reaches none of it.
    #[serde(default, skip_serializing_if = "crate::improve::Gate::is_default")]
    pub improve: crate::improve::Gate,
    /// `[readiness]`: the project's build and required variables for the
    /// readiness check, and whether runs check first (`docs/sandbox.md`).
    #[serde(default, skip_serializing_if = "crate::readiness::ReadinessSection::is_default")]
    pub readiness: crate::readiness::ReadinessSection,
}

/// `[models]`: the user's endpoints, and which model each role's calls go
/// to (#169).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsSection {
    /// Role name to model, such as `reviewer = "haiku"`: the model for a
    /// role whose declaration names none. The planner and memory get it as
    /// `MOLT_ROLE_MODELS` (see `molt_api::model::role_models`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, String>,
    /// Endpoints by name, each with its protocol, base URL, the variable
    /// that holds its key and the models it serves; a call names one of
    /// them `NAME/MODEL`. The gateway gets them as `MOLT_ENDPOINTS`, and
    /// each `key_env`, which no other service may be given.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: BTreeMap<String, molt_gateway::provider::Endpoint>,
}

impl ModelsSection {
    fn is_empty(&self) -> bool {
        self.roles.is_empty() && self.endpoints.is_empty()
    }
}

/// Defaults for the terminal interface; `model`, `effort`, `attempts`,
/// `budget` and `check` are `molt do`'s too (`molt ask` takes the first
/// three but `attempts`), each where its flag is not given.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiSection {
    pub mode: Option<String>,
    pub attempts: Option<u32>,
    pub model: Option<String>,
    pub effort: Option<molt_api::model::Effort>,
    pub budget: Option<f64>,
    pub check: Option<String>,
    pub memory_model: Option<String>,
    pub stream: Option<bool>,
    pub notify: Option<String>,
    pub commit: Option<bool>,
}

impl TuiSection {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.mode.as_deref().is_none_or(|s| matches!(s, "plan" | "confirm" | "auto")),
            "tui.mode must be plan, confirm or auto"
        );
        anyhow::ensure!(
            self.notify.as_deref().is_none_or(|s| matches!(s, "on" | "all" | "off")),
            "tui.notify must be on, all or off"
        );
        anyhow::ensure!(self.attempts.is_none_or(|n| (1..=8).contains(&n)), "tui.attempts must be 1 to 8");
        anyhow::ensure!(self.budget.is_none_or(|b| b.is_finite() && b > 0.0), "tui.budget must be finite and positive");
        for value in [&self.model, &self.memory_model, &self.check].into_iter().flatten() {
            anyhow::ensure!(
                !value.trim().is_empty() && value.len() <= 16 * 1024,
                "tui model or check is empty or exceeds 16 KiB"
            );
        }
        Ok(())
    }
}

/// When the interface's confirm mode stops for the user. It goes on by
/// itself when nothing here calls for a look.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmSection {
    /// Stop before applying a result that adds and removes more lines than this.
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    /// Stop before applying a result that changes more files than this.
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// Stop before applying a result that changes one of these: a path, a
    /// directory ending in `/`, or a file name matched in any directory.
    #[serde(default = "default_ask_paths")]
    pub ask_paths: Vec<String>,
    /// Stop for every check and every result, as before these rules.
    #[serde(default)]
    pub always_ask: bool,
}

fn default_max_lines() -> usize {
    300
}

fn default_max_files() -> usize {
    10
}

fn default_ask_paths() -> Vec<String> {
    [".github/", "Cargo.toml", "Cargo.lock", "package.json", "go.mod", "migrations/"].map(str::to_owned).to_vec()
}

impl Default for ConfirmSection {
    fn default() -> Self {
        Self {
            max_lines: default_max_lines(),
            max_files: default_max_files(),
            ask_paths: default_ask_paths(),
            always_ask: false,
        }
    }
}

/// What the gateway reads from its environment (see `molt_gateway::Config::from_env`).
const GATEWAY_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "MOLT_PROVIDER",
    "MOLT_MODEL_ALIASES",
    "MOLT_OPENAI_MODELS",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "MOLT_MODEL",
    "MOLT_EFFORT",
    "MOLT_MAX_TOKENS",
    "MOLT_MODEL_TIMEOUT_S",
    "MOLT_MODEL_RETRIES",
    "MOLT_MODEL_CONCURRENCY",
    "MOLT_FALLBACKS",
    "MOLT_MODEL_PRICES",
    "MOLT_PRICES_FILE",
    "MOLT_MODELS_FILE",
    "MOLT_ENDPOINTS",
    "XDG_DATA_HOME",
];

/// What the planner reads from its environment (see `molt_planner::Config::from_env`).
const PLANNER_ENV: &[&str] = &[
    "MOLT_PLANNER_MODEL",
    "MOLT_MAX_TURNS",
    "MOLT_MAX_CHECK_ROUNDS",
    "MOLT_BUDGET_USD",
    "MOLT_EXPLORE",
    "MOLT_EXPLORE_MODEL",
    "MOLT_COMPACT",
    "MOLT_MAX_TOKENS",
    "MOLT_MODEL_TIMEOUT_S",
    "MOLT_CHECK_TIMEOUT_S",
    "MOLT_MAP_TOKENS",
    "MOLT_CONTEXT_TOKENS",
    "MOLT_INSTRUCTION_FILES",
    "MOLT_SYNTAX_CHECK",
    "MOLT_PROCESSES",
    "MOLT_GIT",
    "MOLT_ROLE_MODELS",
    "MOLT_SCHEDULE",
    "MOLT_ESCALATION_MODELS",
    "MOLT_MAX_ESCALATIONS",
];

/// What the memory service reads from its environment (see `molt_memory::Config::from_env`).
/// It reads the planner's and the gateway's model too: unless told
/// otherwise, it learns with the model the attempts use.
const MEMORY_ENV: &[&str] = &[
    "MOLT_MEMORY_MODEL",
    "MOLT_MEMORY_EFFORT",
    "MOLT_MEMORY_MAX_TOKENS",
    "MOLT_MEMORY_MODEL_TIMEOUT_S",
    "MOLT_PLANNER_MODEL",
    "MOLT_MODEL",
    "MOLT_ROLE_MODELS",
];

/// Refuse a key a tool declaration does not have: declarations ignore
/// unknown fields so that older readers keep working, but in the user's own
/// file a misspelt `read_only` must not go unnoticed.
fn unknown_tool_keys(doc: &toml::Value) -> anyhow::Result<()> {
    let services = doc.get("service").and_then(toml::Value::as_array).into_iter().flatten();
    for svc in services {
        let name = svc.get("name").and_then(toml::Value::as_str).unwrap_or("?");
        for tool in svc.get("tools").and_then(toml::Value::as_array).into_iter().flatten() {
            let Some(keys) = tool.as_table() else { continue };
            if let Some(key) = keys.keys().find(|k| !ToolDecl::FIELDS.contains(&k.as_str())) {
                anyhow::bail!(
                    "service {name}: a tool has the unknown key {key:?}; a tool takes {}",
                    ToolDecl::FIELDS.join(", ")
                );
            }
        }
    }
    Ok(())
}

/// Whether a variable of this name likely holds a credential.
pub(crate) fn secret_name(name: &str) -> bool {
    ["API_KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|marker| name.to_ascii_uppercase().contains(marker))
}

/// The web service's name.
pub const WEB: &str = "web";

/// The memory database in a data dir.
pub const MEMORY_DB: &str = "memory.db";
/// The directory in the data dir where the planner keeps run checkpoints.
pub const RUNS: &str = "runs";

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config = Self::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.route_roles(&|name| std::env::var(name).ok())?;
        config.route_endpoints(&|name| std::env::var(name).ok())?;
        Ok(config)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        unknown_tool_keys(&toml::from_str(text)?)?;
        let config: Self = toml::from_str(text)?;
        config.provided_tools()?;
        for svc in &config.services {
            svc.validate_isolation()?;
        }
        for (name, server) in &config.mcp {
            server.validate(name).map_err(anyhow::Error::msg)?;
        }
        config.tui.validate()?;
        config.improve.validate().context("in [improve]")?;
        config.readiness.validate()?;
        if !config.models.roles.is_empty() {
            molt_api::model::role_models(&serde_json::to_string(&config.models.roles)?)
                .map_err(|e| anyhow::anyhow!("[models.roles]: {e}"))?;
        }
        for (name, endpoint) in &config.models.endpoints {
            endpoint.check(name).map_err(|e| anyhow::anyhow!("[models.endpoints.{name}]: {e}"))?;
        }
        for (name, profile) in &config.profiles {
            profile.validate().with_context(|| format!("in profile {name}"))?;
        }
        Ok(config)
    }

    /// Every tool the services declare, for the planner, once each
    /// service's declarations are checked against what it provides and
    /// against each other's names.
    pub fn provided_tools(&self) -> anyhow::Result<Vec<ProvidedTool>> {
        let mut tools = Vec::new();
        for svc in &self.services {
            svc.manifest()?.validate_tools().map_err(anyhow::Error::msg)?;
            tools.extend(svc.tools.iter().map(|decl| ProvidedTool { service: svc.name.clone(), decl: decl.clone() }));
        }
        molt_planner::validate_tools(&tools)?;
        Ok(tools)
    }

    /// The default agent setup: the model gateway, file and shell services
    /// confined to `workspace` with forks in `<data_dir>/work`, memory with
    /// its database in `<data_dir>/memory.db`, and the planner. The
    /// executables are taken from `bin_dir`.
    ///
    /// Keep `data_dir` outside `workspace`: a fork inside the project would
    /// find the original's `Cargo.toml` and `.git` in a parent directory.
    pub fn agent(workspace: &Path, data_dir: &Path, bin_dir: &Path) -> anyhow::Result<Self> {
        let text = |p: &Path| p.to_str().map(str::to_owned).with_context(|| format!("{} is not UTF-8", p.display()));
        let bin =
            |name: &str| -> anyhow::Result<Exec> { Ok(Exec { command: text(&bin_dir.join(name))?, args: vec![] }) };
        let tools = |kind: &str| -> anyhow::Result<Exec> {
            let mut exec = bin("molt-tools")?;
            let scratch = text(&data_dir.join("work"))?;
            exec.args = vec![kind.into(), "--root".into(), text(workspace)?, "--scratch".into(), scratch];
            Ok(exec)
        };
        let service = |name: &str, tier: Tier, exec: Exec| -> anyhow::Result<ServiceSection> {
            Ok(ServiceSection::new(ServiceId::new(name)?, tier, exec, vec![format!("{name}.*").parse()?]))
        };
        let request = |target: &str, budget: Budget| -> anyhow::Result<CapRequest> {
            Ok(CapRequest { target: target.parse()?, budget })
        };
        let names = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();

        // The gateway is the only holder of the API key, so only a human may change it.
        let mut model = service("model", Tier::Protected, bin("molt-gateway")?)?;
        model.pass_env = names(GATEWAY_ENV);
        model.requests =
            vec![request(&format!("topic:publish:{}", molt_api::progress::TOPIC), Budget::new(0, 0, 10_000_000))?];
        let changed = format!("topic:publish:{}", molt_api::fs::CHANGED);
        let mut fs = service("fs", Tier::Mutable, tools("fs")?)?;
        fs.requests = vec![request(&changed, Budget::new(0, 0, 100_000_000))?];
        let mut shell = service("shell", Tier::Protected, tools("shell")?)?;
        // Where the environment profiles find rustup and the Cargo cache.
        shell.pass_env = names(&["CARGO_HOME", "RUSTUP_HOME"]);
        // The output of a command the interface runs (`!cmd`), as it comes.
        shell.requests =
            vec![request(&format!("topic:publish:{}", molt_api::progress::TOPIC), Budget::new(0, 0, 10_000_000))?];
        let mut memory_exec = bin("molt-memory")?;
        // The project model leaves out the data dir, with the forks in it, should it be inside the workspace.
        memory_exec.args = vec![
            "--root".into(),
            text(workspace)?,
            "--db".into(),
            text(&data_dir.join(MEMORY_DB))?,
            "--skip".into(),
            text(data_dir)?,
            "--forks".into(),
            text(&data_dir.join("work"))?,
        ];
        let mut memory = service("memory", Tier::Mutable, memory_exec)?;
        memory.pass_env = names(MEMORY_ENV);
        // Memory reads finished episodes from the audit log and has the model
        // learn from them; it follows the files fs changes to keep its model current.
        memory.requests = vec![
            request(molt_api::model::COMPLETE, Budget::new(1_000_000_000, 0, 1_000_000))?,
            request(molt_proto::audit::READ, Budget::new(0, 0, 10_000_000))?,
            request(&format!("topic:subscribe:{}", molt_api::fs::CHANGED), Budget::new(0, 0, 10))?,
            request(molt_api::fs::VIEW, Budget::new(0, 0, 100_000_000))?,
        ];
        let mut planner_exec = bin("molt-planner")?;
        // Checkpoints of runs, to resume one that stopped.
        planner_exec.args = vec!["--state".into(), text(&data_dir.join(RUNS))?];
        let mut planner = service("planner", Tier::Mutable, planner_exec)?;
        planner.pass_env = names(PLANNER_ENV);
        // A budget of zero calls refuses every call, and `ms` 0 leaves deadlines
        // to each request; the planner enforces the run's own limits.
        planner.requests = vec![
            request(molt_api::model::COMPLETE, Budget::new(4_000_000_000, 0, 1_000_000))?,
            request("fs.*", Budget::new(0, 0, 10_000_000))?,
            request(molt_api::shell::RUN, Budget::new(0, 0, 1_000_000))?,
            request(&format!("topic:publish:{}", molt_api::progress::TOPIC), Budget::new(0, 0, 10_000_000))?,
        ];
        // Background processes run as commands do; the planner offers them only with MOLT_PROCESSES=on.
        for method in [molt_api::shell::START, molt_api::shell::READ, molt_api::shell::WRITE, molt_api::shell::STOP] {
            planner.requests.push(request(method, Budget::new(0, 0, 1_000_000))?);
        }
        // What memory knows about the workspace; not remember, forget or consolidate.
        for method in [
            molt_api::memory::INDEX,
            molt_api::memory::MAP,
            molt_api::memory::RECALL,
            molt_api::memory::SYMBOLS,
            molt_api::memory::FORK_SYMBOLS,
        ] {
            planner.requests.push(request(method, Budget::new(0, 0, 10_000_000))?);
        }
        Ok(Self {
            kernel: KernelSection { data_dir: data_dir.to_path_buf(), ..KernelSection::default() },
            services: vec![model, fs, shell, memory, planner],
            environments: BTreeMap::new(),
            confirm: ConfirmSection::default(),
            tui: TuiSection::default(),
            hooks: Vec::new(),
            extensions: Vec::new(),
            profiles: BTreeMap::new(),
            mcp: BTreeMap::new(),
            models: ModelsSection::default(),
            improve: Default::default(),
            readiness: Default::default(),
        })
    }

    /// Add a bridge service, `mcp-NAME`, for each MCP server of `[mcp]`,
    /// with its executable in `bin_dir`, and let the planner call each one
    /// as many times as its `calls` say. A server runs in the sandbox the
    /// standard shell's commands get (with the shell's `--sandbox-policy`,
    /// if any, and the network only when the server's `network` says so),
    /// or unconfined when the shell is `--no-sandbox`. Call it once the
    /// shell's sandbox options are set.
    pub fn install_mcp(&mut self, bin_dir: &Path) -> anyhow::Result<()> {
        if self.mcp.is_empty() {
            return Ok(());
        }
        let sandbox = self.mcp_sandbox()?;
        let command = bin_dir.join("molt-mcp").to_str().context("the molt-mcp path is not UTF-8")?.to_owned();
        let servers: Vec<(String, molt_mcp::McpServer)> =
            self.mcp.iter().map(|(name, server)| (name.clone(), server.clone())).collect();
        for (name, server) in servers {
            server.validate(&name).map_err(anyhow::Error::msg)?;
            let id = molt_mcp::service_name(&name);
            anyhow::ensure!(
                self.service(&id).is_none(),
                "the config has both a service {id} and [mcp.{name}], which is bridged as service {id}: rename one"
            );
            // The bridge gets the variables' names; their values reach it as the service's environment.
            let named = molt_mcp::McpServer { env: BTreeMap::new(), pass_env: server.variables(), ..server.clone() };
            let cache = self.kernel.data_dir.join("mcp").join(format!("{name}.json"));
            let cache = cache.to_str().context("the data dir path is not UTF-8")?.to_owned();
            let mut args = vec![
                "serve".to_owned(),
                "--name".into(),
                name.clone(),
                "--server".into(),
                serde_json::to_string(&named)?,
                "--cache".into(),
                cache,
            ];
            match &sandbox {
                Some(policy) => args.extend(["--policy".into(), serde_json::to_string(policy)?]),
                None => args.push("--no-sandbox".into()),
            }
            let mut bridge = ServiceSection::new(
                ServiceId::new(&id)?,
                Tier::Protected,
                Exec { command: command.clone(), args },
                vec![format!("{id}.*").parse()?],
            );
            bridge.env = server.env.clone();
            bridge.pass_env = server.pass_env.clone();
            self.services.push(bridge);
            if server.calls > 0 {
                let target: Target = molt_mcp::target(&name).parse()?;
                if let Some(planner) =
                    self.service_mut("planner").filter(|p| !p.requests.iter().any(|r| r.target == target))
                {
                    planner.requests.push(CapRequest { target, budget: Budget::new(0, 0, server.calls) });
                }
            }
        }
        Ok(())
    }

    /// The sandbox MCP servers run in: the standard shell's, or none when
    /// its commands run unconfined.
    fn mcp_sandbox(&self) -> anyhow::Result<Option<molt_tools::SandboxPolicy>> {
        let Some(exec) = self
            .service("shell")
            .and_then(|s| s.exec.as_ref())
            .filter(|e| Path::new(&e.command).file_name().is_some_and(|n| n == "molt-tools"))
        else {
            return Ok(Some(molt_tools::SandboxPolicy::default()));
        };
        let arg = |flag: &str| exec.args.windows(2).find(|w| w[0] == flag).map(|w| PathBuf::from(&w[1]));
        if exec.args.iter().any(|a| a == "--no-sandbox") {
            return Ok(None);
        }
        let Some(path) = arg("--sandbox-policy") else { return Ok(Some(molt_tools::SandboxPolicy::default())) };
        let roots = molt_tools::Roots {
            root: arg("--root").context("the shell service has no --root")?,
            scratch: arg("--scratch").context("the shell service has no --scratch")?,
        };
        let mut policy = molt_tools::SandboxPolicy::load(&path, &roots)?;
        // What the policy passes is for commands; a server gets only its own variables.
        policy.environment.clear();
        Ok(Some(policy))
    }

    /// Have the memory service keep the notes about `paths` under the
    /// project id `id` (see `molt_memory::Project`).
    pub fn for_project(&mut self, id: &str, paths: &[PathBuf]) -> anyhow::Result<()> {
        let Some(exec) = self.service_mut("memory").and_then(|s| s.exec.as_mut()) else { return Ok(()) };
        if paths.is_empty() {
            return Ok(());
        }
        exec.args.extend(["--project".into(), id.to_owned()]);
        for path in paths {
            let path = path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))?;
            exec.args.extend(["--project-path".into(), path.to_owned()]);
        }
        Ok(())
    }

    /// Have the memory service keep the user's notes for every project in
    /// the database at `path`, shared by all of them.
    pub fn with_user_db(&mut self, path: &Path) -> anyhow::Result<()> {
        let Some(exec) = self.service_mut("memory").and_then(|s| s.exec.as_mut()) else { return Ok(()) };
        let path = path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))?;
        exec.args.extend(["--user-db".into(), path.to_owned()]);
        Ok(())
    }

    /// Add the web service, reading pages from `hosts` (see
    /// `molt_web::Allow`), with its executable in `bin_dir`, and let the
    /// planner call it. The service is protected: what the agent may read
    /// from the network is the user's choice. Nothing changes when `hosts`
    /// is empty.
    pub fn allow_web(&mut self, hosts: &[String], bin_dir: &Path) -> anyhow::Result<()> {
        if hosts.is_empty() {
            return Ok(());
        }
        molt_web::Allow::parse(hosts).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            self.service(WEB).is_none(),
            "this config already has a web service: list the hosts in its --allow arguments instead"
        );
        let command = bin_dir.join("molt-web").to_str().context("the molt-web path is not UTF-8")?.to_owned();
        let args = hosts.iter().flat_map(|h| ["--allow".to_owned(), h.clone()]).collect();
        let web = ServiceSection::new(
            ServiceId::new(WEB)?,
            Tier::Protected,
            Exec { command, args },
            vec![format!("{WEB}.*").parse()?],
        );
        let planner = self.service_mut("planner").context("there is no planner to read the web for")?;
        planner
            .requests
            .push(CapRequest { target: molt_api::web::FETCH.parse()?, budget: Budget::new(0, 0, 1_000_000) });
        self.services.push(web);
        Ok(())
    }

    /// An explicit user choice; attempts cannot select a resolver.
    pub fn web_resolver(&mut self, spec: &str) -> anyhow::Result<()> {
        molt_web::validate_resolver(spec).map_err(anyhow::Error::msg)?;
        let web = self.service_mut(WEB).context("--web-resolver requires --allow-web or a configured web service")?;
        let exec = web
            .exec
            .as_mut()
            .filter(|exec| Path::new(&exec.command).file_name().is_some_and(|name| name == "molt-web"))
            .context("--web-resolver requires the molt-web service")?;
        let mut kept = Vec::new();
        let mut skip = false;
        for arg in &exec.args {
            if skip {
                skip = false;
                continue;
            }
            if arg == "--resolver" {
                skip = true;
            } else if !arg.starts_with("--resolver=") {
                kept.push(arg.clone());
            }
        }
        kept.extend(["--resolver".into(), spec.into()]);
        exec.args = kept;
        Ok(())
    }

    pub fn service(&self, name: &str) -> Option<&ServiceSection> {
        self.services.iter().find(|s| s.name.as_str() == name)
    }

    pub fn service_mut(&mut self, name: &str) -> Option<&mut ServiceSection> {
        self.services.iter_mut().find(|s| s.name.as_str() == name)
    }

    /// Give the planner and memory `[models.roles]` as `MOLT_ROLE_MODELS`,
    /// unless the environment (`var`) or the service's own `env` sets it.
    pub fn route_roles(&mut self, var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        let env = molt_api::model::ROLE_MODELS_ENV;
        if self.models.roles.is_empty() || var(env).is_some_and(|v| !v.trim().is_empty()) {
            return Ok(());
        }
        let routed = serde_json::to_string(&self.models.roles)?;
        for name in ["planner", "memory"] {
            if let Some(service) = self.service_mut(name) {
                service.env.entry(env.into()).or_insert_with(|| routed.clone());
            }
        }
        Ok(())
    }

    /// The named endpoints the gateway is started with: those of its own
    /// `env`, else of the environment (`var`), else `[models.endpoints]`.
    fn endpoints(
        &self,
        var: &dyn Fn(&str) -> Option<String>,
    ) -> anyhow::Result<BTreeMap<String, molt_gateway::provider::Endpoint>> {
        let env = molt_gateway::provider::ENDPOINTS_VAR;
        let set = self
            .service("model")
            .and_then(|m| m.env.get(env).cloned())
            .or_else(|| var(env))
            .filter(|v| !v.trim().is_empty());
        match set {
            Some(text) => molt_gateway::provider::parse_endpoints(&text).map_err(anyhow::Error::msg),
            None => Ok(self.models.endpoints.clone()),
        }
    }

    /// The variables that hold the named endpoints' keys.
    pub fn endpoint_keys(&self, var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Vec<String>> {
        let mut keys: Vec<String> = self.endpoints(var)?.into_values().map(|e| e.key_env).collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// Give the gateway `[models.endpoints]` as `MOLT_ENDPOINTS`, unless the
    /// environment (`var`) or its own `env` sets it, and each endpoint's
    /// `key_env` to pass; then [`Config::check_endpoint_keys`].
    pub fn route_endpoints(&mut self, var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        let env = molt_gateway::provider::ENDPOINTS_VAR;
        let keys = self.endpoint_keys(var)?;
        let routed = (!self.models.endpoints.is_empty() && var(env).is_none_or(|v| v.trim().is_empty()))
            .then(|| serde_json::to_string(&self.models.endpoints))
            .transpose()?;
        let Some(model) = self.service_mut("model") else { return Ok(()) };
        if let Some(routed) = routed {
            model.env.entry(env.into()).or_insert(routed);
        }
        for key in keys {
            if !model.pass_env.contains(&key) {
                model.pass_env.push(key);
            }
        }
        self.check_endpoint_keys(var)
    }

    /// Refuse to give a named endpoint's key to any service but the
    /// gateway: it is the only one that holds keys.
    pub fn check_endpoint_keys(&self, var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        let keys = self.endpoint_keys(var)?;
        for svc in self.services.iter().filter(|s| s.name.as_str() != "model") {
            if let Some(key) = keys.iter().find(|k| svc.env.contains_key(*k) || svc.pass_env.contains(k)) {
                anyhow::bail!(
                    "service {} is given {key}, the key of a named endpoint: only the model gateway may hold it",
                    svc.name
                );
            }
        }
        for (name, server) in &self.mcp {
            if let Some(key) = keys.iter().find(|k| server.env.contains_key(*k) || server.pass_env.contains(k)) {
                anyhow::bail!(
                    "MCP server {name} is given {key}, the key of a named endpoint: only the model gateway may hold it"
                );
            }
        }
        Ok(())
    }

    pub fn socket_dir(&self) -> PathBuf {
        self.kernel.data_dir.join("sock")
    }

    pub fn secrets_path(&self) -> PathBuf {
        self.kernel.data_dir.join("secrets.json")
    }
}

/// NATS credentials, generated once by `molt nats-config`.
#[derive(Clone, Debug)]
pub struct Secrets {
    pub kernel: Secret,
    pub services: BTreeMap<String, Secret>,
}

#[derive(Serialize, Deserialize)]
struct SecretsFile {
    kernel: String,
    services: BTreeMap<String, String>,
}

impl Secrets {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let f: SecretsFile = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(Self {
            kernel: Secret::from_raw(f.kernel),
            services: f.services.into_iter().map(|(k, v)| (k, Secret::from_raw(v))).collect(),
        })
    }

    /// Load existing secrets and add fresh ones for any new service, and for
    /// the [`CLI`](crate::agent::CLI) identity `molt do` joins the bus with.
    pub fn load_or_create(path: &Path, services: &[ServiceId]) -> anyhow::Result<Self> {
        let mut s = Self::load(path).unwrap_or_else(|_| Self { kernel: Secret::random(), services: BTreeMap::new() });
        for id in services.iter().map(ServiceId::as_str).chain([crate::agent::CLI]) {
            s.services.entry(id.to_owned()).or_insert_with(Secret::random);
        }
        let file = SecretsFile {
            kernel: s.kernel.expose().to_owned(),
            services: s.services.iter().map(|(k, v)| (k.clone(), v.expose().to_owned())).collect(),
        };
        if let Some(dir) = path.parent() {
            crate::lock::create_private_dir(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&file)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_web_resolver_does_not_change_hosts_or_capabilities() {
        let mut cfg = Config::agent(Path::new("/project"), Path::new("/data"), Path::new("/bin")).unwrap();
        assert!(cfg.web_resolver("doh:https://1.1.1.1/dns-query").is_err());
        cfg.allow_web(&["docs.rs".into()], Path::new("/bin")).unwrap();
        let requests = serde_json::to_value(&cfg.service("planner").unwrap().requests).unwrap();
        cfg.web_resolver("doh:https://1.1.1.1/dns-query").unwrap();
        cfg.web_resolver("system").unwrap();
        let args = &cfg.service("web").unwrap().exec.as_ref().unwrap().args;
        assert_eq!(args, &["--allow", "docs.rs", "--resolver", "system"]);
        assert_eq!(serde_json::to_value(&cfg.service("planner").unwrap().requests).unwrap(), requests);
        assert!(cfg.web_resolver("doh:https://127.0.0.1/dns-query").is_err());
    }

    #[test]
    fn interface_defaults_are_optional_and_validate_before_startup() {
        assert!(Config::parse("").unwrap().tui.mode.is_none());
        let config =
            Config::parse("[tui]\nmode='confirm'\nattempts=4\ncheck='make test'\nbudget=2.5\neffort='high'").unwrap();
        assert_eq!(config.tui.attempts, Some(4));
        for field in [
            "mode='unknown'",
            "attempts=0",
            "attempts=9",
            "budget=-1.0",
            "budget=nan",
            "check=''",
            "model=''",
            "notify='unknown'",
            "unknown=1",
        ] {
            assert!(Config::parse(&format!("[tui]\n{field}")).is_err(), "{field}");
        }
    }

    #[test]
    fn a_service_declares_tools_on_methods_it_provides_and_they_reach_its_manifest() {
        let service = |tool: &str| {
            format!(
                "[[service]]\nname = \"lint\"\nexec = {{ command = \"molt-lint\" }}\nprovides = [\"lint.*\"]\n\
                 [[service.tools]]\nname = \"lint\"\ndescription = \"Lint a file.\"\n\
                 input_schema = {{ type = \"object\", properties = {{ path = {{ type = \"string\" }} }} }}\n{tool}"
            )
        };
        let cfg =
            Config::parse(&service("target = \"lint.check\"\nread_only = true\nroles = [\"attempt\", \"reviewer\"]\n"))
                .unwrap();
        let manifest = cfg.service("lint").unwrap().manifest().unwrap();
        assert_eq!(manifest.tools.len(), 1);
        assert!(manifest.tools[0].read_only);
        let provided = cfg.provided_tools().unwrap();
        assert_eq!(
            (provided[0].service.as_str(), provided[0].decl.target.to_string().as_str()),
            ("lint", "lint.check")
        );
        // A manifest without tools serializes as it did before they existed, so its version id is unchanged.
        let mut plain = cfg.service("lint").unwrap().clone();
        plain.tools.clear();
        assert!(!serde_json::to_value(plain.manifest().unwrap()).unwrap().as_object().unwrap().contains_key("tools"));

        for (tool, says) in [
            ("target = \"fs.write\"\n", "does not provide"),
            ("target = \"lint.check\"\nreadonly = true\n", "unknown key \"readonly\""),
            ("target = \"lint.check\"\nroles = [\"reviewer\"]\n", "only reads"),
            ("name = \"x\"\n", "duplicate key"),
        ] {
            let err = format!("{:#}", Config::parse(&service(tool)).unwrap_err());
            assert!(err.contains(says), "{tool}: {err}");
        }
        let taken = service("target = \"lint.check\"\n")
            .replace("name = \"lint\"\ndescription", "name = \"read_file\"\ndescription");
        assert!(format!("{:#}", Config::parse(&taken).unwrap_err()).contains("planner's own tools"));
    }

    #[test]
    fn each_mcp_server_is_a_protected_bridge_service_the_planner_may_call_as_often_as_it_says() {
        let bin = Path::new("/opt/molt/bin");
        let mut cfg = Config::agent(Path::new("/w"), Path::new("/d"), bin).unwrap();
        let servers = Config::parse(
            "[mcp.issues]\ncommand = \"issues-mcp\"\nargs = [\"--stdio\"]\nnetwork = true\ncalls = 50\n\
             env = { TRACKER_TOKEN = \"tok\" }\npass_env = [\"TRACKER_URL\"]\n\
             [mcp.quiet]\ncommand = \"/opt/quiet\"\ncalls = 0\n",
        )
        .unwrap();
        cfg.mcp = servers.mcp;
        cfg.install_mcp(bin).unwrap();
        let bridge = cfg.service("mcp-issues").unwrap();
        assert_eq!(bridge.tier, Tier::Protected);
        assert_eq!(bridge.provides, ["mcp-issues.*".parse::<Target>().unwrap()]);
        assert!(bridge.requests.is_empty(), "it holds no capability");
        assert_eq!(bridge.env["TRACKER_TOKEN"], "tok");
        assert_eq!(bridge.pass_env, ["TRACKER_URL"]);
        let exec = bridge.exec.as_ref().unwrap();
        assert_eq!(exec.command, "/opt/molt/bin/molt-mcp");
        assert_eq!(&exec.args[..3], ["serve", "--name", "issues"]);
        assert!(!exec.args.iter().any(|a| a.contains("tok")), "variable values stay out of the manifest");
        assert!(exec.args.iter().any(|a| a == "--policy"), "it runs in the shell's sandbox");
        assert!(exec.args.iter().any(|a| a == "/d/mcp/issues.json"));
        let planner = cfg.service("planner").unwrap();
        let grant = planner.requests.iter().find(|r| r.target.to_string() == "mcp-issues.call").unwrap();
        assert_eq!(grant.budget.calls, 50);
        assert!(!planner.requests.iter().any(|r| r.target.to_string() == "mcp-quiet.call"), "calls = 0 grants none");

        let mut unconfined = Config::agent(Path::new("/w"), Path::new("/d"), bin).unwrap();
        unconfined.service_mut("shell").unwrap().exec.as_mut().unwrap().args.push("--no-sandbox".into());
        unconfined.mcp = cfg.mcp.clone();
        unconfined.install_mcp(bin).unwrap();
        let args = &unconfined.service("mcp-issues").unwrap().exec.as_ref().unwrap().args;
        assert!(args.iter().any(|a| a == "--no-sandbox") && !args.iter().any(|a| a == "--policy"));
        let err = unconfined.install_mcp(bin).unwrap_err().to_string();
        assert!(err.contains("rename one"), "{err}");
    }

    #[test]
    fn parses_the_documented_example() {
        let cfg = Config::parse(
            r#"
            [kernel]
            data_dir = ".molt"
            transport = "nats"

            [[service]]
            name = "echo"
            exec = { command = "molt-echo" }
            requests = [{ target = "topic:builds", budget = { calls = 100 } }]
            memory_mb = 512
            "#,
        )
        .unwrap();
        assert_eq!(cfg.kernel.transport, TransportKind::Nats);
        let m = cfg.services[0].manifest().unwrap();
        assert_eq!(m.tier, Tier::Mutable);
        assert_eq!(m.requests[0].budget.calls, 100);
        assert!(cfg.services[0].env.is_empty() && cfg.services[0].pass_env.is_empty());
    }

    #[test]
    fn the_confirm_rules_have_defaults_and_take_what_is_set() {
        assert_eq!(Config::parse("").unwrap().confirm, ConfirmSection::default());
        let cfg = Config::parse("[confirm]\nmax_lines = 50\nask_paths = [\"db/\"]\n").unwrap();
        assert_eq!((cfg.confirm.max_lines, cfg.confirm.max_files), (50, 10));
        assert_eq!(cfg.confirm.ask_paths, ["db/"]);
        assert!(Config::parse("[confirm]\nmax_line = 50\n").is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("[kernel]\ntransprot = \"unix\"\n").is_err());
    }

    #[test]
    fn the_example_file_parses_and_matches_the_agent_setup() {
        let text = include_str!("../../../molt.example.toml");
        let cfg = Config::parse(text).unwrap();
        let agent = Config::agent(Path::new("/w"), Path::new("/d"), Path::new("/bin")).unwrap();
        for want in &agent.services {
            let got = cfg.service(want.name.as_str()).unwrap_or_else(|| panic!("no {} service", want.name));
            assert_eq!(got.tier, want.tier, "{}", want.name);
            assert_eq!(got.provides, want.provides, "{}", want.name);
            assert_eq!(got.requests, want.requests, "{}", want.name);
            assert_eq!(got.pass_env, want.pass_env, "{}", want.name);
        }
        assert_eq!(cfg.service("echo").unwrap().isolation, Isolation::Sandbox);
        assert!(cfg.services.iter().filter(|s| s.name.as_str() != "echo").all(|s| s.isolation == Isolation::Native));
    }

    #[test]
    fn the_examples_commented_endpoint_parses_once_uncommented() {
        let text = include_str!("../../../molt.example.toml");
        let start = text.find("# [models.endpoints.glm]").unwrap();
        let block = text[start..].split("\n\n").next().unwrap();
        let uncommented: String =
            block.lines().map(|l| l.trim_start_matches('#').trim_start()).collect::<Vec<_>>().join("\n");
        let cfg = Config::parse(&uncommented).unwrap();
        let glm = &cfg.models.endpoints["glm"];
        assert_eq!(glm.key_env, "GLM_API_KEY");
        assert!(!glm.models["glm-4.6"].output_schema);
    }

    #[test]
    fn the_examples_commented_readiness_parses_once_uncommented_and_credentials_cannot_be_required() {
        let text = include_str!("../../../molt.example.toml");
        let start = text.find("# [readiness]").unwrap();
        let block = text[start..].split("\n\n").next().unwrap();
        let uncommented: String =
            block.lines().map(|l| l.trim_start_matches('#').trim_start()).collect::<Vec<_>>().join("\n");
        let cfg = Config::parse(&uncommented).unwrap();
        assert_eq!(cfg.readiness.build.as_deref(), Some("cargo build --offline"));
        assert!(cfg.readiness.before_run && cfg.readiness.env == ["DATABASE_URL"]);
        let err = Config::parse("[readiness]\nenv = [\"OPENAI_API_KEY\"]\n").unwrap_err();
        assert!(format!("{err:#}").contains("never given"), "{err:#}");
    }

    #[test]
    fn a_sandboxed_service_needs_a_command_and_gets_none_of_molts_settings_or_credentials() {
        let service = |extra: &str| format!("[[service]]\nname = \"c\"\nisolation = \"sandbox\"\n{extra}");
        let exec = "exec = { command = \"/usr/bin/c\" }\n";
        assert!(Config::parse(&service(exec)).is_ok());
        let err = Config::parse(&service("")).unwrap_err().to_string();
        assert!(err.contains("needs exec"), "{err}");
        for extra in [
            "pass_env = [\"ANTHROPIC_API_KEY\"]",
            "pass_env = [\"OPENAI_API_KEY\"]",
            "env = { MOLT_SECRET = \"x\" }",
            "env = { HOME = \"/root\" }",
        ] {
            let err = Config::parse(&service(&format!("{exec}{extra}"))).unwrap_err().to_string();
            assert!(err.contains("cannot be given"), "{extra}: {err}");
        }
        // A native service may still be given them: it is trusted.
        assert!(Config::parse("[[service]]\nname = \"c\"\npass_env = [\"ANTHROPIC_API_KEY\"]\n").is_ok());
        assert!(Config::parse("[[service]]\nname = \"c\"\nisolation = \"wasm\"\n").is_err());
    }

    #[test]
    fn only_a_sandboxed_service_has_mounts_and_each_is_an_absolute_path() {
        let sandboxed = "[[service]]\nname = \"c\"\nisolation = \"sandbox\"\nexec = { command = \"/usr/bin/c\" }\n";
        let cfg = Config::parse(&format!("{sandboxed}mounts = [\"/opt/c-data\"]\n")).unwrap();
        assert_eq!(cfg.services[0].mounts, [PathBuf::from("/opt/c-data")]);
        let err = Config::parse(&format!("{sandboxed}mounts = [\"c-data\"]\n")).unwrap_err().to_string();
        assert!(err.contains("must be an absolute path"), "{err}");
        let err = Config::parse("[[service]]\nname = \"c\"\nmounts = [\"/opt\"]\n").unwrap_err().to_string();
        assert!(err.contains("mounts are for a sandboxed service"), "{err}");
    }

    #[test]
    fn parses_env_and_pass_env() {
        let cfg = Config::parse(
            r#"
            [[service]]
            name = "model"
            exec = { command = "molt-gateway" }
            env = { MOLT_MODEL = "sonnet", ANTHROPIC_BASE_URL = "http://127.0.0.1:9" }
            pass_env = ["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "MOLT_EFFORT"]
            "#,
        )
        .unwrap();
        let svc = &cfg.services[0];
        assert_eq!(svc.env["MOLT_MODEL"], "sonnet");
        assert_eq!(svc.pass_env, ["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "MOLT_EFFORT"]);
        // Unset variables are skipped, and `env` wins over a passed variable.
        let host = |name: &str| match name {
            "ANTHROPIC_API_KEY" => Some("sk-1".to_owned()),
            "ANTHROPIC_BASE_URL" => Some("https://example.invalid".to_owned()),
            _ => None,
        };
        assert_eq!(
            svc.launch_env_from(host),
            [
                ("ANTHROPIC_API_KEY".to_owned(), "sk-1".to_owned()),
                ("ANTHROPIC_BASE_URL".to_owned(), "http://127.0.0.1:9".to_owned()),
                ("MOLT_MODEL".to_owned(), "sonnet".to_owned()),
            ]
        );
        assert!(Config::parse("[[service]]\nname = \"x\"\nenv = { A = 1 }\n").is_err());
    }

    #[test]
    fn agent_config_has_the_five_services() {
        let cfg = Config::agent(Path::new("/home/u/proj"), Path::new("/cache/molt/proj"), Path::new("/opt/molt/bin"))
            .unwrap();
        assert_eq!(cfg.kernel.data_dir, Path::new("/cache/molt/proj"));
        assert_eq!(cfg.kernel.transport, TransportKind::Unix);
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["model", "fs", "shell", "memory", "planner"]);
        for svc in &cfg.services {
            assert_eq!(svc.provides, [format!("{}.*", svc.name).parse::<Target>().unwrap()]);
        }

        let model = cfg.service("model").unwrap();
        assert_eq!(model.exec.as_ref().unwrap().command, "/opt/molt/bin/molt-gateway");
        assert_eq!(model.tier, Tier::Protected);
        assert_eq!(model.requests.iter().map(|r| r.target.to_string()).collect::<Vec<_>>(), ["topic:publish:progress"]);
        assert!(model.pass_env.iter().any(|v| v == "ANTHROPIC_API_KEY"));
        assert!(model.pass_env.iter().any(|v| v == "ANTHROPIC_AUTH_TOKEN"));
        assert!(model.pass_env.iter().any(|v| v == "OPENAI_API_KEY"));
        for service in cfg.services.iter().filter(|s| s.name.as_str() != "model") {
            assert!(!service.pass_env.iter().any(|v| v.starts_with("OPENAI_") || v.starts_with("ANTHROPIC_")));
        }
        assert!(model.pass_env.iter().any(|v| v == "ANTHROPIC_BASE_URL"));

        for kind in ["fs", "shell"] {
            let exec = cfg.service(kind).unwrap().exec.clone().unwrap();
            assert_eq!(exec.command, "/opt/molt/bin/molt-tools");
            assert_eq!(exec.args, [kind, "--root", "/home/u/proj", "--scratch", "/cache/molt/proj/work"]);
        }
        let targets = |name: &str| -> Vec<String> {
            cfg.service(name).unwrap().requests.iter().map(|r| r.target.to_string()).collect()
        };
        assert_eq!(targets("fs"), ["topic:publish:fs.changed"], "fs reports the files it changes");
        assert_eq!(targets("shell"), ["topic:publish:progress"], "the shell streams the output it is asked to");

        let memory = cfg.service("memory").unwrap();
        let exec = memory.exec.as_ref().unwrap();
        assert_eq!(exec.command, "/opt/molt/bin/molt-memory");
        assert_eq!(
            exec.args,
            [
                "--root",
                "/home/u/proj",
                "--db",
                "/cache/molt/proj/memory.db",
                "--skip",
                "/cache/molt/proj",
                "--forks",
                "/cache/molt/proj/work"
            ]
        );
        assert_eq!(memory.tier, Tier::Mutable);
        assert_eq!(targets("memory"), ["model.complete", "kernel.audit.read", "topic:subscribe:fs.changed", "fs.view"]);
        let routed = |v: &String| v == molt_api::model::ROLE_MODELS_ENV;
        assert!(
            memory.pass_env.iter().all(|v| v.starts_with("MOLT_MEMORY_") || v.ends_with("_MODEL") || routed(v)),
            "no secrets"
        );
        // Memory learns with the attempts' model unless told otherwise, so it reads which that is.
        assert!(["MOLT_PLANNER_MODEL", "MOLT_MODEL"].iter().all(|v| memory.pass_env.iter().any(|p| p == v)));

        let planner = cfg.service("planner").unwrap();
        assert_eq!(planner.exec.as_ref().unwrap().command, "/opt/molt/bin/molt-planner");
        assert_eq!(planner.exec.as_ref().unwrap().args, ["--state", "/cache/molt/proj/runs"]);
        assert!(planner.pass_env.iter().any(|v| v == "MOLT_MAX_TURNS"));
        assert!(!planner.pass_env.iter().any(|v| v.starts_with("ANTHROPIC")), "the planner never sees the key");
        let targets: Vec<String> = planner.requests.iter().map(|r| r.target.to_string()).collect();
        assert_eq!(
            targets,
            [
                "model.complete",
                "fs.*",
                "shell.run",
                "topic:publish:progress",
                "shell.start",
                "shell.read",
                "shell.write",
                "shell.stop",
                "memory.index",
                "memory.map",
                "memory.recall",
                "memory.symbols",
                "memory.fork_symbols"
            ]
        );
        for req in &planner.requests {
            assert_eq!(req.budget.ms, 0, "{}: no deadline cap", req.target);
            assert!(req.budget.calls >= 1_000_000, "{}: room for long runs", req.target);
        }
        // One run of 8 attempts x 50 turns at 32k tokens each fits many times over.
        assert!(planner.requests[0].budget.tokens >= 8 * 50 * 32_000 * 100);
    }

    #[test]
    fn allowing_web_hosts_adds_a_protected_web_service_the_planner_may_call() {
        let bin = Path::new("/opt/molt/bin");
        let mut cfg = Config::agent(Path::new("/w"), Path::new("/d"), bin).unwrap();
        cfg.allow_web(&[], bin).unwrap();
        assert!(cfg.service(WEB).is_none(), "no hosts, no service");

        let hosts = ["docs.rs".to_owned(), "*.python.org".to_owned()];
        cfg.allow_web(&hosts, bin).unwrap();
        let web = cfg.service(WEB).unwrap();
        assert_eq!(web.tier, Tier::Protected);
        let exec = web.exec.as_ref().unwrap();
        assert_eq!(exec.command, "/opt/molt/bin/molt-web");
        assert_eq!(exec.args, ["--allow", "docs.rs", "--allow", "*.python.org"]);
        assert_eq!(web.provides, ["web.*".parse::<Target>().unwrap()]);
        assert!(web.requests.is_empty() && web.pass_env.is_empty(), "it holds no capability and no secret");
        let planner = cfg.service("planner").unwrap();
        assert!(planner.requests.iter().any(|r| r.target.to_string() == molt_api::web::FETCH));

        let err = cfg.allow_web(&hosts, bin).unwrap_err().to_string();
        assert!(err.contains("already has a web service"), "{err}");
        let mut fresh = Config::agent(Path::new("/w"), Path::new("/d"), bin).unwrap();
        let err = fresh.allow_web(&["http://docs.rs".to_owned()], bin).unwrap_err().to_string();
        assert!(err.contains("not a host to allow"), "{err}");
        assert!(fresh.service(WEB).is_none());
    }
}
