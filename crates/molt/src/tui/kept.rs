//! Private metadata for results left in forks. Fork contents stay with the fs service.
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

use super::state::Store;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULTS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub fork: String,
    pub task: String,
    pub outcome: String,
    pub time: u64,
    pub files: Option<usize>,
    pub added: usize,
    pub removed: usize,
}

impl Entry {
    pub fn new(fork: String, task: &str, outcome: &str) -> Self {
        Self {
            fork,
            task: task.to_owned(),
            outcome: outcome.to_owned(),
            time: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            files: None,
            added: 0,
            removed: 0,
        }
    }

    pub fn diff(&mut self, files: usize, patch: &str) {
        self.files = Some(files);
        (self.added, self.removed) = super::text::diff_counts(patch);
    }

    fn private(&mut self, secrets: &[String]) {
        self.task = super::app::scrub(&self.task, secrets);
        if !super::state::keep_history(&self.task, &[]) {
            self.task = "[task text omitted: credential-like or oversized content]".into();
        }
        self.task = super::text::clean_line(&self.task);
    }
}

#[derive(Debug)]
pub enum Change {
    Save(Entry),
    Remove(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    entries: Vec<Entry>,
}

fn read(store: &Store) -> anyhow::Result<Vec<Entry>> {
    let Some(bytes) = store.read("kept.json", MAX_BYTES)? else { return Ok(Vec::new()) };
    let saved: Saved = serde_json::from_slice(&bytes).context("invalid kept-results metadata")?;
    ensure!(saved.version == 1 && saved.entries.len() <= MAX_RESULTS, "unsupported or oversized kept-results metadata");
    ensure!(
        saved.entries.iter().all(|entry| {
            Path::new(&entry.fork).is_absolute()
                && entry.fork.len() <= 4096
                && !entry.fork.contains(['\0', '\n', '\r'])
                && entry.task.len() <= 16 * 1024
                && entry.outcome.len() <= 64
        }),
        "invalid kept-results entry"
    );
    Ok(saved.entries)
}

pub fn load(store: &Store, secrets: &[String]) -> anyhow::Result<Vec<Entry>> {
    let mut entries = read(store)?;
    entries.retain(|entry| {
        std::fs::symlink_metadata(&entry.fork).is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
    });
    for entry in &mut entries {
        entry.private(secrets);
    }
    // Prune missing directories durably without overwriting another interface's changes.
    store.locked("kept.lock", || {
        let mut current = read(store)?;
        current.retain(|entry| {
            std::fs::symlink_metadata(&entry.fork).is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
        });
        write(store, current)
    })?;
    Ok(entries)
}

pub fn persist(store: &Store, change: &Change, secrets: &[String]) -> anyhow::Result<()> {
    store.locked("kept.lock", || {
        let mut entries = read(store)?;
        let fork = match change {
            Change::Save(entry) => &entry.fork,
            Change::Remove(fork) => fork,
        };
        entries.retain(|entry| &entry.fork != fork);
        if let Change::Save(entry) = change {
            let mut entry = entry.clone();
            entry.private(secrets);
            entries.push(entry);
        }
        write(store, entries)
    })
}

fn write(store: &Store, entries: Vec<Entry>) -> anyhow::Result<()> {
    ensure!(entries.len() <= MAX_RESULTS, "more than {MAX_RESULTS} kept results; apply or discard some first");
    let bytes = serde_json::to_vec(&Saved { version: 1, entries })?;
    ensure!(bytes.len() <= MAX_BYTES, "kept-results metadata exceeds its size limit");
    store.replace("kept.json", &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn kept_results_survive_restart_prune_missing_forks_and_keep_workspace_state_private() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let fork = tmp.path().join("fork");
        for path in [&a, &b, &fork] {
            std::fs::create_dir(path).unwrap();
        }
        let store = Store::new(tmp.path(), &a).unwrap();
        let mut entry = Entry::new(fork.to_str().unwrap().into(), "fix login", "passed");
        entry.diff(1, "+a\n-b");
        persist(&store, &Change::Save(entry.clone()), &[]).unwrap();
        let restored = Store::new(tmp.path(), &a).unwrap();
        assert_eq!(load(&restored, &[]).unwrap(), [entry.clone()]);
        assert!(load(&Store::new(tmp.path(), &b).unwrap(), &[]).unwrap().is_empty());
        persist(&store, &Change::Save(Entry { task: "token=do-not-save".into(), ..entry }), &[]).unwrap();
        assert!(!String::from_utf8(store.read("kept.json", MAX_BYTES).unwrap().unwrap())
            .unwrap()
            .contains("do-not-save"));
        std::fs::remove_dir(fork).unwrap();
        assert!(load(&restored, &[]).unwrap().is_empty());
        assert!(read(&store).unwrap().is_empty());
        let files = std::fs::read_dir(tmp.path().join("tui")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::metadata(files.join("kept.json")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
