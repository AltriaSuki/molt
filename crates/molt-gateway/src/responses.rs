//! OpenAI Responses adapter. Every call is stateless (`store: false`): the
//! whole conversation goes in `input`, never as a `previous_response_id`, so
//! nothing depends on what the endpoint keeps, and a run resumes, replays and
//! audits from its own messages alone.
//!
//! The native state a stateless conversation needs is a reasoning model's
//! reasoning. Its items come back as `openai_reasoning` blocks, with their
//! id and the encrypted content the call asked for, and go back verbatim on
//! the next turn, in their place before the calls they led to. No other
//! protocol accepts them: they are refused before anything is sent.

use std::collections::HashSet;

use molt_api::model::{CompleteRequest, Effort};
use molt_proto::RemoteError;
use serde_json::{json, Value};

use crate::api::Message;
use crate::body::Call;
use crate::provider::{invalid, Capabilities};

/// The block type that carries one Responses reasoning item.
pub(crate) const REASONING: &str = "openai_reasoning";
/// Most output items one response may have.
const MAX_ITEMS: usize = 1024;
/// Most bytes of events one streamed response may have.
const MAX_STREAM: usize = 6 * 1024 * 1024;

/// Whether `body` was built here: a Responses body has `input` and no
/// `messages`, which every other body the gateway builds has.
pub(crate) fn is_responses(body: &Value) -> bool {
    body.get("input").is_some() && body.get("messages").is_none()
}

/// Refuse a Messages API body carrying a Responses reasoning item, which the
/// API (or a compatible endpoint that ignores unknown blocks) cannot honor.
pub(crate) fn refuse_on_messages(body: &Value) -> Result<(), RemoteError> {
    let native = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .any(|b| b["type"] == REASONING);
    if native {
        return Err(invalid(
            "OpenAI Responses reasoning cannot be sent to the Anthropic Messages API; start a new conversation",
        ));
    }
    Ok(())
}

/// The Responses body for a request `openai::build` has already checked
/// against the model's capabilities.
pub(crate) fn build(
    model: &str,
    req: CompleteRequest,
    caps: &Capabilities,
    effort: Option<Effort>,
    max_tokens: u32,
) -> Result<Call, RemoteError> {
    let mut input = Vec::new();
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
        // An assistant turn keeps its order: reasoning goes back before the calls it led to.
        let mut items = Vec::new();
        let mut parts = Vec::new();
        let mut results = Vec::new();
        for block in blocks {
            match (role, block["type"].as_str()) {
                (_, Some("text")) => {
                    if block.get("citations").is_some() {
                        return Err(invalid("native citations cannot be translated to OpenAI"));
                    }
                    let text = block["text"].as_str().ok_or_else(|| invalid("text block is malformed"))?;
                    if role == "user" {
                        parts.push(json!({"type":"input_text","text":text}));
                    } else {
                        items.push(json!({"role":"assistant","content":text}));
                    }
                }
                ("assistant", Some("tool_use")) => {
                    if !caps.tools {
                        return Err(invalid("this OpenAI model does not support tool history"));
                    }
                    let id = nonempty(&block["id"]).ok_or_else(|| invalid("tool id is missing"))?;
                    let name = nonempty(&block["name"]).ok_or_else(|| invalid("tool name is missing"))?;
                    if !block["input"].is_object() || !used.insert(id.to_owned()) {
                        return Err(invalid("tool input is malformed or id is duplicated"));
                    }
                    items.push(json!({"type":"function_call","call_id":id,"name":name,
                        "arguments":block["input"].to_string()}));
                }
                ("assistant", Some(REASONING)) => items.extend(reasoning_input(&block)?),
                // Another endpoint's plain reasoning is that endpoint's state, not this one's.
                ("assistant", Some("thinking")) if molt_api::model::chat_reasoning(&block).is_some() => {}
                ("user", Some("tool_result")) => {
                    let id = block["tool_use_id"].as_str().ok_or_else(|| invalid("tool result id is missing"))?;
                    if !pending.remove(id) {
                        return Err(invalid("tool result has no matching pending call"));
                    }
                    let output = result_text(&block["content"])?;
                    // The protocol has no error flag on an output, so the failure is said in it.
                    let output = if block["is_error"] == true { format!("Tool error:\n{output}") } else { output };
                    results.push(json!({"type":"function_call_output","call_id":id,"output":output}));
                }
                _ => {
                    return Err(invalid(
                        "provider-native blocks (including signed thinking) cannot be translated to the OpenAI \
                         Responses API; start a new conversation",
                    ))
                }
            }
        }
        if !pending.is_empty() {
            return Err(invalid("all tool results must follow their assistant call before another turn"));
        }
        input.extend(results);
        if !parts.is_empty() {
            input.push(json!({"role":"user","content":parts}));
        }
        for item in items {
            if item["type"] == "function_call" {
                pending.insert(item["call_id"].as_str().unwrap_or_default().to_owned());
            }
            input.push(item);
        }
    }
    if !pending.is_empty() {
        return Err(invalid("conversation ends with unanswered tool calls"));
    }
    if input.is_empty() {
        return Err(invalid("translated input is empty"));
    }
    let mut body = json!({"model":model,"input":input,"max_output_tokens":max_tokens,"store":false});
    if let Some(system) = req.system.filter(|s| !s.is_empty()) {
        body["instructions"] = json!(system);
    }
    if !req.tools.is_empty() {
        let tools: Result<Vec<_>, _> = req
            .tools
            .into_iter()
            .map(|tool| {
                let name = nonempty(&tool["name"]).ok_or_else(|| invalid("tool name is missing"))?;
                if !tool["input_schema"].is_object() {
                    return Err(invalid("tool schema is missing"));
                }
                // Responses makes a function strict unless told otherwise, and a strict
                // schema must close every object, which tool schemas here need not.
                let strict = tool.get("strict").cloned().unwrap_or(json!(false));
                let mut function =
                    json!({"type":"function","name":name,"parameters":tool["input_schema"],"strict":strict});
                if let Some(description) = tool.get("description") {
                    function["description"] = description.clone();
                }
                Ok(function)
            })
            .collect();
        body["tools"] = json!(tools?);
    }
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort":effort});
    }
    // A model with efforts reasons; without stored state its reasoning comes
    // back to the next turn only encrypted.
    if !caps.efforts.is_empty() {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(schema) = req.output_schema {
        body["text"] = json!({"format":{"type":"json_schema","name":"molt_output","strict":true,"schema":schema}});
    }
    if req.stream.is_some() {
        body["stream"] = json!(true);
    }
    Ok(Call { body, max_tokens, ..Call::default() })
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

fn result_text(content: &Value) -> Result<String, RemoteError> {
    match content {
        Value::String(s) => Ok(s.clone()),
        Value::Array(blocks) => {
            let lines: Result<Vec<_>, _> = blocks
                .iter()
                .map(|b| {
                    if b["type"] == "text" {
                        b["text"].as_str().ok_or_else(|| invalid("bad result text"))
                    } else {
                        Err(invalid("native tool result blocks cannot cross providers"))
                    }
                })
                .collect();
            Ok(lines?.join("\n\n"))
        }
        _ => Err(invalid("tool result must contain text")),
    }
}

/// The input item for a reasoning block. Without encrypted content there is
/// nothing a stateless call can send back, so the item is left out, as the
/// API allows.
fn reasoning_input(block: &Value) -> Result<Option<Value>, RemoteError> {
    let id = nonempty(&block["id"]).ok_or_else(|| invalid("reasoning item id is missing"))?;
    let summary = summary(&block["summary"]).map_err(invalid)?;
    match &block["encrypted_content"] {
        Value::Null => Ok(None),
        Value::String(sealed) if !sealed.is_empty() => {
            Ok(Some(json!({"type":"reasoning","id":id,"summary":summary,"encrypted_content":sealed})))
        }
        _ => Err(invalid("reasoning item encrypted_content is malformed")),
    }
}

/// A reasoning summary: `summary_text` parts only.
fn summary(value: &Value) -> Result<Value, String> {
    let parts = match value {
        Value::Null => return Ok(json!([])),
        Value::Array(parts) => parts,
        _ => return Err("reasoning summary is not a list".into()),
    };
    if parts.iter().all(|p| p["type"] == "summary_text" && p["text"].is_string()) {
        Ok(value.clone())
    } else {
        Err("reasoning summary has a part that is not summary_text".into())
    }
}

/// A finished response object, from a whole reply or a stream's last event.
pub(crate) fn decode(value: Value) -> Result<Message, String> {
    if value.get("object").is_some_and(|o| o != "response") {
        return Err("not a response object".into());
    }
    let id = nonempty(&value["id"]).ok_or("response id missing")?;
    let model = nonempty(&value["model"]).ok_or("response model missing")?;
    let status = value["status"].as_str().ok_or("response status missing")?;
    let reason = value["incomplete_details"]["reason"].as_str();
    let (cut, filtered) = match (status, reason) {
        ("completed", _) => (false, false),
        ("incomplete", Some("max_output_tokens")) => (true, false),
        ("incomplete", Some("content_filter")) => (false, true),
        ("incomplete", other) => return Err(format!("response incomplete: {}", other.unwrap_or("no reason given"))),
        ("failed", _) => {
            let error = &value["error"];
            return Err(format!(
                "the response failed: {}: {}; usage is unknown",
                error["code"].as_str().unwrap_or("no code"),
                error["message"].as_str().unwrap_or("no message")
            ));
        }
        (other, _) => return Err(format!("unsupported response status {other}")),
    };
    let output = value["output"].as_array().ok_or("output missing")?;
    if output.len() > MAX_ITEMS {
        return Err("too many output items".into());
    }
    let mut content = Vec::new();
    let mut refused = filtered;
    let mut ids = HashSet::new();
    let mut calls = 0;
    for (i, item) in output.iter().enumerate() {
        match item["type"].as_str() {
            Some("message") => {
                if item["role"] != "assistant" {
                    return Err("output message is not the assistant's".into());
                }
                for part in item["content"].as_array().ok_or("message content is not a list")? {
                    match part["type"].as_str() {
                        Some("output_text") => {
                            if part.get("annotations").is_some_and(|a| !a.is_null() && a != &json!([])) {
                                return Err("unsupported native output annotations".into());
                            }
                            let text = part["text"].as_str().ok_or("output text is not text")?;
                            if !text.is_empty() {
                                content.push(json!({"type":"text","text":text}));
                            }
                        }
                        Some("refusal") => refused = true,
                        other => {
                            return Err(format!("unsupported message content {}", other.unwrap_or("without a type")))
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = nonempty(&item["call_id"]).ok_or("function call_id missing")?;
                if !ids.insert(call_id) {
                    return Err("duplicate function call_id".into());
                }
                let name = nonempty(&item["name"]).ok_or("function name missing")?;
                let arguments = item["arguments"].as_str().ok_or("function arguments missing")?;
                let finished = item["status"].as_str().is_none_or(|s| s == "completed");
                let parsed = serde_json::from_str::<Value>(arguments);
                // A call the output limit cut is never executable, even if a prefix
                // parses: it must be the last item, which the planner does not run.
                let input = match parsed {
                    Ok(v) if finished => v,
                    _ if cut && i + 1 == output.len() => json!({}),
                    Ok(_) => return Err("a function call did not finish".into()),
                    Err(e) => return Err(format!("invalid tool JSON: {e}")),
                };
                if !input.is_object() {
                    return Err("tool input is not an object".into());
                }
                content.push(json!({"type":"tool_use","id":call_id,"name":name,"input":input}));
                calls += 1;
            }
            Some("reasoning") => {
                let id = nonempty(&item["id"]).ok_or("reasoning item id missing")?;
                let summary = summary(&item["summary"])?;
                let sealed = &item["encrypted_content"];
                if !sealed.is_null() && !sealed.is_string() {
                    return Err("reasoning encrypted_content is not text".into());
                }
                content.push(json!({"type":REASONING,"id":id,"summary":summary,"encrypted_content":sealed}));
            }
            other => return Err(format!("unsupported native output item {}", other.unwrap_or("without a type"))),
        }
    }
    let stop = if refused {
        content.clear();
        "refusal"
    } else if cut {
        "max_tokens"
    } else if calls > 0 {
        "tool_use"
    } else {
        "end_turn"
    };
    let usage = &value["usage"];
    let input = usage["input_tokens"].as_u64();
    let cached = usage["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    if input.is_some_and(|n| cached > n) {
        return Err("cache usage exceeds input usage".into());
    }
    serde_json::from_value(json!({"id":id,"model":model,"content":content,"stop_reason":stop,
        "stop_details":{"provider":"openai","protocol":"responses","status":status,"incomplete_reason":reason},
        "usage":{"input_tokens":input.map(|n| n - cached),"output_tokens":usage["output_tokens"],
            "cache_read_input_tokens":cached,"cache_creation_input_tokens":0,
            "reasoning_tokens":usage["output_tokens_details"]["reasoning_tokens"].as_u64()}}))
    .map_err(|e| e.to_string())
}

/// One output item of a streamed response, by its `output_index`.
struct Item {
    kind: String,
    id: Option<String>,
    done: Option<Value>,
}

/// A streamed response. Only text deltas leave it, as previews; function
/// arguments are taken from finished items alone, and the reply from the
/// last event's response, so a stream cut anywhere before it has no answer.
#[derive(Default)]
pub(crate) struct Accumulator {
    id: Option<String>,
    items: Vec<Item>,
    seq: Option<u64>,
    terminal: Option<Value>,
    bytes: usize,
}

impl Accumulator {
    pub fn event(&mut self, event: Value) -> Result<Option<String>, String> {
        self.bytes = self.bytes.saturating_add(event.to_string().len());
        if self.bytes > MAX_STREAM {
            return Err("streamed response exceeds 6 MiB".into());
        }
        let kind = event["type"].as_str().ok_or("stream event has no type")?;
        if self.terminal.is_some() {
            return Err(format!("{kind} after the response finished"));
        }
        if let Some(seq) = event.get("sequence_number") {
            let seq = seq.as_u64().ok_or("sequence_number is not a number")?;
            if self.seq.is_some_and(|last| seq <= last) {
                return Err("sequence_number went back".into());
            }
            self.seq = Some(seq);
        }
        if kind == "error" {
            return Err(format!(
                "OpenAI stream error {}: {}; partial answer and usage are unknown",
                event["code"].as_str().unwrap_or("without a code"),
                event["message"].as_str().unwrap_or("")
            ));
        }
        if kind != "response.created" && self.id.is_none() {
            return Err(format!("{kind} before response.created"));
        }
        match kind {
            "response.created" => {
                if self.id.is_some() {
                    return Err("duplicate response.created".into());
                }
                self.id = Some(nonempty(&event["response"]["id"]).ok_or("response.created has no id")?.to_owned());
            }
            "response.output_item.added" => {
                if event["output_index"].as_u64() != Some(self.items.len() as u64) || self.items.len() >= MAX_ITEMS {
                    return Err("output item out of order".into());
                }
                let item = &event["item"];
                let kind = item["type"].as_str().ok_or("output item has no type")?.to_owned();
                self.items.push(Item { kind, id: item["id"].as_str().map(str::to_owned), done: None });
            }
            "response.output_item.done" => {
                let item = self.open_item(&event)?;
                if event["item"]["type"].as_str() != Some(item.kind.as_str()) {
                    return Err("output item changed type".into());
                }
                item.done = Some(event["item"].clone());
            }
            "response.output_text.delta" => {
                let item = self.open_item(&event)?;
                if item.kind != "message" {
                    return Err("text delta outside a message".into());
                }
                let delta = event["delta"].as_str().ok_or("text delta is not text")?;
                return Ok((!delta.is_empty()).then(|| delta.to_owned()));
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                if self.open_item(&event)?.kind != "function_call" {
                    return Err("arguments outside a function call".into());
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let response = &event["response"];
                if response["id"].as_str() != self.id.as_deref() {
                    return Err("response id changed during the stream".into());
                }
                if response["status"].as_str() != kind.strip_prefix("response.") {
                    return Err(format!("{kind} with another status"));
                }
                self.terminal = Some(response.clone());
            }
            // Content parts, refusal and reasoning summary deltas, progress:
            // the finished items and the last response carry all of them.
            other if other.starts_with("response.") => {}
            other => return Err(format!("unexpected stream event {other}")),
        }
        Ok(None)
    }

    /// The unfinished item an event names by `output_index` and `item_id`.
    fn open_item(&mut self, event: &Value) -> Result<&mut Item, String> {
        let index = event["output_index"].as_u64().ok_or("event has no output_index")?;
        let item = usize::try_from(index).ok().and_then(|i| self.items.get_mut(i)).ok_or("event for no item")?;
        if item.done.is_some() {
            return Err("event for a finished item".into());
        }
        let named = event.get("item_id").and_then(Value::as_str);
        if named.is_some_and(|id| item.id.as_deref().is_some_and(|own| own != id)) {
            return Err("event names another item".into());
        }
        Ok(item)
    }

    pub fn finish(self) -> Result<Message, String> {
        let mut response =
            self.terminal.ok_or("OpenAI Responses stream ended before the response finished; usage is unknown")?;
        // Some endpoints leave the last event's output empty; the finished items are then the output.
        if response["output"].as_array().is_none_or(Vec::is_empty) && !self.items.is_empty() {
            let done: Option<Vec<Value>> = self.items.into_iter().map(|i| i.done).collect();
            response["output"] = json!(done.ok_or("an output item did not finish")?);
        }
        decode(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Decoder;

    fn finished(output: Value, status: &str) -> Value {
        json!({"id":"resp_1","object":"response","model":"gpt-5","status":status,"output":output,
            "usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":3}})
    }

    #[test]
    fn every_byte_boundary_of_a_stream_gives_the_same_reply_and_previews_only_text() {
        let events = [
            json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_1","status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","sequence_number":2,"output_index":0,"item_id":"msg_1","content_index":0,"delta":"你好"}),
            json!({"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"run","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","sequence_number":4,"output_index":1,"item_id":"fc_1","delta":"{\"command\":"}),
            // Interleaved: the message's text goes on after the call has begun.
            json!({"type":"response.output_text.delta","sequence_number":5,"output_index":0,"item_id":"msg_1","content_index":0,"delta":"!"}),
            json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"你好!","annotations":[]}]}}),
            json!({"type":"response.output_item.done","sequence_number":7,"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"run","arguments":"{\"command\":\"ls\"}","status":"completed"}}),
            json!({"type":"response.completed","sequence_number":8,"response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":2}}}),
        ];
        let wire: String =
            events.iter().map(|e| format!("event: {}\r\ndata: {e}\r\n\r\n", e["type"].as_str().unwrap())).collect();
        for size in 1..=wire.len() {
            let mut decoder = Decoder::default();
            let mut accumulator = Accumulator::default();
            let mut text = String::new();
            for bytes in wire.as_bytes().chunks(size) {
                for event in decoder.feed(bytes).unwrap() {
                    text.extend(accumulator.event(event).unwrap());
                }
            }
            decoder.finish().unwrap();
            assert_eq!(text, "你好!");
            let reply = accumulator.finish().unwrap();
            assert_eq!(
                reply.content[1],
                json!({"type":"tool_use","id":"call_1","name":"run","input":{"command":"ls"}})
            );
            assert_eq!(reply.stop_reason.as_deref(), Some("tool_use"));
        }
    }

    #[test]
    fn a_stream_cut_before_its_last_event_or_out_of_order_has_no_reply() {
        let created = json!({"type":"response.created","response":{"id":"resp_1"}});
        let mut a = Accumulator::default();
        a.event(created.clone()).unwrap();
        assert!(a.finish().unwrap_err().contains("usage is unknown"));
        let mut a = Accumulator::default();
        assert!(a
            .event(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}))
            .is_err());
        let mut a = Accumulator::default();
        a.event(created.clone()).unwrap();
        assert!(a
            .event(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message"}}))
            .is_err());
        let mut a = Accumulator::default();
        a.event(json!({"type":"response.created","sequence_number":3,"response":{"id":"resp_1"}})).unwrap();
        assert!(a.event(json!({"type":"response.in_progress","sequence_number":2})).is_err());
        let mut a = Accumulator::default();
        a.event(created).unwrap();
        let error = a.event(json!({"type":"error","code":"server_error","message":"boom"})).unwrap_err();
        assert!(error.contains("usage are unknown"), "{error}");
    }

    #[test]
    fn a_cut_call_is_last_with_no_input_and_an_unfinished_one_elsewhere_is_refused() {
        let call = |status: &str, args: &str| json!({"type":"function_call","call_id":"c","name":"run","arguments":args,"status":status});
        let mut cut = finished(json!([call("incomplete", "{\"command\":\"rm")]), "incomplete");
        cut["incomplete_details"] = json!({"reason":"max_output_tokens"});
        let reply = decode(cut.clone()).unwrap();
        assert_eq!(reply.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(reply.content[0]["input"], json!({}));
        cut["output"] = json!([call("incomplete", "{}"), {"type":"message","role":"assistant","content":[]}]);
        assert!(decode(cut).is_err());
        assert!(decode(finished(json!([call("incomplete", "{}")]), "completed")).is_err());
    }

    #[test]
    fn native_outputs_impossible_usage_and_failures_are_refused() {
        for output in [
            json!([{"type":"web_search_call","id":"ws"}]),
            json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x","annotations":[{"type":"url_citation"}]}]}]),
            json!([{"type":"reasoning","id":"rs","summary":[{"type":"other"}]}]),
        ] {
            assert!(decode(finished(output, "completed")).is_err());
        }
        let mut value = finished(json!([]), "completed");
        value["usage"]["input_tokens_details"]["cached_tokens"] = json!(11);
        assert!(decode(value).is_err());
        let mut failed = finished(json!([]), "failed");
        failed["error"] = json!({"code":"server_error","message":"boom"});
        assert!(decode(failed).unwrap_err().contains("boom"));
        let mut filtered = finished(
            json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x"}]}]),
            "incomplete",
        );
        filtered["incomplete_details"] = json!({"reason":"content_filter"});
        let refused = decode(filtered).unwrap();
        assert_eq!((refused.stop_reason.as_deref(), refused.content.len()), (Some("refusal"), 0));
    }

    #[test]
    fn reasoning_without_encrypted_content_is_kept_but_not_sent_back() {
        let block = json!({"type":REASONING,"id":"rs_1","summary":[],"encrypted_content":null});
        assert_eq!(reasoning_input(&block).unwrap(), None);
        let block = json!({"type":REASONING,"id":"rs_1","summary":[],"encrypted_content":"sealed"});
        assert_eq!(reasoning_input(&block).unwrap().unwrap()["encrypted_content"], "sealed");
        assert!(reasoning_input(&json!({"type":REASONING,"summary":[]})).is_err());
    }
}
