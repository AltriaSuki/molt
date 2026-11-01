//! The interface's conversation, saved as it goes, so that an interface that
//! stopped without finishing (quit with a task in flight, a crash, the
//! process killed) can be picked up again: the summaries later tasks are
//! told about, what the session spent, the task that was in flight and a
//! result that waited for the user.
//!
//! One private file per workspace, replaced whole whenever it changes. A
//! quit with nothing in flight removes it, so the next interface starts as
//! a fresh one. The attempts' own work is the planner's checkpoint (see
//! `molt_planner::checkpoint`): this file only names the run.
use std::collections::BTreeMap;

use anyhow::{ensure, Context};
use molt_api::planner::Earlier;
use serde::{Deserialize, Serialize};

use super::kept;
use super::state::Store;

const FILE: &str = "session.json";
const LOCK: &str = "session.lock";
const VERSION: u32 = 1;
const MAX_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    pub version: u32,
    /// The finished tasks of the conversation, oldest first.
    pub earlier: Vec<Earlier>,
    pub spent_usd: f64,
    pub uncounted_calls: u32,
    pub uncounted_stops: u32,
    /// What the session total already counts of runs that stopped with a
    /// checkpoint: their cost and uncounted calls when their reply came.
    #[serde(default)]
    pub settled_runs: BTreeMap<String, (f64, u32)>,
    /// The task in flight.
    pub active: Option<Active>,
    /// A result that waited for the user's decision.
    pub pending: Option<kept::Entry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Active {
    /// The run's id: its trace, and the name of its checkpoint.
    pub run: String,
    pub task: String,
    pub phase: Phase,
    /// The latest version of its requirements the interface saw.
    pub version: u32,
    /// The planner keeps a checkpoint of it.
    pub checkpointed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Designing,
    Confirming,
    Running,
    Deciding,
    Applying,
}

/// The saved session of this workspace, if there is one.
pub fn load(store: &Store) -> anyhow::Result<Option<Saved>> {
    let Some(bytes) = store.read(FILE, MAX_BYTES)? else { return Ok(None) };
    let saved: Saved = serde_json::from_slice(&bytes).context("it is damaged")?;
    ensure!(saved.version == VERSION, "it is in format {}, and this interface reads format {VERSION}", saved.version);
    ensure!(saved.spent_usd.is_finite() && saved.spent_usd >= 0.0, "its spend is not a cost");
    Ok(Some(saved))
}

/// The saved session of `workspace` under `data`, for a server session to
/// take over (RFC 0006). The file stays: this interface still reads it.
pub(crate) fn load_for(data: &std::path::Path, workspace: &std::path::Path) -> anyhow::Result<Option<Saved>> {
    load(&Store::new(data, workspace)?)
}

pub fn save(store: &Store, saved: &Saved) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&Saved { version: VERSION, ..saved.clone() })?;
    ensure!(bytes.len() <= MAX_BYTES, "the saved session exceeds its size limit; /new starts a smaller one");
    store.locked(LOCK, || store.replace(FILE, &bytes))
}

pub fn remove(store: &Store) -> anyhow::Result<()> {
    store.locked(LOCK, || store.remove(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_api::planner::Outcome;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_session_survives_a_restart_privately_and_damage_is_refused_with_why() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path(), tmp.path()).unwrap();
        assert_eq!(load(&store).unwrap(), None);
        let saved = Saved {
            earlier: vec![Earlier {
                task: "fix parser".into(),
                outcome: Outcome::Passed,
                summary: "Parsed.".into(),
                changes: vec!["src/lib.rs".into()],
                applied: true,
            }],
            spent_usd: 0.5,
            active: Some(Active {
                run: "trace_1".into(),
                task: "add tests".into(),
                phase: Phase::Running,
                version: 2,
                checkpointed: true,
            }),
            ..Saved::default()
        };
        save(&store, &saved).unwrap();
        let back = load(&Store::new(tmp.path(), tmp.path()).unwrap()).unwrap().unwrap();
        assert_eq!(back, Saved { version: VERSION, ..saved });
        let file = std::fs::read_dir(tmp.path().join("tui")).unwrap().next().unwrap().unwrap().path().join(FILE);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);

        std::fs::write(&file, "{\"version\":1,").unwrap();
        assert!(format!("{:#}", load(&store).unwrap_err()).contains("damaged"));
        std::fs::write(&file, serde_json::to_vec(&Saved { version: 9, ..Saved::default() }).unwrap()).unwrap();
        assert!(load(&store).unwrap_err().to_string().contains("format 9"));
        remove(&store).unwrap();
        assert_eq!(load(&store).unwrap(), None);
    }
}
