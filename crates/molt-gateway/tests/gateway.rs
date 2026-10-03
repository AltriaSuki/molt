//! The gateway against a local mock of the Messages API.

use std::time::{Duration, Instant};

use molt_api::model::{user_text, CompleteRequest, Effort, Usage};
use molt_gateway::{Config, Gateway};
use molt_proto::ErrorCode;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn config(server: &MockServer) -> Config {
    Config {
        api_key: "test-key".into(),
        base_url: server.uri(),
        default_model: "opus".into(),
        max_tokens: 32_000,
        effort: None,
        timeout: Duration::from_secs(5),
        max_retries: 2,
        max_in_flight: 4,
        fallbacks: true,
        retry_base: Duration::from_millis(2),
    }
}

fn request() -> CompleteRequest {
    CompleteRequest { messages: vec![user_text("hello")], ..Default::default() }
}

fn message(model: &str) -> Value {
    json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{ "type": "text", "text": "hi" }],
        "stop_reason": "end_turn",
        "stop_details": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    })
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn api_error(kind: &str, message: &str) -> Value {
    json!({ "type": "error", "error": { "type": kind, "message": message } })
}

async fn answer_with(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(response).mount(server).await;
}

async fn answer_once_with(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(response).up_to_n_times(1).mount(server).await;
}

async fn sent(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).map(|v| v.to_str().unwrap())
}

fn body(req: &Request) -> Value {
    req.body_json().unwrap()
}

#[tokio::test]
async fn opus_requests_carry_thinking_effort_fallbacks_and_cache_breakpoints() {
    let server = MockServer::start().await;
    answer_with(&server, ok(message("claude-opus-5-5"))).await;
    let gw = Gateway::new(config(&server)).unwrap();
    let tools = vec![json!({
        "name": "read_file",
        "description": "Read a file.",
        "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] },
        "strict": true
    })];
    let req = CompleteRequest { system: Some("Be brief.".into()), tools: tools.clone(), ..request() };
    gw.complete(req.clone()).await.unwrap();
    gw.complete(CompleteRequest { effort: Some(Effort::Xhigh), ..req.clone() }).await.unwrap();

    let sent = sent(&server).await;
    assert_eq!(sent.len(), 2);
    let first = &sent[0];
    assert_eq!(header(first, "x-api-key"), Some("test-key"));
    assert_eq!(header(first, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(header(first, "content-type"), Some("application/json"));
    assert_eq!(header(first, "anthropic-beta"), Some("server-side-fallback-2026-07-01"));
    assert_eq!(first.headers.get_all("anthropic-beta").iter().count(), 1);
    assert_eq!(
        body(first),
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 32000,
            "cache_control": { "type": "ephemeral" },
            "system": [{ "type": "text", "text": "Be brief.", "cache_control": { "type": "ephemeral" } }],
            "messages": req.messages,
            "tools": tools,
            "thinking": { "type": "adaptive" },
            "output_config": { "effort": "medium" },
            "fallbacks": "default"
        })
    );
    let second = body(&sent[1]);
    assert_eq!(second["output_config"], json!({ "effort": "xhigh" }));
    for field in ["tool_choice", "temperature", "top_p", "top_k"] {
        assert!(second.get(field).is_none(), "{field} was sent");
    }
}

#[tokio::test]
async fn haiku_requests_get_no_thinking_effort_or_fallbacks_and_a_clamped_max_tokens() {
    let server = MockServer::start().await;
    answer_with(&server, ok(message("claude-haiku-4-5"))).await;
    let gw = Gateway::new(Config { effort: Some(Effort::High), ..config(&server) }).unwrap();
    let req = CompleteRequest {
        model: Some("haiku".into()),
        max_tokens: Some(100_000),
        effort: Some(Effort::Low),
        ..request()
    };
    gw.complete(req).await.unwrap();

    let sent = sent(&server).await;
    let body = body(&sent[0]);
    assert_eq!(body["model"], "claude-haiku-4-5");
    assert_eq!(body["max_tokens"], 64_000);
    for field in ["thinking", "output_config", "fallbacks", "system", "tools"] {
        assert!(body.get(field).is_none(), "{field} was sent: {body}");
    }
    assert_eq!(header(&sent[0], "anthropic-beta"), None);
}

#[tokio::test]
async fn fallbacks_can_be_turned_off_and_max_tokens_is_capped_on_opus() {
    let server = MockServer::start().await;
    answer_with(&server, ok(message("claude-opus-5-5"))).await;
    let gw = Gateway::new(Config { fallbacks: false, max_tokens: 500_000, ..config(&server) }).unwrap();
    gw.complete(request()).await.unwrap();

    let sent = sent(&server).await;
    let body = body(&sent[0]);
    assert!(body.get("fallbacks").is_none());
    assert_eq!(header(&sent[0], "anthropic-beta"), None);
    assert_eq!(body["max_tokens"], 128_000);
    assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
}

#[tokio::test]
async fn aliases_resolve_and_unknown_models_pass_through_without_a_price() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|req: &Request| {
            let model = req.body_json::<Value>().unwrap()["model"].clone();
            ok(message(model.as_str().unwrap()))
        })
        .mount(&server)
        .await;
    let gw = Gateway::new(Config { default_model: "sonnet".into(), ..config(&server) }).unwrap();

    let resp = gw.complete(request()).await.unwrap();
    assert_eq!(resp.model, "claude-sonnet-5-5");
    assert!(resp.cost_usd.is_some());

    let resp = gw.complete(CompleteRequest { model: Some("claude-next-1".into()), ..request() }).await.unwrap();
    assert_eq!(resp.model, "claude-next-1");
    assert_eq!(resp.cost_usd, None);

    let sent = sent(&server).await;
    assert_eq!(body(&sent[0])["model"], "claude-sonnet-5-5");
    assert_eq!(body(&sent[0])["output_config"], json!({ "effort": "high" }));
    let unknown = body(&sent[1]);
    assert_eq!(unknown["model"], "claude-next-1");
    assert_eq!(unknown["max_tokens"], 32_000);
    for field in ["thinking", "output_config", "fallbacks"] {
        assert!(unknown.get(field).is_none(), "{field} was sent");
    }
    assert_eq!(header(&sent[1], "anthropic-beta"), None);
}

#[tokio::test]
async fn an_unknown_model_sends_effort_only_when_asked() {
    let server = MockServer::start().await;
    answer_with(&server, ok(message("claude-next-1"))).await;
    let gw = Gateway::new(config(&server)).unwrap();
    let req = CompleteRequest { model: Some("claude-next-1".into()), effort: Some(Effort::Low), ..request() };
    gw.complete(req).await.unwrap();
    assert_eq!(body(&sent(&server).await[0])["output_config"], json!({ "effort": "low" }));
}

#[tokio::test]
async fn overloaded_then_success_is_retried() {
    let server = MockServer::start().await;
    let overloaded = ResponseTemplate::new(529).set_body_json(api_error("overloaded_error", "Overloaded"));
    answer_once_with(&server, overloaded).await;
    answer_with(&server, ok(message("claude-opus-5-5"))).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let resp = gw.complete(request()).await.unwrap();
    assert_eq!(resp.id, "msg_01");
    let sent = sent(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].body, sent[1].body, "a retry resends the same request");
}

#[tokio::test]
async fn retry_after_is_honoured() {
    let server = MockServer::start().await;
    let limited = ResponseTemplate::new(429)
        .insert_header("retry-after", "1")
        .set_body_json(api_error("rate_limit_error", "slow down"));
    answer_once_with(&server, limited).await;
    let overloaded = ResponseTemplate::new(529).insert_header("retry-after", "0");
    answer_once_with(&server, overloaded).await;
    answer_with(&server, ok(message("claude-opus-5-5"))).await;
    // Without the header the backoff would be 20 s or more, so a quick finish shows `retry-after: 0` was used.
    let gw = Gateway::new(Config { retry_base: Duration::from_secs(20), ..config(&server) }).unwrap();

    let start = Instant::now();
    let resp = tokio::time::timeout(Duration::from_secs(5), gw.complete(request())).await.unwrap().unwrap();
    let took = start.elapsed();
    assert_eq!(resp.id, "msg_01");
    assert!(took >= Duration::from_secs(1), "retry-after: 1 was not waited for ({took:?})");
    assert_eq!(sent(&server).await.len(), 3);
}

#[tokio::test]
async fn overload_that_persists_is_busy() {
    let server = MockServer::start().await;
    answer_with(&server, ResponseTemplate::new(529).set_body_json(api_error("overloaded_error", "Overloaded"))).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let err = gw.complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Busy);
    assert!(err.message.contains("overloaded_error: Overloaded"), "{}", err.message);
    assert_eq!(sent(&server).await.len(), 3, "one attempt and two retries");
}

#[tokio::test]
async fn rate_limits_that_persist_are_busy() {
    let server = MockServer::start().await;
    answer_with(&server, ResponseTemplate::new(429).set_body_json(api_error("rate_limit_error", "slow down"))).await;
    let gw = Gateway::new(Config { max_retries: 1, ..config(&server) }).unwrap();
    assert_eq!(gw.complete(request()).await.unwrap_err().code, ErrorCode::Busy);
    assert_eq!(sent(&server).await.len(), 2);
}

#[tokio::test]
async fn server_errors_that_persist_are_unavailable() {
    let server = MockServer::start().await;
    answer_with(
        &server,
        ResponseTemplate::new(500)
            .insert_header("request-id", "req_123")
            .set_body_json(api_error("api_error", "Internal server error")),
    )
    .await;
    let gw = Gateway::new(config(&server)).unwrap();

    let err = gw.complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.message.contains("api_error: Internal server error"), "{}", err.message);
    assert!(err.message.contains("req_123"), "{}", err.message);
    assert_eq!(sent(&server).await.len(), 3);
}

#[tokio::test]
async fn bad_requests_are_invalid_and_not_retried() {
    let server = MockServer::start().await;
    let msg = "max_tokens: 999999 > 128000, which is the maximum allowed";
    answer_with(&server, ResponseTemplate::new(400).set_body_json(api_error("invalid_request_error", msg))).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let err = gw.complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(err.message.contains("invalid_request_error"), "{}", err.message);
    assert!(err.message.contains(msg), "{}", err.message);
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn a_rejected_key_is_denied() {
    let server = MockServer::start().await;
    let unauthorized = ResponseTemplate::new(401).set_body_json(api_error("authentication_error", "invalid x-api-key"));
    answer_with(&server, unauthorized).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let err = gw.complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Denied);
    assert!(err.message.contains("authentication_error: invalid x-api-key"), "{}", err.message);
    assert!(!err.message.contains("test-key"));
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn statuses_map_to_codes_and_only_transient_ones_are_retried() {
    for (status, code, attempts) in [
        (408, ErrorCode::Timeout, 3),
        (409, ErrorCode::Busy, 3),
        (403, ErrorCode::Denied, 1),
        (404, ErrorCode::Invalid, 1),
        (413, ErrorCode::Invalid, 1),
        (422, ErrorCode::Invalid, 1),
    ] {
        let server = MockServer::start().await;
        answer_with(&server, ResponseTemplate::new(status).set_body_json(api_error("some_error", "no"))).await;
        let gw = Gateway::new(config(&server)).unwrap();
        let err = gw.complete(request()).await.unwrap_err();
        assert_eq!(err.code, code, "{status}: {}", err.message);
        assert_eq!(sent(&server).await.len(), attempts, "{status}");
    }
}

#[tokio::test]
async fn redirects_are_not_followed_and_the_key_stays_put() {
    let elsewhere = MockServer::start().await;
    answer_with(&elsewhere, ok(message("claude-opus-5-5"))).await;
    let server = MockServer::start().await;
    let to = format!("{}/v1/messages", elsewhere.uri());
    answer_with(&server, ResponseTemplate::new(307).insert_header("location", to.as_str())).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let err = gw.complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Failed, "{}", err.message);
    assert!(err.message.contains("307") && err.message.contains(&to), "{}", err.message);
    assert_eq!(sent(&server).await.len(), 1, "a redirect is not retried");
    assert!(sent(&elsewhere).await.is_empty(), "the redirect was followed");
}

#[tokio::test]
async fn a_deadline_cuts_an_attempt_short_and_stops_retries() {
    let server = MockServer::start().await;
    answer_with(&server, ok(message("claude-opus-5-5")).set_delay(Duration::from_secs(3))).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let start = Instant::now();
    let call = gw.complete_within(request(), Some(Duration::from_millis(300)));
    let err = tokio::time::timeout(Duration::from_secs(2), call).await.unwrap().unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout);
    assert!(err.message.contains("deadline"), "{}", err.message);
    assert!(start.elapsed() >= Duration::from_millis(300), "{:?}", start.elapsed());
    assert_eq!(sent(&server).await.len(), 1, "no retry once the time is up");

    let err = gw.complete_within(request(), Some(Duration::ZERO)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout);
    assert_eq!(sent(&server).await.len(), 1, "nothing is sent after the deadline");
}

#[tokio::test]
async fn a_retry_that_cannot_finish_before_the_deadline_is_not_started() {
    let server = MockServer::start().await;
    let overloaded = ResponseTemplate::new(529).insert_header("retry-after", "1");
    answer_with(&server, overloaded.set_body_json(api_error("overloaded_error", "Overloaded"))).await;
    let gw = Gateway::new(config(&server)).unwrap();

    // The 1 s wait the API asks for leaves too little of 1.5 s for another attempt.
    let start = Instant::now();
    let err = gw.complete_within(request(), Some(Duration::from_millis(1500))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Busy, "the last failure comes back: {}", err.message);
    assert!(start.elapsed() < Duration::from_secs(1), "slept for a retry: {:?}", start.elapsed());
    assert_eq!(sent(&server).await.len(), 1);

    // The same goes for the gateway's own backoff.
    let server = MockServer::start().await;
    answer_with(&server, ResponseTemplate::new(500)).await;
    let gw = Gateway::new(Config { retry_base: Duration::from_secs(1), ..config(&server) }).unwrap();
    let start = Instant::now();
    let err = gw.complete_within(request(), Some(Duration::from_millis(1500))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(start.elapsed() < Duration::from_millis(750), "slept for a retry: {:?}", start.elapsed());
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn a_retry_that_fits_before_the_deadline_is_made() {
    let server = MockServer::start().await;
    answer_once_with(&server, ResponseTemplate::new(529).insert_header("retry-after", "0")).await;
    answer_with(&server, ok(message("claude-opus-5-5"))).await;
    let gw = Gateway::new(config(&server)).unwrap();
    let resp = gw.complete_within(request(), Some(Duration::from_secs(5))).await.unwrap();
    assert_eq!(resp.id, "msg_01");
    assert_eq!(sent(&server).await.len(), 2);
}

#[tokio::test]
async fn an_unreadable_success_body_is_failed() {
    let server = MockServer::start().await;
    answer_with(&server, ResponseTemplate::new(200).set_body_string("<html>gateway</html>")).await;
    let gw = Gateway::new(config(&server)).unwrap();
    assert_eq!(gw.complete(request()).await.unwrap_err().code, ErrorCode::Failed);
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn a_slow_answer_times_out() {
    let server = MockServer::start().await;
    let slow = ok(message("claude-opus-5-5")).set_delay(Duration::from_secs(2));
    answer_with(&server, slow).await;
    let gw = Gateway::new(Config { timeout: Duration::from_millis(100), max_retries: 1, ..config(&server) }).unwrap();
    let err = gw.complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout);
    assert_eq!(sent(&server).await.len(), 2, "a timeout is retried");
}

#[tokio::test]
async fn an_unreachable_api_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let cfg = Config {
        api_key: "test-key".into(),
        base_url: format!("http://{addr}"),
        default_model: "opus".into(),
        max_tokens: 1000,
        effort: None,
        timeout: Duration::from_secs(2),
        max_retries: 1,
        max_in_flight: 1,
        fallbacks: true,
        retry_base: Duration::from_millis(1),
    };
    let err = Gateway::new(cfg).unwrap().complete(request()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable, "{}", err.message);
}

#[tokio::test]
async fn a_refusal_is_returned_as_is() {
    let server = MockServer::start().await;
    let refusal = json!({
        "id": "msg_02",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": { "type": "refusal", "category": "cyber", "explanation": null },
        "usage": { "input_tokens": 50, "output_tokens": 0 }
    });
    answer_with(&server, ok(refusal)).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let resp = gw.complete(request()).await.unwrap();
    assert!(resp.is_refusal());
    assert_eq!(resp.stop_reason.as_deref(), Some("refusal"));
    assert_eq!(resp.stop_details.unwrap()["category"], "cyber");
    assert!(resp.content.is_empty());
    assert_eq!(resp.usage.input_tokens, 50);
}

#[tokio::test]
async fn content_comes_back_exactly_as_the_api_sent_it() {
    let server = MockServer::start().await;
    let content = json!([
        { "type": "thinking", "thinking": "", "signature": "EqQBCkYIBxgCKkCx/9+abc==" },
        {
            "type": "fallback",
            "from_model": "claude-opus-5-5",
            "to_model": "claude-opus-5",
            "reason": { "category": "cyber" }
        },
        { "type": "text", "text": "Reading it.", "citations": null },
        {
            "type": "tool_use",
            "id": "toolu_01",
            "name": "read_file",
            "input": { "path": "a.rs", "n": 1.5, "deep": [null, true, { "x": [] }] }
        },
        { "type": "something_new", "payload": { "unicode": "caf\u{e9} \u{1f980}", "big": 12345678901234567u64 } }
    ]);
    let mut reply = message("claude-opus-5");
    reply["content"] = content.clone();
    reply["stop_reason"] = json!("tool_use");
    reply["usage"] = json!({
        "input_tokens": 7,
        "output_tokens": 3,
        "cache_creation_input_tokens": null,
        "cache_read_input_tokens": 100,
        "service_tier": "standard"
    });
    answer_with(&server, ok(reply)).await;
    let gw = Gateway::new(config(&server)).unwrap();

    let resp = gw.complete(request()).await.unwrap();
    assert_eq!(Value::Array(resp.content.clone()), content);
    assert_eq!(resp.model, "claude-opus-5");
    assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(
        resp.usage,
        Usage { input_tokens: 7, output_tokens: 3, cache_creation_input_tokens: 0, cache_read_input_tokens: 100 }
    );

    // The next turn sends that content back untouched.
    let next = CompleteRequest { messages: vec![user_text("hello"), resp.as_turn(), user_text("go on")], ..request() };
    gw.complete(next.clone()).await.unwrap();
    let sent = sent(&server).await;
    assert_eq!(body(&sent[1])["messages"], Value::Array(next.messages));
    assert_eq!(body(&sent[1])["messages"][1]["content"], content);
}

#[tokio::test]
async fn cost_is_computed_from_usage() {
    let server = MockServer::start().await;
    let mut reply = message("claude-opus-5-5");
    reply["usage"] = json!({ "input_tokens": 1_000_000, "output_tokens": 0 });
    answer_with(&server, ok(reply)).await;
    let gw = Gateway::new(config(&server)).unwrap();
    let resp = gw.complete(request()).await.unwrap();
    assert!((resp.cost_usd.unwrap() - 4.0).abs() < 1e-9, "{:?}", resp.cost_usd);

    let server = MockServer::start().await;
    let mut reply = message("claude-opus-5-5");
    reply["usage"] = json!({
        "input_tokens": 1000,
        "output_tokens": 2000,
        "cache_creation_input_tokens": 10_000,
        "cache_read_input_tokens": 100_000
    });
    answer_with(&server, ok(reply)).await;
    let gw = Gateway::new(config(&server)).unwrap();
    let resp = gw.complete(request()).await.unwrap();
    let expected = (1000.0 * 4.0 + 2000.0 * 20.0 + 10_000.0 * 5.0 + 100_000.0 * 0.20) / 1e6;
    assert!((resp.cost_usd.unwrap() - expected).abs() < 1e-9, "{:?}", resp.cost_usd);
}

#[tokio::test]
async fn the_answering_models_prices_win_over_the_requested_ones() {
    let server = MockServer::start().await;
    let mut reply = message("claude-haiku-4-5-20251001");
    reply["usage"] = json!({ "input_tokens": 0, "output_tokens": 1_000_000 });
    answer_with(&server, ok(reply)).await;
    let gw = Gateway::new(config(&server)).unwrap();

    // The requested model has no known price, the one that answered does.
    let resp = gw.complete(CompleteRequest { model: Some("claude-next-1".into()), ..request() }).await.unwrap();
    assert!((resp.cost_usd.unwrap() - 5.0).abs() < 1e-9, "{:?}", resp.cost_usd);

    // And when the answering model is unknown, the requested model's prices apply.
    let server = MockServer::start().await;
    let mut reply = message("claude-opus-5");
    reply["usage"] = json!({ "input_tokens": 0, "output_tokens": 1_000_000 });
    answer_with(&server, ok(reply)).await;
    let gw = Gateway::new(config(&server)).unwrap();
    let resp = gw.complete(request()).await.unwrap();
    assert!((resp.cost_usd.unwrap() - 20.0).abs() < 1e-9, "{:?}", resp.cost_usd);
}
