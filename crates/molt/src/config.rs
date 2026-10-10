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
//! requests = [{ target = "topic:builds", budget = { calls = 100 } }]
//! memory_mb = 512
//! env = { GREETING = "hi" }           # set for the process
//! pass_env = ["ECHO_PREFIX"]          # copied from molt's environment when set
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use molt_proto::{Budget, CapRequest, Exec, Manifest, ServiceId, Target, Tier};
use molt_transport::Secret;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
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
        }
    }

    pub fn manifest(&self) -> anyhow::Result<Manifest> {
        Ok(Manifest {
            name: self.name.clone(),
            tier: self.tier,
            parent: None,
            provides: self.provides.clone(),
            requests: self.requests.clone(),
            exec: self.exec.clone(),
        })
    }

    /// The extra environment to launch the service with: each `pass_env`
    /// variable the molt process has, then `env`, which wins.
    pub fn launch_env(&self) -> Vec<(String, String)> {
        self.launch_env_from(|name| std::env::var(name).ok())
    }

    fn launch_env_from(&self, var: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        let passed = self
            .pass_env
            .iter()
            .filter(|name| !self.env.contains_key(*name))
            .filter_map(|name| Some((name.clone(), var(name)?)));
        passed.chain(self.env.iter().map(|(k, v)| (k.clone(), v.clone()))).collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub kernel: KernelSection,
    #[serde(default, rename = "service")]
    pub services: Vec<ServiceSection>,
}

/// What the gateway reads from its environment (see `molt_gateway::Config::from_env`).
const GATEWAY_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "MOLT_MODEL",
    "MOLT_EFFORT",
    "MOLT_MAX_TOKENS",
    "MOLT_MODEL_TIMEOUT_S",
    "MOLT_MODEL_RETRIES",
    "MOLT_MODEL_CONCURRENCY",
    "MOLT_FALLBACKS",
];

/// What the planner reads from its environment (see `molt_planner::Config::from_env`).
const PLANNER_ENV: &[&str] = &[
    "MOLT_PLANNER_MODEL",
    "MOLT_MAX_TURNS",
    "MOLT_MAX_CHECK_ROUNDS",
    "MOLT_BUDGET_USD",
    "MOLT_MAX_TOKENS",
    "MOLT_MODEL_TIMEOUT_S",
    "MOLT_CHECK_TIMEOUT_S",
    "MOLT_MAP_TOKENS",
];

/// What the memory service reads from its environment (see `molt_memory::Config::from_env`).
const MEMORY_ENV: &[&str] =
    &["MOLT_MEMORY_MODEL", "MOLT_MEMORY_EFFORT", "MOLT_MEMORY_MAX_TOKENS", "MOLT_MEMORY_MODEL_TIMEOUT_S"];

/// The memory database in a data dir.
pub const MEMORY_DB: &str = "memory.db";

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
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
        model.requests = vec![request(&format!("topic:{}", molt_api::progress::TOPIC), Budget::new(0, 0, 10_000_000))?];
        let changed = format!("topic:{}", molt_api::fs::CHANGED);
        let mut fs = service("fs", Tier::Mutable, tools("fs")?)?;
        fs.requests = vec![request(&changed, Budget::new(0, 0, 100_000_000))?];
        let shell = service("shell", Tier::Protected, tools("shell")?)?;
        let mut memory_exec = bin("molt-memory")?;
        // The project model leaves out the data dir, with the forks in it, should it be inside the workspace.
        memory_exec.args = vec![
            "--root".into(),
            text(workspace)?,
            "--db".into(),
            text(&data_dir.join(MEMORY_DB))?,
            "--skip".into(),
            text(data_dir)?,
        ];
        let mut memory = service("memory", Tier::Mutable, memory_exec)?;
        memory.pass_env = names(MEMORY_ENV);
        // Memory reads finished episodes from the audit log and has the model
        // learn from them; it follows the files fs changes to keep its model current.
        memory.requests = vec![
            request(molt_api::model::COMPLETE, Budget::new(1_000_000_000, 0, 1_000_000))?,
            request(molt_proto::audit::READ, Budget::new(0, 0, 10_000_000))?,
            request(&changed, Budget::new(0, 0, 10))?,
        ];
        let mut planner = service("planner", Tier::Mutable, bin("molt-planner")?)?;
        planner.pass_env = names(PLANNER_ENV);
        // A budget of zero calls refuses every call, and `ms` 0 leaves deadlines
        // to each request; the planner enforces the run's own limits.
        planner.requests = vec![
            request(molt_api::model::COMPLETE, Budget::new(4_000_000_000, 0, 1_000_000))?,
            request("fs.*", Budget::new(0, 0, 10_000_000))?,
            request(molt_api::shell::RUN, Budget::new(0, 0, 1_000_000))?,
            request(&format!("topic:{}", molt_api::progress::TOPIC), Budget::new(0, 0, 10_000_000))?,
        ];
        // What memory knows about the workspace; not remember, forget or consolidate.
        for method in
            [molt_api::memory::INDEX, molt_api::memory::MAP, molt_api::memory::RECALL, molt_api::memory::SYMBOLS]
        {
            planner.requests.push(request(method, Budget::new(0, 0, 10_000_000))?);
        }
        Ok(Self {
            kernel: KernelSection { data_dir: data_dir.to_path_buf(), ..KernelSection::default() },
            services: vec![model, fs, shell, memory, planner],
        })
    }

    pub fn service(&self, name: &str) -> Option<&ServiceSection> {
        self.services.iter().find(|s| s.name.as_str() == name)
    }

    pub fn service_mut(&mut self, name: &str) -> Option<&mut ServiceSection> {
        self.services.iter_mut().find(|s| s.name.as_str() == name)
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
        assert!(cfg.service("echo").is_some());
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
        assert_eq!(model.requests.iter().map(|r| r.target.to_string()).collect::<Vec<_>>(), ["topic:progress"]);
        assert!(model.pass_env.iter().any(|v| v == "ANTHROPIC_API_KEY"));
        assert!(model.pass_env.iter().any(|v| v == "ANTHROPIC_BASE_URL"));

        for kind in ["fs", "shell"] {
            let exec = cfg.service(kind).unwrap().exec.clone().unwrap();
            assert_eq!(exec.command, "/opt/molt/bin/molt-tools");
            assert_eq!(exec.args, [kind, "--root", "/home/u/proj", "--scratch", "/cache/molt/proj/work"]);
        }
        let targets = |name: &str| -> Vec<String> {
            cfg.service(name).unwrap().requests.iter().map(|r| r.target.to_string()).collect()
        };
        assert_eq!(targets("fs"), ["topic:fs.changed"], "fs reports the files it changes");
        assert!(targets("shell").is_empty());

        let memory = cfg.service("memory").unwrap();
        let exec = memory.exec.as_ref().unwrap();
        assert_eq!(exec.command, "/opt/molt/bin/molt-memory");
        assert_eq!(
            exec.args,
            ["--root", "/home/u/proj", "--db", "/cache/molt/proj/memory.db", "--skip", "/cache/molt/proj"]
        );
        assert_eq!(memory.tier, Tier::Mutable);
        assert_eq!(targets("memory"), ["model.complete", "kernel.audit.read", "topic:fs.changed"]);
        assert!(memory.pass_env.iter().all(|v| v.starts_with("MOLT_MEMORY_")));

        let planner = cfg.service("planner").unwrap();
        assert_eq!(planner.exec.as_ref().unwrap().command, "/opt/molt/bin/molt-planner");
        assert!(planner.pass_env.iter().any(|v| v == "MOLT_MAX_TURNS"));
        assert!(!planner.pass_env.iter().any(|v| v.starts_with("ANTHROPIC")), "the planner never sees the key");
        let targets: Vec<String> = planner.requests.iter().map(|r| r.target.to_string()).collect();
        assert_eq!(
            targets,
            [
                "model.complete",
                "fs.*",
                "shell.run",
                "topic:progress",
                "memory.index",
                "memory.map",
                "memory.recall",
                "memory.symbols"
            ]
        );
        for req in &planner.requests {
            assert_eq!(req.budget.ms, 0, "{}: no deadline cap", req.target);
            assert!(req.budget.calls >= 1_000_000, "{}: room for long runs", req.target);
        }
        // One run of 8 attempts x 50 turns at 32k tokens each fits many times over.
        assert!(planner.requests[0].budget.tokens >= 8 * 50 * 32_000 * 100);
    }
}
