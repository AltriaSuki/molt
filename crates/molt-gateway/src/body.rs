//! The Messages API request body for one `model.complete` call.

use molt_api::model::CompleteRequest;
use molt_proto::{ErrorCode, RemoteError};
use serde_json::{json, Map, Value};

use crate::probe::{schema_as_tool, Probed};
use crate::profile::profile;
use crate::Config;

/// A request body ready to send.
pub(crate) struct Call {
    pub body: Value,
    /// Send the server-side fallback beta header with it.
    pub fallbacks: bool,
    pub max_tokens: u32,
    /// `output_schema` is asked for through [`crate::probe::SCHEMA_TOOL`],
    /// whose input the reply's text becomes.
    pub schema_tool: bool,
}

/// Shape `req` for `model` (already resolved). Takes the request by value so
/// a long conversation is moved into the body rather than copied.
/// `probed` is what `molt models probe` found the model does on this
/// endpoint, which wins over what the gateway knows of the model.
pub(crate) fn build(
    cfg: &Config,
    model: &str,
    req: CompleteRequest,
    probed: Option<&Probed>,
) -> Result<Call, RemoteError> {
    if req.messages.is_empty() {
        return Err(RemoteError { code: ErrorCode::Invalid, message: "messages is empty".into() });
    }
    let mut profile = profile(model);
    if probed.is_some_and(|p| p.effort == Some(false)) {
        profile.effort = false;
        profile.default_effort = None;
    }
    if !req.tools.is_empty() && probed.is_some_and(|p| p.tools == Some(false)) {
        return Err(RemoteError {
            code: ErrorCode::Invalid,
            message: format!("{model} does not call tools on this endpoint (found by `molt models probe`)"),
        });
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
    body.insert("messages".into(), Value::Array(req.messages));
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
                if probed.is_some() { " (found by `molt models probe`)" } else { "" }
            ),
        });
    }
    let mut schema_tool = false;
    if let Some(schema) = req.output_schema {
        if probed.is_some_and(|p| p.output_schema == Some(false)) {
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
        build(cfg, model, req, None)
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
            cost_usd: None,
        };
        let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } });
        let asked = CompleteRequest { output_schema: Some(schema.clone()), ..req() };

        // Ignored output_schema: asked for as a forced tool, without thinking, which a forced call does not take.
        let p = probed(None, Some(false), Some(false));
        let call = build(&cfg(), "claude-sonnet-5-5", asked.clone(), Some(&p)).unwrap();
        assert!(call.schema_tool);
        assert_eq!(call.body["tools"][0]["name"], crate::probe::SCHEMA_TOOL);
        assert_eq!(call.body["tools"][0]["input_schema"], schema);
        assert_eq!(call.body["tool_choice"], json!({ "type": "tool", "name": crate::probe::SCHEMA_TOOL }));
        assert!(call.body.get("thinking").is_none() && call.body.get("output_config").is_none(), "{}", call.body);
        // Effort refused there: the default is left out, and one asked for is refused before HTTP.
        let call = build(&cfg(), "claude-sonnet-5-5", req(), Some(&p)).unwrap();
        assert!(call.body.get("output_config").is_none(), "{}", call.body);
        let err = build(&cfg(), "claude-sonnet-5-5", CompleteRequest { effort: Some(Effort::Low), ..req() }, Some(&p));
        assert!(err.err().unwrap().message.contains("molt models probe"));
        // With tools of its own the schema cannot go through a tool.
        let tool = json!({ "name": "t", "input_schema": { "type": "object" } });
        let both = CompleteRequest { tools: vec![tool.clone()], ..asked.clone() };
        assert!(build(&cfg(), "claude-sonnet-5-5", both, Some(&p)).is_err());
        // No tools there: a request with tools is refused before HTTP.
        let p = probed(Some(false), None, None);
        let tooled = CompleteRequest { tools: vec![tool], ..req() };
        assert_eq!(build(&cfg(), "claude-sonnet-5-5", tooled, Some(&p)).err().unwrap().code, ErrorCode::Invalid);
        // Honoured, or not probed: output_config.format as before.
        for p in [Some(probed(None, Some(true), None)), None] {
            let call = build(&cfg(), "claude-sonnet-5-5", asked.clone(), p.as_ref()).unwrap();
            assert!(!call.schema_tool);
            assert_eq!(call.body["output_config"]["format"]["schema"], schema);
        }
    }
}
