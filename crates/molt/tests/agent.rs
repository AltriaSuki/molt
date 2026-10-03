//! `molt do` end to end: the kernel, the real gateway, tools and planner
//! processes, and a fake Messages API that plays the model.
//!
//! The fake is stateless. It reads its role and the turn from the request:
//! the check designer is offered `submit_check` and attempts are not; the
//! first turn has one message, a turn whose last user message holds
//! `tool_result` blocks follows tool calls, and a plain-text one carries a
//! failed check's feedback. It answers `400` to anything the real API would
//! refuse (see [`check_request`]), and every test checks that it never had to.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use molt::agent::{run_task, Interrupted};
use molt::{Config, Secrets, TransportKind};
use molt_api::fs::{ChangeKind, DROP, FORK, MERGE, WRITE};
use molt_api::model::COMPLETE;
use molt_api::planner::tools::{RUN, SUBMIT_CHECK, WRITE_FILE};
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

#[derive(Clone)]
enum Fake {
    /// Attempts write greeting.txt and say they are done.
    Greets,
    /// The designer writes check.sh and submits it; attempts then greet.
    DesignsThenGreets,
    /// Attempts write the wrong greeting, then fix it after the check fails.
    FixesAfterFeedback,
    /// Attempts write a greeting this many bytes long and say they are done.
    GreetsAtLength(usize),
    /// Every request is refused.
    Refuses,
    /// Attempts run a command that starts a long `sleep`, writes its pid to
    /// this file and waits for it.
    Hangs(PathBuf),
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
        let designer = body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == SUBMIT_CHECK));
        let last = messages.last().cloned().unwrap_or(Value::Null);
        let after_tools = last["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_result"));
        let turn = match (messages.len(), after_tools) {
            (1, _) => Turn::First,
            (_, true) => Turn::AfterTools,
            (_, false) => Turn::Feedback,
        };
        let n = messages.len();
        let model = body["model"].as_str().unwrap_or("unknown");
        let write = |path: &str, content: &str| tool_use(n, WRITE_FILE, json!({ "path": path, "content": content }));
        let (mut content, stop) = match (&self.0, designer, turn) {
            (Fake::Refuses, ..) => return refusal(n, model),
            (_, true, Turn::First) => (vec![write("check.sh", &format!("{CHECK}\n"))], "tool_use"),
            (_, true, _) => {
                let input = json!({ "command": "sh check.sh", "files": ["check.sh"], "rationale": "Greets." });
                (vec![tool_use(n, SUBMIT_CHECK, input)], "tool_use")
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
            (_, false, Turn::First | Turn::Feedback) => (vec![write("greeting.txt", "hello\n")], "tool_use"),
            (_, false, Turn::AfterTools) => {
                (vec![json!({ "type": "text", "text": "Wrote greeting.txt." })], "end_turn")
            }
        };
        // Each turn opens with a thinking block whose signature names the
        // turn's place in the conversation, so check_request can tell that
        // it came back unmodified.
        content.insert(0, json!({ "type": "thinking", "thinking": "", "signature": signature(n) }));
        message(n, model, content, stop)
    }
}

fn message(n: usize, model: &str, content: Vec<Value>, stop: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
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
    }))
}

fn refusal(n: usize, model: &str) -> ResponseTemplate {
    message(n, model, vec![], "refusal")
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
        Self { workspace, data, cfg }
    }

    fn request(&self, check: Option<&str>, attempts: u32) -> RunRequest {
        let workspace = self.workspace.path().canonicalize().unwrap();
        let mut req = RunRequest::new("Write a greeting into greeting.txt.", workspace.to_str().unwrap());
        req.check = check.map(str::to_owned);
        req.attempts = attempts;
        req
    }

    async fn run(&self, req: RunRequest) -> (RunResponse, Vec<Progress>) {
        let mut events = Vec::new();
        let run = run_task(&self.cfg, req, |e| events.push(e.clone()), std::future::pending());
        let resp = tokio::time::timeout(RUN_LIMIT, run).await.expect("the run hung").unwrap();
        (resp, events)
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
    let left: Vec<_> = std::fs::read_dir(&work).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "left in {}: {left:?}", work.display());
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
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_molt"));
    cmd.args(["do", "Write a greeting into greeting.txt."])
        .args(args)
        .arg("--data-dir")
        .arg(data)
        // No molt.toml here, and `--workspace` defaults to the current directory.
        .current_dir(workspace)
        .env("ANTHROPIC_BASE_URL", server.uri())
        .env("ANTHROPIC_API_KEY", API_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The developer's own settings must not change the test.
    for var in [
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
        "MOLT_CHECK_TIMEOUT_S",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

#[tokio::test]
async fn a_given_check_passes_and_the_result_is_applied() {
    let server = fake_api(Fake::Greets).await;
    let setup = Setup::new(&server);
    let (resp, events) = setup.run(setup.request(Some(CHECK), 2)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(resp.check, Some(CheckSpec { command: CHECK.into(), files: vec![], designed: false }));
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
    assert_eq!(started, ["fs", "model", "planner", "shell"]);
    assert!(!pids.values().any(|&pid| pid == std::process::id()));
    for method in [COMPLETE, FORK, WRITE, RUN_CHECK, MERGE, DROP] {
        assert!(requests.contains(&(Some("planner".into()), method.into())), "{method}: {requests:?}");
    }
    assert!(requests.iter().any(|(_, to)| to == PLANNER_RUN), "{requests:?}");
}

#[tokio::test]
async fn without_a_check_the_designer_writes_one_and_it_is_merged_too() {
    let server = fake_api(Fake::DesignsThenGreets).await;
    let setup = Setup::new(&server);
    let (resp, _) = setup.run(setup.request(None, 2)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{resp:#?}");
    assert_eq!(
        resp.check,
        Some(CheckSpec { command: "sh check.sh".into(), files: vec!["check.sh".into()], designed: true })
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
async fn stopping_a_run_kills_the_commands_it_started() {
    let pids = tempfile::tempdir().unwrap();
    let pidfile = pids.path().join("sleep.pid");
    let server = fake_api(Fake::Hangs(pidfile.clone())).await;
    let setup = Setup::new(&server);
    let started = wait_for_pid(&pidfile);
    let run = run_task(&setup.cfg, setup.request(Some(CHECK), 1), |_| {}, async {
        started.await;
    });
    let err = tokio::time::timeout(RUN_LIMIT, run).await.expect("the run hung").unwrap_err();
    assert!(err.is::<Interrupted>(), "{err:#}");
    let pid = read_pid(&pidfile).unwrap();
    assert!(exits(pid).await, "the command's child {pid} outlived the run");
    assert_requests_valid(&server).await;
}

#[tokio::test]
async fn the_cli_carries_out_a_task() {
    let server = fake_api(Fake::Greets).await;
    let workspace = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // The check also fails if the API key reached the commands the agent runs.
    let check = format!("test -z \"${{ANTHROPIC_API_KEY-}}\" && {CHECK}");
    let cmd = molt_do(&server, workspace.path(), data.path(), &["--check", &check, "--attempts", "1"]).output();
    let out = tokio::time::timeout(RUN_LIMIT, cmd).await.expect("molt do hung").unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
    assert!(stdout.contains("outcome: passed"), "{stdout}");
    assert!(stdout.contains("added    greeting.txt"), "{stdout}");
    assert!(stderr.contains("check passed"), "{stderr}");
    assert_eq!(std::fs::read_to_string(workspace.path().join("greeting.txt")).unwrap(), "hello\n");
    assert!(data.path().join("audit.jsonl").exists());
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
    let mut cmd = molt_do(&server, workspace.path(), data.path(), &["--check", CHECK, "--attempts", "1"]);
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
    let molt_do = |data_dir: &Path, key: Option<&str>| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_molt"));
        cmd.args(["do", "anything", "--data-dir"]).arg(data_dir).current_dir(workspace.path());
        match key {
            Some(key) => cmd.env("ANTHROPIC_API_KEY", key),
            None => cmd.env_remove("ANTHROPIC_API_KEY"),
        };
        let out = cmd.output().unwrap();
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = molt_do(data.path(), None);
    assert!(err.contains("ANTHROPIC_API_KEY is not set"), "{err}");
    let inside = workspace.path().canonicalize().unwrap().join("state");
    let err = molt_do(&inside, Some("test"));
    assert!(err.contains("inside the workspace"), "{err}");
    assert!(!inside.exists());
    assert!(std::fs::read_dir(data.path()).unwrap().next().is_none(), "nothing was started");
}
