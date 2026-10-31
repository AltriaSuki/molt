//! The Messages API request body for one `model.complete` call.

use molt_api::model::CompleteRequest;
use molt_proto::{ErrorCode, RemoteError};
use serde_json::{json, Map, Value};

use crate::probe::{schema_as_tool, Probed};
use crate::profile::profile;
use crate::provider::EndpointModel;
use crate::Config;

/// A request body ready to send.
#[derive(Default)]
pub(crate) struct Call {
    pub body: Value,
    /// Send the server-side fallback beta header with it.
    pub fallbacks: bool,
    pub max_tokens: u32,
    /// `output_schema` is asked for through [`crate::probe::SCHEMA_TOOL`],
    /// whose input the reply's text becomes.
    pub schema_tool: bool,
}

/// Leave out the reasoning a Chat Completions endpoint gave
/// ([`molt_api::model::chat_reasoning`]): it has no signature, so the
/// Messages API cannot take it as thinking, and it was that endpoint's
/// state, not this one's. An assistant turn that held nothing else is left
/// out with it, since the API refuses an empty one.
fn strip_chat_reasoning(messages: &mut Vec<Value>) {
    let mut changed = false;
    for message in messages.iter_mut() {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            let before = blocks.len();
            blocks.retain(|b| molt_api::model::chat_reasoning(b).is_none());
            changed |= blocks.len() != before;
        }
    }
    if changed {
        messages.retain(|m| m["role"] != "assistant" || m["content"].as_array().is_none_or(|c| !c.is_empty()));
    }
}

/// Most cache marks a request's messages get: the API takes four in all,
/// and the system prompt and the request's own automatic mark take two.
const MESSAGE_CACHE_POINTS: usize = 2;

/// Mark the last block of each message `points` names as the end of a
/// cached prefix, so a change after it does not lose the cache before it.
/// A point on the last message is left out, since the request's automatic
/// mark is there already, and so is one past the end or on a message whose
/// last block cannot be cached (thinking); the first
/// [`MESSAGE_CACHE_POINTS`] of the rest are kept.
fn mark_cache_points(messages: &mut [Value], points: &[usize]) {
    let last = messages.len().saturating_sub(1);
    let mut marked = 0;
    let mut points = points.to_vec();
    points.sort_unstable();
    points.dedup();
    for at in points {
        if marked == MESSAGE_CACHE_POINTS {
            break;
        }
        if at >= last {
            continue;
        }
        let Some(message) = messages.get_mut(at) else { continue };
        if let Some(text) = message["content"].as_str() {
            message["content"] = json!([{ "type": "text", "text": text }]);
        }
        let Some(last) = message["content"].as_array_mut().and_then(|b| b.last_mut()) else { continue };
        if matches!(last["type"].as_str(), Some("thinking" | "redacted_thinking") | None) {
            continue;
        }
        last["cache_control"] = json!({ "type": "ephemeral" });
        marked += 1;
    }
}

/// Shape `req` for `model` (already resolved). Takes the request by value so
/// a long conversation is moved into the body rather than copied.
/// `probed` is what `molt models probe` found the model does on this
/// endpoint, which wins over what the gateway knows of the model.
/// `declared` is what the user declared of it on a named endpoint: a
/// feature declared missing is never sent, as one a probe found missing.
pub(crate) fn build(
    cfg: &Config,
    model: &str,
    req: CompleteRequest,
    probed: Option<&Probed>,
    declared: Option<&EndpointModel>,
) -> Result<Call, RemoteError> {
    if req.messages.is_empty() {
        return Err(RemoteError { code: ErrorCode::Invalid, message: "messages is empty".into() });
    }
    let refused = |message: String| RemoteError { code: ErrorCode::Invalid, message };
    let mut profile = profile(model);
    if probed.is_some_and(|p| p.effort == Some(false)) {
        profile.effort = false;
        profile.default_effort = None;
    }
    if let Some(d) = declared {
        profile.max_output = Some(profile.max_output.map_or(d.max_output, |cap| cap.min(d.max_output)));
        if d.efforts.is_empty() {
            profile.effort = false;
        }
        profile.default_effort = profile.default_effort.filter(|e| d.efforts.contains(e));
    }
    if !req.tools.is_empty() && declared.is_some_and(|d| !d.tools) {
        return Err(refused(format!("{model} does not call tools: its endpoint declares tools = false")));
    }
    if !req.tools.is_empty() && probed.is_some_and(|p| p.tools == Some(false)) {
        return Err(refused(format!("{model} does not call tools on this endpoint (found by `molt models probe`)")));
    }
    if let Some((d, effort)) = declared.zip(req.effort.or(cfg.effort)).filter(|(d, e)| !d.efforts.contains(e)) {
        if !d.efforts.is_empty() {
            return Err(refused(format!(
                "{model} does not take effort {effort:?}: its endpoint declares only {:?}",
                d.efforts
            )));
        }
    }
    let wanted = req.max_tokens.unwrap_or(cfg.max_tokens);
    let max_tokens = profile.max_output.map_or(wanted, |cap| wanted.min(cap));

    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(max_tokens));
    if req.stream.is_some() {
        body.insert("stream".into(), json!(true));
    }
    // The API puts this breakpoint on the last cacheable block, so each turn
    // of an agent loop reads the turn before it from cache.
    body.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    if let Some(system) = req.system.filter(|s| !s.is_empty()) {
        // Tools render before the system prompt, so this caches both together.
        body.insert(
            "system".into(),
            json!([{ "type": "text", "text": system, "cache_control": { "type": "ephemeral" } }]),
        );
    }
    let mut messages = req.messages;
    // Before the reasoning is stripped, which may leave out whole turns and move the indexes.
    mark_cache_points(&mut messages, &req.cache_points);
    strip_chat_reasoning(&mut messages);
    body.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        body.insert("tools".into(), Value::Array(req.tools));
    }
    if profile.adaptive_thinking {
        body.insert("thinking".into(), json!({ "type": "adaptive" }));
    }

    let mut output = Map::new();
    let effort = req.effort.or(cfg.effort);
    if profile.effort {
        if let Some(effort) = effort.or(profile.default_effort) {
            output.insert("effort".into(), json!(effort));
        }
    } else if let Some(effort) = effort {
        return Err(RemoteError {
            code: ErrorCode::Invalid,
            message: format!(
                "{model} does not support effort {effort:?}{}",
                if declared.is_some_and(|d| d.efforts.is_empty()) {
                    ": its endpoint declares no efforts"
                } else if probed.is_some() {
                    " (found by `molt models probe`)"
                } else {
                    ""
                }
            ),
        });
    }
    let mut schema_tool = false;
    if let Some(schema) = req.output_schema {
        if probed.is_some_and(|p| p.output_schema == Some(false)) || declared.is_some_and(|d| !d.output_schema) {
            schema_as_tool(&mut body, schema)?;
            schema_tool = true;
        } else {
            output.insert("format".into(), json!({ "type": "json_schema", "schema": schema }));
        }
    }
    if !output.is_empty() {
        body.insert("output_config".into(), Value::Object(output));
    }

    let fallbacks = cfg.fallbacks && profile.fallbacks;
    if fallbacks {
        body.insert("fallbacks".into(), json!("default"));
    }
    Ok(Call { body: Value::Object(body), fallbacks, max_tokens, schema_tool })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use molt_api::model::{user_text, Effort};

    use super::*;

    fn cfg() -> Config {
        Config {
            providers: Default::default(),
            api_key: "k".into(),
            auth_token: None,
            base_url: "http://127.0.0.1:1".into(),
            default_model: "opus".into(),
            max_tokens: 32_000,
            effort: None,
            timeout: Duration::from_secs(1),
            max_retries: 0,
            max_in_flight: 1,
            fallbacks: true,
            retry_base: Duration::from_millis(1),
            prices: Default::default(),
            probed: Default::default(),
        }
    }

    fn unprobed(cfg: &Config, model: &str, req: CompleteRequest) -> Result<Call, RemoteError> {
        build(cfg, model, req, None, None)
    }

    fn req() -> CompleteRequest {
        CompleteRequest { messages: vec![user_text("hi")], ..Default::default() }
    }

    #[test]
    fn empty_messages_are_invalid() {
        let err = unprobed(&cfg(), "claude-opus-5-5", CompleteRequest::default()).err().unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn config_effort_applies_and_the_request_overrides_it() {
        let cfg = Config { effort: Some(Effort::Low), ..cfg() };
        let call = unprobed(&cfg, "claude-sonnet-5-5", req()).unwrap();
        assert_eq!(call.body["output_config"], json!({ "effort": "low" }));
        let call = unprobed(&cfg, "claude-sonnet-5-5", CompleteRequest { effort: Some(Effort::Max), ..req() }).unwrap();
        assert_eq!(call.body["output_config"], json!({ "effort": "max" }));
    }

    #[test]
    fn sonnet_defaults_to_high_effort_and_an_unknown_model_to_none() {
        let call = unprobed(&cfg(), "claude-sonnet-5-5", req()).unwrap();
        assert_eq!(call.body["output_config"], json!({ "effort": "high" }));
        assert_eq!(call.body["thinking"], json!({ "type": "adaptive" }));
        assert!(call.fallbacks);
        let call = unprobed(&cfg(), "claude-next-1", req()).unwrap();
        assert!(call.body.get("output_config").is_none());
        assert!(call.body.get("thinking").is_none());
        assert!(!call.fallbacks);
    }

    #[test]
    fn output_schema_goes_into_output_config_format() {
        let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } });
        let call =
            unprobed(&cfg(), "claude-haiku-4-5", CompleteRequest { output_schema: Some(schema.clone()), ..req() })
                .unwrap();
        assert_eq!(call.body["output_config"], json!({ "format": { "type": "json_schema", "schema": schema } }));
    }

    #[test]
    fn a_cache_point_marks_its_messages_last_block_and_the_marks_stay_within_the_apis_four() {
        let messages = vec![
            json!({ "role": "user", "content": "task" }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "ack" }] }),
            json!({ "role": "user", "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] }),
            json!({ "role": "assistant", "content": [{ "type": "thinking", "thinking": "t", "signature": "s" }] }),
            user_text("next"),
        ];
        let marked = CompleteRequest {
            system: Some("sys".into()),
            messages: messages.clone(),
            cache_points: vec![3, 2, 0, 0, 4, 99],
            ..Default::default()
        };
        let call = unprobed(&cfg(), "claude-next-1", marked).unwrap();
        let sent = &call.body["messages"];
        let mark = json!({ "type": "ephemeral" });
        // A string content becomes one text block, which renders the same.
        assert_eq!(sent[0]["content"], json!([{ "type": "text", "text": "task", "cache_control": mark }]));
        assert!(sent[2]["content"][0].get("cache_control").is_none());
        assert_eq!(sent[2]["content"][1]["cache_control"], mark);
        // Thinking cannot be marked; past two, nothing more is.
        assert!(sent[3]["content"][0].get("cache_control").is_none());
        assert!(sent[4]["content"][0].get("cache_control").is_none());
        let marks = call.body.to_string().matches("cache_control").count();
        assert_eq!(marks, 4, "{}", call.body);

        // The last message has the request's own mark: it gets no second one.
        let only = CompleteRequest { cache_points: vec![0], ..req() };
        let call = unprobed(&cfg(), "claude-next-1", only).unwrap();
        assert!(call.body["messages"][0]["content"][0].get("cache_control").is_none());

        // Without points the messages go as they came.
        let call = unprobed(&cfg(), "claude-next-1", CompleteRequest { messages: messages.clone(), ..req() }).unwrap();
        assert_eq!(call.body["messages"], Value::Array(messages));
    }

    #[test]
    fn an_empty_system_prompt_is_left_out() {
        let call =
            unprobed(&cfg(), "claude-opus-5-5", CompleteRequest { system: Some(String::new()), ..req() }).unwrap();
        assert!(call.body.get("system").is_none());
    }

    #[test]
    fn what_a_probe_found_shapes_the_call_before_anything_is_sent() {
        let probed = |tools, output_schema, effort| Probed {
            provider: crate::provider::Provider::Anthropic,
            base_url: "http://127.0.0.1:1".into(),
            model: "claude-sonnet-5-5".into(),
            probed_at_ms: 1,
            tools,
            streaming: None,
            output_schema,
            effort,
            endpoint: None,
            reasoning_tokens: None,
            cost_usd: None,
        };
        let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } });
        let asked = CompleteRequest { output_schema: Some(schema.clone()), ..req() };

        // Ignored output_schema: asked for as a forced tool, without thinking, which a forced call does not take.
        let p = probed(None, Some(false), Some(false));
        let call = build(&cfg(), "claude-sonnet-5-5", asked.clone(), Some(&p), None).unwrap();
        assert!(call.schema_tool);
        assert_eq!(call.body["tools"][0]["name"], crate::probe::SCHEMA_TOOL);
        assert_eq!(call.body["tools"][0]["input_schema"], schema);
        assert_eq!(call.body["tool_choice"], json!({ "type": "tool", "name": crate::probe::SCHEMA_TOOL }));
        assert!(call.body.get("thinking").is_none() && call.body.get("output_config").is_none(), "{}", call.body);
        // Effort refused there: the default is left out, and one asked for is refused before HTTP.
        let call = build(&cfg(), "claude-sonnet-5-5", req(), Some(&p), None).unwrap();
        assert!(call.body.get("output_config").is_none(), "{}", call.body);
        let err =
            build(&cfg(), "claude-sonnet-5-5", CompleteRequest { effort: Some(Effort::Low), ..req() }, Some(&p), None);
        assert!(err.err().unwrap().message.contains("molt models probe"));
        // With tools of its own the schema cannot go through a tool.
        let tool = json!({ "name": "t", "input_schema": { "type": "object" } });
        let both = CompleteRequest { tools: vec![tool.clone()], ..asked.clone() };
        assert!(build(&cfg(), "claude-sonnet-5-5", both, Some(&p), None).is_err());
        // No tools there: a request with tools is refused before HTTP.
        let p = probed(Some(false), None, None);
        let tooled = CompleteRequest { tools: vec![tool], ..req() };
        assert_eq!(build(&cfg(), "claude-sonnet-5-5", tooled, Some(&p), None).err().unwrap().code, ErrorCode::Invalid);
        // Honoured, or not probed: output_config.format as before.
        for p in [Some(probed(None, Some(true), None)), None] {
            let call = build(&cfg(), "claude-sonnet-5-5", asked.clone(), p.as_ref(), None).unwrap();
            assert!(!call.schema_tool);
            assert_eq!(call.body["output_config"]["format"]["schema"], schema);
        }
    }
}
