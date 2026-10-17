//! Declarative project workflows. A skill never grants a capability or replaces a run's check.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MANAGE: &str = "memory.skills";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
    /// Every term must occur as a whole word in the task.
    pub trigger_terms: Vec<String>,
    /// Workspace files and their expected SHA-256, checked on each recall.
    pub prerequisites: BTreeMap<String, String>,
    pub steps: Vec<String>,
    /// Expected independently evaluated acceptance command; informational at use time.
    pub acceptance: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub revision: u64,
    pub spec: Spec,
    pub source_run: String,
    pub source_events: Vec<String>,
    pub writer: String,
    pub writer_version: String,
    pub evaluations: Vec<Evaluation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    pub run: String,
    pub check: Option<String>,
    pub passed: bool,
    pub cost_usd: f64,
    pub uncounted_calls: u32,
    pub duration_ms: u64,
    pub events: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub workspace: String,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Draft { spec: Spec, run: String },
    List,
    Evaluate { id: String, revision: u64, run: String },
    Promote { id: String, revision: u64 },
    Disable { id: String, reason: String },
    Rollback { id: String, revision: u64, reason: String },
    Used { run: String },
}
