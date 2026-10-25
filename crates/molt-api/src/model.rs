//! `model.complete`: one provider call through the gateway.
//!
//! The gateway is the only process that holds the API key. Callers send
//! messages and tool definitions in the Messages API's own JSON shape, and
//! get Anthropic response content back verbatim, so thinking blocks can be passed
//! back unmodified on the next turn. The OpenAI adapters translate unsigned
//! text and function tools to this shared shape and reject native blocks;
//! OpenAI Responses reasoning comes back as `openai_reasoning` blocks, to be
//! passed back the same way.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// An error reply coded `denied` or `invalid` means the call was refused
/// before anything was billed: by the kernel, by the gateway before any
/// HTTP, or by the API with an error status (401, 403, 404, ...) after
/// attempts none of which may have been billed. Every other error leaves
/// the call's cost unknown.
pub const COMPLETE: &str = "model.complete";

/// Who in a run makes a model call, so previews, retries and transcripts can
/// say whose turn it was. An attempt's index is given apart, where there is
/// one. A role this build does not know reads as [`Role::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The check designer.
    Designer,
    /// The vetter, which looks over the designer's check.
    Vetter,
    Attempt,
    /// The reviewer, which looks at a passing result before it is applied.
    Reviewer,
    /// The judge of a result against the run's rubric.
    Judge,
    /// The planner picking a workflow for `auto`.
    Router,
    /// The sub-agent an attempt asks a question with `explore`, which only
    /// reads (#42). An older peer reads it as [`Role::Other`].
    Explorer,
    /// Memory learning from a finished run.
    Memory,
    /// The call that writes an attempt's notes on the turns its
    /// conversation leaves out to fit (#108). An older peer reads it as
    /// [`Role::Other`].
    Compactor,
    #[serde(other)]
    Other,
}

impl Role {
    /// The role's name as a person reads it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Designer => "designer",
            Self::Vetter => "vetter",
            Self::Attempt => "attempt",
            Self::Reviewer => "reviewer",
            Self::Judge => "judge",
            Self::Router => "router",
            Self::Explorer => "explorer",
            Self::Memory => "memory",
            Self::Compactor => "compactor",
            Self::Other => "planner",
        }
    }
}

/// Attempt `index` (counted from 0 on the wire) as a person reads it,
/// counted from 1: `attempt 1` is the first.
pub fn attempt_name(index: u32) -> String {
    format!("attempt {}", u64::from(index) + 1)
}

/// Who made a model call, as a person reads it: [`attempt_name`] for an
/// attempt, else the role's name. A call that names neither, from an
/// older caller, is the planner's.
pub fn speaker(role: Option<Role>, attempt: Option<u32>) -> String {
    match (role, attempt) {
        (None | Some(Role::Attempt), Some(a)) => attempt_name(a),
        (Some(Role::Explorer), Some(a)) => format!("{} explorer", attempt_name(a)),
        (Some(Role::Compactor), Some(a)) => format!("{} compactor", attempt_name(a)),
        (Some(role), _) => role.name().to_owned(),
        (None, None) => Role::Other.name().to_owned(),
    }
}

/// Display context for an opt-in streaming call. The run and call ids come
/// from the authenticated request envelope, not from this payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamContext {
    /// `None` for the check designer.
    pub attempt: Option<u32>,
    pub turn: u32,
}

/// How much the model thinks before answering (`output_config.effort`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl std::str::FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => Err(format!("unknown effort {other:?}: use low, medium, high, xhigh or max")),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CompleteRequest {
    /// `opus`, `sonnet`, `haiku`, or a full model id. `None` uses the
    /// gateway's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Stable instructions, sent as one cached system block. Keep it
    /// byte-identical across the turns of one conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Messages API `messages`, passed through verbatim. Assistant turns must
    /// carry the `content` the gateway returned, unmodified.
    pub messages: Vec<Value>,
    /// Messages API tool definitions, passed through verbatim. Declare the
    /// full set on the first turn and keep it identical afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    /// Output cap, thinking included. `None` uses the gateway's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// JSON Schema the final text must match (`output_config.format`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Stream text previews on `topic:progress`, keeping the final reply
    /// authoritative. Thinking and partial tool inputs are never published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamContext>,
    /// Who makes the call, for display only; the gateway copies it into the
    /// progress events it publishes. New: an older caller leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// The role the call speaks as and the version of its system prompt,
    /// for the audit log; the gateway does not send it to the model. New:
    /// an older caller leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::role::PromptUse>,
    /// The attempt the call is made for, its explorer's calls included, for
    /// the audit log; the gateway does not send it to the model. New: an
    /// older caller leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Token counts as the Messages API reports them. A count the API sends as
/// `null` or leaves out reads as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, deserialize_with = "zero_if_null")]
    pub input_tokens: u64,
    #[serde(default, deserialize_with = "zero_if_null")]
    pub output_tokens: u64,
    #[serde(default, deserialize_with = "zero_if_null")]
    pub cache_creation_input_tokens: u64,
    #[serde(default, deserialize_with = "zero_if_null")]
    pub cache_read_input_tokens: u64,
}

fn zero_if_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(d)?.unwrap_or(0))
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    /// Every token billed, input of all kinds plus output.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

pub const STOP_END_TURN: &str = "end_turn";
pub const STOP_TOOL_USE: &str = "tool_use";
pub const STOP_MAX_TOKENS: &str = "max_tokens";
pub const STOP_REFUSAL: &str = "refusal";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompleteResponse {
    pub id: String,
    /// The model that wrote this message. After a server-side fallback it
    /// differs from the one asked for.
    pub model: String,
    /// Content blocks exactly as the API returned them.
    pub content: Vec<Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<Value>,
    #[serde(default)]
    pub usage: Usage,
    /// Estimated cost in US dollars; `None` when the model's prices are unknown.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Where the prices behind `cost_usd` came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_source: Option<PriceSource>,
    /// The model's context window in tokens, the reply included, when the
    /// gateway's trusted configuration declares it; the planner fits its
    /// conversations to it. New: an older gateway leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
}

/// Which price a cost was worked out with, so that any charge can be checked later.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PriceSource {
    /// The price-table entry used: a model id, or `*/<name>` for the highest
    /// price among several providers' entries for that model.
    pub entry: String,
    /// `MOLT_MODEL_PRICES`, `built-in`, or a snapshot's source such as `litellm`.
    pub source: String,
    /// Where a snapshot was downloaded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// SHA-256 of the download a snapshot was made from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at_ms: Option<u64>,
}

/// One `tool_use` block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl CompleteResponse {
    /// A safety classifier declined the request. The content must not be used.
    pub fn is_refusal(&self) -> bool {
        self.stop_reason.as_deref() == Some(STOP_REFUSAL)
    }

    /// The text blocks, joined by blank lines.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.is_empty())
            .collect();
        parts.join("\n\n")
    }

    /// The tool calls, in the order the model made them.
    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| {
                Some(ToolUse {
                    id: b["id"].as_str()?.to_owned(),
                    name: b["name"].as_str()?.to_owned(),
                    input: b.get("input").cloned().unwrap_or(Value::Null),
                })
            })
            .collect()
    }

    /// This response as the assistant turn to append to the conversation.
    pub fn as_turn(&self) -> Value {
        json!({ "role": "assistant", "content": self.content })
    }
}

/// A user turn holding one text block.
pub fn user_text(text: impl Into<String>) -> Value {
    json!({ "role": "user", "content": [{ "type": "text", "text": text.into() }] })
}

/// A user turn holding the given blocks (for example every `tool_result` of
/// one assistant turn, which must all go back in a single message).
pub fn user_blocks(blocks: Vec<Value>) -> Value {
    json!({ "role": "user", "content": blocks })
}

pub fn text_block(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

pub fn tool_result(tool_use_id: &str, content: impl Into<String>, is_error: bool) -> Value {
    let mut block = json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": content.into() });
    if is_error {
        block["is_error"] = Value::Bool(true);
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_unset_fields() {
        let req = CompleteRequest { messages: vec![user_text("hi")], ..Default::default() };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v, json!({ "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }] }));
        let back: CompleteRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn reads_text_and_tool_calls_in_order() {
        let resp: CompleteResponse = serde_json::from_value(json!({
            "id": "msg_1",
            "model": "claude-opus-5-5",
            "content": [
                { "type": "thinking", "thinking": "", "signature": "sig" },
                { "type": "text", "text": "Reading it." },
                { "type": "tool_use", "id": "toolu_1", "name": "read_file", "input": { "path": "a" } },
                { "type": "tool_use", "id": "toolu_2", "name": "run", "input": { "command": "ls" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        }))
        .unwrap();
        assert_eq!(resp.text(), "Reading it.");
        let calls = resp.tool_uses();
        assert_eq!(calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["read_file", "run"]);
        assert!(!resp.is_refusal());
        // The thinking block survives untouched.
        assert_eq!(resp.as_turn()["content"][0]["signature"], "sig");
        assert_eq!(resp.usage.total(), 15);
    }

    #[test]
    fn null_usage_counts_read_as_zero() {
        let u: Usage = serde_json::from_value(json!({
            "input_tokens": 3, "output_tokens": 4, "cache_creation_input_tokens": null
        }))
        .unwrap();
        assert_eq!(u, Usage { input_tokens: 3, output_tokens: 4, ..Default::default() });
    }

    #[test]
    fn error_results_are_flagged() {
        assert_eq!(tool_result("t", "boom", true)["is_error"], true);
        assert!(tool_result("t", "ok", false).get("is_error").is_none());
    }
}
