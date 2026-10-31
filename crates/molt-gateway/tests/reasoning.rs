//! A DeepSeek-style reasoning model on a named Chat Completions endpoint,
//! against a fake endpoint that answers in DeepSeek's format: its
//! `reasoning_content` comes back as unsigned thinking, goes back verbatim
//! through the tool calls of the user's turn it was given in and never
//! after it, never reaches another protocol, and its reasoning tokens and
//! endpoint are reported with the usage. No real model API is called.

use std::collections::BTreeMap;

use molt_api::model::{tool_result, user_blocks, user_text, CompleteRequest, StreamContext, CHAT_REASONING};
use molt_gateway::probe::Check;
use molt_gateway::{Config, Gateway};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL: &str = "deepseek-reasoner";

/// Plays DeepSeek's reasoner: a user message gets reasoning and a call to
/// the first tool offered, a tool result (or a request without tools) gets
/// reasoning and an answer. The call's id
/// and the reasoning name the request's length, so each turn is told apart.
struct DeepSeek;

impl Respond for DeepSeek {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = req.body_json().unwrap();
        let messages = body["messages"].as_array().unwrap();
        let n = messages.len();
        let tool = body["tools"][0]["function"]["name"].as_str();
        let (reasoning, content, calls, finish) = if messages[n - 1]["role"] == "tool" || tool.is_none() {
            (format!("The file says hi ({n})."), json!(format!("It says hi ({n}).")), Value::Null, "stop")
        } else {
            let call = json!([{"id":format!("call_{n}"),"type":"function",
                "function":{"name":tool,"arguments":"{\"path\":\"a.rs\"}"}}]);
            (format!("I should read a.rs first ({n})."), Value::Null, call, "tool_calls")
        };
        let usage = json!({"prompt_tokens":100,"completion_tokens":40,
            "prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"reasoning_tokens":25}});
        let id = format!("ds-{n}");
        if body["stream"] != true {
            let mut message = json!({"role":"assistant","content":content,"reasoning_content":reasoning});
            if !calls.is_null() {
                message["tool_calls"] = calls;
            }
            return ResponseTemplate::new(200).set_body_json(json!({"id":id,"object":"chat.completion","model":MODEL,
                "choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":usage}));
        }
        // Streamed, the reasoning comes first, cut across chunks, then the answer or the call.
        let chunk = |delta: Value, finish: Value| {
            let chunk = json!({"id":id,"object":"chat.completion.chunk","model":MODEL,
                "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
            format!("data: {chunk}\n\n")
        };
        let (head, tail) = reasoning.split_at(reasoning.len() / 2);
        let mut wire = chunk(json!({"role":"assistant","content":null,"reasoning_content":head}), Value::Null);
        wire += &chunk(json!({"content":null,"reasoning_content":tail}), Value::Null);
        if calls.is_null() {
            wire += &chunk(json!({"content":content,"reasoning_content":null}), Value::Null);
        } else {
            let mut call = calls[0].clone();
            call["index"] = json!(0);
            wire += &chunk(json!({"tool_calls":[call],"reasoning_content":null}), Value::Null);
        }
        wire += &chunk(json!({}), json!(finish));
        let last = json!({"id":id,"object":"chat.completion.chunk","model":MODEL,"choices":[],"usage":usage});
        wire += &format!("data: {last}\n\ndata: [DONE]\n\n");
        ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream")
    }
}

fn endpoint(protocol: &str, url: &str, key_env: &str, model: &str, extra: Value) -> Value {
    let mut declared = json!({ "tools": true, "streaming": true, "output_schema": true, "max_output": 8192 });
    declared.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    json!({ "protocol": protocol, "base_url": url, "key_env": key_env, "models": { model: declared } })
}

/// A gateway with the endpoints `ds` (DeepSeek, priced), `claude`
/// (Anthropic protocol) and `resp` (Responses), each on its own fake.
async fn gateway() -> (Gateway, MockServer, MockServer, MockServer) {
    let (ds, claude, resp) = (MockServer::start().await, MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(DeepSeek).mount(&ds).await;
    let said = json!({"id":"msg_1","type":"message","role":"assistant","model":"glm-4.6",
        "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}});
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(said))
        .mount(&claude)
        .await;
    let finished = json!({"id":"resp_1","object":"response","model":"gpt-5-mini","status":"completed",
        "output":[{"type":"message","id":"m1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"ok","annotations":[]}]}],
        "usage":{"input_tokens":10,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0}}});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(finished))
        .mount(&resp)
        .await;
    let endpoints = json!({
        "ds": endpoint("chat_completions", &ds.uri(), "DS_KEY", MODEL,
            json!({ "price": { "input": 0.5, "output": 2, "cache_read": 0.1 } })),
        "claude": endpoint("anthropic", &claude.uri(), "GLM_KEY", "glm-4.6", json!({})),
        "resp": endpoint("responses", &resp.uri(), "RESP_KEY", "gpt-5-mini", json!({})),
    });
    let vars: BTreeMap<&str, String> = [
        ("MOLT_ENDPOINTS", endpoints.to_string()),
        ("DS_KEY", "ds-secret".into()),
        ("GLM_KEY", "glm-secret".into()),
        ("RESP_KEY", "resp-secret".into()),
        ("MOLT_MODEL_RETRIES", "0".into()),
    ]
    .into_iter()
    .collect();
    let cfg = Config::from_vars(|name| vars.get(name).cloned()).unwrap();
    (Gateway::new(cfg).unwrap(), ds, claude, resp)
}

fn tools() -> Vec<Value> {
    vec![json!({"name":"read_file","description":"Read a file.",
        "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}})]
}

/// The bodies `server` got, in order.
async fn bodies(server: &MockServer) -> Vec<Value> {
    server.received_requests().await.unwrap().iter().map(|r| r.body_json().unwrap()).collect()
}

/// The `reasoning_content` of each assistant message of `body`, in order.
fn sent_back(body: &Value) -> Vec<Option<&str>> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m.get("reasoning_content").and_then(Value::as_str))
        .collect()
}

/// Two user turns of a tool round trip each, streamed or not: what the
/// endpoint got on each call.
async fn two_turns(stream: bool) -> (Vec<Value>, Vec<Value>, String) {
    let (gateway, ds, ..) = gateway().await;
    let mut previews = String::new();
    let mut messages = vec![user_text("What does a.rs say?")];
    let mut replies = Vec::new();
    for step in 0..4 {
        let req = CompleteRequest {
            model: Some(format!("ds/{MODEL}")),
            messages: messages.clone(),
            tools: tools(),
            stream: stream.then_some(StreamContext { attempt: Some(0), turn: step }),
            ..Default::default()
        };
        let reply = if stream {
            gateway.complete_streamed(req, None, |t| previews.push_str(&t)).await.unwrap()
        } else {
            gateway.complete(req).await.unwrap()
        };
        assert_eq!(reply.endpoint.as_deref(), Some("ds"));
        assert_eq!(reply.usage.reasoning_tokens, Some(25), "reasoning tokens are reported, in output_tokens");
        assert_eq!(
            (reply.usage.input_tokens, reply.usage.output_tokens, reply.usage.cache_read_input_tokens),
            (36, 40, 64)
        );
        // Priced as declared: 36 * 0.5 + 64 * 0.1 + 40 * 2 per million.
        assert!((reply.cost_usd.unwrap() - 0.0001044).abs() < 1e-12, "{:?}", reply.cost_usd);
        messages.push(reply.as_turn());
        match reply.tool_uses().first() {
            Some(call) => messages.push(user_blocks(vec![tool_result(&call.id, "fn main() { hi() }", false)])),
            None => messages.push(user_text("And b.rs?")),
        }
        replies.push(reply.as_turn());
    }
    (bodies(&ds).await, replies, previews)
}

async fn reasoning_round_trips_within_the_users_turn_and_is_left_out_after_it(stream: bool) {
    let (sent, replies, previews) = two_turns(stream).await;
    // The reasoning comes back first, as unsigned thinking that names its protocol and field.
    assert_eq!(
        replies[0]["content"][0],
        json!({"type":"thinking","thinking":"I should read a.rs first (1).","protocol":CHAT_REASONING,
            "field":"reasoning_content"})
    );
    assert_eq!(replies[0]["content"][1]["type"], "tool_use");
    assert_eq!(replies[1]["content"][0]["thinking"], "The file says hi (3).");
    assert_eq!(replies[1]["content"][1], json!({"type":"text","text":"It says hi (3)."}));
    assert!(replies.iter().flat_map(|r| r["content"].as_array().unwrap()).all(|b| b.get("signature").is_none()));
    if stream {
        assert_eq!(previews, "It says hi (3).It says hi (7).", "reasoning is never previewed");
    }
    assert_eq!(sent.len(), 4);
    assert_eq!(sent_back(&sent[0]), Vec::<Option<&str>>::new());
    // Within the first turn: the call's reasoning goes back verbatim with it.
    assert_eq!(sent_back(&sent[1]), [Some("I should read a.rs first (1).")]);
    assert_eq!(sent[1]["messages"][1]["tool_calls"][0]["id"], "call_1");
    // A new user message starts a new turn: the earlier turn's reasoning is left out.
    assert_eq!(sent_back(&sent[2]), [None, None]);
    assert_eq!(sent[2]["messages"][3], json!({"role":"assistant","content":"It says hi (3)."}));
    // And the new turn's own goes back through its tool calls.
    assert_eq!(sent_back(&sent[3]), [None, None, Some("I should read a.rs first (5).")]);
    for body in &sent {
        let text = body.to_string();
        assert!(!text.contains("\"thinking\"") && !text.contains(CHAT_REASONING), "{text}");
    }
}

#[tokio::test]
async fn deepseek_reasoning_round_trips_within_the_users_turn_and_is_left_out_after_it() {
    reasoning_round_trips_within_the_users_turn_and_is_left_out_after_it(false).await;
}

#[tokio::test]
async fn streamed_deepseek_reasoning_round_trips_within_the_users_turn_and_is_left_out_after_it() {
    reasoning_round_trips_within_the_users_turn_and_is_left_out_after_it(true).await;
}

#[tokio::test]
async fn chat_reasoning_never_reaches_another_protocol_and_signed_thinking_still_never_reaches_chat() {
    let (_, replies, _) = two_turns(false).await;
    let (gateway, ds, claude, resp) = gateway().await;
    // The DeepSeek conversation, carried on to the other protocols' endpoints mid-turn.
    let history = vec![
        user_text("What does a.rs say?"),
        replies[0].clone(),
        user_blocks(vec![tool_result("call_1", "fn main() { hi() }", false)]),
        // An assistant turn of reasoning alone is left out with it.
        json!({"role":"assistant","content":[{"type":"thinking","thinking":"hm","protocol":CHAT_REASONING}]}),
        user_text("Go on."),
    ];
    for (model, server) in [("claude/glm-4.6", &claude), ("resp/gpt-5-mini", &resp)] {
        let req = CompleteRequest {
            model: Some(model.into()),
            messages: history.clone(),
            tools: tools(),
            ..Default::default()
        };
        let reply = gateway.complete(req).await.unwrap();
        assert_eq!(reply.endpoint.as_deref(), Some(model.split('/').next().unwrap()));
        let body = bodies(server).await.pop().unwrap();
        let text = body.to_string();
        assert!(!text.contains("I should read") && !text.contains(CHAT_REASONING) && !text.contains("hm"), "{text}");
        if model.starts_with("claude") {
            assert_eq!(body["messages"].as_array().unwrap().len(), 4, "{body}");
            assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
            assert!(reply.usage.reasoning_tokens.is_none(), "the Messages API does not count them: unknown");
        } else {
            assert_eq!(reply.usage.reasoning_tokens, Some(0));
        }
    }
    // Signed Messages API thinking is still refused on Chat Completions, before anything is sent.
    let signed = json!({"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"sig"}]});
    let req = CompleteRequest {
        model: Some(format!("ds/{MODEL}")),
        messages: vec![user_text("hi"), signed, user_text("go on")],
        ..Default::default()
    };
    assert!(gateway.complete(req).await.is_err());
    assert!(ds.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_probe_of_a_reasoning_endpoint_records_the_endpoint_and_the_tokens_it_spent_reasoning() {
    let (gateway, ds, ..) = gateway().await;
    let plan = gateway.probe_plan(&format!("ds/{MODEL}")).unwrap();
    assert_eq!(plan.checks, [Check::Tools, Check::Streaming, Check::OutputSchema]);
    let report = gateway.probe(&plan, 1).await;
    assert_eq!((report.calls, report.stopped.as_deref()), (3, None));
    let found = &report.probed;
    assert_eq!((found.endpoint.as_deref(), found.base_url.as_str()), (Some("ds"), ds.uri().as_str()));
    assert_eq!((found.tools, found.streaming), (Some(true), Some(true)));
    assert_eq!(found.reasoning_tokens, Some(75), "25 in each of the three calls");
    assert!(found.cost_usd.is_some_and(|c| c > 0.0));
}
