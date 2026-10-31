//! Plans (`planner.plan`, #47) against a fake bus: a scripted model, an
//! in-memory `fs` whose forks remember what they were made from and merge
//! back into it, and a `shell` whose checks look at a fork's files.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use molt_api::fs::{self, Change, ChangeKind};
use molt_api::model::{CompleteRequest, CompleteResponse, Usage};
use molt_api::plan::{PlanRequest, PlanResponse};
use molt_api::planner::{AttemptStatus, Continuation, Outcome, RunRequest, Verification};
use molt_api::progress::Progress;
use molt_api::shell;
use molt_planner::{Bus, Config};
use molt_proto::{Budget, ErrorCode, RemoteError, TraceId};
use serde_json::{json, Value};

const WORKSPACE: &str = "/work/project";
const TASK: &str = "Add the three step files.";

type Files = BTreeMap<String, String>;
type Model = Box<dyn Fn(&CompleteRequest) -> CompleteResponse + Send + Sync>;

struct Fork {
    origin: String,
    base: Files,
    now: Files,
}

#[derive(Default)]
struct State {
    workspace: Files,
    forks: BTreeMap<String, Fork>,
    forked: u32,
    requests: Vec<CompleteRequest>,
    events: Vec<Progress>,
    /// Commands run, with the workspace they ran in.
    commands: Vec<(String, String)>,
}

struct Fake {
    state: Mutex<State>,
    model: Model,
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

fn changes(base: &Files, now: &Files) -> Vec<Change> {
    let mut out: Vec<Change> = now
        .iter()
        .filter(|(path, content)| base.get(*path) != Some(content))
        .map(|(path, _)| Change {
            path: path.clone(),
            kind: if base.contains_key(path) { ChangeKind::Modified } else { ChangeKind::Added },
        })
        .collect();
    out.extend(
        base.keys().filter(|p| !now.contains_key(*p)).map(|p| Change { path: p.clone(), kind: ChangeKind::Deleted }),
    );
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

impl State {
    fn files(&mut self, at: &str) -> Result<&mut Files, RemoteError> {
        if at == WORKSPACE {
            return Ok(&mut self.workspace);
        }
        self.forks.get_mut(at).map(|f| &mut f.now).ok_or_else(|| invalid(format!("no workspace {at}")))
    }
}

/// What a check finds in `files`: `check-N` passes when step files 1 to N
/// are there, `check-all` when all three are.
fn check(command: &str, files: &Files) -> shell::RunResponse {
    let upto = match command {
        "check-all" => 3,
        other => other.strip_prefix("check-").and_then(|n| n.parse::<usize>().ok()).unwrap_or_default(),
    };
    let missing: Vec<String> = (1..=upto)
        .map(|n| format!("step{n}.txt"))
        .filter(|p| files.get(p).map(String::as_str) != Some("done\n"))
        .collect();
    shell::RunResponse {
        exit_code: Some(if missing.is_empty() { 0 } else { 1 }),
        signal: None,
        timed_out: false,
        cancelled: false,
        stdout: String::new(),
        stderr: if missing.is_empty() { String::new() } else { format!("missing {}\n", missing.join(", ")) },
        truncated: false,
        duration_ms: 1,
        offline_failure: false,
        outputs: Vec::new(),
    }
}

impl Fake {
    fn new(model: impl Fn(&CompleteRequest) -> CompleteResponse + Send + Sync + 'static) -> Arc<Self> {
        let state =
            State { workspace: Files::from([("README.md".to_owned(), "# project\n".to_owned())]), ..State::default() };
        Arc::new(Self { state: Mutex::new(state), model: Box::new(model) })
    }

    fn workspace(&self) -> Files {
        self.state.lock().unwrap().workspace.clone()
    }

    fn fork_files(&self, fork: &str) -> Files {
        self.state.lock().unwrap().forks[fork].now.clone()
    }

    fn forks_alive(&self) -> Vec<String> {
        self.state.lock().unwrap().forks.keys().cloned().collect()
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
        Ok(match target {
            fs::READ => {
                let r: fs::ReadRequest = serde_json::from_value(payload).unwrap();
                let content = st.files(&r.workspace)?.get(&r.path).cloned().ok_or_else(|| invalid("no such file"))?;
                let lines = content.lines().count() as u64;
                json!(fs::ReadResponse { content, first_line: 1, lines, total_lines: lines, truncated: false })
            }
            fs::WRITE => {
                let w: fs::WriteRequest = serde_json::from_value(payload).unwrap();
                let bytes = w.content.len() as u64;
                let created = st.files(&w.workspace)?.insert(w.path, w.content).is_none();
                json!(fs::WriteResponse { bytes, created, syntax: None })
            }
            fs::FORK => {
                let f: fs::ForkRequest = serde_json::from_value(payload).unwrap();
                let files = st.files(&f.workspace)?.clone();
                let fork = format!("/scratch/fork-{}", st.forked);
                st.forked += 1;
                let count = files.len() as u64;
                st.forks.insert(fork.clone(), Fork { origin: f.workspace, base: files.clone(), now: files });
                json!(fs::ForkResponse { fork, files: count })
            }
            fs::DIFF => {
                let d: fs::DiffRequest = serde_json::from_value(payload).unwrap();
                let fork = st.forks.get(&d.fork).ok_or_else(|| invalid("no such fork"))?;
                let changes = changes(&fork.base, &fork.now);
                let patch = changes.iter().map(|c| format!("--- a/{0}\n+++ b/{0}\n+done\n", c.path)).collect();
                json!(fs::DiffResponse { changes, patch, truncated: false, left_out: None })
            }
            fs::MERGE => {
                // Into what the fork was made from: the workspace, or another fork.
                let m: fs::MergeRequest = serde_json::from_value(payload).unwrap();
                let fork = st.forks.get(&m.fork).ok_or_else(|| invalid("no such fork"))?;
                let (origin, now) = (fork.origin.clone(), fork.now.clone());
                let changes = changes(&fork.base, &now);
                let into = st.files(&origin)?;
                for c in &changes {
                    match now.get(&c.path) {
                        Some(content) => into.insert(c.path.clone(), content.clone()),
                        None => into.remove(&c.path),
                    };
                }
                if m.drop {
                    st.forks.remove(&m.fork);
                }
                json!(fs::MergeResponse { changes, operation: None, left_out: None })
            }
            fs::DROP => {
                let d: fs::DropRequest = serde_json::from_value(payload).unwrap();
                json!(fs::DropResponse { dropped: st.forks.remove(&d.fork).is_some() })
            }
            "shell.run" => {
                let r: shell::RunRequest = serde_json::from_value(payload).unwrap();
                st.commands.push((r.command.clone(), r.workspace.clone()));
                let files = st.files(&r.workspace)?.clone();
                json!(check(&r.command, &files))
            }
            other => return Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {other}") }),
        })
    }
}

#[async_trait]
impl Bus for Fake {
    async fn call(&self, target: &str, payload: Value, _: Budget, trace: &TraceId) -> Result<Value, RemoteError> {
        // A plan is one episode: every call of every step is under its trace.
        assert_eq!(trace.as_str(), "trace_plan", "{target}");
        if target == "model.complete" {
            let req: CompleteRequest = serde_json::from_value(payload).unwrap();
            self.state.lock().unwrap().requests.push(req.clone());
            return Ok(serde_json::to_value((self.model)(&req)).unwrap());
        }
        self.fs(target, payload)
    }

    async fn publish(&self, topic: &str, payload: Value) {
        assert_eq!(topic, "progress");
        self.state.lock().unwrap().events.push(serde_json::from_value(payload).unwrap());
    }
}

// --- Scripting -------------------------------------------------------------

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
        price_source: None,
        context_tokens: None,
        endpoint: None,
    }
}

fn call(req: &CompleteRequest, name: &str, input: Value) -> CompleteResponse {
    let id = format!("toolu_{}", req.messages.len());
    response(vec![json!({ "type": "tool_use", "id": id, "name": name, "input": input })], "tool_use")
}

fn done(text: &str) -> CompleteResponse {
    response(vec![json!({ "type": "text", "text": text })], "end_turn")
}

fn offers(req: &CompleteRequest, tool: &str) -> bool {
    req.tools.iter().any(|t| t["name"] == tool)
}

/// Every text of the conversation's first message.
fn first_text(req: &CompleteRequest) -> String {
    let blocks = req.messages[0]["content"].as_array().cloned().unwrap_or_default();
    blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
}

/// The step, from 1, whose run `req` belongs to; `None` for the plan's own calls.
fn step_of(req: &CompleteRequest) -> Option<usize> {
    let text = first_text(req);
    let at = text.find("Do step ")? + "Do step ".len();
    text[at..].split(' ').next()?.parse().ok()
}

const STEPS: [&str; 3] = ["Add step1.txt.", "Add step2.txt.", "Add step3.txt."];

/// The model of a plan whose steps each add their file: the step designer
/// plans `STEPS`, each check designer designs `check-N` (the overall one
/// `check-all`), and each step's attempt writes its file, or, for step
/// `fails`, a wrong one.
fn model(fails: Option<usize>) -> impl Fn(&CompleteRequest) -> CompleteResponse + Send + Sync + 'static {
    model_from(1, fails)
}

/// [`model`] for a plan whose first step is step `first` of [`STEPS`].
fn model_from(
    first: usize,
    fails: Option<usize>,
) -> impl Fn(&CompleteRequest) -> CompleteResponse + Send + Sync + 'static {
    move |req| {
        if offers(req, "submit_plan") {
            return call(req, "submit_plan", json!({ "steps": STEPS, "rationale": "One file per step." }));
        }
        let step = step_of(req).map(|n| n + first - 1);
        if offers(req, "submit_check") {
            let command = step.map_or("check-all".to_owned(), |n| format!("check-{n}"));
            return call(req, "submit_check", json!({ "command": command, "files": [], "rationale": "r" }));
        }
        let n = step.expect("an attempt works on a step");
        match req.messages.len() {
            1 => {
                let content = if fails == Some(n) { "not yet\n" } else { "done\n" };
                call(req, "write_file", json!({ "path": format!("step{n}.txt"), "content": content }))
            }
            _ => done(&format!("Added step{n}.txt.")),
        }
    }
}

fn config() -> Config {
    Config {
        max_tokens: 1000,
        model_timeout: Duration::from_secs(5),
        check_timeout: Duration::from_secs(7),
        late_reply_wait: Duration::from_millis(50),
        max_check_rounds: 1,
        ..Config::default()
    }
}

fn request() -> PlanRequest {
    PlanRequest::new(RunRequest { attempts: 1, review: false, ..RunRequest::new(TASK, WORKSPACE) })
}

async fn plan(fake: &Arc<Fake>, req: PlanRequest) -> PlanResponse {
    plan_with(fake, config(), req).await
}

async fn plan_with(fake: &Arc<Fake>, cfg: Config, req: PlanRequest) -> PlanResponse {
    let bus: Arc<dyn Bus> = fake.clone();
    let run = molt_planner::plan(bus, Arc::new(cfg), req, TraceId::from_raw("trace_plan"));
    tokio::time::timeout(Duration::from_secs(10), run).await.expect("the plan took too long").unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The steps each `Step` event started, from 0.
fn started(fake: &Fake) -> Vec<u32> {
    fake.events()
        .iter()
        .filter_map(|e| match e {
            Progress::Step { step, .. } => Some(*step),
            _ => None,
        })
        .collect()
}

// --- Tests -------------------------------------------------------------------

#[tokio::test]
async fn three_steps_run_in_order_each_with_its_own_check_and_result() {
    let fake = Fake::new(model(None));
    let resp = plan(&fake, request()).await;
    let result = resp.result.as_deref().unwrap();
    assert_eq!(result.outcome, Outcome::Passed, "{}", result.summary);
    assert_eq!(resp.steps, STEPS);
    assert!(!resp.edited && resp.proposed.is_empty() && resp.remaining.is_empty());
    assert_eq!(resp.rationale, "One file per step.");

    // Each step ran, in order, with a check of its own, designed for it, and a result of its own.
    assert_eq!(resp.ran.len(), 3);
    for (at, step) in resp.ran.iter().enumerate() {
        let n = at + 1;
        assert_eq!((step.step, step.goal.as_str()), (at as u32, STEPS[at]));
        assert_eq!(step.run.outcome, Outcome::Passed, "{}", step.run.summary);
        let check = step.run.check.as_ref().unwrap();
        assert_eq!((check.command.as_str(), check.designed), (format!("check-{n}").as_str(), true));
        // Its own changes only: the steps before it were already in what it forked from.
        assert_eq!(step.run.changes, [Change { path: format!("step{n}.txt"), kind: ChangeKind::Added }]);
        assert!(step.run.applied, "merged into the plan's result");
        assert_eq!(step.run.summary, format!("Added step{n}.txt."));
        assert!(step.run.conversation.is_empty());
        assert_eq!(step.run.attempts.len(), 1);
        assert!(close(step.run.cost_usd, 0.375), "{}", step.run.cost_usd);
    }
    assert_eq!(started(&fake), [0, 1, 2]);
    let events = fake.events();
    assert!(matches!(&events[..], [Progress::Planned { steps, edited: false, .. }, ..] if steps == &STEPS));

    // Every step's attempt forked from the result of the steps before it, and its check ran there.
    let staging = match &events[1..].iter().find(|e| matches!(e, Progress::Step { .. })) {
        Some(Progress::Step { fork: Some(fork), .. }) => fork.clone(),
        other => panic!("{other:?}"),
    };
    let commands = fake.state.lock().unwrap().commands.clone();
    let ran: Vec<&str> = commands.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(ran, ["check-1", "check-2", "check-3", "check-all"]);
    assert!(commands.iter().all(|(_, at)| at != WORKSPACE && at != &staging), "{commands:?}");

    // The overall check was designed for the whole task first and passed on the whole result, which is applied.
    let overall = result.check.as_ref().unwrap();
    assert_eq!((overall.command.as_str(), overall.designed), ("check-all", true));
    assert_eq!(result.verification, Some(Verification::Command));
    assert!(result.applied && result.fork.is_none());
    let paths: Vec<&str> = result.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["step1.txt", "step2.txt", "step3.txt"]);
    for n in 1..=3 {
        assert_eq!(fake.workspace()[&format!("step{n}.txt")], "done\n");
    }
    assert!(fake.forks_alive().is_empty(), "{:?}", fake.forks_alive());
    assert!(result.summary.contains("1. Add step1.txt.: passed (`check-1`)"), "{}", result.summary);
    assert!(result.summary.contains("The overall check `check-all` passed."), "{}", result.summary);

    // The plan's calls (steps, overall check) and the steps' 3 calls each: 11, all in the total.
    assert_eq!(fake.requests().len(), 11);
    assert!(close(resp.cost_usd, 11.0 * 0.125) && close(result.cost_usd, resp.cost_usd), "{}", resp.cost_usd);
    assert_eq!(resp.usage.input_tokens, 1100);
    assert_eq!((resp.uncounted_calls, result.uncounted_calls), (0, 0));
    // The step designer speaks with its own prompt, which is on record.
    assert!(result.prompts.iter().any(|p| p.role == "steps"), "{:?}", result.prompts);
}

#[tokio::test]
async fn when_step_two_fails_step_three_never_starts_and_step_one_is_kept_to_apply_or_go_on_from() {
    let fake = Fake::new(model(Some(2)));
    let resp = plan(&fake, request()).await;
    let result = resp.result.as_deref().unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(resp.ran.len(), 2);
    assert_eq!(resp.ran[0].run.outcome, Outcome::Passed);
    assert_eq!(resp.ran[1].run.outcome, Outcome::Failed);
    assert_eq!(resp.ran[1].run.attempts[0].status, AttemptStatus::Failed);
    assert_eq!(started(&fake), [0, 1]);
    assert!(fake.requests().iter().all(|r| step_of(r) != Some(3)), "step 3 never ran");
    assert_eq!(resp.remaining, &STEPS[1..]);

    // Nothing was applied: the workspace is as it was, and step 1's result is kept, with it alone.
    assert!(!result.applied);
    assert!(!fake.workspace().contains_key("step1.txt"));
    let kept = result.fork.clone().unwrap();
    assert_eq!(fake.forks_alive(), std::slice::from_ref(&kept), "only the plan's result is left");
    let files = fake.fork_files(&kept);
    assert_eq!((files.get("step1.txt").map(String::as_str), files.get("step2.txt")), (Some("done\n"), None));
    assert_eq!(result.changes, [Change { path: "step1.txt".into(), kind: ChangeKind::Added }]);
    assert!(result.summary.contains("2. Add step2.txt.: did not pass"), "{}", result.summary);
    assert!(result.summary.contains("3. Add step3.txt.: not started"), "{}", result.summary);
    assert!(result.summary.contains("The result of step 1 is kept in"), "{}", result.summary);
    assert!(result.summary.contains("`check-all` did not run"), "{}", result.summary);

    // It can go on from the failed step in the kept result: the remaining steps, nothing designed again.
    let fixed = Fake::new(model_from(2, None));
    fixed.state.lock().unwrap().forks = std::mem::take(&mut fake.state.lock().unwrap().forks);
    fixed.state.lock().unwrap().forked = 100;
    let mut again = request();
    again.steps = resp.remaining.clone();
    again.run.check = Some("check-all".into());
    again.run.continues = Some(Continuation { conversation: Vec::new(), fork: Some(kept.clone()) });
    let resumed = plan(&fixed, again).await;
    let done = resumed.result.as_deref().unwrap();
    assert_eq!(done.outcome, Outcome::Passed, "{}", done.summary);
    assert_eq!(resumed.ran.iter().map(|s| s.goal.as_str()).collect::<Vec<_>>(), &STEPS[1..]);
    assert!(fixed.requests().iter().all(|r| !offers(r, "submit_plan")));
    assert!(done.applied);
    for n in 1..=3 {
        assert_eq!(fixed.workspace()[&format!("step{n}.txt")], "done\n");
    }

    // Or the kept result is applied as it is: it was made from the workspace, so it merges there.
    let keep = Fake::new(model(Some(2)));
    let resp = plan(&keep, request()).await;
    let kept = resp.result.unwrap().fork.unwrap();
    keep.fs(fs::MERGE, json!({ "fork": kept, "drop": true })).unwrap();
    assert_eq!(keep.workspace().get("step1.txt").map(String::as_str), Some("done\n"));
    assert!(!keep.workspace().contains_key("step2.txt"));
}

#[tokio::test]
async fn an_edited_plan_is_the_one_that_runs_and_the_edit_is_on_record() {
    let fake = Fake::new(model(None));
    // The proposal: the steps alone, nothing run, no fork made.
    let proposal = plan(&fake, PlanRequest { propose: true, ..request() }).await;
    assert_eq!(proposal.steps, STEPS);
    assert!(proposal.result.is_none() && proposal.ran.is_empty() && proposal.error.is_none());
    assert_eq!(fake.requests().len(), 1);
    assert!(close(proposal.cost_usd, 0.125));
    assert!(fake.forks_alive().is_empty() && fake.events().iter().all(|e| !matches!(e, Progress::Planned { .. })));
    let designer = &fake.requests()[0];
    assert!(designer.system.as_deref().unwrap().starts_with("You plan a software task"));
    assert!(first_text(designer).contains(&format!("<task>\n{TASK}\n</task>")));

    // The person drops the second step and words the last one their way.
    let edited = vec!["Add step1.txt.".to_owned(), "Add step3.txt, and step2.txt with it.".to_owned()];
    let fake = Fake::new(model(None));
    let req = PlanRequest { steps: edited.clone(), proposed: proposal.steps.clone(), ..request() };
    let resp =
        plan(&fake, PlanRequest { run: RunRequest { check: Some("check-1".into()), ..req.run.clone() }, ..req }).await;
    assert!(resp.edited);
    assert_eq!((resp.steps.clone(), resp.proposed.clone()), (edited.clone(), STEPS.map(String::from).to_vec()));
    assert!(fake
        .events()
        .iter()
        .any(|e| matches!(e, Progress::Planned { steps, edited: true, .. } if steps == &edited)));
    assert!(fake.events().iter().any(|e| matches!(e, Progress::Note { message, .. }
        if message == "the plan as edited: 2 steps, 3 proposed")));
    // Nothing designs the steps again, and each run is told the edited plan.
    let requests = fake.requests();
    assert!(requests.iter().all(|r| !offers(r, "submit_plan")));
    assert_eq!(resp.ran.iter().map(|s| s.goal.clone()).collect::<Vec<_>>(), edited);
    for r in requests.iter().filter(|r| step_of(r).is_some()) {
        let text = first_text(r);
        assert!(text.contains("2. Add step3.txt, and step2.txt with it."), "{text}");
        assert!(!text.contains("Add step2.txt."), "{text}");
    }
    // The given check is the overall one: nothing designs it.
    assert!(requests.iter().filter(|r| offers(r, "submit_check")).all(|r| step_of(r).is_some()));
    assert_eq!(resp.result.unwrap().check.unwrap().command, "check-1");
}

#[tokio::test]
async fn one_budget_covers_every_step() {
    // Steps and the overall check given: each step is a design and two attempt turns, $0.375.
    let given = || PlanRequest {
        steps: STEPS.map(String::from).to_vec(),
        run: RunRequest { check: Some("check-all".into()), ..request().run },
        ..request()
    };
    // $1: steps 1 and 2 leave $0.25, which step 3 spends on its design and its attempt's first turn.
    let fake = Fake::new(model(None));
    let resp = plan(&fake, PlanRequest { run: RunRequest { budget_usd: Some(1.0), ..given().run }, ..given() }).await;
    let result = resp.result.as_deref().unwrap();
    assert_eq!(result.outcome, Outcome::Failed, "{}", result.summary);
    assert_eq!(started(&fake), [0, 1, 2]);
    let spent: Vec<f64> = resp.ran.iter().map(|s| s.run.cost_usd).collect();
    assert_eq!(spent, [0.375, 0.375, 0.25]);
    assert_eq!(resp.ran[2].run.outcome, Outcome::Failed);
    assert!(close(resp.cost_usd, 1.0) && close(result.cost_usd, 1.0), "{}", resp.cost_usd);
    assert_eq!(fake.requests().len(), 8, "no call once the plan's budget is spent");
    assert_eq!(resp.remaining, &STEPS[2..]);
    assert!(!result.applied && result.fork.is_some(), "steps 1 and 2 are kept");

    // $0.75: step 2 has $0.375, which its attempt runs out of before its check, so step 3 never starts.
    let fake = Fake::new(model(None));
    let resp = plan(&fake, PlanRequest { run: RunRequest { budget_usd: Some(0.75), ..given().run }, ..given() }).await;
    assert_eq!(started(&fake), [0, 1]);
    assert_eq!(resp.ran[1].run.attempts[0].note, "budget exhausted");
    let summary = &resp.result.as_ref().unwrap().summary;
    assert!(summary.contains("3. Add step3.txt.: not started"), "{summary}");
    assert_eq!(fake.requests().len(), 6);
    assert!(close(resp.cost_usd, 0.75), "{}", resp.cost_usd);

    // A call whose cost is not known leaves what is left unknown too: no step starts on it.
    let fake = Fake::new(move |req| {
        let mut resp = model(None)(req);
        if offers(req, "submit_plan") {
            resp.cost_usd = None;
        }
        resp
    });
    let resp =
        plan(&fake, PlanRequest { run: RunRequest { check: Some("check-all".into()), ..request().run }, ..request() })
            .await;
    assert!(started(&fake).is_empty());
    assert_eq!(fake.requests().len(), 1);
    let result = resp.result.as_deref().unwrap();
    assert_eq!((resp.uncounted_calls, result.uncounted_calls), (1, 1));
    assert_eq!(resp.remaining, STEPS);
    assert!(
        result
            .summary
            .contains("1. Add step1.txt.: did not start: 1 model calls of the plan so far have an unknown cost"),
        "{}",
        result.summary
    );
}

#[tokio::test]
async fn a_plan_is_refused_what_it_cannot_do() {
    let fake = Fake::new(model(None));
    let bus: Arc<dyn Bus> = fake.clone();
    let cfg = Arc::new(config());
    let refused = |req: PlanRequest| {
        let (bus, cfg) = (bus.clone(), cfg.clone());
        async move { molt_planner::plan(bus, cfg, req, TraceId::from_raw("trace_plan")).await.unwrap_err() }
    };
    for (req, says) in [
        (PlanRequest { run: RunRequest { checkpoint: true, ..request().run }, ..request() }, "keeps no checkpoint"),
        (
            PlanRequest {
                run: RunRequest {
                    rubric: vec![molt_api::planner::Criterion { text: "c".into(), command: None }],
                    ..request().run
                },
                ..request()
            },
            "overall check is a command",
        ),
        (PlanRequest { run: RunRequest { workflow: Some("ask".into()), ..request().run }, ..request() }, "only reads"),
        (PlanRequest { propose: true, steps: vec!["a".into()], ..request() }, "leave it out"),
        (PlanRequest { steps: vec!["a".into(), " ".into()], ..request() }, "step 2 has no goal"),
        (PlanRequest { proposed: vec!["a".into()], ..request() }, "proposed without steps"),
    ] {
        let err = refused(req).await;
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains(says), "{says}: {}", err.message);
    }
    assert!(fake.requests().is_empty() && fake.forks_alive().is_empty());
}
