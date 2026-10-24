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
    DISPUTE_CHECK, LIST_FILES, RUN, SUBMIT_CHECK, SUBMIT_JUDGEMENT, SUBMIT_REVIEW, SUBMIT_VETTING, WRITE_FILE,
};
use molt_api::planner::{AttemptStatus, CheckSpec, Outcome, RunRequest, RunResponse, RUN as PLANNER_RUN};
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
    /// Attempts greet and, when they are offered this tool, call it too.
    GreetsWithTool(&'static str),
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
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"));
    cmd.args(args)
        .arg("--data-dir")
        .arg(data)
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
        "MOLT_CHECK_TIMEOUT_S",
        "MOLT_MAP_TOKENS",
        "MOLT_CONTEXT_TOKENS",
        "MOLT_INSTRUCTION_FILES",
        "MOLT_SYNTAX_CHECK",
        "MOLT_MEMORY_MODEL",
        "MOLT_MEMORY_EFFORT",
        "MOLT_MEMORY_MAX_TOKENS",
        "MOLT_MEMORY_MODEL_TIMEOUT_S",
    ] {
        cmd.env_remove(var);
    }
    cmd
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

fn nats_server() -> Option<PathBuf> {
    std::env::var_os("MOLT_NATS_SERVER").map(PathBuf::from).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("nats-server")).find(|p| p.is_file())
    })
}

#[tokio::test]
async fn a_task_runs_over_nats_with_provisioned_secrets() {
    let Some(bin) = nats_server() else {
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
    use molt::session::Session;
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let session = Session::start(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
    let mut req = setup.request(Some(CHECK), 1);
    req.apply = false;
    let response = session.run(&req, &molt_proto::TraceId::random()).await.unwrap();
    let fork = response.fork.unwrap();
    session.shutdown(std::slice::from_ref(&fork)).await;
    // A saved path from a different workspace cannot redirect a human's operation.
    let other = tempfile::tempdir().unwrap();
    let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
    let wrong = Session::start(&setup.cfg, other.path().to_str().unwrap(), events).await.unwrap();
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
    let session = Session::start(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
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
    use molt::session::Session;
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
    let session = Session::start(&setup.cfg, w.to_str().unwrap(), events).await.unwrap();
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
    use molt::session::Session;

    let server = fake_api(Fake::DesignsThenGreets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let (events, mut progress) = tokio::sync::mpsc::unbounded_channel();
    let session = Session::start(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
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
    use molt::session::Session;
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let runs = setup.data.path().join(molt::config::RUNS);
    let start = || async {
        let (events, _progress) = tokio::sync::mpsc::unbounded_channel();
        Session::start(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap()
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
                event: Event::BeforeApply,
                command: command.into(),
                blocking: true,
                timeout_ms: if command == "sleep 10" { 20 } else { 5000 },
            },
            Hook {
                event: Event::AfterApply,
                command: "printf notification > notification.txt".into(),
                blocking: false,
                timeout_ms: 5000,
            },
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
            .any(|e| matches!(e, Progress::Note { message, .. } if message.contains("hook BeforeApply"))));
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
async fn the_ui_backend_finishes_without_terminal_reads_and_exits_when_the_pipe_closes() {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    async fn send(out: &mut (impl AsyncWrite + Unpin), value: Value) {
        let bytes = serde_json::to_vec(&value).unwrap();
        out.write_all(&(bytes.len() as u32).to_be_bytes()).await.unwrap();
        out.write_all(&bytes).await.unwrap();
        out.flush().await.unwrap();
    }
    async fn recv(input: &mut (impl AsyncRead + Unpin)) -> Value {
        let mut size = [0; 4];
        input.read_exact(&mut size).await.unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
        input.read_exact(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
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
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    send(
        &mut input,
        json!({"cfg":setup.cfg,"workspace":setup.workspace.path().canonicalize().unwrap().to_str().unwrap()}),
    )
    .await;
    send(&mut input, json!("StartSession")).await;
    let up = tokio::time::timeout(Duration::from_secs(20), recv(&mut output)).await.unwrap();
    assert!(up.get("SessionUp").is_some(), "{up}");
    let req = setup.request(Some(CHECK), 1);
    send(&mut input, json!({"Run":{"req":req,"trace":molt_proto::TraceId::random()}})).await;
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
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let msg = recv(&mut output).await;
            if msg.get("Ran").is_some() {
                break msg;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(result["Ran"]["result"]["Ok"]["applied"], true, "{result}");
    assert_eq!(setup.file("greeting.txt").as_deref(), Some("hello\n"));
    // A result waits for the interface's decision when the interface dies: its fork stays, to be picked up.
    std::fs::remove_file(setup.workspace.path().join("greeting.txt")).unwrap();
    let mut req = setup.request(Some(CHECK), 1);
    req.apply = false;
    send(&mut input, json!({"Run":{"req":req,"trace":molt_proto::TraceId::random()}})).await;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let msg = recv(&mut output).await;
            if msg.get("Ran").is_some() {
                break msg;
            }
        }
    })
    .await
    .unwrap();
    let fork = result["Ran"]["result"]["Ok"]["fork"].as_str().expect("kept in its fork").to_owned();
    drop(input);
    assert!(tokio::time::timeout(Duration::from_secs(15), child.wait()).await.unwrap().unwrap().success());
    assert!(Path::new(&fork).is_dir(), "the fork outlives the interface that died");
}

/// `!cmd` in the interface: the session runs it through the shell service in
/// a fork of the workspace, in the sandbox the attempts get, streams its
/// output, stops it when asked, calls no model, and the audit log has it.
#[tokio::test]
async fn a_session_command_streams_its_output_from_the_sandbox_and_is_audited() {
    use molt::session::Session;
    use tokio_util::sync::CancellationToken;
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let workspace = setup.workspace.path().canonicalize().unwrap();
    let (events, mut progress) = tokio::sync::mpsc::unbounded_channel();
    let session = Session::start(&setup.cfg, workspace.to_str().unwrap(), events).await.unwrap();
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
