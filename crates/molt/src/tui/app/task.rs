//! Starting a task, steering it while it runs, and stopping it.
use super::*;

impl App {
    pub(super) fn interrupt(&mut self) -> Vec<Action> {
        if self.task.is_none() && self.learning.is_empty() {
            return Vec::new();
        }
        self.queue_paused |= !self.queue.is_empty();
        let keep = self.keep();
        if let Some(entry) = self.pending_result.take() {
            self.push(Line::from(span(format!("  kept in {}", clean_line(&entry.fork)), Tone::Dim)));
            self.keep_entry(entry);
        }
        self.prompt = None;
        let stopped = self.task.take();
        let spent = stopped.as_ref().and_then(|task| self.settle_stopped(task));
        if !self.learning.is_empty() {
            // Learning's reply would have said what it cost; the services stop before it comes.
            self.uncounted_stops = self.uncounted_stops.saturating_add(self.learning.len() as u32);
        }
        let mut attempts = Vec::new();
        let mut check_files = Vec::new();
        let mut drop_run = None;
        if let Some(mut task) = stopped {
            self.interrupted_task = Some(task.req.task.clone());
            if matches!(task.phase, Phase::Running) {
                // Stopped by the user, the run is not to go on: its checkpoint goes, its work is kept as below.
                if task.req.checkpoint {
                    self.settled_runs.remove(task.trace.as_str());
                    drop_run = Some(task.trace.to_string());
                }
                attempts = task.lanes.iter().filter_map(|l| l.fork.clone()).collect();
                // A plan's steps that passed are in its own fork.
                attempts.extend(task.plan_fork.clone());
                check_files = task.req.check_files.iter().map(|f| f.path.clone()).collect();
                self.last_lanes = std::mem::take(&mut task.lanes);
                // Starting again needs the check in full; a designed one is only known once accepted.
                if task.req.check.is_some() {
                    let designed = task.settings.check.as_deref() != task.req.check.as_deref();
                    self.prompt = Some(Prompt::Retry {
                        req: Box::new(task.req),
                        designed,
                        stopped: true,
                        attempts: Vec::new(),
                        mode: task.mode,
                        settings: Box::new(task.settings),
                    });
                }
            }
        }
        self.zoom = None;
        self.learning.clear();
        self.push(Line::from(span("■ stopped; restarting the services", Tone::Warn)));
        if let Some(spent) = spent {
            self.push(Line::from(span(format!("  {spent}"), Tone::Dim)));
        }
        self.session = SessionState::Starting;
        vec![Action::Interrupt { keep, attempts, check_files, drop_run }]
    }

    /// Count what a task stopped before its reply has spent so far, and say
    /// what that was; `None` when a reply has settled it already.
    fn settle_stopped(&mut self, task: &Task) -> Option<String> {
        if !matches!(task.phase, Phase::Designing | Phase::Running) {
            return None;
        }
        if !task.req.stream {
            // Without previews no call's cost arrives before the reply.
            self.uncounted_stops = self.uncounted_stops.saturating_add(1);
            return Some("what it spent is unknown: previews were off, and its reply will not come".into());
        }
        let uncounted = task.unsettled_unknown.saturating_add(task.open_calls.len() as u32);
        // A resumed run's spend before the resume is no reply's to settle now either.
        self.spent_usd += task.unsettled.usd + task.carried.0;
        self.uncounted_calls = self.uncounted_calls.saturating_add(uncounted).saturating_add(task.carried.1);
        Some(match uncounted {
            0 => format!("${:.2} spent before it stopped", task.unsettled.usd),
            1 => format!("${:.2} known spent before it stopped · 1 call uncounted", task.unsettled.usd),
            n => format!("${:.2} known spent before it stopped · {n} calls uncounted", task.unsettled.usd),
        })
    }

    /// Start `line`: as an answer or a constraint while a task runs, and
    /// otherwise as the next task of the conversation, going on from the
    /// last one (#217).
    pub(super) fn submit_task(&mut self, line: String) -> Vec<Action> {
        self.submit_from(line, true)
    }

    /// [`Self::submit_task`]; `goes_on` false starts the task from the
    /// workspace, told of the earlier tasks only.
    fn submit_from(&mut self, line: String, goes_on: bool) -> Vec<Action> {
        if self.fork_operations > 0 || self.git_busy {
            self.flash = Some("wait for the current apply, discard or commit".into());
            self.input = Input::with(&line);
            return Vec::new();
        }
        if let Some((question, options)) =
            self.task.as_mut().filter(|t| t.phase == Phase::Running && !t.asked.is_empty()).map(|t| t.asked.remove(0))
        {
            // Typed while an attempt waits on its question: the answer, an option by its number or in words.
            let picked = line.parse::<usize>().ok().and_then(|n| options.get(n.checked_sub(1)?)).cloned();
            let text = picked.unwrap_or(line);
            self.echo_constraint(&text);
            return self.steer(Control::Constrain(Constraint { text, answers: Some(question), ..Default::default() }));
        }
        if self.task.as_ref().is_some_and(|t| t.phase == Phase::Running) {
            // Typed while the attempts run: more of what the task asks.
            self.echo_constraint(&line);
            return self.steer(Control::Constrain(Constraint { text: line, ..Default::default() }));
        }
        if self.task.is_some() {
            self.flash = Some("a task is waiting; Esc stops it".into());
            self.input = Input::with(&line);
            return Vec::new();
        }
        if !matches!(self.session, SessionState::Up { .. }) {
            self.flash = Some("the services are not up".into());
            self.input = Input::with(&line);
            return Vec::new();
        }
        let continues = self.continuation.clone().filter(|_| goes_on);
        self.start_task_with(line, self.mode, self.settings.clone(), continues)
    }

    pub(super) fn start_task(&mut self, line: String, mode: Mode, settings: Settings) -> Vec<Action> {
        self.start_task_with(line, mode, settings, None)
    }

    /// Start `line` as a new task; with `continues`, going on from the last one.
    fn start_task_with(
        &mut self,
        line: String,
        mode: Mode,
        settings: Settings,
        continues: Option<Continuation>,
    ) -> Vec<Action> {
        let files = match self.referenced(&line) {
            Ok(files) => files,
            Err(e) => {
                self.flash = Some(e);
                self.input = Input::with(&line);
                return Vec::new();
            }
        };
        let trace = TraceId::random();
        let mut req = RunRequest::new(line.clone(), self.workspace_path.clone());
        req.files = files;
        // Someone is here to answer the attempts' questions.
        req.interactive = true;
        if let Some(n) = settings.attempts {
            req.attempts = n;
        }
        req.model = settings.model.clone();
        req.effort = settings.effort;
        req.budget_usd = settings.budget_usd;
        req.design_budget_usd = settings.design_budget_usd;
        req.apply = mode == Mode::Auto;
        req.earlier = self.earlier.clone();
        req.stream = settings.stream;
        // An interrupt looks at what the attempts changed itself: the planner must not drop their forks first.
        req.keep_cancelled_forks = true;
        req.workflow = Some(settings.workflow.clone().unwrap_or_else(|| workflow::AUTO.to_owned()));
        req.continues = continues;
        req.session = Some(self.run_session.clone());
        // So that a task the interface stopped in (a quit, a crash) can go on: `/runs resume`.
        req.checkpoint = runs::checkpoints(req.workflow.as_deref());
        // Only a workflow that designs a check stops for it; `auto` may pick one that does.
        let designs = workflow::builtin(req.workflow.as_deref()).is_none_or(|w| w.stages.design.is_some());

        self.gap();
        self.echo(&line);
        if let Some(c) = &req.continues {
            let what = if c.fork.is_some() { "the last task and its kept result" } else { "the last task" };
            self.push(Line::from(span(format!("  goes on from {what} · /new starts a new conversation"), Tone::Dim)));
        }
        let mut task = Task::new(trace.clone(), req, Phase::Running, mode, settings.clone());
        self.last_run = Some(trace.to_string());
        let action = if let Some(check) = &settings.check {
            task.req.check = Some(check.clone());
            Action::Run { req: task.req.clone(), trace }
        } else if mode == Mode::Confirm && designs {
            task.phase = Phase::Designing;
            Action::Design { req: task.req.clone(), trace }
        } else {
            Action::Run { req: task.req.clone(), trace }
        };
        let trace = task.trace.clone();
        self.task = Some(task);
        self.zoom = None;
        self.starting(trace, action)
    }

    /// The actions that start a task with `action`: looking at the work tree
    /// first, when results are committed.
    pub(super) fn starting(&self, trace: TraceId, action: Action) -> Vec<Action> {
        if self.task.as_ref().is_some_and(|task| task.settings.commit) {
            vec![Action::PrepareCommit { trace }, action]
        } else {
            vec![action]
        }
    }

    /// Run `task` through the workflow `name`; `/verify` alone runs the last
    /// task again through the full workflow.
    pub(super) fn with_workflow(&mut self, name: &str, task: &str) -> Vec<Action> {
        if self.task.is_some() {
            self.flash = Some(format!("a task is waiting; /{name} starts a new task once it is done"));
            return Vec::new();
        }
        // `/verify` alone runs the last task again from the workspace, rather than on top of its result.
        let (task, goes_on) = match (task.is_empty(), self.earlier.last()) {
            (false, _) => (task.to_owned(), true),
            (true, Some(last)) if name == workflow::VERIFY => (last.task.clone(), false),
            (true, _) => {
                self.flash = Some(format!("/{name} TASK: the task to run with workflow {name}"));
                return Vec::new();
            }
        };
        let saved = self.settings.workflow.replace(name.to_owned());
        let actions = self.submit_from(task, goes_on);
        self.settings.workflow = saved;
        actions
    }

    /// Start `task` going on from the last finished one: what plain input
    /// does, kept for those who name it, and refused when there is nothing
    /// to go on from.
    pub(super) fn continue_task(&mut self, task: &str) -> Vec<Action> {
        if self.task.is_some() {
            self.flash = Some("a task is waiting; /continue goes on from it once it is done".into());
            return Vec::new();
        }
        if task.is_empty() {
            self.flash = Some("/continue TASK: what to do next, going on from the last task".into());
            return Vec::new();
        }
        let Some(continues) = self.continuation.clone() else {
            self.flash = Some("nothing to go on from: /continue follows a task finished in this conversation".into());
            return Vec::new();
        };
        if self.fork_operations > 0 || self.git_busy {
            self.flash = Some("wait for the current apply, discard or commit".into());
            return Vec::new();
        }
        if !matches!(self.session, SessionState::Up { .. }) {
            self.flash = Some("the services are not up".into());
            return Vec::new();
        }
        self.start_task_with(task.to_owned(), self.mode, self.settings.clone(), Some(continues))
    }

    /// What the user added to a running task, in the log.
    pub(super) fn echo_constraint(&mut self, text: &str) {
        for (i, l) in text::clean(text).into_iter().enumerate() {
            let mark = if i == 0 { span("+ ", Tone::Shell) } else { span("  ", Tone::Plain) };
            self.push(Line::from(vec![mark, span(l, Tone::Plain)]));
        }
    }

    /// Send `action` to the running task.
    pub(super) fn steer(&mut self, action: Control) -> Vec<Action> {
        let Some(task) = self.task.as_ref().filter(|t| t.phase == Phase::Running) else {
            self.flash = Some("no task is running".into());
            return Vec::new();
        };
        vec![Action::Control { trace: task.trace.clone(), action }]
    }

    pub(super) fn controlled(&mut self, trace: &TraceId, action: &Control, result: Result<ControlResponse, String>) {
        let Some(task) = self.task.as_mut().filter(|t| &t.trace == trace) else { return };
        match result {
            Ok(r) => {
                task.state = r.state;
                task.waiting = r.waiting;
                if matches!(action, Control::Constrain(_)) {
                    task.version = task.version.max(r.version);
                    let line = format!("  requirements v{}; the attempts take it up at their next step", r.version);
                    self.push(Line::from(span(line, Tone::Dim)));
                }
            }
            Err(e) => {
                let what = match action {
                    Control::Constrain(_) => "not added",
                    Control::Pause => "not paused",
                    Control::Resume => "not resumed",
                };
                self.error(what, &e);
            }
        }
    }

    /// What a finished run leaves for the next tasks of the conversation.
    pub(super) fn remember(&mut self, task: &str, resp: &RunResponse) {
        let summary = scrub(&tui_next_steps(&resp.summary), &self.secrets);
        let (summary, _) = bounded(summary.trim(), EARLIER_SUMMARY_BYTES);
        self.earlier.push(Earlier {
            task: task.to_owned(),
            outcome: resp.outcome,
            summary: summary.to_owned(),
            changes: resp.changes.iter().map(|c| c.path.clone()).collect(),
            applied: resp.applied,
        });
        let over = self.earlier.len().saturating_sub(EARLIER_TASKS);
        self.earlier.drain(..over);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn a_single_esc_does_not_stop_a_running_task() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "implement it");
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        assert!(app.task.is_some());
        assert_eq!(app.flash.as_deref(), Some("esc again to stop"));
        // Another key in between, or a second press too late, starts over.
        app.update(key(KeyCode::Char('x')));
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        for _ in 0..=QUIT_TICKS {
            app.update(Msg::Tick);
        }
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        assert!(app.task.is_some());
        assert!(matches!(app.update(key(KeyCode::Esc)).as_slice(), [Action::Interrupt { .. }]));
        assert!(app.task.is_none());
    }

    #[test]
    fn an_interrupt_counts_what_the_stopped_run_spent_and_keeps_unknown_calls_uncounted() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.keep_cancelled_forks, "the interface decides which stopped forks hold work");
        app.update(finished(&app, Some(0), "a", Some(0.1)));
        app.update(finished(&app, Some(1), "b", Some(0.2)));
        app.update(finished(&app, Some(1), "c", None));
        let open = molt_api::progress::ModelCall { call: "d".into(), ..model_context(&app, Some(0), 0) };
        app.update(Msg::Progress(Progress::ModelStarted { context: open }));
        let late = finished(&app, Some(0), "d", Some(0.5));
        assert_eq!(app.spent_usd, 0.0);

        assert!(matches!(stop(&mut app).as_slice(), [Action::Interrupt { .. }]));
        assert!((app.spent_usd - 0.3).abs() < 1e-9, "{}", app.spent_usd);
        assert_eq!(app.uncounted_calls, 2, "a call without a price and one cut off are not free");
        assert!(log(&app).contains("$0.30 known spent before it stopped · 2 calls uncounted"), "{}", log(&app));
        assert!(frame_text(&mut app).contains("$0.30 known · 2 calls uncounted"));
        app.update(late);
        assert!((app.spent_usd - 0.3).abs() < 1e-9, "the stopped run is not counted twice");
    }

    #[test]
    fn a_settled_design_is_not_counted_again_when_its_run_is_stopped() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(finished(&app, None, "design", Some(0.25)));
        app.update(Msg::Designed { trace, result: Ok(design()), existing: vec![] });
        app.update(key(KeyCode::Enter));
        app.update(finished(&app, Some(0), "a", Some(0.1)));
        stop(&mut app);
        assert!((app.spent_usd - 0.35).abs() < 1e-9, "{}", app.spent_usd);
        assert_eq!(app.uncounted_calls, 0);
        assert!(log(&app).contains("$0.10 spent before it stopped"), "{}", log(&app));
    }

    #[test]
    fn a_design_stopped_before_its_reply_counts_its_finished_calls() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "task");
        assert_eq!(app.task.as_ref().unwrap().phase, Phase::Designing);
        app.update(finished(&app, None, "design", Some(0.05)));
        assert!(matches!(stop(&mut app).as_slice(), [Action::Interrupt { .. }]));
        assert!((app.spent_usd - 0.05).abs() < 1e-9, "{}", app.spent_usd);
    }

    #[test]
    fn a_stopped_run_without_previews_leaves_the_session_total_marked_incomplete() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/stream off");
        type_in(&mut app, "/check true");
        type_in(&mut app, "implement it");
        stop(&mut app);
        assert_eq!((app.spent_usd, app.uncounted_calls, app.uncounted_stops), (0.0, 0, 1));
        assert!(log(&app).contains("what it spent is unknown"), "{}", log(&app));
        assert!(frame_text(&mut app).contains("$0.00 known · 1 stopped task uncounted"));
    }

    #[test]
    fn an_attempts_question_is_shown_and_what_is_typed_next_answers_it() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.settings.check = Some("make test".into());
        let actions = type_in(&mut app, "indent the file");
        let [Action::Run { req, trace: run }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.interactive, "someone is here to answer");
        let run = run.clone();
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.to_string(), attempt: 0, fork: None }));
        app.update(Msg::Progress(Progress::Asked {
            run: run.to_string(),
            attempt: 0,
            question: 1,
            text: "Tabs or spaces?".into(),
            options: vec!["tabs".into(), "four spaces".into()],
        }));
        let shown = log(&app);
        assert!(shown.contains("? attempt 1 asks  Tabs or spaces?"), "{shown}");
        assert!(shown.contains("  2. four spaces") && shown.contains("type an option's number"), "{shown}");

        // An option by its number.
        let (trace, action) = control(&type_in(&mut app, "2"));
        assert_eq!(trace, run);
        let answer = Constraint { text: "four spaces".into(), answers: Some(1), ..Default::default() };
        assert_eq!(action, Control::Constrain(answer));
        assert!(log(&app).contains("+ four spaces"), "{}", log(&app));

        // Answered, what is typed next steers the run as before.
        let (_, action) = control(&type_in(&mut app, "keep the header"));
        assert_eq!(action, Control::Constrain(Constraint { text: "keep the header".into(), ..Default::default() }));

        // A question answered elsewhere is no longer waiting.
        let asked = |question| Progress::Asked {
            run: run.to_string(),
            attempt: 1,
            question,
            text: "Which file?".into(),
            options: vec![],
        };
        app.update(Msg::Progress(asked(2)));
        let requirement = Requirement {
            version: 3,
            constraint: Constraint { text: "main.rs".into(), answers: Some(2), ..Default::default() },
            at_ms: 0,
        };
        app.update(Msg::Progress(Progress::Required { run: run.to_string(), requirement }));
        assert!(app.task.as_ref().unwrap().asked.is_empty());
    }

    #[test]
    fn what_is_typed_while_a_task_runs_steers_it() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.settings.check = Some("make test".into());
        let actions = type_in(&mut app, "fix it");
        let [Action::Run { trace: run, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let run = run.clone();
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.to_string(), attempt: 0, fork: None }));

        let (trace, action) = control(&type_in(&mut app, "keep the short -v too"));
        assert_eq!(trace, run);
        assert_eq!(
            action,
            Control::Constrain(Constraint { text: "keep the short -v too".into(), ..Default::default() })
        );
        assert!(log(&app).contains("+ keep the short -v too"), "{}", log(&app));
        app.update(Msg::Controlled { trace: run.clone(), action, result: answer(RunState::Running, 1, &[]) });
        assert_eq!(app.task.as_ref().unwrap().version, 1);
        assert!(log(&app).contains("requirements v1"), "{}", log(&app));

        let (_, action) = control(&type_in(&mut app, "/protect Cargo.lock src/api"));
        assert_eq!(
            action,
            Control::Constrain(Constraint {
                protect: vec!["Cargo.lock".into(), "src/api".into()],
                ..Default::default()
            })
        );
        app.update(Msg::Controlled { trace: run.clone(), action, result: Err("too late to change the run".into()) });
        assert!(log(&app).contains("✗ not added: too late to change the run"), "{}", log(&app));

        // A check given while the task runs replaces its check, not the one for later tasks.
        let (_, action) = control(&type_in(&mut app, "/check cargo test -q"));
        assert_eq!(
            action,
            Control::Constrain(Constraint { check: Some("cargo test -q".into()), ..Default::default() })
        );
        assert_eq!(app.settings.check.as_deref(), Some("make test"));

        let (_, action) = control(&type_in(&mut app, "/pause"));
        assert_eq!(action, Control::Pause);
        let waiting = ["attempt 0: a model call"];
        app.update(Msg::Controlled { trace: run.clone(), action, result: answer(RunState::Pausing, 1, &waiting) });
        let task = app.task.as_ref().unwrap();
        assert_eq!(
            (task.state, task.waiting.as_slice()),
            (RunState::Pausing, &["attempt 0: a model call".to_owned()][..])
        );
        let state = |state| Progress::State { run: run.to_string(), state, waiting: vec![] };
        app.update(Msg::Progress(state(RunState::Paused)));
        assert_eq!(app.task.as_ref().unwrap().state, RunState::Paused);
        assert_eq!(control(&type_in(&mut app, "/resume")).1, Control::Resume);
        app.update(Msg::Progress(state(RunState::Running)));
        assert_eq!(app.task.as_ref().unwrap().state, RunState::Running);

        app.update(Msg::Progress(Progress::Required {
            run: run.to_string(),
            requirement: Requirement { version: 2, constraint: Constraint::default(), at_ms: 0 },
        }));
        app.update(Msg::Progress(Progress::Adopted { run: run.to_string(), attempt: 0, version: 2, turn: 3 }));
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.version, 2);
        assert!(plain(&task.lanes[0].lines).iter().any(|l| l.contains("now follows v2")));

        // Once the task is done, there is nothing to steer.
        app.update(Msg::Ran { trace: run, result: Box::new(Ok(passed(true, None))) });
        assert!(type_in(&mut app, "/pause").is_empty());
        assert_eq!(app.flash.as_deref(), Some("no task is running"));
    }

    #[test]
    fn each_task_is_told_what_the_tasks_before_it_did_until_a_new_conversation() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.settings.check = Some("make test".into());
        let actions = type_in(&mut app, "add a --verbose flag");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.earlier.is_empty());
        let trace = trace.clone();
        app.update(Msg::Ran { trace, result: Box::new(Ok(passed(false, Some("/d/work/fork-1")))) });
        if let Some(Prompt::Apply { .. }) = app.prompt {
            app.update(key(KeyCode::Enter));
        }
        assert_eq!(app.earlier.len(), 1);
        assert!(!app.earlier[0].applied);
        app.update(Msg::Applied(Ok(())));
        assert!(app.earlier[0].applied, "applying the result is part of what the next task knows");

        let actions = type_in(&mut app, "now document it");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        let [earlier] = req.earlier.as_slice() else { panic!("{:?}", req.earlier) };
        assert_eq!(earlier.task, "add a --verbose flag");
        assert_eq!(earlier.outcome, Outcome::Passed);
        assert_eq!(earlier.summary, "Trailing commas parse.");
        assert_eq!(earlier.changes, ["src/parser.rs"]);
        let trace = trace.clone();
        app.update(Msg::Ran { trace, result: Box::new(Ok(passed(true, None))) });
        assert_eq!(app.earlier.len(), 2);

        type_in(&mut app, "/new");
        assert!(app.earlier.is_empty());
        let actions = type_in(&mut app, "something else");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.earlier.is_empty());
    }

    #[test]
    fn a_conversation_keeps_its_latest_tasks() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.mode = Mode::Auto;
        for i in 0..EARLIER_TASKS + 2 {
            let actions = type_in(&mut app, &format!("task {i}"));
            let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
            let trace = trace.clone();
            app.update(Msg::Ran { trace, result: Box::new(Ok(passed(true, None))) });
        }
        assert_eq!(app.earlier.len(), EARLIER_TASKS);
        assert_eq!(app.earlier[0].task, "task 2");
    }

    #[test]
    fn attempts_use_the_remaining_effective_budget_without_a_minimum_top_up() {
        for (requested, remaining, expected) in [
            (Some(0.25), None, None),
            (Some(0.2), None, None),
            (None, Some(0.0), None),
            (Some(1.0), None, Some(0.75)),
            (None, Some(9.75), Some(9.75)),
        ] {
            let mut app = App::new("/w", None);
            up(&mut app);
            app.settings.budget_usd = requested;
            type_in(&mut app, "task");
            let trace = app.task.as_ref().unwrap().trace.clone();
            app.settings.budget_usd = Some(100.0); // Next-task settings cannot raise this task's budget.
                                                   // Nothing stops confirm mode here, so the attempts start right away.
            let actions = app.update(Msg::Designed {
                trace,
                result: Ok(DesignResponse { remaining_budget_usd: remaining, ..design() }),
                existing: vec![],
            });
            match expected {
                Some(budget) => {
                    let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
                    assert_eq!(req.budget_usd, Some(budget));
                }
                None => {
                    assert!(actions.is_empty() && app.task.is_none());
                    assert!(log(&app).contains("budget exhausted"));
                }
            }
            assert_eq!(app.spent_usd, 0.25);
        }
    }

    #[test]
    fn an_interrupt_keeps_the_attempts_forks_and_offers_to_start_again() {
        let mut app = App::new("/w", None);
        up(&mut app);
        // These are about the prompts themselves.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let first = req.clone();
        let run = trace.to_string();
        for (attempt, fork) in [(0, Some("/d/work/fork-a")), (1, Some("/d/work/fork-b")), (2, None)] {
            app.update(Msg::Progress(Progress::AttemptStarted {
                run: run.clone(),
                attempt,
                fork: fork.map(str::to_owned),
            }));
        }
        let actions = stop(&mut app);
        assert_eq!(
            actions,
            [Action::Interrupt {
                keep: vec![],
                attempts: vec!["/d/work/fork-a".into(), "/d/work/fork-b".into()],
                check_files: vec!["tests/commas.rs".into()],
                drop_run: Some(run.clone()),
            }]
        );
        // The runtime found changes in one of them.
        assert_eq!(
            app.update(Msg::Stopped { kept: vec!["/d/work/fork-b".into()], notes: vec![] }),
            [Action::StartSession]
        );
        assert_eq!(app.kept, ["/d/work/fork-b"]);
        assert!(log(&app).ends_with("  kept 1 · interrupted\n  kept in /d/work/fork-b"), "{}", log(&app));

        assert!(matches!(app.prompt, Some(Prompt::Retry { stopped: true, .. })));
        assert!(app.update(key(KeyCode::Char('r'))).is_empty(), "not while the services restart");
        up(&mut app);
        let actions = app.update(key(KeyCode::Char('r')));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((&req.check, &req.check_files), (&first.check, &first.check_files));
    }

    #[test]
    fn ask_and_quick_run_their_workflow_without_designing_even_in_confirm_mode() {
        let mut app = App::new("/w", None);
        up(&mut app);
        assert_eq!(app.mode, Mode::Confirm);
        let actions = type_in(&mut app, "/ask what does the parser accept?");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.task.as_str(), req.workflow.as_deref()), ("what does the parser accept?", Some("ask")));
        assert_eq!(app.settings.workflow, None, "the workflow is the task's, not the next one's");
        let answer = RunResponse {
            outcome: Outcome::Unverified,
            check: None,
            changes: vec![],
            patch: String::new(),
            workflow: Some("ask".into()),
            conversation: conversation(),
            ..passed(false, None)
        };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(answer)) });
        assert!(actions.is_empty(), "an answer is not learned from: {actions:?}");
        assert!(log(&app).contains("✓ unverified answer"), "{}", log(&app));

        let actions = type_in(&mut app, "/quick add a comment to main");
        assert!(
            matches!(actions.as_slice(), [Action::Run { req, .. }] if req.workflow.as_deref() == Some("quick")),
            "{actions:?}"
        );
        let actions = type_in(&mut app, "/quick");
        assert!(actions.is_empty() && app.flash.as_deref().is_some_and(|f| f.contains("a task is waiting")));
    }

    #[test]
    fn a_routed_task_says_what_it_was_handled_as_and_verify_runs_it_again_in_full() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.mode = Mode::Auto;
        let actions = type_in(&mut app, "add a comment to main");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.workflow.as_deref(), Some("auto"), "the planner picks the workflow");
        let routed = Progress::Routed {
            run: trace.to_string(),
            workflow: "quick".into(),
            reason: "a comment".into(),
            cost_usd: Some(0.0021),
        };
        app.update(Msg::Progress(routed));
        let said = "handled as quick (a comment) · /verify for the full workflow · pick $0.002";
        assert!(log(&app).contains(said), "{}", log(&app));
        assert_eq!(app.task.as_ref().unwrap().req.workflow.as_deref(), Some("quick"));
        let done = RunResponse { workflow: Some("quick".into()), ..passed(true, None) };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(done)) });
        assert!(actions.iter().all(|a| !matches!(a, Action::Learn { .. })), "quick does not learn: {actions:?}");

        let actions = type_in(&mut app, "/verify");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.task.as_str(), req.workflow.as_deref()), ("add a comment to main", Some("verify")));
    }

    #[test]
    fn confirm_mode_runs_a_task_picked_as_quick_without_stopping_for_a_check() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "add a comment to main");
        let [Action::Design { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.workflow.as_deref(), Some("auto"));
        let picked = DesignResponse { command: None, files: vec![], workflow: Some("quick".into()), ..design() };
        let actions = app.update(Msg::Designed { trace: trace.clone(), result: Ok(picked), existing: vec![] });
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.workflow.as_deref(), Some("quick"));
        assert!(req.verify && req.check.is_none() && app.prompt.is_none());

        // Picked as verify, the check is shown as before and the run names verify.
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "fix the parser");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let picked = DesignResponse { workflow: Some("verify".into()), ..design() };
        app.rules.always_ask = true;
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(picked), existing: vec![] });
        assert!(matches!(app.prompt, Some(Prompt::Check(_))));
        assert_eq!(app.task.as_ref().unwrap().req.workflow.as_deref(), Some("verify"));
    }

    #[test]
    fn continue_sends_the_last_conversation_and_works_in_its_kept_result() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.mode = Mode::Plan;
        assert!(type_in(&mut app, "/continue more").is_empty());
        assert!(app.flash.as_deref().is_some_and(|f| f.starts_with("nothing to go on from")));
        let actions = type_in(&mut app, "first");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let kept = RunResponse { conversation: conversation(), ..passed(false, Some("/fork/first")) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(kept)) });
        assert_eq!(app.kept, ["/fork/first"]);

        let actions = type_in(&mut app, "/continue now document it");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, "now document it");
        assert_eq!(
            req.continues,
            Some(Continuation { conversation: conversation(), fork: Some("/fork/first".into()) }),
            "the whole conversation, and the result it goes on from"
        );
        let applied = RunResponse { conversation: conversation(), ..passed(true, None) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(applied)) });
        assert!(app.kept.is_empty(), "the kept result was applied with the task that went on from it");
        assert_eq!(app.continuation.as_ref().map(|c| c.fork.clone()), Some(None));

        type_in(&mut app, "/new");
        assert!(app.continuation.is_none());
    }

    #[test]
    fn plain_input_goes_on_from_the_last_task_and_new_or_a_bare_verify_starts_afresh() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.mode = Mode::Plan;
        let actions = type_in(&mut app, "解释登录实现");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.continues, None, "the first task has nothing to go on from");
        let kept = RunResponse { conversation: conversation(), ..passed(false, Some("/fork/first")) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(kept)) });
        assert_eq!(app.kept, ["/fork/first"]);

        // No /continue: the follow-up has the whole conversation and works on the kept result.
        let actions = type_in(&mut app, "这个为什么更合适？");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.workflow.as_deref(), Some("auto"));
        assert_eq!(
            req.continues,
            Some(Continuation { conversation: conversation(), fork: Some("/fork/first".into()) })
        );
        assert_eq!(req.earlier.len(), 1);
        assert!(
            log(&app).contains("goes on from the last task and its kept result · /new starts a new conversation"),
            "{}",
            log(&app)
        );

        // Answered without a change: the kept result is still what the next task goes on from, and nothing is applied.
        let mut said = conversation();
        said.push(serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": "Because." }] }));
        let answer = RunResponse {
            outcome: Outcome::Unverified,
            check: None,
            changes: vec![],
            patch: String::new(),
            workflow: Some("ask".into()),
            conversation: said.clone(),
            ..passed(false, None)
        };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(answer)) });
        assert!(actions.iter().all(|a| !matches!(a, Action::Apply { .. })), "{actions:?}");
        assert_eq!(app.kept, ["/fork/first"]);
        assert_eq!(app.continuation, Some(Continuation { conversation: said, fork: Some("/fork/first".into()) }));

        // `/verify` alone runs the last task again from the workspace.
        let actions = type_in(&mut app, "/verify");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.task.as_str(), req.workflow.as_deref()), ("这个为什么更合适？", Some("verify")));
        assert_eq!(req.continues, None);
        let done = RunResponse { conversation: conversation(), ..passed(false, Some("/fork/again")) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(done)) });

        type_in(&mut app, "/new");
        let actions = type_in(&mut app, "something else");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.continues.is_none() && req.earlier.is_empty(), "{req:?}");
        assert!(!log(&app).ends_with("/new starts a new conversation"), "{}", log(&app));
    }

    #[test]
    fn a_follow_up_keeps_the_check_and_workflow_given_also_after_the_session_is_picked_up_again() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.mode = Mode::Auto;
        app.settings.check = Some("cargo test".into());
        let actions = type_in(&mut app, "fix the parser");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let done = RunResponse { conversation: conversation(), ..passed(true, None) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(done)) });

        let actions = type_in(&mut app, "now the other case too");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.check.as_deref(), req.workflow.as_deref()), (Some("cargo test"), Some("auto")));
        assert_eq!(req.continues, Some(Continuation { conversation: conversation(), fork: None }));
        let done = RunResponse { conversation: conversation(), ..passed(true, None) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(done)) });

        // A workflow named for the task is that workflow, going on all the same.
        let actions = type_in(&mut app, "/verify and a test for it");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.workflow.as_deref(), req.check.as_deref()), (Some("verify"), Some("cargo test")));
        assert!(req.continues.is_some());
        let done = RunResponse { conversation: conversation(), ..passed(true, None) };
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(done)) });

        // Picked up by a new interface: the earlier tasks and the check are there, and the pick reads them.
        let saved = app.snapshot();
        let mut again = App::new("/w", None);
        up(&mut again);
        again.mode = Mode::Auto;
        again.settings = app.settings.clone();
        again.restore_session(saved);
        let actions = type_in(&mut again, "and the docs");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.check.as_deref(), req.workflow.as_deref()), (Some("cargo test"), Some("auto")));
        let tasks: Vec<&str> = req.earlier.iter().map(|e| e.task.as_str()).collect();
        assert_eq!(tasks, ["fix the parser", "now the other case too", "and a test for it"]);
        assert_eq!(req.continues, None, "the conversation itself is not saved; its summaries are");
    }
}
