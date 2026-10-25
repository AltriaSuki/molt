//! The client protocol `molt serve` speaks (RFC 0004): JSON-RPC 2.0, one
//! message per line, over a private Unix socket or a pipe. The interface in
//! the terminal is one client of it; an editor's adapter or a script can be
//! another.
//!
//! The payloads are the `molt-api` types the services already answer with;
//! the types here are only the envelopes, the parameters of each method, and
//! the few results that are not a service's reply. Every field added to them
//! later gets `#[serde(default)]`, so a client written for version 1 keeps
//! working; `testdata/protocol-v1.jsonl` holds version 1's messages, and a
//! test reads each of them with the types of today.

use molt_api::memory::ReviewRequest;
use molt_api::plan::PlanRequest;
use molt_api::planner::{CheckSpec, Constraint, DesignResponse, RunRequest, RunResponse};
use molt_proto::TraceId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The protocol version this build speaks. A client says which it speaks in
/// [`method::INITIALIZE`]; one of another version is refused.
pub const VERSION: u32 = 1;
/// The longest message, in bytes, either side takes; a longer one ends the connection.
pub const MAX_LINE: usize = 16 * 1024 * 1024;
/// The name of the socket `molt serve` listens on, in the data dir.
pub const SOCKET: &str = "serve.sock";

/// The methods, in the order the RFC lists them.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_OPEN: &str = "session.open";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const TASK_DESIGN: &str = "task.design";
    pub const TASK_SUBMIT: &str = "task.submit";
    pub const TASK_PLAN: &str = "task.plan";
    pub const TASK_STEER: &str = "task.steer";
    pub const TASK_PAUSE: &str = "task.pause";
    pub const TASK_RESUME: &str = "task.resume";
    pub const QUESTION_ANSWER: &str = "question.answer";
    pub const TASK_STOP: &str = "task.stop";
    pub const TASK_LEARN: &str = "task.learn";
    pub const RESULT_DIFF: &str = "result.diff";
    pub const RESULT_APPLY: &str = "result.apply";
    pub const RESULT_RECHECK: &str = "result.recheck";
    pub const RESULT_KEEP: &str = "result.keep";
    pub const RESULT_DISCARD: &str = "result.discard";
    pub const COMMIT_PREPARE: &str = "commit.prepare";
    pub const COMMIT_MAKE: &str = "commit.make";
    pub const RUNS_LIST: &str = "runs.list";
    pub const RUNS_DROP: &str = "runs.drop";
    pub const RUNS_UNDO: &str = "runs.undo";
    pub const NOTES_LIST: &str = "notes.list";
    pub const NOTES_FORGET: &str = "notes.forget";
    pub const NOTES_REMEMBER: &str = "notes.remember";
    pub const NOTES_PROMOTE: &str = "notes.promote";
    pub const NOTES_CHANGE: &str = "notes.change";
    pub const NOTES_USED: &str = "notes.used";
    pub const COMMAND_RUN: &str = "command.run";
    pub const COMMAND_STOP: &str = "command.stop";

    /// Every method of this version.
    pub const ALL: [&str; 30] = [
        INITIALIZE,
        SESSION_OPEN,
        SESSION_CLOSE,
        TASK_DESIGN,
        TASK_SUBMIT,
        TASK_PLAN,
        TASK_STEER,
        TASK_PAUSE,
        TASK_RESUME,
        QUESTION_ANSWER,
        TASK_STOP,
        TASK_LEARN,
        RESULT_DIFF,
        RESULT_APPLY,
        RESULT_RECHECK,
        RESULT_KEEP,
        RESULT_DISCARD,
        COMMIT_PREPARE,
        COMMIT_MAKE,
        RUNS_LIST,
        RUNS_DROP,
        RUNS_UNDO,
        NOTES_LIST,
        NOTES_FORGET,
        NOTES_REMEMBER,
        NOTES_PROMOTE,
        NOTES_CHANGE,
        NOTES_USED,
        COMMAND_RUN,
        COMMAND_STOP,
    ];

    /// The notification the server sends for each progress event.
    pub const PROGRESS: &str = "progress";
}

/// Error codes: JSON-RPC's own, and the server's from -32000 down.
pub mod code {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// The call was made and failed; the message says why, as the interface shows it.
    pub const FAILED: i64 = -32000;
    /// No session is open on this connection: `session.open` first.
    pub const NO_SESSION: i64 = -32001;
    /// `initialize` has not been called, or was refused.
    pub const NOT_INITIALIZED: i64 = -32002;
    /// `task.stop` or `session.close` stopped the call before it answered:
    /// what it did is unknown, never taken to be nothing.
    pub const CANCELLED: i64 = -32003;
    /// Another connection has the session open.
    pub const BUSY: i64 = -32004;
    /// The session is still starting; `task.stop` waits for nothing.
    pub const STARTING: i64 = -32005;
}

/// A request id: a number or a string, as JSON-RPC allows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(u64),
    String(String),
}

/// A message from the client. Without an `id` it is a notification, which
/// the server answers with nothing, not even an error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    pub fn new(id: Id, method: &str, params: impl Serialize) -> serde_json::Result<Self> {
        Ok(Self { jsonrpc: "2.0".into(), id: Some(id), method: method.into(), params: serde_json::to_value(params)? })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
}

impl Error {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// The server's answer to a request: `result` or `error`, never both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    /// `null` only for a request that could not be read.
    pub id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Response {
    pub fn new(id: Option<Id>, outcome: Result<Value, Error>) -> Self {
        let (result, error) = match outcome {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        Self { jsonrpc: "2.0".into(), id, result, error }
    }

    /// The answer as a result; an answer with neither is a protocol error.
    pub fn outcome(self) -> Result<Value, Error> {
        match (self.result, self.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Err(Error::new(code::INVALID_REQUEST, "the answer has neither a result nor an error")),
        }
    }
}

/// A message from the server that answers nothing: a progress event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Anything the server sends: told apart by whether it has an `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Notification(Notification),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Client {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Initialize {
    /// The version the client speaks.
    pub protocol: u32,
    #[serde(default)]
    pub client: Client,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Initialized {
    pub protocol: u32,
    /// `molt` and its version.
    pub server: Client,
    /// The one workspace this server works in; every request is about it.
    pub workspace: String,
    /// The methods it serves.
    pub methods: Vec<String>,
}

/// What `session.open` answers once the services are up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionOpened {
    /// Each service's live version, by its first eight hex digits.
    pub versions: Vec<(String, String)>,
    /// Why memory is not there; `None` when it is up.
    #[serde(default)]
    pub memory_problem: Option<String>,
    /// The model a task that names none runs on, as the gateway resolves it.
    #[serde(default)]
    pub default_model: Option<String>,
}

/// Forks to keep when the session closes, besides those `result.keep`
/// named; every other fork the session made is removed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Keep {
    #[serde(default)]
    pub keep: Vec<String>,
}

/// `task.design` and `task.submit`. `trace` is the run's id, chosen by the
/// client: its progress events carry it as `run`, and `task.steer`,
/// `task.pause` and the rest name the run by it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub trace: TraceId,
    /// The workspace in it is the server's; an empty one is filled in, and
    /// another is refused.
    pub request: RunRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub trace: TraceId,
    pub request: PlanRequest,
}

/// What `task.design` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Designed {
    pub design: DesignResponse,
    /// The check's files that already exist in the workspace, which it
    /// would replace: for a client that stops for a person there.
    #[serde(default)]
    pub existing: Vec<String>,
}

/// `task.pause`, `task.resume`, and `commit.prepare`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub trace: TraceId,
}

/// `task.steer`: what to add to the running run's requirements.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Steer {
    pub trace: TraceId,
    #[serde(flatten)]
    pub constraint: Constraint,
}

/// `question.answer`: the person's answer to question `question` of a
/// `progress` event `asked`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub trace: TraceId,
    pub question: u32,
    pub text: String,
}

/// `task.stop`: stop everything in flight, keeping the work. The services
/// stop with every command the agent started, and the session closes; open
/// another to go on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    /// Forks to keep, as for `session.close`.
    #[serde(default)]
    pub keep: Vec<String>,
    /// The forks of the attempts that were running: each is kept when it
    /// holds changes to more than `check_files`, and removed otherwise.
    #[serde(default)]
    pub attempts: Vec<String>,
    #[serde(default)]
    pub check_files: Vec<String>,
    /// A checkpointed run not to go on with: its checkpoint is removed.
    #[serde(default)]
    pub drop_run: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stopped {
    /// The attempts' forks kept for their changes.
    pub kept: Vec<String>,
    /// What could not be told about the others.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Learn {
    pub trace: TraceId,
    /// `None`: memory's default model.
    #[serde(default)]
    pub model: Option<String>,
}

/// `result.diff`, `result.keep` and `result.discard`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fork {
    pub fork: String,
}

/// `result.apply`: `paths` of the result kept in `fork`, or all of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Apply {
    pub fork: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recheck {
    pub fork: String,
    /// The files to check; `None` for all the result changes.
    #[serde(default)]
    pub only: Option<Vec<String>>,
    #[serde(default)]
    pub check: Option<CheckSpec>,
}

/// `commit.make`: commit the applied result of run `trace`, which
/// `commit.prepare` named before the run started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub trace: TraceId,
    /// The task, for the commit message.
    pub task: String,
    pub response: Box<RunResponse>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Undo {
    /// The run to take back; `None` for the newest not taken back yet.
    #[serde(default)]
    pub run: Option<String>,
}

/// A stopped run of the workspace, as its checkpoint has it, its
/// conversations left out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoppedRun {
    pub run: String,
    /// The checkpoint, or why it cannot be used.
    pub saved: Result<molt_planner::checkpoint::Saved, String>,
    /// Forks of attempts that would go on, and are gone.
    #[serde(default)]
    pub gone: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DropRun {
    pub run: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub query: String,
}

/// `notes.forget` and `notes.promote`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteReason {
    pub id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Forgot {
    pub forgotten: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Remember {
    pub text: String,
}

/// The id of the note kept, or of the copy made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteId {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeNote {
    pub review: ReviewRequest,
    /// New text, to correct the note; `None` to review it as it is.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsedNotes {
    pub run: String,
}

/// `command.run`: a command of the person's, run as the attempts' are, in a
/// fork of the workspace that is dropped after. Its output comes as
/// `progress` events `output` of `trace`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub trace: TraceId,
    pub command: String,
}

/// An empty result or parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

/// Read one message into `line`, without its newline. `false` at the end
/// of the input; an error for a message past [`MAX_LINE`] or one the input
/// ends in the middle of.
pub async fn read_line<R: AsyncBufRead + Unpin>(input: &mut R, line: &mut Vec<u8>) -> std::io::Result<bool> {
    line.clear();
    let read = input.take(MAX_LINE as u64 + 1).read_until(b'\n', line).await?;
    match line.pop() {
        None if read == 0 => Ok(false),
        Some(b'\n') => Ok(true),
        _ if line.len() >= MAX_LINE => Err(std::io::Error::other("a message is longer than 16 MiB")),
        _ => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the input ended in the middle of a message")),
    }
}

/// Write `value` as one message: compact JSON, which has no newline in it, and a newline.
pub async fn write_line<W: AsyncWrite + Unpin>(out: &mut W, value: &impl Serialize) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_LINE {
        return Err(std::io::Error::other("a message is longer than 16 MiB"));
    }
    bytes.push(b'\n');
    out.write_all(&bytes).await?;
    out.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use sha2::{Digest, Sha256};

    /// Version 1's messages: what each method takes and answers, and progress
    /// events, one JSON object a line. It is never edited: a client written
    /// against it must keep working, so this test reads every line with the
    /// types of today. A change that would break one is a new version.
    const V1: &str = include_str!("testdata/protocol-v1.jsonl");
    const V1_SHA256: &str = "89a009a3310ec608418b994f02b0e0c3680f1d58019118c5b520a0ab3aaf896c";

    /// Whether `new` has everything `old` has: each field, with the same
    /// value or one that has everything it had.
    fn covers(new: &Value, old: &Value, at: &str) -> Result<(), String> {
        match (new, old) {
            (Value::Object(new), Value::Object(old)) => old.iter().try_for_each(|(key, value)| {
                let field = format!("{at}.{key}");
                covers(new.get(key).ok_or_else(|| format!("{field} is gone"))?, value, &field)
            }),
            (Value::Array(new), Value::Array(old)) if new.len() == old.len() => {
                new.iter().zip(old).enumerate().try_for_each(|(i, (new, old))| covers(new, old, &format!("{at}[{i}]")))
            }
            (new, old) if new == old => Ok(()),
            (new, old) => Err(format!("{at} was {old} and is {new}")),
        }
    }

    fn reads<T: DeserializeOwned + Serialize>(old: &Value, at: &str) -> Result<(), String> {
        let typed: T = serde_json::from_value(old.clone()).map_err(|e| format!("{at} does not read: {e}"))?;
        covers(&serde_json::to_value(&typed).unwrap(), old, at)
    }

    fn method_reads(name: &str, params: &Value, result: &Value) -> Result<(), String> {
        use molt_api::fs::{DiffResponse, MergeResponse};
        use molt_api::memory::{ConsolidateResponse, RecallSnapshot, Recalled, ReviewResponse};
        use molt_api::plan::PlanResponse;
        use molt_api::planner::ControlResponse;
        macro_rules! both {
            ($params:ty, $result:ty) => {
                reads::<$params>(params, &format!("{name} params"))
                    .and_then(|()| reads::<$result>(result, &format!("{name} result")))
            };
        }
        match name {
            method::INITIALIZE => both!(Initialize, Initialized),
            method::SESSION_OPEN => both!(Empty, SessionOpened),
            method::SESSION_CLOSE => both!(Keep, Empty),
            method::TASK_DESIGN => both!(Task, Designed),
            method::TASK_SUBMIT => both!(Task, RunResponse),
            method::TASK_PLAN => both!(Plan, PlanResponse),
            method::TASK_STEER => both!(Steer, ControlResponse),
            method::TASK_PAUSE | method::TASK_RESUME => both!(Run, ControlResponse),
            method::QUESTION_ANSWER => both!(Answer, ControlResponse),
            method::TASK_STOP => both!(Stop, Stopped),
            method::TASK_LEARN => both!(Learn, ConsolidateResponse),
            method::RESULT_DIFF => both!(Fork, DiffResponse),
            method::RESULT_APPLY => both!(Apply, MergeResponse),
            method::RESULT_RECHECK => both!(Recheck, crate::session::Rechecked),
            method::RESULT_KEEP | method::RESULT_DISCARD => both!(Fork, Empty),
            method::COMMIT_PREPARE => both!(Run, Empty),
            method::COMMIT_MAKE => both!(Commit, Option<crate::git::Committed>),
            method::RUNS_LIST => both!(Empty, Vec<StoppedRun>),
            method::RUNS_DROP => both!(DropRun, Empty),
            method::RUNS_UNDO => both!(Undo, crate::session::Undone),
            method::NOTES_LIST => both!(Query, Vec<Recalled>),
            method::NOTES_FORGET => both!(NoteReason, Forgot),
            method::NOTES_REMEMBER => both!(Remember, NoteId),
            method::NOTES_PROMOTE => both!(NoteReason, NoteId),
            method::NOTES_CHANGE => both!(ChangeNote, ReviewResponse),
            method::NOTES_USED => both!(UsedNotes, Vec<RecallSnapshot>),
            method::COMMAND_RUN => both!(Command, molt_api::shell::RunResponse),
            method::COMMAND_STOP => both!(Run, Empty),
            other => Err(format!("{other} is not a method any more")),
        }
    }

    #[test]
    fn every_message_of_version_1_still_reads_and_keeps_every_field() {
        let mut seen = Vec::new();
        let mut problems = Vec::new();
        for (n, line) in V1.lines().enumerate() {
            let entry: Value = serde_json::from_str(line).unwrap();
            let result = match (&entry["method"], &entry["notification"]) {
                (Value::String(name), _) => {
                    seen.push(name.clone());
                    method_reads(name, &entry["params"], &entry["result"])
                }
                (_, Value::String(name)) if name == method::PROGRESS => {
                    reads::<molt_api::progress::Progress>(&entry["params"], "progress")
                }
                _ => Err("neither a method nor a notification".into()),
            };
            if let Err(problem) = result {
                problems.push(format!("line {}: {problem}", n + 1));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        for name in method::ALL {
            assert!(seen.iter().any(|s| s == name), "{name} has no message in testdata/protocol-v1.jsonl");
        }
        let sha = hex::encode(Sha256::digest(V1.as_bytes()));
        assert_eq!(sha, V1_SHA256, "testdata/protocol-v1.jsonl is version 1 as released: add a new version instead");
    }

    #[test]
    fn a_message_is_one_line_and_a_longer_one_or_a_cut_one_is_refused() {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
            let mut out = Vec::new();
            write_line(&mut out, &serde_json::json!({"text": "two\nlines"})).await.unwrap();
            assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 1);
            let mut input = &out[..];
            let mut line = Vec::new();
            assert!(read_line(&mut input, &mut line).await.unwrap());
            assert_eq!(serde_json::from_slice::<Value>(&line).unwrap()["text"], "two\nlines");
            assert!(!read_line(&mut input, &mut line).await.unwrap(), "then the end");
            let mut cut = &b"{\"a\":"[..];
            assert_eq!(read_line(&mut cut, &mut line).await.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
            let long = vec![b'x'; MAX_LINE + 1];
            assert!(read_line(&mut &long[..], &mut line).await.unwrap_err().to_string().contains("16 MiB"));
            let mut exact = vec![b'x'; MAX_LINE];
            exact.push(b'\n');
            assert!(read_line(&mut &exact[..], &mut line).await.unwrap());
            assert_eq!(line.len(), MAX_LINE);
        });
    }
}
