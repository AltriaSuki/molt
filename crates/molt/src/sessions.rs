//! The work sessions `molt serve` keeps (RFC 0006): a conversation that
//! outlives the client that had it open and the process that served it.
//!
//! Each session is a directory under `<data_dir>/sessions`, named by its
//! id, with an append-only journal: one JSON entry a line, each written and
//! flushed to disk before the server answers the request it records. What
//! the session holds now (the tasks and their replies, the questions not
//! answered, the results not decided on, what it spent) is the journal read
//! from the start. Nothing is ever rewritten; a line cut off by a crash is
//! one that was never confirmed, and is left out.
//!
//! The attempts' own work stays where it is: a run's checkpoint is the
//! planner's (`molt_planner::checkpoint`), its calls are in the audit log
//! under its trace, and a result is in its fork. The journal names them.

use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context};
use molt_api::planner::{Earlier, RunResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The journal format this build writes, and the newest it reads.
pub const FORMAT: u32 = 1;
const DIR: &str = "sessions";
const JOURNAL: &str = "journal.jsonl";
const LOCK: &str = "lock";
/// The longest entry. A reply's conversation that would make it longer is
/// left out, and the entry says so.
const MAX_LINE: usize = 8 << 20;
/// The largest journal read.
const MAX_JOURNAL: u64 = 512 << 20;

/// One line of the journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The journal format the line was written in.
    pub v: u32,
    /// Its place in the journal, from 1.
    pub seq: u64,
    /// When it was written, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    #[serde(flatten)]
    pub entry: Entry,
}

/// What happened in a session, as the journal has it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// The first entry of every journal.
    Created {
        workspace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// What an interface's saved session held, taken over at the start.
    Imported(Imported),
    /// A task the server accepted, written before the planner is asked: the
    /// person's message, and the run that answers it.
    Task {
        run: String,
        task: String,
    },
    /// The planner's reply to the task of `run`.
    Reply(Box<Reply>),
    /// The call for the task of `run` failed: what it did and spent is unknown.
    Failed {
        run: String,
        error: String,
    },
    /// An attempt of `run` asked the person something.
    Asked {
        run: String,
        question: u32,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
    /// The person's answer to question `question` of `run`.
    Answered {
        run: String,
        question: u32,
        text: String,
    },
    /// What the person did with the result kept in `fork`.
    Decided {
        fork: String,
        decision: Decision,
        /// The paths applied; empty: all of it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    Archived,
    Restored,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub run: String,
    /// The task, its outcome, its summary and its changes, as the next task
    /// is told of it.
    pub earlier: Earlier,
    /// Where the result is kept, when it was not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
    /// The run stopped with a checkpoint under this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// The known part of what it cost.
    pub cost_usd: f64,
    /// Calls whose cost is not known: `cost_usd` is a subtotal when this is not zero.
    #[serde(default)]
    pub uncounted_calls: u32,
    /// The winning attempt's conversation, tool calls and their results
    /// included, as the planner gave it for a task that goes on from it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation: Vec<Value>,
    /// The conversation was too large for an entry and is not here; the
    /// run's calls are still in the audit log under its trace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conversation_left_out: bool,
}

impl Reply {
    /// The entry for `resp`, the reply to `task` of `run`.
    pub fn of(run: &str, task: &str, resp: &RunResponse) -> Self {
        Self {
            run: run.to_owned(),
            earlier: Earlier {
                task: task.to_owned(),
                outcome: resp.outcome,
                summary: resp.summary.clone(),
                changes: resp.changes.iter().map(|c| c.path.clone()).collect(),
                applied: resp.applied,
            },
            fork: resp.fork.clone(),
            checkpoint: resp.checkpoint.clone(),
            cost_usd: resp.cost_usd,
            uncounted_calls: resp.uncounted_calls,
            conversation: resp.conversation.clone(),
            conversation_left_out: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Applied,
    Kept,
    Discarded,
}

/// What the interface's `session.json` (format 1) held. It saved the
/// earlier tasks as summaries only, so that is all there is of them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Imported {
    /// Where it came from, as a person reads it.
    pub from: String,
    #[serde(default)]
    pub earlier: Vec<Earlier>,
    #[serde(default)]
    pub spent_usd: f64,
    #[serde(default)]
    pub uncounted_calls: u32,
    /// Tasks that stopped without saying what they spent.
    #[serde(default)]
    pub uncounted_stops: u32,
    /// The task that was in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ImportedTask>,
    /// The fork of a result that waited for the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImportedTask {
    pub run: String,
    pub task: String,
    /// The planner kept a checkpoint of it: `runs.list` has it.
    pub checkpointed: bool,
}

/// A session as its journal has it now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Loaded {
    pub id: String,
    pub workspace: String,
    #[serde(default)]
    pub title: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub archived: bool,
    /// Every entry, oldest first: the whole history.
    pub entries: Vec<Record>,
    /// The tasks that got a reply, as the next task is told of them.
    pub earlier: Vec<Earlier>,
    /// Tasks accepted whose reply was never recorded: running now, or cut
    /// off by a stop or a crash. What they did and spent is unknown, and
    /// nothing runs them again; one with a checkpoint is in `runs.list`.
    pub unanswered: Vec<Unanswered>,
    /// Questions of unanswered tasks that the person has not answered.
    pub questions: Vec<Question>,
    /// Results kept in a fork that the person has not decided on yet.
    pub results: Vec<Pending>,
    pub spent: Spent,
    /// What to know about what was loaded: damage left out, forks gone, what an import could not bring.
    pub notes: Vec<String>,
    /// Each result's fork, and the place in `earlier` of the task it answered.
    #[serde(skip)]
    result_of: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unanswered {
    pub run: String,
    pub task: String,
    /// The planner kept a checkpoint of it, as far as the session knows.
    #[serde(default)]
    pub checkpointed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub run: String,
    pub question: u32,
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// The run whose result it is; empty for one an import brought.
    pub run: String,
    pub fork: String,
    /// The fork is no longer there.
    #[serde(default)]
    pub gone: bool,
}

/// What the session spent. Never less than it was: a cost not known is
/// counted as not known, not as nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spent {
    /// The sum of the costs that are known.
    pub known_usd: f64,
    /// Calls of answered tasks whose cost is not known.
    pub uncounted_calls: u32,
    /// Tasks that never said what they spent: failed, cut off or running.
    /// A reply settles its task; a failure does not.
    pub unsettled_runs: u32,
}

impl Spent {
    /// Every cost is known: `known_usd` is the total.
    pub fn complete(&self) -> bool {
        self.uncounted_calls == 0 && self.unsettled_runs == 0
    }
}

impl Loaded {
    fn apply(&mut self, record: &Record) {
        self.updated_ms = self.updated_ms.max(record.at_ms);
        match &record.entry {
            Entry::Created { workspace, title } => {
                self.workspace = workspace.clone();
                self.title = title.clone();
                self.created_ms = record.at_ms;
            }
            Entry::Imported(imported) => {
                self.earlier.extend(imported.earlier.iter().cloned());
                self.spent.known_usd += imported.spent_usd;
                self.spent.uncounted_calls = self.spent.uncounted_calls.saturating_add(imported.uncounted_calls);
                self.spent.unsettled_runs = self.spent.unsettled_runs.saturating_add(imported.uncounted_stops);
                if let Some(active) = &imported.active {
                    self.spent.unsettled_runs = self.spent.unsettled_runs.saturating_add(1);
                    self.unanswered.push(Unanswered {
                        run: active.run.clone(),
                        task: active.task.clone(),
                        checkpointed: active.checkpointed,
                    });
                }
                if let Some(fork) = &imported.pending {
                    self.results.push(Pending { run: String::new(), fork: fork.clone(), gone: false });
                }
                self.notes.push(format!(
                    "{} was imported: {} earlier tasks as their summaries; their messages and tool results were \
                     never saved, so they are not here",
                    imported.from,
                    imported.earlier.len()
                ));
            }
            Entry::Task { run, task } => {
                // Unknown until its reply says what it spent.
                self.spent.unsettled_runs = self.spent.unsettled_runs.saturating_add(1);
                self.unanswered.push(Unanswered { run: run.clone(), task: task.clone(), checkpointed: false })
            }
            Entry::Reply(reply) => {
                if self.settle(&reply.run) {
                    self.spent.unsettled_runs = self.spent.unsettled_runs.saturating_sub(1);
                }
                self.earlier.push(reply.earlier.clone());
                self.spent.known_usd += reply.cost_usd;
                self.spent.uncounted_calls = self.spent.uncounted_calls.saturating_add(reply.uncounted_calls);
                if let Some(fork) = &reply.fork {
                    self.result_of.insert(fork.clone(), self.earlier.len() - 1);
                    self.results.push(Pending { run: reply.run.clone(), fork: fork.clone(), gone: false });
                }
            }
            Entry::Failed { run, .. } => {
                // Still not known: it stays among the unsettled.
                self.settle(run);
            }
            Entry::Asked { run, question, text, options } => {
                if self.unanswered.iter().any(|u| u.run == *run) {
                    self.questions.retain(|q| !(q.run == *run && q.question == *question));
                    self.questions.push(Question {
                        run: run.clone(),
                        question: *question,
                        text: text.clone(),
                        options: options.clone(),
                    });
                }
            }
            Entry::Answered { run, question, .. } => {
                self.questions.retain(|q| !(q.run == *run && q.question == *question));
            }
            Entry::Decided { fork, decision, .. } => {
                self.results.retain(|p| p.fork != *fork);
                if *decision == Decision::Applied {
                    // The next task is told that its changes are in the workspace now.
                    if let Some(earlier) = self.result_of.remove(fork).and_then(|i| self.earlier.get_mut(i)) {
                        earlier.applied = true;
                    }
                }
            }
            Entry::Archived => self.archived = true,
            Entry::Restored => self.archived = false,
        }
        self.entries.push(record.clone());
    }

    /// The task of `run` has its answer: it is no longer unanswered, nor are
    /// its questions. Whether it was unanswered.
    fn settle(&mut self, run: &str) -> bool {
        let before = self.unanswered.len();
        self.unanswered.retain(|u| u.run != run);
        self.questions.retain(|q| q.run != run);
        self.unanswered.len() < before
    }

    /// What can only be told now, from the disk: which forks are gone.
    fn look_around(&mut self) {
        for pending in &mut self.results {
            pending.gone = !Path::new(&pending.fork).is_dir();
            if pending.gone {
                self.notes.push(format!(
                    "the result kept in {} is gone: it may have been applied or removed before molt stopped; look at \
                     the workspace before going on",
                    pending.fork
                ));
            }
        }
        if !self.unanswered.is_empty() {
            self.notes.push(format!(
                "{} task(s) never got their reply, so what they did and spent is unknown and nothing runs them again; \
                 one that kept a checkpoint is in runs.list",
                self.unanswered.len()
            ));
        }
    }
}

/// A session as `sessions.list` shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub archived: bool,
    /// The tasks it was given.
    pub tasks: usize,
    pub spent: Spent,
    /// Why it cannot be loaded; `None` when it can.
    #[serde(default)]
    pub problem: Option<String>,
}

/// Another writer holds the session: a client of this server or of another.
#[derive(Debug)]
pub struct Busy(pub String);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session {} is open in another client; close it there first", self.0)
    }
}

impl std::error::Error for Busy {}

/// The sessions of a data dir.
#[derive(Clone, Debug)]
pub struct Sessions {
    dir: PathBuf,
}

impl Sessions {
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.join(DIR) }
    }

    fn path(&self, id: &str) -> anyhow::Result<PathBuf> {
        ensure!(valid_id(id), "{id:?} is not a session id: they look like sess_ and 16 hex digits");
        Ok(self.dir.join(id))
    }

    /// A new session of `workspace`, held for writing.
    pub fn create(&self, workspace: &str, title: Option<String>) -> anyhow::Result<Writer> {
        crate::lock::create_private_dir(&self.dir)?;
        let id = crate::runs::new_session();
        let dir = self.path(&id)?;
        DirBuilder::new().mode(0o700).create(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut writer = Writer::hold(id.clone(), &dir, Loaded { id, ..Loaded::default() }, 0)?;
        writer.append(Entry::Created { workspace: workspace.to_owned(), title })?;
        Ok(writer)
    }

    /// Session `id` as its journal has it, without holding it.
    pub fn load(&self, id: &str) -> anyhow::Result<Loaded> {
        let dir = self.path(id)?;
        let (mut loaded, _) = read(id, &dir)?;
        loaded.look_around();
        Ok(loaded)
    }

    /// Hold session `id` for writing. A line a crash cut off is dropped
    /// first: it was never confirmed. A damaged journal is left as it is.
    pub fn open(&self, id: &str) -> anyhow::Result<Writer> {
        let dir = self.path(id)?;
        let lock = take_lock(id, &dir)?;
        let (mut loaded, good) = read(id, &dir)?;
        loaded.look_around();
        let writer = Writer::with_lock(id.to_owned(), &dir, loaded, good, lock)?;
        writer.file.set_len(good).context("dropping the line a crash cut off")?;
        Ok(writer)
    }

    /// The sessions of `workspace`, the newest first, damaged ones included with why.
    pub fn list(&self, workspace: &str) -> Vec<Listed> {
        let Ok(dirs) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut listed = Vec::new();
        for dir in dirs.flatten() {
            let id = dir.file_name().to_string_lossy().into_owned();
            if !valid_id(&id) {
                continue;
            }
            let item = match read(&id, &dir.path()) {
                Ok((loaded, _)) if loaded.workspace != workspace => continue,
                Ok((loaded, _)) => Listed {
                    tasks: loaded.entries.iter().filter(|r| matches!(r.entry, Entry::Task { .. })).count(),
                    id,
                    title: loaded.title,
                    created_ms: loaded.created_ms,
                    updated_ms: loaded.updated_ms,
                    archived: loaded.archived,
                    spent: loaded.spent,
                    problem: None,
                },
                Err(e) => {
                    // Listed only when what can still be read says it is this workspace's.
                    if first_workspace(&dir.path()).as_deref() != Some(workspace) {
                        continue;
                    }
                    Listed {
                        id,
                        title: None,
                        created_ms: 0,
                        updated_ms: 0,
                        archived: false,
                        tasks: 0,
                        spent: Spent::default(),
                        problem: Some(format!("{e:#}")),
                    }
                }
            };
            listed.push(item);
        }
        listed.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| a.id.cmp(&b.id)));
        listed
    }

    /// The workspace session `id` was made for, when its first entry can be read.
    pub fn workspace(&self, id: &str) -> Option<String> {
        first_workspace(&self.path(id).ok()?)
    }

    /// Remove session `id` and its journal. Only on the person's word; a
    /// session another client holds is not removed.
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let dir = self.path(id)?;
        let _lock = take_lock(id, &dir)?;
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
    }
}

/// A session held for writing: one at a time, across processes.
#[derive(Debug)]
pub struct Writer {
    id: String,
    file: File,
    _lock: File,
    /// The journal's length after the last whole entry.
    len: u64,
    loaded: Loaded,
}

impl Writer {
    fn hold(id: String, dir: &Path, loaded: Loaded, len: u64) -> anyhow::Result<Self> {
        let lock = take_lock(&id, dir)?;
        Self::with_lock(id, dir, loaded, len, lock)
    }

    fn with_lock(id: String, dir: &Path, loaded: Loaded, len: u64, lock: File) -> anyhow::Result<Self> {
        let path = dir.join(JOURNAL);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self { id, file, _lock: lock, len, loaded })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn loaded(&self) -> &Loaded {
        &self.loaded
    }

    /// Write `entry` and flush it to the disk; once this returns, it is kept.
    pub fn append(&mut self, entry: Entry) -> anyhow::Result<()> {
        let at_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let seq = self.loaded.entries.last().map_or(1, |r| r.seq + 1);
        let mut record = Record { v: FORMAT, seq, at_ms, entry };
        let mut line = serde_json::to_vec(&record)?;
        if line.len() > MAX_LINE {
            if let Entry::Reply(reply) = &mut record.entry {
                reply.conversation.clear();
                reply.conversation_left_out = true;
                line = serde_json::to_vec(&record)?;
            }
        }
        ensure!(line.len() <= MAX_LINE, "the entry is larger than a session keeps ({} bytes)", line.len());
        line.push(b'\n');
        let written = self.file.write_all(&line).and_then(|()| self.file.sync_data());
        if let Err(e) = written {
            // A part written is a line never confirmed: take it back so the next one starts clean.
            let _ = self.file.set_len(self.len);
            return Err(e).context("writing to the session's journal");
        }
        self.len += line.len() as u64;
        self.loaded.apply(&record);
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("sess_").is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn take_lock(id: &str, dir: &Path) -> anyhow::Result<File> {
    ensure!(dir.is_dir(), "there is no session {id}");
    let path = dir.join(LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Busy(id.to_owned()).into()),
        Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// Read session `id`'s journal: what it holds, and the length of its whole
/// entries. Only the last line may be cut off; damage anywhere else, or an
/// entry of a newer format, is an error and the journal is not touched.
fn read(id: &str, dir: &Path) -> anyhow::Result<(Loaded, u64)> {
    ensure!(dir.is_dir(), "there is no session {id}");
    let path = dir.join(JOURNAL);
    let mut bytes = Vec::new();
    File::open(&path)
        .and_then(|f| f.take(MAX_JOURNAL + 1).read_to_end(&mut bytes))
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(bytes.len() as u64 <= MAX_JOURNAL, "its journal is larger than {MAX_JOURNAL} bytes");
    let mut loaded = Loaded { id: id.to_owned(), ..Loaded::default() };
    let mut good = 0u64;
    let mut rest = &bytes[..];
    let mut n = 0;
    while !rest.is_empty() {
        n += 1;
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            loaded.notes.push(format!(
                "the last entry (line {n}) was cut off when molt stopped; it was never confirmed and is left out"
            ));
            break;
        };
        let (line, after) = (&rest[..end], &rest[end + 1..]);
        let record = parse(line).with_context(|| {
            format!(
                "line {n} of {} is damaged, and the session is left as it is: delete it, or mend the line",
                path.display()
            )
        })?;
        if n == 1 {
            ensure!(
                matches!(record.entry, Entry::Created { .. }),
                "its journal does not start with the session's creation"
            );
        }
        loaded.apply(&record);
        good += end as u64 + 1;
        rest = after;
    }
    ensure!(n > 0 && !loaded.entries.is_empty(), "its journal is empty: it was never created whole");
    Ok((loaded, good))
}

fn parse(line: &[u8]) -> anyhow::Result<Record> {
    let value: Value = serde_json::from_slice(line).context("not JSON")?;
    let v = value.get("v").and_then(Value::as_u64).context("no format")?;
    if v > u64::from(FORMAT) {
        bail!("it is in format {v}, written by a newer molt; this one reads up to format {FORMAT}");
    }
    serde_json::from_value(value).context("not an entry this molt knows")
}

/// The workspace the first line names, when it can be read.
fn first_workspace(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(JOURNAL)).ok()?;
    let line = bytes.split(|&b| b == b'\n').next()?;
    match parse(line).ok()?.entry {
        Entry::Created { workspace, .. } => Some(workspace),
        _ => None,
    }
}

/// What the interface's saved session (`tui/saved.rs`, format 1) brings to a new session.
pub(crate) fn imported(saved: crate::tui::saved::Saved) -> Imported {
    Imported {
        from: format!("the interface's saved session (format {})", saved.version),
        earlier: saved.earlier,
        spent_usd: saved.spent_usd,
        uncounted_calls: saved.uncounted_calls,
        uncounted_stops: saved.uncounted_stops,
        active: saved.active.map(|a| ImportedTask { run: a.run, task: a.task, checkpointed: a.checkpointed }),
        pending: saved.pending.map(|p| p.fork),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_api::planner::Outcome;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    const WS: &str = "/home/me/project";

    fn reply(run: &str, task: &str, fork: Option<&Path>, cost_usd: f64, uncounted_calls: u32) -> Entry {
        Entry::Reply(Box::new(Reply {
            run: run.into(),
            earlier: Earlier {
                task: task.into(),
                outcome: Outcome::Passed,
                summary: format!("did {task}"),
                changes: vec![format!("{run}.txt")],
                applied: false,
            },
            fork: fork.map(|f| f.to_str().unwrap().to_owned()),
            checkpoint: None,
            cost_usd,
            uncounted_calls,
            conversation: vec![json!({"role": "user", "content": task}), json!({"role": "assistant", "content": "ok"})],
            conversation_left_out: false,
        }))
    }

    fn journal(sessions: &Sessions, id: &str) -> PathBuf {
        sessions.dir.join(id).join(JOURNAL)
    }

    #[test]
    fn ten_rounds_of_tasks_questions_and_decisions_are_all_there_after_a_normal_exit() {
        let data = tempfile::tempdir().unwrap();
        let forks = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(data.path());
        let mut writer = sessions.create(WS, Some("parser work".into())).unwrap();
        let id = writer.id().to_owned();
        for round in 1..=10u32 {
            let run = format!("trace_{round}");
            let fork = forks.path().join(format!("fork-{round}"));
            std::fs::create_dir(&fork).unwrap();
            writer.append(Entry::Task { run: run.clone(), task: format!("change {round}") }).unwrap();
            writer
                .append(Entry::Asked {
                    run: run.clone(),
                    question: 1,
                    text: format!("which {round}?"),
                    options: vec![],
                })
                .unwrap();
            writer.append(Entry::Answered { run: run.clone(), question: 1, text: "this one".into() }).unwrap();
            // The tenth round's second question is still open when its reply comes: it goes with it.
            if round == 10 {
                writer
                    .append(Entry::Asked { run: run.clone(), question: 2, text: "and?".into(), options: vec![] })
                    .unwrap();
            }
            // Round 7 never says what one call cost.
            writer.append(reply(&run, &format!("change {round}"), Some(&fork), 0.25, u32::from(round == 7))).unwrap();
            // Every round but the last is decided on: applied, kept or discarded in turn.
            let decision = match round % 3 {
                0 => Decision::Applied,
                1 => Decision::Kept,
                _ => Decision::Discarded,
            };
            if round < 10 {
                let fork = fork.to_str().unwrap().to_owned();
                writer.append(Entry::Decided { fork, decision, paths: vec![] }).unwrap();
            }
        }
        // An eleventh task is in flight when the person quits.
        writer.append(Entry::Task { run: "trace_11".into(), task: "change 11".into() }).unwrap();
        writer
            .append(Entry::Asked {
                run: "trace_11".into(),
                question: 1,
                text: "sure?".into(),
                options: vec!["yes".into()],
            })
            .unwrap();
        let before = writer.loaded().clone();
        drop(writer);

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&sessions.dir.join(&id)), 0o700);
        assert_eq!(mode(&journal(&sessions, &id)), 0o600);
        let loaded = sessions.load(&id).unwrap();
        assert_eq!(loaded.entries, before.entries, "the whole history, in order");
        assert_eq!(
            (loaded.title.as_deref(), loaded.workspace.as_str(), loaded.archived),
            (Some("parser work"), WS, false)
        );
        assert_eq!(loaded.earlier.len(), 10);
        let applied: Vec<bool> = loaded.earlier.iter().map(|e| e.applied).collect();
        assert_eq!(applied, (1..=10).map(|r| r % 3 == 0 && r < 10).collect::<Vec<_>>(), "applied results say so");
        assert_eq!(loaded.results.len(), 1, "only the last result waits for a decision: {:?}", loaded.results);
        assert!(loaded.results[0].fork.ends_with("fork-10") && !loaded.results[0].gone);
        assert_eq!(
            loaded.unanswered,
            vec![Unanswered { run: "trace_11".into(), task: "change 11".into(), checkpointed: false }]
        );
        assert_eq!(loaded.questions.len(), 1, "only the open task's question waits: {:?}", loaded.questions);
        assert_eq!(
            (loaded.questions[0].run.as_str(), loaded.questions[0].options.clone()),
            ("trace_11", vec!["yes".to_owned()])
        );
        assert!((loaded.spent.known_usd - 2.5).abs() < 1e-9, "{:?}", loaded.spent);
        assert_eq!(
            (loaded.spent.uncounted_calls, loaded.spent.unsettled_runs),
            (1, 1),
            "the task in flight is unknown"
        );
        assert!(!loaded.spent.complete(), "one call and the task in flight are not known");
        assert!(loaded.notes.iter().any(|n| n.contains("never got their reply")), "{:?}", loaded.notes);
        // A result whose fork went is named, not offered.
        std::fs::remove_dir(forks.path().join("fork-10")).unwrap();
        let gone = sessions.load(&id).unwrap();
        assert!(gone.results[0].gone);
        assert!(
            gone.notes.iter().any(|n| n.contains("fork-10 is gone") && n.contains("look at the workspace")),
            "{:?}",
            gone.notes
        );
        std::fs::create_dir(forks.path().join("fork-10")).unwrap();
        let Entry::Reply(first) = &loaded.entries[4].entry else { panic!("{:?}", loaded.entries[4]) };
        assert_eq!(first.conversation.len(), 2, "tool round trips are kept as the planner gave them");

        // Opened again, it goes on where it was, and is listed as this workspace's.
        let mut writer = sessions.open(&id).unwrap();
        writer.append(reply("trace_11", "change 11", None, 0.5, 0)).unwrap();
        assert!(writer.loaded().questions.is_empty() && writer.loaded().unanswered.is_empty());
        assert_eq!(writer.loaded().spent.unsettled_runs, 0, "its reply settled it");
        assert_eq!(writer.loaded().entries.last().unwrap().seq, before.entries.len() as u64 + 1);
        drop(writer);
        let listed = sessions.list(WS);
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].tasks, listed[0].problem.as_deref()), (11, None));
        assert!(sessions.list("/elsewhere").is_empty());
    }

    #[test]
    fn a_line_a_crash_cut_off_is_left_out_and_everything_confirmed_before_it_stays() {
        let data = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(data.path());
        let mut writer = sessions.create(WS, None).unwrap();
        let id = writer.id().to_owned();
        writer.append(Entry::Task { run: "trace_1".into(), task: "one".into() }).unwrap();
        writer.append(reply("trace_1", "one", None, 0.1, 0)).unwrap();
        // Killed while the next task ran, and again in the middle of writing its reply.
        writer.append(Entry::Task { run: "trace_2".into(), task: "two".into() }).unwrap();
        drop(writer);
        let path = journal(&sessions, &id);
        let confirmed = std::fs::read(&path).unwrap();
        let mut cut = confirmed.clone();
        cut.extend_from_slice(br#"{"v":1,"seq":5,"at_ms":1,"kind":"reply","run":"trace_2","earl"#);
        std::fs::write(&path, &cut).unwrap();

        let loaded = sessions.load(&id).unwrap();
        assert_eq!(loaded.entries.len(), 4, "every confirmed entry");
        assert!(loaded.notes.iter().any(|n| n.contains("cut off") && n.contains("line 5")), "{:?}", loaded.notes);
        assert_eq!(loaded.unanswered.iter().map(|u| u.run.as_str()).collect::<Vec<_>>(), ["trace_2"]);
        assert_eq!(loaded.spent.unsettled_runs, 1, "what the cut-off task spent is unknown, not zero");
        assert_eq!(std::fs::read(&path).unwrap(), cut, "loading writes nothing");

        // Holding it again drops the cut line, and what comes next is whole.
        let mut writer = sessions.open(&id).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), confirmed);
        writer.append(Entry::Failed { run: "trace_2".into(), error: "the server stopped".into() }).unwrap();
        drop(writer);
        let loaded = sessions.load(&id).unwrap();
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        assert_eq!(
            (loaded.entries.len(), loaded.spent.unsettled_runs),
            (5, 1),
            "the failed task's cost is unknown, not zero"
        );
        assert!(!loaded.spent.complete());
    }

    #[test]
    fn a_damaged_journal_or_one_of_a_newer_format_is_refused_and_left_as_it_is() {
        let data = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(data.path());
        let mut writer = sessions.create(WS, None).unwrap();
        let id = writer.id().to_owned();
        writer.append(Entry::Task { run: "trace_1".into(), task: "one".into() }).unwrap();
        writer.append(reply("trace_1", "one", None, 0.1, 0)).unwrap();
        drop(writer);
        let path = journal(&sessions, &id);
        let good = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = good.lines().collect();

        let damaged = format!("{}\n{{\"v\":1,\"seq\":2,\n{}\n", lines[0], lines[2]);
        std::fs::write(&path, &damaged).unwrap();
        for err in [sessions.load(&id).unwrap_err(), sessions.open(&id).unwrap_err()] {
            let err = format!("{err:#}");
            assert!(err.contains("line 2") && err.contains("damaged") && err.contains("left as it is"), "{err}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged, "nothing was overwritten");
        let listed = sessions.list(WS);
        assert!(listed[0].problem.as_deref().is_some_and(|p| p.contains("damaged")), "{listed:?}");

        let newer = format!("{good}{}\n", r#"{"v":2,"seq":4,"at_ms":1,"kind":"something_new"}"#);
        std::fs::write(&path, &newer).unwrap();
        let err = format!("{:#}", sessions.open(&id).unwrap_err());
        assert!(err.contains("format 2") && err.contains("newer molt"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);

        std::fs::write(&path, "").unwrap();
        assert!(format!("{:#}", sessions.load(&id).unwrap_err()).contains("never created whole"));
        assert!(sessions.load("../../etc").unwrap_err().to_string().contains("not a session id"));
        assert!(sessions.load("sess_0000000000000000").unwrap_err().to_string().contains("there is no session"));
    }

    #[test]
    fn one_writer_holds_a_session_and_it_is_deleted_only_when_none_does() {
        let data = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(data.path());
        let writer = sessions.create(WS, None).unwrap();
        let id = writer.id().to_owned();
        let busy = sessions.open(&id).unwrap_err();
        assert!(busy.downcast_ref::<Busy>().is_some(), "{busy:#}");
        assert!(sessions.delete(&id).unwrap_err().downcast_ref::<Busy>().is_some());
        assert!(sessions.load(&id).is_ok(), "reading needs no hold");
        drop(writer);
        let mut writer = sessions.open(&id).unwrap();
        writer.append(Entry::Archived).unwrap();
        assert!(writer.loaded().archived);
        writer.append(Entry::Restored).unwrap();
        drop(writer);
        assert!(!sessions.load(&id).unwrap().archived);
        sessions.delete(&id).unwrap();
        assert!(!sessions.dir.join(&id).exists());
        assert!(sessions.list(WS).is_empty());
    }

    #[test]
    fn a_reply_too_large_keeps_everything_but_its_conversation_and_says_so() {
        let data = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(data.path());
        let mut writer = sessions.create(WS, None).unwrap();
        writer.append(Entry::Task { run: "trace_1".into(), task: "one".into() }).unwrap();
        let Entry::Reply(mut big) = reply("trace_1", "one", None, 0.3, 0) else { unreachable!() };
        big.conversation = vec![json!("x".repeat(MAX_LINE))];
        writer.append(Entry::Reply(big)).unwrap();
        let loaded = sessions.load(writer.id()).unwrap();
        let Entry::Reply(kept) = &loaded.entries[2].entry else { panic!() };
        assert!(kept.conversation.is_empty() && kept.conversation_left_out);
        assert_eq!((loaded.earlier.len(), loaded.spent.known_usd), (1, 0.3));
    }

    #[test]
    fn the_interfaces_saved_session_is_imported_without_claiming_what_it_never_saved() {
        use crate::tui::saved::{Active, Phase, Saved};
        let data = tempfile::tempdir().unwrap();
        let sessions = Sessions::new(data.path());
        let earlier = Earlier {
            task: "fix parser".into(),
            outcome: Outcome::Passed,
            summary: "Parsed.".into(),
            changes: vec!["src/lib.rs".into()],
            applied: true,
        };
        let saved = Saved {
            version: 1,
            earlier: vec![earlier.clone()],
            spent_usd: 0.5,
            uncounted_calls: 1,
            uncounted_stops: 2,
            active: Some(Active {
                run: "trace_9".into(),
                task: "add tests".into(),
                phase: Phase::Running,
                version: 1,
                checkpointed: true,
            }),
            ..Saved::default()
        };
        let mut writer = sessions.create(WS, None).unwrap();
        writer.append(Entry::Imported(imported(saved))).unwrap();
        let id = writer.id().to_owned();
        drop(writer);
        let loaded = sessions.load(&id).unwrap();
        assert_eq!(loaded.earlier, vec![earlier], "what the next task is told of");
        assert_eq!(
            loaded.unanswered,
            vec![Unanswered { run: "trace_9".into(), task: "add tests".into(), checkpointed: true }]
        );
        assert_eq!(
            loaded.spent,
            Spent { known_usd: 0.5, uncounted_calls: 1, unsettled_runs: 3 },
            "unknown stays unknown"
        );
        assert!(
            loaded.notes.iter().any(|n| n.contains("as their summaries") && n.contains("never saved")),
            "the import says what was never there: {:?}",
            loaded.notes
        );
        let Entry::Imported(imported) = &loaded.entries[1].entry else { panic!() };
        assert_eq!(imported.from, "the interface's saved session (format 1)");
    }
}
