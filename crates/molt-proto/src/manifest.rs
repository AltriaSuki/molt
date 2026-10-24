use serde::{Deserialize, Serialize};

use crate::tool::ToolDecl;
use crate::{Budget, ServiceId, Target};

/// Content hash of one service version, e.g. `sha256:ab12…`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionId(pub String);

impl std::fmt::Display for VersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who may change a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Part of the trusted computing base. Changed by humans only.
    Kernel,
    /// Evaluator, gate and similar. The agent may propose; a human promotes.
    Protected,
    /// Everything the agent may rewrite through the improvement loop.
    Mutable,
}

/// One capability a service asks for when it starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapRequest {
    pub target: Target,
    #[serde(default)]
    pub budget: Budget,
}

/// How the supervisor starts a service process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Stored in the registry next to each service version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: ServiceId,
    pub tier: Tier,
    /// The version this one was derived from; rolling back means promoting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<VersionId>,
    #[serde(default)]
    pub provides: Vec<Target>,
    #[serde(default)]
    pub requests: Vec<CapRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<Exec>,
    /// Tools the service offers the planner's model, each on a method it
    /// provides (RFC 0002). Left out when there are none, so a manifest
    /// without tools keeps the version id it always had.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDecl>,
}

impl Manifest {
    /// What is wrong with the service's tool declarations, if anything:
    /// each must be well formed, have a name of its own, and target a
    /// method the service provides. A declaration grants nothing; whoever
    /// calls the method still needs a capability for it.
    pub fn validate_tools(&self) -> Result<(), String> {
        let mut names = std::collections::BTreeSet::new();
        for tool in &self.tools {
            tool.validate().map_err(|e| format!("service {}: {e}", self.name))?;
            if !names.insert(tool.name.as_str()) {
                return Err(format!("service {} declares the tool {:?} twice", self.name, tool.name));
            }
            if !self.provides.iter().any(|p| p.covers(&tool.target)) {
                return Err(format!(
                    "service {}: tool {:?} targets {}, which the service does not provide",
                    self.name, tool.name, tool.target
                ));
            }
        }
        Ok(())
    }
}
