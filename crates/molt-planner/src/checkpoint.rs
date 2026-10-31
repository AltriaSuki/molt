//! Checkpoints: what a run has done so far, saved as it goes so that a run
//! stopped before it finished can go on (`resume` in a [`RunRequest`]).
//!
//! One file per run, `<state>/<run>.json`, replaced whole and atomically at
//! every boundary: a model call starting or answered, an attempt's tool
//! calls about to run or their results in, a check run, an attempt ending.
//! It holds the request, the check, each attempt's fork and conversation,
//! and the spend so far, with the model calls not yet answered, whose cost
//! is unknown if the run stops there.
//!
//! A run that finishes removes its checkpoint. One that stops keeps it, and
//! its forks; the oldest checkpoints beyond [`KEEP`] are removed, forks
//! and all, when a new checkpointed run starts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use molt_api::model::Usage;
use molt_api::planner::{AttemptStatus, CheckDispute, CheckFile, Criterion, Finding, Requirement, RunRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The checkpoint format this planner reads and writes.
pub const FORMAT: u32 = 1;
/// Checkpoints kept in a state dir.
pub const KEEP: usize = 10;

/// A run's checkpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Saved {
    pub format: u32,
    pub run: String,
    /// When it was last saved, in milliseconds since the Unix epoch.
    pub saved_ms: u64,
    /// The planner version that saved it.
    pub planner: String,
    pub request: RunRequest,
    /// The check is settled (given, designed, or none applies), so a resume
    /// does not design it again.
    #[serde(default)]
    pub settled: bool,
    #[serde(default)]
    pub check: Option<SavedCheck>,
    /// Without a check: the criteria the run is judged by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rubric: Vec<Criterion>,
    #[serde(default)]
    pub spend: SavedSpend,
    /// Model calls started and not yet answered: if the run stopped here,
    /// their cost is unknown.
    #[serde(default)]
    pub open_calls: u32,
    #[serde(default)]
    pub attempts: Vec<SavedAttempt>,
    /// What was added to the run while it went.
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    /// The escalations an adaptive run made, which count against its cap
    /// after a resume too.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub escalations: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedCheck {
    pub command: String,
    #[serde(default)]
    pub files: Vec<CheckFile>,
    #[serde(default)]
    pub designed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
}

/// The run's spend so far, every resume of it included.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedSpend {
    pub usage: Usage,
    pub cost_usd: f64,
    /// Calls whose cost is not counted: unknown prices, failures without
    /// usage, and calls a stop left unanswered.
    #[serde(default)]
    pub uncounted_calls: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedAttempt {
    pub index: u32,
    #[serde(default)]
    pub fork: Option<String>,
    /// It was offered memory's tools.
    #[serde(default)]
    pub memory_tools: bool,
    /// It was offered `fetch_url`.
    #[serde(default)]
    pub web_tools: bool,
    /// It was offered `load_skill`.
    #[serde(default)]
    pub skill_tools: bool,
    /// Its conversation, the first message included.
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub known: Option<(usize, u64)>,
    /// What its conversation keeps of the turns it left out to fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<Box<crate::compact::Compacted>>,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub check_runs: u32,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub cost_usd: f64,
    /// How it ended; `None` while it can go on.
    #[serde(default)]
    pub end: Option<SavedEnd>,
    /// The version of the run's requirements it works to.
    #[serde(default)]
    pub requirements: u32,
    /// The instruction files of directories it reached that it was given.
    #[serde(default, skip_serializing_if = "crate::scoped::Saved::is_empty")]
    pub dir_instructions: crate::scoped::Saved,
    /// The model an adaptive run gave it, which it keeps after a resume;
    /// `None`: the run's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedEnd {
    pub status: AttemptStatus,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub summary: String,
    /// How it did on the run's rubric, when it was judged and met it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// It stopped because it disputed the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispute: Option<CheckDispute>,
    /// What kind of failure ended it, when the planner could tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<molt_api::planner::Failure>,
}

impl Saved {
    pub fn new(run: &str, request: RunRequest) -> Self {
        Self {
            format: FORMAT,
            run: run.to_owned(),
            saved_ms: now_ms(),
            planner: env!("CARGO_PKG_VERSION").to_owned(),
            request,
            settled: false,
            check: None,
            rubric: Vec::new(),
            spend: SavedSpend::default(),
            open_calls: 0,
            attempts: Vec::new(),
            requirements: Vec::new(),
            escalations: 0,
        }
    }

    /// The attempt `index`, added when it is new.
    pub fn attempt(&mut self, index: u32) -> &mut SavedAttempt {
        let at = match self.attempts.iter().position(|a| a.index == index) {
            Some(at) => at,
            None => {
                self.attempts.push(SavedAttempt {
                    index,
                    fork: None,
                    memory_tools: false,
                    web_tools: false,
                    skill_tools: false,
                    messages: Vec::new(),
                    known: None,
                    compacted: None,
                    turns: 0,
                    check_runs: 0,
                    usage: Usage::default(),
                    cost_usd: 0.0,
                    end: None,
                    requirements: 0,
                    dir_instructions: Default::default(),
                    model: None,
                });
                self.attempts.len() - 1
            }
        };
        &mut self.attempts[at]
    }

    /// Every fork the run made that is still around.
    pub fn forks(&self) -> Vec<String> {
        self.attempts.iter().filter_map(|a| a.fork.clone()).collect()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The file of run `run` in `dir`. Run ids are the planner's trace ids;
/// anything else could name a path outside `dir`.
pub fn path(dir: &Path, run: &str) -> Result<PathBuf, String> {
    let plain =
        !run.is_empty() && run.len() <= 128 && run.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !plain {
        return Err(format!("{run:?} is not a run id"));
    }
    Ok(dir.join(format!("{run}.json")))
}

/// The checkpoint of run `run` in `dir`, or why it cannot be used.
pub fn load(dir: &Path, run: &str) -> Result<Saved, String> {
    let path = path(dir, run)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("there is no checkpoint of run {run} (it finished, or never saved one)"))
        }
        Err(e) => return Err(format!("could not read the checkpoint {}: {e}", path.display())),
    };
    parse(&bytes, run).map_err(|e| format!("the checkpoint {} cannot be used: {e}", path.display()))
}

fn parse(bytes: &[u8], run: &str) -> Result<Saved, String> {
    #[derive(Deserialize)]
    struct Head {
        format: u32,
    }
    let head: Head = serde_json::from_slice(bytes).map_err(|e| format!("it is damaged ({e})"))?;
    if head.format != FORMAT {
        return Err(format!("it is in format {}, and this planner reads format {FORMAT}", head.format));
    }
    let saved: Saved = serde_json::from_slice(bytes).map_err(|e| format!("it is damaged ({e})"))?;
    if saved.run != run {
        return Err(format!("it is the checkpoint of run {}, not {run}", saved.run));
    }
    Ok(saved)
}

/// Every checkpoint in `dir`, newest first; one that cannot be read comes
/// with why.
pub fn list(dir: &Path) -> Vec<(String, Result<Saved, String>)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, Result<Saved, String>)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_owned))
        .map(|run| {
            let saved = load(dir, &run);
            (run, saved)
        })
        .collect();
    out.sort_by_key(|(_, saved)| std::cmp::Reverse(saved.as_ref().map_or(0, |s| s.saved_ms)));
    out
}

/// A run's checkpoint as it is saved: every change is written through.
pub(crate) struct Store {
    path: PathBuf,
    saved: Mutex<Saved>,
}

impl Store {
    /// Save `saved` in `dir` as a new checkpoint, or replace the one there.
    pub fn create(dir: &Path, saved: Saved) -> Result<Self, String> {
        let mut dirs = std::fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
        dirs.create(dir).map_err(|e| format!("could not create the checkpoint dir {}: {e}", dir.display()))?;
        let store = Self { path: path(dir, &saved.run)?, saved: Mutex::new(saved) };
        store.update(|_| {})?;
        Ok(store)
    }

    /// Change the checkpoint and save it. The lock is held while writing,
    /// so saves land in the order of their changes.
    pub fn update(&self, change: impl FnOnce(&mut Saved)) -> Result<(), String> {
        let mut saved = self.saved.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut saved);
        saved.saved_ms = now_ms();
        let bytes = serde_json::to_vec(&*saved).map_err(|e| e.to_string())?;
        write_atomic(&self.path, &bytes)
            .map_err(|e| format!("could not save the checkpoint {}: {e}", self.path.display()))
    }

    /// The run finished: nothing is left to resume.
    pub fn remove(&self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), error = %e, "could not remove a checkpoint");
            }
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // The rename is durable once the directory is.
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// The checkpoints in `dir` beyond the newest `keep`, oldest first, to be
/// removed along with their forks.
pub(crate) fn beyond(dir: &Path, keep: usize) -> Vec<(PathBuf, Vec<String>)> {
    let all = list(dir);
    all.into_iter()
        .skip(keep)
        .filter_map(|(run, saved)| Some((path(dir, &run).ok()?, saved.map(|s| s.forks()).unwrap_or_default())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkpoint_is_saved_whole_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::create(dir.path(), Saved::new("trace_1", RunRequest::new("t", "/w"))).unwrap();
        store.update(|s| s.attempt(1).turns = 3).unwrap();
        let saved = load(dir.path(), "trace_1").unwrap();
        assert_eq!((saved.attempts.len(), saved.attempts[0].turns), (1, 3));
        assert!(!dir.path().join("trace_1.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("trace_1.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.remove();
        assert!(load(dir.path(), "trace_1").unwrap_err().contains("no checkpoint"));
    }

    #[test]
    fn damaged_foreign_and_newer_checkpoints_are_refused_with_why() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace_a.json"), "{\"format\":1,").unwrap();
        assert!(load(dir.path(), "trace_a").unwrap_err().contains("damaged"));
        std::fs::write(dir.path().join("trace_b.json"), "{\"format\":9}").unwrap();
        assert!(load(dir.path(), "trace_b").unwrap_err().contains("format 9"));
        let other = serde_json::to_vec(&Saved::new("trace_x", RunRequest::new("t", "/w"))).unwrap();
        std::fs::write(dir.path().join("trace_c.json"), other).unwrap();
        assert!(load(dir.path(), "trace_c").unwrap_err().contains("trace_x"));
        assert!(load(dir.path(), "../etc/passwd").unwrap_err().contains("not a run id"));
        assert_eq!(list(dir.path()).len(), 3);
    }
}
