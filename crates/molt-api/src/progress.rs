//! Progress events the planner publishes on `topic:progress` while a run is
//! going. `run` is the trace id of the `planner.run` request, so a caller
//! can pick out its own run. Events are best effort: a slow subscriber may
//! miss some.

use serde::{Deserialize, Serialize};

use crate::model::Usage;
use crate::planner::AttemptStatus;

/// Identity and ordering of one model call's preview events. Sequence 0 is
/// its start; gaps mean best-effort progress was omitted, not missing model
/// content. The final complete reply always contains the assembled message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelCall {
    pub run: String,
    pub attempt: Option<u32>,
    pub turn: u32,
    pub call: String,
    pub seq: u64,
}

pub const TOPIC: &str = "progress";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress {
    ModelStarted {
        #[serde(flatten)]
        context: ModelCall,
    },
    ModelText {
        #[serde(flatten)]
        context: ModelCall,
        text: String,
    },
    ModelFinished {
        #[serde(flatten)]
        context: ModelCall,
        /// None on an interrupted or malformed response.
        usage: Option<Usage>,
        cost_usd: Option<f64>,
        error: Option<String>,
    },
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
        /// The end of a failed check's output, a few hundred bytes at most;
        /// empty when it passed or from an older planner.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tail: String,
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
    /// What memory gave the run to start with: the notes recalled, best
    /// first, and the size of the project map.
    Recalled {
        run: String,
        notes: Vec<RecalledNote>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        map_tokens: Option<u32>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecalledNote {
    pub id: String,
    pub text: String,
    pub confidence: f64,
}

impl Progress {
    pub fn run(&self) -> &str {
        match self {
            Self::ModelStarted { context } | Self::ModelText { context, .. } | Self::ModelFinished { context, .. } => {
                &context.run
            }
            Self::CheckReady { run, .. }
            | Self::AttemptStarted { run, .. }
            | Self::ToolCall { run, .. }
            | Self::CheckRan { run, .. }
            | Self::AttemptFinished { run, .. }
            | Self::Note { run, .. }
            | Self::Recalled { run, .. } => run,
        }
    }

    pub fn model_call(&self) -> Option<&ModelCall> {
        match self {
            Self::ModelStarted { context } | Self::ModelText { context, .. } | Self::ModelFinished { context, .. } => {
                Some(context)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_event_from_an_older_planner_still_parses() {
        let old = r#"{"event":"check_ran","run":"r","attempt":1,"passed":false,"exit_code":2}"#;
        let event: Progress = serde_json::from_str(old).unwrap();
        assert_eq!(
            event,
            Progress::CheckRan { run: "r".into(), attempt: 1, passed: false, exit_code: Some(2), tail: String::new() }
        );
        // An empty tail is left out, so older subscribers see what they always saw.
        assert_eq!(serde_json::to_string(&event).unwrap(), old);
    }
}
