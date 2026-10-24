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
/// The built-in workflow that answers a question: one attempt that only
/// reads, in the workspace itself, and changes nothing.
pub const ASK: &str = "ask";
/// The built-in workflow for a small change: one attempt, no designed
/// check (a given one is used), and only the review's fixed rules.
pub const QUICK: &str = "quick";
/// Not a workflow: the name that has the planner pick one of [`ROUTED`]
/// for the task, with one model call, before the run starts.
pub const AUTO: &str = "auto";
/// The workflows [`AUTO`] picks from, lightest first.
pub const ROUTED: [&str; 3] = [ASK, QUICK, VERIFY];

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
    /// The role seated in each slot it names, by slot (`designer`,
    /// `attempt`, `explorer`, `reviewer`, `judge`) and role name; a slot
    /// not named keeps its built-in role. The roles are the planner's own
    /// (see [`crate::role`]). New in #164: optional, so every document
    /// written before it reads the same.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub roles: std::collections::BTreeMap<String, String>,
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
    pub attempts: Option<Attempts>,
    /// Rank attempts that pass, when the request's `select` lets more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<Stage>,
    /// Review a winner that passed its check before it is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
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

/// The attempts stage's parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempts {
    /// How many attempts run, 1 to 8, whatever the request asks. `None`:
    /// the request's `attempts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// The attempt works in the workspace itself, without a fork, with
    /// tools that only read: it answers and changes no file. Such a
    /// workflow has no design, compare or review stage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The attempts are offered `explore`: a question to a sub-agent that
    /// only reads their fork, seated as the `explorer` role, whose
    /// conclusion alone comes back (#42). Not with `read_only`. New in
    /// #42: optional, so every document written before it reads the same.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub explore: bool,
}

/// The review stage's parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// Only the fixed rules look at the diff, with or without a check; no
    /// reviewer model is asked. A result they find nothing in is applied,
    /// one they question is kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rules_only: bool,
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
                attempts: Some(Attempts::default()),
                compare: on,
                review: Some(Review::default()),
                deliver: on,
                learn: on,
            },
            roles: Default::default(),
        }
    }

    /// The built-in `ask`: one read-only attempt answers, unverified.
    pub fn ask() -> Self {
        Self {
            schema: SCHEMA,
            name: ASK.to_owned(),
            description: "Answer with tools that only read, without a fork; nothing is changed or verified.".into(),
            stages: Stages {
                context: Some(Stage {}),
                attempts: Some(Attempts { count: Some(1), read_only: true, explore: false }),
                deliver: Some(Stage {}),
                ..Stages::default()
            },
            roles: Default::default(),
        }
    }

    /// The built-in `quick`: one attempt, the given check if any, the
    /// review's fixed rules.
    pub fn quick() -> Self {
        Self {
            schema: SCHEMA,
            name: QUICK.to_owned(),
            description: "One attempt with the given check or none, the fixed review rules, then apply.".into(),
            stages: Stages {
                context: Some(Stage {}),
                attempts: Some(Attempts { count: Some(1), read_only: false, explore: false }),
                review: Some(Review { rules_only: true }),
                deliver: Some(Stage {}),
                ..Stages::default()
            },
            roles: Default::default(),
        }
    }

    /// Its attempt only reads, in the workspace itself.
    pub fn read_only(&self) -> bool {
        self.stages.attempts.is_some_and(|a| a.read_only)
    }

    /// Its attempts, which change files, are offered `explore`.
    pub fn explores(&self) -> bool {
        self.stages.attempts.is_some_and(|a| a.explore && !a.read_only)
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
        if let Some(count) = self.stages.attempts.and_then(|a| a.count).filter(|n| !(1..=8).contains(n)) {
            return Some(format!("workflow {name:?}: attempts.count must be 1 to 8, not {count}"));
        }
        let s = &self.stages;
        if self.read_only() && (s.design.is_some() || s.compare.is_some() || s.review.is_some()) {
            return Some(format!(
                "workflow {name:?} only reads, so it has nothing to design a check for, compare or review: leave out \
                 design, compare and review"
            ));
        }
        if self.stages.attempts.is_some_and(|a| a.explore && a.read_only) {
            return Some(format!(
                "workflow {name:?} only reads, so its attempt already answers as an explorer: leave out \
                 attempts.explore"
            ));
        }
        for slot in self.roles.keys() {
            let Some(base) = crate::role::Base::from_name(slot) else {
                return Some(format!(
                    "workflow {name:?} seats a role in {slot:?}, which is no slot: use designer, attempt, explorer, \
                     reviewer or judge"
                ));
            };
            let unused = match base {
                crate::role::Base::Designer => s.design.is_none(),
                crate::role::Base::Attempt => self.read_only(),
                crate::role::Base::Explorer => !self.read_only() && !self.explores(),
                crate::role::Base::Reviewer => s.review.is_none_or(|r| r.rules_only),
                crate::role::Base::Judge => self.read_only(),
            };
            if unused {
                return Some(format!("workflow {name:?} seats a role in {slot:?}, which none of its stages uses"));
            }
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
        ASK => Some(Workflow::ask()),
        QUICK => Some(Workflow::quick()),
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
    fn verify_is_the_default_and_ask_and_quick_are_the_light_builtins() {
        assert_eq!(builtin(None), Some(Workflow::verify()));
        assert_eq!(builtin(Some("verify")), Some(Workflow::verify()));
        assert_eq!(builtin(Some("ask")), Some(Workflow::ask()));
        assert_eq!(builtin(Some("quick")), Some(Workflow::quick()));
        assert_eq!(builtin(Some(AUTO)), None, "auto picks a workflow; it is not one");
        for name in ROUTED {
            let w = builtin(Some(name)).unwrap();
            assert!(w.problem().is_none(), "{name}: {:?}", w.problem());
        }
    }

    #[test]
    fn the_light_builtins_read_and_write_as_their_schema_documents() {
        assert_eq!(
            serde_json::to_value(Workflow::ask()).unwrap()["stages"],
            serde_json::json!({ "context": {}, "attempts": { "count": 1, "read_only": true }, "deliver": {} })
        );
        assert_eq!(
            serde_json::to_value(Workflow::quick()).unwrap()["stages"],
            serde_json::json!({
                "context": {}, "attempts": { "count": 1 }, "review": { "rules_only": true }, "deliver": {}
            })
        );
        let verify = serde_json::to_value(Workflow::verify()).unwrap();
        assert_eq!(verify["stages"]["attempts"], serde_json::json!({}), "verify's stages take no parameters");
    }

    #[test]
    fn a_read_only_workflow_that_designs_compares_or_reviews_is_refused() {
        for stage in ["design", "compare", "review"] {
            let mut w = Workflow::ask();
            match stage {
                "design" => w.stages.design = Some(Design::default()),
                "compare" => w.stages.compare = Some(Stage {}),
                _ => w.stages.review = Some(Review::default()),
            }
            assert!(w.problem().is_some_and(|p| p.contains("only reads")), "{stage}");
        }
        let mut many = Workflow::quick();
        many.stages.attempts = Some(Attempts { count: Some(9), ..Attempts::default() });
        assert!(many.problem().is_some_and(|p| p.contains("attempts.count")));
    }

    #[test]
    fn a_workflow_seats_roles_only_in_slots_its_stages_use() {
        let seat = |mut w: Workflow, slot: &str| {
            w.roles.insert(slot.into(), "mine".into());
            w.problem()
        };
        for slot in ["designer", "attempt", "reviewer", "judge"] {
            assert_eq!(seat(Workflow::verify(), slot), None, "{slot}");
        }
        assert_eq!(seat(Workflow::ask(), "explorer"), None);
        assert!(seat(Workflow::verify(), "explorer").is_some_and(|p| p.contains("none of its stages")));
        // An attempt that explores seats the explorer for its sub-agent.
        let mut exploring = Workflow::verify();
        exploring.stages.attempts = Some(Attempts { explore: true, ..Attempts::default() });
        assert!(exploring.explores() && !Workflow::verify().explores());
        assert_eq!(seat(exploring, "explorer"), None);
        let mut ask = Workflow::ask();
        ask.stages.attempts = Some(Attempts { explore: true, ..ask.stages.attempts.unwrap() });
        assert!(!ask.explores());
        assert!(ask.problem().is_some_and(|p| p.contains("leave out attempts.explore")), "{:?}", ask.problem());
        assert!(seat(Workflow::ask(), "attempt").is_some());
        assert!(seat(Workflow::quick(), "reviewer").is_some(), "quick's review asks no model");
        assert!(seat(Workflow::quick(), "designer").is_some());
        assert!(seat(Workflow::verify(), "vetter").is_some_and(|p| p.contains("no slot")));
        let wire = serde_json::json!({
            "schema": 1, "name": "careful", "roles": { "attempt": "terse" },
            "stages": { "attempts": {}, "deliver": {} }
        });
        let careful: Workflow = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(careful.roles["attempt"], "terse");
        assert_eq!(serde_json::to_value(&careful).unwrap(), wire);
        assert!(serde_json::to_value(Workflow::verify()).unwrap().get("roles").is_none());
    }
}
