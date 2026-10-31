//! `model.complete`: one provider call through the gateway.
//!
//! The gateway is the only process that holds the API key. Callers send
//! messages and tool definitions in the Messages API's own JSON shape, and
//! get Anthropic response content back verbatim, so thinking blocks can be passed
//! back unmodified on the next turn. The OpenAI adapters translate unsigned
//! text and function tools to this shared shape and reject native blocks;
//! OpenAI Responses reasoning comes back as `openai_reasoning` blocks, to be
//! passed back the same way. A Chat Completions endpoint's `reasoning_content`
//! (DeepSeek and the like) comes back as a `thinking` block with no signature
//! and a `protocol` of [`CHAT_REASONING`]; see [`chat_reasoning`].

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// An error reply coded `denied` or `invalid` means the call was refused
/// before anything was billed: by the kernel, by the gateway before any
/// HTTP, or by the API with an error status (401, 403, 404, ...) after
/// attempts none of which may have been billed. Every other error leaves
/// the call's cost unknown.
pub const COMPLETE: &str = "model.complete";

/// `model.limits`: what one model call to a model can take, as the
/// gateway's trusted configuration says, so a caller can fit its first
/// request before it has made one. Nothing is sent to a provider and
/// nothing is billed. New: an older gateway answers it with an `invalid`
/// error, and a caller then falls back to its own guess.
pub const LIMITS: &str = "model.limits";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsRequest {
    /// The model as a call names it; `None` asks for the gateway's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsResponse {
    /// Where a call naming it goes, as `place/model`: `glm/glm-4.6`.
    #[serde(default)]
    pub route: String,
    /// The context window in tokens, the reply included, when the gateway
    /// knows it; `None` leaves it to the caller's own setting or guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// The most `max_tokens` a call to it may ask for; a larger one is
    /// lowered to it. `None`: the gateway sets no cap of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

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

/// The variable that names a model for each role ([`role_models`]), read
/// by the planner and by memory.
pub const ROLE_MODELS_ENV: &str = "MOLT_ROLE_MODELS";

/// The roles [`role_models`] may name. The attempts are not one of them:
/// their model is the run's (`--model`).
pub const ROUTED_ROLES: [Role; 8] = [
    Role::Designer,
    Role::Vetter,
    Role::Reviewer,
    Role::Judge,
    Role::Router,
    Role::Explorer,
    Role::Compactor,
    Role::Memory,
];

/// A JSON object of role name to model, such as
/// `{"reviewer":"haiku","explorer":"haiku"}`: the model a role's calls go
/// to when its declaration names none. Refused: a role not of
/// [`ROUTED_ROLES`], and an empty model.
pub fn role_models(text: &str) -> Result<std::collections::BTreeMap<Role, String>, String> {
    let named: std::collections::BTreeMap<String, String> = serde_json::from_str(text)
        .map_err(|e| format!("{ROLE_MODELS_ENV} must be a JSON object of role names to models: {e}"))?;
    let mut models = std::collections::BTreeMap::new();
    for (name, model) in named {
        if name == Role::Attempt.name() {
            return Err(format!(
                "{ROLE_MODELS_ENV}: the attempts' model is the run's; set it with --model or [tui] model"
            ));
        }
        let Some(role) = ROUTED_ROLES.into_iter().find(|r| r.name() == name) else {
            let names: Vec<&str> = ROUTED_ROLES.iter().map(|r| r.name()).collect();
            return Err(format!("{ROLE_MODELS_ENV}: no role {name:?}; the roles are {}", names.join(", ")));
        };
        let model = model.trim();
        if model.is_empty() {
            return Err(format!("{ROLE_MODELS_ENV}: the model of {name} is empty"));
        }
        models.insert(role, model.to_owned());
    }
    Ok(models)
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
    /// Indexes into `messages` of the messages that end a prefix the caller
    /// keeps the same over its next calls, such as a conversation's first
    /// message. The gateway asks the provider to cache the request up to
    /// each, where it takes such marks (the Messages API, at most two), so
    /// that a later change after them does not lose what came before. New:
    /// an older caller leaves it out, and an older gateway ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache_points: Vec<usize>,
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
    /// Of `output_tokens`, those the model spent reasoning, when the
    /// endpoint says (`completion_tokens_details.reasoning_tokens` and the
    /// like). `None` is unknown, never zero: the Messages API does not count
    /// them apart. A breakdown only: they are billed as output, already in
    /// `output_tokens`. New: an older gateway leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

fn zero_if_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(d)?.unwrap_or(0))
}

impl Usage {
    /// Add `other` in. The reasoning tokens of a sum are known only when
    /// every call in it counted them; an empty `Usage` adds nothing.
    pub fn add(&mut self, other: &Usage) {
        if *other == Usage::default() {
            return;
        }
        self.reasoning_tokens = match (*self == Usage::default(), self.reasoning_tokens, other.reasoning_tokens) {
            (true, _, theirs) => theirs,
            (false, Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
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

/// The `protocol` of a `thinking` block that holds a Chat Completions
/// endpoint's reasoning, such as DeepSeek's `reasoning_content`.
pub const CHAT_REASONING: &str = "openai_chat";

/// The reasoning text of `block` and the response field it came in
/// (`reasoning_content` or `reasoning`), when it is a Chat Completions
/// endpoint's reasoning rather than signed Messages API thinking.
pub fn chat_reasoning(block: &Value) -> Option<(&str, &'static str)> {
    if block["type"] != "thinking" || block["protocol"] != CHAT_REASONING {
        return None;
    }
    // Only the two fields these endpoints read; anything else goes back as the usual one.
    let field = if block["field"] == "reasoning" { "reasoning" } else { "reasoning_content" };
    Some((block["thinking"].as_str()?, field))
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
    /// The endpoint the call went to: a named endpoint's name
    /// (`[models.endpoints.NAME]`), or `anthropic` or `openai` for a
    /// built-in one, so the audit log says where each call's usage and cost
    /// were spent. New: an older gateway leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
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
    fn a_limits_reply_reads_with_fields_left_out_and_writes_only_what_is_known() {
        let empty: LimitsResponse = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, LimitsResponse::default());
        let known =
            LimitsResponse { route: "glm/glm-4.6".into(), context_tokens: Some(32_768), max_output_tokens: None };
        assert_eq!(serde_json::to_value(&known).unwrap(), json!({"route":"glm/glm-4.6","context_tokens":32768}));
        assert_eq!(serde_json::to_value(LimitsRequest::default()).unwrap(), json!({}));
    }

    #[test]
    fn role_models_name_routed_roles_only_and_never_the_attempts() {
        let models = role_models(r#"{"reviewer":"haiku","explorer":" openai/gpt-4.1-mini ","memory":"sonnet"}"#);
        let models = models.unwrap();
        assert_eq!(models.get(&Role::Reviewer).map(String::as_str), Some("haiku"));
        assert_eq!(models.get(&Role::Explorer).map(String::as_str), Some("openai/gpt-4.1-mini"));
        assert_eq!(models.len(), 3);
        assert!(role_models(r#"{"attempt":"opus"}"#).unwrap_err().contains("--model"));
        assert!(role_models(r#"{"reviewr":"haiku"}"#).unwrap_err().contains("reviewer"));
        assert!(role_models(r#"{"planner":"haiku"}"#).is_err());
        assert!(role_models(r#"{"judge":" "}"#).is_err());
        assert!(role_models("[]").is_err());
    }

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
    fn reasoning_tokens_add_up_only_while_every_call_counted_them() {
        let call =
            |reasoning| Usage { input_tokens: 10, output_tokens: 5, reasoning_tokens: reasoning, ..Default::default() };
        let mut sum = Usage::default();
        sum.add(&call(Some(3)));
        sum.add(&Usage::default());
        sum.add(&call(Some(2)));
        assert_eq!((sum.output_tokens, sum.reasoning_tokens), (10, Some(5)));
        sum.add(&call(None));
        assert_eq!((sum.output_tokens, sum.reasoning_tokens), (15, None), "unknown, never a subtotal");
        sum.add(&call(Some(1)));
        assert_eq!(sum.reasoning_tokens, None);
        // Left out of the wire when unknown, so an older peer reads the same usage.
        assert!(serde_json::to_value(call(None)).unwrap().get("reasoning_tokens").is_none());
        assert_eq!(serde_json::to_value(call(Some(3))).unwrap()["reasoning_tokens"], 3);
        let chat = json!({ "type": "thinking", "thinking": "why", "protocol": CHAT_REASONING, "field": "reasoning" });
        assert_eq!(chat_reasoning(&chat), Some(("why", "reasoning")));
        assert_eq!(chat_reasoning(&json!({ "type": "thinking", "thinking": "x", "signature": "s" })), None);
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
