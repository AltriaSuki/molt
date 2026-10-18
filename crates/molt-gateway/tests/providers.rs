//! Provider contracts through actual HTTP: the planner's wire shape stays the
//! same, while the gateway owns conversion, capability checks and settlement.

use molt_api::model::{tool_result, user_blocks, user_text, CompleteRequest, Effort, StreamContext};
use molt_gateway::prices::PriceTable;
use molt_gateway::provider::{Capabilities, Provider, Providers};
use molt_gateway::{Config, Gateway};
use molt_proto::ErrorCode;
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(server: &MockServer) -> Config {
    Config {
        api_key: "anthropic-key".into(),
        auth_token: None,
        base_url: server.uri(),
        providers: Providers {
            openai_key: Some("openai-key".into()),
            openai_base_url: server.uri(),
            ..Providers::default()
        },
        default_model: "openai/gpt-4.1".into(),
        max_tokens: 32_000,
        effort: None,
        timeout: Duration::from_secs(3),
        max_retries: 2,
        max_in_flight: 2,
        fallbacks: true,
        retry_base: Duration::from_millis(1),
        prices: PriceTable::from_vars(|name| (name == "MOLT_MODEL_PRICES").then(|| "gpt-4.1=2,8,2,0.5".into()))
            .unwrap()
            .into(),
    }
}
fn request() -> CompleteRequest {
    CompleteRequest { messages: vec![user_text("hello")], ..Default::default() }
}
fn reply() -> Value {
    json!({"id":"chatcmpl-1","model":"gpt-4.1","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}})
}
async fn respond(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}
#[tokio::test]
async fn an_openai_tool_round_trip_preserves_ids_errors_and_usage_without_anthropic_headers() {
    let server = MockServer::start().await;
    let mut value = reply();
    value["choices"][0]["message"] = json!({"role":"assistant","content":null,"tool_calls":[{"id":"call-a","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}]});
    value["choices"][0]["finish_reason"] = json!("tool_calls");
    respond(&server, value).await;
    let gateway = Gateway::new(config(&server)).unwrap();
    let tools = vec![
        json!({"name":"read_file","description":"Read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}),
    ];
    let first = gateway
        .complete(CompleteRequest { tools: tools.clone(), system: Some("system".into()), ..request() })
        .await
        .unwrap();
    assert_eq!(first.tool_uses()[0].id, "call-a");
    assert_eq!(first.tool_uses()[0].input, json!({"path":"a.rs"}));
    assert_eq!(first.usage.input_tokens, 6);
    assert_eq!(first.usage.cache_read_input_tokens, 4);
    assert!((first.cost_usd.unwrap() - 0.000054).abs() < 1e-10);
    assert_eq!(first.stop_details.as_ref().unwrap()["provider"], "openai");
    gateway
        .complete(CompleteRequest {
            tools,
            messages: vec![
                user_text("hello"),
                first.as_turn(),
                user_blocks(vec![tool_result("call-a", "not found", true)]),
            ],
            ..Default::default()
        })
        .await
        .unwrap();
    let sent = server.received_requests().await.unwrap();
    for req in &sent {
        assert_eq!(req.headers["authorization"].to_str().unwrap(), "Bearer openai-key");
        assert!(req.headers.get("x-api-key").is_none());
        assert!(req.headers.get("anthropic-version").is_none());
        assert!(req.headers.get("anthropic-beta").is_none());
    }
    let body: Value = sent[0].body_json().unwrap();
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    assert_eq!(body["max_completion_tokens"], 32_000);
    assert!(body.get("cache_control").is_none());
    let second: Value = sent[1].body_json().unwrap();
    assert_eq!(second["messages"][1]["tool_calls"][0]["id"], "call-a");
    assert_eq!(
        second["messages"][2],
        json!({"role":"tool","tool_call_id":"call-a","content":"Tool error:\nnot found"})
    );
}
#[tokio::test]
async fn unsupported_capabilities_and_native_history_fail_before_http() {
    let server = MockServer::start().await;
    let mut cfg = config(&server);
    cfg.providers.openai_models.insert(
        "text-only".into(),
        Capabilities { tools: false, streaming: false, output_schema: false, efforts: vec![], max_output: 100 },
    );
    let gateway = Gateway::new(cfg).unwrap();
    let cases = vec![
        CompleteRequest { effort: Some(Effort::High), ..request() },
        CompleteRequest { model: Some("openai/unknown".into()), ..request() },
        CompleteRequest {
            model: Some("openai/text-only".into()),
            tools: vec![json!({"name":"x","input_schema":{}})],
            ..request()
        },
        CompleteRequest {
            model: Some("openai/text-only".into()),
            stream: Some(StreamContext { attempt: None, turn: 1 }),
            ..request()
        },
        CompleteRequest { model: Some("openai/text-only".into()), output_schema: Some(json!({})), ..request() },
        CompleteRequest {
            messages: vec![
                user_text("hi"),
                json!({"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"signed"}]}),
            ],
            ..request()
        },
        CompleteRequest { messages: vec![user_blocks(vec![tool_result("missing", "x", false)])], ..request() },
    ];
    for req in cases {
        assert_eq!(gateway.complete(req).await.unwrap_err().code, ErrorCode::Invalid);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn aliases_and_explicit_providers_share_one_gateway_and_keep_keys_separate() {
    let server = MockServer::start().await;
    respond(&server, reply()).await;
    Mock::given(path("/v1/messages")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"msg","model":"claude-haiku-4-5","content":[{"type":"text","text":"anthropic"}],"usage":{"input_tokens":1,"output_tokens":1}}))).mount(&server).await;
    let mut cfg = config(&server);
    cfg.providers.aliases.insert("fast".into(), "openai/gpt-4.1".into());
    let gateway = Gateway::new(cfg).unwrap();
    gateway.complete(CompleteRequest { model: Some("fast".into()), ..request() }).await.unwrap();
    gateway.complete(CompleteRequest { model: Some("anthropic/haiku".into()), ..request() }).await.unwrap();
    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent[0].url.path(), "/v1/chat/completions");
    assert_eq!(sent[1].url.path(), "/v1/messages");
    assert_eq!(sent[1].headers["x-api-key"], "anthropic-key");
    assert!(sent[1].headers.get("authorization").is_none());
}
#[tokio::test]
async fn an_unknown_returned_model_or_missing_usage_never_becomes_zero_cost() {
    let server = MockServer::start().await;
    let mut value = reply();
    value["model"] = json!("unknown-answer-model");
    respond(&server, value).await;
    let gateway = Gateway::new(config(&server)).unwrap();
    assert_eq!(gateway.complete(request()).await.unwrap().cost_usd, None);
    server.reset().await;
    let mut value = reply();
    value.as_object_mut().unwrap().remove("usage");
    respond(&server, value).await;
    let error = gateway.complete(request()).await.unwrap_err();
    assert!(error.message.contains("usage and cost are unknown"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn rate_limits_retry_but_caller_deadlines_and_bad_answers_do_not() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_json(json!({"error":{"type":"rate_limit_error","message":"later"}})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    respond(&server, reply()).await;
    let gateway = Gateway::new(config(&server)).unwrap();
    gateway.complete(request()).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    server.reset().await;
    Mock::given(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_json(reply()))
        .mount(&server)
        .await;
    assert_eq!(
        gateway.complete_within(request(), Some(Duration::from_millis(100))).await.unwrap_err().code,
        ErrorCode::Timeout
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.reset().await;
    let mut value = reply();
    value["choices"][0]["finish_reason"] = json!("unknown");
    respond(&server, value).await;
    assert_eq!(gateway.complete(request()).await.unwrap_err().code, ErrorCode::Failed);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
fn chunk(delta: Value, reason: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"chatcmpl-stream","model":"gpt-4.1","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    )
}
fn usage_chunk() -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"chatcmpl-stream","model":"gpt-4.1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}})
    )
}
#[tokio::test]
async fn streaming_reassembles_tool_json_and_requires_done_and_usage_without_retrying_partials() {
    let server = MockServer::start().await;
    let stream = format!(
        "{}{}{}{}{}data: [DONE]\n\n",
        chunk(json!({"role":"assistant","content":"你好"}), Value::Null),
        chunk(
            json!({"tool_calls":[{"index":0,"id":"call-s","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]}),
            Value::Null
        ),
        chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"\"x.rs\"}"}}]}), Value::Null),
        chunk(json!({}), json!("tool_calls")),
        usage_chunk()
    );
    Mock::given(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream.as_str(), "text/event-stream"))
        .mount(&server)
        .await;
    let gateway = Gateway::new(config(&server)).unwrap();
    let req = CompleteRequest { stream: Some(StreamContext { attempt: Some(0), turn: 1 }), ..request() };
    let mut previews = String::new();
    let response = gateway.complete_streamed(req.clone(), None, |s| previews.push_str(&s)).await.unwrap();
    assert_eq!(previews, "你好");
    assert_eq!(response.tool_uses()[0].input, json!({"path":"x.rs"}));
    let body: Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(body["stream_options"]["include_usage"], true);
    for partial in [stream.replace("data: [DONE]\n\n", ""), stream.replace(&usage_chunk(), "")] {
        server.reset().await;
        Mock::given(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(partial, "text/event-stream"))
            .mount(&server)
            .await;
        assert_eq!(gateway.complete_streamed(req.clone(), None, |_| {}).await.unwrap_err().code, ErrorCode::Failed);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
#[tokio::test]
async fn refusal_content_is_never_a_tool_or_a_final_answer() {
    let server = MockServer::start().await;
    let mut value = reply();
    value["choices"][0]["message"]["refusal"] = json!("declined");
    respond(&server, value).await;
    let response = Gateway::new(config(&server)).unwrap().complete(request()).await.unwrap();
    assert!(response.is_refusal());
    assert!(response.content.is_empty());
}
#[test]
fn debug_redacts_both_keys_and_openai_needs_https_or_loopback() {
    let cfg = Config {
        api_key: String::new(),
        auth_token: None,
        providers: Providers {
            openai_key: Some("private-key".into()),
            default: Provider::Openai,
            ..Providers::default()
        },
        ..config_for_test()
    };
    assert!(!format!("{cfg:?}").contains("private-key"));
    for url in ["http://example.com", "https://user:secret@example.com", "https://example.com?key=secret"] {
        let mut cfg = cfg.clone();
        cfg.providers.openai_base_url = url.into();
        assert!(Gateway::new(cfg).is_err());
    }
}
fn config_for_test() -> Config {
    Config {
        api_key: "unused".into(),
        auth_token: None,
        providers: Providers::default(),
        base_url: "https://api.anthropic.com".into(),
        default_model: "opus".into(),
        max_tokens: 1000,
        effort: None,
        timeout: Duration::from_secs(1),
        max_retries: 0,
        max_in_flight: 1,
        fallbacks: false,
        retry_base: Duration::from_millis(1),
        prices: Default::default(),
    }
}
