//! Processes (#110) against a fake bus: an attempt starts a command that
//! runs on, reads it, writes to it and stops it, each in its own fork, and
//! every process of an attempt is stopped at its final reply and when it
//! ends, cancelled or not.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use molt_api::fs::{self, Change, ChangeKind};
use molt_api::model::{CompleteRequest, CompleteResponse, Usage};
use molt_api::planner::{AttemptStatus, Outcome, RunRequest, RunResponse};
use molt_api::shell::{self, ProcessState, ProcessStatus};
use molt_planner::{Bus, Config};
use molt_proto::{Budget, ErrorCode, RemoteError, TraceId};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const WORKSPACE: &str = "/work/project";

type Files = BTreeMap<String, String>;
type Model = Box<dyn Fn(&CompleteRequest) -> (CompleteResponse, u64) + Send + Sync>;

struct Fork {
    base: Files,
    now: Files,
}

#[derive(Default)]
struct State {
    workspace: Files,
    forks: BTreeMap<String, Fork>,
    forked: u32,
    /// Every call but the model's, in order: (target, payload, deadline).
    calls: Vec<(String, Value, u64)>,
    /// Processes running, by id, with the fork they run in.
    running: BTreeMap<String, String>,
    /// The planner holds a capability for `shell.start`.
    held: bool,
    /// Ids a call named from a fork they do not run in.
    refused: Vec<String>,
}

struct Fake {
    state: Mutex<State>,
    model: Model,
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

fn state(id: &str, command: &str, status: ProcessStatus, output: &str) -> Value {
    json!(ProcessState {
        id: id.into(),
        command: command.into(),
        status,
        exit_code: None,
        signal: None,
        ready: None,
        output: output.into(),
        offset: 0,
        next: output.len() as u64,
        omitted: 0,
        elapsed_ms: 1000,
    })
}

impl Fake {
    fn new(model: impl Fn(&CompleteRequest) -> (CompleteResponse, u64) + Send + Sync + 'static) -> Arc<Self> {
        let state = State { held: true, ..State::default() };
        Arc::new(Self { state: Mutex::new(state), model: Box::new(model) })
    }

    fn calls_to(&self, target: &str) -> Vec<Value> {
        self.state.lock().unwrap().calls.iter().filter(|c| c.0 == target).map(|c| c.1.clone()).collect()
    }

    fn targets(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.iter().map(|c| c.0.clone()).collect()
    }

    fn shell(&self, target: &str, payload: Value) -> Result<Value, RemoteError> {
        let mut st = self.state.lock().unwrap();
        let ws = payload["workspace"].as_str().unwrap_or_default().to_owned();
        if !st.forks.contains_key(&ws) {
            return Err(invalid(format!("{target} outside a fork: {ws}")));
        }
        let owned = |st: &State, id: &str| st.running.get(id) == Some(&ws);
        Ok(match target {
            shell::START => {
                let r: shell::StartRequest = serde_json::from_value(payload).unwrap();
                let id = format!("proc_{}", st.running.len() + 1);
                st.running.insert(id.clone(), ws);
                state(&id, &r.command, ProcessStatus::Running, "listening on :8080\n")
            }
            shell::READ => {
                let id = payload["id"].as_str().unwrap();
                if !owned(&st, id) {
                    return Err(invalid(format!("there is no process {id} in this workspace")));
                }
                state(id, "serve", ProcessStatus::Running, "GET / 200\n")
            }
            shell::WRITE => {
                let id = payload["id"].as_str().unwrap();
                if !owned(&st, id) {
                    return Err(invalid(format!("there is no process {id} in this workspace")));
                }
                state(id, "serve", ProcessStatus::Running, "")
            }
            shell::STOP => {
                let ids: Vec<String> = match payload["id"].as_str() {
                    Some(id) if owned(&st, id) => vec![id.to_owned()],
                    Some(id) => {
                        st.refused.push(id.to_owned());
                        return Err(invalid(format!("there is no process {id} in this workspace")));
                    }
                    None => st.running.iter().filter(|(_, at)| **at == ws).map(|(id, _)| id.clone()).collect(),
                };
                let stopped: Vec<Value> = ids
                    .iter()
                    .map(|id| {
                        st.running.remove(id);
                        state(id, "serve", ProcessStatus::Stopped, "")
                    })
                    .collect();
                json!({ "stopped": stopped })
            }
            _ => unreachable!(),
        })
    }

    fn fs(&self, target: &str, payload: Value) -> Result<Value, RemoteError> {
        let mut st = self.state.lock().unwrap();
        let st = &mut *st;
        let files = |st: &mut State, at: &str| -> Result<Files, RemoteError> {
            if at == WORKSPACE {
                return Ok(st.workspace.clone());
            }
            st.forks.get(at).map(|f| f.now.clone()).ok_or_else(|| invalid(format!("no workspace {at}")))
        };
        Ok(match target {
            fs::WRITE => {
                let w: fs::WriteRequest = serde_json::from_value(payload).unwrap();
                let bytes = w.content.len() as u64;
                let fork = st.forks.get_mut(&w.workspace).ok_or_else(|| invalid("no such fork"))?;
                let created = fork.now.insert(w.path, w.content).is_none();
                json!(fs::WriteResponse { bytes, created, syntax: None })
            }
            fs::FORK => {
                let f: fs::ForkRequest = serde_json::from_value(payload).unwrap();
                let files = files(st, &f.workspace)?;
                let fork = format!("/scratch/fork-{}", st.forked);
                st.forked += 1;
                let count = files.len() as u64;
                st.forks.insert(fork.clone(), Fork { base: files.clone(), now: files });
                json!(fs::ForkResponse { fork, files: count })
            }
            fs::DIFF => {
                let d: fs::DiffRequest = serde_json::from_value(payload).unwrap();
                let fork = st.forks.get(&d.fork).ok_or_else(|| invalid("no such fork"))?;
                let changes: Vec<Change> = fork
                    .now
                    .keys()
                    .filter(|p| fork.base.get(*p) != fork.now.get(*p))
                    .map(|p| Change { path: p.clone(), kind: ChangeKind::Added })
                    .collect();
                let patch = changes.iter().map(|c| format!("--- a/{0}\n+++ b/{0}\n+hi\n", c.path)).collect();
                json!(fs::DiffResponse { changes, patch, truncated: false, left_out: None })
            }
            fs::MERGE => {
                let m: fs::MergeRequest = serde_json::from_value(payload).unwrap();
                let fork = st.forks.remove(&m.fork).ok_or_else(|| invalid("no such fork"))?;
                let changes: Vec<Change> = fork
                    .now
                    .iter()
                    .filter(|(p, c)| fork.base.get(*p) != Some(c))
                    .map(|(p, _)| Change { path: p.clone(), kind: ChangeKind::Added })
                    .collect();
                st.workspace = fork.now;
                json!(fs::MergeResponse { changes, operation: None, left_out: None })
            }
            fs::DROP => {
                let d: fs::DropRequest = serde_json::from_value(payload).unwrap();
                json!(fs::DropResponse { dropped: st.forks.remove(&d.fork).is_some() })
            }
            shell::RUN => {
                let r: shell::RunRequest = serde_json::from_value(payload).unwrap();
                let passed = files(st, &r.workspace)?.get("hello.txt").map(String::as_str) == Some("hi\n");
                json!(shell::RunResponse {
                    exit_code: Some(if passed { 0 } else { 1 }),
                    signal: None,
                    timed_out: false,
                    cancelled: false,
                    stdout: String::new(),
                    stderr: String::new(),
                    truncated: false,
                    duration_ms: 1,
                    offline_failure: false,
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
            let (resp, delay) = (self.model)(&req);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            return Ok(serde_json::to_value(resp).unwrap());
        }
        self.state.lock().unwrap().calls.push((target.to_owned(), payload.clone(), budget.ms));
        if [shell::START, shell::READ, shell::WRITE, shell::STOP].contains(&target) {
            return self.shell(target, payload);
        }
        self.fs(target, payload)
    }

    async fn call_cancellable(
        &self,
        target: &str,
        payload: Value,
        budget: Budget,
        trace: &TraceId,
        cancel: &CancellationToken,
    ) -> Result<Value, RemoteError> {
        tokio::select! {
            biased;
            reply = self.call(target, payload, budget, trace) => reply,
            _ = cancel.cancelled(), if target == "model.complete" => Ok(json!({
                "error": { "code": "cancelled", "message": "local request stopped" },
                "settlement": { "local_stopped": true, "may_have_started": true, "usage": null, "cost_usd": null },
            })),
        }
    }

    fn holds(&self, target: &str) -> bool {
        target == shell::START && self.state.lock().unwrap().held
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
    }
}

fn calls(req: &CompleteRequest, calls: &[(&str, Value)]) -> (CompleteResponse, u64) {
    let n = req.messages.len();
    let content = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| json!({ "type": "tool_use", "id": format!("toolu_{n}_{i}"), "name": name, "input": input }))
        .collect();
    (response(content, "tool_use"), 0)
}

fn done(text: &str) -> (CompleteResponse, u64) {
    (response(vec![json!({ "type": "text", "text": text })], "end_turn"), 0)
}

fn turn(req: &CompleteRequest) -> usize {
    req.messages.len().div_ceil(2)
}

fn tool_names(req: &CompleteRequest) -> Vec<String> {
    req.tools.iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect()
}

/// Every tool result the last message holds, as text.
fn results(req: &CompleteRequest) -> Vec<String> {
    let blocks = req.messages.last().unwrap()["content"].as_array().cloned().unwrap_or_default();
    blocks.iter().filter_map(|b| b["content"].as_str().map(str::to_owned)).collect()
}

fn config(processes: bool) -> Config {
    Config {
        max_tokens: 1000,
        model_timeout: Duration::from_secs(5),
        check_timeout: Duration::from_secs(7),
        late_reply_wait: Duration::from_millis(200),
        processes,
        ..Config::default()
    }
}

fn request(attempts: u32) -> RunRequest {
    RunRequest {
        check: Some("check".into()),
        attempts,
        review: false,
        ..RunRequest::new("Add hello.txt saying hi, and try the dev server.", WORKSPACE)
    }
}

async fn run(fake: &Arc<Fake>, cfg: Config, req: RunRequest) -> RunResponse {
    let bus: Arc<dyn Bus> = fake.clone();
    let run = molt_planner::run(bus, Arc::new(cfg), req, TraceId::from_raw("trace_test"));
    tokio::time::timeout(Duration::from_secs(10), run).await.expect("the run took too long").unwrap()
}

const PROCESS_TOOLS: [&str; 4] = ["start_process", "read_process", "write_process", "stop_process"];

#[tokio::test]
async fn an_attempt_serves_reads_and_stops_a_process_in_its_fork_and_nothing_runs_when_it_is_checked() {
    let fake = Fake::new(|req| match turn(req) {
        1 => calls(req, &[("start_process", json!({ "command": "serve", "ready": "listening", "timeout_s": 5 }))]),
        2 => {
            assert!(results(req)[0].starts_with("Started process proc_1.\nProcess proc_1 (`serve`) is running"));
            calls(
                req,
                &[
                    ("write_file", json!({ "path": "hello.txt", "content": "hi\n" })),
                    ("read_process", json!({ "id": "proc_1", "wait_s": 2 })),
                    ("write_process", json!({ "id": "proc_1", "input": "reload\n" })),
                    ("start_process", json!({ "command": "watch" })),
                ],
            )
        }
        3 => {
            let shown = results(req);
            assert!(shown[1].contains("Output (bytes 0 to 10):\nGET / 200\n"), "{shown:?}");
            assert!(shown[2].starts_with("Wrote the input.\n"), "{shown:?}");
            calls(req, &[("stop_process", json!({ "id": "proc_1" }))])
        }
        _ => {
            assert!(results(req)[0].contains("was stopped"), "{:?}", results(req));
            done("Added hello.txt.")
        }
    });
    let resp = run(&fake, config(true), request(1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);

    // Every call names the attempt's fork, so the shell service keeps the processes to it.
    let fork = fake.calls_to(fs::FORK).len();
    assert_eq!(fork, 1);
    for target in [shell::START, shell::READ, shell::WRITE, shell::STOP] {
        for call in fake.calls_to(target) {
            assert_eq!(call["workspace"], "/scratch/fork-0", "{target}");
        }
    }
    let start = &fake.calls_to(shell::START)[0];
    assert_eq!(
        (start["command"].as_str(), start["ready"].as_str(), start["wait_ms"].as_u64()),
        (Some("serve"), Some("listening"), Some(5000))
    );
    // Without a ready text the service's own short wait applies.
    assert!(fake.calls_to(shell::START)[1].get("wait_ms").is_none());
    assert_eq!(fake.calls_to(shell::READ)[0]["wait_ms"], 2000);
    // The watcher it left running is stopped at its final reply, before the check runs, and again at its end.
    let targets = fake.targets();
    let stops: Vec<usize> = targets.iter().enumerate().filter(|(_, t)| *t == shell::STOP).map(|(i, _)| i).collect();
    let check = targets.iter().position(|t| t == shell::RUN).unwrap();
    let all = fake.calls_to(shell::STOP);
    assert_eq!(all.len(), 3, "{all:?}");
    assert_eq!(all[0]["id"], "proc_1");
    assert!(all[1].get("id").is_none() && all[2].get("id").is_none());
    assert!(stops[1] < check && check < stops[2], "{targets:?}");
    assert!(fake.state.lock().unwrap().running.is_empty());
}

#[tokio::test]
async fn the_process_tools_are_offered_only_when_turned_on_and_held() {
    let fake = Fake::new(|req| match turn(req) {
        1 => {
            let names = tool_names(req);
            assert!(PROCESS_TOOLS.iter().all(|t| !names.contains(&(*t).to_owned())), "{names:?}");
            calls(
                req,
                &[
                    ("start_process", json!({ "command": "serve" })),
                    ("write_file", json!({ "path": "hello.txt", "content": "hi\n" })),
                ],
            )
        }
        _ => {
            assert_eq!(results(req)[0], "There is no tool named \"start_process\".");
            done("Done.")
        }
    });
    let resp = run(&fake, config(false), request(1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    assert!(fake.calls_to(shell::START).is_empty() && fake.calls_to(shell::STOP).is_empty());

    // On, but without a capability for shell.start: still not offered.
    let unheld = Fake::new(|req| {
        assert!(tool_names(req).iter().all(|n| !PROCESS_TOOLS.contains(&n.as_str())));
        match turn(req) {
            1 => calls(req, &[("write_file", json!({ "path": "hello.txt", "content": "hi\n" }))]),
            _ => done("Done."),
        }
    });
    unheld.state.lock().unwrap().held = false;
    assert_eq!(run(&unheld, config(true), request(1)).await.outcome, Outcome::Passed);
    assert!(unheld.calls_to(shell::STOP).is_empty());

    // On and held: after the other built-ins, in a fixed order.
    let offered = Fake::new(|req| {
        let names = tool_names(req);
        assert_eq!(names[names.len() - 4..], PROCESS_TOOLS, "{names:?}");
        match turn(req) {
            1 => calls(req, &[("write_file", json!({ "path": "hello.txt", "content": "hi\n" }))]),
            _ => done("Done."),
        }
    });
    assert_eq!(run(&offered, config(true), request(1)).await.outcome, Outcome::Passed);
}

#[tokio::test]
async fn a_cancelled_attempt_stops_its_processes_and_another_attempts_id_reaches_none_of_them() {
    let fake = Fake::new(|req| match (req.attempt, turn(req)) {
        (Some(0), 1) => calls(req, &[("start_process", json!({ "command": "serve" }))]),
        (Some(0), 2) => (calls(req, &[("write_file", json!({ "path": "hello.txt", "content": "hi\n" }))]).0, 300),
        (Some(0), _) => done("Mine passed."),
        (_, 1) => calls(req, &[("start_process", json!({ "command": "serve" }))]),
        // The other attempt's process, by its id: the attempt's own fork is named, so it is not found.
        (_, 2) => {
            let other = if results(req)[0].starts_with("Started process proc_1.") { "proc_2" } else { "proc_1" };
            calls(req, &[("stop_process", json!({ "id": other }))])
        }
        // Stuck: only cancelling it ends this call.
        (_, _) => (done("Too late.").0, 60_000),
    });
    let resp = run(&fake, config(true), request(2)).await;
    assert_eq!((resp.outcome, resp.winner), (Outcome::Passed, Some(0)), "{}", resp.summary);
    assert_eq!(resp.attempts[1].status, AttemptStatus::Cancelled);
    let stops = fake.calls_to(shell::STOP);
    assert_eq!(fake.calls_to(shell::START).len(), 2);
    assert_eq!(fake.state.lock().unwrap().refused.len(), 1, "the other attempt's process was out of reach");
    // Both forks had every process stopped; none runs on.
    for fork in ["/scratch/fork-0", "/scratch/fork-1"] {
        assert!(stops.iter().any(|s| s["workspace"] == fork && s.get("id").is_none()), "{fork}: {stops:?}");
    }
    assert!(fake.state.lock().unwrap().running.is_empty(), "{:?}", fake.state.lock().unwrap().running);
}
