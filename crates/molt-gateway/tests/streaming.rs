//! Streaming over real HTTP, including a response held open until a preview
//! is observed. No API credentials or live model calls are needed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_api::model::{user_text, CompleteRequest, StreamContext};
use molt_gateway::{Config, Gateway};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(base_url: String) -> Config {
    Config {
        api_key: "test-key".into(),
        base_url,
        default_model: "opus".into(),
        max_tokens: 32000,
        effort: None,
        timeout: Duration::from_secs(5),
        max_retries: 2,
        max_in_flight: 4,
        fallbacks: true,
        retry_base: Duration::from_millis(1),
    }
}

fn request() -> CompleteRequest {
    CompleteRequest {
        messages: vec![user_text("hello")],
        stream: Some(StreamContext { attempt: Some(1), turn: 2 }),
        ..Default::default()
    }
}

fn event(value: Value) -> String {
    format!("event: {}\ndata: {value}\n\n", value["type"].as_str().unwrap())
}

fn start(model: &str) -> String {
    event(json!({"type":"message_start","message":{
        "id":"msg_stream", "model":model, "role":"assistant", "type":"message", "content":[],
        "usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":2}
    }}))
}

fn text() -> String {
    event(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}))
        + &event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}))
}

fn end(reason: &str) -> String {
    event(json!({"type":"content_block_stop","index":0}))
        + &event(json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":9}}))
        + &event(json!({"type":"message_stop"}))
}

#[tokio::test]
async fn text_arrives_before_the_response_is_finished() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let first = start("claude-opus-5-5")
        + &text()
        + &event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"，继续"}}));
    let last = end("end_turn");
    let (release, wait) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let split = loop {
            let mut chunk = [0; 2048];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..split]);
        let length: usize = headers
            .lines()
            .find_map(|l| {
                let (key, value) = l.split_once(':')?;
                key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() < split + length {
            let mut chunk = [0; 2048];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&chunk[..n]);
        }
        let body: Value = serde_json::from_slice(&bytes[split..split + length]).unwrap();
        assert_eq!(body["stream"], true);
        assert!(body.get("attempt").is_none());
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            first.len() + last.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        // Split inside the UTF-8 text and SSE frame as well as between frames.
        for piece in first.as_bytes().chunks(7) {
            socket.write_all(piece).await.unwrap();
        }
        wait.await.unwrap();
        socket.write_all(last.as_bytes()).await.unwrap();
    });
    let gateway = Gateway::new(config(url)).unwrap();
    let (preview, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let call = tokio::spawn(async move {
        gateway
            .complete_streamed(request(), None, |text| {
                let _ = preview.send(text);
            })
            .await
            .unwrap()
    });
    let text = tokio::time::timeout(Duration::from_secs(3), seen.recv())
        .await
        .expect("preview waited for the full response")
        .unwrap();
    assert_eq!(text, "你好");
    let tail = tokio::time::timeout(Duration::from_secs(3), seen.recv())
        .await
        .expect("coalesced text did not flush while the server paused")
        .unwrap();
    assert_eq!(tail, "，继续");
    assert!(!call.is_finished());
    release.send(()).unwrap();
    let response = call.await.unwrap();
    assert_eq!(response.text(), "你好，继续");
    assert_eq!(
        (response.usage.input_tokens, response.usage.output_tokens, response.usage.cache_read_input_tokens),
        (10, 9, 2)
    );
    assert!(response.cost_usd.unwrap() > 0.0);
    server.await.unwrap();
}

#[tokio::test]
async fn partial_and_error_streams_are_not_retried_or_returned_as_success() {
    for suffix in ["".to_owned(), event(json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}))] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"stream":true})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(start("claude-opus-5-5") + &text() + &suffix, "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let gateway = Gateway::new(config(server.uri())).unwrap();
        let error = gateway.complete_streamed(request(), None, |_| {}).await.unwrap_err();
        assert!(error.message.contains("stream"), "{error}");
        server.verify().await;
    }
}

#[tokio::test]
async fn malformed_tool_input_never_becomes_a_completed_response() {
    let server = MockServer::start().await;
    let stream = start("claude-opus-5-5")
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"run","input":{}}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}),
        )
        + &end("tool_use");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let gateway = Gateway::new(config(server.uri())).unwrap();
    let previews = Arc::new(Mutex::new(Vec::new()));
    let copy = previews.clone();
    let error =
        gateway.complete_streamed(request(), None, move |text| copy.lock().unwrap().push(text)).await.unwrap_err();
    assert!(error.message.contains("invalid tool JSON"));
    assert!(previews.lock().unwrap().is_empty());
    server.verify().await;
}

#[tokio::test]
async fn early_http_errors_can_retry_without_repeating_previews_and_unknown_prices_stay_unknown() {
    let server = MockServer::start().await;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let copy = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            if copy.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "0")
                    .set_body_json(json!({"error":{"type":"rate_limit_error","message":"wait"}}))
            } else {
                ResponseTemplate::new(200)
                    .set_body_raw(start("unknown-model") + &text() + &end("end_turn"), "text/event-stream")
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let gateway = Gateway::new(config(server.uri())).unwrap();
    let previews = Arc::new(Mutex::new(Vec::new()));
    let copy = previews.clone();
    let request = CompleteRequest { model: Some("unknown-model".into()), ..request() };
    let response = gateway.complete_streamed(request, None, move |text| copy.lock().unwrap().push(text)).await.unwrap();
    assert_eq!(previews.lock().unwrap().join(""), "你好");
    assert_eq!(response.cost_usd, None);
    server.verify().await;
}
