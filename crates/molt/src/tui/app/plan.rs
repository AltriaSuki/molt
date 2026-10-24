//! `/plan TASK`: a task carried out as ordered steps, each a checked run of
//! its own (`planner.plan`). In confirm and plan mode the planner proposes
//! the steps first and the user confirms or edits them, as they do a
//! designed check; in auto mode the steps run as proposed.
use super::*;

/// A plan's steps, waiting on the user.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanSteps {
    /// The steps to run, one goal each.
    pub steps: Vec<String>,
    /// As the planner proposed them.
    pub proposed: Vec<String>,
    /// Why the planner split the task so.
    pub rationale: String,
    /// What proposing them cost.
    pub cost_usd: f64,
}

impl App {
    /// Start `task` as a plan.
    pub(super) fn plan_command(&mut self, task: &str) -> Vec<Action> {
        if self.task.is_some() {
            self.flash = Some("a task is waiting; /plan starts a new task once it is done".into());
            return Vec::new();
        }
        if task.is_empty() {
            self.flash = Some("/plan TASK: a task to carry out in steps, each checked on its own".into());
            return Vec::new();
        }
        if self.fork_operations > 0 || self.git_busy {
            self.flash = Some("wait for the current apply, discard or commit".into());
            return Vec::new();
        }
        if !matches!(self.session, SessionState::Up { .. }) {
            self.flash = Some("the services are not up".into());
            return Vec::new();
        }
        let mut actions = self.start_task(task.to_owned(), self.mode, self.settings.clone());
        let Some(task) = &mut self.task else { return actions };
        // The task's own start, a check's design or a run, is the plan's instead.
        if !matches!(actions.pop(), Some(Action::Design { .. } | Action::Run { .. })) {
            return actions;
        }
        // A plan keeps no checkpoint: one that stops keeps the result of the steps that passed.
        task.req.checkpoint = false;
        let mut req = PlanRequest::new(task.req.clone());
        if task.mode == Mode::Auto {
            task.phase = Phase::Running;
        } else {
            req.propose = true;
            task.phase = Phase::Designing;
        }
        actions.push(Action::Plan { req: Box::new(req), trace: task.trace.clone() });
        actions
    }

    pub(super) fn planned(&mut self, trace: TraceId, result: Result<PlanResponse, String>) -> Vec<Action> {
        let Some(task) = &mut self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        if task.phase != Phase::Designing {
            let resp = match result {
                Ok(resp) => resp,
                Err(e) => return self.ran(trace, Err(e)),
            };
            self.log.extend(step_lines(&resp));
            return match resp.result {
                Some(result) => self.ran(trace, Ok(*result)),
                None => self.ran(trace, Err(resp.error.unwrap_or_else(|| "the plan has no result".into()))),
            };
        }
        self.log.extend(task.designer_preview.finish("ended; final preview event unavailable", true));
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                self.task = None;
                self.error("no plan", &e);
                return Vec::new();
            }
        };
        task.settle();
        task.design_spend = Spend { usd: resp.cost_usd, unknown: resp.uncounted_calls > 0 };
        self.spent_usd += resp.cost_usd;
        self.uncounted_calls = self.uncounted_calls.saturating_add(resp.uncounted_calls);
        if let Some(e) = resp.error {
            self.task = None;
            self.error("no plan", &format!("{e} · ${:.2} known", resp.cost_usd));
            return Vec::new();
        }
        if resp.uncounted_calls > 0 {
            self.task = None;
            self.push(Line::from(span(
                "  planning cost incomplete; the steps cannot start within an unknown remaining budget",
                Tone::Warn,
            )));
            return Vec::new();
        }
        // What planning spent comes out of the plan's budget.
        let left = task.req.budget_usd.map(|b| b - resp.cost_usd);
        if left.is_some_and(|r| !r.is_finite() || r <= 0.0) {
            self.task = None;
            self.push(Line::from(span("  task budget exhausted by planning; no step started", Tone::Warn)));
            return Vec::new();
        }
        task.req.budget_usd = left;
        task.phase = Phase::Confirming;
        self.prompt = Some(Prompt::Plan(PlanSteps {
            proposed: resp.steps.clone(),
            steps: resp.steps,
            rationale: resp.rationale,
            cost_usd: resp.cost_usd,
        }));
        Vec::new()
    }

    pub(super) fn plan_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        match key.code {
            KeyCode::Enter => {
                let Some(Prompt::Plan(plan)) = self.prompt.take() else { return Vec::new() };
                self.run_plan(plan)
            }
            KeyCode::Char('e') => {
                let Some(Prompt::Plan(plan)) = self.prompt.take() else { return Vec::new() };
                self.input = Input::with(&plan.steps.join("\n"));
                self.prompt = Some(Prompt::EditPlan(plan));
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

    /// The steps in the input box, one per line, as the plan's.
    pub(super) fn edited_plan(&mut self, text: &str, mut plan: PlanSteps) {
        let lines: Vec<String> = text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        match molt_api::plan::steps(&lines) {
            Ok(steps) => plan.steps = steps,
            Err(e) => self.flash = Some(e),
        }
        self.prompt = Some(Prompt::Plan(plan));
    }

    /// Carry out the steps the user confirmed.
    fn run_plan(&mut self, plan: PlanSteps) -> Vec<Action> {
        let Some(task) = &mut self.task else { return Vec::new() };
        task.phase = Phase::Running;
        let edited = plan.steps != plan.proposed;
        let mut lines = vec![Line::from(vec![
            span("◆ plan", Tone::Shell),
            span(if edited { "  as edited" } else { "  as proposed" }, Tone::Dim),
        ])];
        for (i, goal) in plan.steps.iter().enumerate() {
            lines.push(Line::from(span(format!("  {}. {}", i + 1, clean_line(goal)), Tone::Plain)));
        }
        self.log.extend(lines);
        let req = PlanRequest {
            steps: plan.steps,
            // On record only when the user changed them.
            proposed: if edited { plan.proposed } else { Vec::new() },
            ..PlanRequest::new(task.req.clone())
        };
        vec![Action::Plan { req: Box::new(req), trace: task.trace.clone() }]
    }
}

/// How each step of a finished plan went, for the log.
fn step_lines(resp: &PlanResponse) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        span("◆ plan", Tone::Shell),
        span(format!("  {} of {} steps ran", resp.ran.len(), resp.steps.len()), Tone::Dim),
    ])];
    for step in &resp.ran {
        let (mark, tone) = match step.run.outcome {
            Outcome::Failed => ("✗", Tone::Bad),
            _ => ("✓", Tone::Ok),
        };
        let check = step.run.check.as_ref().map_or_else(String::new, |c| format!("  $ {}", c.command));
        lines.push(Line::from(vec![
            span(format!("  {mark} {}. ", step.step + 1), tone),
            span(clean_line(&step.goal), Tone::Plain),
            span(format!("  ${:.2}{}", step.run.cost_usd, clean_line(&check)), Tone::Dim),
        ]));
    }
    for (i, goal) in resp.steps.iter().enumerate().skip(resp.ran.len()) {
        lines.push(Line::from(vec![
            span(format!("  · {}. ", i + 1), Tone::Dim),
            span(clean_line(goal), Tone::Dim),
            span("  not started", Tone::Dim),
        ]));
    }
    lines
}

/// The card for the steps waiting on the user.
pub(crate) fn plan_card(plan: &PlanSteps) -> Vec<Line<'static>> {
    let edited = plan.steps != plan.proposed;
    let head = if edited { "  as edited".to_owned() } else { format!("  proposed · ${:.2}", plan.cost_usd) };
    let mut lines = vec![Line::from(vec![span("◆ plan", Tone::Shell), span(head, Tone::Dim)])];
    for (i, goal) in plan.steps.iter().enumerate() {
        lines.push(Line::from(span(format!("  {}. {}", i + 1, clean_line(goal)), Tone::Plain)));
    }
    for l in text::clean(plan.rationale.trim()) {
        lines.push(Line::from(span(format!("  {l}"), Tone::Dim)));
    }
    lines.push(Line::from(span(
        "  each step is a run with its own check; a step that does not pass stops the plan",
        Tone::Dim,
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use molt_api::plan::StepRun;

    fn proposal(steps: &[&str]) -> PlanResponse {
        PlanResponse {
            steps: steps.iter().map(|s| s.to_string()).collect(),
            proposed: Vec::new(),
            edited: false,
            rationale: "one file each".into(),
            ran: Vec::new(),
            remaining: Vec::new(),
            result: None,
            usage: Default::default(),
            cost_usd: 0.05,
            uncounted_calls: 0,
            error: None,
        }
    }

    fn started(app: &mut App, task: &str) -> (Box<PlanRequest>, TraceId) {
        let actions = type_in(app, task);
        let [Action::Plan { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        (req.clone(), trace.clone())
    }

    #[test]
    fn a_plan_is_proposed_first_and_runs_once_the_user_confirms_its_steps() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.mode = Mode::Confirm;
        app.settings.budget_usd = Some(1.0);
        let (req, trace) = started(&mut app, "/plan add the step files");
        assert!(req.propose && req.steps.is_empty() && !req.run.checkpoint);
        assert_eq!(req.run.task, "add the step files");
        app.update(Msg::Planned { trace: trace.clone(), result: Box::new(Ok(proposal(&["one", "two"]))) });
        let Some(Prompt::Plan(plan)) = &app.prompt else { panic!("{:?}", app.prompt.is_some()) };
        assert_eq!(plan.steps, ["one", "two"]);
        let card = plan_card(plan).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(card.contains("1. one") && card.contains("2. two") && card.contains("one file each"), "{card}");

        let actions = app.update(key(KeyCode::Enter));
        let [Action::Plan { req, trace: same }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(same, &trace);
        assert!(!req.propose && req.proposed.is_empty(), "as proposed: nothing edited to record");
        assert_eq!(req.steps, ["one", "two"]);
        assert_eq!(req.run.budget_usd, Some(0.95), "planning's cost comes out of the plan's budget");
        assert_eq!(app.task.as_ref().unwrap().phase, Phase::Running);
    }

    #[test]
    fn the_user_edits_the_steps_one_per_line_and_the_plan_runs_them_with_the_proposal_on_record() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.mode = Mode::Confirm;
        let (_, trace) = started(&mut app, "/plan add the step files");
        app.update(Msg::Planned { trace, result: Box::new(Ok(proposal(&["one", "two"]))) });
        app.update(key(KeyCode::Char('e')));
        assert!(matches!(app.prompt, Some(Prompt::EditPlan(_))));
        assert_eq!(app.input.text, "one\ntwo");
        app.input = Input::with("one\n\n  three, then two  \n");
        app.update(key(KeyCode::Enter));
        let Some(Prompt::Plan(plan)) = &app.prompt else { panic!() };
        assert_eq!(plan.steps, ["one", "three, then two"]);
        assert!(plan_card(plan)[0].to_string().contains("as edited"));
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Plan { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(
            (req.steps.as_slice(), req.proposed.as_slice()),
            (&["one".to_owned(), "three, then two".to_owned()][..], &["one".to_owned(), "two".to_owned()][..])
        );

        // Steps left empty are refused, and the plan stays as it was.
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.mode = Mode::Confirm;
        let (_, trace) = started(&mut app, "/plan add the step files");
        app.update(Msg::Planned { trace, result: Box::new(Ok(proposal(&["one"]))) });
        app.update(key(KeyCode::Char('e')));
        app.input = Input::with("  \n");
        app.update(key(KeyCode::Enter));
        assert!(matches!(&app.prompt, Some(Prompt::Plan(p)) if p.steps == ["one"]));
        assert_eq!(app.flash.as_deref(), Some("a plan has no steps"));
        app.update(key(KeyCode::Esc));
        assert!(app.task.is_none() && app.prompt.is_none());
    }

    #[test]
    fn in_auto_mode_a_plan_runs_its_steps_as_proposed_and_its_result_ends_the_task() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.mode = Mode::Auto;
        let (req, trace) = started(&mut app, "/plan add the step files");
        assert!(!req.propose && req.run.apply);
        let step = |n: u32, fork: &str| Progress::Step {
            run: trace.to_string(),
            step: n,
            steps: 2,
            goal: format!("step {}", n + 1),
            fork: Some(fork.into()),
        };
        app.update(Msg::Progress(step(0, "/d/fork-plan")));
        assert!(log(&app).contains("step 1/2: step 1"), "{}", log(&app));
        assert_eq!(app.task.as_ref().unwrap().plan_fork.as_deref(), Some("/d/fork-plan"));
        app.update(Msg::Progress(step(1, "/d/fork-plan")));

        let mut failed = passed(false, Some("/d/fork-plan"));
        failed.outcome = Outcome::Failed;
        let mut resp = proposal(&["step 1", "step 2", "step 3"]);
        resp.ran = vec![
            StepRun { step: 0, goal: "step 1".into(), run: passed(true, None) },
            StepRun { step: 1, goal: "step 2".into(), run: failed.clone() },
        ];
        resp.remaining = vec!["step 2".into(), "step 3".into()];
        resp.result = Some(Box::new(failed));
        app.update(Msg::Planned { trace, result: Box::new(Ok(resp)) });
        let shown = log(&app);
        assert!(shown.contains("2 of 3 steps ran"), "{shown}");
        assert!(shown.contains("✓ 1. step 1") && shown.contains("✗ 2. step 2"), "{shown}");
        assert!(shown.contains("3. step 3  not started"), "{shown}");
        assert!(app.task.is_none(), "the plan's result ended the task");
    }

    #[test]
    fn stopping_a_plan_keeps_the_result_of_its_steps() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.mode = Mode::Auto;
        let (_, trace) = started(&mut app, "/plan add the step files");
        app.update(Msg::Progress(Progress::Step {
            run: trace.to_string(),
            step: 0,
            steps: 2,
            goal: "step 1".into(),
            fork: Some("/d/fork-plan".into()),
        }));
        let actions = app.interrupt();
        let [Action::Interrupt { attempts, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(attempts.contains(&"/d/fork-plan".to_owned()), "{attempts:?}");
    }
}
