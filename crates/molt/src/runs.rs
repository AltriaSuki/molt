//! The runs of a data dir, read from its audit log: what `molt runs` and
//! the interface's `/runs` list.
//!
//! Each `planner.run` or `planner.plan` request is a run, under its trace,
//! and a resumed run is the same run asked again. What the list says of a
//! run comes from the log alone: the request (task, session, workflow), the
//! planner's reply (result, cost, what was applied or kept), the replies to
//! `planner.design` on the same trace (a check designed on its own, as the
//! interface does, costs too), memory's reply to the `memory.consolidate`
//! that names the run (what was learned), and the planner's starts and
//! exits: a request the planner never answered before it went was
//! interrupted.
//!
//! So that a listing does not read the whole log each time, the facts read
//! from it are kept in `runs.jsonl` in the data dir, each with the place in
//! the log it was read up to, and the next listing reads on from the last.
//! The index is only a cache: deleted, damaged, of another version, or
//! behind a log that was replaced, it is built again from the log, and the
//! list is the same.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use molt_api::memory::NoteKind;
use molt_api::planner::{Outcome, Verification};
use molt_kernel::audit::{self, AuditError, AuditEvent, Entry, Mark};
use molt_proto::{ErrorCode, Kind};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The index, in the data dir.
pub const INDEX: &str = "runs.jsonl";
/// Held while the index is read and brought up to date.
const LOCK: &str = "runs.lock";
/// The audit log, in the data dir.
const AUDIT: &str = "audit.jsonl";
/// The index's format. An index of another one is built again.
const VERSION: u32 = 1;
/// The most of a task the index keeps; `molt runs show` reads all of it from the log.
const TASK_BYTES: usize = 2000;
/// The most of a run's summary, or of an error, the index keeps.
const SUMMARY_BYTES: usize = 500;
/// The most of a learned note's text the index keeps.
const NOTE_BYTES: usize = 300;
/// The longest session name.
pub const MAX_SESSION: usize = 128;

/// The planner's service, whose methods start runs.
const PLANNER: &str = "planner";
const RUN: &str = molt_api::planner::RUN;
const PLAN: &str = molt_api::plan::PLAN;
const DESIGN: &str = molt_api::planner::DESIGN;
const CONSOLIDATE: &str = molt_api::memory::CONSOLIDATE;

/// One run, as the log has it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// The run's id: the trace of its requests.
    pub run: String,
    /// When it was first asked for, in milliseconds since the epoch.
    pub started_ms: u64,
    /// The task, cut to the first 2000 bytes.
    pub task: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The workflow the run went through, or the one it asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// A plan (`molt do --plan`): its steps were runs of their own inside it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan: bool,
    pub result: Status,
    /// How the result was verified; `None` without a result, or from an older planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// What the run spent, a check designed for it on its own included, as
    /// its reply says: a known subtotal when `uncounted_calls` is above
    /// zero. `None` when no reply said: what an interrupted run spent is
    /// not known, and never taken to be nothing.
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub uncounted_calls: u32,
    /// From each request to its answer, added up over resumes; `None`
    /// while it runs, or when it was never answered.
    pub duration_ms: Option<u64>,
    pub applied: Applied,
    /// Where the result is kept, when it was not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
    /// Files the result changes.
    #[serde(default)]
    pub changed: u32,
    /// What memory learned from the run; `None` when it was not asked to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learning: Option<Learning>,
    /// The run stopped with a checkpoint, and `molt resume RUN` goes on with it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resumable: bool,
    /// What the run said it did, or why it failed: the first 500 bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Where its first request is in the log, to read the run back from.
    #[serde(skip)]
    pub(crate) at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Unverified,
    Judged,
    /// The planner answered with an error.
    Error,
    /// It stopped before it finished, with no result: stopped, killed, or
    /// its planner went.
    Interrupted,
    /// Not answered yet, and the services are still up.
    Running,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Unverified => "unverified",
            Status::Judged => "judged",
            Status::Error => "error",
            Status::Interrupted => "interrupted",
            Status::Running => "running",
        }
    }

    fn of(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Passed => Status::Passed,
            Outcome::Failed => Status::Failed,
            Outcome::Unverified => Status::Unverified,
            Outcome::Judged => Status::Judged,
        }
    }
}

/// Where the result went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    /// Merged into the workspace.
    Workspace,
    /// Kept in its fork (`fork`), as `--no-apply` or the interface asks;
    /// it may have been applied from there since.
    Kept,
    /// No result to apply, or one with no changes.
    Nothing,
    /// No reply said.
    Unknown,
}

impl Applied {
    pub fn name(self) -> &'static str {
        match self {
            Applied::Workspace => "workspace",
            Applied::Kept => "kept",
            Applied::Nothing => "-",
            Applied::Unknown => "?",
        }
    }
}

/// What memory made of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Learning {
    Answered {
        /// The notes it added.
        notes: Vec<Learned>,
        reinforced: u32,
        contradicted: u32,
        /// Why nothing was learned, when nothing was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skipped: Option<String>,
        cost_usd: f64,
        #[serde(default)]
        uncounted_calls: u32,
    },
    Failed {
        error: String,
    },
    /// Asked, and no answer came.
    Unanswered,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub id: String,
    pub kind: NoteKind,
    /// Its first 300 bytes.
    pub text: String,
}

/// What one entry of the log says about runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Fact {
    /// `planner.run` or `planner.plan` was asked for.
    Asked {
        run: String,
        msg: String,
        /// Where the request starts in the log.
        at: u64,
        ts_ms: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        plan: bool,
        task: String,
        workspace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
    },
    /// A check was designed for a run on its own.
    Designing { run: String, msg: String },
    /// Memory was asked to learn from a run.
    Learning { run: String, msg: String },
    /// The request `msg` was cancelled: its caller stopped, or its
    /// deadline passed. The answer that follows is of a run cut off.
    Cancelled { msg: String },
    /// The answer to the request `msg`.
    Answered { msg: String, ts_ms: u64, answer: Answer },
    /// The planner started or exited: what it had not answered, it never will.
    PlannerGone { ts_ms: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Answer {
    Ran(Ended),
    Designed {
        cost_usd: f64,
        #[serde(default)]
        uncounted_calls: u32,
    },
    Learned {
        notes: Vec<Learned>,
        reinforced: u32,
        contradicted: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skipped: Option<String>,
        cost_usd: f64,
        #[serde(default)]
        uncounted_calls: u32,
    },
    /// An error reply. `stopped`: it was cancelled, or the kernel ended it
    /// because the service exited.
    Failed {
        message: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stopped: bool,
    },
}

/// A run's or a plan's reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Ended {
    outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verification: Option<Verification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    cost_usd: f64,
    #[serde(default)]
    uncounted_calls: u32,
    #[serde(default)]
    applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fork: Option<String>,
    #[serde(default)]
    changed: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    resumable: bool,
    #[serde(default)]
    summary: String,
}

/// One line of the index: the header, or a fact and the place in the log
/// just past its entry, or that place alone.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Line {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mark: Option<Mark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fact: Option<Fact>,
}

/// The runs of `data_dir`, oldest first, read from its audit log by way of
/// its index, which this brings up to date (or builds again). None when the
/// log does not exist yet.
pub fn list(data_dir: &Path) -> anyhow::Result<Vec<Run>> {
    let facts = facts(data_dir)?;
    Ok(fold(&facts, live(data_dir)))
}

/// A session: the runs of one name, oldest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// `None` for the runs that named no session.
    pub session: Option<String>,
    pub started_ms: u64,
    /// The known cost of its runs; a subtotal when `unknown_cost` is above zero.
    pub cost_usd: f64,
    /// Runs whose cost is not known, or only in part.
    pub unknown_cost: u32,
    pub runs: Vec<Run>,
}

/// `runs` by session, the session whose first run is oldest first, and the
/// runs without one last.
pub fn sessions(runs: Vec<Run>) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    for run in runs {
        let i = match out.iter().position(|s| s.session == run.session) {
            Some(i) => i,
            None => {
                out.push(Session {
                    session: run.session.clone(),
                    started_ms: run.started_ms,
                    cost_usd: 0.0,
                    unknown_cost: 0,
                    runs: Vec::new(),
                });
                out.len() - 1
            }
        };
        let session = &mut out[i];
        match run.cost_usd {
            Some(usd) => session.cost_usd += usd,
            None => session.unknown_cost += 1,
        }
        if run.cost_usd.is_some() && run.uncounted_calls > 0 {
            session.unknown_cost += 1;
        }
        session.runs.push(run);
    }
    out.sort_by_key(|s| (s.session.is_none(), s.started_ms));
    out
}

/// A new session's name, for an interface opened without one.
pub fn new_session() -> String {
    format!("sess_{:016x}", rand::random::<u64>())
}

/// `name` as a session's name: trimmed, at most [`MAX_SESSION`] bytes, with
/// no control character.
pub fn session_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a session needs a name".into());
    }
    if name.len() > MAX_SESSION {
        return Err(format!("a session name is at most {MAX_SESSION} bytes"));
    }
    if name.chars().any(char::is_control) {
        return Err("a session name has no control characters".into());
    }
    Ok(name.to_owned())
}

/// What is in the log about one run, for `molt runs show`.
#[derive(Debug)]
pub struct Detail {
    pub run: Run,
    /// Its last request, a plan's as its run.
    pub request: Option<molt_api::planner::RunRequest>,
    /// Its last reply: a run's, or a plan's result.
    pub response: Option<molt_api::planner::RunResponse>,
    pub plan: Option<molt_api::plan::PlanResponse>,
}

/// The run `id` of `data_dir` (a unique prefix will do), and what its
/// requests and replies say in full.
pub fn detail(data_dir: &Path, id: &str) -> anyhow::Result<Detail> {
    let runs = list(data_dir)?;
    let found: Vec<&Run> = match runs.iter().find(|r| r.run == id) {
        Some(run) => vec![run],
        None => runs.iter().filter(|r| r.run.starts_with(id)).collect(),
    };
    let run = match found.as_slice() {
        [run] => (*run).clone(),
        [] => anyhow::bail!("no run {id} in {}", data_dir.display()),
        _ => anyhow::bail!("{id} names {} runs; give more of the id", found.len()),
    };
    let mut detail = Detail { run, request: None, response: None, plan: None };
    let path = data_dir.join(AUDIT);
    let mut req = molt_proto::audit::ReadRequest::new(molt_proto::TraceId::from_raw(&detail.run.run));
    req.cursor = Some(detail.run.at);
    // The model's requests hold whole conversations, and say nothing here.
    req.skip_requests_to = vec![molt_proto::ServiceId::new("model")?];
    loop {
        let page = audit::read_trace(&path, &req).with_context(|| format!("reading {}", path.display()))?;
        for logged in page.entries {
            let envelope = logged.envelope;
            let to = envelope.to.to_string();
            match (envelope.kind, to.as_str()) {
                (Kind::Request, RUN | PLAN) => detail.request = serde_json::from_value(envelope.payload).ok(),
                (Kind::Reply, RUN) if envelope.error().is_none() => {
                    detail.response = serde_json::from_value(envelope.payload).ok();
                }
                (Kind::Reply, PLAN) if envelope.error().is_none() => {
                    let plan: Option<molt_api::plan::PlanResponse> = serde_json::from_value(envelope.payload).ok();
                    detail.response = plan.as_ref().and_then(|p| p.result.as_deref().cloned());
                    detail.plan = plan;
                }
                _ => {}
            }
        }
        match page.next {
            Some(next) => req.cursor = Some(next),
            None => return Ok(detail),
        }
    }
}

/// Whether a kernel holds the data dir now: the process named in its lock
/// file is there. Read rather than locked, so that a kernel starting at
/// this moment is never kept from its lock.
fn live(data_dir: &Path) -> bool {
    let Ok(pid) = std::fs::read_to_string(data_dir.join(crate::LOCK_FILE)) else { return false };
    let Ok(pid) = pid.trim().parse::<libc::pid_t>() else { return false };
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Every fact of the log of `data_dir`, in log order: those of the index,
/// and those the log has past it, which are added to it.
fn facts(data_dir: &Path) -> anyhow::Result<Vec<Fact>> {
    let audit = data_dir.join(AUDIT);
    if !audit.exists() {
        return Ok(Vec::new());
    }
    let index = data_dir.join(INDEX);
    // Two listings at once would both append; one waits for the other. A
    // data dir that cannot be written to is read without an index.
    let lock = OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(data_dir.join(LOCK));
    let Ok(lock) = lock else { return Ok(read_on(&audit, None)?.0.into_iter().map(|(_, f)| f).collect()) };
    lock.lock().with_context(|| format!("locking {}", data_dir.join(LOCK).display()))?;
    let kept = read_index(&index);
    let (mut facts, from) = kept.clone().unwrap_or_default();
    let (new, end, rebuilt) = match read_on(&audit, from) {
        Ok((new, end)) if kept.is_some() => (new, end, false),
        Ok((new, end)) => (new, end, true),
        Err(AuditError::Cursor(_)) => {
            facts.clear();
            let (new, end) = read_on(&audit, None)?;
            (new, end, true)
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", audit.display())),
    };
    let written = if rebuilt { write_index(&index, &new, end) } else { append_index(&index, &new, end, from) };
    if let Err(e) = written {
        tracing::warn!("the run index {} could not be written: {e:#}", index.display());
    }
    facts.extend(new.into_iter().map(|(_, fact)| fact));
    Ok(facts)
}

/// The facts of the index at `path` and the place in the log it goes up
/// to; `None` when there is none, or it cannot be used.
fn read_index(path: &Path) -> Option<(Vec<Fact>, Option<Mark>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut lines = std::io::BufReader::new(file).split(b'\n');
    let header: Line = serde_json::from_slice(&lines.next()?.ok()?).ok()?;
    if header.version != Some(VERSION) {
        return None;
    }
    let (mut facts, mut mark) = (Vec::new(), None);
    for line in lines {
        let line: Line = serde_json::from_slice(&line.ok()?).ok()?;
        let at = line.mark?;
        if mark.is_some_and(|m: Mark| at.offset < m.offset) {
            return None;
        }
        mark = Some(at);
        facts.extend(line.fact);
    }
    Some((facts, mark))
}

fn index_line(line: &Line) -> anyhow::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(line)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Write the index at `path` afresh: `facts`, read up to `end`.
fn write_index(path: &Path, facts: &[(Mark, Fact)], end: Mark) -> anyhow::Result<()> {
    let mut bytes = index_line(&Line { version: Some(VERSION), ..Line::default() })?;
    for (mark, fact) in facts {
        bytes.extend(index_line(&Line { mark: Some(*mark), fact: Some(fact.clone()), ..Line::default() })?);
    }
    if facts.last().map(|(m, _)| *m) != Some(end) {
        bytes.extend(index_line(&Line { mark: Some(end), ..Line::default() })?);
    }
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(&bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Add `facts` to the index at `path`, which went up to `from`, and that it now goes up to `end`.
fn append_index(path: &Path, facts: &[(Mark, Fact)], end: Mark, from: Option<Mark>) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    for (mark, fact) in facts {
        bytes.extend(index_line(&Line { mark: Some(*mark), fact: Some(fact.clone()), ..Line::default() })?);
    }
    let last = facts.last().map(|(m, _)| *m).or(from);
    if last != Some(end) {
        bytes.extend(index_line(&Line { mark: Some(end), ..Line::default() })?);
    }
    if !bytes.is_empty() {
        OpenOptions::new().append(true).open(path)?.write_all(&bytes)?;
    }
    Ok(())
}

/// The facts of the log at `path` past `from`, each with the place just
/// past its entry, and where the log ends.
fn read_on(path: &Path, from: Option<Mark>) -> Result<(Vec<(Mark, Fact)>, Mark), AuditError> {
    let mut facts = Vec::new();
    let end = audit::scan_from(path, from, wanted, |entry, at, mark| {
        if let Some(fact) = fact_of(entry, at) {
            facts.push((mark, fact));
        }
    })?;
    Ok((facts, end))
}

/// Whether the line of an entry may say anything about runs: it is a
/// message to or from a method that starts, designs or learns from one, or
/// the planner starting or exiting. Replies carry the target of their request.
fn wanted(line: &str) -> bool {
    [RUN, PLAN, DESIGN, CONSOLIDATE].iter().any(|to| line.contains(&format!("\"to\":\"{to}\"")))
        || (line.contains("\"type\":\"service_") && line.contains("\"service\":\"planner\""))
}

/// The first `max` bytes of `text`, cut at a character.
fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn fact_of(entry: Entry, at: u64) -> Option<Fact> {
    let ts_ms = entry.ts_ms;
    let envelope = match entry.event {
        AuditEvent::ServiceStarted { service, .. } | AuditEvent::ServiceExited { service, .. }
            if service.as_str() == PLANNER =>
        {
            return Some(Fact::PlannerGone { ts_ms });
        }
        AuditEvent::Message { envelope } => envelope,
        _ => return None,
    };
    let to = envelope.to.to_string();
    let run = envelope.trace_id.to_string();
    let msg = envelope.id.to_string();
    match envelope.kind {
        Kind::Request => match to.as_str() {
            RUN | PLAN => {
                #[derive(Deserialize)]
                struct Asked {
                    task: String,
                    #[serde(default)]
                    workspace: String,
                    #[serde(default)]
                    session: Option<String>,
                    #[serde(default)]
                    workflow: Option<String>,
                    /// A plan only proposed: nothing runs.
                    #[serde(default)]
                    propose: bool,
                }
                let asked: Asked = serde_json::from_value(envelope.payload).ok()?;
                if asked.propose {
                    return None;
                }
                Some(Fact::Asked {
                    run,
                    msg,
                    at,
                    ts_ms,
                    plan: to == PLAN,
                    task: cut(&asked.task, TASK_BYTES),
                    workspace: asked.workspace,
                    session: asked.session.map(|s| cut(&s, MAX_SESSION)),
                    workflow: asked.workflow,
                })
            }
            DESIGN => Some(Fact::Designing { run, msg }),
            CONSOLIDATE => {
                let run = envelope.payload.get("episode")?.as_str()?.to_owned();
                Some(Fact::Learning { run, msg })
            }
            _ => None,
        },
        Kind::Reply => {
            let msg = envelope.reply_to.as_ref()?.to_string();
            let answer = match envelope.error() {
                Some(error) => {
                    let by_kernel = envelope.from.as_ref().is_some_and(|f| f.as_str() == molt_proto::KERNEL);
                    let stopped =
                        error.code == ErrorCode::Cancelled || (by_kernel && error.code == ErrorCode::Unavailable);
                    Answer::Failed { message: cut(&error.message, SUMMARY_BYTES), stopped }
                }
                None => answer(&to, envelope.payload)?,
            };
            Some(Fact::Answered { msg, ts_ms, answer })
        }
        Kind::Cancel if to == RUN || to == PLAN => {
            Some(Fact::Cancelled { msg: envelope.reply_to.as_ref()?.to_string() })
        }
        _ => None,
    }
}

/// The reply of a run, as much of it as the list shows.
#[derive(Deserialize)]
struct Reply {
    outcome: Outcome,
    #[serde(default)]
    verification: Option<Verification>,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    cost_usd: f64,
    #[serde(default)]
    uncounted_calls: u32,
    #[serde(default)]
    applied: bool,
    #[serde(default)]
    fork: Option<String>,
    #[serde(default)]
    changes: Vec<IgnoredAny>,
    #[serde(default)]
    checkpoint: Option<String>,
    #[serde(default)]
    summary: String,
}

impl Reply {
    fn ended(self) -> Ended {
        Ended {
            outcome: self.outcome,
            verification: self.verification,
            workflow: self.workflow,
            cost_usd: self.cost_usd,
            uncounted_calls: self.uncounted_calls,
            applied: self.applied,
            fork: self.fork,
            changed: u32::try_from(self.changes.len()).unwrap_or(u32::MAX),
            resumable: self.checkpoint.is_some(),
            summary: cut(&self.summary, SUMMARY_BYTES),
        }
    }
}

fn answer(to: &str, payload: Value) -> Option<Answer> {
    let unreadable =
        |e: serde_json::Error| Answer::Failed { message: format!("the reply does not read: {e}"), stopped: false };
    Some(match to {
        RUN => match serde_json::from_value::<Reply>(payload) {
            Ok(reply) => Answer::Ran(reply.ended()),
            Err(e) => unreadable(e),
        },
        PLAN => {
            #[derive(Deserialize)]
            struct Planned {
                #[serde(default)]
                result: Option<Reply>,
                #[serde(default)]
                cost_usd: f64,
                #[serde(default)]
                uncounted_calls: u32,
                #[serde(default)]
                error: Option<String>,
            }
            match serde_json::from_value::<Planned>(payload) {
                // The plan's totals are its steps' and its own together.
                Ok(Planned { result: Some(result), cost_usd, uncounted_calls, .. }) => {
                    Answer::Ran(Ended { cost_usd, uncounted_calls, ..result.ended() })
                }
                Ok(Planned { result: None, cost_usd, uncounted_calls, error }) => Answer::Ran(Ended {
                    outcome: Outcome::Failed,
                    verification: None,
                    workflow: None,
                    cost_usd,
                    uncounted_calls,
                    applied: false,
                    fork: None,
                    changed: 0,
                    resumable: false,
                    summary: cut(error.as_deref().unwrap_or("the plan has no result"), SUMMARY_BYTES),
                }),
                Err(e) => unreadable(e),
            }
        }
        DESIGN => {
            #[derive(Deserialize)]
            struct Designed {
                #[serde(default)]
                cost_usd: f64,
                #[serde(default)]
                uncounted_calls: u32,
            }
            let d: Designed = serde_json::from_value(payload).ok()?;
            Answer::Designed { cost_usd: d.cost_usd, uncounted_calls: d.uncounted_calls }
        }
        CONSOLIDATE => {
            #[derive(Deserialize)]
            struct Note {
                id: String,
                kind: NoteKind,
                text: String,
            }
            #[derive(Deserialize)]
            struct Consolidated {
                #[serde(default)]
                added: Vec<Note>,
                #[serde(default)]
                reinforced: Vec<IgnoredAny>,
                #[serde(default)]
                contradicted: Vec<IgnoredAny>,
                #[serde(default)]
                skipped: Option<String>,
                #[serde(default)]
                cost_usd: f64,
                #[serde(default)]
                uncounted_calls: u32,
            }
            match serde_json::from_value::<Consolidated>(payload) {
                Ok(c) => Answer::Learned {
                    notes: c
                        .added
                        .into_iter()
                        .map(|n| Learned { id: n.id, kind: n.kind, text: cut(&n.text, NOTE_BYTES) })
                        .collect(),
                    reinforced: u32::try_from(c.reinforced.len()).unwrap_or(u32::MAX),
                    contradicted: u32::try_from(c.contradicted.len()).unwrap_or(u32::MAX),
                    skipped: c.skipped.map(|s| cut(&s, SUMMARY_BYTES)),
                    cost_usd: c.cost_usd,
                    uncounted_calls: c.uncounted_calls,
                },
                Err(e) => unreadable(e),
            }
        }
        _ => return None,
    })
}

/// What a request still waits for.
enum Waiting {
    /// A run's request, asked at `since`; `cancelled` once it was.
    Run {
        id: String,
        since: u64,
        cancelled: bool,
    },
    Design(String),
    Learn(String),
}

/// The runs `facts` tell of, oldest first. `live`: the services are up, so
/// a request with no answer may still get one.
fn fold(facts: &[Fact], live: bool) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut waiting: HashMap<String, Waiting> = HashMap::new();
    // Each run's own reported cost, and what checks designed on their own cost.
    let mut reported: HashMap<String, (f64, u32)> = HashMap::new();
    let mut designed: HashMap<String, (f64, u32)> = HashMap::new();
    let mut ran_for: HashMap<String, u64> = HashMap::new();
    let mut answered: HashSet<String> = HashSet::new();
    for fact in facts {
        match fact {
            Fact::Asked { run, msg, at, ts_ms, plan, task, workspace, session, workflow } => {
                let i = *by_id.entry(run.clone()).or_insert_with(|| {
                    runs.push(Run {
                        run: run.clone(),
                        started_ms: *ts_ms,
                        task: task.clone(),
                        workspace: workspace.clone(),
                        session: session.clone(),
                        workflow: workflow.clone(),
                        plan: *plan,
                        result: Status::Running,
                        verification: None,
                        cost_usd: None,
                        uncounted_calls: 0,
                        duration_ms: None,
                        applied: Applied::Unknown,
                        fork: None,
                        changed: 0,
                        learning: None,
                        resumable: false,
                        summary: String::new(),
                        at: *at,
                    });
                    runs.len() - 1
                });
                // Asked again: a resume, which goes on under the same id.
                runs[i].result = Status::Running;
                runs[i].resumable = false;
                waiting.insert(msg.clone(), Waiting::Run { id: run.clone(), since: *ts_ms, cancelled: false });
            }
            Fact::Designing { run, msg } => {
                waiting.insert(msg.clone(), Waiting::Design(run.clone()));
            }
            Fact::Cancelled { msg } => {
                if let Some(Waiting::Run { cancelled, .. }) = waiting.get_mut(msg) {
                    *cancelled = true;
                }
            }
            Fact::Learning { run, msg } => {
                if let Some(&i) = by_id.get(run) {
                    runs[i].learning = Some(Learning::Unanswered);
                }
                waiting.insert(msg.clone(), Waiting::Learn(run.clone()));
            }
            Fact::Answered { msg, ts_ms, answer } => match (waiting.remove(msg), answer) {
                (Some(Waiting::Run { id, since, cancelled }), answer) => {
                    let Some(&i) = by_id.get(&id) else { continue };
                    *ran_for.entry(id.clone()).or_default() += ts_ms.saturating_sub(since);
                    answered.insert(id.clone());
                    let run = &mut runs[i];
                    match answer {
                        Answer::Ran(ended) => {
                            // A run cut off still says what it did and spent; it did not finish.
                            run.result = if cancelled { Status::Interrupted } else { Status::of(ended.outcome) };
                            run.verification = ended.verification;
                            run.workflow = ended.workflow.clone().or(run.workflow.take());
                            run.applied = match (ended.applied, &ended.fork) {
                                (true, _) => Applied::Workspace,
                                (false, Some(_)) => Applied::Kept,
                                (false, None) => Applied::Nothing,
                            };
                            run.fork = ended.fork.clone().filter(|_| !ended.applied);
                            run.changed = ended.changed;
                            run.resumable = ended.resumable;
                            run.summary = ended.summary.clone();
                            reported.insert(id, (ended.cost_usd, ended.uncounted_calls));
                        }
                        Answer::Failed { message, stopped } => {
                            let stopped = *stopped || cancelled;
                            run.result = if stopped { Status::Interrupted } else { Status::Error };
                            run.summary = message.clone();
                            if run.applied == Applied::Unknown && !stopped {
                                run.applied = Applied::Nothing;
                            }
                        }
                        _ => {}
                    }
                }
                (Some(Waiting::Design(id)), Answer::Designed { cost_usd, uncounted_calls }) => {
                    let spent = designed.entry(id).or_default();
                    spent.0 += cost_usd;
                    spent.1 = spent.1.saturating_add(*uncounted_calls);
                }
                (Some(Waiting::Learn(id)), answer) => {
                    let Some(&i) = by_id.get(&id) else { continue };
                    runs[i].learning = match answer {
                        Answer::Learned { notes, reinforced, contradicted, skipped, cost_usd, uncounted_calls } => {
                            Some(Learning::Answered {
                                notes: notes.clone(),
                                reinforced: *reinforced,
                                contradicted: *contradicted,
                                skipped: skipped.clone(),
                                cost_usd: *cost_usd,
                                uncounted_calls: *uncounted_calls,
                            })
                        }
                        Answer::Failed { message, .. } => Some(Learning::Failed { error: message.clone() }),
                        _ => runs[i].learning.take(),
                    };
                }
                _ => {}
            },
            Fact::PlannerGone { ts_ms } => {
                waiting.retain(|_, w| match w {
                    Waiting::Run { id, since, .. } => {
                        if let Some(&i) = by_id.get(id.as_str()) {
                            runs[i].result = Status::Interrupted;
                            *ran_for.entry(id.clone()).or_default() += ts_ms.saturating_sub(*since);
                            answered.insert(id.clone());
                        }
                        false
                    }
                    Waiting::Design(_) => false,
                    Waiting::Learn(_) => true,
                });
            }
        }
    }
    for waited in waiting.values() {
        if let Waiting::Run { id, .. } = waited {
            if let Some(&i) = by_id.get(id) {
                runs[i].result = if live { Status::Running } else { Status::Interrupted };
            }
        }
    }
    for run in &mut runs {
        if let Some(&(usd, calls)) = reported.get(&run.run) {
            let (design_usd, design_calls) = designed.get(&run.run).copied().unwrap_or_default();
            run.cost_usd = Some(usd + design_usd);
            run.uncounted_calls = calls.saturating_add(design_calls);
        }
        if run.result != Status::Running && answered.contains(&run.run) {
            run.duration_ms = ran_for.get(&run.run).copied();
        }
    }
    runs
}

/// Milliseconds since the epoch as `YYYY-MM-DD HH:MM` UTC.
pub fn when(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rest / 3600, rest % 3600 / 60)
}

/// A cost as the list shows it: `?` when it is not known, and `+?` after a
/// subtotal.
pub fn cost(usd: Option<f64>, uncounted_calls: u32) -> String {
    match (usd, uncounted_calls) {
        (None, _) => "?".to_owned(),
        (Some(usd), 0) => format!("${usd:.4}"),
        (Some(usd), _) => format!("${usd:.4}+?"),
    }
}

/// A duration as `45s`, `3m07s` or `2h05m`.
pub fn duration(ms: Option<u64>) -> String {
    let Some(ms) = ms else { return "-".to_owned() };
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Where a run's result went, in a word.
pub fn applied_name(run: &Run) -> &'static str {
    run.applied.name()
}

/// What memory made of a run, in a few words.
pub fn learned(learning: Option<&Learning>) -> String {
    match learning {
        None => "-".to_owned(),
        Some(Learning::Unanswered) => "?".to_owned(),
        Some(Learning::Failed { .. }) => "failed".to_owned(),
        Some(Learning::Answered { notes, .. }) if notes.len() == 1 => "1 note".to_owned(),
        Some(Learning::Answered { notes, .. }) if !notes.is_empty() => format!("{} notes", notes.len()),
        Some(Learning::Answered { .. }) => "nothing".to_owned(),
    }
}

/// The list as a table: a heading, and a line per run.
pub fn table(runs: &[Run]) -> String {
    let rows: Vec<[String; 8]> = runs
        .iter()
        .map(|r| {
            [
                r.run.clone(),
                when(r.started_ms),
                r.result.name().to_owned() + if r.resumable { "*" } else { "" },
                cost(r.cost_usd, r.uncounted_calls),
                duration(r.duration_ms),
                learned(r.learning.as_ref()),
                r.applied.name().to_owned(),
                first_line(&r.task, 60),
            ]
        })
        .collect();
    let heading = ["RUN", "STARTED (UTC)", "RESULT", "COST", "TIME", "LEARNED", "APPLIED", "TASK"].map(str::to_owned);
    let mut widths = [0usize; 8];
    for row in std::iter::once(&heading).chain(&rows) {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&heading).chain(&rows) {
        let mut line = String::new();
        for (i, (cell, w)) in row.iter().zip(widths).enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<w$}  "));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The first line of `text`, cut to `max` characters with an ellipsis.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The data dir's audit log, for a message that names it.
pub fn audit_path(data_dir: &Path) -> PathBuf {
    data_dir.join(AUDIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_kernel::audit::AuditLog;
    use molt_proto::{CapId, Envelope, ServiceId, TraceId};
    use serde_json::json;

    fn request(trace: &TraceId, to: &str, payload: Value) -> Envelope {
        let mut envelope = Envelope::request(trace.clone(), to.parse().unwrap(), CapId::random(), payload);
        envelope.from = Some(ServiceId::new("cli").unwrap());
        envelope
    }

    fn reply(to: &Envelope, from: &str, payload: Value) -> Envelope {
        let mut reply = to.reply(payload);
        reply.from = Some(ServiceId::new(from).unwrap());
        reply
    }

    fn ran(outcome: &str, cost: f64, applied: bool) -> Value {
        json!({ "outcome": outcome, "summary": "did it", "changes": [{"path": "a", "kind": "added"}], "patch": "",
                "applied": applied, "attempts": [], "usage": {}, "cost_usd": cost,
                "fork": if applied { Value::Null } else { json!("/d/work/fork-1") } })
    }

    async fn append(log: &AuditLog, envelope: Envelope) {
        log.append(AuditEvent::Message { envelope }).await.unwrap();
    }

    async fn planner(log: &AuditLog, started: bool) {
        let service = ServiceId::new(PLANNER).unwrap();
        let event = if started {
            AuditEvent::ServiceStarted { service, pid: Some(1) }
        } else {
            AuditEvent::ServiceExited { service, status: "exit status: 0".into() }
        };
        log.append(event).await.unwrap();
    }

    #[tokio::test]
    async fn runs_come_from_the_log_and_the_index_read_on_or_built_again_lists_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUDIT);
        assert!(list(dir.path()).unwrap().is_empty(), "no log yet");
        let log = AuditLog::open(&path, false).await.unwrap();
        planner(&log, true).await;
        let (one, two, three) = (TraceId::random(), TraceId::random(), TraceId::random());
        // A check designed on its own first, as the interface does; then the run, in a session.
        let design = request(&one, DESIGN, json!({ "task": "first", "workspace": "/w" }));
        append(&log, design.clone()).await;
        append(&log, reply(&design, PLANNER, json!({ "cost_usd": 0.25, "uncounted_calls": 1 }))).await;
        let asked = request(&one, RUN, json!({ "task": "first\nmore", "workspace": "/w", "session": "s1" }));
        append(&log, asked.clone()).await;
        append(&log, reply(&asked, PLANNER, ran("passed", 0.5, true))).await;
        let learn = request(&TraceId::random(), CONSOLIDATE, json!({ "episode": one.to_string(), "workspace": "/w" }));
        append(&log, learn.clone()).await;
        let note = json!({ "id": "note_1", "kind": "lesson", "text": "x".repeat(4000) });
        append(
            &log,
            reply(
                &learn,
                "memory",
                json!({ "added": [note], "reinforced": ["note_0"], "contradicted": [],
            "usage": {}, "cost_usd": 0.01 }),
            ),
        )
        .await;
        log.close().await;

        let first = list(dir.path()).unwrap();
        assert_eq!(first.len(), 1);
        let run = &first[0];
        assert_eq!(
            (run.run.as_str(), run.task.as_str(), run.session.as_deref()),
            (one.as_str(), "first\nmore", Some("s1"))
        );
        assert_eq!(
            (run.result, run.applied, run.changed, run.uncounted_calls),
            (Status::Passed, Applied::Workspace, 1, 1)
        );
        assert_eq!(run.cost_usd, Some(0.75), "the design's cost counts too");
        let Some(Learning::Answered { notes, reinforced: 1, .. }) = &run.learning else { panic!("{:?}", run.learning) };
        assert_eq!(notes[0].text.len(), NOTE_BYTES);
        assert_eq!(learned(run.learning.as_ref()), "1 note");

        // More runs: one kept in its fork, one the planner never answered before it exited, and one unanswered.
        let log = AuditLog::open(&path, false).await.unwrap();
        let kept = request(&two, RUN, json!({ "task": "second", "workspace": "/w", "session": "s1" }));
        append(&log, kept.clone()).await;
        append(&log, reply(&kept, PLANNER, ran("unverified", 0.1, false))).await;
        // Stopped by its caller: the planner still says what it spent.
        let stopped = request(&TraceId::random(), RUN, json!({ "task": "fifth", "workspace": "/w" }));
        append(&log, stopped.clone()).await;
        let mut cancel = stopped.reply(Value::Null);
        (cancel.kind, cancel.from) = (Kind::Cancel, Some(ServiceId::kernel()));
        append(&log, cancel).await;
        append(&log, reply(&stopped, PLANNER, ran("failed", 0.2, false))).await;
        let cut = request(&three, PLAN, json!({ "task": "third", "workspace": "/w" }));
        append(&log, cut.clone()).await;
        // A proposal only is no run.
        append(&log, request(&TraceId::random(), PLAN, json!({ "task": "p", "workspace": "/w", "propose": true })))
            .await;
        planner(&log, false).await;
        let hung = request(&TraceId::random(), RUN, json!({ "task": "fourth", "workspace": "/w" }));
        append(&log, hung.clone()).await;
        log.close().await;

        let listed = list(dir.path()).unwrap();
        assert_eq!(listed[0], first[0]);
        let results: Vec<_> = listed.iter().map(|r| (r.task.as_str(), r.result, r.applied, r.cost_usd)).collect();
        assert_eq!(
            results,
            [
                ("first\nmore", Status::Passed, Applied::Workspace, Some(0.75)),
                ("second", Status::Unverified, Applied::Kept, Some(0.1)),
                ("fifth", Status::Interrupted, Applied::Kept, Some(0.2)),
                ("third", Status::Interrupted, Applied::Unknown, None),
                ("fourth", Status::Interrupted, Applied::Unknown, None),
            ]
        );
        assert!(listed[3].plan && listed[3].duration_ms.is_some());
        assert_eq!(listed[4].duration_ms, None, "never answered, and no one said when it stopped");
        assert_eq!(listed[1].fork.as_deref(), Some("/d/work/fork-1"));
        assert_eq!(fold(&facts(dir.path()).unwrap(), true)[4].result, Status::Running, "while the services are up");

        // Deleted, the index is built again to the same list; damaged or of another version, too.
        let index = dir.path().join(INDEX);
        let written = std::fs::read(&index).unwrap();
        std::fs::remove_file(&index).unwrap();
        assert_eq!(list(dir.path()).unwrap(), listed);
        assert_eq!(std::fs::read(&index).unwrap().len(), written.len(), "one fact a line either way");
        std::fs::write(&index, "{\"version\":1}\nnot json\n").unwrap();
        assert_eq!(list(dir.path()).unwrap(), listed);
        std::fs::write(&index, "{\"version\":99}\n").unwrap();
        assert_eq!(list(dir.path()).unwrap(), listed);

        // An answer that comes later is found from where the index stopped.
        let log = AuditLog::open(&path, false).await.unwrap();
        append(&log, reply(&hung, "kernel", json!({ "error": { "code": "failed", "message": "it broke" } }))).await;
        log.close().await;
        let later = list(dir.path()).unwrap();
        assert_eq!((later[4].result, later[4].summary.as_str()), (Status::Error, "it broke"));
        assert_eq!(later[..4], listed[..4]);

        // A log replaced under the index: built again from the new one.
        std::fs::remove_file(&path).unwrap();
        let log = AuditLog::open(&path, false).await.unwrap();
        let other = request(&TraceId::random(), RUN, json!({ "task": "elsewhere", "workspace": "/w" }));
        append(&log, other.clone()).await;
        let mut stopped =
            reply(&other, "kernel", json!({ "error": { "code": "unavailable", "message": "the service exited" } }));
        stopped.from = Some(ServiceId::kernel());
        append(&log, stopped).await;
        log.close().await;
        let replaced = list(dir.path()).unwrap();
        assert_eq!(replaced.len(), 1);
        assert_eq!((replaced[0].task.as_str(), replaced[0].result), ("elsewhere", Status::Interrupted));
    }

    #[test]
    fn sessions_group_runs_and_count_an_unknown_cost_apart() {
        let run = |id: &str, session: Option<&str>, ms: u64, usd: Option<f64>| Run {
            run: id.into(),
            started_ms: ms,
            task: "t".into(),
            workspace: "/w".into(),
            session: session.map(str::to_owned),
            workflow: None,
            plan: false,
            result: Status::Passed,
            verification: None,
            cost_usd: usd,
            uncounted_calls: 0,
            duration_ms: None,
            applied: Applied::Nothing,
            fork: None,
            changed: 0,
            learning: None,
            resumable: false,
            summary: String::new(),
            at: 0,
        };
        let grouped = sessions(vec![
            run("a", None, 1, Some(1.0)),
            run("b", Some("work"), 2, Some(0.5)),
            run("c", Some("later"), 3, None),
            run("d", Some("work"), 4, Some(0.25)),
        ]);
        let shape: Vec<_> = grouped
            .iter()
            .map(|s| {
                (
                    s.session.as_deref(),
                    s.runs.iter().map(|r| r.run.as_str()).collect::<Vec<_>>(),
                    s.cost_usd,
                    s.unknown_cost,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [(Some("work"), vec!["b", "d"], 0.75, 0), (Some("later"), vec!["c"], 0.0, 1), (None, vec!["a"], 1.0, 0)]
        );
        assert_eq!(session_name("  mine "), Ok("mine".into()));
        assert!(
            session_name("").is_err() && session_name("a\u{1b}b").is_err() && session_name(&"x".repeat(129)).is_err()
        );
    }

    #[test]
    fn costs_durations_and_tasks_read_as_the_list_shows_them() {
        assert_eq!(
            (cost(None, 0), cost(Some(0.5), 0), cost(Some(0.5), 2)),
            ("?".into(), "$0.5000".into(), "$0.5000+?".into())
        );
        assert_eq!(
            [duration(None), duration(Some(45_000)), duration(Some(187_000)), duration(Some(7_500_000))],
            ["-", "45s", "3m07s", "2h05m"]
        );
        assert_eq!(first_line("one\ntwo", 10), "one");
        assert_eq!(first_line("abcdefghijk", 5), "abcd…");
        assert_eq!(when(0), "1970-01-01 00:00");
    }
}
