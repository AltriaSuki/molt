//! The MCP bridge: the tools of a Model Context Protocol server, offered to
//! the planner's model as tools of a Molt service (`docs/mcp.md`).
//!
//! Each server of the user's `[mcp.NAME]` configuration is a service of its
//! own, `mcp-NAME` (a service id cannot hold a dot), started by the
//! supervisor as `molt-mcp serve`. It runs the server's command in the
//! sandbox the shell's commands get, without a workspace and with the
//! network only when the configuration says so, and answers `mcp-NAME.call`
//! by forwarding a [`ToolCall`](molt_proto::tool::ToolCall) as `tools/call`.
//!
//! Before the kernel installs the service, `molt-mcp list` turns the
//! server's tools into declarations (RFC 0002) for its manifest: named
//! `mcp__NAME__TOOL`, external trust, read-only and offered beyond the
//! attempts only where the user's configuration says so. The listing is
//! cached; the serving bridge lists the tools again and, when the server
//! now describes a tool differently than the run was told, refuses calls
//! to it and replaces the cache, so the next run is offered what the server
//! says now.

mod bridge;
mod cache;
mod client;
pub mod schema;

use std::collections::BTreeMap;
use std::path::PathBuf;

use molt_proto::tool::{ResultTrust, ToolDecl, ToolRole, MAX_DESCRIPTION, MAX_NAME, MAX_RESULT_BYTES, MAX_TIMEOUT_MS};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use bridge::{Bridge, Launch, Listing};
pub use cache::ListedTool;

/// What a bridge service's name starts with.
pub const SERVICE_PREFIX: &str = "mcp-";
/// The one method a bridge provides.
pub const CALL: &str = "call";
/// The longest server name: room is left in tool names for the tool's own.
pub const MAX_SERVER_NAME: usize = 32;
/// Calls the planner may make to one server in a run, unless configured.
pub const DEFAULT_CALLS: u64 = 1_000;

fn default_calls() -> u64 {
    DEFAULT_CALLS
}

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_default_calls(n: &u64) -> bool {
    *n == DEFAULT_CALLS
}

/// One MCP server of the user's configuration, `[mcp.NAME]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// The program that serves MCP on its stdin and stdout: an absolute
    /// path, or a name looked up on `PATH`.
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Variables set for the server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Variables copied from molt's environment when it has them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    /// Let the server reach the network; it has none otherwise.
    #[serde(default, skip_serializing_if = "is_false")]
    pub network: bool,
    /// Host paths the server may read, mounted read-only in its sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<PathBuf>,
    /// Calls the planner may make to the server in a run; 0 offers none of its tools.
    #[serde(default = "default_calls", skip_serializing_if = "is_default_calls")]
    pub calls: u64,
    /// Deadline of one call to any of its tools, unless the tool sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Settings of single tools, by the server's name for them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ToolSettings>,
}

/// What the user says of one tool, `[mcp.NAME.tools.TOOL]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSettings {
    /// Offer the tool at all.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// The tool changes nothing. The server's own `readOnlyHint` is never
    /// taken for this: only the user can say it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// Who may be offered it; only attempts unless given. Any role but
    /// `attempt` needs `read_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<ToolRole>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<u32>,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self { enabled: true, read_only: false, roles: None, timeout_ms: None, max_result_bytes: None }
    }
}

/// The service that bridges the server `name`.
pub fn service_name(name: &str) -> String {
    format!("{SERVICE_PREFIX}{name}")
}

/// The method the tools of server `name` call.
pub fn target(name: &str) -> String {
    format!("{SERVICE_PREFIX}{name}.{CALL}")
}

/// The name the model calls a server's tool by: `mcp__SERVER__TOOL`, with
/// any character a tool name cannot have made `_`. None when it is too long.
pub fn tool_name(server: &str, tool: &str) -> Option<String> {
    let tool: String = tool.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let name = format!("mcp__{server}__{tool}");
    (!tool.is_empty() && name.len() <= MAX_NAME).then_some(name)
}

fn env_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

impl McpServer {
    /// What is wrong with the configuration of the server `name`, if anything.
    pub fn validate(&self, name: &str) -> Result<(), String> {
        if name.is_empty()
            || name.len() > MAX_SERVER_NAME
            || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(format!(
                "MCP server name {name:?} must be 1 to {MAX_SERVER_NAME} lowercase letters, digits, `_` or `-`"
            ));
        }
        if self.command.trim().is_empty() {
            return Err(format!("mcp.{name}: command is empty: give the program that serves MCP on stdio"));
        }
        for var in self.env.keys().chain(&self.pass_env) {
            if !env_name_ok(var) {
                return Err(format!("mcp.{name}: {var:?} is not a variable name"));
            }
            if var.starts_with("MOLT_") || var.starts_with("ANTHROPIC_") || var.starts_with("OPENAI_") || var == "HOME"
            {
                return Err(format!(
                    "mcp.{name}: {var} cannot be given to an MCP server: Molt's own settings, the model API keys \
                     and HOME stay out of its sandbox"
                ));
            }
        }
        if let Some(path) = self.mounts.iter().find(|p| !p.is_absolute()) {
            return Err(format!("mcp.{name}: mount {} must be an absolute path", path.display()));
        }
        if self.calls > 1_000_000_000 {
            return Err(format!("mcp.{name}: calls must be at most 1000000000"));
        }
        if self.timeout_ms.is_some_and(|ms| ms == 0 || ms > MAX_TIMEOUT_MS) {
            return Err(format!("mcp.{name}: timeout_ms must be 1 to {MAX_TIMEOUT_MS}"));
        }
        for (tool, settings) in &self.tools {
            let roles = settings.roles.as_deref().unwrap_or(&[ToolRole::Attempt]);
            if roles.is_empty() {
                return Err(format!("mcp.{name}.tools.{tool}: roles is empty; leave it out for attempts"));
            }
            if let Some(role) = roles.iter().find(|r| r.only_reads()) {
                if !settings.read_only {
                    return Err(format!(
                        "mcp.{name}.tools.{tool}: the {} role only reads, so it may be given only a tool you \
                         declare read_only = true",
                        serde_json::to_value(role).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
                    ));
                }
            }
            if settings.timeout_ms.is_some_and(|ms| ms == 0 || ms > MAX_TIMEOUT_MS) {
                return Err(format!("mcp.{name}.tools.{tool}: timeout_ms must be 1 to {MAX_TIMEOUT_MS}"));
            }
            if settings.max_result_bytes.is_some_and(|b| b == 0 || b > MAX_RESULT_BYTES) {
                return Err(format!("mcp.{name}.tools.{tool}: max_result_bytes must be 1 to {MAX_RESULT_BYTES}"));
            }
        }
        Ok(())
    }

    /// Every variable the server gets, by name: `env`'s and `pass_env`'s.
    pub fn variables(&self) -> Vec<String> {
        let mut names: Vec<String> = self.env.keys().cloned().collect();
        names.extend(self.pass_env.iter().filter(|n| !self.env.contains_key(*n)).cloned());
        names
    }

    /// The declaration of a tool the server `name` listed, or why it is not offered.
    pub fn declare(&self, name: &str, tool: &ListedTool) -> Result<Option<ToolDecl>, String> {
        let settings = self.tools.get(&tool.name).cloned().unwrap_or_default();
        if !settings.enabled {
            return Ok(None);
        }
        let model_name = tool_name(name, &tool.name)
            .ok_or_else(|| format!("its name {:?} is too long for a tool name", tool.name))?;
        let said = tool.description.trim();
        let mut description = format!("From the {name} MCP server. ");
        description.push_str(if said.is_empty() { "It gives no description." } else { said });
        if description.len() > MAX_DESCRIPTION {
            let mut cut = MAX_DESCRIPTION - 3;
            while !description.is_char_boundary(cut) {
                cut -= 1;
            }
            description.truncate(cut);
            description.push_str("...");
        }
        let input_schema = tool.input_schema.clone();
        let decl = ToolDecl {
            schema: molt_proto::tool::TOOL_SCHEMA,
            name: model_name,
            description,
            strict: schema::strict_ok(&input_schema),
            input_schema,
            target: target(name).parse().map_err(|e| format!("{e}"))?,
            read_only: settings.read_only,
            roles: settings.roles.unwrap_or_else(|| vec![ToolRole::Attempt]),
            trust: ResultTrust::External,
            timeout_ms: settings.timeout_ms.or(self.timeout_ms),
            max_result_bytes: settings.max_result_bytes,
        };
        decl.validate()?;
        Ok(Some(decl))
    }
}

/// The text of a `tools/call` result, for the model: the text items, a
/// resource's text, a note for what is not text, and structured content
/// when there is nothing else.
pub fn result_text(result: &Value) -> String {
    let mut parts = Vec::new();
    for item in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(item.get("text").and_then(Value::as_str).unwrap_or_default().to_owned()),
            Some("resource") => match item.pointer("/resource/text").and_then(Value::as_str) {
                Some(text) => parts.push(text.to_owned()),
                None => parts.push("[a binary resource, not shown]".to_owned()),
            },
            Some("resource_link") => {
                parts.push(format!("[a link to {}]", item.get("uri").and_then(Value::as_str).unwrap_or("a resource")))
            }
            Some(kind) => parts.push(format!(
                "[{kind} content{}, not shown]",
                item.get("mimeType").and_then(Value::as_str).map(|m| format!(" ({m})")).unwrap_or_default()
            )),
            None => {}
        }
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            return structured.to_string();
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn listed(name: &str) -> ListedTool {
        ListedTool {
            name: name.into(),
            description: "Get an issue.".into(),
            input_schema: json!({ "type": "object", "properties": { "id": { "type": "integer" } } }),
        }
    }

    #[test]
    fn a_listed_tool_becomes_a_prefixed_external_declaration_that_writes_unless_the_user_says_otherwise() {
        let server = McpServer { command: "srv".into(), calls: DEFAULT_CALLS, ..Default::default() };
        let decl = server.declare("issues", &listed("get.issue")).unwrap().unwrap();
        assert_eq!(decl.name, "mcp__issues__get_issue");
        assert_eq!(decl.target.to_string(), "mcp-issues.call");
        assert_eq!((decl.read_only, decl.trust, decl.strict), (false, ResultTrust::External, false));
        assert_eq!(decl.roles, [ToolRole::Attempt]);
        assert!(decl.description.starts_with("From the issues MCP server. Get an issue."));

        let mut server = server;
        server.tools.insert(
            "get.issue".into(),
            ToolSettings {
                read_only: true,
                roles: Some(vec![ToolRole::Attempt, ToolRole::Designer]),
                ..Default::default()
            },
        );
        server.validate("issues").unwrap();
        let decl = server.declare("issues", &listed("get.issue")).unwrap().unwrap();
        assert!(decl.read_only && decl.offered_to(ToolRole::Designer));
        server.tools.get_mut("get.issue").unwrap().enabled = false;
        assert!(server.declare("issues", &listed("get.issue")).unwrap().is_none());
    }

    #[test]
    fn only_a_tool_the_user_declares_read_only_may_go_to_the_designer() {
        let mut server = McpServer { command: "srv".into(), ..Default::default() };
        server
            .tools
            .insert("delete".into(), ToolSettings { roles: Some(vec![ToolRole::Designer]), ..Default::default() });
        assert!(server.validate("issues").unwrap_err().contains("read_only = true"));
    }

    #[test]
    fn bad_server_settings_are_refused_with_the_reason() {
        let ok = McpServer { command: "srv".into(), ..Default::default() };
        assert!(ok.validate("Issues").unwrap_err().contains("lowercase"));
        assert!(McpServer::default().validate("x").unwrap_err().contains("command is empty"));
        let secret = McpServer { pass_env: vec!["ANTHROPIC_API_KEY".into()], ..ok.clone() };
        assert!(secret.validate("x").unwrap_err().contains("cannot be given"));
        let relative = McpServer { mounts: vec!["opt".into()], ..ok };
        assert!(relative.validate("x").unwrap_err().contains("absolute"));
    }

    #[test]
    fn a_result_is_its_text_with_notes_for_what_is_not_text() {
        let result = json!({ "content": [
            { "type": "text", "text": "one" },
            { "type": "image", "data": "...", "mimeType": "image/png" },
            { "type": "resource", "resource": { "uri": "x", "text": "two" } },
        ] });
        assert_eq!(result_text(&result), "one\n[image content (image/png), not shown]\ntwo");
        assert_eq!(result_text(&json!({ "content": [], "structuredContent": { "a": 1 } })), "{\"a\":1}");
    }
}
