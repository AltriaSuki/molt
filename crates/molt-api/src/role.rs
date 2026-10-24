//! Roles: who in a run talks to the model, with what system prompt, tools,
//! model, effort, turns and budget (see `docs/rfc/0003-prompts-and-roles.md`).
//!
//! A role is a declaration. The planner has five built-in ones, and the
//! user's prompt pack may declare more, each based on one of the five. A
//! workflow seats a role in one of its slots by name
//! ([`crate::workflow::Workflow::roles`]); a request or a project never
//! declares one.

use serde::{Deserialize, Serialize};

use crate::model::Effort;

/// The built-in roles a declared role is based on, which are also the
/// slots a workflow seats roles in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    /// Designs the done-check (the `design` stage).
    Designer,
    /// Carries out the task in its own fork (the `attempts` stage).
    Attempt,
    /// Answers with tools that only read: in the workspace itself, as the
    /// attempt of a workflow whose `attempts.read_only` is set (such as
    /// `ask`), or in an attempt's fork, as the sub-agent its `explore` tool
    /// asks (a workflow whose `attempts.explore` is set, #42).
    Explorer,
    /// Looks at a passing result before it is applied (the `review` stage).
    Reviewer,
    /// Decides a result against the run's rubric.
    Judge,
}

impl Base {
    pub const ALL: [Base; 5] = [Self::Designer, Self::Attempt, Self::Explorer, Self::Reviewer, Self::Judge];

    pub fn name(self) -> &'static str {
        match self {
            Self::Designer => "designer",
            Self::Attempt => "attempt",
            Self::Explorer => "explorer",
            Self::Reviewer => "reviewer",
            Self::Judge => "judge",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.name() == name)
    }

    /// The role guards the gate a result passes: its prompt takes effect
    /// only once a person has promoted it.
    pub fn protected(self) -> bool {
        matches!(self, Self::Reviewer | Self::Judge)
    }
}

/// A role of the user's prompt pack: a built-in role with some of its
/// settings replaced. A setting left out is the base role's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDecl {
    /// 1 to 64 lowercase letters, digits, `-` or `_`; not a built-in role's.
    pub name: String,
    pub base: Base,
    /// The prompt of the pack it speaks with, by name: a file
    /// `prompts/NAME.md`, or a built-in prompt's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The tools it is offered, by name, out of those its base would be;
    /// the base's own `submit_*` and `dispute_check` always stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// The most model turns one of its conversations takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// The share of the run's budget it may spend, more than 0 and at most
    /// one. Read for designer roles, and for explorer roles as the share of
    /// what the attempt has left that one `explore` call may spend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_share: Option<f64>,
}

/// Names a prompt and roles may have: 1 to 64 lowercase ASCII letters,
/// digits, `-` or `_`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// Where a prompt's text came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Compiled into the planner.
    #[default]
    Builtin,
    /// A file of the user's prompt pack.
    User,
}

/// One version of a prompt: its name and the SHA-256 of its text.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PromptVersion {
    pub name: String,
    /// Lowercase hex.
    pub sha256: String,
    #[serde(default)]
    pub source: Source,
}

impl PromptVersion {
    /// The first 12 hex digits, enough to tell versions apart in a line.
    pub fn short(&self) -> &str {
        self.sha256.get(..12).unwrap_or(&self.sha256)
    }
}

/// The role a model call or a run spoke as, and the prompt it used.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PromptUse {
    /// The role's name: a built-in's (`attempt`, `vetter`, `router`, ...)
    /// or one the user declared.
    pub role: String,
    pub prompt: PromptVersion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_reads_from_its_declaration_and_rejects_unknown_keys() {
        let role: RoleDecl = serde_json::from_value(serde_json::json!({
            "name": "terse", "base": "attempt", "prompt": "terse", "tools": ["read_file"], "effort": "low",
            "max_turns": 10
        }))
        .unwrap();
        assert_eq!(role.base, Base::Attempt);
        assert_eq!(role.effort, Some(Effort::Low));
        let typo = serde_json::json!({ "name": "t", "base": "attempt", "max_turn": 3 });
        assert!(serde_json::from_value::<RoleDecl>(typo).is_err());
        assert!(serde_json::from_value::<RoleDecl>(serde_json::json!({ "name": "t", "base": "vetter" })).is_err());
    }

    #[test]
    fn only_the_reviewer_and_the_judge_are_protected_and_names_are_plain() {
        let protected: Vec<_> = Base::ALL.into_iter().filter(|b| b.protected()).map(Base::name).collect();
        assert_eq!(protected, ["reviewer", "judge"]);
        for good in ["attempt", "strict-reviewer", "a_1"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-x", "Big", "a/b", "a.md", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
