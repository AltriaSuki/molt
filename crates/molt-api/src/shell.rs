//! The `shell` service: run a command in a workspace.
//!
//! Commands run under `bash -c` (or `sh -c`) with the workspace as working
//! directory, no stdin unless the request gives one ([`RunRequest::stdin`]),
//! their own process group (killed whole on timeout) and an environment
//! without Molt or API secrets.
//!
//! A request that sets [`RunRequest::stream`] also has the command's output
//! published as it comes, on `topic:progress` as
//! [`crate::progress::Progress::Output`] events traced with the request, when
//! the service may publish there. Those events are best effort and bounded;
//! the reply holds the output as always.
//!
//! A command can also run on after the call that started it, as a process
//! (#110): [`START`] starts it, [`READ`] reads what it printed since the
//! last read, [`WRITE`] writes to its standard input and [`STOP`] stops it,
//! or every process of a workspace. A process runs as `run`'s commands do,
//! in the same sandbox, and belongs to the caller and the workspace that
//! started it: a call naming it from another workspace, or by another
//! caller, is told there is no such process. The service bounds how many
//! run, how long each may, and how much of its output it keeps.
//!
//! Output longer than a reply carries is saved whole, up to the service's
//! quota (#238): [`RunResponse::outputs`] and [`ProcessState::saved`] name
//! it by an opaque id, and [`OUTPUT`] reads it a page at a time, searches
//! it for a text, or follows a process's as it grows. A saved output
//! belongs to the caller and the workspace whose command printed it, as a
//! process does, and reading it runs nothing. What could not be saved is
//! said to be cut, with why; an output that expired or was evicted says so,
//! and is never read back as if it were whole.

use serde::{Deserialize, Serialize};

pub const RUN: &str = "shell.run";
/// Start a process: [`StartRequest`], answered with a [`ProcessState`].
pub const START: &str = "shell.start";
/// Read a process's new output: [`ReadRequest`], answered with a [`ProcessState`].
pub const READ: &str = "shell.read";
/// Write to a process's standard input: [`WriteRequest`], answered with a [`ProcessState`].
pub const WRITE: &str = "shell.write";
/// Stop a process, or all of a workspace's: [`StopRequest`], answered with a [`StopResponse`].
pub const STOP: &str = "shell.stop";
/// Read, search or follow a saved output: [`OutputRequest`], answered with an [`OutputPage`].
pub const OUTPUT: &str = "shell.output";

/// What the message of a `shell.run` or `shell.start` error starts with
/// when the command never started because the host ran short of something
/// it needs: processes, open files, memory or namespaces. The rest names
/// the resource and what the host had. The service already tried again a
/// few times; a change to the code cannot help, freeing the resource can.
/// Answered with the error code `busy`.
pub const SHORT_OF: &str = "short of host resources: ";
/// What the message starts with when the command never started because the
/// service already runs as many commands as it may and has as many waiting
/// as it queues. Answered with the error code `busy`; it can work once
/// others finish.
pub const QUEUE_FULL: &str = "the shell's queue is full: ";
/// What the message starts with when the command never started because
/// this host cannot isolate commands (no bubblewrap, no user namespaces)
/// or its security policy refuses to. Nothing falls back to running the
/// command unconfined; only the user can change this. Answered with the
/// error code `unavailable`.
pub const NO_ISOLATION: &str = "isolation unavailable: ";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    /// A directory inside one of the service's roots (a workspace or fork).
    pub workspace: String,
    pub command: String,
    /// Default and maximum are set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Publish the output while the command runs (see the module docs).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// Text for the command's standard input, which is closed after it;
    /// without it the command reads nothing. At most [`MAX_STDIN`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
}

/// The most standard input a request may give.
pub const MAX_STDIN: usize = 4 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResponse {
    /// `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
    /// Output was cut to the service's limit; the start and the end are kept.
    pub truncated: bool,
    pub duration_ms: u64,
    /// The command ran without network and failed with output that looks
    /// like a dependency it could not download.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub offline_failure: bool,
    /// The streams that were cut in the reply, saved whole (or as far as
    /// the quota allowed) for [`OUTPUT`]. A cut stream that is not here was
    /// not saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<SavedOutput>,
}

/// The most input one [`WriteRequest`] may give.
pub const MAX_INPUT: usize = 64 << 10;
/// The longest [`StartRequest::ready`] text.
pub const MAX_READY: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    /// A directory inside one of the service's roots, as for [`RunRequest`].
    pub workspace: String,
    pub command: String,
    /// How long it may run before it is stopped. Default and maximum are set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_ms: Option<u64>,
    /// Text whose appearance in the output means the process is ready,
    /// such as a server's "listening on". At most [`MAX_READY`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
    /// How long the call waits for `ready`, or without it for the process
    /// to exit, before it answers with what it printed so far. Default and
    /// maximum are set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// A TCP port it listens on. It is ready once a socket listens there
    /// in the network it runs on (with `ready` too, once both hold), and
    /// each answer says whether one still does
    /// ([`ProcessState::listening`]). In the sandbox without the network,
    /// that is its workspace's own: the same port in another workspace is
    /// another socket, which it neither takes nor reaches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    /// The workspace the process was started in.
    pub workspace: String,
    pub id: String,
    /// How long to wait for output when there is none yet; none answers at
    /// once. The maximum is set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRequest {
    /// The workspace the process was started in.
    pub workspace: String,
    pub id: String,
    /// At most [`MAX_INPUT`] bytes.
    pub input: String,
    /// Close its standard input after the text, so it reads to the end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopRequest {
    /// The workspace the process was started in.
    pub workspace: String,
    /// The process to stop; none stops every process of the workspace, and
    /// forgets them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopResponse {
    /// The processes stopped, each with the output it printed since it was last read.
    pub stopped: Vec<ProcessState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    /// It ended by itself, or was killed by a signal it did not get from the service.
    Exited,
    /// It was stopped: asked to, its workspace went away, or the service stopped.
    Stopped,
    /// It ran for as long as it was allowed and was stopped.
    Expired,
}

/// A process as a call left it, with the output it printed since it was
/// last read. Its output is standard output and standard error together,
/// in the order they were read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessState {
    /// Opaque, and good only in the workspace and for the caller that started it.
    pub id: String,
    pub command: String,
    pub status: ProcessStatus,
    /// Set once it exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Whether the start's `ready` text has appeared in its output, and
    /// something listened on its `port`; none when the start gave neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    /// Whether a socket listens on the start's `port` now, in the network
    /// it runs on: false once it ended, or when what listened went away
    /// (a server that crashed). None when the start gave no port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listening: Option<bool>,
    pub output: String,
    /// Where `output` starts, counted in bytes of everything it printed:
    /// one read's `next` is the following read's `offset`.
    pub offset: u64,
    pub next: u64,
    /// Bytes between `offset` and `next` that are not in `output`: the
    /// service no longer had them, or they were cut from the middle.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
    pub elapsed_ms: u64,
    /// Its whole output, saved once it printed more than the service keeps
    /// in memory, for [`OUTPUT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<SavedOutput>,
}

/// Which output of a command a saved output holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
    /// A process's standard output and error together, as they were read.
    Combined,
}

impl OutputStream {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Combined => "output",
        }
    }
}

/// Why a saved output holds less than the command printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCut {
    /// It reached the most one output may keep.
    OutputLimit,
    /// The service's outputs together reached their quota, and none it
    /// could give up made room.
    QuotaFull,
    /// The disk had less free space than the service leaves.
    DiskLow,
    /// Writing it failed.
    WriteFailed,
}

impl OutputCut {
    pub fn describe(self) -> &'static str {
        match self {
            Self::OutputLimit => "it reached the most one saved output may hold",
            Self::QuotaFull => "the shell service's saved outputs reached their disk quota",
            Self::DiskLow => "the disk is nearly full",
            Self::WriteFailed => "writing it to disk failed",
        }
    }
}

/// How the command whose output was saved ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    /// It was cancelled, or stopped before it ended by itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

/// An output the service saved, as it stood when the reply was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedOutput {
    /// Opaque, and good only in the workspace and for the caller whose
    /// command printed it.
    pub id: String,
    pub stream: OutputStream,
    pub command: String,
    /// Bytes the command printed on the stream so far.
    pub total: u64,
    /// Bytes saved: the first `stored` of `total`.
    pub stored: u64,
    /// Why `stored` is less than `total`, once it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<OutputCut>,
    /// More may still come: the process runs on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open: bool,
    /// How the command ended; none while it runs, and when that is not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<OutputEnd>,
    /// SHA-256 of the saved bytes, in hex, once no more come.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The trace and the id of the request that ran the command, to find
    /// it in the audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// How long it is kept after no more comes, in seconds.
    pub kept_s: u64,
}

impl SavedOutput {
    /// All the command printed is saved.
    pub fn whole(&self) -> bool {
        self.cut.is_none() && self.stored == self.total
    }
}

/// The most text one [`OutputRequest`] returns.
pub const MAX_OUTPUT_PAGE: u32 = 64 << 10;
/// The most matches one search returns.
pub const MAX_OUTPUT_MATCHES: u32 = 100;
/// The longest search text.
pub const MAX_OUTPUT_SEARCH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRequest {
    /// The workspace the command ran in.
    pub workspace: String,
    pub id: String,
    /// The byte to start at: to read from, or to search from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Most bytes of text to return, at most [`MAX_OUTPUT_PAGE`]; a
    /// default set by the service when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Find the lines holding this text instead of reading, at most
    /// [`MAX_OUTPUT_SEARCH`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub case_insensitive: bool,
    /// Most matches, at most [`MAX_OUTPUT_MATCHES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<u32>,
    /// While it is open, how long to wait for output past `offset` when
    /// there is none yet; none answers at once. The maximum is set by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

/// A page of a saved output, or the lines a search found in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPage {
    pub output: SavedOutput,
    /// Where `text`, or the search, starts.
    pub offset: u64,
    /// Where the next page, or the rest of the search, starts.
    pub next: u64,
    /// The bytes from `offset` to `next`; empty for a search.
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<OutputMatch>,
    /// The search stopped at its most matches: more may follow from `next`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

/// A line a search found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMatch {
    /// From 1.
    pub line: u64,
    /// Where the line starts.
    pub offset: u64,
    /// The line, cut when long.
    pub text: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl RunResponse {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }
}
