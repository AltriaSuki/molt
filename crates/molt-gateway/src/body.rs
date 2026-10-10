//! The Messages API request body for one `model.complete` call.

use molt_api::model::CompleteRequest;
use molt_proto::{ErrorCode, RemoteError};
use serde_json::{json, Map, Value};

use crate::profile::profile;
use crate::Config;

/// A request body ready to send.
pub(crate) struct Call {
    pub body: Value,
    /// Send the server-side fallback beta header with it.
    pub fallbacks: bool,
    pub max_tokens: u32,
}

/// Shape `req` for `model` (already resolved). Takes the request by value so
/// a long conversation is moved into the body rather than copied.
pub(crate) fn build(cfg: &Config, model: &str, req: CompleteRequest) -> Result<Call, RemoteError> {
    if req.messages.is_empty() {
        return Err(RemoteError { code: ErrorCode::Invalid, message: "messages is empty".into() });
    }
    let profile = profile(model);
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
        tracing::debug!(model, ?effort, "dropping effort: this model does not take it");
    }
    if let Some(schema) = req.output_schema {
        output.insert("format".into(), json!({ "type": "json_schema", "schema": schema }));
    }
    if !output.is_empty() {
        body.insert("output_config".into(), Value::Object(output));
    }

    let fallbacks = cfg.fallbacks && profile.fallbacks;
    if fallbacks {
        body.insert("fallbacks".into(), json!("default"));
    }
    Ok(Call { body: Value::Object(body), fallbacks, max_tokens })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use molt_api::model::{user_text, Effort};

    use super::*;

    fn cfg() -> Config {
        Config {
            api_key: "k".into(),
            base_url: "http://127.0.0.1:1".into(),
            default_model: "opus".into(),
            max_tokens: 32_000,
            effort: None,
            timeout: Duration::from_secs(1),
            max_retries: 0,
            max_in_flight: 1,
            fallbacks: true,
            retry_base: Duration::from_millis(1),
        }
    }

    fn req() -> CompleteRequest {
        CompleteRequest { messages: vec![user_text("hi")], ..Default::default() }
    }

    #[test]
    fn empty_messages_are_invalid() {
        let err = build(&cfg(), "claude-opus-5-5", CompleteRequest::default()).err().unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn config_effort_applies_and_the_request_overrides_it() {
        let cfg = Config { effort: Some(Effort::Low), ..cfg() };
        let call = build(&cfg, "claude-sonnet-5-5", req()).unwrap();
        assert_eq!(call.body["output_config"], json!({ "effort": "low" }));
        let call = build(&cfg, "claude-sonnet-5-5", CompleteRequest { effort: Some(Effort::Max), ..req() }).unwrap();
        assert_eq!(call.body["output_config"], json!({ "effort": "max" }));
    }

    #[test]
    fn sonnet_defaults_to_high_effort_and_an_unknown_model_to_none() {
        let call = build(&cfg(), "claude-sonnet-5-5", req()).unwrap();
        assert_eq!(call.body["output_config"], json!({ "effort": "high" }));
        assert_eq!(call.body["thinking"], json!({ "type": "adaptive" }));
        assert!(call.fallbacks);
        let call = build(&cfg(), "claude-next-1", req()).unwrap();
        assert!(call.body.get("output_config").is_none());
        assert!(call.body.get("thinking").is_none());
        assert!(!call.fallbacks);
    }

    #[test]
    fn output_schema_goes_into_output_config_format() {
        let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } });
        let call = build(&cfg(), "claude-haiku-4-5", CompleteRequest { output_schema: Some(schema.clone()), ..req() })
            .unwrap();
        assert_eq!(call.body["output_config"], json!({ "format": { "type": "json_schema", "schema": schema } }));
    }

    #[test]
    fn an_empty_system_prompt_is_left_out() {
        let call = build(&cfg(), "claude-opus-5-5", CompleteRequest { system: Some(String::new()), ..req() }).unwrap();
        assert!(call.body.get("system").is_none());
    }
}
