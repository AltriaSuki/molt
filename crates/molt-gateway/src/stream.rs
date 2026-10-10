//! Bounded SSE framing and Messages API accumulation. Only text previews
//! leave this module; tool JSON and signed thinking stay in the final reply.

use serde_json::{json, Value};

use crate::api::Message;

const MAX_EVENT: usize = 1024 * 1024;
const MAX_MESSAGE: usize = 6 * 1024 * 1024;
const MAX_BLOCKS: usize = 1024;

#[derive(Default)]
pub(crate) struct Decoder {
    line: Vec<u8>,
    data: String,
    event: String,
    bytes: usize,
    cr: bool,
}

impl Decoder {
    /// Accept arbitrary byte boundaries, including in UTF-8 and CRLF.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Value>, String> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.cr && byte == b'\n' {
                self.cr = false;
                continue;
            }
            self.cr = byte == b'\r';
            self.bytes += 1;
            if self.bytes > MAX_EVENT {
                return Err("SSE event exceeds 1 MiB".into());
            }
            if byte != b'\n' && byte != b'\r' {
                self.line.push(byte);
                continue;
            }
            let line = std::str::from_utf8(&self.line).map_err(|_| "SSE contains invalid UTF-8")?;
            if line.is_empty() {
                if !self.data.is_empty() {
                    let value: Value = serde_json::from_str(self.data.trim_end_matches('\n'))
                        .map_err(|e| format!("invalid SSE JSON: {e}"))?;
                    if !self.event.is_empty() && value["type"].as_str() != Some(self.event.as_str()) {
                        return Err("SSE event name does not match its data type".into());
                    }
                    events.push(value);
                }
                self.data.clear();
                self.event.clear();
                self.bytes = 0;
            } else if !line.starts_with(':') {
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "data" => {
                        self.data.push_str(value);
                        self.data.push('\n');
                    }
                    "event" => self.event = value.to_owned(),
                    _ => {}
                }
            }
            self.line.clear();
        }
        Ok(events)
    }

    pub fn finish(&self) -> Result<(), String> {
        if self.line.is_empty() && self.data.is_empty() && self.event.is_empty() {
            Ok(())
        } else {
            Err("SSE ended in the middle of an event".into())
        }
    }
}

struct Block {
    value: Value,
    input: String,
}

#[derive(Default)]
pub(crate) struct Accumulator {
    message: Option<Value>,
    active: Option<Block>,
    size: usize,
    delta: bool,
    stopped: bool,
}

impl Accumulator {
    /// Apply one event and return only its user-visible text delta.
    pub fn event(&mut self, event: Value) -> Result<Option<String>, String> {
        self.size = self.size.saturating_add(event.to_string().len());
        if self.size > MAX_MESSAGE {
            return Err("streamed message exceeds 6 MiB".into());
        }
        let kind = event["type"].as_str().ok_or("stream event has no type")?;
        if self.stopped && kind != "ping" {
            return Err("event after message_stop".into());
        }
        match kind {
            "ping" => {}
            "error" => return Err(format!("stream error: {}", event["error"])),
            "message_start" => {
                if self.message.is_some() {
                    return Err("duplicate message_start".into());
                }
                let msg = &event["message"];
                if !msg["content"].as_array().is_some_and(Vec::is_empty)
                    || msg["id"].as_str().is_none()
                    || msg["model"].as_str().is_none()
                {
                    return Err("invalid message_start".into());
                }
                self.message = Some(msg.clone());
            }
            "content_block_start" => {
                let msg = self.message.as_ref().ok_or("content before message_start")?;
                let blocks = msg["content"].as_array().ok_or("invalid content array")?;
                if self.delta
                    || self.active.is_some()
                    || blocks.len() >= MAX_BLOCKS
                    || event["index"].as_u64() != Some(blocks.len() as u64)
                    || event["content_block"]["type"].as_str().is_none()
                {
                    return Err("invalid content_block_start order or index".into());
                }
                let value = event["content_block"].clone();
                let text = (value["type"] == "text").then(|| value["text"].as_str().map(str::to_owned)).flatten();
                self.active = Some(Block { value, input: String::new() });
                return Ok(text.filter(|s| !s.is_empty()));
            }
            "content_block_delta" => {
                self.check_index(&event)?;
                let block = self.active.as_mut().ok_or("delta outside a content block")?;
                let d = &event["delta"];
                let kind = d["type"].as_str().ok_or("content delta has no type")?;
                let (field, block_kind) = match kind {
                    "text_delta" => ("text", "text"),
                    "thinking_delta" => ("thinking", "thinking"),
                    "signature_delta" => ("signature", "thinking"),
                    "input_json_delta" => {
                        if block.value["type"] != "tool_use" && block.value["type"] != "server_tool_use" {
                            return Err("JSON delta outside a tool block".into());
                        }
                        block.input.push_str(d["partial_json"].as_str().ok_or("JSON delta is not a string")?);
                        return Ok(None);
                    }
                    "citations_delta" => {
                        if block.value["type"] != "text" || !d["citation"].is_object() {
                            return Err("invalid citation delta".into());
                        }
                        if block.value["citations"].is_null() {
                            block.value["citations"] = json!([]);
                        }
                        block.value["citations"].as_array_mut().ok_or("invalid citations")?.push(d["citation"].clone());
                        return Ok(None);
                    }
                    _ => return Err(format!("unsupported content delta {kind:?}")),
                };
                if block.value["type"] != block_kind {
                    return Err(format!("{kind} does not match its block"));
                }
                let text = d[field].as_str().ok_or("content delta is not a string")?;
                let Value::String(target) = &mut block.value[field] else {
                    return Err("content field is not a string".into());
                };
                target.push_str(text);
                if kind == "text_delta" && !text.is_empty() {
                    return Ok(Some(text.to_owned()));
                }
            }
            "content_block_stop" => {
                self.check_index(&event)?;
                let mut block = self.active.take().ok_or("stop outside a content block")?;
                if !block.input.is_empty() {
                    let input: Value =
                        serde_json::from_str(&block.input).map_err(|e| format!("invalid tool JSON: {e}"))?;
                    if !input.is_object() {
                        return Err("tool input is not an object".into());
                    }
                    block.value["input"] = input;
                }
                if (block.value["type"] == "tool_use" || block.value["type"] == "server_tool_use")
                    && !block.value["input"].is_object()
                {
                    return Err("tool input is not an object".into());
                }
                self.message.as_mut().ok_or("stop before message_start")?["content"]
                    .as_array_mut()
                    .ok_or("invalid content array")?
                    .push(block.value);
            }
            "message_delta" => {
                if self.active.is_some() {
                    return Err("message_delta before content_block_stop".into());
                }
                let msg = self.message.as_mut().ok_or("delta before message_start")?;
                for (key, value) in event["delta"].as_object().ok_or("invalid message delta")? {
                    if matches!(key.as_str(), "stop_reason" | "stop_sequence" | "stop_details") {
                        msg[key] = value.clone();
                    }
                }
                if let Some(usage) = event["usage"].as_object() {
                    if !msg["usage"].is_object() {
                        msg["usage"] = json!({});
                    }
                    for (key, value) in usage {
                        msg["usage"][key] = value.clone();
                    }
                }
                self.delta = true;
            }
            "message_stop" => {
                if self.active.is_some() || !self.delta || self.message.is_none() {
                    return Err("message_stop before the message is complete".into());
                }
                self.stopped = true;
            }
            // The protocol permits new event types. Unknown content deltas
            // above fail explicitly because silently losing them would corrupt
            // the content returned on the next turn.
            _ => {}
        }
        Ok(None)
    }

    fn check_index(&self, event: &Value) -> Result<(), String> {
        let msg = self.message.as_ref().ok_or("content before message_start")?;
        let blocks = msg["content"].as_array().ok_or("invalid content array")?;
        if event["index"].as_u64() == Some(blocks.len() as u64) {
            Ok(())
        } else {
            Err("content block index is out of order".into())
        }
    }

    pub fn finish(self) -> Result<Message, String> {
        if !self.stopped {
            return Err("stream ended without message_stop".into());
        }
        serde_json::from_value(self.message.ok_or("missing message_start")?)
            .map_err(|e| format!("invalid completed message: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_handles_every_byte_boundary_and_multiline_data() {
        let input = "event: ping\r\ndata: {\r\ndata: \"type\": \"ping\", \"text\": \"你好\"}\r\n\r\n";
        for split in 0..=input.len() {
            let mut d = Decoder::default();
            let mut events = d.feed(&input.as_bytes()[..split]).unwrap();
            events.extend(d.feed(&input.as_bytes()[split..]).unwrap());
            d.finish().unwrap();
            assert_eq!(events, [json!({ "type": "ping", "text": "你好" })]);
        }
        let mut d = Decoder::default();
        let mut events = Vec::new();
        for &byte in input.as_bytes() {
            events.extend(d.feed(&[byte]).unwrap());
        }
        assert_eq!(events.len(), 1);
        d.finish().unwrap();
    }

    #[test]
    fn framing_rejects_bad_or_unbounded_events() {
        assert!(Decoder::default().feed(b"data: not json\n\n").is_err());
        assert!(Decoder::default().feed(b"event: ping\ndata: {\"type\":\"error\"}\n\n").is_err());
        assert!(Decoder::default().feed(&vec![b'x'; MAX_EVENT + 1]).is_err());
        let mut d = Decoder::default();
        d.feed(b"data: {}").unwrap();
        assert!(d.finish().is_err());
    }

    fn start() -> Value {
        json!({ "type": "message_start", "message": { "id": "m", "model": "test", "content": [],
            "usage": { "input_tokens": 5, "cache_read_input_tokens": 3, "output_tokens": 1 } } })
    }

    #[test]
    fn preserves_signed_thinking_and_waits_for_complete_tool_json() {
        let mut a = Accumulator::default();
        a.event(start()).unwrap();
        for (index, block, deltas) in [
            (
                0,
                json!({"type":"thinking", "thinking":"", "signature":""}),
                vec![
                    json!({"type":"thinking_delta", "thinking":"private"}),
                    json!({"type":"signature_delta", "signature":"sig"}),
                ],
            ),
            (1, json!({"type":"text", "text":""}), vec![json!({"type":"text_delta", "text":"hello"})]),
            (
                2,
                json!({"type":"tool_use", "id":"t", "name":"run", "input":{}}),
                vec![
                    json!({"type":"input_json_delta", "partial_json":"{\"command\":"}),
                    json!({"type":"input_json_delta", "partial_json":"\"ls\"}"}),
                ],
            ),
        ] {
            a.event(json!({"type":"content_block_start", "index":index, "content_block":block})).unwrap();
            for delta in deltas {
                let preview = a.event(json!({"type":"content_block_delta", "index":index, "delta":delta})).unwrap();
                assert_eq!(preview, (index == 1).then(|| "hello".into()));
            }
            a.event(json!({"type":"content_block_stop", "index":index})).unwrap();
        }
        a.event(json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":9}}))
            .unwrap();
        a.event(json!({"type":"message_stop"})).unwrap();
        let m = a.finish().unwrap();
        assert_eq!(m.content[0], json!({"type":"thinking", "thinking":"private", "signature":"sig"}));
        assert_eq!(m.content[2]["input"], json!({"command":"ls"}));
        let usage: molt_api::model::Usage = m.usage.into();
        assert_eq!((usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens), (5, 9, 3));
    }

    #[test]
    fn truncated_misordered_and_malformed_tool_streams_fail() {
        let mut a = Accumulator::default();
        assert!(a.event(json!({"type":"message_stop"})).is_err());
        a.event(start()).unwrap();
        assert!(a.event(start()).is_err());
        assert!(a
            .event(json!({"type":"content_block_start", "index":1000000, "content_block":{"type":"text","text":""}}))
            .is_err());
        a.event(json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use","input":{}}}))
            .unwrap();
        a.event(
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta","partial_json":"{"}}),
        )
        .unwrap();
        assert!(a.event(json!({"type":"content_block_stop", "index":0})).is_err());
        assert!(a.finish().is_err());
    }
}
