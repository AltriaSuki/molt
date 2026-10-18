//! A private, writable place for the build output of a workspace's commands.
//!
//! A fork links ignored build directories such as `target/` to the user's
//! originals, which are read-only in the sandbox, and a build into a new
//! directory inside the fork would end up in the result. So every command
//! gets `CARGO_TARGET_DIR` and `GOCACHE` in `<scratch>/.molt/build/<key>`,
//! keyed by the original workspace, so later forks and runs of the same
//! workspace reuse what was built. Nothing in `.molt` can be named as a
//! workspace, so the file tools never reach it.
//!
//! Cargo's directory is one of a few slots, each held by one command at a
//! time: two forks building the same package into one directory would replace
//! each other's binaries while their tests run. Go's cache is safe to share.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use molt_proto::RemoteError;
use sha2::{Digest, Sha256};

use crate::error::failed;
use crate::{fork, Roots};

/// More than the shell service runs at once, so a command finds a free one.
const SLOTS: usize = 16;
/// How many forks of forks are followed back to the original workspace.
const MAX_FORK_DEPTH: usize = 8;

/// The private build directories one command uses, held until it is dropped.
pub(crate) struct BuildDir {
    cargo: PathBuf,
    go: PathBuf,
    _slot: File,
}

impl BuildDir {
    /// The directories to make writable, each mounted at its own path.
    pub(crate) fn mounts(&self) -> [&Path; 2] {
        [&self.cargo, &self.go]
    }

    /// The variables that send build output there.
    pub(crate) fn env(&self) -> [(&'static str, OsString); 3] {
        [
            ("CARGO_TARGET_DIR", self.cargo.clone().into_os_string()),
            ("GOCACHE", self.go.clone().into_os_string()),
            ("PYTHONDONTWRITEBYTECODE", "1".into()),
        ]
    }
}

/// The private build directories for a command in the canonical workspace `ws`.
pub(crate) fn acquire(roots: &Roots, ws: &Path) -> Result<BuildDir, RemoteError> {
    let error = |e: io::Error| failed(format!("private build directory: {e}"));
    let key = hex::encode(&Sha256::digest(origin(roots, ws).as_os_str().as_encoded_bytes())[..8]);
    let data = roots.scratch.join(".molt");
    let dir = data.join("build").join(key);
    for path in [&data, &data.join("build"), &dir] {
        private_dir(path).map_err(error)?;
    }
    let go = dir.join("go");
    private_dir(&go).map_err(error)?;
    for n in 0..SLOTS {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(format!("cargo-{n}.lock")))
            .map_err(error)?;
        // SAFETY: lock owns a live file descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                continue;
            }
            return Err(error(e));
        }
        let cargo = dir.join(format!("cargo-{n}"));
        private_dir(&cargo).map_err(error)?;
        return Ok(BuildDir { cargo, go, _slot: lock });
    }
    Err(failed(format!(
        "every private build directory of this workspace is in use ({SLOTS} commands are building); retry when one finishes"
    )))
}

/// The workspace a fork, or a fork of a fork, was copied from; `ws` itself
/// when it is not a fork.
fn origin(roots: &Roots, ws: &Path) -> PathBuf {
    let mut path = ws.to_path_buf();
    for _ in 0..MAX_FORK_DEPTH {
        match fork::base(roots, &path.to_string_lossy()) {
            Some(base) if base != path => path = base,
            _ => break,
        }
    }
    path
}

/// Create `path` with mode 0700 if missing; refuse one that is a symlink,
/// another user's, or open to others.
fn private_dir(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} must be a private directory of this user, reached without symlinks",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(tmp: &Path) -> Roots {
        let root = tmp.join("project");
        fs::create_dir_all(&root).unwrap();
        Roots { root, scratch: tmp.join("scratch") }.canonical().unwrap()
    }

    #[test]
    fn commands_at_once_get_separate_cargo_directories_and_reuse_them_later() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        let first = acquire(&roots, &roots.root).unwrap();
        let second = acquire(&roots, &roots.root).unwrap();
        assert_ne!(first.cargo, second.cargo);
        assert_eq!(first.go, second.go);
        assert!(first.cargo.starts_with(roots.scratch.join(".molt/build")));
        let (cargo, go) = (first.cargo.clone(), first.go.clone());
        drop(first);
        let again = acquire(&roots, &roots.root).unwrap();
        assert_eq!((again.cargo.clone(), again.go.clone()), (cargo, go));
        assert_eq!(fs::metadata(&again.cargo).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_build_directory_reached_through_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::create_dir(roots.scratch.join(".molt")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, roots.scratch.join(".molt/build")).unwrap();
        let err = acquire(&roots, &roots.root).err().unwrap();
        assert!(err.message.contains("private build directory"), "{}", err.message);
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }
}
