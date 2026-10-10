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
//! 4. **Apply.** The first attempt to pass wins, the others are cancelled,
//!    and the winner's changes are merged into the workspace.

use serde::{Deserialize, Serialize};

use crate::fs::Change;
use crate::model::{Effort, Usage};

pub const RUN: &str = "planner.run";
/// Design the done-check for a task without running it, so a person can
/// look at it first. Takes a [`RunRequest`] (only the task, workspace,
/// model, effort and budget matter) and answers a [`DesignResponse`]; a
/// `planner.run` with its command and files then skips the design.
pub const DESIGN: &str = "planner.design";

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
}

impl RunRequest {
    pub fn new(task: impl Into<String>, workspace: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            workspace: workspace.into(),
            check: None,
            check_files: Vec::new(),
            verify: true,
            attempts: two(),
            model: None,
            effort: None,
            max_turns: None,
            max_check_rounds: None,
            budget_usd: None,
            apply: true,
            stream: false,
        }
    }
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
    /// Why the designer settled on this check, or on none.
    #[serde(default)]
    pub rationale: String,
    /// The check run once on the workspace as it is. `None` without a
    /// check, or when it could not be run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    pub usage: Usage,
    pub cost_usd: f64,
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
    pub attempts: Vec<AttemptReport>,
    /// Totals over the designer and every attempt.
    pub usage: Usage,
    /// Known cost subtotal; see uncounted_calls for incomplete settlement.
    pub cost_usd: f64,
    /// Calls still pending, failed without usage, or returned without a known
    /// price. cost_usd is only a subtotal when this is nonzero.
    #[serde(default)]
    pub uncounted_calls: u32,
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
        pub rationale: String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_gets_defaults() {
        let req: RunRequest = serde_json::from_value(serde_json::json!({ "task": "t", "workspace": "/w" })).unwrap();
        assert_eq!(req, RunRequest::new("t", "/w"));
        assert_eq!(req.attempts, 2);
        assert!(req.apply);
        assert!(req.verify);
        // Requests from callers that know neither field stay as they were.
        let wire = serde_json::to_value(&req).unwrap();
        assert!(wire.get("verify").is_none() && wire.get("check_files").is_none(), "{wire}");
    }
}
