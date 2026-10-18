//! Bounded, private interface state, separated even when workspaces share a data directory.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use sha2::{Digest, Sha256};

pub const HISTORY_LIMIT: usize = 1000;
const ENTRY_BYTES: usize = 16 * 1024;
const HISTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum HistoryChange {
    Append(String),
    Clear,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(data: &Path, workspace: &Path) -> anyhow::Result<Self> {
        let workspace = workspace.canonicalize().context("resolving the workspace for interface state")?;
        let key = hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()));
        let mut dir = data.to_path_buf();
        for child in ["tui", key.as_str()] {
            dir.push(child);
            match std::fs::create_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            let meta = std::fs::symlink_metadata(&dir)?;
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "interface state directory is not a real directory"
            );
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { dir })
    }

    pub fn history(&self, secrets: &[String]) -> anyhow::Result<(Vec<String>, bool)> {
        let Some(bytes) = self.read("history.jsonl", HISTORY_BYTES)? else { return Ok((Vec::new(), false)) };
        let mut entries = Vec::new();
        let mut damaged = false;
        for line in bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            match serde_json::from_slice::<String>(line) {
                Ok(entry) if keep_history(&entry, secrets) => entries.push(entry),
                Ok(_) => {}
                Err(_) => damaged = true,
            }
        }
        trim_history(&mut entries);
        Ok((entries, damaged))
    }

    pub fn change_history(&self, change: &HistoryChange, secrets: &[String]) -> anyhow::Result<()> {
        let lock = self.open("history.lock", true)?;
        // Serialize append/compaction across interfaces sharing this workspace.
        // SAFETY: this process owns the regular file descriptor for the lock.
        ensure!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0, "could not lock input history");
        let HistoryChange::Append(entry) = change else { return self.replace("history.jsonl", &[]) };
        if !keep_history(entry, secrets) {
            return Ok(());
        }
        let (mut entries, damaged) = self.history(secrets)?;
        if entries.last() == Some(entry) {
            return Ok(());
        }
        entries.push(entry.clone());
        let pruned = trim_history(&mut entries);
        let encoded = serde_json::to_vec(entry)?;
        let size = self.dir.join("history.jsonl").symlink_metadata().map_or(0, |m| m.len());
        if damaged || pruned || size + encoded.len() as u64 + 1 > HISTORY_BYTES as u64 {
            let mut bytes = Vec::new();
            for entry in &entries {
                serde_json::to_writer(&mut bytes, entry)?;
                bytes.push(b'\n');
            }
            self.replace("history.jsonl", &bytes)
        } else {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(self.dir.join("history.jsonl"))?;
            ensure!(file.metadata()?.is_file(), "input history is not a regular file");
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&encoded)?;
            file.write_all(b"\n")?;
            file.sync_data()?;
            Ok(())
        }
    }

    fn open(&self, name: &str, create: bool) -> anyhow::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(create)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.dir.join(name))?;
        ensure!(file.metadata()?.is_file(), "interface state is not a regular file");
        Ok(file)
    }

    pub fn read(&self, name: &str, limit: usize) -> anyhow::Result<Option<Vec<u8>>> {
        let file = match self.open(name, false) {
            Ok(file) => file,
            Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "saved interface state exceeds its size limit");
        Ok(Some(bytes))
    }

    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        match std::fs::remove_file(self.dir.join(name)) {
            Ok(()) => File::open(&self.dir)?.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    pub fn replace(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join(name))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

/// Known environment credentials use the same scrubber as tool output. Also
/// exclude common credential assignments, token prefixes and private keys.
pub fn keep_history(text: &str, secrets: &[String]) -> bool {
    if text.trim().is_empty()
        || text.len() > ENTRY_BYTES
        || text.contains(['\0', '\x1b'])
        || super::app::scrub(text, secrets) != text
    {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("private key-----") {
        return false;
    }
    let compact: String = lower.chars().filter(|c| !c.is_whitespace() && !['\'', '"'].contains(c)).collect();
    if ["api_key", "api-key", "apikey", "password", "secret", "token", "authorization"]
        .iter()
        .any(|key| compact.contains(&format!("{key}=")) || compact.contains(&format!("{key}:")))
    {
        return false;
    }
    !text.split(|c: char| c.is_whitespace() || "\"'`=,:;()[]{}".contains(c)).any(|word| {
        word.len() >= 12
            && (["sk-", "ghp_", "gho_", "github_pat_", "xoxb-", "xoxp-"].iter().any(|p| word.starts_with(p))
                || (word.starts_with("AKIA") && word.len() >= 20))
    })
}

pub(super) fn trim_history(entries: &mut Vec<String>) -> bool {
    let before = entries.len();
    let mut total = 0;
    let keep = entries
        .iter()
        .rev()
        .take(HISTORY_LIMIT)
        .take_while(|entry| {
            // JSON escaping can expand each byte up to six bytes.
            total += serde_json::to_vec(entry).map_or(HISTORY_BYTES, |v| v.len() + 1);
            total <= HISTORY_BYTES
        })
        .count();
    entries.drain(..entries.len().saturating_sub(keep));
    entries.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_survives_restart_is_private_and_is_separate_for_each_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let store = Store::new(tmp.path(), &a).unwrap();
        for entry in ["first\nsecond line", "second", "second"] {
            store.change_history(&HistoryChange::Append(entry.into()), &[]).unwrap();
        }
        let restored = Store::new(tmp.path(), &a).unwrap();
        assert_eq!(restored.history(&[]).unwrap().0, ["first\nsecond line", "second"]);
        assert!(Store::new(tmp.path(), &b).unwrap().history(&[]).unwrap().0.is_empty());
        assert_eq!(std::fs::metadata(store.dir.join("history.jsonl")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&store.dir).unwrap().permissions().mode() & 0o777, 0o700);
        restored.change_history(&HistoryChange::Clear, &[]).unwrap();
        assert!(store.history(&[]).unwrap().0.is_empty());
    }
    #[test]
    fn history_filters_credentials_and_recovers_valid_records_from_damage() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path(), tmp.path()).unwrap();
        let secrets = vec!["known-sensitive-value".into()];
        for entry in [
            "use known-sensitive-value",
            "token = hidden",
            "sk-ant-unknown-value",
            "-----BEGIN PRIVATE KEY-----",
            "safe task",
        ] {
            store.change_history(&HistoryChange::Append(entry.into()), &secrets).unwrap();
        }
        assert_eq!(store.history(&secrets).unwrap().0, ["safe task"]);
        store.replace("history.jsonl", b"\"good\"\nbroken\n\"last\"\n\"torn").unwrap();
        assert_eq!(store.history(&[]).unwrap(), (vec!["good".into(), "last".into()], true));
        store.change_history(&HistoryChange::Append("next".into()), &[]).unwrap();
        assert!(!store.history(&[]).unwrap().1);
        assert!(!keep_history(&"a".repeat(ENTRY_BYTES + 1), &[]));
    }
    #[test]
    fn history_compacts_and_two_interfaces_do_not_lose_each_others_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Store::new(tmp.path(), tmp.path()).unwrap();
        let b = Store::new(tmp.path(), tmp.path()).unwrap();
        let lines: String = (0..HISTORY_LIMIT).map(|i| format!("\"task {i}\"\n")).collect();
        a.replace("history.jsonl", lines.as_bytes()).unwrap();
        a.change_history(&HistoryChange::Append("new a".into()), &[]).unwrap();
        b.change_history(&HistoryChange::Append("new b".into()), &[]).unwrap();
        let entries = a.history(&[]).unwrap().0;
        assert_eq!(entries.len(), HISTORY_LIMIT);
        assert_eq!(entries[0], "task 2");
        assert_eq!(&entries[HISTORY_LIMIT - 2..], ["new a", "new b"]);
    }
    #[test]
    fn symlinks_cannot_redirect_history_reads_or_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path(), tmp.path()).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, "leave this").unwrap();
        std::os::unix::fs::symlink(&outside, store.dir.join("history.jsonl")).unwrap();
        assert!(store.history(&[]).is_err());
        assert!(store.change_history(&HistoryChange::Append("task".into()), &[]).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "leave this");
    }
}
