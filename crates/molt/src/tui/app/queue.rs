//! Ordered, explicit tasks. Ordinary input still constrains the running task.
use super::*;

const MAX_TASKS: usize = 100;
const TASK_BYTES: usize = 64 * 1024;
const QUEUE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, serde::Serialize)]
pub struct Queued {
    pub text: String,
    pub mode: Mode,
    pub settings: Settings,
}

impl Queued {
    fn bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(QUEUE_BYTES, |bytes| bytes.len())
    }
}

pub struct View {
    pub selected: usize,
    pub draft: Option<Input>,
}

impl App {
    pub(super) fn queue_command(&mut self, arg: &str) -> Vec<Action> {
        let (command, rest) = arg.split_once(' ').map_or((arg, ""), |(command, rest)| (command, rest.trim()));
        match command {
            "" => self.queue_view = Some(View { selected: 0, draft: None }),
            "add" if rest.is_empty() => self.queue_view = Some(View { selected: 0, draft: Some(Input::default()) }),
            "add" => self.enqueue(rest.to_owned()),
            "pause" if rest.is_empty() => {
                self.queue_paused = true;
                self.flash = Some("queue paused; the current task continues".into());
            }
            "resume" if rest.is_empty() => {
                self.queue_paused = false;
                self.flash = Some("queue resumed".into());
            }
            "clear" if rest.is_empty() => {
                self.queue.clear();
                self.flash = Some("queued tasks cleared; the current task continues".into());
            }
            "remove" => match self.queue_index(rest) {
                Some(at) => {
                    self.queue.remove(at);
                }
                None => self.flash = Some("/queue remove N: use a position from /queue".into()),
            },
            "move" => {
                let indices: Vec<_> = rest.split_whitespace().collect();
                let pair = (indices.len() == 2).then(|| (self.queue_index(indices[0]), self.queue_index(indices[1])));
                if let Some((Some(from), Some(to))) = pair {
                    let task = self.queue.remove(from);
                    self.queue.insert(to, task);
                } else {
                    self.flash = Some("/queue move FROM TO: use two positions from /queue".into());
                }
            }
            _ => self.flash = Some("/queue [add TASK|remove N|move FROM TO|pause|resume|clear]".into()),
        }
        Vec::new()
    }

    fn queue_index(&self, text: &str) -> Option<usize> {
        text.parse::<usize>().ok().filter(|&n| n > 0 && n <= self.queue.len()).map(|n| n - 1)
    }

    fn enqueue(&mut self, text: String) {
        let task = Queued { text, mode: self.mode, settings: self.settings.clone() };
        if task.text.len() > TASK_BYTES
            || self.queue.len() >= MAX_TASKS
            || task.bytes().saturating_add(self.queue.iter().map(Queued::bytes).sum()) > QUEUE_BYTES
        {
            self.flash = Some("queue full: at most 100 tasks, 64 KiB per task and 4 MiB total; draft retained".into());
            if let Some(view) = &mut self.queue_view {
                view.draft = Some(Input::with(&task.text));
            } else {
                self.input = Input::with(&format!("/queue add {}", task.text));
            }
            return;
        }
        self.queue.push(task);
        self.flash = Some(format!("queued {}", self.queue.len()));
    }

    pub(super) fn advance_queue(&mut self) -> Vec<Action> {
        if let Some(view) = &mut self.queue_view {
            view.selected = view.selected.min(self.queue.len().saturating_sub(1));
        }
        if self.quit
            || self.quit_armed.is_some()
            || self.quit_after_git
            || self.queue_paused
            || self.queue.is_empty()
            || self.task.is_some()
            || self.git_busy
            || self.fork_operations > 0
            || self.kept_pending.is_some()
            || !matches!(self.session, SessionState::Up { .. })
        {
            return Vec::new();
        }
        let queued = self.queue[0].clone();
        let draft = self.input.clone();
        // Completion may predate the previous apply. The planner reads @files from
        // the current workspace; only path syntax can be checked here.
        let complete = std::mem::replace(&mut self.files_complete, false);
        let actions = self.start_task(queued.text, queued.mode, queued.settings);
        self.files_complete = complete;
        self.input = draft;
        if self.task.is_some() {
            self.queue.remove(0);
            // A failure's optional retry offer yields to explicitly queued work.
            self.prompt = None;
        } else {
            self.queue_paused = true;
            self.flash = Some(format!(
                "queue paused: {}; fix the task and /queue resume",
                self.flash.take().unwrap_or_else(|| "task could not start".into())
            ));
        }
        actions
    }

    pub(super) fn queue_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let view = self.queue_view.as_mut().unwrap();
        if let Some(input) = &mut view.draft {
            if key_is(key, KeyCode::Esc) || ctrl(key, 'c') {
                view.draft = None;
            } else if ctrl(key, 'j')
                || (key.code == KeyCode::Enter && key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT))
            {
                input.newline();
            } else if key_is(key, KeyCode::Enter) {
                let text = input.take();
                if !text.trim().is_empty() {
                    view.draft = None;
                    return self.submit(format!("/queue add {}", text.trim()));
                }
            } else {
                input.key(key);
            }
            return Vec::new();
        }
        let at = view.selected.min(self.queue.len().saturating_sub(1));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.queue_view = None,
            KeyCode::Char('a') => view.draft = Some(Input::default()),
            KeyCode::Up | KeyCode::Char('k') => view.selected = at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => view.selected = (at + 1).min(self.queue.len().saturating_sub(1)),
            KeyCode::Char('K') if at > 0 => {
                self.queue.swap(at, at - 1);
                view.selected = at - 1;
            }
            KeyCode::Char('J') if at + 1 < self.queue.len() => {
                self.queue.swap(at, at + 1);
                view.selected = at + 1;
            }
            KeyCode::Delete | KeyCode::Char('n') if !self.queue.is_empty() => {
                self.queue.remove(at);
                view.selected = at.min(self.queue.len().saturating_sub(1));
            }
            KeyCode::Char('p') => self.queue_paused = true,
            KeyCode::Char('r') => self.queue_paused = false,
            KeyCode::Enter | KeyCode::Char('v') if !self.queue.is_empty() => {
                self.viewer = Some(Viewer {
                    title: format!("queued task {}", at + 1),
                    lines: block(&scrub(&self.queue[at].text, &self.secrets), "", Tone::Plain),
                    scroll: 0,
                });
            }
            _ => {}
        }
        Vec::new()
    }

    pub(in crate::tui) fn queue_lines(&self) -> Vec<Line<'static>> {
        if self.queue.is_empty() {
            return block("no queued tasks · a adds a task", "", Tone::Dim);
        }
        self.queue
            .iter()
            .enumerate()
            .flat_map(|(index, task)| {
                let mark =
                    if self.queue_view.as_ref().is_some_and(|view| view.selected == index) { "›" } else { " " };
                let model = task.settings.model.as_deref().unwrap_or("default model");
                let check = task.settings.check.as_deref().unwrap_or("design a check");
                vec![
                    Line::from(span(
                        format!("{mark} {}. {}", index + 1, clean_line(&scrub(&task.text, &self.secrets))),
                        Tone::Shell,
                    )),
                    Line::from(span(format!("   {} · {}", task.mode.label(), clean_line(model)), Tone::Dim)),
                    Line::from(span(format!("   check: {}", clean_line(&scrub(check, &self.secrets))), Tone::Dim)),
                ]
            })
            .collect()
    }

    pub(in crate::tui) fn unexecuted(&self) -> Vec<String> {
        self.queue.iter().map(|task| text::clean(&scrub(&task.text, &self.secrets)).join("\n")).collect()
    }

    pub(super) fn show_unexecuted(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let mut lines = block("These tasks have not run. Their complete text is printed after exit.", "", Tone::Warn);
        for (index, task) in self.unexecuted().iter().enumerate() {
            lines.push(Line::from(span(format!("queued {}", index + 1), Tone::Shell)));
            lines.extend(block(task, "  ", Tone::Plain));
        }
        self.viewer =
            Some(Viewer { title: "unexecuted queued tasks · Ctrl+D again to quit".into(), lines, scroll: 0 });
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn queued_tasks_snapshot_execution_settings_and_plain_input_still_steers_the_current_task() {
        let mut app = queue_app();
        type_in(&mut app, "first");
        type_in(&mut app, "/model haiku");
        type_in(&mut app, "/attempts 2");
        type_in(&mut app, "/budget 1.5");
        type_in(&mut app, "/effort high");
        type_in(&mut app, "/stream off");
        assert!(type_in(&mut app, "/queue add second").is_empty());
        type_in(&mut app, "/model sonnet");
        type_in(&mut app, "/queue add third");
        let actions = type_in(&mut app, "also support Unicode");
        assert!(
            matches!(actions.as_slice(), [Action::Control { action: Control::Constrain(c), .. }] if c.text == "also support Unicode")
        );
        assert_eq!(app.queue.len(), 2);
        app.mode = Mode::Auto;
        app.settings.check = Some("false".into());
        app.settings.model = Some("opus".into());
        app.take_preference_changes();
        let trace = app.task.as_ref().unwrap().trace.clone();
        let actions = app.update(Msg::Ran { trace, result: Ok(passed(false, Some("/fork/first"))) });
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(
            (req.task.as_str(), req.model.as_deref(), req.check.as_deref()),
            ("second", Some("haiku"), Some("true"))
        );
        assert_eq!(
            (req.attempts, req.budget_usd, req.effort, req.apply, req.stream),
            (2, Some(1.5), Some(Effort::High), false, false)
        );
        assert_eq!(req.earlier.last().unwrap().task, "first");
        assert_eq!(app.mode, Mode::Auto);
        assert_eq!(app.settings.model.as_deref(), Some("opus"));
        assert!(app.take_preference_changes().is_empty());
        let trace = app.task.as_ref().unwrap().trace.clone();
        let actions = app.update(Msg::Ran { trace, result: Ok(passed(false, Some("/fork/second"))) });
        assert!(
            matches!(actions.as_slice(), [Action::Run { req, .. }] if req.task == "third" && req.model.as_deref() == Some("sonnet") && !req.apply)
        );
        assert_eq!(app.kept, ["/fork/first", "/fork/second"]);
        assert!(app.queue.is_empty());
    }

    #[test]
    fn a_queue_waits_for_apply_commit_and_discard_to_settle_before_starting_the_next_task() {
        let mut app = queue_app();
        app.mode = Mode::Confirm;
        app.rules.always_ask = true;
        app.settings.commit = true;
        type_in(&mut app, "first");
        type_in(&mut app, "/queue add second");
        type_in(&mut app, "/queue add third");
        let trace = app.task.as_ref().unwrap().trace.clone();
        assert!(app.update(Msg::Ran { trace, result: Ok(passed(false, Some("/fork/first"))) }).is_empty());
        assert_eq!(app.queue.len(), 2);
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::Apply { fork: "/fork/first".into() }]);
        assert_eq!(app.queue.len(), 2);
        let actions = app.update(Msg::Applied(Ok(())));
        assert!(matches!(actions.as_slice(), [Action::Commit { .. }]));
        assert_eq!(app.queue.len(), 2);
        let actions = app.update(Msg::Committed(Ok(None)));
        assert!(matches!(actions.as_slice(), [Action::PrepareCommit { .. }, Action::Run { req, .. }]
            if req.task == "second" && req.earlier.last().unwrap().applied));
        let trace = app.task.as_ref().unwrap().trace.clone();
        app.update(Msg::Ran { trace, result: Ok(passed(false, Some("/fork/second"))) });
        assert_eq!(app.update(key(KeyCode::Char('n'))), [Action::Discard { fork: "/fork/second".into() }]);
        assert_eq!(app.queue.len(), 1);
        let actions = app.update(Msg::Discarded(Ok(())));
        assert!(
            matches!(actions.as_slice(), [Action::PrepareCommit { .. }, Action::Run { req, .. }] if req.task == "third")
        );
    }

    #[test]
    fn queued_confirm_tasks_design_their_own_check_and_wait_for_each_decision() {
        let mut app = queue_app();
        type_in(&mut app, "first");
        app.mode = Mode::Confirm;
        app.settings.check = None;
        app.rules.always_ask = true;
        type_in(&mut app, "/queue add second");
        type_in(&mut app, "/queue add third");
        app.mode = Mode::Auto;
        app.settings.check = Some("false".into());
        let trace = app.task.as_ref().unwrap().trace.clone();
        let actions = app.update(Msg::Ran { trace, result: Ok(passed(false, Some("/fork/first"))) });
        let [Action::Design { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, "second");
        assert!(!req.apply && req.check.is_none());
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        assert!(matches!(app.prompt, Some(Prompt::Check(_))));
        assert_eq!(app.queue.len(), 1);
        let actions = app.update(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Design { req, .. }] if req.task == "third"));
    }

    #[test]
    fn stopping_the_current_task_preserves_and_pauses_its_queue_until_explicit_resume() {
        let mut app = queue_app();
        type_in(&mut app, "first");
        type_in(&mut app, "/queue add second\nwith a second line");
        assert!(matches!(stop(&mut app).as_slice(), [Action::Interrupt { .. }]));
        assert!(app.queue_paused && app.queue.len() == 1);
        assert!(frame_text(&mut app).contains("1 queued"));
        assert_eq!(app.update(Msg::Stopped { kept: vec![], notes: vec![] }), [Action::StartSession]);
        assert!(app
            .update(Msg::SessionUp {
                versions: vec![],
                memory_problem: Some("not configured".into()),
                default_model: None
            })
            .is_empty());
        let actions = type_in(&mut app, "/queue resume");
        assert!(matches!(actions.as_slice(), [Action::Run { req, .. }] if req.task == "second\nwith a second line"));
    }

    #[test]
    fn queue_exit_warning_and_terminal_report_retain_complete_unexecuted_task_text() {
        for command in [false, true] {
            let mut app = queue_app();
            type_in(&mut app, "/queue pause");
            let task = "first line\nsecond line with spaces  and Unicode 中文";
            type_in(&mut app, &format!("/queue add {task}"));
            if command {
                assert!(type_in(&mut app, "/quit").is_empty());
            } else {
                assert!(app.update(ctrl_key('d')).is_empty());
            }
            let shown = plain(&app.viewer.as_ref().unwrap().lines).join("\n");
            assert!(shown.contains("first line\n  second line with spaces  and Unicode 中文"), "{shown}");
            assert_eq!(app.unexecuted(), [task]);
            assert!(matches!(app.update(ctrl_key('d')).as_slice(), [Action::Quit { .. }]));
            assert!(app.quit && app.queue.len() == 1);
            assert_eq!(app.unexecuted(), [task]);
        }
    }

    #[test]
    fn the_queue_browser_adds_multiline_tasks_and_can_reorder_inspect_and_remove_them() {
        let mut app = queue_app();
        type_in(&mut app, "/queue pause");
        for task in ["first", "second", "third"] {
            type_in(&mut app, &format!("/queue add {task}"));
        }
        type_in(&mut app, "/queue move 3 1");
        type_in(&mut app, "/queue remove 2");
        assert_eq!(app.unexecuted(), ["third", "second"]);
        type_in(&mut app, "/queue");
        assert!(frame_text(&mut app).contains("task queue · paused"));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('K')));
        assert_eq!(app.unexecuted(), ["second", "third"]);
        app.update(key(KeyCode::Char('n')));
        app.update(key(KeyCode::Char('a')));
        app.update(Msg::Paste("cancel this draft".into()));
        app.update(ctrl_key('c'));
        assert!(app.queue_view.as_ref().unwrap().draft.is_none() && app.quit_armed.is_none());
        app.update(key(KeyCode::Char('a')));
        app.update(Msg::Paste("new\nmultiline task".into()));
        app.update(key(KeyCode::Enter));
        assert_eq!(app.unexecuted(), ["third", "new\nmultiline task"]);
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Enter));
        assert!(plain(&app.viewer.as_ref().unwrap().lines).join("\n").contains("new\nmultiline task"));
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Esc));
        type_in(&mut app, "/queue move 0 1");
        assert_eq!(app.queue.len(), 2);
        type_in(&mut app, "/queue clear");
        assert!(app.queue.is_empty());
    }

    #[test]
    fn queue_limits_keep_rejected_drafts_and_bad_file_references_pause_without_losing_the_task() {
        let mut app = queue_app();
        type_in(&mut app, "/queue pause");
        let oversized = format!("/queue add {}", "a".repeat(64 * 1024 + 1));
        type_in(&mut app, &oversized);
        assert!(app.queue.is_empty());
        assert_eq!(app.input.full(), oversized);
        app.input.take();
        for _ in 0..100 {
            type_in(&mut app, "/queue add task");
        }
        type_in(&mut app, "/queue add overflow");
        assert_eq!(app.queue.len(), 100);
        assert_eq!(app.input.full(), "/queue add overflow");

        let tmp = tempfile::tempdir().unwrap();
        let mut app = queue_app();
        app.workspace_path = tmp.path().to_str().unwrap().into();
        type_in(&mut app, "first");
        type_in(&mut app, "/queue add update @../outside");
        app.input = Input::with("my unsent draft");
        let trace = app.task.as_ref().unwrap().trace.clone();
        assert!(app.update(Msg::Ran { trace, result: Err("failed".into()) }).is_empty());
        assert!(app.queue_paused && app.queue.len() == 1 && app.task.is_none());
        assert_eq!(app.input.full(), "my unsent draft");
        std::fs::write(tmp.path().join("later.txt"), "added by the preceding task").unwrap();
        app.input.take();
        type_in(&mut app, "/queue remove 1");
        type_in(&mut app, "/queue add update @later.txt");
        app.files = Some(vec![]);
        app.files_complete = true;
        let actions = type_in(&mut app, "/queue resume");
        assert!(matches!(actions.as_slice(), [Action::Run { req, .. }] if req.files == ["later.txt"]));
    }

    #[test]
    fn a_failed_task_yields_its_optional_retry_offer_to_queued_work() {
        let mut app = queue_app();
        type_in(&mut app, "first");
        type_in(&mut app, "/queue add second");
        let trace = app.task.as_ref().unwrap().trace.clone();
        let mut response = passed(false, None);
        response.outcome = Outcome::Failed;
        let actions = app.update(Msg::Ran { trace, result: Ok(response) });
        assert!(matches!(actions.as_slice(), [Action::Run { req, .. }] if req.task == "second"));
        assert!(app.prompt.is_none());
    }
}
