//! `git` (#111) against a fake bus: an attempt finds the commit that broke
//! a function, reads its patch and fixes the file, every query naming its
//! own fork; the tool is offered only when it is on and the planner holds
//! `fs.git`, after the other built-ins and before `explore`, and an `ask`
//! run reads the workspace's history itself.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use molt_api::fs::{self, Change, ChangeKind, GitQuery, GitRequest};
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
    /// The `fs.git` requests, in order, with their deadlines.
    queries: Vec<(GitRequest, u64)>,
    /// The planner holds a capability for `fs.git`.
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

    fn queries(&self) -> Vec<(GitRequest, u64)> {
        self.state.lock().unwrap().queries.clone()
    }

    fn answer(&self, target: &str, payload: Value, budget: Budget) -> Result<Value, RemoteError> {
        let mut st = self.state.lock().unwrap();
        let st = &mut *st;
        Ok(match target {
            fs::GIT => {
                let req: GitRequest = serde_json::from_value(payload).unwrap();
                st.queries.push((req.clone(), budget.ms));
                let output = match &req.query {
                    GitQuery::Log { search: Some(s), .. } => {
                        format!("abc1234 2026-10-01 Ann\n    Tidy up calc\n\nM\tcalc.py\n# searched {s}")
                    }
                    GitQuery::Show { rev, .. } => format!("commit {rev}\n-    return a + b\n+    return a - b\n"),
                    _ => "The workspace is the top of a git repository.\n".to_owned(),
                };
                json!({ "output": output, "truncated": false, "baseline": "f".repeat(40) })
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
        match target {
            fs::GIT => self.state.lock().unwrap().held,
            shell::START => true,
            _ => false,
        }
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

fn config(git: bool) -> Config {
    Config {
        max_tokens: 1000,
        model_timeout: Duration::from_secs(5),
        check_timeout: Duration::from_secs(7),
        late_reply_wait: Duration::from_millis(200),
        git,
        ..Config::default()
    }
}

fn request() -> RunRequest {
    RunRequest {
        check: Some("python -m pytest".into()),
        attempts: 1,
        review: false,
        ..RunRequest::new("add() subtracts since a recent commit; find it and fix it.", WORKSPACE)
    }
}

async fn run(fake: &Arc<Fake>, cfg: Config, req: RunRequest) -> RunResponse {
    let bus: Arc<dyn Bus> = fake.clone();
    let run = molt_planner::run(bus, Arc::new(cfg), req, TraceId::from_raw("trace_test"));
    tokio::time::timeout(Duration::from_secs(10), run).await.expect("the run took too long").unwrap()
}

#[tokio::test]
async fn an_attempt_finds_the_commit_that_broke_a_function_in_the_history_and_fixes_it() {
    let fake = Fake::new(|req| match turn(req) {
        1 => calls(req, &[("git", json!({ "query": "log", "search": "a + b", "path": "calc.py" }))]),
        2 => {
            let found = &results(req)[0];
            assert!(found.contains("Tidy up calc") && found.contains("searched a + b"), "{found}");
            calls(req, &[("git", json!({ "query": "show", "rev": "abc1234" })), ("git", json!({ "query": "status" }))])
        }
        3 => {
            assert!(results(req)[0].contains("-    return a + b"), "{:?}", results(req));
            calls(req, &[("write_file", json!({ "path": "calc.py", "content": GOOD }))])
        }
        _ => done("abc1234 turned + into -; put it back."),
    });
    let resp = run(&fake, config(true), request()).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    let queries = fake.queries();
    assert_eq!(queries.len(), 3);
    for (req, deadline) in &queries {
        assert_eq!(req.workspace, "/scratch/fork-0", "each query names the attempt's fork");
        assert!(*deadline >= 30_000, "room for the service's own deadline: {deadline}");
    }
    assert_eq!(
        queries[0].0.query,
        GitQuery::Log { rev: None, path: Some("calc.py".into()), search: Some("a + b".into()), limit: None }
    );
    assert_eq!(queries[1].0.query, GitQuery::Show { rev: "abc1234".into(), path: None });
    assert_eq!(fake.state.lock().unwrap().workspace["calc.py"], GOOD);
}

#[tokio::test]
async fn git_is_offered_only_when_turned_on_and_held_and_then_before_explore() {
    let fake = Fake::new(|req| match turn(req) {
        1 => {
            assert!(!tool_names(req).contains(&"git".to_owned()), "{:?}", tool_names(req));
            calls(
                req,
                &[
                    ("git", json!({ "query": "status" })),
                    ("git", json!({ "query": "no such query" })),
                    ("write_file", json!({ "path": "calc.py", "content": GOOD })),
                ],
            )
        }
        _ => {
            let shown = results(req);
            assert_eq!(shown[0], "There is no tool named \"git\".");
            assert_eq!(shown[1], "There is no tool named \"git\".", "not even its input is looked at");
            done("Done.")
        }
    });
    assert_eq!(run(&fake, config(false), request()).await.outcome, Outcome::Passed);
    assert!(fake.queries().is_empty());

    let unheld = Fake::new(|req| {
        assert!(!tool_names(req).contains(&"git".to_owned()));
        match turn(req) {
            1 => calls(req, &[("write_file", json!({ "path": "calc.py", "content": GOOD }))]),
            _ => done("Done."),
        }
    });
    unheld.state.lock().unwrap().held = false;
    assert_eq!(run(&unheld, config(true), request()).await.outcome, Outcome::Passed);

    // On and held: after the process tools, and explore stays last.
    let offered = Fake::new(|req| {
        let names = tool_names(req);
        let at = |name: &str| names.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name}: {names:?}"));
        assert_eq!(at("git"), at("stop_process") + 1, "{names:?}");
        assert_eq!(at("explore"), names.len() - 1, "{names:?}");
        match turn(req) {
            1 => calls(req, &[("write_file", json!({ "path": "calc.py", "content": GOOD }))]),
            _ => done("Done."),
        }
    });
    let cfg = Config { processes: true, explore: true, ..config(true) };
    assert_eq!(run(&offered, cfg, request()).await.outcome, Outcome::Passed);
}

#[tokio::test]
async fn an_ask_run_reads_the_history_of_the_workspace_itself() {
    let fake = Fake::new(|req| match turn(req) {
        1 => {
            assert!(tool_names(req).contains(&"git".to_owned()), "{:?}", tool_names(req));
            calls(req, &[("git", json!({ "query": "blame", "path": "calc.py", "start_line": 2 }))])
        }
        _ => done("Line 2 came from abc1234."),
    });
    let req = RunRequest {
        workflow: Some("ask".into()),
        check: None,
        ..RunRequest::new("Why does add() subtract?", WORKSPACE)
    };
    let resp = run(&fake, config(true), req).await;
    // An answer has nothing to check.
    assert_eq!(resp.outcome, Outcome::Unverified, "{}", resp.summary);
    let queries = fake.queries();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].0.workspace, WORKSPACE, "an ask run has no fork");
    assert_eq!(
        queries[0].0.query,
        GitQuery::Blame { path: "calc.py".into(), rev: None, start_line: Some(2), end_line: None }
    );
}
