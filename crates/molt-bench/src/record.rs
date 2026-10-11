//! Results: one JSON line per run, appended as each run finishes, so an
//! interrupted benchmark keeps what it measured and can pick up from there.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use molt_api::model::Usage;
use molt_api::planner::Outcome;
use serde::{Deserialize, Serialize};

use crate::agent::ErrorKind;
use crate::grade::Grade;
use crate::task::{Difficulty, Kind, Language, Split};

/// The version of [`Record`]'s format.
pub const FORMAT: u32 = 1;

/// The settings that make results comparable. Every record in one results
/// file has the same.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    /// Identifies the molt executable and the services beside it
    /// ([`crate::agent::build_id`]), so results of different builds are never pooled.
    pub molt_build: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_turns: Option<u32>,
    /// Each run's spending limit.
    pub task_usd: f64,
    /// Each run's time limit.
    pub timeout_s: u64,
    /// Variables set for every run, as `NAME=VALUE`. Variables whose names
    /// look secret are left out, and passwords in URLs are redacted.
    pub env: Vec<String>,
    /// The agent's commands ran in Molt's sandbox. Runs of builds before
    /// the sandbox did not.
    #[serde(default)]
    pub sandboxed: bool,
}

/// One run of one arm on one task, graded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub format: u32,
    /// The `molt bench run` invocation that made it.
    pub run: String,
    pub started_ms: u64,
    pub task: String,
    pub task_hash: String,
    pub language: Language,
    pub kind: Kind,
    pub difficulty: Difficulty,
    pub split: Split,
    pub arm: String,
    pub arm_args: Vec<String>,
    /// Which repetition of this arm on this task, from 0.
    pub trial: u32,
    pub setup: Setup,
    pub molt_version: String,
    /// The commit of the repository the tasks came from, `-dirty` when it had changes.
    pub git_commit: Option<String>,
    /// The hidden tests passed.
    pub passed: bool,
    /// The agent said it was done (see [`crate::agent::AgentRun::claimed_done`]).
    pub claimed_done: bool,
    /// From starting the agent to its exit; grading is not included.
    pub wall_ms: u64,
    pub cost_usd: f64,
    pub reported_cost_usd: Option<f64>,
    pub usage: Usage,
    pub model_calls: u32,
    pub turns: u32,
    pub attempts: u32,
    pub uncounted_calls: u32,
    /// Model calls with no price, whose cost is not in `cost_usd`.
    #[serde(default)]
    pub unpriced_calls: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub outcome: Option<Outcome>,
    pub applied: Option<bool>,
    /// Why the run has no result of the agent's own, when it has none.
    pub error: Option<String>,
    #[serde(default)]
    pub error_kind: Option<ErrorKind>,
    pub grade: Grade,
}

impl Record {
    /// Whether the run's error says nothing about the agent, so it may be
    /// run again ([`ErrorKind::retryable`]).
    pub fn retryable(&self) -> bool {
        self.error.is_some() && self.error_kind.is_some_and(ErrorKind::retryable)
    }
}

/// Read every record in `path`. A last line cut off mid-write is ignored;
/// any other line that does not parse is an error.
pub fn load(path: &Path) -> anyhow::Result<Vec<Record>> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse(path, &bytes)?.0)
}

/// The records; the length of the bytes to keep, which leaves out a last
/// line cut off mid-write (even inside a character); and whether those
/// bytes lack their final newline.
fn parse(path: &Path, bytes: &[u8]) -> anyhow::Result<(Vec<Record>, usize, bool)> {
    let mut records = Vec::new();
    let mut offset = 0;
    for (i, line) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
        let whole = line.ends_with(b"\n");
        if !line.trim_ascii().is_empty() {
            match serde_json::from_slice::<Record>(line) {
                Ok(r) if r.format == FORMAT => records.push(r),
                Ok(r) => {
                    bail!("{}:{}: a record in format {}; this molt reads {FORMAT}", path.display(), i + 1, r.format)
                }
                // Cut off by a crash in the middle of writing it.
                Err(_) if !whole => return Ok((records, offset, false)),
                Err(e) => bail!("{}:{}: {e}", path.display(), i + 1),
            }
        }
        offset += line.len();
    }
    Ok((records, offset, !bytes.is_empty() && !bytes.ends_with(b"\n")))
}

/// A results file open for appending.
pub struct Results {
    path: PathBuf,
    file: fs::File,
}

impl Results {
    /// Open `path`, creating it if needed, and return the records already in
    /// it. A last line cut off mid-write is removed, so new records start on
    /// a line of their own.
    pub fn open(path: &Path) -> anyhow::Result<(Results, Vec<Record>)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let (records, keep, unterminated) = parse(path, &bytes)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        if keep < bytes.len() {
            file.set_len(keep as u64)?;
        }
        if unterminated {
            file.write_all(b"\n")?;
        }
        Ok((Results { path: path.to_path_buf(), file }, records))
    }

    /// Append `record` as one line and flush it to disk.
    pub fn append(&mut self, record: &Record) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).with_context(|| format!("writing {}", self.path.display()))?;
        self.file.sync_data()?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn record(task: &str, arm: &str, trial: u32, passed: bool) -> Record {
        Record {
            format: FORMAT,
            run: "r1".into(),
            started_ms: 1,
            task: task.into(),
            task_hash: "h".into(),
            language: Language::Python,
            kind: Kind::Bugfix,
            difficulty: Difficulty::Medium,
            split: Split::Dev,
            arm: arm.into(),
            arm_args: vec![],
            trial,
            setup: Setup {
                molt_build: "b1".into(),
                model: None,
                effort: None,
                max_turns: None,
                task_usd: 5.0,
                timeout_s: 1800,
                env: vec![],
                sandboxed: true,
            },
            molt_version: "0.1.0".into(),
            git_commit: None,
            passed,
            claimed_done: true,
            wall_ms: 60_000,
            cost_usd: 0.5,
            reported_cost_usd: Some(0.5),
            usage: Usage::default(),
            model_calls: 10,
            turns: 9,
            attempts: 1,
            uncounted_calls: 0,
            unpriced_calls: 0,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            outcome: Some(Outcome::Passed),
            applied: Some(true),
            error: None,
            error_kind: None,
            grade: Grade {
                passed,
                exit_code: Some(if passed { 0 } else { 1 }),
                timed_out: false,
                duration_ms: 5,
                output: String::new(),
            },
        }
    }

    #[test]
    fn records_append_and_a_torn_last_line_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/results.jsonl");
        let (mut results, old) = Results::open(&path).unwrap();
        assert!(old.is_empty());
        results.append(&record("py-a", "molt", 0, true)).unwrap();
        results.append(&record("py-a", "plain", 0, false)).unwrap();
        drop(results);

        // A crash in the middle of a write leaves half a line.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&serde_json::to_string(&record("py-b", "molt", 0, true)).unwrap()[..50]);
        fs::write(&path, &text).unwrap();
        assert_eq!(load(&path).unwrap().len(), 2);

        let (mut results, old) = Results::open(&path).unwrap();
        assert_eq!(old.len(), 2);
        results.append(&record("py-b", "molt", 0, true)).unwrap();
        let all = load(&path).unwrap();
        assert_eq!(
            all.iter().map(|r| (r.task.as_str(), r.arm.as_str())).collect::<Vec<_>>(),
            [("py-a", "molt"), ("py-a", "plain"), ("py-b", "molt")]
        );
    }

    #[test]
    fn a_line_torn_inside_a_character_is_dropped_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let mut r = record("py-a", "molt", 0, true);
        r.grade.output = "café ✓".repeat(10);
        let line = serde_json::to_string(&r).unwrap();
        let cut = line.find('é').unwrap() + 1;
        let mut bytes = format!("{line}\n").into_bytes();
        bytes.extend_from_slice(&line.as_bytes()[..cut]);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(load(&path).unwrap().len(), 1);
        let (mut results, old) = Results::open(&path).unwrap();
        assert_eq!(old.len(), 1);
        results.append(&r).unwrap();
        assert_eq!(load(&path).unwrap().len(), 2);
    }

    #[test]
    fn a_whole_last_record_without_a_newline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        fs::write(&path, serde_json::to_string(&record("py-a", "molt", 0, true)).unwrap()).unwrap();
        let (mut results, old) = Results::open(&path).unwrap();
        assert_eq!(old.len(), 1);
        results.append(&record("py-a", "plain", 0, true)).unwrap();
        assert_eq!(load(&path).unwrap().len(), 2);
    }

    #[test]
    fn a_bad_line_in_the_middle_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let good = serde_json::to_string(&record("py-a", "molt", 0, true)).unwrap();
        fs::write(&path, format!("{good}\nnot json\n{good}\n")).unwrap();
        assert!(load(&path).unwrap_err().to_string().contains(":2:"));
        assert!(Results::open(&path).is_err());
    }
}
