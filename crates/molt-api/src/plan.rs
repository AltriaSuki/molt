//! `planner.plan`: a task carried out as ordered steps, each its own run
//! with its own check (issue #47; see `docs/plan.md`).
//!
//! A plan goes in two parts, as a checked run does with `planner.design`:
//!
//! 1. **Steps.** The planner's step designer splits the task into a few
//!    ordered steps, one goal sentence each. A front end that lets a person
//!    look first asks with [`PlanRequest::propose`] and gets the steps back
//!    without anything running; the person confirms or edits them.
//! 2. **Runs.** Each step is its own `planner.run` under the plan's trace:
//!    it designs its own check, races its attempts, and its winner is merged
//!    into the plan's result, a fork of the workspace the next step's
//!    attempts fork from. A step that does not pass stops the plan; the
//!    steps after it never start, and the result of the steps that passed
//!    is kept. Once every step has passed, the plan's overall check runs on
//!    the result, which is then applied, or kept, as a run's result is.
//!
//! One budget covers the whole plan: the step designer, the overall
//! check's designer, and every step's run, each of which gets what the
//! ones before it left.

use serde::{Deserialize, Serialize};

use crate::model::Usage;
use crate::planner::{RunRequest, RunResponse};

/// Plan a task and carry out its steps. Takes a [`PlanRequest`] and
/// answers a [`PlanResponse`].
pub const PLAN: &str = "planner.plan";
/// The most steps a plan has.
pub const MAX_STEPS: usize = 8;
/// The longest goal a step may have, in bytes.
pub const MAX_GOAL: usize = 1_000;
/// Offered only to the step designer: the plan's steps.
pub const SUBMIT_PLAN: &str = "submit_plan";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanRequest {
    /// The whole task, and what every step's run shares: the workspace,
    /// attempts, model, limits, workflow, `review` and `select`.
    /// `budget_usd` is the one budget of the whole plan. `check`, when
    /// given, is the plan's overall check, run once every step has passed;
    /// without one the planner designs one for the whole task, unless
    /// `verify` is false. `apply` is about the plan's result: each step's
    /// result always goes into it. `continues.fork` names the kept result
    /// of an earlier plan to go on in, as its reply's
    /// [`PlanResponse::remaining`] says.
    #[serde(flatten)]
    pub run: RunRequest,
    /// The steps to carry out, in order, one goal each. Empty: the step
    /// designer writes them first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
    /// The steps as the step designer proposed them, when `steps` is what a
    /// person made of them, so that what they changed is on record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed: Vec<String>,
    /// Only design the steps and answer with them; nothing runs. For a
    /// front end that shows them to a person first. Not with `steps`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub propose: bool,
}

impl PlanRequest {
    pub fn new(run: RunRequest) -> Self {
        Self { run, steps: Vec::new(), proposed: Vec::new(), propose: false }
    }
}

/// What `steps` say, cleaned: each goal trimmed. An error says what is
/// wrong with them, in a request's terms.
pub fn steps(steps: &[String]) -> Result<Vec<String>, String> {
    if steps.is_empty() {
        return Err("a plan has no steps".to_owned());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!("a plan has at most {MAX_STEPS} steps, not {}", steps.len()));
    }
    let mut clean = Vec::new();
    for (i, goal) in steps.iter().enumerate() {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err(format!("step {} has no goal", i + 1));
        }
        if goal.len() > MAX_GOAL {
            return Err(format!("step {} is longer than {MAX_GOAL} bytes: give one goal sentence", i + 1));
        }
        clean.push(goal.to_owned());
    }
    Ok(clean)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanResponse {
    /// The steps the plan goes by, in order: as proposed, as given, or as a
    /// person edited them. With `propose`, the proposal.
    pub steps: Vec<String>,
    /// The step designer's proposal, when `steps` is a person's edit of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed: Vec<String>,
    /// `steps` differ from `proposed`: a person edited the plan.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    /// Why the step designer split the task so.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
    /// Each step that ran, in order: its check and its result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ran: Vec<StepRun>,
    /// The steps not done, from the one that did not pass on. To go on
    /// from there, ask for these as `steps`, with the result's `fork` as
    /// `continues.fork`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remaining: Vec<String>,
    /// The plan's result, as a run's reply: the overall check, the changes
    /// of every step that passed, whether they were applied or where they
    /// are kept, and the totals. `None` with `propose`, or when the plan
    /// could not start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RunResponse>>,
    /// Totals over everything the plan spent, its steps included.
    pub usage: Usage,
    /// Known subtotal; see `uncounted_calls`.
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncounted_calls: u32,
    /// Why there are no steps, when the step designer gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// One step's run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    /// Its place in the plan, from 0.
    pub step: u32,
    pub goal: String,
    /// Its run's reply, without the conversation: its check, attempts,
    /// changes and cost. `applied` means merged into the plan's result.
    pub run: RunResponse,
}

/// The step designer's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitPlan {
    pub steps: Vec<String>,
    #[serde(default)]
    pub rationale: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_request_is_a_run_request_with_steps() {
        let wire = serde_json::json!({ "task": "t", "workspace": "/w", "steps": ["a", "b"], "budget_usd": 2.0 });
        let req: PlanRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(req.steps, ["a", "b"]);
        assert_eq!(req.run.budget_usd, Some(2.0));
        assert_eq!(req.run.attempts, 2);
        let plain = PlanRequest::new(RunRequest::new("t", "/w"));
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({ "task": "t", "workspace": "/w", "attempts": 2, "apply": true })
        );
    }

    #[test]
    fn steps_are_trimmed_and_bounded() {
        assert_eq!(steps(&[" a ".into(), "b".into()]).unwrap(), ["a", "b"]);
        assert!(steps(&[]).unwrap_err().contains("no steps"));
        assert!(steps(&["a".into(), " ".into()]).unwrap_err().contains("step 2 has no goal"));
        assert!(steps(&vec!["a".to_owned(); MAX_STEPS + 1]).unwrap_err().contains("at most 8"));
        assert!(steps(&["x".repeat(MAX_GOAL + 1)]).unwrap_err().contains("one goal sentence"));
    }
}
