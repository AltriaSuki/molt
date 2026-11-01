//! `read_output` (#238) against a fake bus: the done-check's long output
//! is cut in its feedback, which names the saved output; the attempt
//! searches it for the failure in the middle, and fixes the file, without
//! the check running again until it does. Offered only when the planner
//! holds `shell.output`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use molt_api::fs::{self, Change, ChangeKind};
use molt_api::model::{CompleteRequest, CompleteResponse, Usage};
use molt_api::planner::{Outcome, RunRequest, RunResponse};
use molt_api::shell;
use molt_planner::{Bus, Config};
use molt_proto::{Budget, ErrorCode, RemoteError, TraceId};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const WORKSPACE: &str = "/work/project";
const BAD: &str = "def add(a, b):\n    return a - b\n";
const GOOD: &str = "def add(a, b):\n    return a + b\n";

type Files = BTreeMap<String, String>;
type Model = Box<dyn Fn(&CompleteRequest) -> CompleteResponse + Send + Sync>;

#[derive(Default)]
struct State {
    workspace: Files,
    forks: BTreeMap<String, (Files, Files)>,
    forked: u32,
    /// The `shell.output` requests, in order.
    reads: Vec<shell::OutputRequest>,
    /// How many commands ran.
    runs: u32,
    /// The planner holds a capability for `shell.output`.
    held: bool,
}

struct Fake {
    state: Mutex<State>,
    model: Model,
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

impl Fake {
    fn new(model: impl Fn(&CompleteRequest) -> CompleteResponse + Send + Sync + 'static) -> Arc<Self> {
        let workspace = Files::from([("calc.py".to_owned(), BAD.to_owned())]);
        let state = State { workspace, held: true, ..State::default() };
        Arc::new(Self { state: Mutex::new(state), model: Box::new(model) })
    }

    fn reads(&self) -> Vec<shell::OutputRequest> {
        self.state.lock().unwrap().reads.clone()
    }

    fn runs(&self) -> u32 {
        self.state.lock().unwrap().runs
    }

    fn answer(&self, target: &str, payload: Value, _: Budget) -> Result<Value, RemoteError> {
        let mut st = self.state.lock().unwrap();
        let st = &mut *st;
        Ok(match target {
            shell::OUTPUT => {
                let req: shell::OutputRequest = serde_json::from_value(payload).unwrap();
                st.reads.push(req.clone());
                if req.id != "out_check" {
                    return Err(invalid(format!("there is no saved output {}", req.id)));
                }
                json!(shell::OutputPage {
                    output: saved(),
                    offset: 0,
                    next: 900_000,
                    text: String::new(),
                    matches: vec![shell::OutputMatch {
                        line: 4012,
                        offset: 451_200,
                        text: "calc.py:2: AssertionError: add(2, 2) == 0".into(),
                    }],
                    more: false,
                })
            }
            fs::READ => {
                let path = payload["path"].as_str().unwrap();
                let content = st.workspace.get(path).cloned().ok_or_else(|| invalid("no such file"))?;
                json!({ "content": content, "first_line": 1, "lines": 2, "total_lines": 2, "truncated": false })
            }
            fs::WRITE => {
                let w: fs::WriteRequest = serde_json::from_value(payload).unwrap();
                let fork = st.forks.get_mut(&w.workspace).ok_or_else(|| invalid("no such fork"))?;
                let created = fork.1.insert(w.path, w.content.clone()).is_none();
                json!(fs::WriteResponse { bytes: w.content.len() as u64, created, syntax: None })
            }
            fs::FORK => {
                let fork = format!("/scratch/fork-{}", st.forked);
                st.forked += 1;
                st.forks.insert(fork.clone(), (st.workspace.clone(), st.workspace.clone()));
                json!(fs::ForkResponse { fork, files: 1 })
            }
            fs::DIFF => {
                let d: fs::DiffRequest = serde_json::from_value(payload).unwrap();
                let (base, now) = st.forks.get(&d.fork).ok_or_else(|| invalid("no such fork"))?;
                let changes: Vec<Change> = now
                    .keys()
                    .filter(|p| base.get(*p) != now.get(*p))
                    .map(|p| Change { path: p.clone(), kind: ChangeKind::Modified })
                    .collect();
                json!(fs::DiffResponse { changes, patch: String::new(), truncated: false, left_out: None })
            }
            fs::MERGE => {
                let m: fs::MergeRequest = serde_json::from_value(payload).unwrap();
                let (base, now) = st.forks.remove(&m.fork).ok_or_else(|| invalid("no such fork"))?;
                let changes: Vec<Change> = now
                    .keys()
                    .filter(|p| base.get(*p) != now.get(*p))
                    .map(|p| Change { path: p.clone(), kind: ChangeKind::Modified })
                    .collect();
                st.workspace = now;
                json!(fs::MergeResponse { changes, operation: None, left_out: None })
            }
            fs::DROP => {
                let d: fs::DropRequest = serde_json::from_value(payload).unwrap();
                json!(fs::DropResponse { dropped: st.forks.remove(&d.fork).is_some() })
            }
            shell::RUN => {
                let r: shell::RunRequest = serde_json::from_value(payload).unwrap();
                let files = st.forks.get(&r.workspace).map(|f| f.1.clone()).unwrap_or_default();
                let passed = files.get("calc.py").map(String::as_str) == Some(GOOD);
                st.runs += 1;
                json!(shell::RunResponse {
                    exit_code: Some(if passed { 0 } else { 1 }),
                    signal: None,
                    timed_out: false,
                    cancelled: false,
                    stdout: if passed {
                        "ok\n".into()
                    } else {
                        "test 1 ok\n[... 899000 bytes omitted ...]\n9999 passed, 1 failed\n".into()
                    },
                    stderr: String::new(),
                    truncated: !passed,
                    duration_ms: 1,
                    offline_failure: false,
                    outputs: if passed { Vec::new() } else { vec![saved()] },
                })
            }
            other => return Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {other}") }),
        })
    }
}

#[async_trait]
impl Bus for Fake {
    async fn call(&self, target: &str, payload: Value, budget: Budget, _: &TraceId) -> Result<Value, RemoteError> {
        if target == "model.complete" {
            let req: CompleteRequest = serde_json::from_value(payload).unwrap();
            return Ok(serde_json::to_value((self.model)(&req)).unwrap());
        }
        self.answer(target, payload, budget)
    }

    async fn call_cancellable(
        &self,
        target: &str,
        payload: Value,
        budget: Budget,
        trace: &TraceId,
        _: &CancellationToken,
    ) -> Result<Value, RemoteError> {
        self.call(target, payload, budget, trace).await
    }

    fn holds(&self, target: &str) -> bool {
        target == shell::OUTPUT && self.state.lock().unwrap().held
    }

    async fn publish(&self, _: &str, _: Value) {}
}

fn response(content: Vec<Value>, stop: &str) -> CompleteResponse {
    CompleteResponse {
        id: "msg_fake".into(),
        model: "fake".into(),
        content,
        stop_reason: Some(stop.into()),
        stop_details: None,
        usage: Usage { input_tokens: 100, output_tokens: 10, ..Default::default() },
        cost_usd: Some(0.125),
        price_source: None,
        context_tokens: None,
        endpoint: None,
    }
}

fn calls(req: &CompleteRequest, calls: &[(&str, Value)]) -> CompleteResponse {
    let n = req.messages.len();
    let content = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| json!({ "type": "tool_use", "id": format!("toolu_{n}_{i}"), "name": name, "input": input }))
        .collect();
    response(content, "tool_use")
}

fn done(text: &str) -> CompleteResponse {
    response(vec![json!({ "type": "text", "text": text })], "end_turn")
}

fn turn(req: &CompleteRequest) -> usize {
    req.messages.len().div_ceil(2)
}

fn tool_names(req: &CompleteRequest) -> Vec<String> {
    req.tools.iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect()
}

fn results(req: &CompleteRequest) -> Vec<String> {
    let blocks = req.messages.last().unwrap()["content"].as_array().cloned().unwrap_or_default();
    blocks.iter().filter_map(|b| b["content"].as_str().map(str::to_owned)).collect()
}

fn config() -> Config {
    Config {
        max_tokens: 1000,
        model_timeout: Duration::from_secs(5),
        check_timeout: Duration::from_secs(7),
        late_reply_wait: Duration::from_millis(200),
        ..Config::default()
    }
}

/// The check's output as the shell service saved it.
fn saved() -> shell::SavedOutput {
    shell::SavedOutput {
        id: "out_check".into(),
        stream: shell::OutputStream::Stdout,
        command: "python -m pytest".into(),
        total: 900_000,
        stored: 900_000,
        cut: None,
        open: false,
        end: Some(shell::OutputEnd { exit_code: Some(1), ..shell::OutputEnd::default() }),
        sha256: Some("ab".repeat(32)),
        trace: Some("trace_test".into()),
        request: Some("msg_1".into()),
        kept_s: 7200,
    }
}

fn request() -> RunRequest {
    RunRequest {
        check: Some("python -m pytest".into()),
        attempts: 1,
        review: false,
        ..RunRequest::new("Make the tests pass.", WORKSPACE)
    }
}

async fn run(fake: &Arc<Fake>, cfg: Config, req: RunRequest) -> RunResponse {
    let bus: Arc<dyn Bus> = fake.clone();
    let run = molt_planner::run(bus, Arc::new(cfg), req, TraceId::from_raw("trace_test"));
    tokio::time::timeout(Duration::from_secs(10), run).await.expect("the run took too long").unwrap()
}

#[tokio::test]
async fn the_checks_middle_failure_is_read_by_reference_and_the_check_does_not_run_again_for_it() {
    let fake = Fake::new(|req| {
        let names = tool_names(req);
        // Last but the attempt's own list, which comes after every other tool.
        assert_eq!(names[names.len() - 2..], ["read_output", "todo"], "{names:?}");
        match turn(req) {
            1 => done("Nothing to change."),
            2 => {
                let feedback = req.messages.last().unwrap()["content"].to_string();
                assert!(feedback.contains("The whole stdout is saved as out_check (all 900000 bytes)"), "{feedback}");
                calls(req, &[("read_output", json!({ "id": "out_check", "search": "Error" }))])
            }
            3 => {
                let found = &results(req)[0];
                assert!(
                    found.contains("line 4012 (offset 451200): calc.py:2: AssertionError")
                        && found.contains("exited with code 1"),
                    "{found}"
                );
                calls(req, &[("write_file", json!({ "path": "calc.py", "content": GOOD }))])
            }
            _ => done("add() subtracted; it adds now."),
        }
    });
    let resp = run(&fake, config(), request()).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    let reads = fake.reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].workspace, "/scratch/fork-0", "the read names the attempt's fork");
    assert_eq!((reads[0].search.as_deref(), reads[0].id.as_str()), (Some("Error"), "out_check"));
    // Once before the fix and once after it: reading the output ran nothing.
    assert_eq!(fake.runs(), 2);
}

#[tokio::test]
async fn without_shell_output_the_tool_is_not_offered_and_a_call_finds_no_such_tool() {
    let fake = Fake::new(|req| {
        assert!(!tool_names(req).iter().any(|n| n == "read_output"));
        match turn(req) {
            1 => calls(req, &[("read_output", json!({ "id": "out_check" }))]),
            2 => {
                assert_eq!(results(req)[0], "There is no tool named \"read_output\".");
                calls(req, &[("write_file", json!({ "path": "calc.py", "content": GOOD }))])
            }
            _ => done("Fixed."),
        }
    });
    fake.state.lock().unwrap().held = false;
    let resp = run(&fake, config(), request()).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    assert!(fake.reads().is_empty());
}
