//! One kernel per data directory: two kernels on one audit log, registry
//! and socket directory would corrupt each other.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context};

pub const LOCK_FILE: &str = "kernel.lock";

/// An exclusive `flock` on `<data_dir>/kernel.lock`, held until dropped.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
}

impl DataDirLock {
    /// Take the lock, creating the directory if needed. Fails at once when
    /// another kernel holds it.
    pub fn acquire(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let path = data_dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
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
}
