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
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use molt_proto::{CapRequest, Exec, Manifest, ServiceId, Target, Tier};
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
}

fn mutable() -> Tier {
    Tier::Mutable
}

impl ServiceSection {
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
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub kernel: KernelSection,
    #[serde(default, rename = "service")]
    pub services: Vec<ServiceSection>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
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

    /// Load existing secrets and add fresh ones for any new service.
    pub fn load_or_create(path: &Path, services: &[ServiceId]) -> anyhow::Result<Self> {
        let mut s = Self::load(path).unwrap_or_else(|_| Self { kernel: Secret::random(), services: BTreeMap::new() });
        for id in services {
            s.services.entry(id.to_string()).or_insert_with(Secret::random);
        }
        let file = SecretsFile {
            kernel: s.kernel.expose().to_owned(),
            services: s.services.iter().map(|(k, v)| (k.clone(), v.expose().to_owned())).collect(),
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
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
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse("[kernel]\ntransprot = \"unix\"\n").is_err());
    }
}
