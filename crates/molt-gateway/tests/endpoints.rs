//! The user's named endpoints (`MOLT_ENDPOINTS`) against fake endpoints of
//! each protocol: each is called with its own key from its own variable,
//! shaped by what its models are declared to do, priced as declared, and
//! probed by name. No real model API is called.

use std::collections::BTreeMap;

use molt_api::model::{user_text, CompleteRequest, Effort, StreamContext};
use molt_gateway::probe::{self, Check, Records};
use molt_gateway::provider::Provider;
use molt_gateway::{Config, Gateway};
use molt_proto::ErrorCode;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEYS: [&str; 4] = ["builtin-anthropic-key", "glm-secret", "ds-secret", "resp-secret"];

fn endpoint(protocol: &str, url: &str, key_env: &str, model: &str, extra: Value) -> Value {
    let mut declared = json!({ "tools": true, "streaming": true, "output_schema": true, "max_output": 8192 });
    declared.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    json!({ "protocol": protocol, "base_url": url, "key_env": key_env, "models": { model: declared } })
}

/// A gateway started with `vars` and nothing else of the environment.
fn config(vars: &[(&str, String)]) -> Config {
    let mut vars: BTreeMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
    vars.entry("MOLT_MODEL_RETRIES".into()).or_insert_with(|| "0".into());
    let mut cfg = Config::from_vars(|name| vars.get(name).cloned()).unwrap();
    cfg.retry_base = std::time::Duration::from_millis(1);
    cfg
}

fn message(model: &str, content: Value) -> Value {
    json!({
        "id": "msg_01", "type": "message", "role": "assistant", "model": model,
        "content": content, "stop_reason": "end_turn",
        "usage": { "input_tokens": 1000, "output_tokens": 100 }
    })
}

fn said(model: &str, text: &str) -> Value {
    message(model, json!([{ "type": "text", "text": text }]))
}

async fn answer(server: &MockServer, at: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).map(|v| v.to_str().unwrap())
}

/// The one request `server` got.
async fn one(server: &MockServer) -> Request {
    let got = server.received_requests().await.unwrap();
    assert_eq!(got.len(), 1, "{}", server.uri());
    got.into_iter().next().unwrap()
}

fn request(model: &str) -> CompleteRequest {
    CompleteRequest { model: Some(model.into()), messages: vec![user_text("hello")], ..Default::default() }
}

#[tokio::test]
async fn a_named_endpoint_of_each_protocol_gets_its_own_key_and_no_other() {
    let (builtin, glm, ds, resp) =
        (MockServer::start().await, MockServer::start().await, MockServer::start().await, MockServer::start().await);
    answer(&builtin, "/v1/messages", said("claude-opus-5-5", "builtin")).await;
    answer(&glm, "/v1/messages", said("glm-4.6", "glm")).await;
    let chat = json!({"id":"c1","model":"house-chat","choices":[{"index":0,"message":{"role":"assistant",
        "content":"ds"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":100}});
    answer(&ds, "/v1/chat/completions", chat).await;
    let responses = json!({"id":"resp_1","object":"response","model":"gpt-5-mini","status":"completed",
        "output":[{"type":"message","id":"m1","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"resp","annotations":[]}]}],
        "usage":{"input_tokens":1000,"output_tokens":100}});
    answer(&resp, "/v1/responses", responses).await;
    let endpoints = json!({
        "glm": endpoint("anthropic", &format!("{}/", glm.uri()), "GLM_API_KEY", "glm-4.6",
            json!({ "context": 200000, "price": { "input": 0.6, "output": 2.2 } })),
        "ds": endpoint("chat_completions", &ds.uri(), "DS_KEY", "house-chat", json!({ "context": 64000 })),
        "resp": endpoint("responses", &resp.uri(), "RESP_TOKEN", "gpt-5-mini",
            json!({ "efforts": ["low", "high"], "price": { "input": 1, "output": 10 } })),
        "nokey": endpoint("anthropic", "https://nokey.example", "NOKEY_API_KEY", "m", json!({})),
    });
    let gateway = Gateway::new(config(&[
        ("ANTHROPIC_API_KEY", KEYS[0].into()),
        ("ANTHROPIC_BASE_URL", builtin.uri()),
        ("MOLT_ENDPOINTS", endpoints.to_string()),
        ("GLM_API_KEY", KEYS[1].into()),
        ("DS_KEY", KEYS[2].into()),
        ("RESP_TOKEN", KEYS[3].into()),
        ("MOLT_MODEL_ALIASES", r#"{"cheap":"ds/house-chat"}"#.into()),
    ]))
    .unwrap();

    let reply = gateway.complete(request("glm/glm-4.6")).await.unwrap();
    assert_eq!(reply.text(), "glm");
    // Priced as declared: 1000 * 0.6 + 100 * 2.2 per million.
    assert!((reply.cost_usd.unwrap() - 0.00082).abs() < 1e-12, "{:?}", reply.cost_usd);
    let source = reply.price_source.unwrap();
    assert_eq!((source.entry.as_str(), source.source.as_str()), ("glm/glm-4.6", "endpoint glm"));
    assert_eq!(reply.context_tokens, Some(200_000));
    let reply = gateway.complete(request("cheap")).await.unwrap();
    assert_eq!((reply.text().as_str(), reply.context_tokens), ("ds", Some(64_000)));
    assert!(reply.cost_usd.is_none(), "no declared price and none in the table: unknown, never zero");
    let reply = gateway.complete(CompleteRequest { effort: Some(Effort::High), ..request("resp/gpt-5-mini") }).await;
    assert_eq!(reply.unwrap().text(), "resp");
    assert_eq!(gateway.complete(request("opus")).await.unwrap().text(), "builtin");

    let (b, g, d, r) = (one(&builtin).await, one(&glm).await, one(&ds).await, one(&resp).await);
    assert_eq!((header(&b, "x-api-key"), header(&b, "authorization")), (Some(KEYS[0]), None));
    assert_eq!((header(&g, "x-api-key"), header(&g, "authorization")), (Some(KEYS[1]), None));
    assert_eq!(header(&g, "anthropic-version"), Some("2023-06-01"));
    assert_eq!((header(&d, "authorization"), header(&d, "x-api-key")), (Some("Bearer ds-secret"), None));
    assert_eq!((header(&r, "authorization"), header(&r, "x-api-key")), (Some("Bearer resp-secret"), None));
    assert!(header(&d, "anthropic-version").is_none() && header(&r, "anthropic-version").is_none());
    assert_eq!(r.url.path(), "/v1/responses");
    let (gb, db, rb): (Value, Value, Value) = (g.body_json().unwrap(), d.body_json().unwrap(), r.body_json().unwrap());
    assert_eq!((gb["model"].as_str(), gb["max_tokens"].as_u64()), (Some("glm-4.6"), Some(8192)));
    assert_eq!((db["model"].as_str(), db["max_completion_tokens"].as_u64()), (Some("house-chat"), Some(8192)));
    assert_eq!((rb["model"].as_str(), rb["reasoning"]["effort"].as_str()), (Some("gpt-5-mini"), Some("high")));
    // Each server saw its own key and none of the others, in any header or the body.
    for (req, own) in [(&b, KEYS[0]), (&g, KEYS[1]), (&d, KEYS[2]), (&r, KEYS[3])] {
        let seen = format!("{:?} {}", req.headers, String::from_utf8_lossy(&req.body));
        for key in KEYS.iter().filter(|k| **k != own) {
            assert!(!seen.contains(key), "{} got {key}", req.url);
        }
    }

    // An undeclared model, or an endpoint whose key is not set, is refused before HTTP.
    let err = gateway.complete(request("glm/glm-5")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(err.message.contains("serves no model \"glm-5\"") && err.message.contains("glm-4.6"), "{}", err.message);
    let err = gateway.complete(request("nokey/m")).await.unwrap_err();
    assert!(err.message.contains("set NOKEY_API_KEY"), "{}", err.message);
    let err = gateway.complete(CompleteRequest { effort: Some(Effort::Medium), ..request("resp/gpt-5-mini") }).await;
    assert_eq!(err.unwrap_err().code, ErrorCode::Invalid, "an effort not declared is refused");
    for server in [&builtin, &glm, &ds, &resp] {
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "nothing more was sent");
    }
    // The key never shows in the config's debug output.
    let shown = format!("{:?}", config(&[("MOLT_ENDPOINTS", endpoints.to_string()), ("GLM_API_KEY", KEYS[1].into())]));
    assert!(!shown.contains(KEYS[1]) && shown.contains("GLM_API_KEY"), "{shown}");
}

#[tokio::test]
async fn what_an_anthropic_endpoint_declares_shapes_its_calls_without_a_probe() {
    let server = MockServer::start().await;
    answer(
        &server,
        "/v1/messages",
        message(
            "m",
            json!([{"type":"tool_use","id":"t","name":"molt_output",
        "input":{"ok":true}}]),
        ),
    )
    .await;
    let mut endpoints = json!({ "local": endpoint("anthropic", &server.uri(), "LOCAL_TOKEN", "m",
        json!({ "tools": false, "streaming": false, "output_schema": false, "max_output": 1024 })) });
    endpoints["local"]["auth"] = json!("bearer");
    let gateway = Gateway::new(config(&[
        ("MOLT_ENDPOINTS", endpoints.to_string()),
        ("LOCAL_TOKEN", "local-token".into()),
        ("MOLT_EFFORT", "high".into()),
    ]))
    .unwrap();
    let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } });
    // MOLT_EFFORT is not sent to a model that declares no effort; a request asking for one is refused.
    let err = gateway.complete(request("local/m")).await.unwrap_err();
    assert!(err.message.contains("declares no efforts"), "{}", err.message);

    let gateway =
        Gateway::new(config(&[("MOLT_ENDPOINTS", endpoints.to_string()), ("LOCAL_TOKEN", "local-token".into())]))
            .unwrap();
    let asked = CompleteRequest {
        output_schema: Some(schema.clone()),
        stream: Some(StreamContext { attempt: None, turn: 0 }),
        max_tokens: Some(4096),
        ..request("local/m")
    };
    let reply = gateway.complete_streamed(asked, None, |_| {}).await.unwrap();
    assert_eq!(reply.text(), r#"{"ok":true}"#, "the schema's tool input comes back as the answer");
    let tooled =
        CompleteRequest { tools: vec![json!({"name":"t","input_schema":{"type":"object"}})], ..request("local/m") };
    let err = gateway.complete(tooled).await.unwrap_err();
    assert!(err.message.contains("declares tools = false"), "{}", err.message);

    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent.len(), 1, "only the schema call was sent");
    assert_eq!(header(&sent[0], "authorization"), Some("Bearer local-token"));
    assert!(header(&sent[0], "x-api-key").is_none());
    let body: Value = sent[0].body_json().unwrap();
    assert!(body.get("stream").is_none(), "declared not to stream: made whole");
    assert_eq!(body["max_tokens"], 1024, "capped at the declared max_output");
    assert_eq!(body["tools"][0]["name"], probe::SCHEMA_TOOL);
    assert_eq!(body["tools"][0]["input_schema"], schema);
    assert!(body.get("output_config").is_none(), "{body}");
}

/// An Anthropic-compatible endpoint that streams and calls tools, ignores
/// `output_config.format` (it answers in prose) and refuses an effort.
fn ignores_schemas(req: &Request) -> ResponseTemplate {
    let body: Value = req.body_json().unwrap();
    if body.pointer("/output_config/effort").is_some() {
        return ResponseTemplate::new(400).set_body_json(
            json!({"type":"error","error":{"type":"invalid_request_error","message":"unknown field effort"}}),
        );
    }
    if body["stream"] == true {
        let events: String = [
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
        .collect();
        return ResponseTemplate::new(200).set_body_raw(events, "text/event-stream");
    }
    if let Some(tool) = body["tools"].as_array().and_then(|t| t.first()) {
        let input = match tool["name"].as_str().unwrap() {
            probe::SCHEMA_TOOL => json!({ "answer": 5 }),
            "submit_notes" => json!({ "notes": [] }),
            _ => json!({ "n": 7 }),
        };
        let call = json!([{ "type": "tool_use", "id": "toolu_1", "name": tool["name"], "input": input }]);
        return ResponseTemplate::new(200).set_body_json(message("glm-4.6", call));
    }
    ResponseTemplate::new(200).set_body_json(said("glm-4.6", "2 + 3 is 5."))
}

#[tokio::test]
async fn a_probe_of_a_named_endpoint_records_an_ignored_output_schema_and_its_calls_then_go_through_a_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ignores_schemas).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("models.json");
    // Declared to keep to schemas and take efforts, as the user believed.
    let endpoints = json!({ "glm": endpoint("anthropic", &server.uri(), "GLM_API_KEY", "glm-4.6",
        json!({ "efforts": ["low", "high"], "price": { "input": 0.6, "output": 2.2 } })) });
    let vars = [
        ("MOLT_ENDPOINTS", endpoints.to_string()),
        ("GLM_API_KEY", "glm-secret".into()),
        ("MOLT_MODEL_ALIASES", r#"{"glm":"glm/glm-4.6"}"#.into()),
        ("MOLT_MODELS_FILE", file.display().to_string()),
    ];
    // Before the first probe there is no file.
    let gateway = Gateway::new(probe::probing(config(&vars[..3]))).unwrap();
    let plan = gateway.probe_plan("glm").unwrap();
    assert_eq!(
        (plan.provider, plan.endpoint.as_deref(), plan.qualified().as_str()),
        (Provider::Anthropic, Some("glm"), "glm/glm-4.6")
    );
    assert_eq!(plan.base_url, server.uri());
    assert_eq!(plan.checks, [Check::Tools, Check::Streaming, Check::OutputSchema, Check::Effort(Effort::Low)]);
    assert!(server.received_requests().await.unwrap().is_empty(), "a plan calls nothing");

    let report = gateway.probe(&plan, 1).await;
    assert_eq!((report.calls, report.stopped.as_deref()), (4, None));
    let found = &report.probed;
    assert_eq!(
        (found.provider, found.base_url.as_str(), found.model.as_str()),
        (Provider::Anthropic, server.uri().as_str(), "glm-4.6")
    );
    assert_eq!(
        (found.tools, found.streaming, found.output_schema, found.effort),
        (Some(true), Some(true), Some(false), Some(false))
    );
    assert!(found.cost_usd.is_some_and(|c| c > 0.0), "priced as declared: {:?}", found.cost_usd);
    // The record names the endpoint; the Messages API does not count reasoning tokens apart: unknown.
    assert_eq!((found.endpoint.as_deref(), found.reasoning_tokens), (Some("glm"), None));
    let mut records = Records::default();
    records.put(report.probed);
    records.write(&file).unwrap();

    // A gateway started from then on asks for the schema through a tool and sends no effort.
    let gateway = Gateway::new(config(&vars)).unwrap();
    let schema = json!({ "type": "object", "properties": { "answer": { "type": "integer" } }, "required": ["answer"] });
    let before = server.received_requests().await.unwrap().len();
    let reply =
        gateway.complete(CompleteRequest { output_schema: Some(schema.clone()), ..request("glm") }).await.unwrap();
    assert_eq!(reply.text(), r#"{"answer":5}"#);
    let err = gateway.complete(CompleteRequest { effort: Some(Effort::Low), ..request("glm/glm-4.6") }).await;
    assert!(err.unwrap_err().message.contains("molt models probe"));
    // Memory learns through its own submit_notes tool, which reaches the endpoint as it is.
    let notes = json!({ "name": "submit_notes", "description": "Submit.", "input_schema": { "type": "object" } });
    let reply = gateway.complete(CompleteRequest { tools: vec![notes.clone()], ..request("glm") }).await.unwrap();
    assert_eq!(reply.content[0]["name"], "submit_notes");
    let sent: Vec<Value> =
        server.received_requests().await.unwrap()[before..].iter().map(|r| r.body_json().unwrap()).collect();
    assert_eq!(sent.len(), 2, "the refused effort was not sent");
    assert_eq!(sent[0]["tools"][0]["name"], probe::SCHEMA_TOOL);
    assert_eq!(sent[0]["tool_choice"]["name"], probe::SCHEMA_TOOL);
    assert!(sent[0].get("output_config").is_none(), "{}", sent[0]);
    assert_eq!(sent[1]["tools"], json!([notes]));
    assert!(sent[1].get("tool_choice").is_none() && sent[1].get("output_config").is_none(), "{}", sent[1]);
}

#[test]
fn bad_endpoints_are_refused_saying_what_to_change() {
    let ok = endpoint("anthropic", "https://glm.example/api", "GLM_API_KEY", "glm-4.6", json!({}));
    let with = |name: &str, f: &dyn Fn(&mut Value)| {
        let mut e = ok.clone();
        f(&mut e);
        json!({ name: e }).to_string()
    };
    let refused = [
        (with("openai", &|_| {}), "built-in endpoints"),
        (with("GLM", &|_| {}), "lowercase"),
        (with("glm", &|e| e["base_url"] = json!("http://glm.example")), "must be https"),
        (with("glm", &|e| e["base_url"] = json!("https://glm.example/v1/")), "remove the trailing /v1"),
        (with("glm", &|e| e["base_url"] = json!("https://u:p@glm.example")), "cannot hold credentials"),
        (with("glm", &|e| e["key_env"] = json!("sk-123")), "must be a variable name"),
        (with("glm", &|e| e["key_env"] = json!("MOLT_KEY")), "not a variable for a key"),
        (with("glm", &|e| e["protocol"] = json!("realtime")), "unknown variant"),
        (with("glm", &|e| e["models"] = json!({})), "declare the models"),
        (with("glm", &|e| e["models"]["glm-4.6"]["max_output"] = json!(0)), "max_output must be positive"),
        (with("glm", &|e| e["models"]["glm-4.6"]["context"] = json!(100)), "larger than max_output"),
        (with("glm", &|e| e["models"]["glm-4.6"]["typo"] = json!(true)), "unknown field"),
        (with("glm", &|e| e["models"]["glm-4.6"]["price"] = json!({ "input": -1, "output": 1 })), "not negative"),
        (
            with("glm", &|e| {
                e["protocol"] = json!("chat_completions");
                e["auth"] = json!("x-api-key");
            }),
            "OpenAI protocols take bearer",
        ),
        (
            with("glm", &|e| {
                e["protocol"] = json!("responses");
                e["models"]["glm-4.6"]["efforts"] = json!(["max"]);
            }),
            "not an OpenAI effort",
        ),
    ];
    for (endpoints, why) in &refused {
        let vars: BTreeMap<&str, String> =
            [("MOLT_ENDPOINTS", endpoints.clone()), ("GLM_API_KEY", "k".into())].into_iter().collect();
        let err = Config::from_vars(|name| vars.get(name).cloned()).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{endpoints}: {err:#}");
    }
    // An alias may name a declared endpoint, and nothing else of that shape.
    let vars = |aliases: &str| -> BTreeMap<&str, String> {
        [
            ("MOLT_ENDPOINTS", json!({ "glm": ok.clone() }).to_string()),
            ("GLM_API_KEY", "k".into()),
            ("MOLT_MODEL_ALIASES", aliases.into()),
        ]
        .into_iter()
        .collect()
    };
    let good = vars(r#"{"g":"glm/glm-4.6"}"#);
    assert!(Config::from_vars(|name| good.get(name).cloned()).is_ok());
    let bad = vars(r#"{"g":"zhipu/glm-4.6"}"#);
    let err = Config::from_vars(|name| bad.get(name).cloned()).unwrap_err();
    assert!(format!("{err:#}").contains("endpoint/model"), "{err:#}");
}

#[tokio::test]
async fn without_named_endpoints_a_slash_in_a_model_id_still_goes_to_the_default_endpoint() {
    let server = MockServer::start().await;
    answer(&server, "/v1/messages", said("deepseek/house-chat", "ok")).await;
    let gateway =
        Gateway::new(config(&[("ANTHROPIC_API_KEY", "k".into()), ("ANTHROPIC_BASE_URL", server.uri())])).unwrap();
    assert_eq!(gateway.complete(request("deepseek/house-chat")).await.unwrap().text(), "ok");
    let body: Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
    assert_eq!(body["model"], "deepseek/house-chat");
}
