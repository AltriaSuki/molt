//! The `planner` service: carry out one task end to end.
//!
//! A run works in four steps:
//!
//! 1. **Check first.** The done-check is a shell command whose exit status 0
//!    means the task is done. If the caller gives none, a designer agent
//!    writes one (possibly with new test files) before any work starts. A
//!    caller can also ask for no check: one attempt then does the task and
//!    its result is returned unverified.
//! 2. **Parallel attempts.** Each attempt is an agent loop (model and tools)
//!    in its own fork of the workspace, with its own approach hint.
//! 3. **Verify.** When an attempt says it is done, the planner restores the
//!    check files (so an attempt cannot weaken the check) and runs the check
//!    in that attempt's fork. A failure goes back to the attempt as feedback.
//! 4. **Select.** By default the first attempt to pass wins and the others
//!    are cancelled. With [`Select::Wait`] or [`Select::All`] the others get
//!    time to pass too, and the passing candidates are compared on signals
//!    that can be audited (see [`Selection`]).
//! 5. **Apply.** The winner's changes are merged into the workspace.

use serde::{Deserialize, Serialize};

use crate::fs::Change;
use crate::model::{Effort, Usage};

pub const RUN: &str = "planner.run";
/// Design the done-check for a task without running it, so a person can
/// look at it first. Takes a [`RunRequest`] (only the task, workspace,
/// model, effort and budget matter) and answers a [`DesignResponse`]; a
/// `planner.run` with its command and files then skips the design.
pub const DESIGN: &str = "planner.design";
/// Steer a run while it goes: add to what it must do, pause it, or let it go
/// on. Takes a [`ControlRequest`] and answers a [`ControlResponse`]. Only a
/// run still going on the planner that answers can be steered.
pub const CONTROL: &str = "planner.control";

fn two() -> u32 {
    2
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    pub task: String,
    /// The directory to work on (inside the `fs` and `shell` services' root).
    pub workspace: String,
    /// Done-check command. `None`: the planner designs one first, unless
    /// `verify` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Files `check` depends on, as `planner.design` returned them: written
    /// into every attempt's fork and restored before each check run. Only
    /// with `check`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_files: Vec<CheckFile>,
    /// The project's own test command, as `planner.design` returned it: run
    /// on each passing candidate to compare them. Only with `check`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    /// How to pick among attempts that pass.
    #[serde(default, skip_serializing_if = "Select::is_first")]
    pub select: Select,
    /// False: run without a done-check, one attempt, result unverified. Only
    /// without `check`.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub verify: bool,
    /// Parallel attempts, 1 to 8.
    #[serde(default = "two")]
    pub attempts: u32,
    /// Model for the attempts and the designer (`opus`, `sonnet`, `haiku` or an id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Model turns per attempt. Default set by the planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Check runs per attempt. Each failed run but the last goes back to the
    /// attempt as feedback; when the last fails, the attempt has failed.
    /// Default set by the planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_check_rounds: Option<u32>,
    /// What the attempts' file writes and edits do about syntax errors they
    /// introduce (see [`crate::fs::SyntaxCheck`]). Default set by the planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax_check: Option<crate::fs::SyntaxCheck>,
    /// Spending limit for the whole run in US dollars. Default set by the
    /// planner. Calls to a model whose prices the gateway does not know are
    /// not counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// Merge the winning attempt into the workspace. When false, the winner's
    /// fork is kept and its path returned.
    #[serde(default = "yes")]
    pub apply: bool,
    /// Publish model text previews while calls are in flight.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// Have a reviewer look at a winner that passed its check, for changes
    /// that game the check, before it is applied. With `apply`, only an
    /// approved result is merged.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub review: bool,
    /// Save a checkpoint as the run goes, so that a run stopped before it
    /// finished (interrupted, out of budget, killed) can go on with
    /// `resume`. Needs a planner that keeps checkpoints.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checkpoint: bool,
    /// Go on with the checkpointed run named here rather than start one. The
    /// task, check, attempts, model and limits are the checkpoint's; `apply`,
    /// `stream` and a `budget_usd` given here replace its own, and the
    /// workspace must be its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Earlier tasks of the same conversation, oldest first, given to the
    /// model as context. The run still has its own check and budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<Earlier>,
}

/// A task done before this one in the same conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Earlier {
    pub task: String,
    pub outcome: Outcome,
    /// What the run said it did, or why it failed.
    #[serde(default)]
    pub summary: String,
    /// The paths its result changed.
    #[serde(default)]
    pub changes: Vec<String>,
    /// Its changes are in the workspace.
    #[serde(default)]
    pub applied: bool,
}

/// Which checkpointed run to go on with, and how.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The run's id: the trace it ran under.
    pub run: String,
    /// Leave the stopped attempts where they are and start new ones, with the
    /// same check and the cost so far.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry: bool,
}

/// How a run picks among its attempts that pass the done-check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Select {
    /// The first to pass wins and the others are cancelled.
    #[default]
    First,
    /// After the first passes, the others get `secs` more seconds to pass
    /// too; then the ones still going are cancelled and the passing ones
    /// compared.
    Wait { secs: u64 },
    /// Every attempt runs to its end; the passing ones are compared.
    All,
}

impl Select {
    pub fn is_first(&self) -> bool {
        *self == Select::First
    }
}

impl std::fmt::Display for Select {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Select::First => f.write_str("first"),
            Select::Wait { secs } => write!(f, "wait:{secs}"),
            Select::All => f.write_str("all"),
        }
    }
}

impl std::str::FromStr for Select {
    type Err = String;

    /// `first`, `all`, or `wait:<seconds>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "first" => Ok(Select::First),
            "all" => Ok(Select::All),
            other => other
                .strip_prefix("wait:")
                .and_then(|secs| secs.trim().parse().ok())
                .map(|secs| Select::Wait { secs })
                .ok_or_else(|| format!("{other:?} is not a way to select: use first, all or wait:<seconds>")),
        }
    }
}

impl RunRequest {
    pub fn new(task: impl Into<String>, workspace: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            workspace: workspace.into(),
            check: None,
            check_files: Vec::new(),
            suite: None,
            select: Select::First,
            verify: true,
            attempts: two(),
            model: None,
            effort: None,
            max_turns: None,
            max_check_rounds: None,
            syntax_check: None,
            budget_usd: None,
            apply: true,
            stream: false,
            checkpoint: false,
            resume: None,
            review: true,
            earlier: Vec::new(),
        }
    }
}

/// A change to a running run (see [`CONTROL`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest {
    /// The run's id: the trace of its `planner.run` request.
    pub run: String,
    #[serde(flatten)]
    pub action: Control,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Control {
    /// Add to what the run must do. It becomes the next version of the
    /// run's requirements: each attempt takes it at its next step (before
    /// its next model call), and a result found under an earlier version is
    /// not applied until the attempt has gone over it again.
    Constrain(Constraint),
    /// Start no new model call, command or check until [`Control::Resume`].
    /// What is already running finishes; the response says what that is.
    Pause,
    /// Go on after [`Control::Pause`].
    Resume,
}

/// What a [`Control::Constrain`] adds. At least one of its parts is set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    /// In the user's words, for the model.
    #[serde(default)]
    pub text: String,
    /// Workspace-relative files or directories the result must leave as
    /// they are. A result that changes one is not accepted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protect: Vec<String>,
    /// A new done-check command, in place of the one the run had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// One version of a run's requirements after the task itself, which is
/// version 0.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub version: u32,
    #[serde(flatten)]
    pub constraint: Constraint,
    /// When it was added, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    /// Paused, while calls already running finish.
    Pausing,
    /// Paused, and nothing is running.
    Paused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub state: RunState,
    /// The run's requirements version now.
    pub version: u32,
    /// What is running and cannot be stopped at once, as short descriptions.
    #[serde(default)]
    pub waiting: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// An attempt passed the done-check.
    Passed,
    /// No attempt passed (or the run stopped on its budget or an error).
    Failed,
    /// No automated check applies to this task; the single attempt's answer
    /// is returned without verification.
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub command: String,
    /// Workspace-relative files the designer wrote for the check. The planner
    /// restores them before every check run.
    #[serde(default)]
    pub files: Vec<String>,
    /// Written by the planner's designer rather than given by the caller.
    pub designed: bool,
    /// The project's own test command, run on each passing candidate when
    /// several are compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
}

/// A file a done-check depends on, with its full contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckFile {
    pub path: String,
    pub content: String,
}

/// How the designed check did on the workspace as it is, before any work.
/// A check that already passes cannot tell a finished task from an
/// unfinished one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The end of its output.
    #[serde(default)]
    pub tail: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesignResponse {
    /// The check command; `None` when no automated check fits the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The files the designer wrote for the check.
    #[serde(default)]
    pub files: Vec<CheckFile>,
    /// The project's own test command, when the designer found one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    /// Why the designer settled on this check, or on none.
    #[serde(default)]
    pub rationale: String,
    /// The check run once on the workspace as it is. `None` without a
    /// check, or when it could not be run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    pub usage: Usage,
    /// Known subtotal, not a complete price when `uncounted_calls` is nonzero.
    pub cost_usd: f64,
    /// Pending replies, calls without usage, and replies without a known price.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncounted_calls: u32,
    /// Remaining effective task limit, including the planner's default.
    /// `None` on incomplete settlement or in a reply from an older planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_budget_usd: Option<f64>,
    /// Design failure, with settlement preserved. Callers must not run a
    /// failed design as an unverified task. Invalid requests still use errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Local design work stopped; this does not confirm remote cancellation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What the reviewer made of a winner's changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The changes do the task.
    Approve,
    /// Something needs a person's look before the changes are used.
    Concern,
    /// The changes game the check rather than do the task.
    Reject,
}

/// The review of a winner that passed its check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub verdict: Verdict,
    /// What it rests on, each citing the diff where it can: the fixed rules'
    /// findings first, then the reviewer model's.
    #[serde(default)]
    pub findings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Passed the done-check, or finished a run that has no check.
    Passed,
    /// Finished without passing the check, or ran out of turns or check rounds.
    Failed,
    /// Stopped because another attempt won or the budget ran out.
    Cancelled,
    /// A model refusal, or a model or tool error the attempt could not recover from.
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptReport {
    pub index: u32,
    pub status: AttemptStatus,
    pub turns: u32,
    pub check_runs: u32,
    pub usage: Usage,
    /// Known cost subtotal; see uncounted_calls for incomplete settlement.
    pub cost_usd: f64,
    /// Why it ended: the tail of the last failed check, an error, or empty.
    #[serde(default)]
    pub note: String,
    /// The version of the run's requirements it last worked to.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub requirements: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunResponse {
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSpec>,
    /// Index of the attempt whose result is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<u32>,
    /// The winning attempt's final message, or why the run failed.
    pub summary: String,
    pub changes: Vec<Change>,
    /// Unified diff of the winner's changes.
    pub patch: String,
    /// `patch` was cut to the fs service's size limit; `changes` is complete.
    #[serde(default)]
    pub patch_truncated: bool,
    /// The changes were merged into the workspace.
    pub applied: bool,
    /// Where the winner's files are when they were not merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
    /// The winner's review; absent when there was none (no winner, no
    /// check, no changes, or review turned off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
    pub attempts: Vec<AttemptReport>,
    /// Totals over the designer and every attempt.
    pub usage: Usage,
    /// Known cost subtotal; see uncounted_calls for incomplete settlement.
    pub cost_usd: f64,
    /// Calls still pending, failed without usage, or returned without a known
    /// price. cost_usd is only a subtotal when this is nonzero.
    #[serde(default)]
    pub uncounted_calls: u32,
    /// The run stopped before it finished and its checkpoint is kept: the
    /// run id to resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// What was added to the run while it went, in order. `check` above is
    /// the one the result was checked with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<Requirement>,
    /// How the winner was picked when more than one attempt passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
}

/// How the passing attempts were compared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// The ranking rule, in words.
    pub rule: String,
    /// Every passing candidate, best first; the first is the winner unless
    /// the review held it back and a later one was approved.
    pub candidates: Vec<Candidate>,
}

/// A passing attempt and what it was compared on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub attempt: u32,
    /// The project's suite passed in its fork. `None`: there is no suite,
    /// or it could not be run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_passed: Option<bool>,
    /// Lines its diff adds and removes.
    pub lines_changed: u32,
    pub files_changed: u32,
    /// The diff was cut short, so `lines_changed` is a lower bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The review of it, when it was reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
}

/// Names and inputs of the tools the planner offers the model. Each maps to
/// one `fs` or `shell` call in the attempt's fork, or, for [`FIND_SYMBOL`]
/// and [`RECALL`] (offered when memory is up), one `memory` call about the
/// workspace.
pub mod tools {
    use serde::{Deserialize, Serialize};

    pub const READ_FILE: &str = "read_file";
    pub const WRITE_FILE: &str = "write_file";
    pub const EDIT_FILE: &str = "edit_file";
    pub const LIST_FILES: &str = "list_files";
    pub const SEARCH: &str = "search";
    pub const RUN: &str = "run";
    pub const FIND_SYMBOL: &str = "find_symbol";
    pub const RECALL: &str = "recall";
    /// Offered only to the check designer: the check it settled on.
    pub const SUBMIT_CHECK: &str = "submit_check";
    /// Offered only to the reviewer: its verdict.
    pub const SUBMIT_REVIEW: &str = "submit_review";

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ReadFile {
        pub path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WriteFile {
        pub path: String,
        pub content: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct EditFile {
        pub path: String,
        pub old_string: String,
        pub new_string: String,
        #[serde(default)]
        pub replace_all: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ListFiles {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub depth: Option<u32>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Search {
        pub pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub glob: Option<String>,
        #[serde(default)]
        pub case_insensitive: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Run {
        pub command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub timeout_s: Option<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FindSymbol {
        pub name: String,
        /// Also list where it is used. Default true.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub references: Option<bool>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Recall {
        pub query: String,
    }

    /// `command` is `None` when no automated check fits the task (a
    /// question to answer, say); the run is then unverified.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SubmitCheck {
        #[serde(default)]
        pub command: Option<String>,
        /// Files the designer wrote that the check depends on.
        #[serde(default)]
        pub files: Vec<String>,
        /// The project's own test command, when it has one.
        #[serde(default)]
        pub suite: Option<String>,
        pub rationale: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SubmitReview {
        pub verdict: super::Verdict,
        #[serde(default)]
        pub findings: Vec<String>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_design_replies_remain_readable_without_inventing_a_budget() {
        let wire = serde_json::json!({"usage":{}, "cost_usd":0.25});
        let response: DesignResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(response.uncounted_calls, 0);
        assert!(response.error.is_none() && response.remaining_budget_usd.is_none() && !response.cancelled);
        let wire = serde_json::to_value(response).unwrap();
        for key in ["uncounted_calls", "error", "remaining_budget_usd", "cancelled"] {
            assert!(wire.get(key).is_none());
        }
    }

    #[test]
    fn minimal_request_gets_defaults() {
        let req: RunRequest = serde_json::from_value(serde_json::json!({ "task": "t", "workspace": "/w" })).unwrap();
        assert_eq!(req, RunRequest::new("t", "/w"));
        assert_eq!(req.attempts, 2);
        assert!(req.apply);
        assert!(req.verify);
        // Requests from callers that know neither field stay as they were.
        assert!(req.review);
        let wire = serde_json::to_value(&req).unwrap();
        assert!(wire.get("verify").is_none() && wire.get("check_files").is_none(), "{wire}");
        assert!(wire.get("review").is_none(), "{wire}");
        assert!(wire.get("select").is_none() && wire.get("suite").is_none(), "{wire}");
    }

    #[test]
    fn select_reads_and_writes_its_three_modes() {
        for (text, select) in [("first", Select::First), ("all", Select::All), ("wait:30", Select::Wait { secs: 30 })] {
            assert_eq!(text.parse::<Select>().unwrap(), select);
            assert_eq!(select.to_string(), text);
        }
        assert!("wait".parse::<Select>().is_err() && "wait:x".parse::<Select>().is_err());
        let wire = serde_json::to_value(Select::Wait { secs: 5 }).unwrap();
        assert_eq!(wire, serde_json::json!({"mode":"wait","secs":5}));
    }
}
