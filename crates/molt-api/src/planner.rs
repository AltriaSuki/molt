//! The `planner` service: carry out one task end to end.
//!
//! A run works in four steps:
//!
//! 1. **Check first.** The done-check is a shell command whose exit status 0
//!    means the task is done. If the caller gives none, a designer agent
//!    writes one (possibly with new test files) before any work starts. For
//!    a task no command can check (research, writing), the designer gives a
//!    rubric instead: criteria an independent judge decides one by one (see
//!    [`Criterion`]). A caller can also ask for no check: one attempt then
//!    does the task and its result is returned unverified.
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
/// model, effort, budget and design budget matter) and answers a [`DesignResponse`]; a
/// `planner.run` with its command and files then skips the design.
pub const DESIGN: &str = "planner.design";
/// Steer a run while it goes: add to what it must do, pause it, or let it go
/// on. Takes a [`ControlRequest`] and answers a [`ControlResponse`]. Only a
/// run still going on the planner that answers can be steered.
pub const CONTROL: &str = "planner.control";
/// What a run or design that stopped because designing used up its share
/// of the budget says to do next, in `molt do`'s flags. A front end with
/// other controls can put its own in place of this text.
pub const DESIGN_BUDGET_NEXT: &str = "Give the check yourself with --check CMD, or the criteria a judge should \
     decide with --criterion TEXT (repeatable); run unverified with --no-check; or allow more for designing with \
     --design-budget-usd USD (at most the run's budget).";

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
    /// Criteria the result must meet, as `planner.design` returned them, for
    /// a task no command can check: an independent judge decides each.
    /// Only without `check`, and with `verify`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rubric: Vec<Criterion>,
    /// Model for the judge of a rubric. Default: the attempts' model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
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
    /// The most designing the check may spend, in US dollars: the
    /// designer's and the vetter's model calls together. It comes out of
    /// the run's budget and is at most that. Default: a share of the run's
    /// budget set by the planner. Like the run's budget, it is checked
    /// before each model call, so the last one can go past it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_budget_usd: Option<f64>,
    /// Merge the winning attempt into the workspace. When false, the winner's
    /// fork is kept and its path returned.
    #[serde(default = "yes")]
    pub apply: bool,
    /// Publish model text previews while calls are in flight.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// A person follows the run and can answer: attempts are offered
    /// [`tools::ASK_USER`], whose question the planner publishes as
    /// [`crate::progress::Progress::Asked`]. The answer comes back on
    /// [`CONTROL`] as a [`Constraint`] that [`Constraint::answers`] it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interactive: bool,
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
    /// When the caller cancels the run, leave the attempts' forks where they
    /// are instead of dropping them: the caller looks at what they changed,
    /// keeps the ones that hold work and drops the rest itself. A run that
    /// ends any other way, out of budget included, still drops them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_cancelled_forks: bool,
    /// Go on with the checkpointed run named here rather than start one. The
    /// task, check, attempts, model and limits are the checkpoint's; `apply`,
    /// `stream`, and a `budget_usd` or `design_budget_usd` given here replace
    /// its own, and the
    /// workspace must be its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Earlier tasks of the same conversation, oldest first, given to the
    /// model as context. The run still has its own check and budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<Earlier>,
    /// Workspace-relative files the user pointed at for this task (`@path`
    /// in the TUI). Their text goes to the designer and the attempts as
    /// project data; a file too large is named with its size only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// The workflow to run, by name: one the planner defines (see
    /// [`crate::workflow`]). A request names one and never defines one.
    /// `None`: [`crate::workflow::VERIFY`], which the fields above configure.
    /// [`crate::workflow::AUTO`] has the planner pick one for the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Go on from the previous task of the conversation: its whole
    /// conversation with the model comes before this task, and its kept
    /// result, when there is one, is where the attempt works.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<Continuation>,
}

/// What a task goes on from (see [`RunRequest::continues`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Continuation {
    /// The previous task's conversation, as its reply's
    /// [`RunResponse::conversation`] gave it. The planner leaves out its
    /// oldest turns when it does not fit the model's input.
    #[serde(default)]
    pub conversation: Vec<serde_json::Value>,
    /// The fork its result was kept in, when it was not applied: the
    /// attempt works there, so one attempt runs. `None`: a new fork of the
    /// workspace, which holds the result when it was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<String>,
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
            rubric: Vec::new(),
            judge_model: None,
            verify: true,
            attempts: two(),
            model: None,
            effort: None,
            max_turns: None,
            max_check_rounds: None,
            syntax_check: None,
            budget_usd: None,
            design_budget_usd: None,
            apply: true,
            stream: false,
            interactive: false,
            checkpoint: false,
            keep_cancelled_forks: false,
            resume: None,
            review: true,
            earlier: Vec::new(),
            files: Vec::new(),
            workflow: None,
            continues: None,
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
    /// The question (of a [`crate::progress::Progress::Asked`]) that `text`
    /// answers. The attempt that asked waits for it; every attempt follows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<u32>,
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
    /// An attempt met every criterion of the run's rubric, as an independent
    /// model judge (and the criteria's own commands) found. Weaker evidence
    /// than a passing check. New: an older reader does not know it.
    Judged,
}

/// One thing a result must satisfy, for a task no single command can check:
/// a statement that can be decided true or false by looking at the result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    pub text: String,
    /// A command, run from the workspace root, that exits 0 when the
    /// criterion is met. Without one, the judge model decides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// How a result did on one criterion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The criterion, as the rubric words it.
    pub criterion: String,
    pub met: bool,
    /// The judge's reason, or the end of the command's output.
    #[serde(default)]
    pub reason: String,
    /// Decided by the criterion's command rather than the judge model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_command: bool,
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
    /// Without a command: the criteria the designer gave instead, for an
    /// independent judge. Empty when there is nothing to judge either.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rubric: Vec<Criterion>,
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
    /// The workflow the planner picked, for a request that named
    /// [`crate::workflow::AUTO`]. Another than `verify` designs nothing:
    /// run the task with it. `None` for any other request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
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
    /// Optional source information; older peers only have the combined findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ReviewDetails>,
}

/// Fixed rules and the independent model opinion, without changing the aggregate verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDetails {
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<Verdict>,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Passed the done-check, or finished a run that has no check.
    Passed,
    /// Finished without passing the check, or ran out of turns or check rounds.
    Failed,
    /// Stopped because another attempt won or the budget ran out, or because
    /// it disputed the check (its report's `dispute` says so).
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
    /// It stopped because it found the check contradicting the task. Such
    /// an attempt is reported as cancelled, not failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispute: Option<CheckDispute>,
}

/// An attempt's claim that the done-check contradicts the task, with what a
/// person needs to decide it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDispute {
    /// The attempt that raised it.
    pub attempt: u32,
    /// The check's assertion, as specific as the attempt could make it.
    pub assertion: String,
    /// The words of the task (or of a file it points to) the assertion contradicts.
    pub task_quote: String,
    /// A command that shows the contradiction.
    pub reproduce: String,
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
    /// Build artifacts the winner's commands left in its fork, which are not
    /// among `changes` and are not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<crate::fs::LeftOut>,
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
    /// How the winner was picked when more than one attempt passed. Boxed,
    /// like `dispute`, to keep the reply small where it is rarely set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Box<Selection>>,
    /// The criteria the result was judged by, in a run with a rubric.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rubric: Vec<Criterion>,
    /// How the winner did on each criterion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judgement: Vec<Finding>,
    /// No attempt passed and one disputed the check: the first such dispute.
    /// The check needs a person's look before the task is run again. Boxed
    /// to keep the reply small where it is rarely set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispute: Option<Box<CheckDispute>>,
    /// How the result was verified, or would have been had an attempt
    /// passed. `None` only in a reply from an older planner, which did not
    /// say: unknown, not unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// The workflow the run went through, by name. `None` in a reply from
    /// an older planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The conversation of the attempt whose result this is, for a next
    /// task that goes on from it ([`Continuation`]); the oldest turns are
    /// left out to keep it within a model call. Empty without a winner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation: Vec<serde_json::Value>,
}

/// What a result was held to: the level of evidence behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// A done-check command, run by the planner in the result's fork.
    Command,
    /// A rubric: an independent model judge, and the criteria's own commands.
    Judged,
    /// Only the review's fixed rules over the diff, as the built-in `quick`
    /// does for a result without a check.
    Rules,
    /// Nothing checked the result.
    None,
}

impl Verification {
    pub fn name(self) -> &'static str {
        match self {
            Verification::Command => "command",
            Verification::Judged => "judged",
            Verification::Rules => "rules",
            Verification::None => "none",
        }
    }
}

impl std::fmt::Display for Verification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
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
    /// Criteria of the rubric it met, in a run with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria_met: Option<u32>,
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
/// workspace, or, for [`FETCH_URL`] (offered when the planner may call
/// `web.fetch`), one `web` call.
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
    pub const FETCH_URL: &str = "fetch_url";
    /// Offered only to attempts of an interactive run: ask the person a
    /// question, and wait a while for the answer.
    pub const ASK_USER: &str = "ask_user";
    /// Offered only to the check designer: the check it settled on.
    pub const SUBMIT_CHECK: &str = "submit_check";
    /// Offered only to the reviewer: its verdict.
    pub const SUBMIT_REVIEW: &str = "submit_review";
    /// Offered only to the judge of a rubric: its finding on each criterion.
    pub const SUBMIT_JUDGEMENT: &str = "submit_judgement";
    /// Offered only to the vetter of a designed check: where the check
    /// conflicts with the task.
    pub const SUBMIT_VETTING: &str = "submit_vetting";
    /// Offered only to attempts that run against a done-check: say the check
    /// contradicts the task, which ends the attempt.
    pub const DISPUTE_CHECK: &str = "dispute_check";
    /// Offered only to the call that picks a workflow for a run that names
    /// `auto`: the workflow, and why.
    pub const SUBMIT_ROUTE: &str = "submit_route";

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

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FetchUrl {
        pub url: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AskUser {
        pub question: String,
        /// Answers to pick from; the person may answer otherwise.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub options: Vec<String>,
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
        /// Without a command: criteria for an independent judge instead.
        #[serde(default)]
        pub rubric: Vec<super::Criterion>,
        pub rationale: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SubmitJudgement {
        pub findings: Vec<Judged>,
    }

    /// The judge's finding on one criterion, by its number from 1.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Judged {
        pub criterion: u32,
        pub met: bool,
        #[serde(default)]
        pub reason: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SubmitVetting {
        #[serde(default)]
        pub conflicts: Vec<Conflict>,
    }

    /// One assertion of a designed check that the task contradicts or gives
    /// no basis for.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Conflict {
        pub assertion: String,
        /// The task's words it conflicts with; empty when the task says nothing about it.
        #[serde(default)]
        pub task_quote: String,
        pub why: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DisputeCheck {
        pub assertion: String,
        pub task_quote: String,
        pub reproduce: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SubmitRoute {
        pub workflow: String,
        #[serde(default)]
        pub reason: String,
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
    #[test]
    fn old_review_payloads_keep_their_verdict_without_claiming_a_finding_source() {
        let wire = serde_json::json!({"verdict":"concern", "findings":["a rule or a model found this"]});
        let review: super::Review = serde_json::from_value(wire.clone()).unwrap();
        assert!(review.details.is_none());
        assert_eq!(serde_json::to_value(review).unwrap(), wire);
    }

    use super::*;

    #[test]
    fn a_request_without_files_reads_and_writes_as_before() {
        let wire = serde_json::json!({"task": "t", "workspace": "/w"});
        let req: RunRequest = serde_json::from_value(wire).unwrap();
        assert!(req.files.is_empty());
        assert!(serde_json::to_value(&req).unwrap().get("files").is_none());
        let req = RunRequest { files: vec!["src/lexer.rs".into()], ..req };
        assert_eq!(serde_json::to_value(&req).unwrap()["files"], serde_json::json!(["src/lexer.rs"]));
    }

    #[test]
    fn a_request_without_a_design_budget_reads_and_writes_as_before() {
        let wire = serde_json::json!({"task": "t", "workspace": "/w"});
        let req: RunRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(req.design_budget_usd, None);
        assert!(serde_json::to_value(&req).unwrap().get("design_budget_usd").is_none());
        let req = RunRequest { design_budget_usd: Some(0.5), ..req };
        assert_eq!(serde_json::to_value(&req).unwrap()["design_budget_usd"], serde_json::json!(0.5));
    }

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
    fn replies_without_a_dispute_read_and_write_as_before() {
        let report = serde_json::json!({
            "index": 0, "status": "failed", "turns": 1, "check_runs": 1, "usage": {}, "cost_usd": 0.5
        });
        let read: AttemptReport = serde_json::from_value(report.clone()).unwrap();
        assert!(read.dispute.is_none());
        assert!(serde_json::to_value(&read).unwrap().get("dispute").is_none());
        let disputed = AttemptReport {
            status: AttemptStatus::Cancelled,
            dispute: Some(CheckDispute {
                attempt: 0,
                assertion: "the error names column 20".into(),
                task_quote: "columns count Unicode characters".into(),
                reproduce: "cargo test --test spec".into(),
            }),
            ..read
        };
        let wire = serde_json::to_value(&disputed).unwrap();
        assert_eq!(wire["dispute"]["task_quote"], "columns count Unicode characters");
        assert_eq!(serde_json::from_value::<AttemptReport>(wire).unwrap(), disputed);
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
    fn a_request_without_a_workflow_reads_and_writes_as_before() {
        let wire = serde_json::json!({"task": "t", "workspace": "/w"});
        let req: RunRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(req.workflow, None);
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({"task": "t", "workspace": "/w", "attempts": 2, "apply": true})
        );
        let req = RunRequest { workflow: Some("verify".into()), ..req };
        assert_eq!(serde_json::to_value(&req).unwrap()["workflow"], "verify");
    }

    #[test]
    fn verification_levels_have_their_wire_names() {
        for (level, name) in [
            (Verification::Command, "command"),
            (Verification::Judged, "judged"),
            (Verification::Rules, "rules"),
            (Verification::None, "none"),
        ] {
            assert_eq!(serde_json::to_value(level).unwrap(), name);
            assert_eq!(level.to_string(), name);
        }
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
