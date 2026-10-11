//! OpenAI Chat Completions adapter. Only unsigned text and function-tool
//! conversations can cross providers; native thinking/media are refused.

use std::collections::{BTreeMap, HashSet};

use molt_api::model::{CompleteRequest, Effort};
use molt_proto::RemoteError;
use serde_json::{json, Value};

use crate::api::Message;
use crate::body::Call;
use crate::provider::invalid;
use crate::Config;

pub(crate) fn build(cfg: &Config, model: &str, req: CompleteRequest) -> Result<Call, RemoteError> {
    let caps = cfg.providers.capabilities(model)?;
    if req.messages.is_empty() {
        return Err(invalid("messages is empty"));
    }
    if !req.tools.is_empty() && !caps.tools {
        return Err(invalid("this OpenAI model does not support tools"));
    }
    if req.stream.is_some() && !caps.streaming {
        return Err(invalid("this OpenAI model does not support streaming"));
    }
    if req.output_schema.is_some() && !caps.output_schema {
        return Err(invalid("this OpenAI model does not support structured output"));
    }
    let effort = req.effort.or(cfg.effort);
    if effort.is_some_and(|e| !caps.efforts.contains(&e) || e == Effort::Max) {
        return Err(invalid("this OpenAI model does not support the requested effort"));
    }
    let max_tokens = req.max_tokens.unwrap_or(cfg.max_tokens).min(caps.max_output);
    if max_tokens == 0 {
        return Err(invalid("max_tokens must be positive"));
    }
    let mut messages = Vec::new();
    if let Some(system) = req.system.filter(|s| !s.is_empty()) {
        messages.push(json!({"role":"system", "content":system}));
    }
    let mut pending = HashSet::new();
    let mut used = HashSet::new();
    for message in req.messages {
        let role = message["role"].as_str().ok_or_else(|| invalid("message role is missing"))?;
        if role != "user" && role != "assistant" {
            return Err(invalid("only user and assistant messages can be translated"));
        }
        let blocks = if let Some(text) = message["content"].as_str() {
            vec![json!({"type":"text", "text":text})]
        } else {
            message["content"].as_array().cloned().ok_or_else(|| invalid("message content must be text or blocks"))?
        };
        let mut text = Vec::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    if block.get("citations").is_some() { return Err(invalid("native citations cannot be translated to OpenAI")); }
                    text.push(block["text"].as_str().ok_or_else(|| invalid("text block is malformed"))?.to_owned());
                }
                Some("tool_use") if role == "assistant" => {
                    if !caps.tools { return Err(invalid("this OpenAI model does not support tool history")); }
                    let id = block["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| invalid("tool id is missing"))?;
                    let name = block["name"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| invalid("tool name is missing"))?;
                    if !block["input"].is_object() || !used.insert(id.to_owned()) { return Err(invalid("tool input is malformed or id is duplicated")); }
                    calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":block["input"].to_string()}}));
                }
                Some("tool_result") if role == "user" => {
                    let id = block["tool_use_id"].as_str().ok_or_else(|| invalid("tool result id is missing"))?;
                    if !pending.remove(id) { return Err(invalid("tool result has no matching pending call")); }
                    let content = match &block["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(blocks) => {
                            let lines: Result<Vec<_>, _> = blocks.iter().map(|b| {
                                if b["type"] == "text" { b["text"].as_str().ok_or_else(|| invalid("bad result text")) }
                                else { Err(invalid("native tool result blocks cannot cross providers")) }
                            }).collect();
                            lines?.join("\n\n")
                        }
                        _ => return Err(invalid("tool result must contain text")),
                    };
                    // Preserve failure semantics even though Chat Completions has no is_error field.
                    let content = if block["is_error"] == true { format!("Tool error:\n{content}") } else { content };
                    results.push(json!({"role":"tool","tool_call_id":id,"content":content}));
                }
                _ => return Err(invalid("provider-native blocks (including signed thinking) cannot be translated to OpenAI; start a new conversation")),
            }
        }
        if !pending.is_empty() {
            return Err(invalid("all tool results must follow their assistant call before another turn"));
        }
        messages.extend(results);
        if !text.is_empty() || !calls.is_empty() {
            let content = (!text.is_empty()).then(|| text.join("\n\n"));
            let mut message = json!({"role":role,"content":content});
            if !calls.is_empty() {
                for call in &calls {
                    pending.insert(call["id"].as_str().unwrap().to_owned());
                }
                message["tool_calls"] = json!(calls);
            }
            messages.push(message);
        }
    }
    if !pending.is_empty() {
        return Err(invalid("conversation ends with unanswered tool calls"));
    }
    if messages.is_empty() {
        return Err(invalid("translated messages are empty"));
    }
    let mut body = json!({"model":model,"messages":messages,"max_completion_tokens":max_tokens,"store":false});
    if !req.tools.is_empty() {
        let tools: Result<Vec<_>, _> = req
            .tools
            .into_iter()
            .map(|tool| {
                let name =
                    tool["name"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| invalid("tool name is missing"))?;
                if !tool["input_schema"].is_object() {
                    return Err(invalid("tool schema is missing"));
                }
                let mut function = json!({"name":name,"parameters":tool["input_schema"]});
                if let Some(description) = tool.get("description") {
                    function["description"] = description.clone();
                }
                if let Some(strict) = tool.get("strict") {
                    function["strict"] = strict.clone();
                }
                Ok(json!({"type":"function","function":function}))
            })
            .collect();
        body["tools"] = json!(tools?);
    }
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    if let Some(schema) = req.output_schema {
        body["response_format"] =
            json!({"type":"json_schema","json_schema":{"name":"molt_output","strict":true,"schema":schema}});
    }
    if req.stream.is_some() {
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage":true});
    }
    Ok(Call { body, fallbacks: false, max_tokens })
}

pub(crate) fn decode(value: Value) -> Result<Message, String> {
    let bad = |s: &str| s.to_owned();
    let choices = value["choices"].as_array().ok_or_else(|| bad("choices missing"))?;
    if choices.len() != 1 || choices[0]["index"] != 0 {
        return Err(bad("expected one choice at index 0"));
    }
    let message = &choices[0]["message"];
    if message["role"] != "assistant" {
        return Err(bad("not an assistant message"));
    }
    for field in ["audio", "function_call", "reasoning_content", "reasoning", "annotations"] {
        if message.get(field).is_some_and(|v| !v.is_null() && v != &json!([])) {
            return Err(format!("unsupported native output {field}"));
        }
    }
    let reason = choices[0]["finish_reason"].as_str().ok_or_else(|| bad("finish reason missing"))?;
    let refused = !message["refusal"].is_null() || reason == "content_filter";
    let stop = if refused {
        "refusal"
    } else {
        match reason {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            _ => return Err(format!("unsupported finish reason {reason}")),
        }
    };
    let mut content = Vec::new();
    if !refused {
        match &message["content"] {
            Value::String(s) if !s.is_empty() => content.push(json!({"type":"text","text":s})),
            Value::String(_) | Value::Null => {}
            _ => return Err(bad("content is not text")),
        }
        if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
            let calls = calls.as_array().ok_or_else(|| bad("tool_calls is not an array"))?;
            if calls.len() > 64 {
                return Err(bad("too many tool calls"));
            }
            let mut ids = HashSet::new();
            for call in calls {
                if call["type"] != "function" {
                    return Err(bad("unsupported tool type"));
                }
                let id = call["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| bad("tool id missing"))?;
                if !ids.insert(id) {
                    return Err(bad("duplicate tool id"));
                }
                let name = call["function"]["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| bad("tool name missing"))?;
                let args = call["function"]["arguments"].as_str().ok_or_else(|| bad("arguments missing"))?;
                // An output-limit cut is never an executable tool, even if a prefix parses.
                let input: Value = match serde_json::from_str(args) {
                    Ok(v) => v,
                    Err(_) if reason == "length" => json!({}),
                    Err(e) => return Err(format!("invalid tool JSON: {e}")),
                };
                if !input.is_object() {
                    return Err(bad("tool input is not an object"));
                }
                content.push(json!({"type":"tool_use","id":id,"name":name,"input":input}));
            }
            if reason == "length" && content.iter().filter(|b| b["type"] == "tool_use").count() > 1 {
                return Err(bad("multiple cut tool calls cannot be used safely"));
            }
        }
        if reason == "tool_calls" && !content.iter().any(|b| b["type"] == "tool_use") {
            return Err(bad("tool_calls finish without tools"));
        }
    }
    let usage = &value["usage"];
    let prompt = usage["prompt_tokens"].as_u64();
    let cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    let write = usage["prompt_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0);
    let cache = cached.checked_add(write).ok_or_else(|| bad("cache counts overflow"))?;
    if prompt.is_some_and(|p| cache > p) {
        return Err(bad("cache usage exceeds prompt usage"));
    }
    let id = value["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| bad("response id missing"))?;
    let model = value["model"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| bad("response model missing"))?;
    serde_json::from_value(json!({"id":id,"model":model,"content":content,"stop_reason":stop,
        "stop_details":{"provider":"openai","finish_reason":reason},
        "usage":{"input_tokens":prompt.map(|p| p-cache),"output_tokens":usage["completion_tokens"],
            "cache_read_input_tokens":cached,"cache_creation_input_tokens":write}}))
    .map_err(|e| e.to_string())
}

#[derive(Default)]
pub(crate) struct Accumulator {
    id: Option<String>,
    model: Option<String>,
    text: String,
    refusal: String,
    tools: BTreeMap<u64, Value>,
    finish: Option<String>,
    usage: Option<Value>,
    bytes: usize,
    done: bool,
}

impl Accumulator {
    pub fn event(&mut self, event: Value) -> Result<Option<String>, String> {
        if self.done {
            return Err("event after [DONE]".into());
        }
        self.bytes = self.bytes.saturating_add(event.to_string().len());
        if self.bytes > 6 * 1024 * 1024 {
            return Err("streamed message exceeds 6 MiB".into());
        }
        if event["type"] == "openai_done" {
            self.done = true;
            return Ok(None);
        }
        if event.get("error").is_some() {
            return Err("OpenAI stream error; partial answer and usage are unknown".into());
        }
        if event["object"] != "chat.completion.chunk" {
            return Err("not a chat completion chunk".into());
        }
        for (field, stored) in [("id", &mut self.id), ("model", &mut self.model)] {
            let value = event[field].as_str().filter(|s| !s.is_empty()).ok_or_else(|| format!("{field} missing"))?;
            if stored.as_deref().is_some_and(|old| old != value) {
                return Err(format!("{field} changed during stream"));
            }
            *stored = Some(value.to_owned());
        }
        let choices = event["choices"].as_array().ok_or("choices missing")?;
        if !event["usage"].is_null() {
            if self.usage.is_some() || self.finish.is_none() || !choices.is_empty() {
                return Err("usage must follow the finished choice once".into());
            }
            self.usage = Some(event["usage"].clone());
        }
        if choices.is_empty() {
            return Ok(None);
        }
        if choices.len() != 1 || choices[0]["index"] != 0 || self.finish.is_some() {
            return Err("invalid choice order or index".into());
        }
        let delta = &choices[0]["delta"];
        if delta.get("role").is_some_and(|r| r != "assistant") {
            return Err("invalid delta role".into());
        }
        for field in ["audio", "function_call", "reasoning_content", "reasoning"] {
            if delta.get(field).is_some_and(|v| !v.is_null()) {
                return Err(format!("unsupported native delta {field}"));
            }
        }
        let text = delta["content"].as_str().unwrap_or("");
        if !delta["content"].is_null() && !delta["content"].is_string() {
            return Err("content delta is not text".into());
        }
        self.text.push_str(text);
        if let Some(refusal) = delta["refusal"].as_str() {
            self.refusal.push_str(refusal);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().filter(|i| *i < 64).ok_or("invalid tool index")?;
                if index > self.tools.len() as u64 {
                    return Err("tool index has a gap".into());
                }
                let tool = self
                    .tools
                    .entry(index)
                    .or_insert_with(|| json!({"id":"","type":"function","function":{"name":"","arguments":""}}));
                if call.get("type").is_some_and(|t| t != "function") {
                    return Err("unsupported tool type".into());
                }
                if let Some(value) = call["id"].as_str() {
                    if tool["id"].as_str().is_some_and(|old| !old.is_empty() && old != value) {
                        return Err("tool id changed".into());
                    }
                    tool["id"] = json!(value);
                }
                for field in ["name", "arguments"] {
                    if let Some(value) = call["function"][field].as_str() {
                        tool["function"][field].as_str().ok_or("invalid tool fragment")?;
                        let joined = format!("{}{value}", tool["function"][field].as_str().unwrap());
                        tool["function"][field] = json!(joined);
                    }
                }
            }
        }
        if let Some(reason) = choices[0]["finish_reason"].as_str() {
            self.finish = Some(reason.to_owned());
        }
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }

    pub fn finish(self) -> Result<Message, String> {
        if !self.done {
            return Err("OpenAI stream ended before [DONE]; usage is unknown".into());
        }
        let reason = self.finish.ok_or("OpenAI stream has no finish reason")?;
        let usage = self.usage.ok_or("OpenAI stream has no final token usage")?;
        let mut message =
            json!({"role":"assistant","content":self.text,"tool_calls":self.tools.into_values().collect::<Vec<_>>()});
        if !self.refusal.is_empty() {
            message["refusal"] = json!(self.refusal);
        }
        decode(
            json!({"id":self.id,"model":self.model,"choices":[{"index":0,"finish_reason":reason,"message":message}],"usage":usage}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{provider::Provider, stream::Decoder};
    #[test]
    fn every_utf8_and_crlf_byte_boundary_is_accepted_without_exposing_tool_json() {
        let chunk = |delta, finish| {
            format!(
                "data: {}\r\n\r\n",
                json!({"id":"c","model":"gpt-4.1","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
            )
        };
        let wire = format!(
            "{}{}{}data: {}\r\n\r\ndata: [DONE]\r\n\r\n",
            chunk(json!({"content":"你好"}), Value::Null),
            chunk(
                json!({"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"run","arguments":"{\"command\":\"你好\"}"}}]}),
                Value::Null
            ),
            chunk(json!({}), json!("tool_calls")),
            json!({"id":"c","model":"gpt-4.1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}})
        );
        for size in 1..=wire.len() {
            let mut decoder = Decoder::for_provider(Provider::Openai);
            let mut accumulator = Accumulator::default();
            let mut text = String::new();
            for bytes in wire.as_bytes().chunks(size) {
                for event in decoder.feed(bytes).unwrap() {
                    if let Some(t) = accumulator.event(event).unwrap() {
                        text.push_str(&t);
                    }
                }
            }
            decoder.finish().unwrap();
            assert_eq!(text, "你好");
            let response = accumulator.finish().unwrap();
            assert_eq!(response.content[1]["input"], json!({"command":"你好"}));
        }
    }
    #[test]
    fn native_output_and_impossible_cache_counts_are_refused() {
        let base = json!({"id":"c","model":"gpt-4.1","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"x"}}],"usage":{"prompt_tokens":1,"completion_tokens":2}});
        for field in ["audio", "function_call", "reasoning_content"] {
            let mut value = base.clone();
            value["choices"][0]["message"][field] = json!("native");
            assert!(decode(value).is_err());
        }
        let mut value = base.clone();
        value["usage"]["prompt_tokens_details"] = json!({"cached_tokens":2});
        assert!(decode(value).is_err());
        let mut value = base;
        value["usage"]["prompt_tokens_details"] = json!({"cached_tokens":u64::MAX,"cache_write_tokens":1});
        assert!(decode(value).is_err());
    }
}
