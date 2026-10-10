//! The `shell` service: run a command in a workspace.
//!
//! Commands run under `bash -c` (or `sh -c`) with the workspace as working
//! directory, no stdin, their own process group (killed whole on timeout),
//! and an environment without Molt or API secrets.

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
}

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
}

impl RunResponse {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
    }
}
