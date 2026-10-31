//! What the tests of the app's modules share: keys, results, frames and
//! the services' replies they are built from.
pub(super) use molt_api::fs::Change;
pub(super) use molt_api::model::Usage;
pub(super) use molt_api::planner::{AttemptReport, Baseline, CheckFile, CheckSpec, Requirement};
pub(super) use molt_api::progress::RecalledNote;
pub(super) use ratatui::crossterm::event::KeyEventState;

use super::*;

/// Esc twice, as stopping a running task takes.
pub(super) fn stop(app: &mut App) -> Vec<Action> {
    assert!(app.update(key(KeyCode::Esc)).is_empty(), "one Esc only warns");
    assert_eq!(app.flash.as_deref(), Some("esc again to stop"));
    app.update(key(KeyCode::Esc))
}

pub(super) fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE })
}

pub(super) fn ctrl_key(c: char) -> Msg {
    Msg::Key(KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

pub(super) fn up(app: &mut App) {
    app.update(Msg::SessionUp {
        versions: vec![("planner".into(), "ab12cd34".into())],
        memory_problem: None,
        default_model: None,
    });
}

pub(super) fn type_in(app: &mut App, text: &str) -> Vec<Action> {
    app.update(Msg::Paste(text.into()));
    app.update(key(KeyCode::Enter))
}

pub(super) fn log(app: &App) -> String {
    plain(&app.log).join("\n")
}

pub(super) fn frame_text(app: &mut App) -> String {
    frame_rows(app, 140).concat()
}

pub(super) fn frame_rows(app: &mut App, width: u16) -> Vec<String> {
    frame_rows_sized(app, width, 40)
}

pub(super) fn frame_rows_sized(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::tui::view::draw(frame, app)).unwrap();
    let cells: Vec<String> = terminal.backend().buffer().content.iter().map(|c| c.symbol().to_owned()).collect();
    cells.chunks(usize::from(width)).map(|row| row.concat()).collect()
}

pub(super) fn model_context(app: &App, attempt: Option<u32>, seq: u64) -> molt_api::progress::ModelCall {
    molt_api::progress::ModelCall {
        run: app.task.as_ref().unwrap().trace.to_string(),
        attempt,
        role: None,
        turn: 1,
        call: "fake-stream".into(),
        seq,
    }
}

pub(super) fn queue_app() -> App {
    let mut app = App::new("/work/project", None);
    app.update(Msg::SessionUp { versions: vec![], memory_problem: Some("not configured".into()), default_model: None });
    app.mode = Mode::Plan;
    app.settings.check = Some("true".into());
    app
}

pub(super) fn finished(app: &App, attempt: Option<u32>, call: &str, cost_usd: Option<f64>) -> Msg {
    let context = molt_api::progress::ModelCall { call: call.into(), ..model_context(app, attempt, 9) };
    Msg::Progress(Progress::ModelFinished { context, usage: None, cost_usd, error: None })
}

/// Lets more than the looking window pass since the last key.
pub(super) fn look_away(app: &mut App) {
    for _ in 0..=LOOKING_TICKS {
        app.update(Msg::Tick);
    }
}

pub(super) fn alerts(actions: &[Action]) -> Vec<&str> {
    actions.iter().filter_map(|a| if let Action::Alert { text } = a { Some(text.as_str()) } else { None }).collect()
}

pub(super) fn workspace_files() -> Msg {
    let files = ["Cargo.toml", "src/lexer.rs", "src/lib.rs", "src/parser/lex_table.rs", "tests/lexer.rs"];
    Msg::Files { files: files.map(str::to_owned).to_vec(), complete: true }
}

pub(super) fn reads(actions: &[Action]) -> Vec<(TraceId, Option<u64>)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::ReadAudit { trace, cursor } => Some((trace.clone(), *cursor)),
            _ => None,
        })
        .collect()
}

pub(super) fn attempt_with_tool_call(app: &mut App, trace: &TraceId, detail: &str) {
    let run = trace.to_string();
    app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 0, fork: None }));
    let (tool, detail) = ("shell".into(), detail.into());
    app.update(Msg::Progress(Progress::ToolCall { run, attempt: 0, turn: 1, tool, detail }));
}

pub(super) fn design() -> DesignResponse {
    DesignResponse {
        command: Some("cargo test --test commas".into()),
        files: vec![CheckFile { path: "tests/commas.rs".into(), content: "#[test]\nfn t() {}\n".into() }],
        suite: None,
        rubric: vec![],
        rationale: "the parser has tests".into(),
        baseline: Some(Baseline { passed: false, exit_code: Some(101), tail: String::new() }),
        usage: Usage::default(),
        cost_usd: 0.25,
        uncounted_calls: 0,
        remaining_budget_usd: None,
        error: None,
        cancelled: false,
        workflow: None,
    }
}

pub(super) fn passed(apply: bool, fork: Option<&str>) -> RunResponse {
    RunResponse {
        outcome: Outcome::Passed,
        check: Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: true, suite: None }),
        winner: Some(1),
        summary: "Trailing commas parse.".into(),
        changes: vec![Change { path: "src/parser.rs".into(), kind: ChangeKind::Modified }],
        left_out: None,
        patch: "--- a/src/parser.rs\n+++ b/src/parser.rs\n@@\n-a\n+b\n+c\n".into(),
        patch_truncated: false,
        applied: apply,
        fork: fork.map(str::to_owned),
        review: None,
        attempts: vec![
            AttemptReport {
                index: 0,
                status: AttemptStatus::Cancelled,
                turns: 4,
                check_runs: 1,
                usage: Usage::default(),
                cost_usd: 0.1,
                note: String::new(),
                requirements: 0,
                dispute: None,
                failure: None,
            },
            AttemptReport {
                index: 1,
                status: AttemptStatus::Passed,
                turns: 5,
                check_runs: 1,
                usage: Usage::default(),
                cost_usd: 0.2,
                note: String::new(),
                requirements: 0,
                dispute: None,
                failure: None,
            },
        ],
        usage: Usage::default(),
        cost_usd: 0.3,
        uncounted_calls: 0,
        checkpoint: None,
        requirements: vec![],
        selection: None,
        rubric: vec![],
        judgement: vec![],
        dispute: None,
        verification: None,
        workflow: None,
        conversation: Vec::new(),
        prompts: Vec::new(),
    }
}

pub(super) fn control(actions: &[Action]) -> (TraceId, Control) {
    let [Action::Control { trace, action }] = actions else { panic!("{actions:?}") };
    (trace.clone(), action.clone())
}

pub(super) fn answer(state: RunState, version: u32, waiting: &[&str]) -> Result<ControlResponse, String> {
    Ok(ControlResponse { state, version, waiting: waiting.iter().map(|w| w.to_string()).collect() })
}

pub(super) fn inspected_note() -> Recalled {
    serde_json::from_value(serde_json::json!({
        "note": { "id":"note_config", "rev":7, "kind":"fact", "text":"Build with cargo", "confidence":0.8,
            "created_ms":0, "updated_ms":0, "provenance":{"trace":"run_source", "service":"memory", "version":"v1", "events":["source_event"]}},
        "score":0.9, "reason":{"keywords":"cargo", "relevance":0.9, "confidence":0.8, "recency":0.7},
        "details":{"dependencies":[{"path":"Cargo.toml", "sha256":"old_hash"}], "needs_review":["Cargo.toml"], "temporary":true, "reason":"verified earlier"}
    })).unwrap()
}

pub(super) fn failed(attempts: Vec<AttemptReport>) -> RunResponse {
    RunResponse {
        outcome: Outcome::Failed,
        winner: None,
        summary: "No attempt passed the done-check.".into(),
        changes: vec![],
        patch: String::new(),
        attempts,
        ..passed(false, None)
    }
}

pub(super) fn attempt(index: u32, status: AttemptStatus, note: &str) -> AttemptReport {
    AttemptReport {
        index,
        status,
        turns: 3,
        check_runs: 2,
        usage: Usage::default(),
        cost_usd: 0.1,
        note: note.into(),
        requirements: 0,
        dispute: None,
        failure: None,
    }
}

/// The check's prompt, and why it is up, for a design and the files it would replace.
pub(super) fn check_stop(d: DesignResponse, existing: &[&str]) -> Vec<String> {
    let mut app = App::new("/w", None);
    up(&mut app);
    let actions = type_in(&mut app, "task");
    let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    let existing = existing.iter().map(|e| e.to_string()).collect();
    let actions = app.update(Msg::Designed { trace: trace.clone(), result: Ok(d), existing });
    assert!(actions.is_empty() && matches!(app.prompt, Some(Prompt::Check(_))), "{actions:?}");
    app.task.as_ref().unwrap().stops.clone()
}

/// Why the result's prompt is up, for a result.
pub(super) fn apply_stop(resp: RunResponse) -> Vec<String> {
    let mut app = App::new("/w", None);
    up(&mut app);
    type_in(&mut app, "/check true");
    let actions = type_in(&mut app, "task");
    let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
    let actions = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(resp)) });
    assert!(!actions.iter().any(|a| matches!(a, Action::Apply { .. })), "{actions:?}");
    assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
    app.task.as_ref().unwrap().stops.clone()
}

pub(super) fn with_mods(code: KeyCode, modifiers: KeyModifiers) -> Msg {
    Msg::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, state: KeyEventState::NONE })
}

pub(super) fn conversation() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "<task>first</task>" }] }),
        serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": "Done." }] }),
    ]
}

/// The checkpoint of run `run` of `req`, as `/runs` lists it, having spent `usd` with `open` calls unanswered.
pub(super) fn stopped_run(run: &str, req: &RunRequest, usd: f64, open: u32) -> runs::Stopped {
    let mut saved = molt_planner::checkpoint::Saved::new(run, req.clone());
    saved.spend.cost_usd = usd;
    saved.open_calls = open;
    saved.requirements = vec![Requirement {
        version: 1,
        constraint: Constraint { text: "keep the API".into(), ..Default::default() },
        at_ms: 0,
    }];
    runs::Stopped { run: run.into(), saved: Ok(saved), gone: Vec::new() }
}
