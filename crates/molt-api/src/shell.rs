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

use serde::{Deserialize, Serialize};

pub const RUN: &str = "shell.run";

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

impl RunResponse {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }
}
