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
    /// Whether the start's `ready` text has appeared in its output; none
    /// when the start gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
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
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl RunResponse {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }
}
