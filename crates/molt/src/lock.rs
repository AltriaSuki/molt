//! One kernel per data directory: two kernels on one audit log, registry
//! and socket directory would corrupt each other.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{bail, Context};

pub const LOCK_FILE: &str = "kernel.lock";

/// An exclusive `flock` on `<data_dir>/kernel.lock`, held until dropped.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
}

impl DataDirLock {
    /// Take the lock, creating the directory if needed (see
    /// [`create_private_dir`]). Fails at once when another kernel holds it.
    pub fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        create_private_dir(data_dir)?;
        if let Ok(Some(mode)) = open_to_others(data_dir) {
            tracing::warn!(
                "{} is open to other users (mode {mode:o}), and the audit log in it records file contents and \
                 model conversations; make it private with chmod 700",
                data_dir.display()
            );
        }
        let path = data_dir.join(LOCK_FILE);
        // Private, so no one else can take the lock and keep molt from starting.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut pid = String::new();
                let _ = file.read_to_string(&mut pid);
                let holder = match pid.trim() {
                    "" => String::new(),
                    pid => format!(" (pid {pid})"),
                };
                bail!(
                    "another molt kernel{holder} is already running on {}; stop it or use another data dir",
                    data_dir.display()
                );
            }
            Err(TryLockError::Error(e)) => return Err(e).with_context(|| format!("locking {}", path.display())),
        }
        // Only for the message above; the lock itself is what counts.
        let _ = file.set_len(0).and_then(|()| file.write_all(std::process::id().to_string().as_bytes()));
        Ok(Self { _file: file })
    }
}

/// Create `dir`, and any missing parent, with mode 0700. A data dir holds
/// the audit log and forks of the project, which are for this user only. An
/// existing directory keeps its mode.
pub(crate) fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("creating {}", dir.display()))
}

/// The mode of `dir` when its group or others have any access to it.
fn open_to_others(dir: &Path) -> std::io::Result<Option<u32>> {
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    Ok((mode & 0o077 != 0).then_some(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let first = DataDirLock::acquire(&data).unwrap();
        let err = DataDirLock::acquire(&data).unwrap_err().to_string();
        assert!(err.contains("already running"), "{err}");
        assert!(err.contains(&format!("pid {}", std::process::id())), "{err}");
        drop(first);
        DataDirLock::acquire(&data).unwrap();
    }

    #[test]
    fn the_data_dir_and_the_lock_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("cache/molt/data");
        let _lock = DataDirLock::acquire(&data).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data), 0o700);
        assert_eq!(mode(&dir.path().join("cache")), 0o700, "parents molt creates are private too");
        assert_eq!(mode(&data.join(LOCK_FILE)), 0o600);
        assert_eq!(open_to_others(&data).unwrap(), None);

        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(open_to_others(&data).unwrap(), Some(0o755));
        create_private_dir(&data).unwrap();
        assert_eq!(mode(&data), 0o755, "an existing directory is left as it is");
    }
}
