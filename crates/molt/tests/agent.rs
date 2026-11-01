//! `molt do` end to end: the kernel, the real gateway, tools and planner
//! processes, and a fake Messages API that plays the model.
//!
//! The fake is stateless. It reads its role and the turn from the request:
//! the check designer is offered `submit_check` and attempts are not; the
//! first turn has one message, a turn whose last user message holds
//! `tool_result` blocks follows tool calls, and a plain-text one carries a
//! failed check's feedback. It answers `400` to anything the real API would
//! refuse (see [`check_request`]), and every test checks that it never had to.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use molt::agent::{run_task, Done, Interrupted, Learn};
use molt::{Config, Secrets, TransportKind};
use molt_api::fs::{ChangeKind, DROP, FORK, MERGE, WRITE};
use molt_api::memory::SUBMIT_NOTES;
use molt_api::model::COMPLETE;
use molt_api::planner::tools::{
    DISPUTE_CHECK, LIST_FILES, RUN, SUBMIT_CHECK, SUBMIT_JUDGEMENT, SUBMIT_REVIEW, SUBMIT_ROUTE, SUBMIT_VETTING,
    WRITE_FILE,
};
use molt_api::planner::{AttemptStatus, CheckSpec, Outcome, RunRequest, RunResponse, Schedule, RUN as PLANNER_RUN};
use molt_api::progress::Progress;
use molt_api::shell::RUN as RUN_CHECK;
use molt_kernel::audit::AuditEvent;
use molt_proto::{Kind, ServiceId};
use molt_transport::{nats, Secret};
use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL: &str = "claude-opus-5-5";
const API_KEY: &str = "test";
const CHECK: &str = "grep -q hello greeting.txt";
/// Over a megabyte, the most nats-server lets through by default.
const LONG_GREETING: usize = 1536 * 1024;
/// Far more than a run needs; only a hang should ever hit it.
const RUN_LIMIT: Duration = Duration::from_secs(120);
/// The `--scratch` placeholder in molt.example.toml.
const EXAMPLE_SCRATCH: &str = "/home/you/.cache/molt/work";
/// The note the fake model takes from every finished run.
const LEARNED_TEXT: &str = "The done-check greps greeting.txt for hello.";

#[derive(Clone)]
enum Fake {
    /// Attempts write greeting.txt and say they are done.
    Greets,
    /// Attempts write greeting.txt and notes.txt, then say they are done.
    GreetsWithNotes,
    ConcurrentEdit(PathBuf),
    /// The designer writes check.sh and submits it; attempts then greet.
    DesignsThenGreets,
    /// Attempts write the wrong greeting, then fix it after the check fails.
    FixesAfterFeedback,
    /// Attempts write a greeting this many bytes long and say they are done.
    GreetsAtLength(usize),
    /// Every request is refused.
    Refuses,
    /// Attempts dispute the check rather than greet.
    Disputes,
    /// The designer lists files turn after turn and never submits a check.
    NeverSubmits,
    /// Attempts run a command that starts a long `sleep`, writes its pid to
    /// this file and waits for it.
    Hangs(PathBuf),
    /// The designer submits the check alone, with no file of its own; attempts greet.
    DesignsWithoutFiles,
    /// Attempts greet and, when they are offered this tool, call it too.
    GreetsWithTool(&'static str),
    /// Attempts greet and, when they are offered the first of these tools,
    /// make each call in turn, in the same turn.
    GreetsWithCalls(Vec<(&'static str, Value)>),
    /// Attempts greet only on a model whose id has this in it; on any other
    /// they write the wrong word.
    GreetsOn(&'static str),
}

#[derive(Debug, PartialEq)]
enum Turn {
    First,
    AfterTools,
    Feedback,
}

struct FakeApi(Fake);

impl Respond for FakeApi {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = match check_request(req) {
            Ok(body) => body,
            Err(problem) => return ResponseTemplate::new(400).set_body_json(error("invalid_request_error", &problem)),
        };
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let n = messages.len();
        let model = body["model"].as_str().unwrap_or("unknown");
        let stream = body["stream"] == true;
        // Memory learning from a finished run submits its notes with a tool,
        // having first said something no schema allows, as some endpoints do.
        if learns(&body) {
            let note =
                json!({ "kind": "lesson", "text": LEARNED_TEXT, "confidence": 0.9, "steps": [1], "contradicts": "" });
            let content = vec![
                json!({ "type": "thinking", "thinking": "", "signature": signature(n) }),
                json!({ "type": "text", "text": "<notes>the check greps greeting.txt</notes>" }),
                tool_use(n, SUBMIT_NOTES, json!({ "notes": [note], "supports": [] })),
            ];
            return message(n, model, content, "tool_use", stream);
        }
        // The router of a task that names `auto` picks the full workflow.
        if body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_ROUTE)) {
            let input = json!({ "workflow": "verify", "reason": "A change to check." });
            let content = vec![
                json!({ "type": "thinking", "thinking": "", "signature": signature(n) }),
                tool_use(n, SUBMIT_ROUTE, input),
            ];
            return message(n, model, content, "tool_use", stream);
        }
        // The reviewer of a passing result approves it.
        if body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_REVIEW)) {
            let input = json!({ "verdict": "approve", "findings": [] });
            let content = vec![
                json!({ "type": "thinking", "thinking": "", "signature": signature(n) }),
                tool_use(n, SUBMIT_REVIEW, input),
            ];
            return message(n, model, content, "tool_use", stream);
        }
        // The vetter of a designed check finds it fits the task.
        if body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_VETTING)) {
            let content = vec![
                json!({ "type": "thinking", "thinking": "", "signature": signature(n) }),
                tool_use(n, SUBMIT_VETTING, json!({ "conflicts": [] })),
            ];
            return message(n, model, content, "tool_use", stream);
        }
        // The judge of a rubric finds its one criterion met.
        if body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_JUDGEMENT)) {
            let input = json!({ "findings": [{ "criterion": 1, "met": true, "reason": "greeting.txt greets." }] });
            let content = vec![
                json!({ "type": "thinking", "thinking": "", "signature": signature(n) }),
                tool_use(n, SUBMIT_JUDGEMENT, input),
            ];
            return message(n, model, content, "tool_use", stream);
        }
        let designer = body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_CHECK));
        let last = messages.last().cloned().unwrap_or(Value::Null);
        let after_tools = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
        let turn = match (messages.len(), after_tools) {
            (1, _) => Turn::First,
            (_, true) => Turn::AfterTools,
            (_, false) => Turn::Feedback,
        };
        let write = |path: &str, content: &str| tool_use(n, WRITE_FILE, json!({ "path": path, "content": content }));
        let (mut content, stop) = match (&self.0, designer, turn) {
            (Fake::Refuses, ..) => return message(n, model, vec![], "refusal", stream),
            (Fake::NeverSubmits, true, _) => (vec![tool_use(n, LIST_FILES, json!({}))], "tool_use"),
            (Fake::DesignsWithoutFiles, true, _) => {
                let input = json!({ "command": CHECK, "files": [], "rationale": "Greets." });
                (vec![tool_use(n, SUBMIT_CHECK, input)], "tool_use")
            }
            (_, true, Turn::First) => (vec![write("check.sh", &format!("{CHECK}\n"))], "tool_use"),
            (_, true, _) => {
                let input = json!({ "command": "sh check.sh", "files": ["check.sh"], "rationale": "Greets." });
                (vec![tool_use(n, SUBMIT_CHECK, input)], "tool_use")
            }
            (Fake::Disputes, false, _) => {
                let input = json!({
                    "assertion": "the check greps greeting.txt for hello",
                    "task_quote": "Write a greeting into greeting.txt.",
                    "reproduce": CHECK,
                });
                (vec![tool_use(n, DISPUTE_CHECK, input)], "tool_use")
            }
            (Fake::FixesAfterFeedback, false, Turn::First) => (vec![write("greeting.txt", "goodbye\n")], "tool_use"),
            (Fake::GreetsOn(on), false, Turn::First) if !model.contains(on) => {
                (vec![write("greeting.txt", "goodbye\n")], "tool_use")
            }
            (Fake::Hangs(pidfile), false, Turn::First) => {
                let command = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
                (vec![tool_use(n, RUN, json!({ "command": command }))], "tool_use")
            }
            (Fake::GreetsAtLength(len), false, Turn::First) => {
                let greeting = format!("hello{}\n", "o".repeat(len - "hello\n".len()));
                (vec![write("greeting.txt", &greeting)], "tool_use")
            }
            (Fake::GreetsWithNotes, false, Turn::First) => {
                let mut notes = write("notes.txt", "scratch\n");
                notes["id"] = json!(format!("toolu_{n:03}_notes"));
                (vec![write("greeting.txt", "hello\n"), notes], "tool_use")
            }
            (Fake::GreetsWithTool(tool), false, Turn::First)
                if body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == *tool)) =>
            {
                (vec![write("greeting.txt", "hello\n"), tool_use(n, tool, json!({ "text": "hi" }))], "tool_use")
            }
            (Fake::GreetsWithCalls(calls), false, Turn::First)
                if body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == calls[0].0)) =>
            {
                let mut content = vec![write("greeting.txt", "hello\n")];
                for (i, (tool, input)) in calls.iter().enumerate() {
                    let mut call = tool_use(n, tool, input.clone());
                    call["id"] = json!(format!("toolu_{n:03}_{i}"));
                    content.push(call);
                }
                (content, "tool_use")
            }
            (_, false, Turn::First | Turn::Feedback) => (vec![write("greeting.txt", "hello\n")], "tool_use"),
            (_, false, Turn::AfterTools) => {
                if let Fake::ConcurrentEdit(original) = &self.0 {
                    // A user edits the original outside the sandbox while the
                    // attempt is working; its done-check cannot do this.
                    std::fs::write(original, "theirs\n").unwrap();
                }
                (vec![json!({ "type": "text", "text": "Wrote greeting.txt." })], "end_turn")
            }
        };
        // Each turn opens with a thinking block whose signature names the
        // turn's place in the conversation, so check_request can tell that
        // it came back unmodified.
        content.insert(0, json!({ "type": "thinking", "thinking": "", "signature": signature(n) }));
        message(n, model, content, stop, stream)
    }
}

fn message(n: usize, model: &str, content: Vec<Value>, stop: &str, stream: bool) -> ResponseTemplate {
    let body = json!({
        "id": format!("msg_{n:03}"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop,
        "stop_sequence": null,
        "usage": {
            "input_tokens": 1000,
            "output_tokens": 100,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    if !stream {
        return ResponseTemplate::new(200).set_body_json(body);
    }
    let event = |value: Value| format!("event: {}\ndata: {value}\n\n", value["type"].as_str().unwrap());
    let mut start = body.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    let mut wire = event(json!({"type":"message_start","message":start}));
    for (index, block) in body["content"].as_array().unwrap().iter().enumerate() {
        let mut initial = block.clone();
        let deltas = match block["type"].as_str().unwrap() {
            "text" => {
                initial["text"] = json!("");
                vec![json!({"type":"text_delta","text":block["text"]})]
            }
            "thinking" => {
                initial["thinking"] = json!("");
                initial["signature"] = json!("");
                vec![
                    json!({"type":"thinking_delta","thinking":block["thinking"]}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ]
            }
            "tool_use" => {
                initial["input"] = json!({});
                vec![json!({"type":"input_json_delta","partial_json":block["input"].to_string()})]
            }
            _ => unreachable!(),
        };
        wire.push_str(&event(json!({"type":"content_block_start","index":index,"content_block":initial})));
        for delta in deltas {
            wire.push_str(&event(json!({"type":"content_block_delta","index":index,"delta":delta})));
        }
        wire.push_str(&event(json!({"type":"content_block_stop","index":index})));
    }
    wire.push_str(&event(json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":body["usage"]})));
    wire.push_str(&event(json!({"type":"message_stop"})));
    ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream")
}

/// Ids follow from the conversation's length, so replies are reproducible.
fn tool_use(n: usize, name: &str, input: Value) -> Value {
    json!({ "type": "tool_use", "id": format!("toolu_{n:03}"), "name": name, "input": input })
}

/// The signature of the thinking block that opens the reply to a request of
/// `n` messages, which becomes message `n` of the next request.
fn signature(n: usize) -> String {
    format!("sig_{n:03}")
}

/// The request's body, or what the real Messages API would answer `400`
/// (or `401`) to: a missing key or version, no model or `max_tokens`, a
/// conversation that does not start and end with a user turn, an empty or
/// modified assistant turn, a tool call without its result right after it,
/// a result for no call, or duplicate tool names.
fn check_request(req: &Request) -> Result<Value, String> {
    if req.headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(API_KEY) {
        return Err("missing or wrong x-api-key".into());
    }
    if req.headers.get("anthropic-version").is_none() {
        return Err("missing anthropic-version".into());
    }
    let body: Value = req.body_json().map_err(|e| format!("body is not JSON: {e}"))?;
    if !body["model"].is_string() || !body["max_tokens"].as_u64().is_some_and(|t| t > 0) {
        return Err("model and a positive max_tokens are required".into());
    }
    let mut names: Vec<&str> =
        body["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    if names.len() != count {
        return Err("tool names must be unique".into());
    }
    let messages = body["messages"].as_array().ok_or("messages must be an array")?;
    let role = |i: usize| messages.get(i).and_then(|m| m["role"].as_str());
    if role(0) != Some("user") || role(messages.len().saturating_sub(1)) != Some("user") {
        return Err("the conversation must start and end with a user turn".into());
    }
    for (i, msg) in messages.iter().enumerate() {
        let blocks = msg["content"].as_array().filter(|b| !b.is_empty()).ok_or(format!("message {i} is empty"))?;
        let of_type = |kind: &'static str| blocks.iter().filter(move |b| b["type"] == kind);
        if role(i) == Some("assistant") {
            if blocks[0]["type"] != "thinking" || blocks[0]["signature"] != signature(i) {
                return Err(format!("assistant message {i} is not the reply the API gave"));
            }
            let calls: Vec<&Value> = of_type("tool_use").map(|b| &b["id"]).collect();
            if calls.is_empty() {
                continue;
            }
            let next = messages.get(i + 1).and_then(|m| m["content"].as_array()).cloned().unwrap_or_default();
            let results: Vec<&Value> =
                next.iter().take_while(|b| b["type"] == "tool_result").map(|b| &b["tool_use_id"]).collect();
            if results != calls {
                return Err(format!("the tool calls of message {i} are not answered first thing, in order"));
            }
        } else if let Some(stray) = of_type("tool_result").find(|r| {
            let calls = i.checked_sub(1).and_then(|p| messages[p]["content"].as_array());
            !calls.is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use" && b["id"] == r["tool_use_id"]))
        }) {
            return Err(format!("message {i} answers a tool call that was not made: {}", stray["tool_use_id"]));
        }
    }
    Ok(body)
}

fn error(kind: &str, message: &str) -> Value {
    json!({ "type": "error", "error": { "type": kind, "message": message } })
}

async fn fake_api(fake: Fake) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(FakeApi(fake)).mount(&server).await;
    server
}

fn bin_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_molt-gateway")).parent().unwrap().to_path_buf()
}

/// A workspace with one file, a data dir beside it, and the default agent
/// setup pointed at `server`.
struct Setup {
    workspace: TempDir,
    data: TempDir,
    cfg: Config,
    /// Have memory learn from each run: one more model call after it.
    learn: bool,
}

impl Setup {
    fn new(server: &MockServer) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("README.txt"), "A test project.\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut cfg = Config::agent(&workspace.path().canonicalize().unwrap(), data.path(), &bin_dir()).unwrap();
        cfg.kernel.fsync = false;
        for svc in &mut cfg.services {
            // The developer's own MOLT_* settings must not change the test.
            svc.pass_env.clear();
            if std::env::var_os("RUST_LOG").is_none() {
                svc.env.insert("RUST_LOG".into(), "warn".into());
            }
        }
        let model = cfg.service_mut("model").unwrap();
        model.env.insert("ANTHROPIC_BASE_URL".into(), server.uri());
        model.env.insert("ANTHROPIC_API_KEY".into(), API_KEY.into());
        Self { workspace, data, cfg, learn: false }
    }

    fn request(&self, check: Option<&str>, attempts: u32) -> RunRequest {
        let workspace = self.workspace.path().canonicalize().unwrap();
        let mut req = RunRequest::new("Write a greeting into greeting.txt.", workspace.to_str().unwrap());
        req.check = check.map(str::to_owned);
        req.attempts = attempts;
        req
    }

    async fn run(&self, req: RunRequest) -> (RunResponse, Vec<Progress>) {
        let (done, events) = self.run_done(req).await;
        (done.run, events)
    }

    async fn run_done(&self, req: RunRequest) -> (Done, Vec<Progress>) {
        let mut events = Vec::new();
        let run = run_task(
            &self.cfg,
            req,
            self.learn.then(Learn::default),
            |e| events.push(e.clone()),
            std::future::pending(),
        );
        let done = tokio::time::timeout(RUN_LIMIT, run).await.expect("the run hung").unwrap();
        (done, events)
    }

    fn file(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.path().join(name)).ok()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.workspace.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn assert_forks_dropped(&self) {
        assert_forks_dropped(self.data.path());
    }
}

/// No fork outlived the run that made it.
fn assert_forks_dropped(data_dir: &Path) {
    let work = data_dir.join("work");
    let left: Vec<_> =
        std::fs::read_dir(&work).unwrap().map(|e| e.unwrap().file_name()).filter(|n| n != ".molt").collect();
    assert!(left.is_empty(), "left in {}: {left:?}", work.display());
    let recovery = work.join(".molt");
    if recovery.exists() {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(recovery).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700, "retained recovery material stays private");
    }
}

/// Every request the fake got was one the real API would have accepted.
async fn assert_requests_valid(server: &MockServer) {
    for (i, req) in server.received_requests().await.unwrap().iter().enumerate() {
        if let Err(problem) = check_request(req) {
            panic!("request {i} would be refused: {problem}");
        }
    }
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server.received_requests().await.unwrap().iter().map(|r| r.body_json().unwrap()).collect()
}

/// Whether a request is memory learning from a run.
fn learns(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_NOTES))
}

fn offers_submit_check(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_CHECK))
}

/// The text of the last message of a request.
fn last_text(body: &Value) -> String {
    let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
    last["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect()
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

async fn wait_for_pid(path: &Path) -> i32 {
    loop {
        if let Some(pid) = read_pid(path) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// True once `pid` has exited (gone, or a zombie nobody reaped yet).
fn dead(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Whether `pid` exits within a few seconds. One that does not is killed,
/// so a failing test leaves nothing running.
async fn exits(pid: i32) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if dead(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // SAFETY: plain syscall.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    false
}

/// `molt do` in `workspace` with state in `data`, using the fake API.
fn molt_do(server: &MockServer, workspace: &Path, data: &Path, args: &[&str]) -> tokio::process::Command {
    let mut all = vec!["do", "Write a greeting into greeting.txt."];
    all.extend(args);
    molt(server, workspace, data, &all)
}

/// `molt ARGS` in `workspace` with state in `data`, using the fake API.
fn molt(server: &MockServer, workspace: &Path, data: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = molt_in(server, workspace, data);
    cmd.args(args).arg("--data-dir").arg(data);
    cmd
}

/// `molt` in `workspace`, using the fake API, with the project registry in `data`.
fn molt_in(server: &MockServer, workspace: &Path, data: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"));
    cmd
        // The project registry, kept out of the developer's own.
        .env("XDG_DATA_HOME", data.join("user-data"))
        // `--workspace` defaults to the current directory.
        .current_dir(workspace)
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env("ANTHROPIC_API_KEY", API_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The developer's own settings must not change the test.
    for var in [
        "ANTHROPIC_AUTH_TOKEN",
        "MOLT_MODEL",
        "MOLT_EFFORT",
        "MOLT_MAX_TOKENS",
        "MOLT_MODEL_TIMEOUT_S",
        "MOLT_MODEL_RETRIES",
        "MOLT_MODEL_CONCURRENCY",
        "MOLT_FALLBACKS",
        "MOLT_PLANNER_MODEL",
        "MOLT_MAX_TURNS",
        "MOLT_MAX_CHECK_ROUNDS",
        "MOLT_BUDGET_USD",
        "MOLT_DESIGN_BUDGET_SHARE",
        "MOLT_EXPLORE",
        "MOLT_EXPLORE_MODEL",
        "MOLT_COMPACT",
        "MOLT_CHECK_TIMEOUT_S",
        "MOLT_MAP_TOKENS",
        "MOLT_CONTEXT_TOKENS",
        "MOLT_INSTRUCTION_FILES",
        "MOLT_SYNTAX_CHECK",
        "MOLT_PROCESSES",
        "MOLT_GIT",
        "MOLT_MEMORY_MODEL",
        "MOLT_MEMORY_EFFORT",
        "MOLT_MEMORY_MAX_TOKENS",
        "MOLT_MEMORY_MODEL_TIMEOUT_S",
        "MOLT_ROLE_MODELS",
        "MOLT_MODELS_FILE",
        "MOLT_ENDPOINTS",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// Start a session on `cfg`'s data dir, waiting out the lock of one just
/// shut down. Another test's child, forked before it execs, can hold a
/// copy of the lock's file descriptor for a moment after the kernel that
/// took it is gone.
async fn start_session(
    cfg: &Config,
    workspace: &str,
    events: tokio::sync::mpsc::UnboundedSender<Progress>,
) -> anyhow::Result<molt::session::Session> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match molt::session::Session::start(cfg, workspace, events.clone()).await {
            Err(e) if format!("{e:#}").contains("is already running on") && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            result => return result,
        }
    }
}

#[tokio::test]
async fn memory_learns_with_the_model_the_run_asked_for() {
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    setup.learn = true;
    let mut req = setup.request(Some(CHECK), 1);
    req.model = Some("haiku".into());
    let (done, _) = setup.run_done(req).await;
    assert_eq!(done.run.outcome, Outcome::Passed, "{:#?}", done.run);
    let learned = done.learned.unwrap().unwrap();
    assert_eq!(learned.added.len(), 1, "{learned:#?}");
    let bodies = bodies(&server).await;
    let models: std::collections::BTreeSet<&str> = bodies.iter().filter_map(|b| b["model"].as_str()).collect();
    assert_eq!(models.into_iter().collect::<Vec<_>>(), ["claude-haiku-4-5"], "attempts and learning alike");
    let learning = bodies.iter().find(|b| learns(b)).expect("memory learned");
    assert!(learning.get("output_config").is_none(), "no effort for a model that takes none, and no output schema");
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn a_given_check_passes_and_the_result_is_applied() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let (resp, events) = setup.run(setup.request(Some(CHECK), 2)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(resp.check, Some(CheckSpec { command: CHECK.into(), files: vec![], designed: false, suite: None }));
    assert!(resp.applied);
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert_eq!(resp.changes.len(), 1, "{resp:#?}");
    assert!(resp.changes.iter().any(|c| c.path == "greeting.txt" && c.kind == ChangeKind::Added), "{resp:#?}");
    assert!(resp.patch.contains("+hello"), "{}", resp.patch);
    assert_eq!(resp.attempts.len(), 2);
    let winner = resp.winner.expect("a winner") as usize;
    assert_eq!(resp.attempts[winner].status, AttemptStatus::Passed);
    assert!(resp.usage.total() > 0);

    let bodies = bodies(&server).await;
    assert!(!bodies.is_empty());
    assert!(bodies.iter().all(|b| b["model"] == MODEL), "the gateway's default model");
    assert!(!bodies.iter().any(offers_submit_check), "a given check needs no designer");
    assert_requests_valid(&server).await;

    assert!(events.iter().any(|e| matches!(e, Progress::AttemptStarted { .. })), "{events:#?}");
    assert!(events.iter().any(|e| matches!(e, Progress::CheckRan { passed: true, .. })), "{events:#?}");
    setup.assert_forks_dropped();

    // The services ran as processes of their own and did the work over the bus.
    let audit = setup.data.path().join("audit.jsonl");
    molt_kernel::audit::verify(&audit).await.unwrap();
    let entries = molt_kernel::audit::read_all(&audit).await.unwrap();
    let mut pids = std::collections::HashMap::new();
    let mut requests = std::collections::HashSet::new();
    for entry in entries {
        match entry.event {
            AuditEvent::ServiceStarted { service, pid: Some(pid) } => {
                pids.insert(service.to_string(), pid);
            }
            AuditEvent::Message { envelope } if envelope.kind == Kind::Request => {
                requests.insert((envelope.from.map(|f| f.to_string()), envelope.to.to_string()));
            }
            _ => {}
        }
    }
    let mut started: Vec<_> = pids.keys().map(String::as_str).collect();
    started.sort_unstable();
    assert_eq!(started, ["fs", "memory", "model", "planner", "shell"]);
    assert!(!pids.values().any(|&pid| pid == std::process::id()));
    for method in [COMPLETE, FORK, WRITE, RUN_CHECK, MERGE, DROP] {
        assert!(requests.contains(&(Some("planner".into()), method.into())), "{method}: {requests:?}");
    }
    assert!(requests.iter().any(|(_, to)| to == PLANNER_RUN), "{requests:?}");
}

#[tokio::test]
async fn build_output_of_the_check_is_left_out_of_the_result_and_the_workspace() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    // A check that builds into a directory of its own in the workspace, as a
    // designer reaching for a writable target directory did, and leaves
    // bytecode behind, with no .gitignore to hide either.
    let check = "mkdir -p target-done-check/debug __pycache__ && echo x > target-done-check/debug/libgreet.rlib \
                 && echo x > __pycache__/greet.cpython-312.pyc && echo built > \"$CARGO_TARGET_DIR/marker\" \
                 && grep -q hello greeting.txt";
    let (resp, _) = setup.run(setup.request(Some(check), 1)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert!(resp.applied, "{resp:#?}");
    let changes: Vec<_> = resp.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(changes, [("greeting.txt", ChangeKind::Added)], "{resp:#?}");
    assert!(!resp.patch.contains("target-done-check"), "{}", resp.patch);
    let left_out = resp.left_out.as_ref().expect("the artifacts are reported");
    assert_eq!(left_out.count, 2);
    assert_eq!(left_out.paths, ["__pycache__/greet.cpython-312.pyc", "target-done-check/debug/libgreet.rlib"]);
    assert_eq!(setup.files(), ["README.txt", "greeting.txt"]);
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert!(molt::agent::report(&resp).contains("left out 2 build artifacts"), "{}", molt::agent::report(&resp));

    // The check's own build output went to a private directory in the data dir.
    let build = setup.data.path().join("work/.molt/build");
    let marker = std::fs::read_dir(&build)
        .unwrap()
        .flat_map(|key| std::fs::read_dir(key.unwrap().path()).unwrap())
        .map(|slot| slot.unwrap().path().join("marker"))
        .find(|marker| marker.is_file())
        .expect("CARGO_TARGET_DIR is writable and kept");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "built\n");
    setup.assert_forks_dropped();
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn a_second_run_starts_with_what_the_first_learned_and_a_map_of_the_code() {
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    setup.learn = true;
    std::fs::create_dir(setup.workspace.path().join("src")).unwrap();
    std::fs::write(setup.workspace.path().join("src/greet.py"), "def greet(name):\n    return 'hello ' + name\n")
        .unwrap();

    let (first, events) = setup.run_done(setup.request(Some(CHECK), 1)).await;
    assert_eq!(first.run.outcome, Outcome::Passed, "{:#?}", first.run);
    let learned = first.learned.expect("memory was asked to learn").expect("memory learned");
    assert_eq!(learned.added.len(), 1, "{learned:#?}");
    let note = &learned.added[0];
    assert_eq!(note.text, LEARNED_TEXT);
    assert_eq!(note.workspace.as_deref(), setup.workspace.path().canonicalize().unwrap().to_str());
    assert!(learned.cost_usd > 0.0 && learned.usage.total() > 0, "{learned:#?}");
    assert!(events
        .iter()
        .any(|e| matches!(e, Progress::Note { message, .. } if message.starts_with("project model: "))));
    // The note names its evidence: messages of the run, as the audit log has them.
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let logged: std::collections::HashSet<String> = audit
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::Message { envelope } => Some(envelope.id.to_string()),
            _ => None,
        })
        .collect();
    assert!(!note.provenance.events.is_empty());
    assert!(note.provenance.events.iter().all(|id| logged.contains(id)), "{:?}", note.provenance);
    assert!(audit.iter().any(|e| matches!(e.event, AuditEvent::AuditRead { .. })), "memory read the run back");
    let asked = bodies(&server).await;
    // With no memory model set, memory learned with the attempts' model: the gateway's default.
    let learning: Vec<&Value> = asked.iter().filter(|b| learns(b)).collect();
    assert_eq!(learning.len(), 1, "one learning call: the tool's input was valid");
    assert_eq!(learning[0]["model"], MODEL);
    assert!(asked.iter().all(|b| b["model"] == MODEL), "every stage used one model");
    let asked = asked.len();

    let (second, _) = setup.run_done(setup.request(Some(CHECK), 1)).await;
    assert_eq!(second.run.outcome, Outcome::Passed, "{:#?}", second.run);
    let bodies = bodies(&server).await;
    let attempt = bodies[asked..].iter().find(|b| b["tools"].is_array()).expect("the second run's attempt");
    let context = attempt["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(context.starts_with("<project_context>"), "{context}");
    assert!(context.contains(&format!("] {LEARNED_TEXT}")), "{context}");
    assert!(context.contains("src/greet.py"), "{context}");
    assert!(context.contains("def greet(name):"), "{context}");
    let tools: Vec<&str> = attempt["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(tools.contains(&"find_symbol") && tools.contains(&"recall"), "{tools:?}");
    // A run like `molt do`'s has no one to answer: its attempts are not offered the question.
    assert!(!tools.contains(&"ask_user"), "{tools:?}");
    // Learning the same thing again confirms the note rather than adding a copy.
    let again = second.learned.unwrap().unwrap();
    assert!(again.added.is_empty() && again.reinforced == vec![note.id.clone()], "{again:#?}");
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let run = audit
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            AuditEvent::Message { envelope }
                if envelope.kind == Kind::Request && envelope.to.to_string() == PLANNER_RUN =>
            {
                Some(envelope.trace_id.to_string())
            }
            _ => None,
        })
        .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["memory", "used", &run, "--json", "--data-dir"])
        .arg(setup.data.path())
        .env("XDG_DATA_HOME", setup.data.path().join("user-data"))
        .current_dir(setup.workspace.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let captured: Vec<molt_api::memory::RecallSnapshot> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].notes[0].note, *note, "later reinforcement must not rewrite this run's context");
    assert_eq!(captured[0].purpose, "context");
    assert!(context.contains(&captured[0].notes[0].note.text));
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn without_a_check_the_designer_writes_one_and_it_is_merged_too() {
    let server = fake_api(Fake::DesignsThenGreets).await;
    let setup = Setup::new(&server);
    let (resp, _) = setup.run(setup.request(None, 2)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(
        resp.check,
        Some(CheckSpec { command: "sh check.sh".into(), files: vec!["check.sh".into()], designed: true, suite: None })
    );
    assert!(resp.applied);
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert_eq!(setup.file("check.sh").as_deref(), Some(format!("{CHECK}\n").as_str()));
    // The designer's two turns: write check.sh, then submit it.
    assert_eq!(bodies(&server).await.iter().filter(|b| offers_submit_check(b)).count(), 2);
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn a_failed_check_goes_back_to_the_attempt_which_fixes_it() {
    let server = fake_api(Fake::FixesAfterFeedback).await;
    let setup = Setup::new(&server);
    let (resp, events) = setup.run(setup.request(Some(CHECK), 1)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(resp.attempts.len(), 1);
    assert_eq!(resp.attempts[0].check_runs, 2, "{resp:#?}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    let checks: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            Progress::CheckRan { passed, .. } => Some(*passed),
            _ => None,
        })
        .collect();
    assert_eq!(checks, [false, true], "{events:#?}");
    let feedback: Vec<String> = bodies(&server)
        .await
        .iter()
        .filter(|b| b["messages"].as_array().is_some_and(|m| m.len() > 1))
        .map(last_text)
        .filter(|t| t.contains(CHECK))
        .collect();
    assert_eq!(feedback.len(), 1, "the failed check goes back to the model once");
    assert!(feedback[0].contains("exit code 1"), "{}", feedback[0]);
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn an_adaptive_run_starts_on_a_cheap_model_and_escalates_with_its_reason_in_the_audit_log() {
    let server = fake_api(Fake::GreetsOn("sonnet")).await;
    let mut setup = Setup::new(&server);
    let planner = setup.cfg.service_mut("planner").unwrap();
    planner.env.insert("MOLT_ESCALATION_MODELS".into(), "haiku,sonnet".into());
    let mut req = setup.request(Some(CHECK), 3);
    req.schedule = Some(Schedule::Adaptive);
    req.max_check_rounds = Some(1);
    // The reviewer has the run's model; only the attempts' are scheduled.
    req.review = false;
    let (resp, _) = setup.run(req).await;
    assert_eq!((resp.outcome, resp.winner), (Outcome::Passed, Some(1)), "{resp:#?}");
    assert_eq!(resp.attempts.len(), 2, "{resp:#?}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    let firsts: Vec<String> = bodies(&server)
        .await
        .iter()
        .filter(|b| b["messages"].as_array().is_some_and(|m| m.len() == 1) && !offers_submit_check(b))
        .filter_map(|b| b["model"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(firsts.len(), 2, "{firsts:?}");
    assert!(firsts[0].contains("haiku") && firsts[1].contains("sonnet"), "{firsts:?}");

    // Each decision is in the audit log, with its reason, price assumption and policy version.
    let audit = setup.data.path().join("audit.jsonl");
    molt_kernel::audit::verify(&audit).await.unwrap();
    let decided: Vec<Value> = molt_kernel::audit::read_all(&audit)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.event {
            AuditEvent::Message { envelope } if envelope.payload["event"] == "scheduled" => Some(envelope.payload),
            _ => None,
        })
        .collect();
    assert_eq!(decided.len(), 2, "{decided:#?}");
    assert_eq!(decided[0]["action"], "start");
    let up = &decided[1];
    assert_eq!((up["action"].as_str(), up["model"].as_str()), (Some("escalate"), Some("sonnet")), "{up:#}");
    assert_eq!(up["policy"], molt_api::planner::ADAPTIVE_POLICY);
    assert!(up["reason"].as_str().unwrap().contains("attempt 1 failed"), "{up:#}");
    assert!(up["price"]["basis"].as_str().unwrap().contains("sonnet has not run"), "{up:#}");
    assert!(up["budget"]["limit_usd"].as_f64().is_some(), "{up:#}");
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn molt_do_prints_a_disputed_check_and_exits_nonzero() {
    let server = fake_api(Fake::Disputes).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let args = ["--check", CHECK, "--attempts", "1", "--no-learn"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args).output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.starts_with("outcome: failed, the check was disputed\n"), "{stdout}");
    assert!(stdout.contains("The done-check was disputed: attempt 1 found that it contradicts the task."), "{stdout}");
    assert!(stdout.contains("The check asserts: the check greps greeting.txt for hello"), "{stdout}");
    assert!(stdout.contains("The task says: \"Write a greeting into greeting.txt.\""), "{stdout}");
    assert!(stdout.contains(&format!("To reproduce: `{CHECK}`")), "{stdout}");
    assert!(stderr.contains("attempt 1 disputed the check"), "{stderr}");
    assert!(!workspace.path().join("greeting.txt").exists());

    // With --json the dispute is a field of its own, and the attempt is not a failed one.
    let args = ["--check", CHECK, "--attempts", "1", "--no-learn", "--json"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args).output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    assert_eq!(out.status.code(), Some(1));
    let resp: RunResponse = serde_json::from_slice(&out.stdout).unwrap();
    let dispute = resp.dispute.expect("the reply carries the dispute");
    assert_eq!((dispute.attempt, dispute.reproduce.as_str()), (0, CHECK));
    assert_eq!(resp.attempts[0].status, AttemptStatus::Cancelled);
    assert_requests_valid(&server).await;
    assert_forks_dropped(data.path());
}

#[tokio::test]
async fn molt_do_given_criteria_designs_nothing_and_a_judge_decides_them() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let args = ["--criterion", "greeting.txt greets the reader", "--attempts", "1", "--no-learn", "--no-review"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args).output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("greeting.txt greets the reader"), "{stdout}");
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
    let bodies = bodies(&server).await;
    assert!(!bodies.iter().any(offers_submit_check), "no model call designs a check");
    let judged = bodies
        .iter()
        .filter(|b| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_JUDGEMENT)));
    assert_eq!(judged.count(), 1);
    // The attempt's two turns and the judge's one: nothing else.
    assert_eq!(bodies.len(), 3);
    assert_requests_valid(&server).await;

    // A criterion with a check or without one makes no sense together.
    for conflicting in [["--check", CHECK], ["--no-check", "--no-learn"]] {
        let mut args = vec!["--criterion", "greets"];
        args.extend(conflicting);
        let out = molt_do(&server, workspace.path(), data.path(), &args).output().await.unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn molt_do_stops_a_designer_that_never_submits_at_its_budget_and_says_what_to_do() {
    let server = fake_api(Fake::NeverSubmits).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // The first call spends more than designing may: the second is never made.
    let args = ["--budget-usd", "1", "--design-budget-usd", "0.000001", "--no-learn"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args).output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("the design budget ran out before the designer submitted a check"), "{stdout}");
    assert!(stdout.contains("of the run's $1.00 budget is left"), "{stdout}");
    for next in ["--check CMD", "--criterion TEXT", "--no-check", "--design-budget-usd USD"] {
        assert!(stdout.contains(next), "{next}: {stdout}");
    }
    let bodies = bodies(&server).await;
    assert_eq!(bodies.len(), 1, "one designer call, and no attempt");
    assert!(offers_submit_check(&bodies[0]));
    assert!(!workspace.path().join("greeting.txt").exists());

    // More for designing than the run has is refused before anything starts.
    let args = ["--budget-usd", "1", "--design-budget-usd", "2", "--no-learn"];
    let out = molt_do(&server, workspace.path(), data.path(), &args).output().await.unwrap();
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("design_budget_usd ($2.00) is more than the run's budget ($1.00)"), "{stderr}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_requests_valid(&server).await;
    assert_forks_dropped(data.path());
}

#[tokio::test]
async fn a_refusal_fails_the_run_and_leaves_the_workspace_alone() {
    let server = fake_api(Fake::Refuses).await;
    let setup = Setup::new(&server);
    let (resp, _) = setup.run(setup.request(Some(CHECK), 2)).await;

    assert_eq!(resp.outcome, Outcome::Failed, "{resp:#?}");
    assert!(!resp.applied);
    assert!(resp.changes.is_empty(), "{resp:#?}");
    assert!(resp.attempts.iter().all(|a| a.status == AttemptStatus::Error), "{resp:#?}");
    assert_eq!(setup.files(), ["README.txt"]);
    assert_eq!(setup.file("README.txt").as_deref(), Some("A test project.\n"));
    // One call per attempt: a refusal is final, not retried.
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn a_base_url_ending_in_v1_fails_with_how_to_fix_it_at_no_cost_and_memory_says_why_it_skipped() {
    let server = MockServer::start().await;
    let not_found = ResponseTemplate::new(404).set_body_string("404 page not found");
    Mock::given(method("POST")).and(path("/v1/v1/messages")).respond_with(not_found).mount(&server).await;
    let mut setup = Setup::new(&server);
    setup.learn = true;
    setup.cfg.service_mut("model").unwrap().env.insert("ANTHROPIC_BASE_URL".into(), format!("{}/v1", server.uri()));
    let (done, _) = setup.run_done(setup.request(Some(CHECK), 1)).await;

    let resp = done.run;
    assert_eq!(resp.outcome, Outcome::Failed, "{resp:#?}");
    let note = &resp.attempts[0].note;
    assert!(note.contains("404") && note.contains("remove the trailing /v1"), "{note}");
    assert_eq!(resp.uncounted_calls, 0, "a refused call was not billed");
    let learned = done.learned.expect("asked to learn").unwrap_err();
    assert_eq!(learned, "skipped: the run failed before any model call was answered");
    assert_eq!(server.received_requests().await.unwrap().len(), 1, "a 404 is not retried, and memory made no call");
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn stopping_a_run_kills_the_commands_it_started() {
    let pids = tempfile::tempdir().unwrap();
    let pidfile = pids.path().join("sleep.pid");
    let server = fake_api(Fake::Hangs(pidfile.clone())).await;
    let mut setup = Setup::new(&server);
    // This legacy fixture writes a host PID marker outside the fork. Its
    // process-group checks explicitly use the unconfined path; namespace
    // lifecycle coverage lives in molt-tools/tests/sandbox.rs.
    setup.cfg.service_mut("shell").unwrap().exec.as_mut().unwrap().args.push("--no-sandbox".into());
    let started = wait_for_pid(&pidfile);
    let run = run_task(&setup.cfg, setup.request(Some(CHECK), 1), None, |_| {}, async {
        started.await;
    });
    let err = tokio::time::timeout(RUN_LIMIT, run).await.expect("the run hung").unwrap_err();
    assert!(err.is::<Interrupted>(), "{err:#}");
    let pid = read_pid(&pidfile).unwrap();
    assert!(exits(pid).await, "the command's child {pid} outlived the run");
    assert_requests_valid(&server).await;
    // The planner was stopped before it could drop the attempt's fork.
    setup.assert_forks_dropped();
}

/// The pid of the latest start of `service` in the audit log at `path`.
async fn started_pid(path: &Path, service: &str) -> i32 {
    loop {
        let entries = molt_kernel::audit::read_all(path).await.unwrap_or_default();
        let pid = entries.iter().rev().find_map(|e| match &e.event {
            AuditEvent::ServiceStarted { service: s, pid: Some(pid) } if s.as_str() == service => Some(*pid),
            _ => None,
        });
        if let Some(pid) = pid {
            return pid as i32;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_planner_that_dies_mid_run_ends_the_run_rather_than_starting_it_again() {
    let pids = tempfile::tempdir().unwrap();
    let pidfile = pids.path().join("sleep.pid");
    let server = fake_api(Fake::Hangs(pidfile.clone())).await;
    let mut setup = Setup::new(&server);
    // This legacy fixture writes a host PID marker outside the fork. Its
    // process-group checks explicitly use the unconfined path; namespace
    // lifecycle coverage lives in molt-tools/tests/sandbox.rs.
    setup.cfg.service_mut("shell").unwrap().exec.as_mut().unwrap().args.push("--no-sandbox".into());
    let kill_planner = async {
        let sleep = wait_for_pid(&pidfile).await;
        let planner = started_pid(&setup.data.path().join("audit.jsonl"), "planner").await;
        // SAFETY: plain syscall.
        assert_eq!(unsafe { libc::kill(planner, libc::SIGKILL) }, 0);
        sleep
    };
    let run = run_task(&setup.cfg, setup.request(Some(CHECK), 1), None, |_| {}, std::future::pending());
    // Sent again, planner.run would start the hanging task over on the restarted planner.
    let (result, sleep) = tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, kill_planner) })
        .await
        .expect("the run went on after its planner died");
    let err = result.unwrap_err();
    assert!(format!("{err:#}").contains("planner.run failed"), "{err:#}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1, "one model call, from the only run");
    assert!(exits(sleep).await, "the command's child {sleep} outlived the run");
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn a_service_that_cannot_start_ends_the_run_at_once_and_says_why() {
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    let model = setup.cfg.service_mut("model").unwrap();
    let exec = model.exec.as_mut().unwrap();
    exec.command = format!("{}/missing-gateway", bin_dir().display());
    model.max_restarts = 1;
    let started = Instant::now();
    let run = run_task(&setup.cfg, setup.request(Some(CHECK), 1), None, |_| {}, std::future::pending());
    let err = tokio::time::timeout(RUN_LIMIT, run).await.expect("the run hung").unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("the model service did not come up: gave up after 1 restarts"), "{err}");
    assert!(err.contains("spawn failed: No such file or directory"), "{err}");
    // Not the whole startup wait.
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}: {err}", started.elapsed());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_project_keeps_its_notes_without_a_folder_and_after_a_move() {
    let server = fake_api(Fake::Greets).await;
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let molt = |args: &[&str], dir: &Path| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
            .args(args)
            .env("XDG_DATA_HOME", data.path().join("user-data"))
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .current_dir(dir)
            .output()
            .unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "molt {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        stdout.into_owned()
    };
    let run = |args: &[&str], dir: &Path| {
        let mut all = vec!["--check", CHECK, "--attempts", "1"];
        all.extend(args);
        let cmd = molt_do(&server, dir, data.path(), &all).output();
        async move {
            let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(out.status.success(), "{stderr}");
            assert!(stderr.contains("memory: learned 1 note"), "{stderr}");
            String::from_utf8(out.stdout).unwrap()
        }
    };
    let show = |args: &[&str], dir: &Path| {
        let mut all = vec!["memory", "show", "--data-dir", data.path().to_str().unwrap()];
        all.extend(args);
        molt(&all, dir)
    };

    // A project with no folder works in a private one, and learns there.
    let listed = molt(&["project", "new", "Japan trip"], home.path());
    assert!(listed.contains("Japan trip  no folder"), "{listed}");
    let id = listed.split_whitespace().next().unwrap().to_owned();
    let files = data.path().join(format!("user-data/molt/projects/{id}/files")).canonicalize().unwrap();
    assert!(listed.contains(&format!("files: {}", files.display())), "{listed}");
    assert_eq!(molt(&["project", "path", "Japan trip"], home.path()), format!("{}\n", files.display()));
    assert_eq!(molt(&["project", "path", &id], home.path()), format!("{}\n", files.display()));
    assert!(molt(&["project", "list"], home.path()).contains(files.to_str().unwrap()));
    let output = run(&["--project", "Japan trip"], home.path()).await;
    assert!(output.contains(&format!("applied to {}", files.display())), "{output}");
    assert_eq!(std::fs::read_to_string(files.join("greeting.txt")).unwrap(), "hello\n");
    assert!(show(&["--project", &id], home.path()).contains(LEARNED_TEXT));

    // A directory becomes a project the first time molt runs in it, and
    // keeps its notes when it moves and molt is told.
    let old = home.path().join("site");
    std::fs::create_dir(&old).unwrap();
    run(&[], &old).await;
    let new = home.path().join("site-renamed");
    std::fs::rename(&old, &new).unwrap();
    let moved = molt(&["project", "move", "site", new.to_str().unwrap()], home.path());
    assert!(moved.contains(new.canonicalize().unwrap().to_str().unwrap()), "{moved}");
    assert!(show(&[], &new).contains(LEARNED_TEXT));
    // Nothing in a project's directory says which project it is.
    assert_eq!(std::fs::read_dir(&new).unwrap().count(), 1, "only the greeting");
}

#[tokio::test]
async fn a_note_remembered_for_every_project_is_recalled_in_another() {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let molt = |args: &[&str], dir: &Path| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
            .args(args)
            .env("XDG_DATA_HOME", home.path().join("user-data"))
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .env_remove("ANTHROPIC_API_KEY")
            .current_dir(dir)
            .output()
            .unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "molt {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        stdout.into_owned()
    };
    // Two projects, each with its own data dir and so its own notes.
    let mut at = Vec::new();
    for name in ["a", "b"] {
        let dir = home.path().join(name);
        let data = home.path().join(format!("data-{name}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        molt(&["project", "new", name, "--path", dir.to_str().unwrap()], home.path());
        at.push((dir, data.to_str().unwrap().to_owned()));
    }
    let memory = |args: &[&str], (dir, data): &(PathBuf, String)| {
        let mut all = vec!["memory"];
        all.extend(args);
        all.extend(["--data-dir", data.as_str()]);
        molt(&all, dir)
    };

    let kept = memory(&["remember", "Build with make."], &at[0]);
    assert!(kept.contains("for this project"), "{kept}");
    let kept = memory(&["remember", "--global", "Answer in Chinese."], &at[0]);
    assert!(kept.contains("for every project"), "{kept}");
    let shown = memory(&["show"], &at[1]);
    assert!(shown.contains("Answer in Chinese."), "{shown}");
    assert!(shown.contains("yours, for every project"), "{shown}");
    assert!(!shown.contains("Build with make."), "{shown}");

    // A project note goes everywhere only when promoted.
    let id = memory(&["show", "build"], &at[0]).split_whitespace().next().unwrap().to_owned();
    memory(&["promote", &id, "--reason", "every project here builds with make"], &at[0]);
    assert!(memory(&["show"], &at[1]).contains("Build with make."));
    assert!(memory(&["show", "build"], &at[0]).contains("promoted as"));
}

#[tokio::test]
async fn a_token_only_cli_run_keeps_bearer_credentials_out_of_sandbox_commands() {
    struct BearerApi;
    impl Respond for BearerApi {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            assert_eq!(req.headers.get("authorization").unwrap(), "Bearer token-test");
            assert!(!req.headers.contains_key("x-api-key"));
            // Reuse the strict body/signature validator with its existing API-key fixture.
            let mut normalized = req.clone();
            normalized.headers.remove("authorization");
            normalized.headers.insert("x-api-key", API_KEY.parse().unwrap());
            check_request(&normalized).expect("valid Messages API request");
            FakeApi(Fake::Greets).respond(&normalized)
        }
    }
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(BearerApi).mount(&server).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let check = format!("test -z \"${{ANTHROPIC_AUTH_TOKEN+x}}\" && {CHECK}");
    let cmd = molt_do(&server, workspace.path(), data.path(), &["--check", &check, "--attempts", "1"])
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("MOLT_PROVIDER")
        .env_remove("MOLT_MODEL_ALIASES")
        .env("ANTHROPIC_AUTH_TOKEN", "token-test")
        .output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
    assert!(!server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn molt_do_says_before_the_run_which_dependencies_commands_cannot_get_and_how_to_fix_it() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let lock = "[[package]]\nname = \"memchr\"\nversion = \"2.7.4\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    std::fs::write(workspace.path().join("Cargo.lock"), lock).unwrap();
    let args = ["--check", CHECK, "--attempts", "1", "--no-memory", "--no-review"];
    // A Cargo home with no cache in it.
    let cargo = tempfile::tempdir().unwrap();
    let cmd = molt_do(&server, workspace.path(), data.path(), &args).env("CARGO_HOME", cargo.path()).output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let notice = stderr.find("dependency preflight: Rust: Cargo.lock lists crates").expect(&stderr);
    assert!(stderr[notice..].lines().next().unwrap().contains("cargo fetch"), "{stderr}");
    assert!(notice < stderr.find("check passed").expect(&stderr), "the notice comes before the run: {stderr}");

    // With the Cargo cache, by default or asked for, there is nothing to say.
    std::fs::create_dir(cargo.path().join("registry")).unwrap();
    for extra in [&[][..], &["--deps-cache", "cargo"][..]] {
        let data = tempfile::tempdir().unwrap();
        std::fs::remove_file(workspace.path().join("greeting.txt")).unwrap();
        let cached = [&args[..], extra].concat();
        let cmd = molt_do(&server, workspace.path(), data.path(), &cached).env("CARGO_HOME", cargo.path()).output();
        let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(!stderr.contains("dependency preflight"), "{extra:?}: {stderr}");
    }
    // Unconfined by choice is not offered a cache.
    let err = molt_do(&server, workspace.path(), data.path(), &["--deps-cache", "cargo", "--no-sandbox"]).output();
    let err = tokio::time::timeout(RUN_LIMIT, err).await.expect("molt do hung").unwrap();
    assert!(!err.status.success());
    assert!(String::from_utf8_lossy(&err.stderr).contains("cannot be used with"), "{err:?}");
}

#[tokio::test]
async fn the_reviewers_calls_go_to_the_model_role_models_name_through_molt_do() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let args = ["--check", CHECK, "--attempts", "1", "--no-memory"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args)
        .env("MOLT_ROLE_MODELS", r#"{"reviewer":"haiku"}"#)
        .output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let bodies = bodies(&server).await;
    let reviews = |b: &&Value| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_REVIEW));
    let (reviewer, others): (Vec<&Value>, Vec<&Value>) = bodies.iter().partition(reviews);
    assert!(!reviewer.is_empty(), "the result was reviewed");
    assert!(reviewer.iter().all(|b| b["model"] == "claude-haiku-4-5"), "{reviewer:?}");
    assert!(!others.is_empty() && others.iter().all(|b| b["model"] == MODEL), "the attempts keep the run's model");
}

/// A named endpoint of the user's: it answers as `inner` does, for its own
/// key only.
struct Keyed<R> {
    key: &'static str,
    inner: R,
}

impl<R: Respond> Respond for Keyed<R> {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if req.headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(self.key) {
            return ResponseTemplate::new(401).set_body_json(error("authentication_error", "invalid x-api-key"));
        }
        let mut req = req.clone();
        req.headers.insert("x-api-key", API_KEY.parse().unwrap());
        self.inner.respond(&req)
    }
}

/// A user config holding `text`, in `home`, for `XDG_CONFIG_HOME`.
fn user_config(home: &Path, text: &str) -> PathBuf {
    let dir = home.join("config");
    std::fs::create_dir_all(dir.join("molt")).unwrap();
    std::fs::write(dir.join("molt/config.toml"), text).unwrap();
    dir
}

/// `[models.endpoints.NAME]` on `server`, with one model.
fn named_endpoint(name: &str, server: &MockServer, key_env: &str, model: &str, extra: &str) -> String {
    format!(
        "[models.endpoints.{name}]\nprotocol = \"anthropic\"\nbase_url = \"{}\"\nkey_env = \"{key_env}\"\n\n\
         [models.endpoints.{name}.models.\"{model}\"]\ntools = true\nstreaming = true\noutput_schema = true\n\
         max_output = 8192\n{extra}\n",
        server.uri()
    )
}

#[tokio::test]
async fn a_reviewer_routed_to_a_named_endpoint_is_called_there_with_its_key_and_nothing_else_gets_it() {
    let server = fake_api(Fake::Greets).await;
    let cheap = MockServer::start().await;
    let keyed = Keyed { key: "cheap-key", inner: FakeApi(Fake::Greets) };
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(keyed).mount(&cheap).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let endpoint =
        named_endpoint("cheap", &cheap, "CHEAP_API_KEY", "claude-haiku-4-5", "price = { input = 1, output = 5 }");
    let config =
        user_config(home.path(), &format!("{endpoint}\n[models.roles]\nreviewer = \"cheap/claude-haiku-4-5\"\n"));
    // The check fails if the endpoint's key reached the commands the agent runs.
    let check = format!("test -z \"${{CHEAP_API_KEY-}}\" && {CHECK}");
    let args = ["--check", &check, "--attempts", "1", "--no-memory"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args)
        .env("XDG_CONFIG_HOME", &config)
        .env("CHEAP_API_KEY", "cheap-key")
        .output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reviews = |b: &Value| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_REVIEW));
    let reviewer = cheap.received_requests().await.unwrap();
    assert!(!reviewer.is_empty(), "the result was reviewed on the named endpoint");
    for req in &reviewer {
        let body: Value = req.body_json().unwrap();
        assert!(reviews(&body) && body["model"] == "claude-haiku-4-5", "{body}");
        assert_eq!(req.headers["x-api-key"].to_str().unwrap(), "cheap-key");
    }
    let others = server.received_requests().await.unwrap();
    assert!(!others.is_empty());
    for req in &others {
        let body: Value = req.body_json().unwrap();
        assert!(!reviews(&body) && body["model"] == MODEL, "the attempts keep the run's model: {body}");
        assert!(!String::from_utf8_lossy(&req.body).contains("cheap-key") && req.headers["x-api-key"] == API_KEY);
    }
}

#[tokio::test]
async fn attempt_explorer_and_reviewer_on_three_named_endpoints_are_each_billed_and_counted_where_they_ran() {
    // The run's default endpoint is left unused: every role goes to one of the user's.
    let unused = fake_api(Fake::Greets).await;
    let explores = Fake::GreetsWithCalls(vec![("explore", json!({ "question": "Where does the greeting go?" }))]);
    let mut config = String::new();
    let mut servers = Vec::new();
    // Name, key, model and price per million input and output tokens of each endpoint.
    for (name, key, model, price) in [
        ("main", "main-key", MODEL, (4.0, 20.0)),
        ("scout", "scout-key", "claude-haiku-4-5", (1.0, 5.0)),
        ("judge", "judge-key", "claude-sonnet-5-5", (2.0, 10.0)),
    ] {
        let server = MockServer::start().await;
        let keyed = Keyed { key, inner: FakeApi(explores.clone()) };
        Mock::given(method("POST")).and(path("/v1/messages")).respond_with(keyed).mount(&server).await;
        let price = format!("price = {{ input = {}, output = {} }}", price.0, price.1);
        config += &named_endpoint(name, &server, &format!("{}_API_KEY", name.to_uppercase()), model, &price);
        servers.push((name, key, model, server));
    }
    config += "\n[models.roles]\nexplorer = \"scout/claude-haiku-4-5\"\nreviewer = \"judge/claude-sonnet-5-5\"\n";
    let (workspace, data, home) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let model = format!("main/{MODEL}");
    let args = ["--check", CHECK, "--attempts", "1", "--no-memory", "--json", "--model", &model];
    let mut cmd = molt_do(&unused, workspace.path(), data.path(), &args);
    cmd.env("XDG_CONFIG_HOME", user_config(home.path(), &config)).env("MOLT_EXPLORE", "on");
    for (name, key, ..) in &servers {
        cmd.env(format!("{}_API_KEY", name.to_uppercase()), key);
    }
    let out = tokio::time::timeout(RUN_LIMIT, cmd.output()).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["outcome"], "passed");
    assert!(unused.received_requests().await.unwrap().is_empty(), "no call went to the default endpoint");

    // Each role's calls reached its own endpoint, with that endpoint's key and model.
    let offers =
        |body: &Value, tool: &str| body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == tool));
    let mut sent = std::collections::BTreeMap::new();
    for (name, key, model, server) in &servers {
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty(), "{name} was called");
        for req in &requests {
            let body: Value = req.body_json().unwrap();
            assert_eq!((body["model"].as_str(), req.headers["x-api-key"].to_str().unwrap()), (Some(*model), *key));
            let role = match (offers(&body, "explore"), offers(&body, SUBMIT_REVIEW)) {
                (true, _) => "main",
                (_, true) => "judge",
                _ => "scout",
            };
            assert_eq!(role, *name, "{body}");
        }
        sent.insert(name.to_string(), requests.len());
    }

    // And the audit log says, of each call, where it went and what it cost there.
    let audit = molt_kernel::audit::read_all(&data.path().join("audit.jsonl")).await.unwrap();
    let envelopes: Vec<&molt_proto::Envelope> = audit
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::Message { envelope } => Some(envelope),
            _ => None,
        })
        .collect();
    let roles: std::collections::HashMap<String, String> = envelopes
        .iter()
        .filter(|e| e.kind == Kind::Request && e.to.to_string() == COMPLETE)
        .map(|e| (e.id.to_string(), e.payload["role"].as_str().unwrap_or("").to_owned()))
        .collect();
    let mut spent: std::collections::BTreeMap<String, (String, usize, u64, u64, f64)> = Default::default();
    for reply in envelopes.iter().filter(|e| e.kind == Kind::Reply) {
        let Some(role) = reply.reply_to.as_ref().and_then(|id| roles.get(id.as_str())) else { continue };
        let endpoint = reply.payload["endpoint"].as_str().expect("each reply names its endpoint").to_owned();
        let entry = spent.entry(role.clone()).or_insert((endpoint.clone(), 0, 0, 0, 0.0));
        assert_eq!(entry.0, endpoint, "a role's calls all went to one endpoint");
        entry.1 += 1;
        entry.2 += reply.payload["usage"]["input_tokens"].as_u64().unwrap();
        entry.3 += reply.payload["usage"]["output_tokens"].as_u64().unwrap();
        entry.4 += reply.payload["cost_usd"].as_f64().expect("priced as its endpoint declares");
        assert!(reply.payload["usage"].get("reasoning_tokens").is_none(), "the Messages API does not count them");
    }
    let roles: Vec<&str> = spent.keys().map(String::as_str).collect();
    assert_eq!(roles, ["attempt", "explorer", "reviewer"]);
    // Every call is 1000 tokens in and 100 out, priced per million as its endpoint declares.
    for (role, endpoint, per_call) in
        [("attempt", "main", 0.006), ("explorer", "scout", 0.0015), ("reviewer", "judge", 0.003)]
    {
        let (at, calls, input, output, cost) = &spent[role];
        assert_eq!((at.as_str(), *calls), (endpoint, sent[endpoint]), "{role}");
        assert_eq!((*input, *output), (1000 * *calls as u64, 100 * *calls as u64), "{role}");
        assert!((cost - per_call * *calls as f64).abs() < 1e-9, "{role}: {cost}");
    }
}

#[tokio::test]
async fn molt_models_probe_reaches_a_named_endpoint_of_the_user_config_by_name() {
    let server = MockServer::start().await;
    let keyed = Keyed { key: "glm-key", inner: ThirdParty };
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(keyed).mount(&server).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let endpoint = named_endpoint("glm", &server, "GLM_API_KEY", "glm-4.6", "efforts = [\"low\"]\ncontext = 200000");
    let config = user_config(home.path(), &endpoint);
    let molt = |args: &[&str]| {
        let mut cmd = molt_in(&server, workspace.path(), data.path());
        cmd.args(args).env("XDG_CONFIG_HOME", &config).env("GLM_API_KEY", "glm-key");
        cmd.output()
    };
    let out =
        tokio::time::timeout(RUN_LIMIT, molt(&["models", "probe", "glm/glm-4.6", "--yes"])).await.unwrap().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stderr.contains(&format!("Probing glm/glm-4.6 at {}", server.uri())), "{stderr}");
    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent.len(), 4);
    assert!(sent.iter().all(|r| r.headers["x-api-key"] == "glm-key"), "each call has the endpoint's own key");
    let records = molt_gateway::probe::Records::load(&data.path().join("user-data/molt/models.json")).unwrap();
    let p = &records.models[0];
    assert_eq!((p.model.as_str(), p.base_url.as_str()), ("glm-4.6", server.uri().as_str()));
    assert_eq!((p.output_schema, p.effort), (Some(false), Some(false)));

    let out = molt(&["models", "show"]).await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("glm: anthropic at {}, key in GLM_API_KEY (set): glm/glm-4.6", server.uri())),
        "{stdout}"
    );
    let out = molt(&["models", "show", "glm/glm-4.6"]).await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("glm-4.6 on endpoint glm at") && stdout.contains("a context of 200000 tokens"), "{stdout}");
    assert!(stdout.contains("probed: tools yes, streaming yes, output_schema no, effort no"), "{stdout}");
    assert_eq!(server.received_requests().await.unwrap().len(), 4, "show calls nothing");
}

/// An Anthropic-compatible endpoint that calls tools and streams, but
/// ignores `output_config.format` and refuses `output_config.effort`.
struct ThirdParty;

impl Respond for ThirdParty {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = req.body_json().unwrap();
        if body.pointer("/output_config/effort").is_some() {
            return ResponseTemplate::new(400).set_body_json(error("invalid_request_error", "unknown field: effort"));
        }
        let content = match body["tools"].as_array().and_then(|t| t.first()) {
            Some(tool) => {
                vec![json!({ "type": "tool_use", "id": "toolu_1", "name": tool["name"], "input": { "n": 7 } })]
            }
            None => vec![json!({ "type": "text", "text": "2 + 3 is 5." })],
        };
        message(1, "glm-4.6", content, "end_turn", body["stream"] == true)
    }
}

#[tokio::test]
async fn molt_models_probe_asks_first_then_records_what_the_endpoint_does_and_show_reads_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ThirdParty).mount(&server).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let probe = |extra: &[&str]| {
        let mut cmd = molt_in(&server, workspace.path(), data.path());
        cmd.args(["models", "probe", "glm-4.6"]).args(extra);
        cmd.output()
    };

    // Not at a terminal and without --yes: it says what it would call, and calls nothing.
    let out = tokio::time::timeout(RUN_LIMIT, probe(&[])).await.unwrap().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("makes up to 4 calls") && stderr.contains("--yes"), "{stderr}");
    assert!(stderr.contains(&server.uri()), "it names the endpoint: {stderr}");
    assert!(server.received_requests().await.unwrap().is_empty());
    let file = data.path().join("user-data/molt/models.json");
    assert!(!file.exists());

    let out = tokio::time::timeout(RUN_LIMIT, probe(&["--yes"])).await.unwrap().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    assert!(stdout.contains("output_schema): no"), "{stdout}");
    let records = molt_gateway::probe::Records::load(&file).unwrap();
    let p = &records.models[0];
    assert_eq!((p.model.as_str(), p.base_url.as_str()), ("glm-4.6", server.uri().as_str()));
    assert_eq!((p.tools, p.streaming, p.output_schema, p.effort), (Some(true), Some(true), Some(false), Some(false)));

    let mut show = molt_in(&server, workspace.path(), data.path());
    let out = show.args(["models", "show", "glm-4.6"]).output().await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("probed: tools yes, streaming yes, output_schema no, effort no"), "{stdout}");
    assert_eq!(server.received_requests().await.unwrap().len(), 4, "show calls nothing");
}

#[tokio::test]
async fn the_cli_carries_out_a_task() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    // A project's own molt.toml is never read unless asked for: it could run anything with the user's key.
    std::fs::write(workspace.path().join("molt.toml"), "[[service]]\nname = \"model\"\nexec = 'not toml\n").unwrap();
    // The check fails if the API key reached the commands the agent runs, or the passed variable did not.
    let check = format!("test -z \"${{ANTHROPIC_API_KEY-}}\" && test \"$PROJECT_SETTING\" = on && {CHECK}");
    let args = ["--check", &check, "--attempts", "1", "--pass-env", "PROJECT_SETTING"];
    let cmd = molt_do(&server, workspace.path(), data.path(), &args).env("PROJECT_SETTING", "on").output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
    assert!(stdout.contains("outcome: passed"), "{stdout}");
    assert!(stdout.contains("added    greeting.txt"), "{stdout}");
    assert!(stderr.contains("check passed"), "{stderr}");
    assert!(!stderr.contains("config:"), "{stderr}");
    // Memory learned from the run, and says so.
    assert!(stderr.contains("memory: learned 1 note; cost $"), "{stderr}");
    assert!(stderr.contains(&format!("  + [lesson] {LEARNED_TEXT}")), "{stderr}");
    assert!(data.path().join("memory.db").is_file());

    // What memory holds can be seen and corrected from the CLI.
    let memory = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
            .arg("memory")
            .args(args)
            .arg("--data-dir")
            .arg(data.path())
            .env("XDG_DATA_HOME", data.path().join("user-data"))
            .current_dir(workspace.path())
            .output()
            .unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "molt memory {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        (stdout.into_owned(), stderr.into_owned())
    };
    let (shown, _) = memory(&["show"]);
    assert!(shown.contains(&format!("lesson 0.90\n    {LEARNED_TEXT}\n")), "{shown}");
    let id = shown.split_whitespace().next().unwrap().to_owned();
    assert!(id.starts_with("note_"), "{shown}");
    let matched = memory(&["show", "greps", "hello"]).0;
    assert!(matched.starts_with(&id) && matched.contains(LEARNED_TEXT), "{matched}");
    assert!(matched.contains("keywords \"greps\" OR \"hello\""), "{matched}");
    let reviewed = memory(&[
        "review",
        &id,
        "--rev",
        "1",
        "--reason",
        "temporary check",
        "--depends-on",
        "greeting.txt",
        "--temporary",
    ])
    .0;
    assert!(reviewed.contains("revision 2"), "{reviewed}");
    let shown = memory(&["show"]).0;
    assert!(shown.contains("temporary experience") && shown.contains("depends on: greeting.txt"), "{shown}");
    let corrected = memory(&[
        "correct",
        &id,
        "The greeting check requires a newline.",
        "--rev",
        "2",
        "--reason",
        "clarified the requirement",
        "--depends-on",
        "greeting.txt",
    ])
    .0;
    let corrected_id = corrected.split_whitespace().next().unwrap();
    assert_ne!(corrected_id, id);
    let (forgot, _) = memory(&["forget", corrected_id, "--reason", "the check changed"]);
    assert_eq!(forgot, format!("forgot {corrected_id}\n"));
    assert_eq!(memory(&["show"]), (String::new(), "no notes\n".to_owned()));
    // The forget went through the bus, so the audit log has it.
    let audit = molt_kernel::audit::read_all(&data.path().join("audit.jsonl")).await.unwrap();
    assert!(audit.iter().any(|e| matches!(&e.event, AuditEvent::Message { envelope }
        if envelope.to.to_string() == "memory.forget" && envelope.payload["reason"] == "the check changed")));
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
    assert!(data.path().join("audit.jsonl").exists());
    assert_requests_valid(&server).await;
    assert_forks_dropped(data.path());
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn a_committed_run_can_be_undone_and_memory_stops_counting_it() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let w = workspace.path();
    git(w, &["init", "-q", "-b", "main"]);
    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
        git(w, &["config", key, value]);
    }
    std::fs::write(w.join("notes.txt"), "draft\n").unwrap();
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "start"]);
    // The user's own change, staged before the run, stays out of its commit.
    std::fs::write(w.join("notes.txt"), "mine\n").unwrap();
    git(w, &["add", "notes.txt"]);

    let args = ["--check", CHECK, "--attempts", "1", "--commit"];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, w, data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
    assert!(stderr.contains("committed ") && stderr.contains("on branch main"), "{stderr}");
    assert_eq!(git(w, &["show", "--name-only", "--format=", "HEAD"]), "greeting.txt\n");
    let run = git(w, &["log", "-1", "--format=%(trailers:key=Molt-Run,valueonly)"]).trim().to_owned();
    assert!(run.starts_with("trace_"), "{run}");
    assert!(git(w, &["log", "-1", "--format=%B"]).contains(&format!("Molt-Check: {CHECK}")));
    assert_eq!(git(w, &["diff", "--cached", "--name-only"]), "notes.txt\n");
    let memory = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
            .args(["memory", "show", "--data-dir"])
            .arg(data.path())
            .env("XDG_DATA_HOME", data.path().join("user-data"))
            .current_dir(w)
            .output()
            .unwrap();
        assert!(out.status.success(), "memory show: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    };
    assert!(memory().contains(LEARNED_TEXT));

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["undo", "--data-dir"])
        .arg(data.path())
        .env("XDG_DATA_HOME", data.path().join("user-data"))
        .current_dir(w)
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.starts_with(&format!("undid run {run}: reverted ")), "{stdout}");
    assert!(stderr.contains("memory no longer counts the run: 1 notes only it showed forgotten"), "{stderr}");
    assert!(!w.join("greeting.txt").exists());
    assert!(git(w, &["log", "-1", "--format=%B"]).contains(&format!("Molt-Undo: {run}")));
    assert_eq!(git(w, &["diff", "--cached", "--name-only"]), "notes.txt\n");
    assert_eq!(memory(), "no notes\n");
    // The memory service did the undo, so the audit log has it.
    let audit = molt_kernel::audit::read_all(&data.path().join("audit.jsonl")).await.unwrap();
    assert!(audit.iter().any(|e| matches!(&e.event, AuditEvent::Message { envelope }
        if envelope.to.to_string() == "memory.undo" && envelope.payload["run"] == run.as_str())));
}

#[tokio::test]
async fn commit_outside_git_still_applies_and_exits_4() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let args = ["--check", CHECK, "--attempts", "1", "--commit", "--no-learn"];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, workspace.path(), data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("is not in a git work tree; the result will be applied but not committed"), "{stderr}");
    assert!(stderr.contains("not committed: --commit: "), "{stderr}");
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
}

#[tokio::test]
async fn allowed_web_hosts_start_the_web_service_and_offer_fetch_url_only_then() {
    let server = fake_api(Fake::Greets).await;
    let fetch_offered =
        |body: &Value| body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == "fetch_url"));
    for allow in [None, Some("docs.rs")] {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut args = vec!["--check", CHECK, "--attempts", "1", "--no-memory", "--no-review"];
        args.extend(allow.iter().flat_map(|host| ["--allow-web", *host]));
        let before = bodies(&server).await.len();
        let cmd = molt_do(&server, workspace.path(), data.path(), &args).output();
        let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
        let sent = bodies(&server).await;
        assert!(sent[before..].iter().all(|b| fetch_offered(b) == allow.is_some()), "{allow:?}");
        let audit = molt_kernel::audit::read_all(&data.path().join("audit.jsonl")).await.unwrap();
        let web_started = audit
            .iter()
            .any(|e| matches!(&e.event, AuditEvent::ServiceStarted { service, .. } if service.as_str() == "web"));
        assert_eq!(web_started, allow.is_some());
    }
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn streamed_calls_keep_tools_signatures_and_parallel_call_identity() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let mut req = setup.request(Some(CHECK), 2);
    req.stream = true;
    let (response, events) = setup.run(req).await;
    assert_eq!(response.outcome, Outcome::Passed);
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    let mut calls = std::collections::HashMap::new();
    let mut saw_preview = false;
    for event in &events {
        if let Some(context) = event.model_call() {
            let key = (context.run.clone(), context.attempt, context.turn);
            let previous = calls.entry(context.call.clone()).or_insert((key.clone(), None));
            assert_eq!(previous.0, key);
            assert!(previous.1.is_none_or(|seq| context.seq > seq));
            previous.1 = Some(context.seq);
        }
        if let Progress::ModelText { text, .. } = event {
            assert_eq!(text, "Wrote greeting.txt.");
            saw_preview = true;
        }
    }
    assert!(saw_preview, "{events:?}");
    assert!(calls.len() >= 2);
    assert_requests_valid(&server).await;
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    assert!(audit.iter().any(|e| matches!(&e.event, AuditEvent::Message { envelope }
        if envelope.kind == Kind::Event && envelope.payload["event"] == "model_text"
        && envelope.payload["run"] == envelope.trace_id.as_str())));
}

#[tokio::test]
async fn streaming_keeps_json_stdout_complete_and_previews_on_stderr() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let out = tokio::time::timeout(
        RUN_LIMIT,
        molt_do(
            &server,
            workspace.path(),
            data.path(),
            &["--check", CHECK, "--attempts", "1", "--stream", "--json", "--no-learn"],
        )
        .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["outcome"], "passed");
    assert_eq!(result["summary"], "Wrote greeting.txt.");
    assert_eq!(result["workspace"], workspace.path().canonicalize().unwrap().to_str().unwrap());
    assert!(stderr.contains("preview: Wrote greeting.txt."), "{stderr}");
    assert!(stderr.contains("model finished (cost $"), "{stderr}");
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn the_cli_runs_the_example_config_when_given_one() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let example = include_str!("../../../molt.example.toml");
    assert!(example.contains(EXAMPLE_SCRATCH));
    let config = example
        .replace("command = \"molt-", &format!("command = \"{}/molt-", bin_dir().display()))
        .replace(EXAMPLE_SCRATCH, data.path().join("work").to_str().unwrap());
    let path = dir.path().join("molt.toml");
    std::fs::write(&path, config).unwrap();
    let args = ["--check", CHECK, "--attempts", "1", "--config", path.to_str().unwrap()];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, workspace.path(), data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
    assert!(stderr.contains(&format!("config: {}\n", path.display())), "{stderr}");
    assert!(stdout.contains(&format!("applied to {}", workspace.path().canonicalize().unwrap().display())), "{stdout}");
    assert!(stderr.contains("memory: learned 1 note"), "{stderr}");
    assert!(workspace.path().join(".molt/memory.db").is_file(), "where the example puts it");
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
    assert_requests_valid(&server).await;
    assert_forks_dropped(data.path());
}

#[tokio::test]
async fn the_verification_level_is_in_the_json_and_the_commit_trailers() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let w = workspace.path();
    git(w, &["init", "-q", "-b", "main"]);
    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
        git(w, &["config", key, value]);
    }
    std::fs::write(w.join("notes.txt"), "draft\n").unwrap();
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "start"]);
    let args = ["--check", CHECK, "--attempts", "1", "--commit", "--json", "--no-learn"];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, w, data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "status {:?}\nstderr:\n{stderr}", out.status);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["verification"], "command", "{json:#}");
    let level = git(w, &["log", "-1", "--format=%(trailers:key=Molt-Verification,valueonly)"]);
    assert_eq!(level.trim(), "command");
    let run = git(w, &["log", "-1", "--format=%(trailers:key=Molt-Run,valueonly)"]);
    assert!(run.trim().starts_with("trace_"), "{run}");
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn a_result_that_cannot_be_applied_exits_3_and_is_kept() {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let original = workspace.path().canonicalize().unwrap().join("greeting.txt");
    let server = fake_api(Fake::ConcurrentEdit(original.clone())).await;
    let args = ["--check", CHECK, "--attempts", "1", "--json"];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, workspace.path(), data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{stderr}");
    let resp: RunResponse = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert!(!resp.applied);
    assert!(resp.summary.contains("not applied"), "{}", resp.summary);
    let fork = PathBuf::from(resp.fork.expect("the result is kept"));
    assert_eq!(std::fs::read_to_string(fork.join("greeting.txt")).unwrap(), "hello\n");
    assert_eq!(std::fs::read_to_string(&original).unwrap(), "theirs\n");
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn molt_apply_checks_the_chosen_files_or_a_hand_edit_again_and_applies_only_those() {
    let server = fake_api(Fake::GreetsWithNotes).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let w = workspace.path();
    let args = ["--check", CHECK, "--attempts", "1", "--no-apply", "--json", "--no-learn"];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, w, data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let resp: RunResponse = serde_json::from_slice(&out.stdout).unwrap();
    let paths: Vec<&str> = resp.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["greeting.txt", "notes.txt"]);
    let fork = resp.fork.expect("the result is kept");
    let apply = |args: &[&str]| {
        let mut all = vec!["apply", fork.as_str()];
        all.extend(args);
        let mut cmd = molt(&server, w, data.path(), &all);
        async move { tokio::time::timeout(RUN_LIMIT, cmd.output()).await.expect("molt apply hung").unwrap() }
    };

    // Part of a result is not what was checked: the CLI wants a check, or to be told there is none.
    let out = apply(&["--only", "greeting.txt"]).await;
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--no-check"));

    // The check runs on the chosen files alone: without greeting.txt it fails, and nothing is applied.
    let out = apply(&["--only", "notes.txt", "--check", CHECK]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("the check failed on the 1 chosen files"), "{stderr}");
    assert!(!w.join("notes.txt").exists() && !w.join("greeting.txt").exists());

    // A hand edit to the fork is checked again too.
    std::fs::write(Path::new(&fork).join("greeting.txt"), "goodbye\n").unwrap();
    let out = apply(&["--check", CHECK]).await;
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!w.join("greeting.txt").exists());
    std::fs::write(Path::new(&fork).join("greeting.txt"), "hello, edited\n").unwrap();

    let out = apply(&["--only", "greeting.txt", "--check", CHECK]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("the check passed on the 1 chosen files"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "applied 1 files: greeting.txt\n");
    assert_eq!(std::fs::read_to_string(w.join("greeting.txt")).unwrap(), "hello, edited\n");
    assert!(!w.join("notes.txt").exists(), "the file not chosen is not applied");
    assert!(!Path::new(&fork).exists(), "the fork is dropped once part of it is applied");

    // What was chosen is in the audit log, with the merge that applied it.
    let audit = molt_kernel::audit::read_all(&data.path().join("audit.jsonl")).await.unwrap();
    assert!(audit.iter().any(|e| matches!(&e.event, AuditEvent::Message { envelope }
        if envelope.to.to_string() == MERGE && envelope.payload["paths"] == json!(["greeting.txt"]))));
    assert_requests_valid(&server).await;
    assert_forks_dropped(data.path());
}

#[tokio::test]
async fn ctrl_c_stops_molt_do_and_the_commands_it_started() {
    let pids = tempfile::tempdir().unwrap();
    let pidfile = pids.path().join("sleep.pid");
    let server = fake_api(Fake::Hangs(pidfile.clone())).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut cmd =
        molt_do(&server, workspace.path(), data.path(), &["--check", CHECK, "--attempts", "1", "--no-sandbox"]);
    // A process group of its own, as a terminal gives a command, so the
    // signal below reaches molt and its services but not this test.
    cmd.process_group(0);
    let child = cmd.spawn().unwrap();
    let group = child.id().unwrap() as i32;
    let pid = tokio::time::timeout(RUN_LIMIT, wait_for_pid(&pidfile)).await.expect("the command never started");

    // What Ctrl-C in a terminal does: SIGINT to the whole foreground process group.
    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::killpg(group, libc::SIGINT) }, 0);
    let out = tokio::time::timeout(RUN_LIMIT, child.wait_with_output()).await.expect("molt do hung").unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{stderr}");
    assert!(stderr.contains("interrupted"), "{stderr}");
    assert!(exits(pid).await, "the command's child {pid} outlived molt do");
    assert_forks_dropped(data.path());
}

#[tokio::test]
async fn sigterm_and_sighup_stop_molt_do_like_ctrl_c() {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let pids = tempfile::tempdir().unwrap();
        let pidfile = pids.path().join("sleep.pid");
        let server = fake_api(Fake::Hangs(pidfile.clone())).await;
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let child =
            molt_do(&server, workspace.path(), data.path(), &["--check", CHECK, "--attempts", "1", "--no-sandbox"])
                .spawn()
                .unwrap();
        let molt = child.id().unwrap() as i32;
        let pid = tokio::time::timeout(RUN_LIMIT, wait_for_pid(&pidfile)).await.expect("the command never started");

        // To molt alone, as `kill`, `timeout` or a CI runner sends it.
        // SAFETY: plain syscall.
        assert_eq!(unsafe { libc::kill(molt, signal) }, 0);
        let out = tokio::time::timeout(RUN_LIMIT, child.wait_with_output()).await.expect("molt do hung").unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(128 + signal), "signal {signal}: {stderr}");
        assert!(stderr.contains("interrupted"), "{stderr}");
        assert!(exits(pid).await, "the command's child {pid} outlived molt do");
        assert_forks_dropped(data.path());
    }
}

/// `molt runs` lists what `molt do` ran, from the project's own data dir:
/// its result and cost as `molt do --json` said them, an interrupted run as
/// interrupted, the runs of a session together, and the same list again
/// once its index is deleted.
#[tokio::test]
async fn molt_runs_lists_the_projects_runs_by_session_and_builds_the_same_list_again_from_the_log() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // No --data-dir anywhere: each command finds the project's data dir in the cache.
    let cache = home.path().join("cache");
    let molt_here = |server: &MockServer, args: &[&str]| {
        let mut cmd = molt_in(server, workspace.path(), home.path());
        cmd.env("XDG_CACHE_HOME", &cache).args(args);
        cmd
    };
    let run = |out: std::process::Output| {
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
        (stdout.into_owned(), stderr.into_owned())
    };
    let task = "Write a greeting into greeting.txt.";
    let mut done = Vec::new();
    for extra in [&["--no-apply"][..], &[]] {
        let mut args = vec!["do", task, "--check", CHECK, "--attempts", "1", "--session", "greeting", "--json"];
        args.extend(extra);
        let out = tokio::time::timeout(RUN_LIMIT, molt_here(&server, &args).output()).await.expect("molt do hung");
        done.push(serde_json::from_str::<Value>(&run(out.unwrap()).0).unwrap());
    }
    // A third run, outside the session, stopped while its attempt waits on a command.
    let pids = tempfile::tempdir().unwrap();
    let pidfile = pids.path().join("sleep.pid");
    let hangs = fake_api(Fake::Hangs(pidfile.clone())).await;
    let args = ["do", "Wait for it.", "--check", CHECK, "--attempts", "1", "--no-sandbox"];
    let child = molt_here(&hangs, &args).spawn().unwrap();
    let pid = tokio::time::timeout(RUN_LIMIT, wait_for_pid(&pidfile)).await.expect("the command never started");
    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) }, 0);
    let out = tokio::time::timeout(RUN_LIMIT, child.wait_with_output()).await.expect("molt do hung").unwrap();
    assert_eq!(out.status.code(), Some(143), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(exits(pid).await);

    let (json, _) = run(molt_here(&server, &["runs", "--json"]).output().await.unwrap());
    let listed: Value = serde_json::from_str(&json).unwrap();
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 3, "{json}");
    for (run, done) in listed.iter().zip(&done) {
        assert_eq!(run["run"], done["run"], "{json}");
        assert_eq!(run["result"], done["outcome"], "{json}");
        assert_eq!(run["cost_usd"], done["cost_usd"], "{json}");
        assert!(run["cost_usd"].as_f64().unwrap() > 0.0, "{json}");
        assert_eq!((&run["task"], &run["session"]), (&json!(task), &json!("greeting")));
        assert_eq!(run["learning"]["status"], "answered", "{json}");
        assert!(run["duration_ms"].as_u64().is_some(), "{json}");
    }
    assert_eq!(listed[0]["learning"]["notes"][0]["text"], LEARNED_TEXT, "{json}");
    assert_eq!(listed[1]["learning"]["reinforced"], 1, "the second run bore the note out: {json}");
    assert_eq!((&listed[0]["applied"], &listed[0]["fork"]), (&json!("kept"), &done[0]["fork"]));
    assert_eq!(listed[1]["applied"], "workspace");
    // Stopped, it is interrupted, not failed; the planner still said what it had spent.
    let stopped = &listed[2];
    assert_eq!((&stopped["result"], &stopped["task"]), (&json!("interrupted"), &json!("Wait for it.")), "{json}");
    assert!(stopped["cost_usd"].as_f64().is_some_and(|c| c > 0.0) && stopped.get("session").is_none(), "{stopped}");

    let (table, _) = run(molt_here(&server, &["runs"]).output().await.unwrap());
    let rows: Vec<&str> = table.lines().collect();
    assert_eq!(rows.len(), 4, "{table}");
    assert!(rows[0].starts_with("RUN ") && rows[1].contains("passed") && rows[1].contains("kept"), "{table}");
    assert!(rows[1].contains("1 note") && rows[2].contains("workspace") && rows[2].contains("nothing"), "{table}");
    assert!(rows[3].contains("interrupted") && rows[3].contains("Wait for it."), "{table}");

    // Deleted, the index is built again from the audit log, to the same list.
    let data: Vec<PathBuf> =
        std::fs::read_dir(cache.join("molt")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    let [data] = data.as_slice() else { panic!("one data dir: {data:?}") };
    std::fs::remove_file(data.join(molt::runs::INDEX)).unwrap();
    assert_eq!(run(molt_here(&server, &["runs", "--json"]).output().await.unwrap()).0, json);
    assert!(data.join(molt::runs::INDEX).is_file());

    // By session: the session's two runs together, then the one outside any.
    let (grouped, _) = run(molt_here(&server, &["runs", "--session", "--json"]).output().await.unwrap());
    let grouped: Value = serde_json::from_str(&grouped).unwrap();
    let shape: Vec<(Value, usize)> = grouped
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["session"].clone(), s["runs"].as_array().unwrap().len()))
        .collect();
    assert_eq!(shape, [(json!("greeting"), 2), (Value::Null, 1)]);
    let total = done[0]["cost_usd"].as_f64().unwrap() + done[1]["cost_usd"].as_f64().unwrap();
    assert!((grouped[0]["cost_usd"].as_f64().unwrap() - total).abs() < 1e-12, "{grouped}");
    let (named, _) = run(molt_here(&server, &["runs", "--session", "greeting"]).output().await.unwrap());
    assert_eq!(named.lines().count(), 3, "{named}");
    let (text, _) = run(molt_here(&server, &["runs", "--session"]).output().await.unwrap());
    assert!(text.starts_with("session greeting · 2 runs · $") && text.contains("\nno session · 1 run"), "{text}");

    // One run in full, by the start of its id.
    let id = done[1]["run"].as_str().unwrap();
    let (shown, _) = run(molt_here(&server, &["runs", "show", &id[..12]]).output().await.unwrap());
    for part in [
        &format!("run {id}\n"),
        &format!("task:\n  {task}\n"),
        "result: passed (verification: command), cost $",
        &format!("check: {CHECK}\n"),
        "attempts:\n  1: passed after ",
        "added    greeting.txt",
        "applied to ",
        "memory gave it: 1 note in ",
        " lesson 0.90\n",
        &format!("\n        {LEARNED_TEXT}\n"),
        "memory learned: 0 new, 1 confirmed",
    ] {
        assert!(shown.contains(part), "{part:?} in:\n{shown}");
    }
    let first = done[0]["run"].as_str().unwrap();
    let (shown, _) = run(molt_here(&server, &["runs", "show", first]).output().await.unwrap());
    let kept = format!("not applied; the result was kept in {}", done[0]["fork"].as_str().unwrap());
    for part in [
        &kept,
        &"memory gave it: 0 notes".to_owned(),
        &"memory learned: 1 new".to_owned(),
        &format!("+ [lesson] {LEARNED_TEXT}"),
    ] {
        assert!(shown.contains(part), "{part:?} in:\n{shown}");
    }

    // Another data dir has none of them.
    let elsewhere = tempfile::tempdir().unwrap();
    let args = ["runs", "--data-dir", elsewhere.path().to_str().unwrap()];
    let (none, said) = run(molt_here(&server, &args).output().await.unwrap());
    assert!(none.is_empty() && said.starts_with("no runs in "), "{said}");
    assert_requests_valid(&server).await;
}

fn nats_server() -> Option<PathBuf> {
    std::env::var_os("MOLT_NATS_SERVER").map(PathBuf::from).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("nats-server")).find(|p| p.is_file())
    })
}

#[tokio::test]
async fn a_task_runs_over_nats_with_provisioned_secrets() {
    let Some(bin) = nats_server() else {
        assert!(
            std::env::var_os("MOLT_REQUIRE_NATS_TESTS").is_none(),
            "MOLT_REQUIRE_NATS_TESTS is set, and nats-server was not found: set MOLT_NATS_SERVER"
        );
        eprintln!("skipping: nats-server not found");
        return;
    };
    let server = fake_api(Fake::GreetsAtLength(LONG_GREETING)).await;
    let mut setup = Setup::new(&server);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    setup.cfg.kernel.transport = TransportKind::Nats;
    setup.cfg.kernel.nats_url = format!("nats://127.0.0.1:{port}");

    // What `molt nats-config` does: the secrets include one for the CLI.
    let ids: Vec<ServiceId> = setup.cfg.services.iter().map(|s| s.name.clone()).collect();
    let secrets = Secrets::load_or_create(&setup.cfg.secrets_path(), &ids).unwrap();
    assert!(secrets.services.contains_key(molt::agent::CLI));
    let users: Vec<(ServiceId, Secret)> =
        secrets.services.iter().map(|(id, s)| (ServiceId::new(id.as_str()).unwrap(), s.clone())).collect();
    let conf = setup.data.path().join("nats.conf");
    std::fs::write(&conf, nats::server_config(nats::DEFAULT_PREFIX, &secrets.kernel, &users)).unwrap();
    let mut nats = tokio::process::Command::new(bin)
        .args(["-a", "127.0.0.1", "-p", &port.to_string(), "-c"])
        .arg(&conf)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut ready = false;
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "nats-server did not start");

    let (resp, _) = setup.run(setup.request(Some(CHECK), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    // The greeting is bigger than nats-server's default payload limit, and
    // so are the fs.write and the model requests that carry it.
    assert_eq!(setup.file("greeting.txt").map(|g| g.len()), Some(LONG_GREETING), "{}", resp.summary);
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
    let _ = nats.kill().await;
}

#[test]
fn the_cli_refuses_a_bad_setup_before_starting_anything() {
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let user_data = tempfile::tempdir().unwrap();
    let molt_do = |data_dir: &Path, key: Option<&str>, args: &[&str]| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_molt"));
        cmd.args(["do", "anything", "--data-dir"]).arg(data_dir).args(args).current_dir(workspace.path());
        cmd.env("XDG_DATA_HOME", user_data.path());
        cmd.env_remove("OPENAI_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN").env_remove("MOLT_PROVIDER");
        match key {
            Some(key) => cmd.env("ANTHROPIC_API_KEY", key),
            None => cmd.env_remove("ANTHROPIC_API_KEY"),
        };
        let out = cmd.output().unwrap();
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = molt_do(data.path(), None, &[]);
    assert!(err.contains("ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY is not set"), "{err}");
    let err = molt_do(data.path(), Some("  "), &[]);
    assert!(err.contains("ANTHROPIC_AUTH_TOKEN"), "{err}");
    let ws = workspace.path().canonicalize().unwrap();
    let inside = ws.join("state");
    let err = molt_do(&inside, Some("test"), &[]);
    assert!(err.contains("inside the workspace"), "{err}");
    assert!(!inside.exists());
    // The same, reached through `..`: refused before anything is created.
    let roundabout = data.path().join("missing/..").join("..").join(ws.file_name().unwrap()).join("state");
    let err = molt_do(&roundabout, Some("test"), &[]);
    assert!(err.contains("inside the workspace"), "{err}");
    assert!(!inside.exists());
    // Limits the planner would refuse only once everything is running.
    for args in [["--max-turns", "0"], ["--budget-usd", "0"], ["--budget-usd", "NaN"]] {
        let err = molt_do(data.path(), Some("test"), &args);
        assert!(err.contains(args[0]), "{err}");
    }
    let err = molt_do(data.path(), Some("test"), &["--pass-env", "ANTHROPIC_AUTH_TOKEN"]);
    assert!(err.contains("commands never see"), "{err}");
    let err = molt_do(data.path(), Some("test"), &["--allow-web", "https://docs.rs/serde"]);
    assert!(err.contains("not a host to allow"), "{err}");
    assert!(std::fs::read_dir(data.path()).unwrap().next().is_none(), "nothing was started");
}

#[tokio::test]
async fn a_kept_session_result_survives_restart_and_refuses_workspace_conflicts() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    let mut req = setup.request(Some(CHECK), 1);
    req.apply = false;
    let response = session.run(&req, &molt_proto::TraceId::random()).await.unwrap();
    let fork = response.fork.unwrap();
    session.shutdown(std::slice::from_ref(&fork)).await;
    // A saved path from a different workspace cannot redirect a human's operation.
    let other = tempfile::tempdir().unwrap();
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let wrong = start_session(&setup.cfg, other.path().to_str().unwrap(), events).await.unwrap();
    for error in [
        wrong.diff(&fork).await.unwrap_err(),
        wrong.apply(&fork).await.unwrap_err(),
        wrong.discard(&fork).await.unwrap_err(),
    ] {
        assert!(format!("{error:#}").contains("another workspace"));
    }
    assert!(Path::new(&fork).is_dir());
    assert_eq!(setup.files(), ["README.txt"]);
    wrong.shutdown(std::slice::from_ref(&fork)).await;
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    assert!(session.diff(&fork).await.unwrap().patch.contains("+hello"));
    std::fs::write(workspace.join("greeting.txt"), "user's work\n").unwrap();
    let error = format!("{:#}", session.apply(&fork).await.unwrap_err());
    assert!(error.contains("conflict:") && error.contains("greeting.txt"), "{error}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("user's work\n"));
    assert!(Path::new(&fork).is_dir());
    std::fs::remove_file(workspace.join("greeting.txt")).unwrap();
    session.apply(&fork).await.unwrap();
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert!(!Path::new(&fork).exists());
    session.shutdown(&[]).await;
}

#[tokio::test]
async fn session_undo_notifies_memory_and_cli_undo_refuses_its_lock_before_changing_git() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let w = setup.workspace.path();
    git(w, &["init", "-q", "-b", "main"]);
    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
        git(w, &["config", key, value]);
    }
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "start"]);
    let config = setup.data.path().join("config.toml");
    std::fs::write(&config, toml::to_string(&setup.cfg).unwrap()).unwrap();
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, w.to_str().unwrap(), events).await.unwrap();
    for locked_cli in [false, true] {
        let trace = molt_proto::TraceId::random();
        let before = molt::git::Before::look(w).unwrap();
        let response = session.run(&setup.request(Some(CHECK), 1), &trace).await.unwrap();
        assert!(response.applied);
        before.commit(&response, trace.as_str(), "write greeting", None).unwrap().unwrap();
        session.learn(&trace, None).await.unwrap();
        let head = git(w, &["rev-parse", "HEAD"]);
        if locked_cli {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
                .args(["undo", trace.as_str(), "--config"])
                .arg(&config)
                .arg("--workspace")
                .arg(w)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(4), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stderr).contains("use /undo"));
            assert_eq!(git(w, &["rev-parse", "HEAD"]), head, "a locked CLI undo changed nothing");
        }
        let report = session.undo(if locked_cli { Some(trace.to_string()) } else { None }).await.unwrap();
        assert!(matches!(report.git, molt::git::Undo::Reverted { .. }));
        assert!(report.memory.unwrap().is_ok());
        assert!(!w.join("greeting.txt").exists());
        assert!(session.notes("").await.unwrap().is_empty());
        let head = git(w, &["rev-parse", "HEAD"]);
        let report = session.undo(Some(trace.to_string())).await.unwrap();
        assert!(matches!(report.git, molt::git::Undo::Already { .. }));
        assert!(report.memory.unwrap().is_ok());
        assert_eq!(git(w, &["rev-parse", "HEAD"]), head);
    }
    // If Git succeeded but a fresh memory service cannot start, status 4
    // still exposes the partial result and the exact retry id.
    let trace = molt_proto::TraceId::random();
    let before = molt::git::Before::look(w).unwrap();
    let response = session.run(&setup.request(Some(CHECK), 1), &trace).await.unwrap();
    before.commit(&response, trace.as_str(), "write greeting", None).unwrap().unwrap();
    session.learn(&trace, None).await.unwrap();
    session.shutdown(&[]).await;
    let mut broken = setup.cfg.clone();
    let memory = broken.service_mut("memory").unwrap();
    memory.exec.as_mut().unwrap().command = setup.data.path().join("missing-memory").to_str().unwrap().into();
    memory.max_restarts = 1;
    std::fs::write(&config, toml::to_string(&broken).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["undo", trace.as_str(), "--config"])
        .arg(&config)
        .arg("--workspace")
        .arg(w)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("undid run"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("memory could not be told"));
    assert!(!w.join("greeting.txt").exists());
}

/// What the interface does with a task in its default mode, on one session:
/// design the check and show it, run with the check as accepted, keep the
/// result for the user to look at, apply it, learn, then list and forget
/// what was learned.
#[tokio::test]
async fn a_session_shows_the_check_first_and_applies_only_when_asked() {
    let server = fake_api(Fake::DesignsThenGreets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let (events, mut progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    assert_eq!(session.memory_problem(), None);
    assert!(session.versions().iter().any(|(name, v)| name == "planner" && v.len() == 8), "{:?}", session.versions());
    let trace = molt_proto::TraceId::random();
    let mut req = setup.request(None, 2);
    req.apply = false;

    let design = tokio::time::timeout(RUN_LIMIT, session.design(&req, &trace)).await.unwrap().unwrap();
    assert_eq!(design.command.as_deref(), Some("sh check.sh"));
    assert_eq!(design.files.len(), 1);
    assert_eq!(
        (design.files[0].path.as_str(), design.files[0].content.as_str()),
        ("check.sh", "grep -q hello greeting.txt\n")
    );
    assert_eq!(design.rationale, "Greets.");
    let baseline = design.baseline.as_ref().expect("the check ran on the workspace as it is");
    assert!(!baseline.passed, "{baseline:?}");
    assert!(design.cost_usd > 0.0);
    // Nothing in the workspace yet: the designer worked in a fork.
    assert_eq!(setup.files(), ["README.txt"]);

    req.check = design.command.clone();
    req.check_files = design.files.clone();
    let resp = tokio::time::timeout(RUN_LIMIT, session.run(&req, &trace)).await.unwrap().unwrap();
    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(
        resp.check,
        Some(CheckSpec { command: "sh check.sh".into(), files: vec!["check.sh".into()], designed: true, suite: None })
    );
    assert!(!resp.applied);
    // Only the run's own designer: the accepted check was not designed again.
    assert_eq!(bodies(&server).await.iter().filter(|b| offers_submit_check(b)).count(), 2);
    assert_eq!(setup.files(), ["README.txt"], "nothing is applied before the user says so");
    let fork = resp.fork.clone().expect("the result is kept for the user");
    // What an interrupt asks of a fork, to keep it only when it holds work.
    assert_eq!(session.changes(&fork).await.unwrap(), ["check.sh", "greeting.txt"]);
    session.apply(&fork).await.unwrap();
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert_eq!(setup.file("check.sh").as_deref(), Some(format!("{CHECK}\n").as_str()));

    let mut seen = Vec::new();
    while let Ok(event) = progress.try_recv() {
        seen.push(event);
    }
    assert!(seen.iter().all(|e| e.run() == trace.as_str()), "{seen:#?}");
    assert!(seen.iter().any(|e| matches!(e, Progress::CheckReady { designed: true, .. })), "{seen:#?}");
    assert!(seen.iter().any(|e| matches!(e, Progress::AttemptStarted { fork: Some(_), .. })), "{seen:#?}");

    let captured = session.used_notes(trace.as_str()).await.unwrap();
    assert!(!captured.is_empty());
    assert!(captured.iter().all(|s| s.run == trace.as_str() && s.workspace == workspace.to_str().unwrap()));

    let learned = session.learn(&trace, Some("haiku".into())).await.unwrap();
    assert_eq!(learned.added.len(), 1, "{learned:#?}");
    // Memory learned with the model it was given, not its default.
    let learning = bodies(&server).await.into_iter().filter(learns);
    assert_eq!(learning.map(|b| b["model"].clone()).collect::<Vec<_>>(), [json!("claude-haiku-4-5")]);
    let mut notes = session.notes("").await.unwrap();
    assert_eq!(notes.iter().map(|r| r.note.text.as_str()).collect::<Vec<_>>(), [LEARNED_TEXT]);
    let review = molt_api::memory::ReviewRequest {
        workspace: workspace.to_str().unwrap().into(),
        id: notes[0].note.id.clone(),
        expected_rev: notes[0].note.rev,
        reason: "checked the greeting".into(),
        depends_on: vec!["greeting.txt".into()],
        temporary: true,
    };
    let saved = session.change_note(review.clone(), None).await.unwrap();
    assert!(saved.details.temporary && saved.note.rev > review.expected_rev);
    let refused = session.change_note(review.clone(), None).await.unwrap_err();
    assert!(format!("{refused:#}").contains("revision"));
    std::fs::write(workspace.join("greeting.txt"), "a later change\n").unwrap();
    assert!(session.notes("").await.unwrap().is_empty(), "ordinary recall excludes quarantined notes");
    let inspected = session.inspect_notes("").await.unwrap();
    assert_eq!(inspected.len(), 1);
    assert_eq!(inspected[0].details.needs_review, ["greeting.txt"]);
    let review = molt_api::memory::ReviewRequest { expected_rev: inspected[0].note.rev, ..review };
    let corrected =
        session.change_note(review, Some("The greeting changed; inspect it before editing.".into())).await.unwrap();
    assert_ne!(corrected.note.id, saved.note.id);
    notes = session.notes("").await.unwrap();
    assert_eq!(notes[0].note.id, corrected.note.id);
    assert_eq!(session.used_notes(trace.as_str()).await.unwrap(), captured, "review does not rewrite earlier context");
    assert!(session.forget(&notes[0].note.id, "the check moved").await.unwrap());
    assert!(session.notes("").await.unwrap().is_empty());

    session.shutdown(&[]).await;
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn a_checkpointed_run_that_runs_out_of_budget_is_resumed_with_its_fork() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let mut req = setup.request(Some(CHECK), 1);
    req.checkpoint = true;
    // The first model call spends it all.
    req.budget_usd = Some(0.000_001);
    let trace = molt_proto::TraceId::random();
    let stopped = molt::agent::run_task_as(&setup.cfg, req, trace.clone(), None, |_| {}, std::future::pending());
    let stopped = tokio::time::timeout(RUN_LIMIT, stopped).await.expect("the run hung").unwrap().run;
    assert_eq!(stopped.checkpoint.as_deref(), Some(trace.as_str()), "{stopped:#?}");
    let runs = setup.data.path().join(molt::config::RUNS);
    let saved = molt_planner::checkpoint::load(&runs, trace.as_str()).unwrap();
    let fork = saved.attempts[0].fork.clone().expect("the attempt had a fork");
    assert!(std::path::Path::new(&fork).is_dir(), "the fork is kept for the resume");
    assert_eq!(setup.file("greeting.txt"), None);

    let mut req = saved.request.clone();
    req.resume = Some(molt_api::planner::Resume { run: trace.to_string(), retry: false });
    req.budget_usd = Some(10.0);
    let resumed = molt::agent::run_task_as(&setup.cfg, req, trace.clone(), None, |_| {}, std::future::pending());
    let resumed = tokio::time::timeout(RUN_LIMIT, resumed).await.expect("the run hung").unwrap().run;
    assert_eq!(resumed.outcome, Outcome::Passed, "{resumed:#?}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert!(resumed.cost_usd > stopped.cost_usd, "the cost goes on: {} then {}", stopped.cost_usd, resumed.cost_usd);
    assert!(molt_planner::checkpoint::load(&runs, trace.as_str()).is_err(), "a finished run keeps no checkpoint");
    assert!(!std::path::Path::new(&fork).exists());
}

/// A checkpointed run in `session` that stops early: the first model call spends its budget.
async fn stop_early(setup: &Setup, session: &molt::session::Session) -> (RunResponse, molt_proto::TraceId) {
    let mut req = setup.request(Some(CHECK), 1);
    req.checkpoint = true;
    req.budget_usd = Some(0.000_001);
    let trace = molt_proto::TraceId::random();
    let resp = tokio::time::timeout(RUN_LIMIT, session.run(&req, &trace)).await.expect("the run hung").unwrap();
    (resp, trace)
}

#[tokio::test]
async fn the_interfaces_session_keeps_a_stopped_runs_forks_to_resume_it_and_drops_them_when_told() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let runs = setup.data.path().join(molt::config::RUNS);
    let start = || async {
        let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
        start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap()
    };

    // Quit: the session stops, and the stopped run's fork stays for the next one.
    let session = start().await;
    let (stopped, trace) = stop_early(&setup, &session).await;
    assert_eq!(stopped.checkpoint.as_deref(), Some(trace.as_str()), "{stopped:#?}");
    session.shutdown(&[]).await;
    let saved = molt_planner::checkpoint::load(&runs, trace.as_str()).unwrap();
    let fork = saved.attempts[0].fork.clone().expect("the attempt had a fork");
    assert!(Path::new(&fork).is_dir(), "the fork outlives the session that made it");

    // Reopen and continue: the run goes on in its fork, once, and its cost goes on.
    let session = start().await;
    let mut req = saved.request.clone();
    req.resume = Some(molt_api::planner::Resume { run: trace.to_string(), retry: false });
    req.budget_usd = Some(10.0);
    let resumed = tokio::time::timeout(RUN_LIMIT, session.run(&req, &trace)).await.expect("the run hung").unwrap();
    assert_eq!(resumed.outcome, Outcome::Passed, "{resumed:#?}");
    assert!(resumed.applied && setup.file("greeting.txt").as_deref() == Some("hello\n"));
    assert!(resumed.cost_usd > stopped.cost_usd, "{} then {}", stopped.cost_usd, resumed.cost_usd);
    assert!(molt_planner::checkpoint::load(&runs, trace.as_str()).is_err(), "a finished run keeps no checkpoint");
    let again = session.run(&req, &trace).await.unwrap_err();
    assert!(format!("{again:#}").contains("no checkpoint"), "a finished run is not run again: {again:#}");

    // Dropped from a later session: its fork and its checkpoint go.
    std::fs::remove_file(workspace.join("greeting.txt")).unwrap();
    let (_, dropped) = stop_early(&setup, &session).await;
    session.shutdown(&[]).await;
    let session = start().await;
    let fork = molt_planner::checkpoint::load(&runs, dropped.as_str()).unwrap().forks().remove(0);
    session.drop_run(dropped.as_str()).await.unwrap();
    assert!(!Path::new(&fork).exists());
    assert!(molt_planner::checkpoint::load(&runs, dropped.as_str()).is_err());

    // Stopped by the user: the session drops its checkpoint, and its fork with it.
    let (_, cancelled) = stop_early(&setup, &session).await;
    let fork = molt_planner::checkpoint::load(&runs, cancelled.as_str()).unwrap().forks().remove(0);
    session.shutdown_dropping(&[], Some(cancelled.as_str())).await;
    assert!(molt_planner::checkpoint::load(&runs, cancelled.as_str()).is_err());
    assert!(!Path::new(&fork).exists());
    assert_eq!(setup.files(), ["README.txt"]);
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn lifecycle_formatting_is_rechecked_and_cannot_weaken_the_protected_check() {
    use molt_planner::hooks::{Event, Hook};
    for (command, should_apply) in [
        ("printf '\\n' >> greeting.txt", true),
        ("printf goodbye > greeting.txt", false),
        ("printf 'exit 0\\n' > check.sh; printf goodbye > greeting.txt", false),
        ("sleep 10", false),
    ] {
        let server = fake_api(Fake::DesignsThenGreets).await;
        let mut setup = Setup::new(&server);
        let hooks = vec![
            Hook {
                blocking: true,
                timeout_ms: if command == "sleep 10" { 20 } else { 5000 },
                ..Hook::new(Event::ResultReady, command)
            },
            Hook { timeout_ms: 5000, ..Hook::new(Event::AfterApply, "printf notification > notification.txt") },
        ];
        setup
            .cfg
            .service_mut("planner")
            .unwrap()
            .env
            .insert("MOLT_HOOKS_JSON".into(), serde_json::to_string(&hooks).unwrap());
        let (response, events) = setup.run(setup.request(None, 1)).await;
        assert_eq!(response.applied, should_apply, "{command}: {}", response.summary);
        if should_apply {
            assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n\n"));
            assert_eq!(setup.file("check.sh").as_deref(), Some(&format!("{CHECK}\n")[..]));
        } else {
            assert_eq!(response.outcome, Outcome::Failed);
            assert!(response.fork.is_some());
            assert!(setup.file("greeting.txt").is_none());
            assert!(response.summary.contains("Not applied"));
        }
        assert!(setup.file("notification.txt").is_none(), "notifications only write in a disposable fork");
        assert!(events
            .iter()
            .any(|e| matches!(e, Progress::Note { message, .. } if message.contains("hook result.ready"))));
        let audit = std::fs::read_to_string(setup.data.path().join("audit.jsonl")).unwrap();
        assert!(audit.contains("duration_ms") && audit.contains("shell.run"));
    }
}

#[tokio::test]
async fn a_comparison_suite_cannot_mutate_the_checked_candidate() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let mut req = setup.request(Some(CHECK), 2);
    req.select = molt_api::planner::Select::All;
    req.suite = Some("printf 'broken\\n' > greeting.txt".into());
    let (resp, _) = setup.run(req).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(resp.selection.as_ref().unwrap().candidates.len(), 2);
    assert!(resp.applied);
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
}

#[tokio::test]
async fn all_candidates_are_formatted_and_rechecked_before_selection() {
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    let hooks = serde_json::json!([{"event":"before_apply", "command":"printf '\\n' >> greeting.txt", "blocking":true, "timeout_ms":5000}]);
    setup.cfg.service_mut("planner").unwrap().env.insert("MOLT_HOOKS_JSON".into(), hooks.to_string());
    let mut req = setup.request(Some(CHECK), 2);
    req.select = molt_api::planner::Select::All;
    let (resp, _) = setup.run(req).await;
    assert!(resp.applied, "{resp:#?}");
    assert_eq!(resp.selection.as_ref().unwrap().candidates.len(), 2);
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n\n"));
}

#[tokio::test]
async fn a_tool_before_hook_refuses_a_write_and_the_model_reads_why_and_what_a_check_hook_adds() {
    use molt_planner::hooks::{Event, Hook};
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    // It decides from the event it reads on stdin, as a hook script does.
    let guard = r#"if grep -q '"tool_name":"write_file"'; then printf '{"decision":"deny","reason":"greeting files are frozen"}'; fi"#;
    let hooks = vec![
        Hook {
            matcher: Some("write_file|edit_file".into()),
            paths: vec!["greeting.*".into()],
            blocking: true,
            timeout_ms: 5000,
            ..Hook::new(Event::ToolBefore, guard)
        },
        Hook {
            timeout_ms: 5000,
            ..Hook::new(Event::CheckAfter, r#"printf '{"decision":"annotate","context":"Ask the owner first."}'"#)
        },
    ];
    let env = &mut setup.cfg.service_mut("planner").unwrap().env;
    env.insert("MOLT_HOOKS_JSON".into(), serde_json::to_string(&hooks).unwrap());
    let req = RunRequest { max_check_rounds: Some(2), ..setup.request(Some(CHECK), 1) };
    let (response, events) = setup.run(req).await;
    assert!(!response.applied, "{}", response.summary);
    assert!(setup.file("greeting.txt").is_none());
    let bodies = bodies(&server).await;
    let said = |text: &str| bodies.iter().any(|b| b["messages"].to_string().contains(text));
    assert!(said("Not run: the user's tool.before hook refused this call: greeting files are frozen"));
    assert!(said("[Context from the user's check.after hook]\\nAsk the owner first."));
    let decision = "hook tool.before write_file: deny: greeting files are frozen";
    assert!(events.iter().any(|e| matches!(e, Progress::Note { message, .. } if message.starts_with(decision))));
    let audit = std::fs::read_to_string(setup.data.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains(decision), "the decision is in the audit");
    assert!(audit.contains(r#"\"hook_event_name\":\"tool.before\""#), "and so is the event the hook read");
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn an_apply_before_hook_keeps_a_passing_result_out_of_the_workspace() {
    use molt_planner::hooks::{Event, Hook};
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    let refuse = r#"printf '{"decision":"deny","reason":"greetings need a sign-off"}'"#;
    let hooks = vec![
        Hook { paths: vec!["greeting.txt".into()], timeout_ms: 5000, ..Hook::new(Event::ApplyBefore, refuse) },
        Hook { timeout_ms: 5000, ..Hook::new(Event::AfterApply, "printf applied > applied.txt") },
    ];
    let env = &mut setup.cfg.service_mut("planner").unwrap().env;
    env.insert("MOLT_HOOKS_JSON".into(), serde_json::to_string(&hooks).unwrap());
    // Learning happens on this side, and so do its hooks.
    setup.learn = true;
    setup.cfg.hooks = vec![Hook::new(Event::LearnAfter, "cat > /dev/null")];
    let (done, _) = setup.run_done(setup.request(Some(CHECK), 1)).await;
    let learned = done.learned.expect("it learned").expect("it learned");
    let audit = std::fs::read_to_string(setup.data.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains(r#"\"hook_event_name\":\"learn.after\""#), "the learn.after hook heard of it");
    assert!(audit.contains(&format!(r#"\"added\":[\"{}\"]"#, learned.added[0].id)));
    let response = done.run;
    assert_eq!(response.outcome, Outcome::Passed);
    assert!(!response.applied);
    assert!(
        response.summary.contains("an apply.before hook refused them: greetings need a sign-off"),
        "{}",
        response.summary
    );
    let fork = response.fork.expect("the result is kept");
    assert_eq!(std::fs::read_to_string(Path::new(&fork).join("greeting.txt")).unwrap(), "hello\n");
    assert!(setup.file("greeting.txt").is_none());
}

#[tokio::test]
async fn a_kept_result_the_user_applies_passes_the_apply_hooks_too() {
    use molt_planner::hooks::{Event, Hook};
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let refuse = "grep -q greeting.txt && { echo 'greetings need a sign-off' >&2; exit 2; }";
    setup.cfg.hooks = vec![Hook { paths: vec!["*.txt".into()], ..Hook::new(Event::ApplyBefore, refuse) }];
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    let req = RunRequest { apply: false, ..setup.request(Some(CHECK), 1) };
    let fork = session.run(&req, &molt_proto::TraceId::random()).await.unwrap().fork.unwrap();
    let error = format!("{:#}", session.apply(&fork).await.unwrap_err());
    assert!(error.contains("an apply.before hook refused the result: greetings need a sign-off"), "{error}");
    assert!(setup.file("greeting.txt").is_none());
    assert!(Path::new(&fork).is_dir(), "the result is kept");
    session.shutdown(std::slice::from_ref(&fork)).await;

    setup.cfg.hooks = vec![Hook::new(Event::AfterApply, "cat > /dev/null")];
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    session.apply(&fork).await.unwrap();
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    session.shutdown(&[]).await;
    let audit = std::fs::read_to_string(setup.data.path().join("audit.jsonl")).unwrap();
    assert!(audit.contains(r#"\"hook_event_name\":\"apply.after\""#));
    assert!(audit.contains(r#"\"changes\":[\"greeting.txt\"]"#));
}

/// One side of the client protocol (docs/serve.md): JSON-RPC, a message a line.
struct Rpc<R, W> {
    input: tokio::io::BufReader<R>,
    out: W,
    next: u64,
    /// Progress events read while waiting for an answer.
    events: Vec<Value>,
}

impl<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin> Rpc<R, W> {
    fn new(input: R, out: W) -> Self {
        Self { input: tokio::io::BufReader::new(input), out, next: 0, events: Vec::new() }
    }

    async fn line(&mut self, value: &Value) {
        use tokio::io::AsyncWriteExt;
        self.out.write_all(format!("{value}\n").as_bytes()).await.unwrap();
        self.out.flush().await.unwrap();
    }

    /// Send a request without waiting; its id.
    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        self.line(&json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})).await;
        self.next
    }

    /// The answer to request `id`, keeping the events that come before it.
    async fn answer(&mut self, id: u64) -> Value {
        use tokio::io::AsyncBufReadExt;
        tokio::time::timeout(RUN_LIMIT, async {
            loop {
                let mut line = String::new();
                assert!(self.input.read_line(&mut line).await.unwrap() > 0, "the server hung up");
                let msg: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(msg["jsonrpc"], "2.0", "{msg}");
                if msg["method"] == "progress" {
                    self.events.push(msg["params"].clone());
                } else if msg["id"] == id {
                    return msg;
                }
            }
        })
        .await
        .expect("no answer")
    }

    /// The first progress event `pred` takes, read from what comes.
    async fn event(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        use tokio::io::AsyncBufReadExt;
        if let Some(event) = self.events.iter().find(|e| pred(e)) {
            return event.clone();
        }
        tokio::time::timeout(RUN_LIMIT, async {
            loop {
                let mut line = String::new();
                assert!(self.input.read_line(&mut line).await.unwrap() > 0, "the server hung up");
                let msg: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(msg["method"], "progress", "an answer came first: {msg}");
                self.events.push(msg["params"].clone());
                if pred(&msg["params"]) {
                    return msg["params"].clone();
                }
            }
        })
        .await
        .expect("no such event")
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        let answer = self.answer(id).await;
        assert!(answer.get("error").is_none(), "{method}: {answer}");
        answer["result"].clone()
    }
}

/// The interface's own server: the config it resolved comes first on the
/// pipe, then the client protocol, as the interface speaks it.
#[tokio::test]
async fn the_ui_backend_finishes_without_terminal_reads_and_exits_when_the_pipe_closes() {
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    let mut noise = molt::ServiceSection::new(
        ServiceId::new("noise").unwrap(),
        molt_proto::Tier::Mutable,
        molt_proto::Exec {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "printf untrusted-service-stdout".into()],
        },
        Vec::new(),
    );
    noise.max_restarts = 0;
    setup.cfg.services.push(noise);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .arg("ui-backend")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let mut rpc = Rpc::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    rpc.line(&json!({"cfg": setup.cfg, "workspace": workspace.to_str().unwrap()})).await;
    let hello = rpc.call("initialize", json!({"protocol": 1, "client": {"name": "molt-tui"}})).await;
    assert_eq!(hello["workspace"], workspace.to_str().unwrap());
    let up = rpc.call("session.open", json!({})).await;
    assert!(up["versions"].as_array().is_some_and(|v| !v.is_empty()), "{up}");
    let req = setup.request(Some(CHECK), 1);
    let id = rpc.send("task.submit", json!({"trace": molt_proto::TraceId::random(), "request": req})).await;
    // With no frontend reading messages, both model turns and review still run.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if server.received_requests().await.unwrap_or_default().len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let result = rpc.answer(id).await;
    assert_eq!(result["result"]["applied"], true, "{result}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    // Only protocol messages came on the pipe: a service's stdout is not among them.
    assert!(rpc.events.iter().all(|e| e["event"].is_string()), "{:?}", rpc.events);
    // A result waits for the interface's decision when the interface dies: its fork stays, to be picked up.
    std::fs::remove_file(setup.workspace.path().join("greeting.txt")).unwrap();
    let mut req = setup.request(Some(CHECK), 1);
    req.apply = false;
    let kept = rpc.call("task.submit", json!({"trace": molt_proto::TraceId::random(), "request": req})).await;
    let fork = kept["fork"].as_str().expect("kept in its fork").to_owned();
    drop(rpc);
    assert!(tokio::time::timeout(Duration::from_secs(15), child.wait()).await.unwrap().unwrap().success());
    assert!(Path::new(&fork).is_dir(), "the fork outlives the interface that died");
}

/// A script as a client of `molt serve`: over the socket in the data dir,
/// it runs a task to a result kept in its fork, sees its progress, reads
/// its diff, applies it, and closes; the server stops on SIGTERM.
#[tokio::test]
async fn a_script_runs_a_task_through_molt_serve_and_applies_the_result() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let mut serve = molt(&server, &workspace, setup.data.path(), &["serve"]).spawn().unwrap();
    let socket = setup.data.path().join("serve.sock");
    let started = Instant::now();
    let stream = loop {
        match tokio::net::UnixStream::connect(&socket).await {
            Ok(stream) => break stream,
            Err(_) if started.elapsed() < Duration::from_secs(20) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(e) => {
                serve.start_kill().unwrap();
                let out = serve.wait_with_output().await.unwrap();
                panic!(
                    "molt serve did not listen on {}: {e}\n{}",
                    socket.display(),
                    String::from_utf8_lossy(&out.stderr)
                )
            }
        }
    };
    assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600, "only its user opens it");
    let (input, out) = stream.into_split();
    let mut rpc = Rpc::new(input, out);
    let hello = rpc.call("initialize", json!({"protocol": 1, "client": {"name": "script"}})).await;
    assert_eq!(hello["workspace"], workspace.to_str().unwrap());
    let missing = rpc.send("result.diff", json!({"fork": "/nowhere"})).await;
    assert_eq!(rpc.answer(missing).await["error"]["code"], -32001, "no session yet");
    rpc.call("session.open", json!({})).await;

    // The request names no workspace: the server's own is the one.
    let trace = molt_proto::TraceId::random();
    let mut req = RunRequest::new("Write a greeting into greeting.txt.", "");
    (req.check, req.attempts, req.apply, req.stream) = (Some(CHECK.into()), 1, false, true);
    let result = rpc.call("task.submit", json!({"trace": trace, "request": req})).await;
    assert_eq!(result["outcome"], "passed", "{result}");
    assert!(setup.file("greeting.txt").is_none(), "a result kept is not applied");
    let fork = result["fork"].as_str().expect("kept in its fork").to_owned();
    let of_run: Vec<&Value> = rpc.events.iter().filter(|e| e["run"] == trace.as_str()).collect();
    let saw = |kind: &str| of_run.iter().any(|e| e["event"] == kind);
    assert!(saw("attempt_started") && saw("tool_call") && saw("check_ran") && saw("attempt_finished"), "{of_run:?}");
    let costed = |e: &&&Value| e["event"] == "model_finished" && e["role"] == "attempt" && e["cost_usd"].is_number();
    assert!(of_run.iter().any(|e| costed(&e)), "who made each call, and what it cost: {of_run:?}");

    let diff = rpc.call("result.diff", json!({"fork": fork})).await;
    assert_eq!(diff["changes"][0]["path"], "greeting.txt", "{diff}");
    assert!(diff["patch"].as_str().unwrap().contains("+hello"), "{diff}");
    let applied = rpc.call("result.apply", json!({"fork": fork})).await;
    assert_eq!(applied["changes"][0]["path"], "greeting.txt", "{applied}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    assert_eq!(rpc.call("session.close", json!({"keep": []})).await, json!({}));
    drop(rpc);

    // SAFETY: plain syscall.
    unsafe { libc::kill(serve.id().unwrap() as i32, libc::SIGTERM) };
    let status = tokio::time::timeout(Duration::from_secs(15), serve.wait()).await.unwrap().unwrap();
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "the socket goes with the server");
    setup.assert_forks_dropped();
    assert_requests_valid(&server).await;

    // The run is listed, in the session the server gave its client, with its result kept in its fork.
    let out = molt(&server, &workspace, setup.data.path(), &["runs", "--json"]).output().await.unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let listed: Value = serde_json::from_slice(&out.stdout).unwrap();
    let run = &listed[0];
    assert_eq!((&run["run"], &run["result"], &run["applied"]), (&json!(trace), &json!("passed"), &json!("kept")));
    assert_eq!(run["fork"], fork.as_str());
    assert!(run["session"].as_str().is_some_and(|s| s.starts_with("sess_")), "{run}");
}

/// The interface's Esc, through the protocol: `task.stop` cuts the run off,
/// its call answers that it was stopped, the command the attempt started
/// ends, and the attempt's fork is kept for the work in it.
#[tokio::test]
async fn a_task_stopped_through_the_protocol_keeps_the_attempts_work_and_its_commands_end() {
    let pids = tempfile::tempdir().unwrap();
    let pidfile = pids.path().join("sleep.pid");
    let hang = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let server = fake_api(Fake::GreetsWithCalls(vec![(RUN, json!({ "command": hang }))])).await;
    let mut setup = Setup::new(&server);
    // The pid marker is outside the fork, as in `stopping_a_run_kills_the_commands_it_started`.
    setup.cfg.service_mut("shell").unwrap().exec.as_mut().unwrap().args.push("--no-sandbox".into());
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .arg("ui-backend")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let mut rpc = Rpc::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    rpc.line(&json!({"cfg": setup.cfg, "workspace": workspace.to_str().unwrap()})).await;
    rpc.call("initialize", json!({"protocol": 1})).await;
    rpc.call("session.open", json!({})).await;
    let mut req = setup.request(Some(CHECK), 1);
    req.keep_cancelled_forks = true;
    let trace = molt_proto::TraceId::random();
    let submitted = rpc.send("task.submit", json!({"trace": trace, "request": req})).await;
    let started = rpc.event(|e| e["event"] == "attempt_started" && e["run"] == trace.as_str()).await;
    let fork = started["fork"].as_str().expect("the attempt's fork").to_owned();
    let pid = wait_for_pid(&pidfile).await;

    let stop = rpc.send("task.stop", json!({"attempts": [fork], "check_files": []})).await;
    let cut = rpc.answer(submitted).await;
    assert_eq!(cut["error"]["code"], -32003, "the run's call answers that it was stopped: {cut}");
    let stopped = rpc.answer(stop).await;
    assert_eq!(stopped["result"], json!({"kept": [fork], "notes": []}), "{stopped}");
    assert!(exits(pid).await, "the command's child {pid} outlived the stop");
    assert_eq!(std::fs::read_to_string(Path::new(&fork).join("greeting.txt")).unwrap(), "hello\n");
    assert!(setup.file("greeting.txt").is_none());
    // The next session opens, as the interface's does after a stop.
    rpc.call("session.open", json!({})).await;
    rpc.call("session.close", json!({"keep": [fork]})).await;
    drop(rpc);
    assert!(tokio::time::timeout(Duration::from_secs(15), child.wait()).await.unwrap().unwrap().success());
    assert!(Path::new(&fork).is_dir(), "kept as the client asked");
}

/// An editor of `molt acp`, as Zed is one: ACP on the child's standard
/// input and output, the permission prompts answered as a person would.
struct Editor {
    child: tokio::process::Child,
    input: tokio::io::BufReader<tokio::process::ChildStdout>,
    out: Option<tokio::process::ChildStdin>,
    next: u64,
    /// The session updates read so far.
    updates: Vec<Value>,
    /// Where `molt acp` keeps each folder's data dir.
    cache: PathBuf,
    log: PathBuf,
}

impl Editor {
    fn start(server: &MockServer, data: &Path) -> Self {
        let cache = data.join("cache");
        let log = data.join("acp.log");
        let mut cmd = molt_in(server, data, data);
        cmd.arg("acp")
            .env("XDG_CACHE_HOME", &cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap());
        let mut child = cmd.spawn().unwrap();
        let input = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let out = child.stdin.take();
        Self { child, input, out, next: 0, updates: Vec::new(), cache, log }
    }

    async fn line(&mut self, value: &Value) {
        use tokio::io::AsyncWriteExt;
        let out = self.out.as_mut().expect("the input is open");
        out.write_all(format!("{value}\n").as_bytes()).await.unwrap();
        out.flush().await.unwrap();
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        self.line(&json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})).await;
        self.next
    }

    /// The next message from `molt acp`; a session update is kept too.
    async fn recv(&mut self) -> Value {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let read = self.input.read_line(&mut line).await.unwrap();
        if read == 0 {
            panic!("molt acp hung up:\n{}", std::fs::read_to_string(&self.log).unwrap_or_default());
        }
        let msg: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(msg["jsonrpc"], "2.0", "{msg}");
        if msg["method"] == "session/update" {
            self.updates.push(msg["params"]["update"].clone());
        }
        msg
    }

    /// The answer to request `id`. Each permission prompt that comes first
    /// is answered with what `permit` picks from it.
    async fn answer(&mut self, id: u64, mut permit: impl FnMut(&Value) -> Value) -> Value {
        tokio::time::timeout(RUN_LIMIT, async {
            loop {
                let msg = self.recv().await;
                if msg["method"] == "session/request_permission" {
                    let result = permit(&msg["params"]);
                    self.line(&json!({"jsonrpc": "2.0", "id": msg["id"], "result": result})).await;
                } else if msg.get("method").is_none() && msg["id"] == id {
                    return msg;
                } else {
                    assert_eq!(msg["method"], "session/update", "nothing else comes: {msg}");
                }
            }
        })
        .await
        .expect("no answer")
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.answer(id, |asked| panic!("asked for nothing: {asked}")).await
    }

    /// The first update `pred` takes, read from what comes.
    async fn update(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        let found = tokio::time::timeout(RUN_LIMIT, async {
            loop {
                let msg = self.recv().await;
                assert_eq!(msg["method"], "session/update", "something else came first: {msg}");
                if pred(&msg["params"]["update"]) {
                    return msg["params"]["update"].clone();
                }
            }
        })
        .await;
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        found.unwrap_or_else(|_| panic!("no such update in {:#?}\n{log}", self.updates))
    }

    /// Close the editor's side: `molt acp` stops its services and exits.
    async fn close(mut self) {
        drop(self.out.take());
        let status = tokio::time::timeout(Duration::from_secs(60), self.child.wait()).await.expect("molt acp hung");
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        assert!(status.unwrap().success(), "{log}");
    }

    /// Close, and check that the forks left in every folder's data dir are
    /// `kept`, and no other.
    async fn close_and_check(self, kept: &[PathBuf]) {
        let cache = self.cache.clone();
        self.close().await;
        let mut kept = kept.to_vec();
        kept.sort();
        assert_eq!(Self::forks(&cache), kept);
    }

    fn forks(cache: &Path) -> Vec<PathBuf> {
        let mut forks = Vec::new();
        for dir in std::fs::read_dir(cache.join("molt")).unwrap() {
            let Ok(work) = std::fs::read_dir(dir.unwrap().path().join("work")) else { continue };
            // A fork is a directory; the files beside it are its records.
            forks.extend(work.map(|e| e.unwrap().path()).filter(|p| p.is_dir() && !p.ends_with(".molt")));
        }
        forks.sort();
        forks
    }
}

/// What a person picks in a permission prompt: the first option of `kind`.
fn pick(asked: &Value, kind: &str) -> Value {
    let option = asked["options"].as_array().unwrap().iter().find(|o| o["kind"] == kind).expect("no such option");
    json!({"outcome": {"outcome": "selected", "optionId": option["optionId"]}})
}

/// The text between the first two backticks of `text`.
fn quoted(text: &str) -> &str {
    text.split('`').nth(1).unwrap_or_else(|| panic!("nothing quoted in {text}"))
}

fn text_of(update: &Value) -> String {
    let texts = update["content"].as_array().into_iter().flatten();
    texts.filter_map(|c| c["content"]["text"].as_str()).collect()
}

/// An editor runs a task through `molt acp` as Zed does: it initializes,
/// opens a session in a folder, prompts, sees the run's plan, tool calls and
/// messages, and is asked before anything is applied. Allowed, the result is
/// applied; in another folder, kept aside, nothing is.
#[tokio::test]
async fn an_editor_runs_a_task_through_molt_acp_and_only_a_result_it_allows_is_applied() {
    let server = fake_api(Fake::Greets).await;
    let (one, two) = (Setup::new(&server), Setup::new(&server));
    let data = tempfile::tempdir().unwrap();
    let mut editor = Editor::start(&server, data.path());
    let hello = editor
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}, "clientInfo": {"name": "test-editor", "version": "1"}}),
        )
        .await;
    assert_eq!(hello["result"]["protocolVersion"], 1, "{hello}");
    let relative = editor.call("session/new", json!({"cwd": "greeting", "mcpServers": []})).await;
    assert_eq!(relative["error"]["code"], -32602, "the folder must be an absolute path: {relative}");
    let ws = one.workspace.path().canonicalize().unwrap();
    let opened = editor.call("session/new", json!({"cwd": ws, "mcpServers": []})).await;
    let session = opened["result"]["sessionId"].as_str().expect("a session").to_owned();

    let prompt =
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "Write a greeting into greeting.txt."}]});
    let id = editor.send("session/prompt", prompt).await;
    let mut asked = Vec::new();
    let answer = editor
        .answer(id, |prompt| {
            assert!(one.file("greeting.txt").is_none(), "nothing is applied before the person says so");
            asked.push(prompt.clone());
            pick(prompt, "allow_once")
        })
        .await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(one.file("greeting.txt").as_deref(), Some("hello\n"), "applied once allowed");
    let [asked] = asked.as_slice() else { panic!("asked once: {asked:?}") };
    assert_eq!(asked["sessionId"], session.as_str());
    let options: Vec<(&str, &str)> = asked["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["optionId"].as_str().unwrap(), o["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(options, [("apply", "allow_once"), ("keep", "reject_once"), ("discard", "reject_once")]);
    let content = asked["toolCall"]["content"].as_array().unwrap();
    let diff = content.iter().find(|c| c["type"] == "diff" && c["path"] == ws.join("greeting.txt").to_str().unwrap());
    let diff = diff.unwrap_or_else(|| panic!("a diff of greeting.txt: {content:#?}"));
    assert_eq!((diff["newText"].as_str(), diff.get("oldText")), (Some("hello\n"), None), "{diff}");

    let updates = editor.updates.clone();
    let of = |kind: &'static str| updates.iter().filter(move |u| u["sessionUpdate"] == kind);
    let write =
        of("tool_call").find(|u| u["title"] == "write greeting.txt (attempt 1)" || u["title"] == "write greeting.txt");
    let write = write.unwrap_or_else(|| panic!("the attempt's write: {updates:#?}"));
    assert_eq!((write["kind"].as_str(), write["status"].as_str()), (Some("edit"), Some("in_progress")), "{write}");
    let ended = of("tool_call_update").any(|u| u["toolCallId"] == write["toolCallId"] && u["status"] == "completed");
    assert!(ended, "the write's call ends: {updates:#?}");
    assert!(
        of("tool_call")
            .any(|u| u["title"].as_str().is_some_and(|t| t.starts_with("Run the check")) && u["status"] == "completed"),
        "{updates:#?}"
    );
    let plans: Vec<&Value> = of("plan").collect();
    let last = plans.last().expect("a plan");
    let steps: Vec<(&str, &str)> = last["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["content"].as_str().unwrap(), e["status"].as_str().unwrap()))
        .collect();
    assert_eq!(steps[0], ("Design the check the result must pass", "completed"), "the router picked verify: {steps:?}");
    assert!(steps.iter().all(|(_, status)| *status == "completed"), "{steps:?}");
    let said: String = of("agent_message_chunk").filter_map(|u| u["content"]["text"].as_str()).collect();
    assert!(said.contains("Passed its check `sh check.sh`."), "{said}");
    let applied =
        of("tool_call_update").find(|u| u["toolCallId"] == asked["toolCall"]["toolCallId"]).expect("the apply ends");
    // The designed check's file is part of the result, as with `molt do`.
    assert_eq!(
        (applied["status"].as_str(), text_of(applied).as_str()),
        (Some("completed"), "Applied 2 files."),
        "{applied}"
    );
    assert_eq!(one.file("check.sh").as_deref(), Some("grep -q hello greeting.txt\n"));

    // In another folder, a server of its own; the person keeps the result aside.
    let ws2 = two.workspace.path().canonicalize().unwrap();
    let opened = editor.call("session/new", json!({"cwd": ws2, "mcpServers": []})).await;
    let other = opened["result"]["sessionId"].as_str().expect("a session").to_owned();
    assert_ne!(other, session);
    editor.updates.clear();
    let prompt =
        json!({"sessionId": other, "prompt": [{"type": "text", "text": "Write a greeting into greeting.txt."}]});
    let id = editor.send("session/prompt", prompt).await;
    let answer = editor.answer(id, |prompt| pick(prompt, "reject_once")).await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(two.file("greeting.txt").is_none(), "nothing applied");
    assert!(one.file("greeting.txt").is_some(), "the first folder is left alone");
    let kept =
        editor.updates.iter().rev().find(|u| u["sessionUpdate"] == "tool_call_update").expect("the apply ends").clone();
    assert_eq!(kept["status"], "failed", "{kept}");
    let said = text_of(&kept);
    assert!(said.starts_with("Not applied. The result is kept in `"), "{said}");
    let fork = PathBuf::from(quoted(&said));
    assert_eq!(std::fs::read_to_string(fork.join("greeting.txt")).unwrap(), "hello\n");

    editor.close_and_check(&[fork]).await;
    assert_requests_valid(&server).await;
}

/// Cancelling in the editor stops the run and keeps what its attempts did:
/// the prompt ends as cancelled, the attempts' forks are named and hold
/// their writes, nothing is applied, and the next prompt runs.
#[tokio::test]
async fn cancelling_in_the_editor_stops_the_task_and_keeps_the_attempts_work() {
    let server = fake_api(Fake::GreetsWithCalls(vec![(RUN, json!({ "command": "sleep 300" }))])).await;
    let setup = Setup::new(&server);
    let data = tempfile::tempdir().unwrap();
    let mut editor = Editor::start(&server, data.path());
    editor.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let ws = setup.workspace.path().canonicalize().unwrap();
    let opened = editor.call("session/new", json!({"cwd": ws, "mcpServers": []})).await;
    let session = opened["result"]["sessionId"].as_str().expect("a session").to_owned();
    let prompt =
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "Write a greeting into greeting.txt."}]});
    let mut kept = Vec::new();
    for round in 0..2 {
        editor.updates.clear();
        let id = editor.send("session/prompt", prompt.clone()).await;
        let sleeping = |u: &Value| {
            u["sessionUpdate"] == "tool_call" && u["title"].as_str().is_some_and(|t| t.starts_with("run sleep 300"))
        };
        editor.update(sleeping).await;
        editor.line(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})).await;
        let answer = editor.answer(id, |asked| panic!("nothing to ask: {asked}")).await;
        assert_eq!(answer["result"]["stopReason"], "cancelled", "round {round}: {answer}");
        let said: String = editor
            .updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|u| u["content"]["text"].as_str())
            .collect();
        assert!(said.starts_with("Stopped. What the attempts did is kept, and nothing was applied: "), "{said}");
        let forks: Vec<PathBuf> =
            said.split('`').skip(1).step_by(2).filter(|f| f.starts_with('/')).map(PathBuf::from).collect();
        assert!(!forks.is_empty(), "{said}");
        for fork in &forks {
            assert_eq!(std::fs::read_to_string(fork.join("greeting.txt")).unwrap(), "hello\n", "{}", fork.display());
        }
        let stopped =
            editor.updates.iter().any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed");
        assert!(stopped, "the calls still going end: {:#?}", editor.updates);
        assert!(setup.file("greeting.txt").is_none());
        kept.extend(forks);
    }
    kept.sort();
    editor.close_and_check(&kept).await;
    assert_requests_valid(&server).await;
}

/// An attempt's question (`ask_user`) is put to the person in the editor
/// with its options, and the one they pick is the answer the attempt reads.
#[tokio::test]
async fn a_question_an_attempt_asks_is_put_to_the_editor_and_the_pick_reaches_the_attempt() {
    use molt_api::planner::tools::ASK_USER;
    let ask = json!({ "question": "Which greeting?", "options": ["hello there", "hi there"] });
    let server = fake_api(Fake::GreetsWithCalls(vec![(ASK_USER, ask)])).await;
    let setup = Setup::new(&server);
    let data = tempfile::tempdir().unwrap();
    let mut editor = Editor::start(&server, data.path());
    editor.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
    let ws = setup.workspace.path().canonicalize().unwrap();
    let opened = editor.call("session/new", json!({"cwd": ws, "mcpServers": []})).await;
    let session = opened["result"]["sessionId"].as_str().expect("a session").to_owned();
    let prompt =
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "Write a greeting into greeting.txt."}]});
    let id = editor.send("session/prompt", prompt).await;
    let mut questions = Vec::new();
    let answer = editor
        .answer(id, |asked| {
            if asked["toolCall"]["kind"] != "think" {
                return pick(asked, "allow_once");
            }
            questions.push(asked.clone());
            let option = asked["options"].as_array().unwrap().iter().find(|o| o["name"] == "hi there").unwrap();
            json!({"outcome": {"outcome": "selected", "optionId": option["optionId"]}})
        })
        .await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(!questions.is_empty(), "the attempts' questions were asked");
    for asked in &questions {
        assert_eq!(asked["toolCall"]["title"], "Which greeting?");
        let names: Vec<&str> =
            asked["options"].as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["hello there", "hi there", "Let it decide"]);
    }
    let answered =
        editor.updates.iter().filter(|u| u["sessionUpdate"] == "tool_call_update" && text_of(u) == "Answer: hi there");
    assert_eq!(answered.count(), questions.len(), "{:#?}", editor.updates);
    let read = bodies(&server).await.iter().any(|body| {
        let messages = body["messages"].as_array().into_iter().flatten();
        let blocks = messages.filter_map(|m| m["content"].as_array()).flatten();
        blocks.filter(|b| b["type"] == "tool_result").any(|b| b["content"].to_string().contains("hi there"))
    });
    assert!(read, "the attempt read the answer");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    editor.close_and_check(&[]).await;
    assert_requests_valid(&server).await;
}

/// `!cmd` in the interface: the session runs it through the shell service in
/// a fork of the workspace, in the sandbox the attempts get, streams its
/// output, stops it when asked, calls no model, and the audit log has it.
#[tokio::test]
async fn a_session_command_streams_its_output_from_the_sandbox_and_is_audited() {
    use tokio_util::sync::CancellationToken;
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let (events, mut progress) = tokio::sync::mpsc::unbounded_channel();
    let session = start_session(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    let trace = molt_proto::TraceId::random();
    // As the shell's own sandbox tests check it: the sandbox's home, and no host secret in reach.
    let secret = setup.data.path().join("host-secret");
    std::fs::write(&secret, "outside").unwrap();
    let command = format!(
        "echo hi && test \"$HOME\" = /home/molt && ! cat '{}' && echo kept-not > written.txt && echo oops >&2",
        secret.display()
    );
    let ran = session.command(&command, &trace, CancellationToken::new()).await.unwrap();
    assert!(ran.success(), "{ran:?}");
    assert_eq!(ran.stdout, "hi\n");
    let mut streamed = Vec::new();
    // Published before the reply, the events may still be on their way to this channel.
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), progress.recv()).await {
        if let Progress::Output { run, text, stderr, .. } = event {
            assert_eq!(run, trace.as_str());
            streamed.push((text, stderr));
        }
    }
    // Chunks that arrive together go out as one event, so read each stream whole.
    let stream = |err: bool| streamed.iter().filter(|(_, e)| *e == err).map(|(t, _)| t.as_str()).collect::<String>();
    assert_eq!(stream(false), "hi\n", "{streamed:?}");
    assert!(stream(true).lines().any(|line| line == "oops"), "{streamed:?}");
    assert!(!workspace.join("written.txt").exists(), "the command ran in a fork that was dropped");
    assert_eq!(setup.files(), ["README.txt"]);

    // Stopped while it runs, the shell says it stopped it.
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        stopping.cancel();
    });
    let started = Instant::now();
    let ran = session.command("sleep 30", &molt_proto::TraceId::random(), stop).await.unwrap();
    assert!(ran.cancelled && !ran.success(), "{ran:?}");
    assert!(started.elapsed() < Duration::from_secs(20));
    session.shutdown(&[]).await;

    assert!(bodies(&server).await.is_empty(), "no model was called");
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let audited = audit.iter().any(|e| match &e.event {
        AuditEvent::Message { envelope } => {
            envelope.trace_id == trace
                && matches!(&envelope.to, molt_proto::Target::Method { .. })
                && envelope.to.to_string() == RUN_CHECK
                && envelope.payload["command"] == command.as_str()
        }
        _ => false,
    });
    assert!(audited, "the command's shell.run is in the audit log");
}

/// `/diff` after a result is applied lists the files it changed.
#[tokio::test]
async fn changes_against_head_list_what_an_applied_result_changed() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let w = setup.workspace.path();
    git(w, &["init", "-q", "-b", "main"]);
    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com"), ("commit.gpgsign", "false")] {
        git(w, &["config", key, value]);
    }
    git(w, &["add", "."]);
    git(w, &["commit", "-q", "-m", "start"]);
    let (response, _) = setup.run(setup.request(Some(CHECK), 1)).await;
    assert!(response.applied, "{response:#?}");
    let repo = molt::git::Repo::open(w).unwrap();
    let changes = repo.changes().unwrap();
    assert_eq!(changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["greeting.txt"]);
    assert!(changes[0].untracked && changes[0].lines == Some((1, 0)), "{changes:?}");
    assert!(repo.file_diff(&changes[0]).unwrap().patch.contains("+hello"));
}

/// A prompt pack the planner is pointed at: its override is what the model
/// is told, and the kernel's audit log has each call's role and prompt hash.
#[tokio::test]
async fn an_override_in_the_prompt_pack_reaches_the_model_and_its_hash_the_audit_log() {
    let server = fake_api(Fake::Greets).await;
    let mut setup = Setup::new(&server);
    let pack = tempfile::tempdir().unwrap();
    let text = "You greet. Write greeting.txt and stop.";
    std::fs::write(pack.path().join("attempt.md"), format!("{text}\n")).unwrap();
    let planner = setup.cfg.service_mut("planner").unwrap();
    planner.env.insert(molt_planner::pack::PROMPTS_ENV.into(), pack.path().to_str().unwrap().into());
    let before = bodies(&server).await.len();
    let (resp, _) = setup.run(setup.request(Some(CHECK), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    let hash = molt_planner::pack::sha256(text);
    let used = resp.prompts.iter().find(|p| p.role == "attempt").expect("the attempt's prompt is in the reply");
    assert_eq!((used.prompt.sha256.as_str(), used.prompt.source), (hash.as_str(), molt_api::role::Source::User));

    let sent = bodies(&server).await;
    let attempts: Vec<&Value> = sent[before..]
        .iter()
        .filter(|b| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == WRITE_FILE)))
        .collect();
    assert!(!attempts.is_empty());
    for body in &attempts {
        let system = body["system"][0]["text"].as_str().unwrap();
        assert!(system.starts_with(&format!("{text}\n\nThe done-check:")), "{system}");
        assert!(body.get("prompt").is_none(), "the version is for the audit log, not the model API");
    }
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let audited: Vec<&Value> = audit
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::Message { envelope }
                if envelope.kind == Kind::Request && envelope.to.to_string() == COMPLETE =>
            {
                Some(&envelope.payload)
            }
            _ => None,
        })
        .filter(|p| p["prompt"]["role"] == "attempt")
        .collect();
    assert!(!audited.is_empty());
    for payload in audited {
        assert_eq!(payload["prompt"]["prompt"], json!({ "name": "attempt", "sha256": hash, "source": "user" }));
    }
    assert_requests_valid(&server).await;
}

/// A service of the user's config that declares a tool: the planner offers
/// it when the config grants the planner its method, and the call goes
/// through the kernel, which writes it to the audit log.
#[tokio::test]
async fn a_tool_a_configured_service_declares_is_offered_called_and_audited() {
    use molt_proto::tool::ToolDecl;
    let server = fake_api(Fake::GreetsWithTool("echo_back")).await;
    for granted in [false, true] {
        let mut setup = Setup::new(&server);
        let echo = molt_proto::Exec { command: env!("CARGO_BIN_EXE_molt-echo").into(), args: vec![] };
        let mut svc = molt::config::ServiceSection::new(
            ServiceId::new("echo").unwrap(),
            molt_proto::Tier::Mutable,
            echo,
            vec!["echo.*".parse().unwrap()],
        );
        svc.tools = vec![serde_json::from_value::<ToolDecl>(json!({
            "name": "echo_back",
            "description": "Repeat the input back.",
            "input_schema": { "type": "object", "properties": { "text": { "type": "string" } } },
            "target": "echo.echo",
            "read_only": true,
        }))
        .unwrap()];
        setup.cfg.services.push(svc);
        if granted {
            let planner = setup.cfg.service_mut("planner").unwrap();
            let grant = molt_proto::CapRequest {
                target: "echo.echo".parse().unwrap(),
                budget: molt_proto::Budget::new(0, 0, 100),
            };
            planner.requests.push(grant);
        }
        let before = bodies(&server).await.len();
        let (resp, _) = setup.run(setup.request(Some(CHECK), 1)).await;
        assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
        let sent = bodies(&server).await;
        let offered = |b: &Value| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == "echo_back"));
        // Attempts are offered it; the reviewer, for whom it is not declared, is not.
        let attempt = |b: &&Value| {
            b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == WRITE_FILE) && !offers_submit_check(b))
        };
        let attempts: Vec<&Value> = sent[before..].iter().filter(attempt).collect();
        assert!(!attempts.is_empty());
        assert!(attempts.iter().all(|b| offered(b) == granted), "granted: {granted}");
        assert!(sent[before..].iter().filter(|b| !attempt(b)).all(|b| !offered(b)));
        let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
        let calls: Vec<&molt_proto::Envelope> = audit
            .iter()
            .filter_map(|e| match &e.event {
                AuditEvent::Message { envelope } if envelope.to.to_string() == "echo.echo" => Some(envelope),
                _ => None,
            })
            .collect();
        if !granted {
            assert!(calls.is_empty());
            continue;
        }
        let request = calls.iter().find(|e| e.kind == Kind::Request).expect("the tool call is in the audit log");
        assert_eq!(request.from.as_ref().map(|f| f.as_str()), Some("planner"));
        assert_eq!(request.payload["tool"], "echo_back");
        assert_eq!(request.payload["input"], json!({ "text": "hi" }));
        // The attempt got the service's answer back, marked as data.
        let result = sent[before..]
            .iter()
            .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .find(|c| c["type"] == "tool_result" && c["content"].as_str().is_some_and(|t| t.contains("echo_back")))
            .expect("the tool's result went back to the model");
        assert!(result["content"].as_str().unwrap().contains("<tool_output tool=\"echo_back\">"), "{result}");
    }
    assert_requests_valid(&server).await;
}

/// An MCP server of the user's config, bridged as service `mcp-fake`: its
/// tools are offered to attempts as `mcp__fake__*`, every call goes through
/// the kernel into the audit log, a call beyond the planner's budget for it
/// is refused by the kernel, and a server that crashes gives its call an
/// error result while the run goes on. With `calls = 0` nothing is offered.
#[tokio::test]
async fn an_mcp_servers_tools_are_called_through_the_kernel_within_their_budget_and_survive_a_crash() {
    let calls = vec![
        ("mcp__fake__echo", json!({ "text": "hi" })),
        ("mcp__fake__crash", json!({})),
        ("mcp__fake__echo", json!({ "text": "again" })),
    ];
    let server = fake_api(Fake::GreetsWithCalls(calls)).await;
    for budget in [0, 2] {
        let mut setup = Setup::new(&server);
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().canonicalize().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../molt-mcp/tests/fake_mcp_server.py");
        std::fs::copy(script, dir_path.join("server.py")).unwrap();
        let text = format!(
            "[mcp.fake]\ncommand = \"/usr/bin/python3\"\nargs = [\"{0}/server.py\", \"{0}\"]\nmounts = [\"{0}\"]\n\
             calls = {budget}\n[mcp.fake.tools.echo]\nread_only = true\n",
            dir_path.display()
        );
        setup.cfg.mcp = Config::parse(&text).unwrap().mcp;
        setup.cfg.install_mcp(&bin_dir()).unwrap();
        let before = bodies(&server).await.len();
        let (resp, _) = setup.run(setup.request(Some(CHECK), 1)).await;
        assert_eq!(resp.outcome, Outcome::Passed, "the run goes on: {}", resp.summary);
        let sent = bodies(&server).await;
        let offered =
            |b: &Value| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == "mcp__fake__echo"));
        let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
        let to_bridge = |e: &&AuditEvent| match e {
            AuditEvent::Message { envelope } => envelope.to.to_string() == "mcp-fake.call",
            AuditEvent::Denied { to, .. } => to.to_string() == "mcp-fake.call",
            _ => false,
        };
        let events: Vec<&AuditEvent> = audit.iter().map(|e| &e.event).filter(to_bridge).collect();
        if budget == 0 {
            assert!(sent[before..].iter().all(|b| !offered(b)), "a server with calls = 0 offers nothing");
            assert!(events.is_empty());
            continue;
        }
        assert!(sent[before..].iter().any(offered));
        let requests: Vec<&molt_proto::Envelope> = events
            .iter()
            .filter_map(|e| match e {
                AuditEvent::Message { envelope } if envelope.kind == Kind::Request => Some(envelope),
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 2, "the third call is refused before delivery");
        assert!(requests.iter().all(|r| r.from.as_ref().map(|f| f.as_str()) == Some("planner")));
        assert_eq!(requests[0].payload["tool"], "mcp__fake__echo");
        assert_eq!(requests[0].payload["input"], json!({ "text": "hi" }));
        let denied = events.iter().find_map(|e| match e {
            AuditEvent::Denied { from, code, .. } => Some((from.as_str().to_owned(), *code)),
            _ => None,
        });
        assert_eq!(denied, Some(("planner".to_owned(), molt_proto::ErrorCode::OverBudget)), "{events:?}");
        let results: Vec<Value> = sent[before..]
            .iter()
            .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .filter(|c| c["type"] == "tool_result" && c["content"].as_str().is_some_and(|t| t.contains("mcp")))
            .collect();
        let result = |needle: &str| {
            results
                .iter()
                .find(|r| r["content"].as_str().unwrap().contains(needle))
                .unwrap_or_else(|| panic!("no tool result with {needle:?} in {results:#?}"))
        };
        let echo = result("echo: hi");
        assert!(echo["content"].as_str().unwrap().contains("<tool_output tool=\"mcp__fake__echo\">"));
        assert!(echo["content"].as_str().unwrap().contains("data from outside the workspace"), "{echo}");
        assert_eq!(result("crashing on purpose")["is_error"], true);
        assert_eq!(results.iter().filter(|r| r["is_error"] == true).count(), 2, "{results:#?}");
    }
    assert_requests_valid(&server).await;
}

/// Agent Skills (#166): a user skill is named in the first message and its
/// body comes through load_skill; a project skill's script is read as text
/// and never runs, even when the model tries; the run's recall snapshot
/// records both as offered.
#[tokio::test]
async fn a_users_skill_is_offered_by_name_and_read_through_load_skill_and_a_projects_script_never_runs() {
    use molt_api::planner::tools::LOAD_SKILL;
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("ran");
    let script = format!("touch '{}'\n", marker.display());
    let calls = vec![
        (LOAD_SKILL, json!({ "name": "greet-style" })),
        (LOAD_SKILL, json!({ "name": "deploy", "file": "scripts/deploy.sh" })),
        (RUN, json!({ "command": "sh .molt/skills/deploy/scripts/deploy.sh" })),
        (LOAD_SKILL, json!({ "name": "deploy", "file": "../../config.toml" })),
    ];
    let server = fake_api(Fake::GreetsWithCalls(calls)).await;
    let mut setup = Setup::new(&server);
    let user = tempfile::tempdir().unwrap();
    std::fs::create_dir(user.path().join("greet-style")).unwrap();
    std::fs::write(
        user.path().join("greet-style/SKILL.md"),
        "---\nname: greet-style\ndescription: How this user likes greetings written.\n---\nSay hello in lowercase.\n",
    )
    .unwrap();
    let project = setup.workspace.path().join(".molt/skills/deploy");
    std::fs::create_dir_all(project.join("scripts")).unwrap();
    std::fs::write(
        project.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the app.\n---\nAlways run scripts/deploy.sh first.\n",
    )
    .unwrap();
    std::fs::write(project.join("scripts/deploy.sh"), &script).unwrap();
    std::fs::set_permissions(project.join("scripts/deploy.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let planner = setup.cfg.service_mut("planner").unwrap();
    planner.env.insert(molt_planner::skills::SKILLS_ENV.into(), user.path().to_str().unwrap().into());

    let (resp, events) = setup.run(setup.request(Some(CHECK), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    assert!(events.iter().any(
        |e| matches!(e, Progress::Note { message, .. } if message == "skills offered: greet-style (user), deploy (project)")
    ));
    let sent = bodies(&server).await;
    let first = sent
        .iter()
        .find(|b| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == LOAD_SKILL)))
        .expect("an attempt was offered load_skill");
    let opening = first["messages"][0].to_string();
    assert!(opening.contains("- greet-style: How this user likes greetings written."), "{opening}");
    assert!(opening.contains("- deploy: Deploy the app."), "{opening}");
    assert!(!opening.contains("Say hello in lowercase"), "only names and descriptions come first");
    let all = sent.iter().map(|b| b["messages"].to_string()).collect::<String>();
    assert!(all.contains("Say hello in lowercase."), "load_skill returned the user's skill");
    assert!(all.contains("The project skill deploy") && all.contains("do not run its scripts"));
    assert!(all.contains(script.trim()), "the script came back as text");
    assert!(all.contains("without `..`"), "a path out of the skill is refused");
    assert!(!marker.exists(), "the project's script never ran");
    assert!(!setup.workspace.path().join("ran").exists());

    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let run = audit
        .iter()
        .find_map(|e| match &e.event {
            AuditEvent::Message { envelope }
                if envelope.kind == Kind::Request && envelope.to.to_string() == PLANNER_RUN =>
            {
                Some(envelope.trace_id.to_string())
            }
            _ => None,
        })
        .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .args(["memory", "used", &run, "--json", "--data-dir"])
        .arg(setup.data.path())
        .env("XDG_DATA_HOME", setup.data.path().join("user-data"))
        .current_dir(setup.workspace.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let captured: Vec<molt_api::memory::RecallSnapshot> = serde_json::from_slice(&out.stdout).unwrap();
    let offered: Vec<(&str, &str)> =
        captured[0].skills.iter().map(|s| (s.name.as_str(), s.location.as_str())).collect();
    let user_md = user.path().join("greet-style/SKILL.md").canonicalize().unwrap();
    assert_eq!(offered, [("greet-style", user_md.to_str().unwrap()), ("deploy", ".molt/skills/deploy/SKILL.md")]);
    assert_requests_valid(&server).await;
    setup.assert_forks_dropped();
}

#[tokio::test]
async fn molt_do_plan_runs_each_given_step_with_its_own_designed_check_then_the_overall_one() {
    let server = fake_api(Fake::DesignsWithoutFiles).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let args = [
        "--step",
        "Write the greeting.",
        "--step",
        "Check that the greeting is still there.",
        "--check",
        CHECK,
        "--attempts",
        "1",
        "--json",
        "--no-learn",
    ];
    let out = tokio::time::timeout(RUN_LIMIT, molt_do(&server, workspace.path(), data.path(), &args).output())
        .await
        .expect("molt do hung")
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "status {:?}\nstdout:\n{}\nstderr:\n{stderr}",
        out.status,
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stderr.contains("plan: 2 steps: 1. Write the greeting."), "{stderr}");
    assert!(stderr.contains("step 1/2: Write the greeting."), "{stderr}");
    assert!(stderr.contains("step 2/2: Check that the greeting is still there."), "{stderr}");
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["outcome"], "passed", "{json:#}");
    assert_eq!(json["check"]["command"], CHECK, "the given check is the overall one");
    assert_eq!(json["applied"], true, "{json:#}");
    let ran = json["plan"]["ran"].as_array().unwrap();
    assert_eq!(ran.len(), 2, "{json:#}");
    for step in ran {
        assert_eq!(step["run"]["outcome"], "passed", "{step:#}");
        // Each step designed a check of its own.
        assert_eq!(step["run"]["check"]["designed"], true, "{step:#}");
    }
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
    // Every step, and the plan itself, is in the audit log under the plan's trace.
    let audit = molt_kernel::audit::read_all(&data.path().join("audit.jsonl")).await.unwrap();
    let plans: Vec<_> = audit
        .iter()
        .filter_map(|e| match &e.event {
            AuditEvent::Message { envelope }
                if envelope.kind == Kind::Request && envelope.to.to_string() == molt_api::plan::PLAN =>
            {
                Some(envelope)
            }
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].payload["steps"][1], "Check that the greeting is still there.");
    // And so is its answer: each step's check and result.
    assert!(audit.iter().any(|e| matches!(&e.event, AuditEvent::Message { envelope }
        if envelope.kind == Kind::Reply && envelope.payload["ran"].as_array().is_some_and(|r| r.len() == 2))));
    assert_requests_valid(&server).await;
    assert_forks_dropped(data.path());
}

/// Background processes (#110) through the real kernel and shell service:
/// with `MOLT_PROCESSES=on` an attempt starts a server that runs on in its
/// fork, the start says it is ready, the planner stops it at the attempt's
/// final reply before the check, and nothing of it is left running.
#[tokio::test]
async fn a_process_an_attempt_starts_runs_until_its_final_reply_and_leaves_nothing_behind() {
    use molt_api::planner::tools::START_PROCESS;
    let marker = format!("molt-process-test-{}", std::process::id());
    let command = format!("echo up; while :; do echo {marker} > /dev/null; sleep 0.05; done");
    let calls = vec![(START_PROCESS, json!({ "command": command, "ready": "up", "timeout_s": 20 }))];
    let server = fake_api(Fake::GreetsWithCalls(calls)).await;
    let mut setup = Setup::new(&server);
    setup.cfg.service_mut("planner").unwrap().env.insert("MOLT_PROCESSES".into(), "on".into());
    let (resp, _) = setup.run(setup.request(Some(CHECK), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);

    let sent = bodies(&server).await;
    let started = sent
        .iter()
        .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|c| c["type"] == "tool_result" && c["content"].as_str().is_some_and(|t| t.starts_with("Started process")))
        .expect("the start's result went back to the model");
    let text = started["content"].as_str().unwrap();
    assert!(
        text.contains("is running") && text.contains("Its ready text appeared.") && text.contains("up\n"),
        "{text}"
    );
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let requests = |to: &str| {
        audit
            .iter()
            .filter(|e| {
                matches!(&e.event, AuditEvent::Message { envelope }
                if envelope.kind == Kind::Request && envelope.to.to_string() == to
                    && envelope.from.as_ref().map(|f| f.as_str()) == Some("planner"))
            })
            .count()
    };
    assert_eq!(requests("shell.start"), 1);
    assert!(requests("shell.stop") >= 2, "stopped at the final reply and at the attempt's end");
    // Nothing that ran the command is left: not the sandbox, not the shell in it.
    let alive: Vec<String> = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())))
        .filter(|e| {
            std::fs::read(e.path().join("cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains(&marker))
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(alive.is_empty(), "processes left running: {alive:?}");
    assert_requests_valid(&server).await;
    assert_forks_dropped(setup.data.path());
}

/// Git history (#111) through the real kernel and fs service: with
/// `MOLT_GIT=on` an attempt in a fork finds the commit that broke a
/// function by its text and reads its patch, the queries are the
/// planner's `fs.git` requests in the audit log, and the repository is
/// exactly as it was, its index and the user's change not committed included.
#[tokio::test]
async fn an_attempt_reads_the_commit_that_introduced_a_regression_and_the_repository_is_untouched() {
    use molt_api::planner::tools::GIT;
    let calls = vec![
        (GIT, json!({ "query": "log", "search": "a + b" })),
        (GIT, json!({ "query": "show", "rev": "HEAD~1", "path": "calc.py" })),
        (GIT, json!({ "query": "status" })),
    ];
    let server = fake_api(Fake::GreetsWithCalls(calls)).await;
    let mut setup = Setup::new(&server);
    let ws = setup.workspace.path().to_path_buf();
    let commit = |files: &[(&str, &str)], message: &str| {
        for (path, content) in files {
            std::fs::write(ws.join(path), content).unwrap();
        }
        git(&ws, &["add", "-A"]);
        git(&ws, &["-c", "user.name=Ann", "-c", "user.email=ann@example.com", "commit", "-q", "-m", message]);
    };
    git(&ws, &["init", "-q", "-b", "main"]);
    commit(&[("calc.py", "def add(a, b):\n    return a + b\n")], "Add add()");
    commit(&[("calc.py", "def add(a, b):\n    return a - b\n")], "Tidy up calc");
    commit(&[("notes.txt", "later\n")], "Add notes");
    std::fs::write(ws.join("README.txt"), "A test project, edited.\n").unwrap();
    let state = |ws: &Path| {
        let index = std::fs::read(ws.join(".git/index")).unwrap();
        (git(ws, &["status", "--porcelain"]), git(ws, &["rev-parse", "HEAD"]), index)
    };
    let before = state(&ws);
    setup.cfg.service_mut("planner").unwrap().env.insert("MOLT_GIT".into(), "on".into());
    let (resp, _) = setup.run(setup.request(Some(CHECK), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);

    let sent = bodies(&server).await;
    let results: Vec<String> = sent
        .iter()
        .flat_map(|b| b["messages"].as_array().cloned().unwrap_or_default())
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|c| c["type"] == "tool_result")
        .filter_map(|c| c["content"].as_str().map(str::to_owned))
        .collect();
    let has = |text: &str| results.iter().any(|r| r.contains(text));
    assert!(has("Tidy up calc") && has("M\tcalc.py"), "the log found the commit: {results:?}");
    assert!(has("-    return a + b\n+    return a - b"), "its patch: {results:?}");
    assert!(has("modified README.txt") && has("added    greeting.txt"), "theirs and the attempt's: {results:?}");
    let audit = molt_kernel::audit::read_all(&setup.data.path().join("audit.jsonl")).await.unwrap();
    let queries = audit
        .iter()
        .filter(|e| {
            matches!(&e.event, AuditEvent::Message { envelope }
            if envelope.kind == Kind::Request && envelope.to.to_string() == molt_api::fs::GIT
                && envelope.from.as_ref().map(|f| f.as_str()) == Some("planner"))
        })
        .count();
    assert_eq!(queries, 3);
    // The result was applied; the repository's history, index and the user's own change are as they were.
    let (status, head, index) = state(&ws);
    assert_eq!((head, index), (before.1, before.2));
    assert_eq!(status, format!("{}?? greeting.txt\n", before.0));
    assert_requests_valid(&server).await;
    assert_forks_dropped(setup.data.path());
}

/// `molt ext ARGS` with the user's config, packs and caches under `home`,
/// with no terminal.
fn molt_ext(home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .arg("ext")
        .args(args)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("user-data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Extension packs (#170) through the real binary: `molt ext add` lists
/// what a pack would get and runs nothing; it installs only on approval;
/// the pack's MCP server is offered to a `molt do` run; once disabled its
/// tools are not; an update that asks for more is not installed without
/// another approval; and `molt ext remove` takes everything it added.
#[tokio::test]
async fn an_extension_pack_is_shown_approved_offered_disabled_kept_to_its_approval_and_removed() {
    let calls = vec![("mcp__fakepack__echo", json!({ "text": "hi" }))];
    let server = fake_api(Fake::GreetsWithCalls(calls)).await;
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("ran");
    let src = home.path().join("src");
    std::fs::create_dir_all(src.join("skills/greet-style")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../molt-mcp/tests/fake_mcp_server.py");
    std::fs::copy(script, src.join("server.py")).unwrap();
    // Were anything of the pack run on install, this would leave the marker.
    std::fs::write(src.join("install.sh"), format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(src.join("install.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        src.join("skills/greet-style/SKILL.md"),
        "---\nname: greet-style\ndescription: How greetings are written here.\n---\nLowercase.\n",
    )
    .unwrap();
    let manifest = "schema = 1\nname = \"fakepack\"\nversion = \"1.0.0\"\ndescription = \"An echo server.\"\n\
                    [mcp.fakepack]\ncommand = \"/usr/bin/python3\"\n\
                    args = [\"${MOLT_EXTENSION_DIR}/server.py\", \"${MOLT_EXTENSION_DIR}\"]\ncalls = 5\n\
                    [mcp.fakepack.tools.echo]\nread_only = true\n";
    std::fs::write(src.join("molt-extension.toml"), manifest).unwrap();
    let src_arg = src.to_str().unwrap();
    let installed = home.path().join("user-data/molt/extensions/fakepack");
    let state = home.path().join("config/molt/extensions.toml");

    // No terminal and no --yes: shown, not installed.
    let out = molt_ext(home.path(), &["add", src_arg]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    for want in [
        "extension fakepack 1.0.0",
        "MCP server fakepack runs /usr/bin/python3 ${MOLT_EXTENSION_DIR}/server.py ${MOLT_EXTENSION_DIR}",
        "the agent may call MCP server fakepack 5 times a run (mcp__fakepack__*)",
        "MCP server fakepack's tool echo is offered to attempt, as a tool that only reads",
        "skill greet-style",
        "Nothing was installed: approve it at a terminal, or pass --yes.",
    ] {
        assert!(stderr.contains(want), "{want:?} in\n{stderr}");
    }
    assert!(!stderr.contains("network"), "{stderr}");
    assert!(!installed.exists() && !state.exists());

    let out = molt_ext(home.path(), &["add", src_arg, "--yes"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(installed.join("server.py").is_file());
    assert!(!marker.exists(), "installing ran nothing of the pack");
    let list = String::from_utf8_lossy(&molt_ext(home.path(), &["list"]).stdout).into_owned();
    assert!(list.starts_with("fakepack 1.0.0 enabled sha256 "), "{list}");

    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = |data: &Path| {
        let mut cmd = molt_do(&server, workspace.path(), data, &["--check", CHECK, "--attempts", "1", "--no-learn"]);
        cmd.env("XDG_CONFIG_HOME", home.path().join("config")).env("XDG_DATA_HOME", home.path().join("user-data"));
        cmd.output()
    };
    let offered =
        |b: &Value| b["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == "mcp__fakepack__echo"));
    let out = tokio::time::timeout(RUN_LIMIT, run(data.path())).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let sent = bodies(&server).await;
    assert!(sent.iter().any(offered), "the pack's tool is offered");
    let all = sent.iter().map(|b| b["messages"].to_string()).collect::<String>();
    assert!(all.contains("echo: hi"), "the call went to the pack's server and came back");
    assert!(all.contains("greet-style: How greetings are written here."), "the pack's skill is offered");
    assert!(!marker.exists());

    let out = molt_ext(home.path(), &["disable", "fakepack"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let before = sent.len();
    let out = tokio::time::timeout(RUN_LIMIT, run(data.path())).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let sent = bodies(&server).await;
    assert!(sent.len() > before);
    assert!(sent[before..].iter().all(|b| !offered(b)), "a disabled pack's tools are not offered");
    assert!(!sent[before..].iter().any(|b| b["messages"].to_string().contains("greet-style")));
    assert!(molt_ext(home.path(), &["enable", "fakepack"]).status.success());

    // An update that asks for the network is shown as such and not installed without a yes.
    std::fs::write(
        src.join("molt-extension.toml"),
        manifest.replace("1.0.0", "1.1.0").replace("calls = 5", "calls = 5\nnetwork = true"),
    )
    .unwrap();
    let out = molt_ext(home.path(), &["add", src_arg]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("This updates 1.0.0 to 1.1.0. It asks for more than you approved."), "{stderr}");
    assert!(stderr.contains("  + MCP server fakepack has the network"), "{stderr}");
    let shown = String::from_utf8_lossy(&molt_ext(home.path(), &["show", "fakepack"]).stdout).into_owned();
    assert!(shown.starts_with("extension fakepack 1.0.0 (enabled)") && !shown.contains("network"), "{shown}");

    // An archive whose entry leads outside installs nothing.
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    let name = b"../../escaped";
    header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
    header.set_size(2);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, &b"hi"[..]).unwrap();
    let archive = home.path().join("evil.tar");
    std::fs::write(&archive, tar.into_inner().unwrap()).unwrap();
    let out = molt_ext(home.path(), &["add", archive.to_str().unwrap(), "--yes"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("leads outside the pack; nothing was installed"), "{stderr}");
    assert!(!home.path().join("escaped").exists() && !home.path().join("user-data/escaped").exists());

    let cache = home.path().join("cache/molt/proj-0/mcp");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("fakepack.json"), "{}").unwrap();
    let out = molt_ext(home.path(), &["remove", "fakepack"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!installed.exists() && !cache.join("fakepack.json").exists());
    assert!(!std::fs::read_to_string(&state).unwrap().contains("fakepack"));
    let list = String::from_utf8_lossy(&molt_ext(home.path(), &["list"]).stdout).into_owned();
    assert!(list.starts_with("no extension packs installed"), "{list}");
    let out = tokio::time::timeout(RUN_LIMIT, run(data.path())).await.expect("molt do hung").unwrap();
    assert!(out.status.success(), "a run after removal starts: {}", String::from_utf8_lossy(&out.stderr));
    assert_requests_valid(&server).await;
}

/// Registries (#170) through the real binary: `molt ext registry add`
/// records a directory with a `molt-registry.toml`, `molt ext search`
/// finds a pack it lists, and `molt ext add NAME` installs it only once its
/// grant is approved, while a path of that name in the working directory is
/// still installed as a path unless `--registry` says otherwise.
#[test]
fn a_pack_is_found_by_name_in_a_registry_and_installed_as_its_path_would_be() {
    let home = tempfile::tempdir().unwrap();
    let ext = |cwd: &Path, args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_molt"))
            .current_dir(cwd)
            .arg("ext")
            .args(args)
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_DATA_HOME", home.path().join("user-data"))
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.success(), text(&out.stdout), text(&out.stderr))
    };
    let manifest = |name: &str, description: &str| {
        format!("schema = 1\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"{description}\"\n")
    };
    let registry = home.path().join("registry");
    std::fs::create_dir_all(registry.join("packs/notes/skills/tidy")).unwrap();
    std::fs::write(registry.join("packs/notes/molt-extension.toml"), manifest("notes", "Team notes.")).unwrap();
    std::fs::write(
        registry.join("packs/notes/skills/tidy/SKILL.md"),
        "---\nname: tidy\ndescription: Tidy notes.\n---\nTidy.\n",
    )
    .unwrap();
    std::fs::write(
        registry.join("molt-registry.toml"),
        "schema = 1\n[pack.notes]\ndescription = \"Team notes, kept tidy.\"\npath = \"packs/notes\"\n",
    )
    .unwrap();

    let cwd = home.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let (ok, _, err) = ext(&cwd, &["add", "notes", "--yes"]);
    assert!(!ok && err.contains("no registry to look it up in"), "{err}");
    let (ok, out, err) = ext(&cwd, &["registry", "add", "team", registry.to_str().unwrap()]);
    assert!(ok && out.contains("registry team lists 1 packs"), "{out}{err}");
    let (ok, out, _) = ext(&cwd, &["search", "tidy"]);
    assert!(ok && out.contains("notes (team: packs/notes)") && out.contains("Team notes, kept tidy."), "{out}");

    // Without a terminal or --yes nothing is installed; the grant was shown with where it is from.
    let (ok, _, err) = ext(&cwd, &["add", "notes"]);
    assert!(!ok && err.contains("(listed by registry team)") && err.contains("skill tidy"), "{err}");
    assert!(!home.path().join("user-data/molt/extensions/notes").exists());
    let (ok, out, err) = ext(&cwd, &["add", "notes", "--yes"]);
    assert!(ok && out.contains("installed"), "{out}{err}");
    let (_, list, _) = ext(&cwd, &["list"]);
    assert!(list.starts_with("notes 1.0.0 enabled"), "{list}");
    let (_, out, _) = ext(&cwd, &["search"]);
    assert!(out.contains("notes (team: packs/notes) installed"), "{out}");

    // A directory named like the pack, where the command runs, is the path it means.
    std::fs::create_dir_all(cwd.join("notes")).unwrap();
    std::fs::write(cwd.join("notes/molt-extension.toml"), manifest("notes", "My own notes.")).unwrap();
    let (ok, _, err) = ext(&cwd, &["add", "notes", "--yes"]);
    assert!(ok && err.contains(&format!("from {}", cwd.join("notes").canonicalize().unwrap().display())), "{err}");
    let (ok, _, err) = ext(&cwd, &["add", "notes", "--registry", "team", "--yes"]);
    assert!(ok && err.contains("(listed by registry team)"), "{err}");

    let (ok, out, _) = ext(&cwd, &["registry", "remove", "team"]);
    assert!(ok && out.contains("the packs installed from it stay"), "{out}");
    let (_, out, _) = ext(&cwd, &["registry", "list"]);
    assert!(out.starts_with("no registries"), "{out}");
    let (_, list, _) = ext(&cwd, &["list"]);
    assert!(list.starts_with("notes 1.0.0 enabled"), "{list}");
}

/// The interface's server on a pipe, with `cfg` for `workspace`, initialized.
async fn ui_backend(
    cfg: &Config,
    workspace: &Path,
) -> (tokio::process::Child, Rpc<tokio::process::ChildStdout, tokio::process::ChildStdin>) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"))
        .arg("ui-backend")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut rpc = Rpc::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    rpc.line(&json!({"cfg": cfg, "workspace": workspace.to_str().unwrap()})).await;
    rpc.call("initialize", json!({"protocol": 1})).await;
    (child, rpc)
}

/// RFC 0006: a stored session holds every task, reply, decision and cost
/// through a server killed outright and through a normal exit, the next
/// task is told of the earlier ones, and loading it calls no model.
#[tokio::test]
async fn a_stored_session_survives_a_killed_server_and_a_normal_exit_and_loading_it_calls_no_model() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let task = |round: u32| format!("Write a greeting into greeting.txt (round {round}).");
    let submit = |round: u32| {
        let mut req = setup.request(Some(CHECK), 1);
        (req.task, req.workspace, req.apply) = (task(round), String::new(), false);
        json!({"trace": molt_proto::TraceId::random(), "request": req})
    };

    let (mut first, mut rpc) = ui_backend(&setup.cfg, &workspace).await;
    let made = rpc.call("sessions.new", json!({"title": "greetings"})).await;
    let id = made["session"]["id"].as_str().unwrap().to_owned();
    rpc.call("session.open", json!({})).await;
    let one = rpc.call("task.submit", submit(1)).await;
    let kept = one["fork"].as_str().expect("kept in its fork").to_owned();
    rpc.call("result.keep", json!({"fork": kept})).await;
    let two = rpc.call("task.submit", submit(2)).await;
    let waiting = two["fork"].as_str().expect("kept in its fork").to_owned();
    // Killed outright, with a result the person has not decided on.
    first.kill().await.unwrap();
    drop(rpc);

    let calls = server.received_requests().await.unwrap().len();
    let (mut second, mut rpc) = ui_backend(&setup.cfg, &workspace).await;
    let listed = rpc.call("sessions.list", json!({})).await;
    assert_eq!((&listed[0]["id"], &listed[0]["tasks"]), (&json!(id), &json!(2)), "{listed}");
    let loaded = rpc.call("sessions.load", json!({"id": id})).await;
    assert_eq!(server.received_requests().await.unwrap().len(), calls, "loading called no model");
    assert_eq!(loaded["held"], true, "{loaded}");
    let session = &loaded["session"];
    let tasks: Vec<&Value> = session["earlier"].as_array().unwrap().iter().map(|e| &e["task"]).collect();
    assert_eq!(tasks, [&json!(task(1)), &json!(task(2))], "{session}");
    assert_eq!(session["results"].as_array().unwrap().len(), 1, "{session}");
    assert_eq!((&session["results"][0]["fork"], &session["results"][0]["gone"]), (&json!(waiting), &json!(false)));
    let paid = |result: &Value| result["cost_usd"].as_f64().unwrap();
    let known = session["spent"]["known_usd"].as_f64().unwrap();
    assert!(paid(&one) > 0.0 && (known - paid(&one) - paid(&two)).abs() < 1e-9, "{session}");
    assert_eq!(session["spent"]["unsettled_runs"], 0, "{session}");
    assert!(session["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "reply" && !e["conversation"].as_array().unwrap().is_empty()));

    // The next task is told of the earlier ones; then the waiting result is applied.
    rpc.call("session.open", json!({})).await;
    let before = server.received_requests().await.unwrap().len();
    let three = rpc.call("task.submit", submit(3)).await;
    assert_eq!(three["outcome"], "passed", "{three}");
    let told = bodies(&server).await[before..]
        .iter()
        .any(|b| b.to_string().contains("round 1") && b.to_string().contains("round 2"));
    assert!(told, "the third task's model calls were told of the first two");
    let applied = rpc.call("result.apply", json!({"fork": waiting})).await;
    assert_eq!(applied["changes"][0]["path"], "greeting.txt", "{applied}");
    assert_eq!(rpc.call("session.close", json!({"keep": []})).await, json!({}));
    drop(rpc);
    assert!(tokio::time::timeout(Duration::from_secs(15), second.wait()).await.unwrap().unwrap().success());

    // After a normal exit everything is there, applied as it was.
    let (mut third, mut rpc) = ui_backend(&setup.cfg, &workspace).await;
    let session = rpc.call("sessions.load", json!({"id": id})).await["session"].clone();
    let applied: Vec<&Value> = session["earlier"].as_array().unwrap().iter().map(|e| &e["applied"]).collect();
    assert_eq!(applied, [&json!(false), &json!(true), &json!(false)], "{session}");
    let decisions: Vec<&Value> = session["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "decided")
        .map(|e| &e["decision"])
        .collect();
    assert_eq!(decisions, [&json!("kept"), &json!("applied")], "{session}");
    let three_fork = three["fork"].as_str().unwrap();
    assert_eq!(session["results"][0]["fork"], three_fork, "{session}");
    assert!(Path::new(three_fork).is_dir(), "a result the session waits on outlives the close");
    let known = session["spent"]["known_usd"].as_f64().unwrap();
    assert!((known - paid(&one) - paid(&two) - paid(&three)).abs() < 1e-9, "{session}");
    assert!(session["notes"].as_array().unwrap().is_empty(), "{session}");
    rpc.call("sessions.delete", json!({"id": id})).await;
    drop(rpc);
    assert!(tokio::time::timeout(Duration::from_secs(15), third.wait()).await.unwrap().unwrap().success());
    assert_requests_valid(&server).await;
}
