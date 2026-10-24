//! Progress events the planner publishes on `topic:progress` while a run is
//! going. `run` is the trace id of the `planner.run` request, so a caller
//! can pick out its own run. Events are best effort: a slow subscriber may
//! miss some. The gateway publishes the model events, and the shell the
//! output of a command run with [`crate::shell::RunRequest::stream`].

use serde::{Deserialize, Serialize};

use crate::model::{Role, Usage};
use crate::planner::{AttemptStatus, Requirement, RunState};

/// Identity and ordering of one model call's preview events. Sequence 0 is
/// its start; gaps mean best-effort progress was omitted, not missing model
/// content. The final complete reply always contains the assembled message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelCall {
    pub run: String,
    pub attempt: Option<u32>,
    /// Who makes the call; absent from an older gateway or caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
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
    /// The gateway is about to retry a model call that failed, after
    /// `delay_ms`. `failure` says what failed in a few words, such as
    /// `model API 502`; this is retry `retry` of at most `retries`. New: an
    /// older reader does not know it.
    ModelRetry {
        run: String,
        /// The attempt the call belongs to, when the caller said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        /// Who makes the call, when the caller said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<Role>,
        call: String,
        failure: String,
        delay_ms: u64,
        retry: u32,
        retries: u32,
    },
    /// The done-check is settled. `command` is `None` for an unverified run,
    /// or one judged by its `rubric` (the criteria's words).
    CheckReady {
        run: String,
        command: Option<String>,
        files: Vec<String>,
        designed: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rubric: Vec<String>,
    },
    AttemptStarted {
        run: String,
        attempt: u32,
        /// The attempt's fork; absent when it could not be made, or from an
        /// older planner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork: Option<String>,
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
        /// empty when it passed or from an older planner. For a judged
        /// result, the criteria not met.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tail: String,
        /// A judge decided it against the run's rubric; no check command ran.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        judged: bool,
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
    /// A new version of the run's requirements was accepted.
    Required {
        run: String,
        #[serde(flatten)]
        requirement: Requirement,
    },
    /// An attempt took up `version` of the requirements before its turn `turn`.
    Adopted {
        run: String,
        attempt: u32,
        version: u32,
        turn: u32,
    },
    /// The run was paused or let go on, or what a pausing run waits for changed.
    State {
        run: String,
        state: RunState,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        waiting: Vec<String>,
    },
    /// Attempt `attempt` asked the person `text` and waits for the answer: a
    /// constraint on `planner.control` that answers `question`, numbered
    /// from 1 within the run.
    Asked {
        run: String,
        attempt: u32,
        question: u32,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
    /// What memory gave the run to start with: the notes recalled, best
    /// first, and the size of the project map.
    Recalled {
        run: String,
        notes: Vec<RecalledNote>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        map_tokens: Option<u32>,
    },
    /// The planner picked `workflow` for a run that named
    /// [`crate::workflow::AUTO`], before anything else ran. `reason` is
    /// short, and empty when the pick was not the model's (it failed, say).
    Routed {
        run: String,
        workflow: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
    },
    /// Output of a streamed `shell.run`, whose trace id `run` is: whole
    /// lines where it can, in the order read, numbered from 0 so a gap
    /// shows. `cut` is the last event of a command whose output went past
    /// what the shell streams; the reply still has its end.
    Output {
        run: String,
        seq: u64,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stderr: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cut: bool,
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
            Self::ModelRetry { run, .. }
            | Self::CheckReady { run, .. }
            | Self::AttemptStarted { run, .. }
            | Self::ToolCall { run, .. }
            | Self::CheckRan { run, .. }
            | Self::AttemptFinished { run, .. }
            | Self::Note { run, .. }
            | Self::Asked { run, .. }
            | Self::Required { run, .. }
            | Self::Adopted { run, .. }
            | Self::State { run, .. }
            | Self::Recalled { run, .. }
            | Self::Routed { run, .. }
            | Self::Output { run, .. } => run,
        }
    }

    /// Who makes the model call of a model or retry event, as a person
    /// reads it ([`crate::model::speaker`]).
    pub fn speaker(&self) -> Option<String> {
        match self {
            Self::ModelRetry { role, attempt, .. } => Some(crate::model::speaker(*role, *attempt)),
            _ => self.model_call().map(|c| crate::model::speaker(c.role, c.attempt)),
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

/// A retry as one line: `model API 502, retrying in 60s (2/4)`. `left` is
/// the wait still ahead, which a display counts down.
pub fn retry_line(failure: &str, left: std::time::Duration, retry: u32, retries: u32) -> String {
    let secs = left.as_secs_f64().ceil() as u64;
    let when = if secs == 0 { "retrying now".to_owned() } else { format!("retrying in {secs}s") };
    format!("{failure}, {when} ({retry}/{retries})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_reads_as_one_line_that_counts_down() {
        use std::time::Duration;
        assert_eq!(retry_line("model API 502", Duration::from_secs(60), 2, 4), "model API 502, retrying in 60s (2/4)");
        assert_eq!(
            retry_line("model API 502", Duration::from_millis(1_200), 1, 4),
            "model API 502, retrying in 2s (1/4)"
        );
        assert_eq!(retry_line("model API 502", Duration::ZERO, 1, 4), "model API 502, retrying now (1/4)");
    }

    #[test]
    fn a_check_event_from_an_older_planner_still_parses() {
        let old = r#"{"event":"check_ran","run":"r","attempt":1,"passed":false,"exit_code":2}"#;
        let event: Progress = serde_json::from_str(old).unwrap();
        assert_eq!(
            event,
            Progress::CheckRan {
                run: "r".into(),
                attempt: 1,
                passed: false,
                exit_code: Some(2),
                tail: String::new(),
                judged: false
            }
        );
        // An empty tail is left out, so older subscribers see what they always saw.
        assert_eq!(serde_json::to_string(&event).unwrap(), old);

        let old = r#"{"event":"attempt_started","run":"r","attempt":0}"#;
        let event: Progress = serde_json::from_str(old).unwrap();
        assert_eq!(event, Progress::AttemptStarted { run: "r".into(), attempt: 0, fork: None });
        assert_eq!(serde_json::to_string(&event).unwrap(), old);
    }

    #[test]
    fn model_events_name_their_role_and_older_ones_still_parse() {
        let old = r#"{"event":"model_started","run":"r","attempt":null,"turn":1,"call":"c","seq":0}"#;
        let event: Progress = serde_json::from_str(old).unwrap();
        assert_eq!(event.model_call().unwrap().role, None);
        assert_eq!(serde_json::to_string(&event).unwrap(), old, "no role is left out, as before");
        assert_eq!(event.speaker().as_deref(), Some("planner"));

        let new = r#"{"event":"model_started","run":"r","attempt":null,"role":"reviewer","turn":1,"call":"c","seq":0}"#;
        let event: Progress = serde_json::from_str(new).unwrap();
        assert_eq!(event.speaker().as_deref(), Some("reviewer"));
        assert_eq!(serde_json::to_string(&event).unwrap(), new);

        let retry = r#"{"event":"model_retry","run":"r","role":"judge","call":"c","failure":"model API 502","delay_ms":1,"retry":1,"retries":4}"#;
        assert_eq!(serde_json::from_str::<Progress>(retry).unwrap().speaker().as_deref(), Some("judge"));
        // A role from a newer peer does not make the event unreadable.
        let later = new.replace("reviewer", "auditor");
        assert_eq!(serde_json::from_str::<Progress>(&later).unwrap().model_call().unwrap().role, Some(Role::Other));
    }

    #[test]
    fn attempts_are_counted_from_one_and_other_calls_by_their_role() {
        use crate::model::speaker;
        assert_eq!(speaker(Some(Role::Attempt), Some(0)), "attempt 1");
        assert_eq!(speaker(None, Some(2)), "attempt 3", "an older caller names only the attempt");
        assert_eq!(speaker(Some(Role::Designer), None), "designer");
        assert_eq!(speaker(Some(Role::Reviewer), None), "reviewer");
        assert_eq!(speaker(Some(Role::Judge), None), "judge");
        assert_eq!(speaker(Some(Role::Memory), None), "memory");
        assert_eq!(speaker(Some(Role::Explorer), Some(1)), "attempt 2 explorer");
        assert_eq!(speaker(None, None), "planner");
    }
}
