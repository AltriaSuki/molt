//! Module registry: content-addressed service versions and one live pointer
//! per service.
//!
//! A version id is the SHA-256 of the manifest's canonical JSON, so a version
//! can never change after it is stored. Promotion is a compare-and-swap on the
//! pointer: the caller names the version it expects to replace, so two racing
//! promotions cannot both win. Rolling back is promoting the parent.
//!
//! The registry enforces the tier rules itself, whoever asks:
//! - kernel and protected services are promoted by a human only;
//! - a gate may not change a service's tier;
//! - a gate may not promote a version that asks for capabilities the live
//!   version did not have, unless that version was live before (a rollback).
//!   Anything else that widens capabilities needs a human.
//!
//! Data modules (such as the planner's prompts) live beside the services,
//! content-addressed the same way, with a live pointer per module name. A
//! gate may move that pointer only between mutable-tier versions; anything
//! of another tier is a human's to promote. No live version means the
//! reader's own built-in content, which a rollback can return to.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use molt_proto::{Manifest, Module, ServiceId, Tier, VersionId};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("unknown version {0}")]
    UnknownVersion(VersionId),
    #[error("version {version} belongs to {actual}, not {service}")]
    WrongService { service: ServiceId, version: VersionId, actual: ServiceId },
    #[error("{0} is a {1:?}-tier service; only a human can promote it")]
    NeedsHuman(ServiceId, Tier),
    #[error("a gate cannot change the tier of {0}")]
    TierChange(ServiceId),
    #[error("the new version requests capabilities the live one lacks; a human must approve")]
    NewCapabilities,
    #[error("live version of {service} is {actual:?}, not the expected {expected:?}")]
    Conflict { service: ServiceId, expected: Option<VersionId>, actual: Option<VersionId> },
    #[error("unknown module version {0}")]
    UnknownModuleVersion(VersionId),
    #[error("module version {version} belongs to {actual}, not {module}")]
    WrongModule { module: String, version: VersionId, actual: String },
    #[error("module {0} is {1:?}-tier; only a human can promote it")]
    ModuleNeedsHuman(String, Tier),
    #[error("live version of module {module} is {actual:?}, not the expected {expected:?}")]
    ModuleConflict { module: String, expected: Option<VersionId>, actual: Option<VersionId> },
    #[error("{0}")]
    InvalidModule(String),
    #[error("io: {0}")]
    Io(String),
}

/// Who is asking for a promotion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authority {
    /// A person, through the CLI or kernel configuration.
    Human,
    /// A service holding a capability for `kernel.registry.promote`.
    Gate(ServiceId),
}

impl std::fmt::Display for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Authority::Human => f.write_str("human"),
            Authority::Gate(s) => write!(f, "gate:{s}"),
        }
    }
}

pub fn version_of(manifest: &Manifest) -> VersionId {
    // serde_json maps are sorted, so this is canonical for a given manifest.
    let bytes = serde_json::to_vec(&serde_json::to_value(manifest).expect("manifests serialize")).unwrap();
    VersionId(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// A data module's version id: the SHA-256 of its canonical JSON.
pub fn module_version_of(module: &Module) -> VersionId {
    let bytes = serde_json::to_vec(&serde_json::to_value(module).expect("modules serialize")).unwrap();
    VersionId(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// The live data modules of the registry persisted under `dir`, read
/// without creating or locking anything: what a reader that is not the
/// kernel (such as `molt do` starting a planner) uses. None when there is
/// no registry there yet.
pub fn live_modules_in(dir: &std::path::Path) -> Result<Vec<(VersionId, Module)>, RegistryError> {
    let state: State = match std::fs::read(dir.join("registry.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| RegistryError::Io(e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RegistryError::Io(e.to_string())),
    };
    Ok(state.live_modules_sorted())
}

pub struct Registry {
    dir: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    versions: HashMap<VersionId, Manifest>,
    live: HashMap<ServiceId, VersionId>,
    /// Every version that has ever been live. Returning to one of these
    /// (a rollback) does not need fresh approval for its capabilities.
    #[serde(default)]
    approved: std::collections::HashSet<VersionId>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    modules: HashMap<VersionId, Module>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    live_modules: HashMap<String, VersionId>,
}

impl State {
    fn live_modules_sorted(&self) -> Vec<(VersionId, Module)> {
        let mut live: Vec<(VersionId, Module)> =
            self.live_modules.values().filter_map(|v| self.modules.get(v).map(|m| (v.clone(), m.clone()))).collect();
        live.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        live
    }
}

impl Registry {
    /// A registry persisted under `dir` (created if missing, readable by its
    /// owner only, as is the file the registry writes there).
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let dir = dir.into();
        let mut dirs = std::fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
        dirs.create(&dir).map_err(|e| RegistryError::Io(e.to_string()))?;
        let file = dir.join("registry.json");
        let state = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| RegistryError::Io(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(RegistryError::Io(e.to_string())),
        };
        Ok(Self { dir: Some(dir), state: Mutex::new(state) })
    }

    /// A registry that lives only in memory.
    pub fn in_memory() -> Self {
        Self { dir: None, state: Mutex::default() }
    }

    fn persist(&self, state: &State) -> Result<(), RegistryError> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let io = |e: std::io::Error| RegistryError::Io(e.to_string());
        let tmp = dir.join("registry.json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| RegistryError::Io(e.to_string()))?;
        options.open(&tmp).and_then(|mut f| f.write_all(&bytes)).map_err(io)?;
        std::fs::rename(&tmp, dir.join("registry.json")).map_err(io)
    }

    /// Store a version. Storing never makes it live.
    pub fn store(&self, manifest: Manifest) -> Result<VersionId, RegistryError> {
        let version = version_of(&manifest);
        let mut st = self.state.lock().unwrap();
        st.versions.insert(version.clone(), manifest);
        self.persist(&st)?;
        Ok(version)
    }

    pub fn manifest(&self, version: &VersionId) -> Option<Manifest> {
        self.state.lock().unwrap().versions.get(version).cloned()
    }

    pub fn live(&self, service: &ServiceId) -> Option<VersionId> {
        self.state.lock().unwrap().live.get(service).cloned()
    }

    /// Make `version` the live version of `service` if the live version is
    /// still `expected`.
    pub fn promote(
        &self,
        service: &ServiceId,
        expected: Option<&VersionId>,
        version: &VersionId,
        by: &Authority,
    ) -> Result<Option<VersionId>, RegistryError> {
        let mut st = self.state.lock().unwrap();
        let new = st.versions.get(version).ok_or_else(|| RegistryError::UnknownVersion(version.clone()))?;
        if &new.name != service {
            return Err(RegistryError::WrongService {
                service: service.clone(),
                version: version.clone(),
                actual: new.name.clone(),
            });
        }
        let current_id = st.live.get(service).cloned();
        let current = current_id.as_ref().and_then(|v| st.versions.get(v));
        if *by != Authority::Human {
            for tier in [Some(new.tier), current.map(|c| c.tier)].into_iter().flatten() {
                if tier != Tier::Mutable {
                    return Err(RegistryError::NeedsHuman(service.clone(), tier));
                }
            }
            let Some(current) = current else {
                // A brand-new service has no approved capabilities to stay within.
                return Err(RegistryError::NewCapabilities);
            };
            if current.tier != new.tier {
                return Err(RegistryError::TierChange(service.clone()));
            }
            let rollback = st.approved.contains(version);
            let within = rollback
                || new.requests.iter().all(|r| {
                    current.requests.iter().any(|c| c.target.covers(&r.target) && r.budget.fits_within(&c.budget))
                });
            if !within {
                return Err(RegistryError::NewCapabilities);
            }
        }
        if current_id.as_ref() != expected {
            return Err(RegistryError::Conflict {
                service: service.clone(),
                expected: expected.cloned(),
                actual: current_id,
            });
        }
        st.live.insert(service.clone(), version.clone());
        st.approved.insert(version.clone());
        self.persist(&st)?;
        Ok(current_id)
    }
}

impl Registry {
    /// Store a data module's version. Storing never makes it live.
    pub fn store_module(&self, module: Module) -> Result<VersionId, RegistryError> {
        module.validate().map_err(RegistryError::InvalidModule)?;
        let version = module_version_of(&module);
        let mut st = self.state.lock().unwrap();
        st.modules.insert(version.clone(), module);
        self.persist(&st)?;
        Ok(version)
    }

    pub fn module(&self, version: &VersionId) -> Option<Module> {
        self.state.lock().unwrap().modules.get(version).cloned()
    }

    pub fn live_module(&self, name: &str) -> Option<VersionId> {
        self.state.lock().unwrap().live_modules.get(name).cloned()
    }

    /// Every live data module, by name.
    pub fn live_modules(&self) -> Vec<(VersionId, Module)> {
        self.state.lock().unwrap().live_modules_sorted()
    }

    /// Make `version` the live version of module `name`, or with `None`
    /// return it to the reader's built-in content, if the live version is
    /// still `expected`. A gate may only move between mutable-tier versions.
    pub fn promote_module(
        &self,
        name: &str,
        expected: Option<&VersionId>,
        version: Option<&VersionId>,
        by: &Authority,
    ) -> Result<Option<VersionId>, RegistryError> {
        let mut st = self.state.lock().unwrap();
        let new = match version {
            Some(v) => {
                let m = st.modules.get(v).ok_or_else(|| RegistryError::UnknownModuleVersion(v.clone()))?;
                if m.name != name {
                    return Err(RegistryError::WrongModule {
                        module: name.to_owned(),
                        version: v.clone(),
                        actual: m.name.clone(),
                    });
                }
                Some(m)
            }
            None => None,
        };
        let current_id = st.live_modules.get(name).cloned();
        if *by != Authority::Human {
            let current = current_id.as_ref().and_then(|v| st.modules.get(v));
            for tier in [new.map(|m| m.tier), current.map(|c| c.tier)].into_iter().flatten() {
                if tier != Tier::Mutable {
                    return Err(RegistryError::ModuleNeedsHuman(name.to_owned(), tier));
                }
            }
        }
        if current_id.as_ref() != expected {
            return Err(RegistryError::ModuleConflict {
                module: name.to_owned(),
                expected: expected.cloned(),
                actual: current_id,
            });
        }
        match version {
            Some(v) => st.live_modules.insert(name.to_owned(), v.clone()),
            None => st.live_modules.remove(name),
        };
        self.persist(&st)?;
        Ok(current_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_proto::{Budget, CapRequest};

    fn module(name: &str, tier: Tier, parent: Option<&VersionId>, text: &str) -> Module {
        Module { name: name.into(), tier, parent: parent.cloned(), text: text.into() }
    }

    #[test]
    fn a_gate_moves_a_mutable_module_and_back_but_never_another_tier() {
        let reg = Registry::in_memory();
        let gate = Authority::Gate(ServiceId::new("gate").unwrap());
        let v1 = reg.store_module(module("prompt.attempt", Tier::Mutable, None, "be careful")).unwrap();
        assert_eq!(v1, module_version_of(&module("prompt.attempt", Tier::Mutable, None, "be careful")));
        assert_eq!(reg.promote_module("prompt.attempt", None, Some(&v1), &gate), Ok(None));
        let v2 = reg.store_module(module("prompt.attempt", Tier::Mutable, Some(&v1), "be very careful")).unwrap();
        // Promotion is a compare-and-swap on the live version.
        assert!(matches!(
            reg.promote_module("prompt.attempt", None, Some(&v2), &gate),
            Err(RegistryError::ModuleConflict { .. })
        ));
        assert_eq!(reg.promote_module("prompt.attempt", Some(&v1), Some(&v2), &gate), Ok(Some(v1.clone())));
        // Rolling back is promoting the parent, and the built-in is the parent of the first.
        assert_eq!(reg.promote_module("prompt.attempt", Some(&v2), Some(&v1), &gate), Ok(Some(v2)));
        assert_eq!(reg.promote_module("prompt.attempt", Some(&v1), None, &gate), Ok(Some(v1)));
        assert_eq!(reg.live_module("prompt.attempt"), None);

        let reviewer = reg.store_module(module("prompt.reviewer", Tier::Protected, None, "approve")).unwrap();
        assert_eq!(
            reg.promote_module("prompt.reviewer", None, Some(&reviewer), &gate),
            Err(RegistryError::ModuleNeedsHuman("prompt.reviewer".into(), Tier::Protected))
        );
        reg.promote_module("prompt.reviewer", None, Some(&reviewer), &Authority::Human).unwrap();
        // Nor may a gate replace a protected version with a mutable one, or retire it.
        let sneaky = reg.store_module(module("prompt.reviewer", Tier::Mutable, None, "approve all")).unwrap();
        for to in [Some(&sneaky), None] {
            assert!(matches!(
                reg.promote_module("prompt.reviewer", Some(&reviewer), to, &gate),
                Err(RegistryError::ModuleNeedsHuman(..))
            ));
        }
        // A version of another module is refused.
        assert!(matches!(
            reg.promote_module("prompt.attempt", None, Some(&sneaky), &Authority::Human),
            Err(RegistryError::WrongModule { .. })
        ));
        assert!(matches!(
            reg.store_module(module("Prompt/x", Tier::Mutable, None, "x")),
            Err(RegistryError::InvalidModule(_))
        ));
    }

    #[test]
    fn live_modules_are_read_from_the_file_without_a_kernel() {
        let dir = tempfile::tempdir().unwrap();
        assert!(live_modules_in(dir.path()).unwrap().is_empty());
        let reg = Registry::open(dir.path()).unwrap();
        let v = reg.store_module(module("prompt.attempt", Tier::Mutable, None, "text")).unwrap();
        reg.store_module(module("prompt.designer", Tier::Mutable, None, "stored, never live")).unwrap();
        reg.promote_module("prompt.attempt", None, Some(&v), &Authority::Human).unwrap();
        let live = live_modules_in(dir.path()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!((&live[0].0, live[0].1.text.as_str()), (&v, "text"));
        assert_eq!(Registry::open(dir.path()).unwrap().live_modules(), live);
    }

    fn manifest(name: &str, tier: Tier, requests: &[(&str, u64)]) -> Manifest {
        Manifest {
            name: ServiceId::new(name).unwrap(),
            tier,
            parent: None,
            provides: vec![],
            requests: requests
                .iter()
                .map(|(t, tokens)| CapRequest { target: t.parse().unwrap(), budget: Budget::new(*tokens, 0, 0) })
                .collect(),
            exec: None,
            tools: Vec::new(),
        }
    }

    #[test]
    fn version_ids_are_content_hashes() {
        let a = manifest("p", Tier::Mutable, &[]);
        let mut b = a.clone();
        assert_eq!(version_of(&a), version_of(&b));
        b.tier = Tier::Protected;
        assert_ne!(version_of(&a), version_of(&b));
    }

    #[test]
    fn racing_promotions_cannot_both_win() {
        let reg = Registry::in_memory();
        let p = ServiceId::new("p").unwrap();
        let v1 = reg.store(manifest("p", Tier::Mutable, &[("m.x", 10)])).unwrap();
        reg.promote(&p, None, &v1, &Authority::Human).unwrap();
        let mut m2 = manifest("p", Tier::Mutable, &[("m.x", 5)]);
        m2.parent = Some(v1.clone());
        let v2 = reg.store(m2).unwrap();
        let mut m3 = manifest("p", Tier::Mutable, &[("m.x", 1)]);
        m3.parent = Some(v1.clone());
        let v3 = reg.store(m3).unwrap();
        let gate = Authority::Gate(ServiceId::new("gate").unwrap());
        assert_eq!(reg.promote(&p, Some(&v1), &v2, &gate), Ok(Some(v1.clone())));
        assert!(matches!(reg.promote(&p, Some(&v1), &v3, &gate), Err(RegistryError::Conflict { .. })));
        // Rolling back is promoting the parent.
        assert_eq!(reg.promote(&p, Some(&v2), &v1, &gate), Ok(Some(v2)));
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = ServiceId::new("p").unwrap();
        let v = {
            let reg = Registry::open(dir.path()).unwrap();
            let v = reg.store(manifest("p", Tier::Mutable, &[])).unwrap();
            reg.promote(&p, None, &v, &Authority::Human).unwrap();
            v
        };
        assert_eq!(Registry::open(dir.path()).unwrap().live(&p), Some(v));
    }

    #[cfg(unix)]
    #[test]
    fn a_new_registry_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let reg_dir = dir.path().join("registry");
        Registry::open(&reg_dir).unwrap().store(manifest("p", Tier::Mutable, &[])).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&reg_dir), 0o700);
        assert_eq!(mode(&reg_dir.join("registry.json")), 0o600);
    }
}
