//! Progress events the planner publishes on `topic:progress` while a run is
//! going. `run` is the trace id of the `planner.run` request, so a caller
//! can pick out its own run. Events are best effort: a slow subscriber may
//! miss some.

use serde::{Deserialize, Serialize};

use crate::planner::AttemptStatus;

pub const TOPIC: &str = "progress";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress {
    /// The done-check is settled. `command` is `None` for an unverified run.
    CheckReady {
        run: String,
        command: Option<String>,
        files: Vec<String>,
        designed: bool,
    },
    AttemptStarted {
        run: String,
        attempt: u32,
    },
    /// The model asked for a tool. `detail` is a short one-line description.
    ToolCall {
        run: String,
        attempt: u32,
        turn: u32,
        tool: String,
        detail: String,
    },
    CheckRan {
        run: String,
        attempt: u32,
        passed: bool,
        exit_code: Option<i32>,
    },
    AttemptFinished {
        run: String,
        attempt: u32,
        status: AttemptStatus,
    },
    Note {
        run: String,
        message: String,
    },
}

impl Progress {
    pub fn run(&self) -> &str {
        match self {
            Self::CheckReady { run, .. }
            | Self::AttemptStarted { run, .. }
            | Self::ToolCall { run, .. }
            | Self::CheckRan { run, .. }
            | Self::AttemptFinished { run, .. }
            | Self::Note { run, .. } => run,
        }
    }
}
