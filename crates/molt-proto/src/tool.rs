//! Tool declarations: what a service tells the planner's model it can do
//! (RFC 0002, `docs/rfc/0002-tools.md`).
//!
//! A service lists its tools in its [`Manifest`](crate::Manifest). Each
//! declaration names a bus method the service provides; the planner offers
//! the tool only when it holds a capability for that method, and a call is
//! one budgeted bus call with a [`ToolCall`] payload, answered with a
//! [`ToolOutput`]. A declaration grants nothing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Target;

/// The version of the declaration format this crate reads and writes.
pub const TOOL_SCHEMA: u32 = 1;
/// Longest tool name: the model API's limit.
pub const MAX_NAME: usize = 64;
/// Longest description, in bytes.
pub const MAX_DESCRIPTION: usize = 4_096;
/// Largest input schema, in bytes of JSON.
pub const MAX_INPUT_SCHEMA: usize = 16_384;
/// Deadline for a call whose declaration gives none.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// The longest deadline a declaration may ask for.
pub const MAX_TIMEOUT_MS: u64 = 1_800_000;
/// The most result text a call returns to the model, and the default.
pub const MAX_RESULT_BYTES: u32 = 50_000;

/// Who in a run may be offered a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRole {
    /// The agents that carry out the task, the read-only `ask` attempt included.
    Attempt,
    /// The agent that designs the done-check.
    Designer,
    /// The agent that vets a designed check against the task.
    Vetter,
    /// The agent that reviews the winning change.
    Reviewer,
    /// The agent that judges a result against a rubric.
    Judge,
}

impl ToolRole {
    /// Every role but the attempts: they look at the work of others, and
    /// are offered a provided tool only when it is read-only.
    pub fn only_reads(self) -> bool {
        self != ToolRole::Attempt
    }
}

/// How far a tool's result is to be believed: in every case it is data for
/// the model, never instructions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultTrust {
    /// What the person following the run said.
    User,
    /// What the workspace holds or a command in it printed: as trustworthy
    /// as the project.
    Workspace,
    /// From outside the workspace (the web, another system): it may try to
    /// instruct whoever reads it.
    #[default]
    External,
}

fn schema_version() -> u32 {
    TOOL_SCHEMA
}

fn attempts() -> Vec<ToolRole> {
    vec![ToolRole::Attempt]
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One tool, as a service declares it.
///
/// Unknown fields are ignored, so that an older reader keeps working on a
/// newer manifest; a change that alters what a field means raises
/// [`TOOL_SCHEMA`], and a reader refuses a version it does not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDecl {
    /// The version of the format this declaration is written to.
    #[serde(default = "schema_version")]
    pub schema: u32,
    /// The name the model calls it by: letters, digits, `_` and `-`.
    pub name: String,
    /// What it does and when to use it, for the model.
    pub description: String,
    /// The JSON schema of its input: an object schema.
    pub input_schema: Value,
    /// The bus method a call goes to; one the service provides.
    pub target: Target,
    /// It changes nothing: it may run alongside other reads, and may be
    /// offered to the roles that only read.
    #[serde(default)]
    pub read_only: bool,
    /// Who may be offered it. Only attempts, unless it says otherwise.
    #[serde(default = "attempts")]
    pub roles: Vec<ToolRole>,
    /// How far its results are to be believed.
    #[serde(default)]
    pub trust: ResultTrust,
    /// Deadline for one call; [`DEFAULT_TIMEOUT_MS`] when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Most bytes of result text the model gets; [`MAX_RESULT_BYTES`] when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<u32>,
    /// Ask the model API to hold calls to the schema exactly. The schema
    /// must then be one the API's strict mode accepts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
}

impl ToolDecl {
    /// The fields a declaration may have, for configuration that refuses unknown keys.
    pub const FIELDS: &'static [&'static str] = &[
        "schema",
        "name",
        "description",
        "input_schema",
        "target",
        "read_only",
        "roles",
        "trust",
        "timeout_ms",
        "max_result_bytes",
        "strict",
    ];

    /// The deadline of one call.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    /// The most result text the model gets.
    pub fn max_result_bytes(&self) -> usize {
        self.max_result_bytes.unwrap_or(MAX_RESULT_BYTES) as usize
    }

    /// Whether `role` may be offered it: one of its roles, and for the
    /// roles that only read, read-only.
    pub fn offered_to(&self, role: ToolRole) -> bool {
        self.roles.contains(&role) && (self.read_only || !role.only_reads())
    }

    /// The tool as the model API takes it.
    pub fn definition(&self) -> Value {
        let mut def = serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        });
        if self.strict {
            def["strict"] = Value::Bool(true);
        }
        def
    }

    /// What is wrong with it, if anything.
    pub fn validate(&self) -> Result<(), String> {
        let name = &self.name;
        if self.schema != TOOL_SCHEMA {
            return Err(format!(
                "tool {name:?} is written to declaration schema {}, and this Molt reads only {TOOL_SCHEMA}",
                self.schema
            ));
        }
        if name.is_empty()
            || name.len() > MAX_NAME
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!("tool name {name:?} must be 1 to {MAX_NAME} letters, digits, `_` or `-`"));
        }
        if self.description.trim().is_empty() || self.description.len() > MAX_DESCRIPTION {
            return Err(format!("tool {name:?} needs a description of at most {MAX_DESCRIPTION} bytes"));
        }
        let schema_bytes = serde_json::to_vec(&self.input_schema).map(|b| b.len()).unwrap_or(usize::MAX);
        if self.input_schema.get("type").and_then(Value::as_str) != Some("object") || schema_bytes > MAX_INPUT_SCHEMA {
            return Err(format!(
                "tool {name:?} needs an input_schema with \"type\": \"object\", of at most {MAX_INPUT_SCHEMA} bytes"
            ));
        }
        if !matches!(self.target, Target::Method { .. }) {
            return Err(format!("tool {name:?} must target one method, such as lint.check, not {}", self.target));
        }
        if self.roles.is_empty() {
            return Err(format!("tool {name:?} is offered to no role: give roles, or leave them out for attempts"));
        }
        if let Some(role) = self.roles.iter().find(|r| r.only_reads() && !self.read_only) {
            return Err(format!(
                "tool {name:?} changes things (read_only is false), so it cannot be offered to the {} role, which \
                 only reads",
                serde_json::to_value(role).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
            ));
        }
        if self.trust == ResultTrust::User {
            return Err(format!(
                "tool {name:?} cannot claim the user's trust: only the planner's own ask_user carries the user's \
                 words; use workspace or external"
            ));
        }
        if self.timeout_ms.is_some_and(|ms| ms == 0 || ms > MAX_TIMEOUT_MS) {
            return Err(format!("tool {name:?}: timeout_ms must be 1 to {MAX_TIMEOUT_MS}"));
        }
        if self.max_result_bytes.is_some_and(|b| b == 0 || b > MAX_RESULT_BYTES) {
            return Err(format!("tool {name:?}: max_result_bytes must be 1 to {MAX_RESULT_BYTES}"));
        }
        Ok(())
    }
}

/// A tool a service declared, as the planner is told of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvidedTool {
    /// The service that declared it.
    pub service: crate::ServiceId,
    #[serde(flatten)]
    pub decl: ToolDecl,
}

/// The payload of a call to a declared tool's method.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The tool's name, so that one method can serve several tools.
    pub tool: String,
    /// The directory the calling agent works in: its fork, or the
    /// workspace itself for an agent that only reads.
    pub workspace: String,
    /// The model's input, as it gave it.
    pub input: Value,
}

/// The reply to a [`ToolCall`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The result, as text for the model.
    #[serde(default)]
    pub content: String,
    /// The tool ran and failed; `content` says why.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lint() -> ToolDecl {
        serde_json::from_value(json!({
            "name": "lint",
            "description": "Lint a file.",
            "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
            "target": "lint.check",
        }))
        .unwrap()
    }

    #[test]
    fn a_declaration_defaults_to_attempts_external_trust_and_writing() {
        let decl = lint();
        assert_eq!((decl.schema, decl.read_only, decl.trust), (1, false, ResultTrust::External));
        assert_eq!(decl.roles, [ToolRole::Attempt]);
        assert_eq!((decl.timeout_ms(), decl.max_result_bytes()), (60_000, 50_000));
        assert!(decl.validate().is_ok());
        assert_eq!(
            decl.definition(),
            json!({ "name": "lint", "description": "Lint a file.", "input_schema": decl.input_schema })
        );
        assert!(decl.offered_to(ToolRole::Attempt) && !decl.offered_to(ToolRole::Reviewer));
    }

    #[test]
    fn the_roles_that_only_read_get_only_tools_that_read() {
        let writes = ToolDecl { roles: vec![ToolRole::Attempt, ToolRole::Reviewer], ..lint() };
        assert!(writes.validate().unwrap_err().contains("reviewer role, which only reads"));
        assert!(!writes.offered_to(ToolRole::Reviewer));
        let reads = ToolDecl { read_only: true, ..writes };
        assert!(reads.validate().is_ok());
        assert!(reads.offered_to(ToolRole::Reviewer) && !reads.offered_to(ToolRole::Designer));
    }

    #[test]
    fn bad_declarations_are_refused_with_the_reason() {
        let bad = |decl: ToolDecl, says: &str| assert!(decl.validate().unwrap_err().contains(says), "{says}");
        bad(ToolDecl { name: "rm -rf".into(), ..lint() }, "letters, digits");
        bad(ToolDecl { schema: 2, ..lint() }, "schema 2");
        bad(ToolDecl { input_schema: json!({ "type": "string" }), ..lint() }, "\"type\": \"object\"");
        bad(ToolDecl { target: "lint.*".parse().unwrap(), ..lint() }, "one method");
        bad(ToolDecl { roles: vec![], ..lint() }, "no role");
        bad(ToolDecl { timeout_ms: Some(0), ..lint() }, "timeout_ms");
        bad(ToolDecl { max_result_bytes: Some(60_000), ..lint() }, "max_result_bytes");
        bad(ToolDecl { description: " ".into(), ..lint() }, "description");
        bad(ToolDecl { trust: ResultTrust::User, ..lint() }, "cannot claim the user's trust");
    }

    #[test]
    fn a_newer_declaration_with_fields_this_reader_does_not_know_still_reads() {
        let mut v = serde_json::to_value(lint()).unwrap();
        v["someday"] = json!(true);
        assert_eq!(serde_json::from_value::<ToolDecl>(v).unwrap(), lint());
        let provided: ProvidedTool =
            serde_json::from_value(json!({ "service": "lint", "name": "lint", "description": "d",
                "input_schema": { "type": "object" }, "target": "lint.check" }))
            .unwrap();
        assert_eq!(provided.service.as_str(), "lint");
    }
}
