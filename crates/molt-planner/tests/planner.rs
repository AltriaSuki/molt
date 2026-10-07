//! Whole runs against a fake bus: a scripted model, an in-memory `fs` with
//! forks, and a `shell` whose done-check is a closure over a fork's files.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use molt_api::fs::{self, Change, ChangeKind};
use molt_api::memory;
use molt_api::model::{CompleteRequest, CompleteResponse, Usage};
use molt_api::planner::{AttemptStatus, CheckSpec, Outcome, RunRequest, RunResponse};
use molt_api::progress::Progress;
use molt_api::shell;
use molt_planner::{Bus, Config};
use molt_proto::{Budget, ErrorCode, RemoteError, TraceId};
use serde_json::{json, Value};

const WORKSPACE: &str = "/work/project";

type Files = BTreeMap<String, String>;

struct Reply {
    resp: Result<CompleteResponse, RemoteError>,
    delay: Duration,
}

impl From<CompleteResponse> for Reply {
    fn from(resp: CompleteResponse) -> Self {
        Self { resp: Ok(resp), delay: Duration::ZERO }
    }
}

type Model = Box<dyn Fn(&CompleteRequest) -> Reply + Send + Sync>;
type Shell = Box<dyn Fn(&str, &Files) -> shell::RunResponse + Send + Sync>;

#[derive(Clone, Debug)]
struct Call {
    target: String,
    payload: Value,
    budget: Budget,
}

struct State {
    workspace: Files,
    /// Fork path to (files at fork time, files now).
    forks: BTreeMap<String, (Files, Files)>,
    forked: u32,
    /// Lines `fs.read` returns when the request sets no limit.
    page: u64,
    merge_conflict: bool,
    /// `fs.merge` writes the first change, then fails with a `partial:` error.
    merge_partial: bool,
    drop_fails: bool,
    patch_truncated: bool,
    /// A memory service answers `memory.*`; without it those calls are `unavailable`.
    memory: bool,
    /// Memory is there but answers nothing: every call times out.
    memory_hangs: bool,
    calls: Vec<Call>,
    requests: Vec<CompleteRequest>,
    events: Vec<Progress>,
}

struct Fake {
    state: Mutex<State>,
    model: Model,
    shell: Shell,
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

fn parse<T: serde::de::DeserializeOwned>(payload: Value) -> T {
    serde_json::from_value(payload).unwrap()
}

fn files<'a>(st: &'a mut State, workspace: &str) -> Result<&'a mut Files, RemoteError> {
    if workspace == WORKSPACE {
        return Ok(&mut st.workspace);
    }
    st.forks.get_mut(workspace).map(|(_, now)| now).ok_or_else(|| invalid(format!("no workspace {workspace}")))
}

fn changes(base: &Files, now: &Files) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, content) in now {
        match base.get(path) {
            None => out.push(Change { path: path.clone(), kind: ChangeKind::Added }),
            Some(old) if old != content => out.push(Change { path: path.clone(), kind: ChangeKind::Modified }),
            _ => {}
        }
    }
    for path in base.keys().filter(|p| !now.contains_key(*p)) {
        out.push(Change { path: path.clone(), kind: ChangeKind::Deleted });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

impl Fake {
    fn new(
        model: impl Fn(&CompleteRequest) -> Reply + Send + Sync + 'static,
        shell: impl Fn(&str, &Files) -> shell::RunResponse + Send + Sync + 'static,
    ) -> Arc<Self> {
        let state = State {
            workspace: Files::from([("README.md".to_owned(), "# project\n".to_owned())]),
            forks: BTreeMap::new(),
            forked: 0,
            page: 2000,
            merge_conflict: false,
            merge_partial: false,
            drop_fails: false,
            patch_truncated: false,
            memory: false,
            memory_hangs: false,
            calls: Vec::new(),
            requests: Vec::new(),
            events: Vec::new(),
        };
        Arc::new(Self { state: Mutex::new(state), model: Box::new(model), shell: Box::new(shell) })
    }

    fn set(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
    }

    fn workspace(&self) -> Files {
        self.state.lock().unwrap().workspace.clone()
    }

    fn forks_alive(&self) -> Vec<String> {
        self.state.lock().unwrap().forks.keys().cloned().collect()
    }

    fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    fn calls_to(&self, target: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.target == target).collect()
    }

    fn requests(&self) -> Vec<CompleteRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    fn events(&self) -> Vec<Progress> {
        self.state.lock().unwrap().events.clone()
    }

    fn fs(&self, target: &str, payload: Value) -> Result<Value, RemoteError> {
        let mut st = self.state.lock().unwrap();
        let st = &mut *st;
        let reply = match target {
            fs::READ => {
                let r: fs::ReadRequest = parse(payload);
                let page = st.page;
                let content = files(st, &r.workspace)?
                    .get(&r.path)
                    .cloned()
                    .ok_or_else(|| invalid(format!("no such file: {}", r.path)))?;
                let lines: Vec<&str> = content.split_inclusive('\n').collect();
                let first = r.offset.unwrap_or(1).max(1);
                let start = (first - 1) as usize;
                let window: Vec<&str> =
                    lines.iter().skip(start).take(r.limit.unwrap_or(page) as usize).copied().collect();
                json!(fs::ReadResponse {
                    content: window.concat(),
                    first_line: first,
                    lines: window.len() as u64,
                    total_lines: lines.len() as u64,
                    truncated: start + window.len() < lines.len(),
                })
            }
            fs::WRITE => {
                let w: fs::WriteRequest = parse(payload);
                let bytes = w.content.len() as u64;
                let created = files(st, &w.workspace)?.insert(w.path, w.content).is_none();
                json!(fs::WriteResponse { bytes, created })
            }
            fs::EDIT => {
                let e: fs::EditRequest = parse(payload);
                let file = files(st, &e.workspace)?
                    .get_mut(&e.path)
                    .ok_or_else(|| invalid(format!("no such file: {}", e.path)))?;
                let found = file.matches(&e.old).count() as u64;
                if found == 0 || (found > 1 && !e.replace_all) {
                    return Err(invalid(format!("old text occurs {found} times in {}", e.path)));
                }
                *file = file.replace(&e.old, &e.new);
                json!(fs::EditResponse { replacements: found })
            }
            fs::LIST => {
                let l: fs::ListRequest = parse(payload);
                let entries = files(st, &l.workspace)?
                    .iter()
                    .map(|(path, content)| fs::Entry {
                        path: path.clone(),
                        kind: fs::EntryKind::File,
                        size: content.len() as u64,
                    })
                    .collect();
                json!(fs::ListResponse { entries, truncated: false })
            }
            fs::SEARCH => {
                let s: fs::SearchRequest = parse(payload);
                let mut matches = Vec::new();
                for (path, content) in files(st, &s.workspace)?.iter() {
                    for (i, line) in content.lines().enumerate() {
                        if line.contains(&s.pattern) {
                            matches.push(fs::Match { path: path.clone(), line: i as u64 + 1, text: line.to_owned() });
                        }
                    }
                }
                json!(fs::SearchResponse { matches, truncated: false })
            }
            fs::FORK => {
                let f: fs::ForkRequest = parse(payload);
                if f.workspace != WORKSPACE {
                    return Err(invalid(format!("{} is not a workspace", f.workspace)));
                }
                let fork = format!("/scratch/fork-{}", st.forked);
                st.forked += 1;
                st.forks.insert(fork.clone(), (st.workspace.clone(), st.workspace.clone()));
                json!(fs::ForkResponse { fork, files: st.workspace.len() as u64 })
            }
            fs::DIFF => {
                let d: fs::DiffRequest = parse(payload);
                let (base, now) = st.forks.get(&d.fork).ok_or_else(|| invalid("no such fork"))?;
                let changes = changes(base, now);
                let patch = changes
                    .iter()
                    .map(|c| {
                        let body: String = now
                            .get(&c.path)
                            .map(|t| t.lines().map(|l| format!("+{l}\n")).collect())
                            .unwrap_or_default();
                        format!("--- a/{0}\n+++ b/{0}\n{body}", c.path)
                    })
                    .collect();
                json!(fs::DiffResponse { changes, patch, truncated: st.patch_truncated })
            }
            fs::MERGE => {
                let m: fs::MergeRequest = parse(payload);
                let (base, now) = st.forks.get(&m.fork).cloned().ok_or_else(|| invalid("no such fork"))?;
                let changes = changes(&base, &now);
                if st.merge_conflict {
                    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
                    return Err(RemoteError {
                        code: ErrorCode::Failed,
                        message: format!("conflict: {}", paths.join(", ")),
                    });
                }
                for (i, c) in changes.iter().enumerate() {
                    if st.merge_partial && i > 0 {
                        let written: Vec<&str> = changes[..i].iter().map(|c| c.path.as_str()).collect();
                        return Err(RemoteError {
                            code: ErrorCode::Failed,
                            message: format!(
                                "partial: could not write {}: disk full; written: {}",
                                c.path,
                                written.join(", ")
                            ),
                        });
                    }
                    match now.get(&c.path) {
                        Some(content) => st.workspace.insert(c.path.clone(), content.clone()),
                        None => st.workspace.remove(&c.path),
                    };
                }
                if m.drop && st.drop_fails {
                    let message = "the merge succeeded, but dropping the fork failed".to_owned();
                    return Err(RemoteError { code: ErrorCode::Failed, message });
                }
                if m.drop {
                    st.forks.remove(&m.fork);
                }
                json!(fs::MergeResponse { changes })
            }
            fs::DROP => {
                let d: fs::DropRequest = parse(payload);
                if st.drop_fails {
                    return Err(RemoteError { code: ErrorCode::Failed, message: "could not delete the fork".into() });
                }
                json!(fs::DropResponse { dropped: st.forks.remove(&d.fork).is_some() })
            }
            other => return Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {other}") }),
        };
        Ok(reply)
    }
}

impl Fake {
    /// A memory that knows one note and one function of the workspace.
    fn memory(&self, target: &str, payload: Value) -> Result<Value, RemoteError> {
        if self.state.lock().unwrap().memory_hangs {
            return Err(RemoteError { code: ErrorCode::Timeout, message: format!("{target} did not answer") });
        }
        if !self.state.lock().unwrap().memory {
            return Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {target}") });
        }
        assert_eq!(payload["workspace"], WORKSPACE, "memory knows the user's workspace, not the forks");
        Ok(match target {
            memory::INDEX => json!(memory::IndexResponse { files: 3, parsed: 3, symbols: 7, ..Default::default() }),
            memory::MAP => json!(memory::MapResponse {
                map: "src/greet.rs:\n  3: pub fn greet(name: &str) -> String\n".into(),
                files: 1,
                symbols: 1,
                tokens: 20,
            }),
            memory::RECALL => {
                let note = memory::Note {
                    id: "note_1".into(),
                    kind: memory::NoteKind::Lesson,
                    text: "hello.txt must end with a newline or the check fails.".into(),
                    workspace: Some(WORKSPACE.into()),
                    confidence: 0.7,
                    created_ms: 1,
                    updated_ms: 1,
                    reinforced: 0,
                    conflicts: vec![],
                    provenance: memory::Provenance {
                        trace: "trace_before".into(),
                        events: vec!["msg_1".into()],
                        service: "memory".into(),
                        version: "v1".into(),
                    },
                    rev: 1,
                };
                json!(memory::RecallResponse { notes: vec![memory::Recalled { note, score: 1.0 }] })
            }
            memory::SYMBOLS => json!(memory::SymbolsResponse {
                definitions: vec![memory::Definition {
                    path: "src/greet.rs".into(),
                    line: 3,
                    kind: "function".into(),
                    name: "greet".into(),
                    signature: "pub fn greet(name: &str) -> String".into(),
                }],
                references: vec![],
                truncated: false,
            }),
            other => return Err(invalid(format!("no {other}"))),
        })
    }
}

#[async_trait]
impl Bus for Fake {
    async fn call(&self, target: &str, payload: Value, budget: Budget, _: &TraceId) -> Result<Value, RemoteError> {
        let call = Call { target: target.to_owned(), payload: payload.clone(), budget };
        self.state.lock().unwrap().calls.push(call);
        match target {
            "model.complete" => {
                let req: CompleteRequest = parse(payload);
                self.state.lock().unwrap().requests.push(req.clone());
                let reply = (self.model)(&req);
                tokio::time::sleep(reply.delay).await;
                reply.resp.map(|r| serde_json::to_value(r).unwrap())
            }
            "shell.run" => {
                let r: shell::RunRequest = parse(payload);
                let snapshot = files(&mut self.state.lock().unwrap(), &r.workspace)?.clone();
                Ok(serde_json::to_value((self.shell)(&r.command, &snapshot)).unwrap())
            }
            t if t.starts_with("memory.") => self.memory(t, payload),
            _ => self.fs(target, payload),
        }
    }

    async fn publish(&self, topic: &str, payload: Value) {
        assert_eq!(topic, "progress");
        self.state.lock().unwrap().events.push(parse(payload));
    }
}

// --- Scripting helpers -----------------------------------------------------

fn response(content: Vec<Value>, stop: &str) -> CompleteResponse {
    CompleteResponse {
        id: "msg_fake".into(),
        model: "fake".into(),
        content,
        stop_reason: Some(stop.into()),
        stop_details: None,
        usage: Usage { input_tokens: 100, output_tokens: 10, ..Default::default() },
        // A binary fraction, so sums are exact.
        cost_usd: Some(0.125),
    }
}

/// Call the tools in order; ids are unique within a conversation.
fn use_tools(req: &CompleteRequest, calls: &[(&str, Value)]) -> Reply {
    let n = req.messages.len();
    let content = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| json!({ "type": "tool_use", "id": format!("toolu_{n}_{i}"), "name": name, "input": input }))
        .collect();
    response(content, "tool_use").into()
}

fn done(text: &str) -> Reply {
    response(vec![json!({ "type": "text", "text": text })], "end_turn").into()
}

fn exit(code: i32, stderr: &str) -> shell::RunResponse {
    shell::RunResponse {
        exit_code: Some(code),
        signal: None,
        timed_out: false,
        cancelled: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
        truncated: false,
        duration_ms: 1,
    }
}

/// The model turn this request asks for, from 1.
fn turn(req: &CompleteRequest) -> usize {
    req.messages.len().div_ceil(2)
}

fn is_designer(req: &CompleteRequest) -> bool {
    req.tools.iter().any(|t| t["name"] == "submit_check")
}

/// Which attempt sent `req`, from the approach hint in its first message.
fn attempt_of(req: &CompleteRequest) -> u32 {
    match req.messages[0]["content"][1]["text"].as_str() {
        None => 0,
        Some(hint) if hint.contains("first run the done-check") => 1,
        Some(_) => 2,
    }
}

/// Every text in the last message, tool results included.
fn last_text(req: &CompleteRequest) -> String {
    let last = req.messages.last().unwrap();
    let blocks = last["content"].as_array().unwrap();
    blocks.iter().filter_map(|b| b["text"].as_str().or(b["content"].as_str())).collect::<Vec<_>>().join("\n")
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

fn request(check: Option<&str>, attempts: u32) -> RunRequest {
    RunRequest { check: check.map(Into::into), attempts, ..RunRequest::new("Add hello.txt saying hi.", WORKSPACE) }
}

async fn run(fake: &Arc<Fake>, req: RunRequest) -> RunResponse {
    run_with(fake, config(), req).await
}

async fn run_with(fake: &Arc<Fake>, cfg: Config, req: RunRequest) -> RunResponse {
    let bus: Arc<dyn Bus> = fake.clone();
    let run = molt_planner::run(bus, Arc::new(cfg), req, TraceId::from_raw("trace_test"));
    tokio::time::timeout(Duration::from_secs(5), run).await.expect("the run took too long").unwrap()
}

/// The check `check`: passes when hello.txt says hi.
fn hello_check(command: &str, files: &Files) -> shell::RunResponse {
    match (command, files.get("hello.txt").map(String::as_str)) {
        ("check", Some("hi\n")) => exit(0, ""),
        ("check", other) => exit(1, &format!("expected hi, got {other:?}\n")),
        _ => exit(0, ""),
    }
}

fn write_hello(req: &CompleteRequest, content: &str) -> Reply {
    use_tools(req, &[("write_file", json!({ "path": "hello.txt", "content": content }))])
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// --- Tests -------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_check_passes_and_the_winner_is_merged() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 1)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    assert_eq!(resp.check, Some(CheckSpec { command: "check".into(), files: vec![], designed: false }));
    assert_eq!((resp.winner, resp.applied, resp.fork.as_deref()), (Some(0), true, None));
    assert_eq!(resp.summary, "Wrote hello.txt.");
    assert_eq!(resp.changes, vec![Change { path: "hello.txt".into(), kind: ChangeKind::Added }]);
    assert!(resp.patch.contains("+hi") && !resp.patch_truncated, "{}", resp.patch);
    assert_eq!(fake.workspace()["hello.txt"], "hi\n");
    assert!(fake.forks_alive().is_empty());

    let a = &resp.attempts[0];
    assert_eq!((a.status, a.turns, a.check_runs), (AttemptStatus::Passed, 2, 1));
    assert_eq!(a.usage.input_tokens, 200);
    assert!(close(a.cost_usd, 0.25) && close(resp.cost_usd, 0.25));
    assert_eq!((resp.usage, resp.uncounted_calls), (a.usage, 0));

    // Every call goes to the attempt's fork, with the deadlines the contract sets.
    let fork = fake.calls_to("fs.fork")[0].clone();
    assert_eq!(fork.budget, Budget::new(0, 600_000, 0));
    for model in fake.calls_to("model.complete") {
        assert_eq!(model.budget, Budget::new(1000, 5000, 0));
    }
    let write = &fake.calls_to("fs.write")[0];
    assert_eq!(write.payload["workspace"], "/scratch/fork-0");
    assert_eq!(write.budget.ms, 60_000);
    let check = &fake.calls_to("shell.run")[0];
    assert_eq!(check.payload, json!({ "workspace": "/scratch/fork-0", "command": "check", "timeout_ms": 7000 }));
    assert_eq!(check.budget.ms, 37_000);
    // The fork is dropped by its own call, so a failed drop cannot fail the merge.
    let ends: Vec<(String, Value)> = fake
        .calls()
        .into_iter()
        .filter(|c| c.target == "fs.merge" || c.target == "fs.drop")
        .map(|c| (c.target, c.payload))
        .collect();
    assert_eq!(
        ends,
        [
            ("fs.merge".to_owned(), json!({ "fork": "/scratch/fork-0", "drop": false })),
            ("fs.drop".to_owned(), json!({ "fork": "/scratch/fork-0" })),
        ]
    );

    let run = "trace_test".to_owned();
    assert_eq!(
        fake.events(),
        vec![
            Progress::CheckReady { run: run.clone(), command: Some("check".into()), files: vec![], designed: false },
            Progress::AttemptStarted { run: run.clone(), attempt: 0 },
            Progress::ToolCall {
                run: run.clone(),
                attempt: 0,
                turn: 1,
                tool: "write_file".into(),
                detail: "write hello.txt".into()
            },
            Progress::CheckRan { run: run.clone(), attempt: 0, passed: true, exit_code: Some(0) },
            Progress::AttemptFinished { run, attempt: 0, status: AttemptStatus::Passed },
        ]
    );
}

#[tokio::test]
async fn a_failed_check_is_fed_back_until_the_attempt_fixes_it() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hello\n"),
            3 => {
                let feedback = last_text(req);
                assert!(feedback.contains("The done-check failed"), "{feedback}");
                assert!(feedback.contains("`check`") && feedback.contains("exit code 1"), "{feedback}");
                assert!(feedback.contains("expected hi, got Some(\"hello\\n\")"), "{feedback}");
                write_hello(req, "hi\n")
            }
            _ => done("Done."),
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 1)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    let a = &resp.attempts[0];
    assert_eq!((a.status, a.turns, a.check_runs), (AttemptStatus::Passed, 4, 2));
    let ran: Vec<bool> = fake
        .events()
        .iter()
        .filter_map(|e| match e {
            Progress::CheckRan { passed, .. } => Some(*passed),
            _ => None,
        })
        .collect();
    assert_eq!(ran, [false, true]);
    assert_eq!(fake.workspace()["hello.txt"], "hi\n");
}

#[tokio::test]
async fn check_rounds_are_limited() {
    let fake = Fake::new(|_| done("Nothing to do."), |_, _| exit(1, "still broken\n"));
    let resp = run(&fake, RunRequest { max_check_rounds: Some(2), ..request(Some("check"), 1) }).await;

    assert_eq!(resp.outcome, Outcome::Failed);
    let a = &resp.attempts[0];
    assert_eq!((a.status, a.turns, a.check_runs), (AttemptStatus::Failed, 2, 2));
    assert!(a.note.contains("still broken"), "{}", a.note);
    assert!(resp.summary.contains("No attempt passed the done-check `check`"), "{}", resp.summary);
    assert!(resp.summary.contains("attempt 0: failed"), "{}", resp.summary);
    assert!(fake.calls_to("fs.merge").is_empty());
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn the_first_attempt_to_pass_wins_and_the_others_are_cancelled() {
    let fake = Fake::new(
        |req| match (attempt_of(req), turn(req)) {
            // Slow enough that the others are in their model call when it passes.
            (0, 1) => Reply { delay: Duration::from_millis(50), ..write_hello(req, "hi\n") },
            (0, _) => done("Mine passed."),
            // The others are stuck in a long model call when the winner cancels them.
            _ => Reply { delay: Duration::from_secs(60), ..done("Too late.") },
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 3)).await;

    assert_eq!((resp.outcome, resp.winner), (Outcome::Passed, Some(0)));
    assert_eq!(resp.summary, "Mine passed.");
    let statuses: Vec<AttemptStatus> = resp.attempts.iter().map(|a| a.status).collect();
    assert_eq!(statuses, [AttemptStatus::Passed, AttemptStatus::Cancelled, AttemptStatus::Cancelled]);
    assert_eq!(resp.attempts[1].note, "attempt cancelled");
    assert_eq!(fake.calls_to("fs.fork").len(), 3);
    assert!(fake.forks_alive().is_empty());
    assert_eq!(fake.workspace()["hello.txt"], "hi\n");
    // The cancelled calls were sent and will be billed, but their replies did not come in time to be counted.
    assert_eq!(fake.requests().len(), 4);
    assert!(close(resp.cost_usd, 0.25), "{}", resp.cost_usd);
    assert_eq!(resp.uncounted_calls, 2);
}

#[tokio::test]
async fn replies_to_cancelled_attempts_that_land_soon_are_counted() {
    let fake = Fake::new(
        |req| match (attempt_of(req), turn(req)) {
            (0, 1) => Reply { delay: Duration::from_millis(50), ..write_hello(req, "hi\n") },
            (0, _) => done("Mine passed."),
            _ => Reply { delay: Duration::from_millis(300), ..done("Too late.") },
        },
        hello_check,
    );
    let wait = Duration::from_secs(3);
    let started = tokio::time::Instant::now();
    let resp = run_with(&fake, Config { late_reply_wait: wait, ..config() }, request(Some("check"), 3)).await;

    assert_eq!((resp.outcome, resp.applied), (Outcome::Passed, true));
    assert_eq!(resp.attempts[1].status, AttemptStatus::Cancelled);
    assert_eq!(resp.uncounted_calls, 0);
    assert!(close(resp.cost_usd, 0.5), "{}", resp.cost_usd);
    assert_eq!(resp.usage.input_tokens, 400);
    // The run waited for the replies, not for the whole grace period.
    assert!(started.elapsed() < wait, "{:?}", started.elapsed());
}

/// The designer's check: `sh check.sh` runs check.sh, which the attempt may tamper with.
fn script_check(command: &str, files: &Files) -> shell::RunResponse {
    if command != "sh check.sh" {
        return exit(0, "");
    }
    match files.get("check.sh").map(String::as_str) {
        Some("exit 0\n") => exit(0, ""),
        Some("test -f feature.txt\n") if files.contains_key("feature.txt") => exit(0, ""),
        Some("test -f feature.txt\n") => exit(1, "feature.txt is missing\n"),
        _ => exit(127, "no check.sh\n"),
    }
}

#[tokio::test]
async fn an_attempt_cannot_pass_by_editing_a_check_file() {
    let fake = Fake::new(
        |req| {
            if is_designer(req) {
                return match turn(req) {
                    1 => use_tools(
                        req,
                        &[("write_file", json!({ "path": "check.sh", "content": "test -f feature.txt\n" }))],
                    ),
                    _ => use_tools(
                        req,
                        &[(
                            "submit_check",
                            json!({ "command": "sh check.sh", "files": ["check.sh"], "rationale": "r" }),
                        )],
                    ),
                };
            }
            match turn(req) {
                1 => use_tools(req, &[("write_file", json!({ "path": "check.sh", "content": "exit 0\n" }))]),
                2 => done("Cheated."),
                3 => use_tools(req, &[("write_file", json!({ "path": "feature.txt", "content": "f\n" }))]),
                _ => done("Added feature.txt."),
            }
        },
        script_check,
    );
    let resp = run(&fake, request(None, 1)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    assert_eq!(resp.summary, "Added feature.txt.");
    assert_eq!(resp.attempts[0].check_runs, 2);
    // The restored check landed, not the attempt's edit.
    assert_eq!(fake.workspace()["check.sh"], "test -f feature.txt\n");
    assert_eq!(fake.workspace()["feature.txt"], "f\n");
}

#[tokio::test]
async fn designed_check_files_are_written_into_every_attempt_fork() {
    let script: String = (1..=8).map(|i| format!("# line {i}\n")).collect::<String>() + "test -f feature.txt\n";
    let check_script = script.clone();
    let fake = Fake::new(
        move |req| {
            if is_designer(req) {
                return match turn(req) {
                    1 => use_tools(
                        req,
                        &[
                            ("write_file", json!({ "path": "tests/feature.sh", "content": check_script })),
                            ("run", json!({ "command": "sh tests/feature.sh" })),
                        ],
                    ),
                    _ => use_tools(
                        req,
                        &[(
                            "submit_check",
                            json!({ "command": "sh tests/feature.sh", "files": ["./tests/feature.sh"], "rationale": "r" }),
                        )],
                    ),
                };
            }
            match turn(req) {
                1 => use_tools(req, &[("read_file", json!({ "path": "tests/feature.sh" }))]),
                2 => use_tools(req, &[("write_file", json!({ "path": "feature.txt", "content": "f\n" }))]),
                _ => done("Added feature.txt."),
            }
        },
        |command, files| match command {
            "sh tests/feature.sh" if files.contains_key("feature.txt") => exit(0, ""),
            _ => exit(1, "missing\n"),
        },
    );
    // Reads come back three lines at a time, so the designer's copy must page through.
    fake.set(|st| st.page = 3);
    let resp = run(&fake, request(None, 2)).await;

    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    let spec =
        CheckSpec { command: "sh tests/feature.sh".into(), files: vec!["tests/feature.sh".into()], designed: true };
    assert_eq!(resp.check, Some(spec));

    // The designer's fork is dropped before the attempts fork.
    let calls = fake.calls();
    let order: Vec<String> = calls
        .iter()
        .filter(|c| c.target == "fs.fork" || c.target == "fs.drop")
        .map(|c| format!("{} {}", c.target, c.payload["fork"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(order[..2], ["fs.fork ".to_owned(), "fs.drop /scratch/fork-0".to_owned()]);

    for fork in ["/scratch/fork-1", "/scratch/fork-2"] {
        let written = calls.iter().any(|c| {
            c.target == "fs.write"
                && c.payload["workspace"] == fork
                && c.payload["path"] == "tests/feature.sh"
                && c.payload["content"] == script.as_str()
        });
        assert!(written, "the check file did not reach {fork}");
    }
    let ready = fake.events().into_iter().find(|e| matches!(e, Progress::CheckReady { .. }));
    assert_eq!(
        ready,
        Some(Progress::CheckReady {
            run: "trace_test".into(),
            command: Some("sh tests/feature.sh".into()),
            files: vec!["tests/feature.sh".into()],
            designed: true,
        })
    );
    // The attempts were told about the check and its files.
    let first = &fake.requests().into_iter().find(|r| !is_designer(r)).unwrap().messages[0];
    let text = first["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("`sh tests/feature.sh`") && text.contains("tests/feature.sh."), "{text}");
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn when_no_check_fits_one_unverified_attempt_runs() {
    let fake = Fake::new(
        |req| {
            if is_designer(req) {
                use_tools(req, &[("submit_check", json!({ "command": null, "files": [], "rationale": "A question." }))])
            } else {
                done("The answer is 42.")
            }
        },
        |_, _| panic!("no command should run"),
    );
    let resp = run(&fake, RunRequest { task: "What is the answer?".into(), ..request(None, 3) }).await;

    assert_eq!(resp.outcome, Outcome::Unverified);
    assert_eq!((resp.check, resp.winner, resp.applied), (None, Some(0), false));
    assert_eq!(resp.summary, "The answer is 42.");
    assert_eq!(resp.attempts.len(), 1);
    assert!(resp.changes.is_empty());
    assert!(fake.forks_alive().is_empty());
    let requests = fake.requests();
    let first = &requests.last().unwrap().messages[0];
    assert!(first["content"][0]["text"].as_str().unwrap().contains("no automated done-check"));
    assert!(fake.events().contains(&Progress::CheckReady {
        run: "trace_test".into(),
        command: None,
        files: vec![],
        designed: true
    }));
}

#[tokio::test]
async fn a_designer_that_never_submits_is_nudged_once_then_the_run_fails() {
    let fake = Fake::new(
        |req| match (is_designer(req), turn(req)) {
            (true, _) => done("Looks fine to me."),
            (false, 1) => write_hello(req, "hi\n"),
            (false, _) => done("Wrote it."),
        },
        |_, _| panic!("no command should run"),
    );
    let resp = run(&fake, request(None, 2)).await;

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(is_designer));
    assert!(last_text(&requests[1]).contains("You have not called submit_check"));
    // A designer that gave up is not a task without a check: nothing runs unverified.
    assert_eq!((resp.outcome, resp.applied, resp.attempts.len()), (Outcome::Failed, false, 0));
    assert!(
        resp.summary.contains("Could not design a done-check: the check designer stopped without submitting"),
        "{}",
        resp.summary
    );
    assert!(!fake.workspace().contains_key("hello.txt"));
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn a_designer_out_of_turns_fails_the_run_whatever_the_attempts_turn_limit() {
    let fake = Fake::new(|req| use_tools(req, &[("list_files", json!({}))]), |_, _| exit(0, ""));
    let resp = run(&fake, RunRequest { max_turns: Some(3), ..request(None, 2) }).await;

    // max_turns limits each attempt; the designer has its own limit.
    assert_eq!(fake.requests().len(), 25);
    assert_eq!((resp.outcome, resp.attempts.len()), (Outcome::Failed, 0));
    assert!(resp.summary.contains("ran out of turns without submitting a check"), "{}", resp.summary);
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn a_refusal_ends_the_attempt_and_nothing_is_merged() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => response(vec![json!({ "type": "text", "text": "partial" })], "refusal").into(),
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 1)).await;

    assert_eq!(resp.outcome, Outcome::Failed);
    let a = &resp.attempts[0];
    assert_eq!((a.status, a.note.as_str(), a.turns), (AttemptStatus::Error, "the model declined", 2));
    assert!(resp.summary.contains("the model declined"), "{}", resp.summary);
    assert!(!fake.workspace().contains_key("hello.txt"));
    assert!(fake.calls_to("fs.merge").is_empty() && fake.calls_to("shell.run").is_empty());
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn model_errors_end_the_attempt() {
    let fake = Fake::new(
        |_| Reply {
            resp: Err(RemoteError { code: ErrorCode::Unavailable, message: "gateway down".into() }),
            delay: Duration::ZERO,
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 1)).await;
    assert_eq!(resp.outcome, Outcome::Failed);
    assert_eq!(resp.attempts[0].status, AttemptStatus::Error);
    assert!(resp.attempts[0].note.contains("gateway down"), "{}", resp.attempts[0].note);
}

#[tokio::test]
async fn a_designer_refusal_fails_the_run_before_any_attempt() {
    let fake = Fake::new(|_| response(vec![], "refusal").into(), |_, _| exit(0, ""));
    let resp = run(&fake, request(None, 2)).await;
    assert_eq!(resp.outcome, Outcome::Failed);
    assert!(resp.attempts.is_empty());
    assert!(resp.summary.contains("the model declined"), "{}", resp.summary);
    assert!(close(resp.cost_usd, 0.125));
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn an_exhausted_budget_stops_every_attempt() {
    let fake = Fake::new(|req| use_tools(req, &[("list_files", json!({}))]), hello_check);
    let resp = run(&fake, RunRequest { budget_usd: Some(0.5), ..request(Some("check"), 2) }).await;

    assert_eq!(resp.outcome, Outcome::Failed);
    for a in &resp.attempts {
        assert_eq!((a.status, a.note.as_str()), (AttemptStatus::Cancelled, "budget exhausted"));
    }
    // Each attempt may have had one call in flight when the budget ran out.
    let calls = fake.requests().len();
    assert!((4..=5).contains(&calls), "{calls} model calls");
    assert!((0.5..=0.625).contains(&resp.cost_usd), "{}", resp.cost_usd);
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn tool_results_go_back_together_and_assistant_turns_verbatim() {
    let first_turn = vec![
        json!({ "type": "thinking", "thinking": "", "signature": "sig-1" }),
        json!({ "type": "text", "text": "Let me look." }),
        json!({ "type": "tool_use", "id": "t1", "name": "read_file", "input": { "path": "missing.txt" } }),
        json!({ "type": "tool_use", "id": "t2", "name": "edit_file", "input": { "path": "README.md", "old_string": "zzz", "new_string": "y" } }),
        json!({ "type": "tool_use", "id": "t3", "name": "frobnicate", "input": {} }),
        json!({ "type": "tool_use", "id": "t4", "name": "read_file", "input": { "file": "README.md" } }),
        json!({ "type": "tool_use", "id": "t5", "name": "write_file", "input": { "path": "b.txt", "content": "new\n" } }),
        json!({ "type": "tool_use", "id": "t6", "name": "read_file", "input": { "path": "b.txt" } }),
        json!({ "type": "tool_use", "id": "t7", "name": "search", "input": { "pattern": "project" } }),
    ];
    let scripted = first_turn.clone();
    let fake = Fake::new(
        move |req| match turn(req) {
            1 => response(scripted.clone(), "tool_use").into(),
            _ => done("Done."),
        },
        |_, _| exit(0, ""),
    );
    let resp = run(&fake, request(Some("check"), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);

    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let (one, two) = (&requests[0], &requests[1]);
    assert_eq!(
        (one.system.as_ref(), &one.tools, &one.messages[0]),
        (two.system.as_ref(), &two.tools, &two.messages[0])
    );
    assert_eq!(two.messages.len(), 3);
    assert_eq!(two.messages[1], json!({ "role": "assistant", "content": first_turn }));

    let results = &two.messages[2];
    assert_eq!(results["role"], "user");
    let blocks = results["content"].as_array().unwrap();
    let ids: Vec<&str> = blocks.iter().map(|b| b["tool_use_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["t1", "t2", "t3", "t4", "t5", "t6", "t7"]);
    let errors: Vec<bool> = blocks.iter().map(|b| b["is_error"] == true).collect();
    assert_eq!(errors, [true, true, true, true, false, false, false]);
    let text = |i: usize| blocks[i]["content"].as_str().unwrap();
    assert!(text(0).contains("no such file"), "{}", text(0));
    assert!(text(1).contains("Read the file again"), "{}", text(1));
    assert!(text(2).contains("no tool named \"frobnicate\""), "{}", text(2));
    assert!(text(3).contains("Invalid input for read_file"), "{}", text(3));
    // The read after the write in the same turn sees the write.
    assert_eq!(text(5), "new\n");
    assert_eq!(text(6), "README.md:1: # project");
}

#[tokio::test]
async fn a_call_cut_off_by_the_output_limit_is_not_run() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => {
                let call = json!({ "type": "tool_use", "id": "t1", "name": "write_file", "input": { "path": "hello.txt", "content": "h" } });
                response(vec![call], "max_tokens").into()
            }
            2 => response(vec![json!({ "type": "text", "text": "Now I will" })], "max_tokens").into(),
            3 => write_hello(req, "hi\n"),
            _ => done("Done."),
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);

    let requests = fake.requests();
    let cut = &requests[1].messages[2]["content"][0];
    assert_eq!((cut["tool_use_id"].as_str(), cut["is_error"].as_bool()), (Some("t1"), Some(true)));
    assert!(cut["content"].as_str().unwrap().contains("cut off"));
    assert!(last_text(&requests[2]).contains("Continue where you left off"));
    // Only the complete write ran.
    let writes = fake.calls_to("fs.write");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].payload["content"], "hi\n");
}

#[tokio::test]
async fn max_turns_is_respected() {
    let fake = Fake::new(|req| use_tools(req, &[("list_files", json!({}))]), hello_check);
    let resp = run(&fake, RunRequest { max_turns: Some(3), ..request(Some("check"), 1) }).await;

    assert_eq!(resp.outcome, Outcome::Failed);
    let a = &resp.attempts[0];
    assert_eq!((a.status, a.note.as_str(), a.turns), (AttemptStatus::Failed, "ran out of turns", 3));
    assert_eq!(fake.requests().len(), 3);
    assert!(fake.forks_alive().is_empty());
}

#[tokio::test]
async fn a_merge_conflict_keeps_the_winners_fork() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    fake.set(|st| st.merge_conflict = true);
    let resp = run(&fake, request(Some("check"), 1)).await;

    assert_eq!((resp.outcome, resp.applied), (Outcome::Passed, false));
    assert_eq!(resp.fork.as_deref(), Some("/scratch/fork-0"));
    assert!(
        resp.summary.contains("conflict: hello.txt") && resp.summary.contains("/scratch/fork-0"),
        "{}",
        resp.summary
    );
    assert_eq!(fake.forks_alive(), ["/scratch/fork-0"]);
    assert!(!fake.workspace().contains_key("hello.txt"));
}

#[tokio::test]
async fn without_apply_the_winners_fork_is_returned() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    let resp = run(&fake, RunRequest { apply: false, ..request(Some("check"), 1) }).await;

    assert_eq!((resp.outcome, resp.applied), (Outcome::Passed, false));
    assert_eq!(resp.fork.as_deref(), Some("/scratch/fork-0"));
    assert_eq!(resp.changes.len(), 1);
    assert_eq!(fake.forks_alive(), ["/scratch/fork-0"]);
    assert!(fake.calls_to("fs.merge").is_empty());
    assert!(!fake.workspace().contains_key("hello.txt"));
}

#[tokio::test]
async fn an_unforkable_workspace_is_an_error_reply() {
    let fake = Fake::new(|_| done("x"), |_, _| exit(0, ""));
    let bus: Arc<dyn Bus> = fake.clone();
    let req = RunRequest::new("t", "/elsewhere");
    let err = molt_planner::run(bus, Arc::new(config()), req, TraceId::random()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(fake.requests().is_empty());
}

/// The designer of a run with no check: it submits none.
fn no_check(req: &CompleteRequest) -> Reply {
    use_tools(req, &[("submit_check", json!({ "command": null, "files": [], "rationale": "A question." }))])
}

fn text(t: &str) -> Value {
    json!({ "type": "text", "text": t })
}

#[tokio::test]
async fn a_reply_split_by_the_output_limit_is_returned_whole() {
    let fake = Fake::new(
        |req| match (is_designer(req), turn(req)) {
            (true, _) => no_check(req),
            (false, 1) => {
                let call = json!({ "type": "tool_use", "id": "t1", "name": "list_files", "input": {} });
                response(vec![text("Let me look."), call], "tool_use").into()
            }
            (false, 2) => response(vec![text("PART ONE of the answer,")], "max_tokens").into(),
            (false, _) => done("PART TWO of the answer."),
        },
        |_, _| panic!("no command should run"),
    );
    let resp = run(&fake, RunRequest { task: "What is the answer?".into(), ..request(None, 1) }).await;

    assert_eq!(resp.outcome, Outcome::Unverified);
    // Text before a tool call is not part of the reply; text cut off by the limit is.
    assert_eq!(resp.summary, "PART ONE of the answer,\n\nPART TWO of the answer.");
}

#[tokio::test]
async fn an_empty_final_reply_is_asked_for_once() {
    let script = |answer: bool| {
        move |req: &CompleteRequest| -> Reply {
            if is_designer(req) {
                return no_check(req);
            }
            if last_text(req).contains("Your reply was empty") {
                return if answer { done("The answer is 42.") } else { response(vec![], "end_turn").into() };
            }
            match turn(req) {
                1 => {
                    let input = json!({ "path": "README.md" });
                    let call = json!({ "type": "tool_use", "id": "t1", "name": "read_file", "input": input });
                    response(vec![text("The answer is 42. Let me confirm in the README."), call], "tool_use").into()
                }
                // Models may end their turn with no content after a tool result.
                _ => response(vec![], "end_turn").into(),
            }
        }
    };
    let question = RunRequest { task: "What is the answer?".into(), ..request(None, 1) };

    let fake = Fake::new(script(true), |_, _| panic!("no command should run"));
    let resp = run(&fake, question.clone()).await;
    assert_eq!((resp.outcome, resp.summary.as_str()), (Outcome::Unverified, "The answer is 42."));
    assert_eq!(resp.attempts[0].turns, 3);
    let asked = fake.requests().iter().filter(|r| last_text(r).contains("Your reply was empty")).count();
    assert_eq!(asked, 1);

    // A second empty reply is accepted rather than asked for again.
    let fake = Fake::new(script(false), |_, _| panic!("no command should run"));
    let resp = run(&fake, question).await;
    assert_eq!(resp.summary, "The attempt finished without a summary.");
    assert_eq!(resp.attempts[0].turns, 3);
}

#[tokio::test]
async fn only_new_files_can_be_check_files() {
    let fake = Fake::new(
        |req| {
            if !is_designer(req) {
                return match turn(req) {
                    1 => write_hello(req, "hi\n"),
                    _ => done("Wrote hello.txt."),
                };
            }
            let submit = |files: &[&str]| {
                use_tools(
                    req,
                    &[("submit_check", json!({ "command": "sh tests/check.sh", "files": files, "rationale": "r" }))],
                )
            };
            match turn(req) {
                // A test added to an existing file, which every check run would reset to this copy.
                1 => use_tools(
                    req,
                    &[
                        ("write_file", json!({ "path": "README.md", "content": "# project\ntest\n" })),
                        ("write_file", json!({ "path": "tests/check.sh", "content": "test -f hello.txt\n" })),
                    ],
                ),
                2 => submit(&["tests/check.sh", "README.md"]),
                3 => submit(&["Cargo.toml"]),
                _ => submit(&["tests/check.sh"]),
            }
        },
        |command, files| match command {
            "sh tests/check.sh" if files.get("hello.txt").map(String::as_str) == Some("hi\n") => exit(0, ""),
            _ => exit(1, "no hello\n"),
        },
    );
    fake.set(|st| {
        st.workspace.insert("Cargo.toml".into(), "[package]\n".into());
    });
    let resp = run(&fake, request(None, 1)).await;

    let designer: Vec<CompleteRequest> = fake.requests().into_iter().filter(is_designer).collect();
    assert_eq!(designer.len(), 4);
    for (req, problem) in [
        (&designer[2], "README.md as a check file: it already exists"),
        (&designer[3], "Cargo.toml as a check file: it is not a file you created"),
    ] {
        let result = &req.messages.last().unwrap()["content"][0];
        assert_eq!(result["is_error"], true);
        let message = result["content"].as_str().unwrap();
        assert!(message.contains(problem) && message.contains("tests/"), "{message}");
    }
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);
    assert_eq!(resp.check.unwrap().files, ["tests/check.sh"]);
    assert_eq!(fake.workspace()["README.md"], "# project\n");
}

#[tokio::test]
async fn a_fork_that_cannot_be_dropped_after_the_merge_still_counts_as_applied() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    fake.set(|st| st.drop_fails = true);
    let resp = run(&fake, request(Some("check"), 1)).await;

    assert_eq!((resp.outcome, resp.applied, resp.fork.as_deref()), (Outcome::Passed, true, None));
    assert_eq!(resp.summary, "Wrote hello.txt.");
    assert_eq!(fake.workspace()["hello.txt"], "hi\n");
    assert_eq!(fake.calls_to("fs.drop").len(), 1);
}

#[tokio::test]
async fn a_merge_that_stops_partway_is_reported_as_partial() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => use_tools(
                req,
                &[
                    ("write_file", json!({ "path": "a.txt", "content": "a\n" })),
                    ("write_file", json!({ "path": "hello.txt", "content": "hi\n" })),
                ],
            ),
            _ => done("Wrote a.txt and hello.txt."),
        },
        hello_check,
    );
    fake.set(|st| st.merge_partial = true);
    let resp = run(&fake, request(Some("check"), 1)).await;

    assert_eq!((resp.outcome, resp.applied), (Outcome::Passed, false));
    assert_eq!(resp.fork.as_deref(), Some("/scratch/fork-0"));
    assert!(
        resp.summary.contains("The merge stopped partway, so the workspace has only some of the changes: partial:")
            && resp.summary.contains("written: a.txt")
            && resp.summary.contains("/scratch/fork-0"),
        "{}",
        resp.summary
    );
    assert_eq!(fake.workspace().get("a.txt").map(String::as_str), Some("a\n"));
    assert_eq!(fake.forks_alive(), ["/scratch/fork-0"]);
}

#[tokio::test]
async fn a_truncated_patch_is_flagged() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    fake.set(|st| st.patch_truncated = true);
    let resp = run(&fake, request(Some("check"), 1)).await;
    assert_eq!((resp.outcome, resp.patch_truncated, resp.changes.len()), (Outcome::Passed, true, 1));
}

#[tokio::test]
async fn memory_gives_every_conversation_the_project_context_and_its_tools() {
    let fake = Fake::new(
        |req| match turn(req) {
            1 if is_designer(req) => use_tools(
                req,
                &[("submit_check", json!({ "command": "check", "files": [], "rationale": "hello.txt says hi" }))],
            ),
            1 => {
                use_tools(req, &[("find_symbol", json!({ "name": "greet" })), ("recall", json!({ "query": "hello" }))])
            }
            2 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    fake.set(|st| st.memory = true);
    let resp = run(&fake, request(None, 1)).await;
    assert_eq!(resp.outcome, Outcome::Passed, "{}", resp.summary);

    // Memory is asked for notes (which shows it answers), then the model is
    // brought up to date, before anything else happens.
    let calls = fake.calls();
    let targets: Vec<&str> = calls.iter().take(3).map(|c| c.target.as_str()).collect();
    assert_eq!(targets, ["memory.recall", "memory.index", "memory.map"]);
    assert_eq!(calls[2].payload["query"], "Add hello.txt saying hi.");
    assert_eq!(calls[2].payload["max_tokens"], 3000);

    let requests = fake.requests();
    for req in &requests {
        let context = req.messages[0]["content"][0]["text"].as_str().unwrap();
        assert!(context.starts_with("<project_context>"), "{context}");
        assert!(context.contains("- [lesson 0.70] hello.txt must end with a newline"), "{context}");
        assert!(context.contains("src/greet.rs:\n  3: pub fn greet"), "{context}");
        assert!(req.messages[0]["content"][1]["text"].as_str().unwrap().starts_with("<task>"));
        let names: Vec<&str> = req.tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"find_symbol") && names.contains(&"recall"), "{names:?}");
    }

    // Memory's answers come back as tool results.
    let second = requests.iter().find(|r| !is_designer(r) && turn(r) == 2).unwrap();
    let results = last_text(second);
    assert!(results.contains("Defined at:\nsrc/greet.rs:3: function pub fn greet(name: &str) -> String"), "{results}");
    assert!(results.contains("- [lesson 0.70] hello.txt must end"), "{results}");
    assert_eq!(fake.calls_to("memory.symbols")[0].payload["name"], "greet");
    assert!(fake.events().iter().any(|e| matches!(e, Progress::Note { message, .. }
        if message == "project model: 3 files, 7 definitions (3 parsed again, 0 ms)")));

    // Without memory, the same run offers neither the block nor the tools.
    let plain = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    assert_eq!(run(&plain, request(Some("check"), 1)).await.outcome, Outcome::Passed);
    let first = &plain.requests()[0];
    assert!(first.messages[0]["content"][0]["text"].as_str().unwrap().starts_with("<task>"));
    assert_eq!(first.tools.len(), 6);
    assert_eq!(plain.calls_to("memory.recall").len(), 1, "asked once, then left alone");
    assert!(plain.calls_to("memory.index").is_empty() && plain.calls_to("memory.map").is_empty());

    // A memory that does not answer is given up on at once, and said so.
    let hung = Fake::new(
        |req| match turn(req) {
            1 => write_hello(req, "hi\n"),
            _ => done("Wrote hello.txt."),
        },
        hello_check,
    );
    hung.set(|st| st.memory_hangs = true);
    assert_eq!(run(&hung, request(Some("check"), 1)).await.outcome, Outcome::Passed);
    assert_eq!(hung.calls().iter().filter(|c| c.target.starts_with("memory.")).count(), 1);
    assert_eq!(hung.calls_to("memory.recall")[0].budget.ms, 10_000);
    assert_eq!(hung.requests()[0].tools.len(), 6);
    assert!(hung.events().iter().any(|e| matches!(e, Progress::Note { message, .. }
        if message == "running without memory: memory.recall did not answer")));
}

#[tokio::test]
async fn unknown_model_prices_are_reported_and_stop_further_actions() {
    let fake = Fake::new(
        |req| {
            let mut reply = write_hello(req, "hi\n");
            reply.resp.as_mut().unwrap().cost_usd = None;
            reply
        },
        hello_check,
    );
    let resp = run(&fake, request(Some("check"), 1)).await;
    assert_eq!(resp.outcome, Outcome::Failed);
    assert_eq!(resp.uncounted_calls, 1);
    assert_eq!(resp.cost_usd, 0.0); // Known subtotal, explicitly incomplete.
    assert!(!resp.applied);
    assert!(fake.calls_to("fs.write").is_empty());
    assert!(fake.calls_to("shell.run").is_empty());
}
