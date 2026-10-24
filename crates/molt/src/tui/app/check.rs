//! The check confirmation: the designed check, shown before the attempts run.
use super::*;

impl App {
    pub(super) fn designed(
        &mut self,
        trace: TraceId,
        result: Result<DesignResponse, String>,
        existing: &[String],
    ) -> Vec<Action> {
        let Some(task) = &mut self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        self.log.extend(task.designer_preview.finish("ended; final preview event unavailable", true));
        match result {
            Ok(d) => {
                task.settle();
                task.design_spend = Spend { usd: d.cost_usd, unknown: d.uncounted_calls > 0 };
                self.spent_usd += d.cost_usd;
                self.uncounted_calls = self.uncounted_calls.saturating_add(d.uncounted_calls);
                if d.cancelled || d.error.is_some() {
                    self.task = None;
                    self.error(
                        "no check",
                        &format!(
                            "{} · ${:.2} known · {} calls uncounted",
                            d.error.as_deref().map_or_else(|| "design cancelled".to_owned(), tui_next_steps),
                            d.cost_usd,
                            d.uncounted_calls
                        ),
                    );
                    return Vec::new();
                }
                if let Some(picked) = d.workflow.clone() {
                    task.req.workflow = Some(picked);
                    task.req.checkpoint &= runs::checkpoints(task.req.workflow.as_deref());
                }
                // The workflow picked for the task designs nothing: it runs as it is.
                if d.workflow.as_deref().is_some_and(|w| w != workflow::VERIFY) {
                    return self.start_after_check(d, true);
                }
                task.stops = check_stops(&d, existing);
                if task.stops.is_empty() && !self.rules.always_ask {
                    return self.start_after_check(d, true);
                }
                task.phase = Phase::Confirming;
                self.prompt = Some(Prompt::Check(d));
            }
            Err(e) => {
                self.task = None;
                self.error("no check", &e);
            }
        }
        Vec::new()
    }

    pub(super) fn check_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Some(Prompt::Check(d)) = &self.prompt else { return Vec::new() };
        match key.code {
            KeyCode::Enter => {
                let Some(Prompt::Check(d)) = self.prompt.take() else { return Vec::new() };
                self.start_after_check(d, true)
            }
            KeyCode::Char('n') => {
                let Some(Prompt::Check(d)) = self.prompt.take() else { return Vec::new() };
                self.start_after_check(d, false)
            }
            KeyCode::Char('e') if d.command.is_some() => {
                self.input = Input::with(d.command.as_deref().unwrap_or_default());
                let Some(Prompt::Check(d)) = self.prompt.take() else { return Vec::new() };
                self.prompt = Some(Prompt::EditCheck(d));
                Vec::new()
            }
            KeyCode::Char('v') if !d.files.is_empty() => {
                let mut lines = Vec::new();
                for f in &d.files {
                    lines.push(Line::from(span(clean_line(&f.path), Tone::Shell)));
                    for (n, l) in text::clean(&f.content).into_iter().enumerate() {
                        lines.push(Line::from(vec![span(format!("{:>4}  ", n + 1), Tone::Dim), span(l, Tone::Plain)]));
                    }
                    lines.push(Line::default());
                }
                self.viewer = Some(Viewer { title: "check files".into(), lines, scroll: 0 });
                Vec::new()
            }
            KeyCode::Esc => {
                self.prompt = None;
                self.task = None;
                self.push(Line::from(span("  cancelled", Tone::Dim)));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Start the attempts with the check the user accepted, or with none.
    fn start_after_check(&mut self, d: DesignResponse, verify: bool) -> Vec<Action> {
        let Some(task) = &mut self.task else { return Vec::new() };
        if d.cancelled || d.error.is_some() || d.uncounted_calls > 0 {
            self.task = None;
            self.push(Line::from(span(
                "  design cost incomplete; attempts cannot start within an unknown remaining budget",
                Tone::Warn,
            )));
            return Vec::new();
        }
        let remaining = d.remaining_budget_usd.or_else(|| task.req.budget_usd.map(|b| b - d.cost_usd));
        if remaining.is_some_and(|r| !r.is_finite() || r <= 0.0) {
            self.task = None;
            self.push(Line::from(span("  task budget exhausted by the check design; no attempts started", Tone::Warn)));
            return Vec::new();
        }
        task.req.budget_usd = remaining;
        // The check is settled: what designing could spend is spent, and it must not exceed what is left.
        task.req.design_budget_usd = None;
        task.phase = Phase::Running;
        task.check_shown = true;
        let mut lines = Vec::new();
        match (&d.command, verify) {
            (Some(command), true) => {
                lines.push(check_head(true));
                lines.push(Line::from(vec![span("  $ ", Tone::Dim), span(clean_line(command), Tone::Plain)]));
                for f in &d.files {
                    lines.push(Line::from(span(format!("  + {}", clean_line(&f.path)), Tone::Dim)));
                }
                task.req.check = Some(command.clone());
                task.req.check_files = d.files;
                task.req.suite = d.suite;
            }
            (None, true) if !d.rubric.is_empty() => {
                lines.push(check_head(true));
                lines.extend(rubric_lines(&d.rubric.iter().map(|c| c.text.clone()).collect::<Vec<_>>()));
                task.req.rubric = d.rubric;
            }
            // The line saying which workflow was picked says it designs no check.
            _ if d.workflow.as_deref().is_some_and(|w| w != workflow::VERIFY) => {}
            _ => {
                lines.push(Line::from(vec![
                    span("◆ check", Tone::Shell),
                    span("  none; the result is unverified", Tone::Dim),
                ]));
                task.req.verify = false;
            }
        }
        self.log.extend(lines);
        vec![Action::Run { req: task.req.clone(), trace: task.trace.clone() }]
    }
}

/// A rubric's criteria, one line each, for the log.
pub(super) fn rubric_lines(rubric: &[String]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(span("  judged by a model against these criteria:", Tone::Warn))];
    for (i, c) in rubric.iter().enumerate() {
        lines.push(Line::from(span(format!("  {}. {}", i + 1, clean_line(c)), Tone::Plain)));
    }
    lines
}

/// Why a designed check needs the user's look before the attempts start.
/// Why confirm mode stopped at a check an attempt disputed.
pub(crate) const DISPUTED: &str = "check disputed";

/// The check card for a run that ended on a dispute, with the dispute as its
/// rationale. `None` without a dispute, or when the check's files are not
/// known here (the planner designed them within the run), so it cannot be
/// run again as it was.
pub(super) fn disputed_check(req: &RunRequest, resp: &RunResponse) -> Option<DesignResponse> {
    let dispute = resp.dispute.as_ref().filter(|_| resp.outcome == Outcome::Failed)?;
    let (command, files, suite) = match (&req.check, &resp.check) {
        (Some(command), _) => (command.clone(), req.check_files.clone(), req.suite.clone()),
        (None, Some(spec)) if spec.files.is_empty() => (spec.command.clone(), Vec::new(), spec.suite.clone()),
        _ => return None,
    };
    let rationale = format!(
        "Attempt {} disputed this check: it asserts {}, but the task says \"{}\". To reproduce: {}. Edit the \
         check, or run it again as it is.",
        dispute.attempt + 1,
        dispute.assertion.trim(),
        dispute.task_quote.trim(),
        dispute.reproduce.trim()
    );
    Some(DesignResponse {
        command: Some(command),
        files,
        suite,
        rubric: Vec::new(),
        rationale,
        baseline: None,
        usage: Default::default(),
        cost_usd: 0.0,
        uncounted_calls: 0,
        remaining_budget_usd: req.budget_usd,
        error: None,
        cancelled: false,
        workflow: None,
    })
}

/// A planner's message with `molt do`'s next steps after the design budget
/// ran out put in this interface's terms.
pub(super) fn tui_next_steps(message: &str) -> String {
    message.replace(
        molt_api::planner::DESIGN_BUDGET_NEXT,
        "Give the check yourself with /check CMD, or allow more for designing with /design-budget USD (at most \
         the task's /budget), then send the task again.",
    )
}

fn check_stops(d: &DesignResponse, existing: &[String]) -> Vec<String> {
    if d.command.is_none() && !d.rubric.is_empty() {
        return vec!["criteria judged by a model".into()];
    }
    if d.command.is_none() {
        return vec!["no automated check".into()];
    }
    let mut stops = Vec::new();
    if d.uncounted_calls > 0 {
        stops.push("design cost incomplete".into());
    }
    match &d.baseline {
        Some(b) if b.passed => stops.push("baseline passes".into()),
        Some(_) => {}
        None => stops.push("baseline not run".into()),
    }
    match existing.len() {
        0 => {}
        1 => stops.push(format!("replaces {}", clean_line(&existing[0]))),
        n => stops.push(format!("replaces {n} files")),
    }
    stops
}

pub(super) fn check_head(designed: bool) -> Line<'static> {
    let by = if designed { "designed" } else { "yours" };
    Line::from(vec![span("◆ check", Tone::Shell), span(format!("  {by}"), Tone::Dim)])
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn the_check_designs_calls_count_until_its_reply_settles_them() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        let actions = type_in(&mut app, "implement it");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(finished(&app, None, "d", Some(0.04)));
        assert_eq!(app.task.as_ref().unwrap().spend().label(), "$0.04");
        app.update(Msg::Designed { trace, result: Ok(design()), existing: vec![] });
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.spend().label(), format!("${:.2}", design().cost_usd));
    }

    #[test]
    fn confirm_mode_shows_the_check_then_the_result_before_anything_changes() {
        let mut app = App::new("/home/u/parser", Some("/home/u"));
        assert_eq!(app.workspace, "~/parser");
        assert_eq!(app.mode, Mode::Confirm);
        up(&mut app);
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "accept trailing commas");
        let [Action::Design { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(!req.apply && req.check.is_none());
        let trace = trace.clone();

        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        assert!(matches!(app.prompt, Some(Prompt::Check(_))));
        // Typing goes nowhere while the check waits on the user.
        assert!(app.update(key(KeyCode::Char('x'))).is_empty());
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.check.as_deref(), Some("cargo test --test commas"));
        assert_eq!(req.check_files.len(), 1);
        assert!(!req.apply);
        assert!(
            log(&app).contains("◆ check  designed\n  $ cargo test --test commas\n  + tests/commas.rs"),
            "{}",
            log(&app)
        );

        let run = trace.to_string();
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 1, fork: None }));
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 0, fork: None }));
        app.update(Msg::Progress(Progress::ToolCall {
            run: run.clone(),
            attempt: 1,
            turn: 3,
            tool: "edit_file".into(),
            detail: "edit src/parser.rs".into(),
        }));
        app.update(Msg::Progress(Progress::CheckRan {
            run: run.clone(),
            attempt: 1,
            passed: true,
            exit_code: Some(0),
            tail: String::new(),
            judged: false,
        }));
        // Another run's events are not this task's.
        app.update(Msg::Progress(Progress::AttemptStarted { run: "other".into(), attempt: 5, fork: None }));
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.lanes.iter().map(|l| l.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(plain(&task.lanes[1].lines), [" 3 edit src/parser.rs", "   check passed"]);

        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-1"))) });
        assert_eq!(actions, [Action::Learn { trace: trace.clone(), model: None }]);
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        assert!(
            log(&app).contains("✓ passed  1 file  +2 −1\n  Trailing commas parse.\n  modified src/parser.rs"),
            "{}",
            log(&app)
        );
        assert!(log(&app).contains("  ✓ attempt 2 passed · 5 turns · 1 check · $0.20"), "{}", log(&app));

        app.update(key(KeyCode::Char('d')));
        assert_eq!(app.viewer.as_ref().unwrap().title, "diff");
        app.update(key(KeyCode::Esc));
        assert!(app.viewer.is_none());
        assert_eq!(app.update(key(KeyCode::Char('y'))), [Action::Apply { fork: "/d/work/fork-1".into() }]);
        app.update(Msg::Applied(Ok(())));
        assert!(app.task.is_none());
        assert!((app.spent_usd - 0.55).abs() < 1e-9, "{}", app.spent_usd);
    }

    #[test]
    fn the_check_can_be_edited_dropped_or_refused() {
        let mut app = App::new("/w", None);
        up(&mut app);
        // These are about the prompts themselves.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        let trace = trace.clone();
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        app.update(key(KeyCode::Char('e')));
        assert_eq!(app.input.text, "cargo test --test commas");
        app.update(ctrl_key('u'));
        type_in(&mut app, "cargo test");
        let Some(Prompt::Check(d)) = &app.prompt else { panic!("back to the check") };
        assert_eq!(d.command.as_deref(), Some("cargo test"));
        assert_eq!(d.baseline, None, "the baseline was of the old command");
        let actions = app.update(key(KeyCode::Char('n')));
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!() };
        assert!(!req.verify && req.check.is_none() && req.check_files.is_empty());
        let run = Some(trace.to_string());

        // Esc twice while it runs stops everything.
        let actions = stop(&mut app);
        assert_eq!(actions, [Action::Interrupt { keep: vec![], attempts: vec![], check_files: vec![], drop_run: run }]);
        assert!(app.task.is_none());
        assert_eq!(app.session, SessionState::Starting);
        assert_eq!(app.update(Msg::Stopped { kept: vec![], notes: vec![] }), [Action::StartSession]);

        // A design that fails ends the task.
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        app.update(Msg::Designed {
            trace: trace.clone(),
            result: Err("planner.design failed".into()),
            existing: vec![],
        });
        assert!(app.task.is_none());
        assert!(log(&app).ends_with("✗ no check: planner.design failed"), "{}", log(&app));
    }

    #[test]
    fn incomplete_design_cost_is_shown_and_cannot_be_spent_as_free() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "task");
        let trace = app.task.as_ref().unwrap().trace.clone();
        let d = DesignResponse { uncounted_calls: 2, ..design() };
        app.update(Msg::Designed { trace, result: Ok(d), existing: vec![] });
        let frame = frame_text(&mut app);
        assert!(
            frame.contains("cost incomplete")
                && frame.contains("2 calls uncounted")
                && frame.contains("remaining budget unknown")
        );
        assert_eq!((app.spent_usd, app.uncounted_calls), (0.25, 2));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none());
        assert!(log(&app).contains("attempts cannot start"));
    }

    #[test]
    fn a_failed_cancelled_design_records_settlement_without_offer_to_run_unverified() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "task");
        let trace = app.task.as_ref().unwrap().trace.clone();
        let d = DesignResponse {
            command: None,
            files: vec![],
            cancelled: true,
            error: Some("design stopped".into()),
            uncounted_calls: 1,
            ..design()
        };
        assert!(app.update(Msg::Designed { trace: trace.clone(), result: Ok(d), existing: vec![] }).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none());
        assert_eq!((app.spent_usd, app.uncounted_calls), (0.25, 1));
        assert!(log(&app).contains("$0.25 known · 1 calls uncounted"));
        assert!(frame_text(&mut app).contains("1 calls uncounted"));
        app.update(Msg::Designed { trace, result: Ok(design()), existing: vec![] });
        assert_eq!(app.spent_usd, 0.25, "late replies are not counted twice");
    }

    #[test]
    fn a_design_out_of_its_budget_stops_confirm_mode_and_says_what_to_do_here() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/budget 1");
        type_in(&mut app, "/design-budget 0.4");
        let actions = type_in(&mut app, "write the docs");
        assert!(matches!(&actions[0], Action::Design { req, .. }
            if req.budget_usd == Some(1.0) && req.design_budget_usd == Some(0.4)));
        let trace = app.task.as_ref().unwrap().trace.clone();
        let error = format!(
            "could not design a done-check: the design budget ran out before the designer submitted a check: \
             designing spent $0.40 of the $0.40 it may use, and $0.60 of the run's $1.00 budget is left. {}",
            molt_api::planner::DESIGN_BUDGET_NEXT
        );
        let d = DesignResponse {
            command: None,
            files: vec![],
            cost_usd: 0.4,
            remaining_budget_usd: Some(0.6),
            error: Some(error),
            ..design()
        };
        assert!(app.update(Msg::Designed { trace, result: Ok(d), existing: vec![] }).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none(), "no attempt starts");
        let shown = log(&app);
        assert!(shown.contains("the design budget ran out"), "{shown}");
        assert!(shown.contains("$0.60 of the run's $1.00 budget is left"), "{shown}");
        assert!(shown.contains("/check CMD") && shown.contains("/design-budget USD"), "{shown}");
        assert!(!shown.contains("--check"), "{shown}");
    }

    #[test]
    fn a_designed_check_starts_the_attempts_without_the_design_budget() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/budget 1");
        type_in(&mut app, "/design-budget 1");
        type_in(&mut app, "task");
        let trace = app.task.as_ref().unwrap().trace.clone();
        let d = DesignResponse { remaining_budget_usd: Some(0.75), ..design() };
        let actions = app.update(Msg::Designed { trace, result: Ok(d), existing: vec![] });
        let actions = if actions.is_empty() { app.update(key(KeyCode::Enter)) } else { actions };
        // Designing is over: what is left is the attempts', and no design budget can exceed it.
        assert!(
            matches!(&actions[0], Action::Run { req, .. }
            if req.budget_usd == Some(0.75) && req.design_budget_usd.is_none()),
            "{actions:?}"
        );
    }

    #[test]
    fn a_disputed_check_stops_at_the_check_card_to_be_edited_and_run_again() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "accept trailing commas");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        let actions = app.update(key(KeyCode::Enter));
        assert!(matches!(actions.as_slice(), [Action::Run { .. }]), "{actions:?}");

        let dispute = molt_api::planner::CheckDispute {
            attempt: 0,
            assertion: "tests/commas.rs expects column 20".into(),
            task_quote: "columns count Unicode characters".into(),
            reproduce: "cargo test --test commas".into(),
        };
        let mut disputed = attempt(0, AttemptStatus::Cancelled, "disputed the check");
        disputed.dispute = Some(dispute.clone());
        let resp = RunResponse {
            summary: "The done-check was disputed: attempt 0 found that it contradicts the task.".into(),
            dispute: Some(Box::new(dispute)),
            ..failed(vec![disputed, attempt(1, AttemptStatus::Failed, "x")])
        };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(resp) });
        assert!(actions.iter().all(|a| matches!(a, Action::Learn { .. })), "nothing runs: {actions:?}");
        assert!(log(&app).contains("  – attempt 1 disputed the check · 3 turns"), "{}", log(&app));
        // It stops at the check card, with the dispute, instead of offering a retry.
        let Some(Prompt::Check(d)) = &app.prompt else { panic!("not at the check card: {:?}", app.prompt.is_some()) };
        assert_eq!(d.command.as_deref(), Some("cargo test --test commas"));
        assert_eq!(d.files.len(), 1, "the check's files come along");
        assert!(d.rationale.starts_with("Attempt 1 disputed this check: it asserts tests/commas.rs expects column 20, but the task says \"columns count Unicode characters\". To reproduce: cargo test --test commas."), "{}", d.rationale);
        let task = app.task.as_ref().unwrap();
        assert_eq!((&task.phase, task.stops.as_slice()), (&Phase::Confirming, [DISPUTED.to_owned()].as_slice()));
        assert_ne!(task.trace, trace, "going on is a new run");
        let screen = frame_text(&mut app);
        assert!(screen.contains("◆ check  disputed by an attempt"), "{screen}");
        assert!(screen.contains("Attempt 1 disputed this check"), "{screen}");

        // Edit the check, then go on with it.
        app.update(key(KeyCode::Char('e')));
        assert!(matches!(app.prompt, Some(Prompt::EditCheck(_))));
        app.input = Input::with("cargo test --test commas -- --skip column");
        app.update(key(KeyCode::Enter));
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Run { req, trace: next }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.check.as_deref(), Some("cargo test --test commas -- --skip column"));
        assert_eq!(req.check_files.len(), 1);
        assert_eq!(next, &app.task.as_ref().unwrap().trace);
        assert_eq!(app.task.as_ref().unwrap().phase, Phase::Running);
    }

    #[test]
    fn confirm_mode_goes_through_a_clean_task_without_a_key() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "accept trailing commas");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        let actions = app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("no stop for the check: {actions:?}") };
        assert_eq!(req.check.as_deref(), Some("cargo test --test commas"));
        assert!(app.prompt.is_none());
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-1"))) });
        assert_eq!(actions, [Action::Apply { fork: "/d/work/fork-1".into() }, Action::Learn { trace, model: None }]);
        assert!(app.prompt.is_none());
        app.update(Msg::Applied(Ok(())));
        let log = log(&app);
        assert!(log.contains("  $ cargo test --test commas") && log.ends_with("  applied"), "{log}");
        assert!(app.task.is_none());
    }

    #[test]
    fn confirm_mode_stops_for_a_check_that_needs_a_look() {
        assert_eq!(
            check_stop(DesignResponse { command: None, files: vec![], ..design() }, &[]),
            ["no automated check"]
        );
        let rubric = vec![molt_api::planner::Criterion { text: "Cites sources.".into(), command: None }];
        assert_eq!(
            check_stop(DesignResponse { command: None, files: vec![], rubric, ..design() }, &[]),
            ["criteria judged by a model"]
        );
        let passing = Baseline { passed: true, exit_code: Some(0), tail: String::new() };
        assert_eq!(check_stop(DesignResponse { baseline: Some(passing), ..design() }, &[]), ["baseline passes"]);
        assert_eq!(check_stop(DesignResponse { baseline: None, ..design() }, &[]), ["baseline not run"]);
        assert_eq!(check_stop(design(), &["tests/commas.rs"]), ["replaces tests/commas.rs"]);
    }
}
