//! `improver.propose`: the improver service reads how runs of one of the
//! planner's prompts failed and proposes a new text for it
//! (`docs/improve.md`). It only proposes: whoever calls it decides what the
//! text is tested on and whether it is promoted.

use serde::{Deserialize, Serialize};

pub const PROPOSE: &str = "improver.propose";

/// The tool the improver's model hands its proposal over with.
pub const SUBMIT_PROMPT: &str = "submit_prompt";

/// The most a proposed text may hold, as for a prompt of the pack.
pub const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposeRequest {
    /// The prompt to improve, such as `attempt`.
    pub prompt: String,
    /// Its text as runs use it now.
    pub current: String,
    /// What went wrong, most common first.
    pub patterns: Vec<Pattern>,
    /// The model to ask; `None` uses the gateway's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Runs that failed the same way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    /// Such as `said it was done, but the hidden tests failed`.
    pub what: String,
    pub cases: Vec<Case>,
}

/// One failed run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Case {
    pub task: String,
    /// What the agent was asked.
    pub ticket: String,
    /// What the run left behind: its changes, its last words, the grade's output.
    pub evidence: String,
}

/// The improver's answer. Only these two fields: a reply with anything else
/// is refused, so an improver cannot slip settings in beside its text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposeResponse {
    pub text: String,
    /// Why this text should fix the patterns, for the person reading the decision.
    pub rationale: String,
}
