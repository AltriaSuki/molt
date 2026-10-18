//! `--deps-cache cargo`: commands build offline from the host's Cargo cache.
//!
//! The sandbox's home is private, so Cargo finds no downloaded crates and,
//! with no network, fails at the first dependency. With this option the
//! host's `$CARGO_HOME/registry` and `git` are mounted read-only at their own
//! paths, and commands get a private writable `CARGO_HOME` in the workspace's
//! build directory, with `CARGO_NET_OFFLINE=true`. Nothing else of the host's
//! Cargo home is visible: not its credentials, and of its config only the
//! source replacement and registry index URLs, without credentials in them
//! (a mirror's crates are cached under the mirror's name, so without them
//! Cargo would look for them under crates.io's).
//!
//! Cargo writes in its home even offline: the package cache lock, its
//! last-use database, and the sources it extracts from a downloaded `.crate`
//! into `registry/src`. Source replacement cannot point at the host cache,
//! whose layout is neither a local registry nor a vendored directory. So the
//! private home is an overlay made of links: to the read-only index and
//! `.crate` files, one to each source the host already extracted, and room to
//! extract the rest, which Cargo does from the read-only `.crate`. Git
//! checkouts work the same way, over the read-only git database. The links
//! are refreshed before every command through descriptors opened without
//! following symlinks, since commands can change anything in the private home.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};

/// The host's Cargo cache, checked when the shell starts.
#[derive(Debug)]
pub(crate) struct CargoCache {
    /// Canonical `registry` and `git` directories; `git` when there is one.
    registry: PathBuf,
    git: Option<PathBuf>,
    /// The private home's `config.toml`.
    config: String,
}

impl CargoCache {
    /// The cache of the Cargo home `home`.
    pub(crate) fn open(home: &Path) -> anyhow::Result<Self> {
        ensure!(home.is_absolute(), "the Cargo home {} must be an absolute path", home.display());
        let home = home.canonicalize().with_context(|| format!("the Cargo home {}", home.display()))?;
        let dir = |name: &str| -> anyhow::Result<Option<PathBuf>> {
            let path = home.join(name);
            if !path.exists() {
                return Ok(None);
            }
            let real = path.canonicalize().with_context(|| format!("resolving {}", path.display()))?;
            ensure!(real.is_dir(), "{} is not a directory", path.display());
            // A cache that holds the home would expose its credentials.
            ensure!(!home.starts_with(&real), "{} must not contain the Cargo home {}", path.display(), home.display());
            crate::sandbox::check_mount(&real).with_context(|| format!("mounting {}", path.display()))?;
            Ok(Some(real))
        };
        let registry = dir("registry")?.with_context(|| {
            format!(
                "--deps-cache cargo found no Cargo cache in {}: run `cargo fetch` in the project on this machine first",
                home.join("registry").display()
            )
        })?;
        let git = dir("git")?;
        Ok(Self { registry, git, config: config(&home)? })
    }

    /// The host directories to mount read-only.
    pub(crate) fn mounts(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.registry.as_path()).chain(self.git.as_deref())
    }

    /// Bring the private home `home`, an existing private directory, up to
    /// date with the host cache.
    pub(crate) fn prepare(&self, home: &Path) -> io::Result<()> {
        let root = Dir::open(home)?;
        let registry = root.sub("registry")?;
        for name in ["index", "cache"] {
            let host = self.registry.join(name);
            if host.is_dir() {
                registry.link(OsStr::new(name), &host)?;
            }
        }
        overlay(&registry, "src", &self.registry.join("src"))?;
        if let Some(host) = &self.git {
            let git = root.sub("git")?;
            if host.join("db").is_dir() {
                git.link(OsStr::new("db"), &host.join("db"))?;
            }
            overlay(&git, "checkouts", &host.join("checkouts"))?;
        }
        root.write("config.toml", self.config.as_bytes())
    }
}

/// `parent/name/<group>/<entry>` for every entry of `host/<group>`: a link
/// to the host's, unless Cargo made one of its own there.
fn overlay(parent: &Dir, name: &str, host: &Path) -> io::Result<()> {
    let dir = parent.sub(name)?;
    let groups = match std::fs::read_dir(host) {
        Ok(groups) => groups,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for group in groups {
        let group = group?;
        if !group.file_type()?.is_dir() {
            continue;
        }
        let private = dir.sub(group.file_name())?;
        for entry in std::fs::read_dir(group.path())? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                private.link(&entry.file_name(), &entry.path())?;
            }
        }
        // A source the host cleaned up since: Cargo extracts it again.
        private.prune()?;
    }
    Ok(())
}

/// A directory of the private home, opened without following symlinks.
struct Dir(File);

fn c_name(name: &OsStr) -> io::Result<CString> {
    if name.is_empty() || name.as_bytes().contains(&b'/') || name == ".." || name == "." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a plain file name"));
    }
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a file name"))
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

impl Dir {
    fn open(path: &Path) -> io::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: path is NUL-terminated; the descriptor is owned by the File.
        Ok(Self(unsafe { File::from_raw_fd(check(libc::open(path.as_ptr(), flags))?) }))
    }

    /// The subdirectory `name`, made private if missing; a symlink is refused.
    fn sub(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        let name = c_name(name.as_ref())?;
        let fd = self.0.as_raw_fd();
        // SAFETY: fd is open and name is NUL-terminated.
        if let Err(e) = check(unsafe { libc::mkdirat(fd, name.as_ptr(), 0o700) }) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: as above; the descriptor is owned by the File.
        Ok(Self(unsafe { File::from_raw_fd(check(libc::openat(fd, name.as_ptr(), flags))?) }))
    }

    /// Where the symlink `name` points; `None` when there is nothing there,
    /// and an `InvalidInput` error when it is not a symlink.
    fn target(&self, name: &CString) -> io::Result<Option<PathBuf>> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: fd is open, name is NUL-terminated, and buf holds buf.len() bytes.
        let n = unsafe { libc::readlinkat(self.0.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ENOENT) => Ok(None),
                Some(libc::EINVAL) => Err(io::Error::new(io::ErrorKind::InvalidInput, "not a symlink")),
                _ => Err(e),
            };
        }
        buf.truncate(n as usize);
        Ok(Some(PathBuf::from(OsString::from_vec(buf))))
    }

    fn unlink(&self, name: &CString) -> io::Result<()> {
        // SAFETY: fd is open and name is NUL-terminated.
        check(unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) }).map(drop)
    }

    /// Make `name` a symlink to `target`, unless it is something else already.
    fn link(&self, name: &OsStr, target: &Path) -> io::Result<()> {
        let name = c_name(name)?;
        match self.target(&name) {
            Ok(Some(current)) if current == target => return Ok(()),
            Ok(Some(_)) => self.unlink(&name)?,
            Ok(None) => {}
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Ok(()),
            Err(e) => return Err(e),
        }
        let target = CString::new(target.as_os_str().as_bytes())?;
        // SAFETY: fd is open and both strings are NUL-terminated.
        match check(unsafe { libc::symlinkat(target.as_ptr(), self.0.as_raw_fd(), name.as_ptr()) }) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        }
    }

    /// Remove the symlinks whose target is gone.
    fn prune(&self) -> io::Result<()> {
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.0.as_raw_fd()))? {
            let name = c_name(&entry?.file_name())?;
            if let Ok(Some(target)) = self.target(&name) {
                if !target.exists() {
                    self.unlink(&name)?;
                }
            }
        }
        Ok(())
    }

    /// Replace the file `name` with `content`, if it holds something else.
    fn write(&self, name: &str, content: &[u8]) -> io::Result<()> {
        use std::io::{Read, Write};
        let fd = self.0.as_raw_fd();
        let open = |name: &CString, flags: libc::c_int| -> io::Result<File> {
            // SAFETY: fd is open, name is NUL-terminated; the descriptor is owned by the File.
            let new =
                check(unsafe { libc::openat(fd, name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600) })?;
            Ok(unsafe { File::from_raw_fd(new) })
        };
        let final_name = c_name(OsStr::new(name))?;
        let mut current = Vec::new();
        if let Ok(file) = open(&final_name, libc::O_RDONLY | libc::O_NONBLOCK) {
            file.take(content.len() as u64 + 1).read_to_end(&mut current)?;
            if current == content {
                return Ok(());
            }
        }
        let temp = c_name(OsStr::new(&format!(".{name}.molt")))?;
        let _ = self.unlink(&temp);
        let mut file = open(&temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        file.write_all(content)?;
        drop(file);
        // SAFETY: fd is open and both names are NUL-terminated.
        check(unsafe { libc::renameat(fd, temp.as_ptr(), fd, final_name.as_ptr()) }).map(drop)
    }
}

/// The part of the host's Cargo config the private home gets: where crates
/// come from, without credentials.
fn config(home: &Path) -> anyhow::Result<String> {
    use toml::{Table, Value};
    // Cargo reads `config` when both exist.
    let mut text = None;
    for name in ["config", "config.toml"] {
        match std::fs::read_to_string(home.join(name)) {
            Ok(found) => {
                text = Some((name, found));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", home.join(name).display())),
        }
    }
    let Some((name, text)) = text else { return Ok(String::new()) };
    let table: Table = text.parse().with_context(|| format!("{} is not valid TOML", home.join(name).display()))?;
    // A URL with a user or password in it carries a credential.
    let plain = |value: &Value| {
        value.as_str().is_none_or(|s| {
            !s.split_once("://").is_some_and(|(_, rest)| rest.split('/').next().is_some_and(|host| host.contains('@')))
        })
    };
    let keep = |entries: Option<&Value>, keys: &[&str]| -> Table {
        let mut kept = Table::new();
        for (name, entry) in entries.and_then(Value::as_table).into_iter().flatten() {
            let Some(entry) = entry.as_table() else { continue };
            let fields: Table = entry
                .iter()
                .filter(|(key, _)| keys.contains(&key.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !fields.is_empty() && fields.values().all(plain) {
                kept.insert(name.clone(), Value::Table(fields));
            }
        }
        kept
    };
    let mut out = Table::new();
    let source = keep(
        table.get("source"),
        &["replace-with", "registry", "local-registry", "directory", "git", "branch", "tag", "rev"],
    );
    let registries = keep(table.get("registries"), &["index", "protocol"]);
    for (key, kept) in [("source", source), ("registries", registries)] {
        if !kept.is_empty() {
            out.insert(key.into(), Value::Table(kept));
        }
    }
    Ok(toml::to_string(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn host(tmp: &Path) -> PathBuf {
        let home = tmp.join("cargo");
        for dir in ["registry/index/r-1", "registry/cache/r-1", "registry/src/r-1/a-1.0.0", "registry/src/r-1/b-1.0.0"]
        {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::write(home.join("credentials.toml"), "[registry]\ntoken = \"secret\"\n").unwrap();
        home
    }

    #[test]
    fn the_private_home_links_the_host_cache_and_keeps_what_cargo_extracted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = host(tmp.path());
        let cache = CargoCache::open(&home).unwrap();
        let registry = home.canonicalize().unwrap().join("registry");
        assert_eq!(cache.mounts().collect::<Vec<_>>(), [registry.as_path()], "git is mounted only when there is one");
        let private = tmp.path().join("private");
        fs::create_dir(&private).unwrap();
        cache.prepare(&private).unwrap();
        assert_eq!(fs::read_link(private.join("registry/index")).unwrap(), registry.join("index"));
        assert_eq!(fs::read_link(private.join("registry/src/r-1/a-1.0.0")).unwrap(), registry.join("src/r-1/a-1.0.0"));
        assert!(!private.join("credentials.toml").exists());
        // What Cargo extracted itself stays; a source the host removed goes.
        fs::create_dir(private.join("registry/src/r-1/c-1.0.0")).unwrap();
        fs::remove_dir(registry.join("src/r-1/b-1.0.0")).unwrap();
        cache.prepare(&private).unwrap();
        assert!(private.join("registry/src/r-1/c-1.0.0").is_dir());
        assert!(fs::symlink_metadata(private.join("registry/src/r-1/b-1.0.0")).is_err());
        // A link a command pointed elsewhere is put back.
        fs::remove_file(private.join("registry/index")).unwrap();
        std::os::unix::fs::symlink("/etc", private.join("registry/index")).unwrap();
        cache.prepare(&private).unwrap();
        assert_eq!(fs::read_link(private.join("registry/index")).unwrap(), registry.join("index"));
    }

    #[test]
    fn a_private_directory_replaced_by_a_symlink_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = CargoCache::open(&host(tmp.path())).unwrap();
        let (private, elsewhere) = (tmp.path().join("private"), tmp.path().join("elsewhere"));
        fs::create_dir_all(private.join("registry")).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, private.join("registry/src")).unwrap();
        assert!(cache.prepare(&private).is_err());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn a_home_without_a_registry_says_to_fetch_first() {
        let tmp = tempfile::tempdir().unwrap();
        let err = CargoCache::open(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("run `cargo fetch` in the project"), "{err:#}");
        assert!(CargoCache::open(Path::new("relative")).is_err());
    }

    #[test]
    fn only_sources_and_registry_urls_without_credentials_are_copied_from_the_host_config() {
        let tmp = tempfile::tempdir().unwrap();
        let home = host(tmp.path());
        fs::write(
            home.join("config.toml"),
            r#"
[source.crates-io]
replace-with = "mirror"
[source.mirror]
registry = "sparse+https://mirror.example/index/"
[source.private]
registry = "https://user:pass@private.example/index"
[registries.work]
index = "sparse+https://work.example/"
token = "secret-token"
credential-provider = "cargo:token"
[http]
proxy = "http://user:pw@proxy.example"
"#,
        )
        .unwrap();
        let config: toml::Table = CargoCache::open(&home).unwrap().config.parse().unwrap();
        assert_eq!(config["source"]["crates-io"]["replace-with"].as_str(), Some("mirror"));
        assert_eq!(config["source"]["mirror"]["registry"].as_str(), Some("sparse+https://mirror.example/index/"));
        assert!(config["source"].get("private").is_none());
        assert_eq!(config["registries"]["work"].as_table().unwrap().keys().collect::<Vec<_>>(), ["index"]);
        assert!(config.get("http").is_none());
    }
}
