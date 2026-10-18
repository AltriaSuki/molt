//! Workflows: which stages a planner run goes through, and with what
//! parameters (see `docs/rfc/0001-workflows.md`).
//!
//! A run names its workflow ([`crate::planner::RunRequest::workflow`]); it
//! never carries a definition. Definitions are the planner's own: the
//! built-ins here and the ones its trusted configuration adds. A project can
//! only pick one of those by name.

use serde::{Deserialize, Serialize};

/// The version of the workflow schema this crate reads and writes.
pub const SCHEMA: u32 = 1;

/// The built-in workflow a run without one goes through: today's pipeline.
pub const VERIFY: &str = "verify";

/// A named set of stages, in the fixed order context, design, attempts,
/// compare, review, deliver, learn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// The schema version it is written to: [`SCHEMA`].
    pub schema: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub stages: Stages,
}

/// Each stage, with its parameters when enabled; `None` leaves it out.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stages {
    /// Gather what the model is given besides the task: memory's notes and
    /// project map, the project's instruction files, the referenced files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Stage>,
    /// Design a done-check (or a rubric) when the request gives none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<Design>,
    /// Run the attempts in forks, each checked by what the run goes by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<Stage>,
    /// Rank attempts that pass, when the request's `select` lets more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<Stage>,
    /// Review a winner that passed its check before it is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Stage>,
    /// Return the winner's changes and, with `apply`, merge them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<Stage>,
    /// Have memory learn from the finished run. The caller does this once
    /// the planner has answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learn: Option<Stage>,
}

/// An enabled stage that takes no parameters in this schema version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {}

/// The design stage's parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Design {
    /// The share of the run's budget designing may spend, more than 0 and at
    /// most 1, for runs that set no `design_budget_usd`. `None`: the
    /// planner's own share (`MOLT_DESIGN_BUDGET_SHARE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_share: Option<f64>,
}

impl Workflow {
    /// The built-in `verify`: every stage, with the planner's defaults.
    pub fn verify() -> Self {
        let on = Some(Stage {});
        Self {
            schema: SCHEMA,
            name: VERIFY.to_owned(),
            description: "Design or take a check, race attempts against it, compare, review, apply and learn.".into(),
            stages: Stages {
                context: on,
                design: Some(Design::default()),
                attempts: on,
                compare: on,
                review: on,
                deliver: on,
                learn: on,
            },
        }
    }

    /// Why the planner cannot run this workflow, if it cannot.
    pub fn problem(&self) -> Option<String> {
        let name = &self.name;
        if self.schema != SCHEMA {
            return Some(format!(
                "workflow {name:?} is written to schema {}; this planner reads {SCHEMA}",
                self.schema
            ));
        }
        if name.trim().is_empty() {
            return Some("a workflow has no name".to_owned());
        }
        // Without attempts there is no result, and without deliver no one sees it.
        if self.stages.attempts.is_none() || self.stages.deliver.is_none() {
            return Some(format!("workflow {name:?} must enable the attempts and deliver stages"));
        }
        if let Some(share) = self.stages.design.and_then(|d| d.budget_share) {
            if !(share > 0.0 && share <= 1.0) {
                return Some(format!(
                    "workflow {name:?}: design.budget_share must be more than 0 and at most 1, not {share}"
                ));
            }
        }
        None
    }
}

/// The built-in workflow `name` names, with `None` for the default.
pub fn builtin(name: Option<&str>) -> Option<Workflow> {
    match name.unwrap_or(VERIFY) {
        VERIFY => Some(Workflow::verify()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workflow_reads_from_its_schema_and_rejects_unknown_stages() {
        let wire = serde_json::json!({
            "schema": 1, "name": "quick",
            "stages": { "context": {}, "attempts": {}, "deliver": {} }
        });
        let quick: Workflow = serde_json::from_value(wire.clone()).unwrap();
        assert!(quick.stages.design.is_none() && quick.stages.review.is_none() && quick.problem().is_none());
        assert_eq!(serde_json::to_value(&quick).unwrap(), wire);
        let typo = serde_json::json!({ "schema": 1, "name": "q", "stages": { "attempt": {} } });
        assert!(serde_json::from_value::<Workflow>(typo).is_err());
        let param = serde_json::json!({ "schema": 1, "name": "q", "stages": { "design": { "budget_share": 0.1 } } });
        let w: Workflow = serde_json::from_value(param).unwrap();
        assert_eq!(w.stages.design.unwrap().budget_share, Some(0.1));
    }

    #[test]
    fn a_workflow_without_attempts_or_deliver_or_from_another_schema_is_refused() {
        let ok = Workflow::verify();
        assert!(ok.problem().is_none());
        let mut no_deliver = ok.clone();
        no_deliver.stages.deliver = None;
        let mut no_attempts = ok.clone();
        no_attempts.stages.attempts = None;
        let newer = Workflow { schema: 2, ..ok.clone() };
        let mut share = ok.clone();
        share.stages.design = Some(Design { budget_share: Some(1.5) });
        for bad in [no_deliver, no_attempts, newer, share] {
            assert!(bad.problem().is_some(), "{bad:?}");
        }
    }

    #[test]
    fn verify_is_the_default_and_the_only_builtin() {
        assert_eq!(builtin(None), Some(Workflow::verify()));
        assert_eq!(builtin(Some("verify")), Some(Workflow::verify()));
        assert_eq!(builtin(Some("quick")), None);
    }
}
