//! Environment profiles: what one toolchain's commands need from the sandbox,
//! as data. Each says which projects it applies to (`detect`), the tools
//! whose installation is mounted read-only, variables to set, whether to
//! build from the user's Cargo cache, and probes that show it works, each
//! with a one-line fix. The built-in ones (`env_profiles.toml`) cover Rust,
//! Python, Go and Node; the user's config may add profiles or replace one.
//!
//! A project only picks which profiles apply, by having their files: what a
//! profile mounts, sets or opens comes from Molt or the user. Built-in
//! profiles never turn on the network and mount only what the host points
//! at (a tool found on PATH, the Cargo home), never a path a project names.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cargo_cache::CargoCache;
use crate::sandbox::{cargo_home_from, check_mount, SandboxPolicy};

const BUILT_IN: &str = include_str!("env_profiles.toml");
/// What the sandbox shows every command, so tools there need no mount.
const VISIBLE: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64"];
/// At most this many read-only mounts in a policy (`SandboxPolicy::validate`).
const MAX_MOUNTS: usize = 32;

/// One toolchain's environment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvProfile {
    /// Paths relative to the project; the profile applies when any exists.
    pub detect: Vec<String>,
    /// Commands found on PATH whose installation is mounted read-only. A
    /// rustup proxy brings rustup's home and `RUSTUP_HOME`.
    pub tools: Vec<String>,
    /// More host directories to mount read-only, absolute.
    pub read_only: Vec<PathBuf>,
    /// Variables set for commands. `$WORKSPACE`, `$BUILD` and `$PATH` in a
    /// value become the command's workspace, its private build directory
    /// and the PATH commands get.
    pub env: BTreeMap<String, String>,
    /// Build offline from the user's Cargo cache when there is one (see `cargo_cache`).
    pub cargo_cache: bool,
    /// Share the host network. Only a profile of the user's can.
    pub network: bool,
    /// Commands that show the environment works, run by `molt env doctor`.
    pub probes: Vec<Probe>,
}

/// A command that exits 0 when part of an environment works.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub command: String,
    /// What to do when it fails, in a sentence.
    #[serde(default)]
    pub fix: String,
}

/// Molt's own profiles.
pub fn built_in() -> BTreeMap<String, EnvProfile> {
    toml::from_str(BUILT_IN).expect("the built-in environment profiles are valid")
}

/// The built-in profiles with the user's over them: one of the user's
/// replaces the built-in profile of its name.
pub fn profiles(user: &BTreeMap<String, EnvProfile>) -> BTreeMap<String, EnvProfile> {
    let mut all = built_in();
    all.extend(user.iter().map(|(name, p)| (name.clone(), p.clone())));
    all
}

/// The profiles that apply to the project in `root`, each with the path that matched.
pub fn detect<'a>(profiles: &'a BTreeMap<String, EnvProfile>, root: &Path) -> Vec<(&'a str, &'a EnvProfile, &'a str)> {
    profiles
        .iter()
        .filter_map(|(name, p)| {
            let found = p.detect.iter().find(|rel| {
                let rel = Path::new(rel.as_str());
                rel.is_relative()
                    && rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
                    && root.join(rel).exists()
            })?;
            Some((name.as_str(), p, found.as_str()))
        })
        .collect()
}

/// What applying one profile did not manage, as sentences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub name: String,
    pub notes: Vec<String>,
}

/// Add what the profiles that apply to the project in `root` need to
/// `policy`. Nothing in `keep_out` (the project, the scratch directory),
/// inside it or holding it is mounted. `var` reads the host environment. What cannot be
/// provided is noted, never an error: the commands then fail as they would
/// without the profile, and `molt env doctor` says why.
pub fn apply(
    policy: &mut SandboxPolicy,
    root: &Path,
    keep_out: &[&Path],
    profiles: &BTreeMap<String, EnvProfile>,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<Applied> {
    let keep_out: Vec<PathBuf> =
        keep_out.iter().map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf())).collect();
    let home = var("HOME").map(PathBuf::from).filter(|h| h.is_absolute()).map(|h| h.canonicalize().unwrap_or(h));
    let path = var("PATH").unwrap_or_default();
    let mut out = Vec::new();
    for (name, profile, _) in detect(profiles, root) {
        let mut notes = Vec::new();
        let mut mounts = Vec::new();
        for tool in &profile.tools {
            match tool_mounts(tool, &path, home.as_deref(), &keep_out, var) {
                Ok((dirs, rustup_home)) => {
                    mounts.extend(dirs);
                    let name = "RUSTUP_HOME".to_owned();
                    if let Some(rustup) = rustup_home.filter(|_| !policy.environment.contains(&name)) {
                        policy.set_env.entry(name).or_insert_with(|| rustup.display().to_string());
                    }
                }
                Err(note) => notes.push(note),
            }
        }
        for dir in &profile.read_only {
            match dir.canonicalize() {
                Ok(real) if check_mount(&real).is_ok() && !overlaps(&real, &keep_out) => mounts.push(real),
                Ok(_) => notes.push(format!("{} cannot be shown to commands", dir.display())),
                Err(e) => notes.push(format!("{}: {e}", dir.display())),
            }
        }
        for dir in mounts {
            if policy.read_only.iter().any(|kept| dir.starts_with(kept)) {
                continue;
            }
            if policy.read_only.len() >= MAX_MOUNTS {
                notes.push(format!("{} is not mounted: the sandbox takes at most {MAX_MOUNTS}", dir.display()));
                continue;
            }
            policy.read_only.push(dir);
        }
        for (key, value) in &profile.env {
            if !policy.environment.contains(key) {
                policy.set_env.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        // A Cargo home the user arranged wins.
        let own_home = policy.environment.iter().any(|n| n == "CARGO_HOME");
        if profile.cargo_cache && policy.cargo_cache.is_none() && !own_home {
            match cargo_home_from(var) {
                Ok(home) if !home.join("registry").is_dir() => notes.push(format!(
                    "there is no Cargo cache in {}, so commands cannot build crates from it",
                    home.join("registry").display()
                )),
                Ok(home) => match CargoCache::open(&home) {
                    Ok(_) => policy.cargo_cache = Some(home),
                    Err(e) => notes.push(format!("commands cannot build from your Cargo cache: {e:#}")),
                },
                Err(e) => notes.push(format!("commands cannot build from your Cargo cache: {e:#}")),
            }
        }
        policy.network |= profile.network;
        out.push(Applied { name: name.to_owned(), notes });
    }
    out
}

/// The directories to mount for `tool`, the last of them rustup's home
/// when the tool is rustup's, or why it cannot be shown to commands.
fn tool_mounts(
    tool: &str,
    path: &OsStr,
    home: Option<&Path>,
    keep_out: &[PathBuf],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
    let found = find_in_path(tool, path).ok_or_else(|| format!("`{tool}` is not on PATH"))?;
    let real = found.canonicalize().map_err(|e| format!("{}: {e}", found.display()))?;
    let mut dirs = Vec::new();
    let mut rustup_home = None;
    if rustup_proxy(&found, &real) {
        // Only the proxies' own directory: the Cargo home above it holds credentials.
        dirs.extend(own_dir(&found, keep_out)?);
        let rustup = var("RUSTUP_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.map(|h| h.join(".rustup")))
            .ok_or_else(|| format!("`{tool}` is rustup's, and rustup has no home directory"))?;
        let rustup = rustup.canonicalize().map_err(|e| format!("rustup's home {}: {e}", rustup.display()))?;
        if check_mount(&rustup).is_err() || overlaps(&rustup, keep_out) {
            return Err(format!("rustup's home {} cannot be shown to commands", rustup.display()));
        }
        dirs.push(rustup.clone());
        rustup_home = Some(rustup);
    } else {
        for exe in [&found, &real] {
            dirs.extend(install_dir(exe, home, keep_out)?);
        }
    }
    Ok((dirs, rustup_home))
}

/// The directory `exe` is in, unless the sandbox shows it already.
fn own_dir(exe: &Path, keep_out: &[PathBuf]) -> Result<Option<PathBuf>, String> {
    let dir = exe
        .parent()
        .and_then(|d| d.canonicalize().ok())
        .ok_or_else(|| format!("{} has no directory", exe.display()))?;
    if VISIBLE.iter().any(|v| dir.starts_with(v)) {
        return Ok(None);
    }
    if check_mount(&dir).is_err() || overlaps(&dir, keep_out) || dir == Path::new("/opt") {
        return Err(format!("{} is in {}, which cannot be shown to commands", exe.display(), dir.display()));
    }
    Ok(Some(dir))
}

/// Where `exe` is installed: the prefix above its `bin` directory, so its
/// libraries come along, or the directory itself when that prefix is the
/// home directory or one directly in it (`~/.local`, `~/.cargo`), which hold
/// far more than the tool.
fn install_dir(exe: &Path, home: Option<&Path>, keep_out: &[PathBuf]) -> Result<Option<PathBuf>, String> {
    let Some(dir) = own_dir(exe, keep_out)? else { return Ok(None) };
    let prefix = (dir.file_name() == Some(OsStr::new("bin"))).then(|| dir.parent()).flatten();
    let prefix = prefix.filter(|p| {
        let near_home = home.is_some_and(|h| *p == h || p.parent() == Some(h));
        !near_home && check_mount(p).is_ok() && !overlaps(p, keep_out) && *p != Path::new("/opt")
    });
    Ok(Some(prefix.map(Path::to_path_buf).unwrap_or(dir)))
}

/// Whether `found` is one of rustup's proxies: a link to rustup, symbolic or hard.
fn rustup_proxy(found: &Path, real: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if real.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rustup")) {
        return true;
    }
    let same = |a: &std::fs::Metadata, b: &std::fs::Metadata| a.dev() == b.dev() && a.ino() == b.ino();
    match (std::fs::metadata(found), found.parent().map(|d| std::fs::metadata(d.join("rustup")))) {
        (Ok(tool), Some(Ok(rustup))) => same(&tool, &rustup),
        _ => false,
    }
}

/// Whether `dir` is one of `paths`, holds one or is inside one.
fn overlaps(dir: &Path, paths: &[PathBuf]) -> bool {
    paths.iter().any(|p| p.starts_with(dir) || dir.starts_with(p))
}

/// The first `tool` on `path` that is an executable file.
fn find_in_path(tool: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(tool))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn exe(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn host(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars.iter().map(|(k, v)| (k.to_string(), v.as_os_str().into())).collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_built_in_profiles_cover_four_toolchains_and_never_open_the_network() {
        let all = built_in();
        for name in ["rust", "python", "python-venv", "go", "node"] {
            let p = &all[name];
            assert!(!p.network && p.read_only.is_empty(), "{name}");
            assert!(!p.probes.is_empty() && p.probes.iter().all(|probe| !probe.fix.is_empty()), "{name}");
        }
        assert!(all["rust"].cargo_cache);
        let mut user = BTreeMap::new();
        user.insert("rust".to_owned(), EnvProfile { detect: vec![], ..EnvProfile::default() });
        assert!(profiles(&user)["rust"].detect.is_empty(), "the user's profile replaces the built-in one");
    }

    #[test]
    fn a_rust_project_gets_rustups_toolchain_and_the_cargo_cache_but_not_the_cargo_home() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let home = tmp.join("home");
        exe(&home.join(".cargo/bin/rustup"));
        symlink("rustup", home.join(".cargo/bin/cargo")).unwrap();
        symlink("rustup", home.join(".cargo/bin/rustc")).unwrap();
        fs::create_dir_all(home.join(".rustup/toolchains")).unwrap();
        fs::create_dir_all(home.join(".cargo/registry/index")).unwrap();
        fs::write(home.join(".cargo/credentials.toml"), "token = \"secret\"\n").unwrap();
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        let bin = home.join(".cargo/bin");
        let var = host(&[("HOME", &home), ("PATH", &bin)]);
        let mut policy = SandboxPolicy::default();
        let applied = apply(&mut policy, &project, &[&project], &built_in(), &var);
        assert_eq!(applied, [Applied { name: "rust".into(), notes: vec![] }]);
        assert_eq!(policy.read_only, [bin, home.join(".rustup")], "never the whole Cargo home");
        assert_eq!(policy.set_env["RUSTUP_HOME"], home.join(".rustup").display().to_string());
        assert_eq!(policy.cargo_cache, Some(home.join(".cargo")));
        assert!(!policy.network);

        // Without a Cargo cache the run goes on, and the note says what to do.
        fs::remove_dir_all(home.join(".cargo/registry")).unwrap();
        let mut policy = SandboxPolicy::default();
        let applied = apply(&mut policy, &project, &[&project], &built_in(), &var);
        assert_eq!(policy.cargo_cache, None);
        assert!(applied[0].notes[0].contains("no Cargo cache in"), "{:?}", applied[0].notes);
        // A Cargo home of the user's own is left alone.
        let mut policy = SandboxPolicy { environment: vec!["CARGO_HOME".into()], ..SandboxPolicy::default() };
        assert!(apply(&mut policy, &project, &[&project], &built_in(), &var)[0].notes.is_empty());
    }

    #[test]
    fn a_tool_is_mounted_from_its_prefix_unless_that_is_near_home_or_holds_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let home = tmp.join("home");
        exe(&tmp.join("opt/node22/bin/node"));
        exe(&home.join(".local/bin/npm"));
        fs::create_dir_all(home.join("project")).unwrap();
        let none: [PathBuf; 0] = [];
        assert_eq!(install_dir(&tmp.join("opt/node22/bin/node"), Some(&home), &none), Ok(Some(tmp.join("opt/node22"))));
        assert_eq!(install_dir(&home.join(".local/bin/npm"), Some(&home), &none), Ok(Some(home.join(".local/bin"))));
        let project = [home.join("project")];
        exe(&home.join("bin/go"));
        assert_eq!(install_dir(&home.join("bin/go"), None, &project), Ok(Some(home.join("bin"))), "holds the project");
        assert_eq!(install_dir(Path::new("/usr/bin/env"), Some(&home), &none), Ok(None), "visible already");
        exe(&home.join("project/bin/tool"));
        assert!(install_dir(&home.join("project/bin/tool"), Some(&home), &project).is_err());
    }

    #[test]
    fn a_venv_puts_its_interpreter_first_and_a_project_cannot_name_what_is_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().canonicalize().unwrap();
        fs::create_dir(project.join(".venv")).unwrap();
        // A project can only pick profiles; what it points at is never mounted.
        fs::write(project.join(".venv/pyvenv.cfg"), "home = /etc\n").unwrap();
        let var = host(&[("PATH", Path::new("/usr/bin:/bin"))]);
        let mut policy = SandboxPolicy::default();
        let applied = apply(&mut policy, &project, &[&project], &built_in(), &var);
        assert_eq!(applied.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["python-venv"]);
        assert_eq!(policy.set_env["PATH"], "$WORKSPACE/.venv/bin:$PATH");
        assert_eq!(policy.set_env["VIRTUAL_ENV"], "$WORKSPACE/.venv");
        assert!(policy.read_only.is_empty() && !policy.network && policy.cargo_cache.is_none());
        // A detect path that leaves the project matches nothing.
        let mut odd = BTreeMap::new();
        odd.insert("odd".to_owned(), EnvProfile { detect: vec!["../".into(), "/etc".into()], ..EnvProfile::default() });
        assert!(detect(&odd, &project.join(".venv")).is_empty());
    }
}
