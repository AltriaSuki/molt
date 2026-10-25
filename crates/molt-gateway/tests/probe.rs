//! `molt models probe` against a fake Messages API endpoint, and the gateway
//! shaping its calls by what the probe found. No real model API is called.

use std::time::Duration;

use molt_api::model::{user_text, CompleteRequest, Effort};
use molt_gateway::probe::{self, Check, Records};
use molt_gateway::{Config, Gateway};
use molt_proto::ErrorCode;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn config(server: &MockServer) -> Config {
    Config {
        providers: Default::default(),
        api_key: "test-key".into(),
        auth_token: None,
        base_url: server.uri(),
        default_model: "opus".into(),
        max_tokens: 32_000,
        effort: Some(Effort::High),
        timeout: Duration::from_secs(5),
        max_retries: 2,
        max_in_flight: 4,
        fallbacks: true,
        retry_base: Duration::from_millis(1),
        prices: Default::default(),
        probed: Default::default(),
    }
}

fn message(content: Value, stop: &str) -> Value {
    json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": "glm-4.6",
        "content": content, "stop_reason": stop,
        "usage": { "input_tokens": 12, "output_tokens": 6 }
    })
}

fn stream() -> String {
    [
        json!({"type":"message_start","message":{"id":"msg","model":"glm-4.6","role":"assistant",
            "type":"message","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
    .collect()
}

/// An Anthropic-compatible endpoint, as some third-party ones are: tools and
/// streaming work, `output_config.format` is ignored (it answers in prose),
/// and `output_config.effort` is refused with a 400.
fn third_party(req: &Request) -> ResponseTemplate {
    let body: Value = req.body_json().unwrap();
    if body.pointer("/output_config/effort").is_some() {
        return ResponseTemplate::new(400).set_body_json(
            json!({"type":"error","error":{"type":"invalid_request_error","message":"unknown field effort"}}),
        );
    }
    if body["stream"] == true {
        return ResponseTemplate::new(200).set_body_raw(stream(), "text/event-stream");
    }
    if let Some(tool) = body["tools"].as_array().and_then(|t| t.first()) {
        let input = if tool["name"] == probe::SCHEMA_TOOL { json!({ "answer": 5 }) } else { json!({ "n": 7 }) };
        let call = json!([{ "type": "tool_use", "id": "toolu_1", "name": tool["name"], "input": input }]);
        return ResponseTemplate::new(200).set_body_json(message(call, "tool_use"));
    }
    ResponseTemplate::new(200).set_body_json(message(json!([{ "type": "text", "text": "2 + 3 is 5." }]), "end_turn"))
}

async fn sent(server: &MockServer) -> Vec<Value> {
    server.received_requests().await.unwrap().iter().map(|r| r.body_json().unwrap()).collect()
}

#[tokio::test]
async fn a_probe_records_an_ignored_output_schema_and_the_gateway_then_asks_through_a_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(third_party).mount(&server).await;

    let prober = Gateway::new(probe::probing(config(&server))).unwrap();
    let plan = prober.probe_plan("glm-4.6").unwrap();
    assert_eq!(plan.model, "glm-4.6");
    assert_eq!(plan.base_url, server.uri());
    assert_eq!(plan.checks, [Check::Tools, Check::Streaming, Check::OutputSchema, Check::Effort(Effort::Low)]);
    // Nothing is sent to plan a probe.
    assert!(sent(&server).await.is_empty());

    let report = prober.probe(&plan, 1_000).await;
    let p = &report.probed;
    assert_eq!(
        (p.tools, p.streaming, p.output_schema, p.effort),
        (Some(true), Some(true), Some(false), Some(false)),
        "{:?}",
        report.findings
    );
    assert_eq!((report.calls, report.stopped.as_deref()), (4, None));
    // The model has no price: the probe's cost is unknown, never zero.
    assert_eq!(p.cost_usd, None);
    let bodies = sent(&server).await;
    assert_eq!(bodies.len(), 4, "one call a check, with no retries");
    for body in &bodies {
        assert_eq!(body["max_tokens"], probe::MAX_TOKENS);
    }
    // Only the effort check sends an effort, whatever the configured one.
    assert_eq!(bodies.iter().filter(|b| b.pointer("/output_config/effort").is_some()).count(), 1);

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("molt/models.json");
    let mut records = Records::default();
    records.put(report.probed.clone());
    records.write(&file).unwrap();

    // A gateway started after the probe reads the record and goes by it.
    server.reset().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(third_party).mount(&server).await;
    let mut cfg = config(&server);
    cfg.effort = None;
    cfg.probed =
        Records::from_vars(|name| (name == probe::FILE_VAR).then(|| file.display().to_string())).unwrap().into();
    let gateway = Gateway::new(cfg).unwrap();
    let schema = json!({ "type": "object", "properties": { "answer": { "type": "integer" } }, "required": ["answer"] });
    let reply = gateway
        .complete(CompleteRequest {
            model: Some("glm-4.6".into()),
            messages: vec![user_text("What is 2 + 3?")],
            output_schema: Some(schema.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(reply.content, [json!({ "type": "text", "text": "{\"answer\":5}" })]);
    assert_eq!(reply.stop_reason.as_deref(), Some("end_turn"));
    let body = &sent(&server).await[0];
    assert_eq!(body["tools"][0]["input_schema"], schema);
    assert_eq!(body["tool_choice"]["name"], probe::SCHEMA_TOOL);
    assert!(body.get("output_config").is_none(), "{body}");

    // The effort the endpoint refused is refused before HTTP now.
    let error = gateway
        .complete(CompleteRequest {
            model: Some("glm-4.6".into()),
            messages: vec![user_text("hi")],
            effort: Some(Effort::Low),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Invalid);
    assert!(error.message.contains("molt models probe"), "{}", error.message);
    assert_eq!(sent(&server).await.len(), 1);

    // The record is for that endpoint only: elsewhere the schema goes as output_config again.
    let other = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(third_party).mount(&other).await;
    let mut cfg = config(&other);
    cfg.effort = None;
    cfg.probed = records.into();
    let elsewhere = Gateway::new(cfg).unwrap();
    let reply = elsewhere
        .complete(CompleteRequest {
            model: Some("glm-4.6".into()),
            messages: vec![user_text("What is 2 + 3?")],
            output_schema: Some(schema),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(reply.content[0]["text"], "2 + 3 is 5.");
    assert!(sent(&other).await[0].pointer("/output_config/format").is_some());
}

#[tokio::test]
async fn a_probe_that_finds_no_stream_makes_later_calls_unstreamed_and_stops_at_a_refused_key() {
    let server = MockServer::start().await;
    // Streams are answered with whole JSON bodies; everything else works.
    let plain = |req: &Request| {
        let body: Value = req.body_json().unwrap();
        let content = match body["tools"].as_array().and_then(|t| t.first()) {
            Some(tool) => json!([{ "type": "tool_use", "id": "t", "name": tool["name"], "input": { "n": 7 } }]),
            None => json!([{ "type": "text", "text": "{\"answer\":5}" }]),
        };
        ResponseTemplate::new(200).set_body_json(message(content, "end_turn"))
    };
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(plain).mount(&server).await;
    let prober = Gateway::new(probe::probing(config(&server))).unwrap();
    let report = prober.probe(&prober.probe_plan("glm-4.6").unwrap(), 1).await;
    let p = &report.probed;
    assert_eq!((p.tools, p.streaming, p.output_schema, p.effort), (Some(true), Some(false), Some(true), Some(true)));

    let mut cfg = config(&server);
    cfg.effort = None;
    let mut records = Records::default();
    records.put(report.probed.clone());
    cfg.probed = records.into();
    let gateway = Gateway::new(cfg).unwrap();
    let mut previews = 0;
    let reply = gateway
        .complete_streamed(
            CompleteRequest {
                model: Some("glm-4.6".into()),
                messages: vec![user_text("hi")],
                stream: Some(molt_api::model::StreamContext { attempt: Some(0), turn: 1 }),
                ..Default::default()
            },
            None,
            |_| previews += 1,
        )
        .await
        .unwrap();
    assert_eq!((reply.content[0]["text"].as_str(), previews), (Some("{\"answer\":5}"), 0));
    assert!(sent(&server).await.last().unwrap().get("stream").is_none());

    // A refused key ends the probe at its first call: the others would fail the same way.
    let refusing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}),
        ))
        .mount(&refusing)
        .await;
    let prober = Gateway::new(probe::probing(config(&refusing))).unwrap();
    let report = prober.probe(&prober.probe_plan("glm-4.6").unwrap(), 1).await;
    assert_eq!(report.calls, 1);
    assert!(report.stopped.is_some());
    let p = &report.probed;
    assert_eq!((p.tools, p.streaming, p.output_schema, p.effort), (None, None, None, None));
    // Refused by status, so not billed.
    assert_eq!(p.cost_usd, Some(0.0));
    assert_eq!(refusing.received_requests().await.unwrap().len(), 1);
}

#[test]
fn haiku_is_not_asked_for_an_effort_it_takes_none_of() {
    let cfg = Config { base_url: "http://127.0.0.1:1".into(), ..config_without_server() };
    let prober = Gateway::new(probe::probing(cfg)).unwrap();
    let plan = prober.probe_plan("haiku").unwrap();
    assert_eq!(plan.model, "claude-haiku-4-5");
    assert_eq!(plan.checks, [Check::Tools, Check::Streaming, Check::OutputSchema]);
    assert!(plan.skipped[0].starts_with("effort"), "{:?}", plan.skipped);
    // An OpenAI model with no credential cannot be probed.
    assert!(prober.probe_plan("openai/gpt-4.1").is_err());
}

fn config_without_server() -> Config {
    Config {
        providers: Default::default(),
        api_key: "test-key".into(),
        auth_token: None,
        base_url: String::new(),
        default_model: "opus".into(),
        max_tokens: 32_000,
        effort: None,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        max_in_flight: 1,
        fallbacks: true,
        retry_base: Duration::from_millis(1),
        prices: Default::default(),
        probed: Default::default(),
    }
}
