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

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use molt_proto::{Manifest, ServiceId, Tier, VersionId};
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

#[cfg(test)]
mod tests {
    use super::*;
    use molt_proto::{Budget, CapRequest};

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
