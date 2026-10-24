//! The retry offer after no attempt passed.
use super::*;

impl App {
    /// Keys while a failed run offers a retry: the actions, or `None` to
    /// handle the key as usual, once the offer is gone.
    pub(super) fn retry_key(&mut self, key: &KeyEvent) -> Option<Vec<Action>> {
        let Some(Prompt::Retry { attempts, .. }) = &self.prompt else { return None };
        let empty = self.input.text.is_empty();
        match key.code {
            KeyCode::Char('r') if empty && key.modifiers.is_empty() => {
                if !matches!(self.session, SessionState::Up { .. }) {
                    self.flash = Some("the services are not up".into());
                    return Some(Vec::new());
                }
                let Some(Prompt::Retry { req, designed, mode, settings, .. }) = self.prompt.take() else { return None };
                Some(self.retry(*req, designed, mode, *settings))
            }
            KeyCode::Char('v') if empty && key.modifiers.is_empty() => {
                let mut lines = Vec::new();
                for a in attempts {
                    lines.push(Line::from(span(format!("attempt {}", a.index + 1), Tone::Shell)));
                    let note = if a.note.is_empty() { "(no note)".to_owned() } else { scrub(&a.note, &self.secrets) };
                    lines.extend(block(&note, "  ", Tone::Plain));
                    lines.push(Line::default());
                }
                lines.extend(lane_lines(&self.last_lanes));
                self.viewer = Some(Viewer { title: "why the attempts failed".into(), lines, scroll: 0 });
                Some(Vec::new())
            }
            KeyCode::Esc => {
                self.prompt = None;
                Some(Vec::new())
            }
            _ => {
                self.prompt = None;
                None
            }
        }
    }

    /// Run a failed task again with the check it ran with.
    fn retry(&mut self, req: RunRequest, designed: bool, mode: Mode, settings: Settings) -> Vec<Action> {
        let trace = TraceId::random();
        self.gap();
        self.echo(&req.task.clone());
        let mut lines = vec![check_head(if designed { "designed" } else { "yours" })];
        if let Some(command) = &req.check {
            lines.push(Line::from(vec![span("  $ ", Tone::Dim), span(clean_line(command), Tone::Plain)]));
        }
        for f in &req.check_files {
            lines.push(Line::from(span(format!("  + {}", clean_line(&f.path)), Tone::Dim)));
        }
        lines.push(Line::from(span("  again, with the same check", Tone::Dim)));
        self.log.extend(lines);
        self.scroll = 0;
        self.zoom = None;
        self.last_run = Some(trace.to_string());
        self.task =
            Some(Task { check_shown: true, ..Task::new(trace.clone(), req.clone(), Phase::Running, mode, settings) });
        self.starting(trace.clone(), Action::Run { req, trace })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn a_failed_run_says_why_and_retries_with_the_same_check() {
        let mut app = App::new("/w", None);
        app.secrets = vec!["sk-ant-secret-1234".into()];
        up(&mut app);
        // These are about the prompts themselves.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "accept trailing commas");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Run { req: first, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let first = first.clone();

        let run = trace.to_string();
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 0, fork: None }));
        app.update(Msg::Progress(Progress::CheckRan {
            run: run.clone(),
            attempt: 0,
            passed: false,
            exit_code: Some(101),
            tail: "stdout:\nrunning 1 test\nkey sk-ant-secret-1234\nassertion failed: commas\n".into(),
            judged: false,
        }));
        assert_eq!(
            plain(&app.task.as_ref().unwrap().lanes[0].lines),
            ["   check failed (exit 101)", "   key [redacted]", "   assertion failed: commas"]
        );

        let note = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n") + "\nsk-ant-secret-1234";
        let resp = failed(vec![
            attempt(0, AttemptStatus::Failed, &note),
            attempt(1, AttemptStatus::Error, "model call failed: overloaded"),
        ]);
        let actions = app.update(Msg::Ran { trace, result: Ok(resp) });
        assert_eq!(actions.len(), 1, "only learning: {actions:?}");
        let log = log(&app);
        assert!(
            log.contains("  ✗ attempt 1 failed · 3 turns · 2 checks · $0.10\n      line 6\n      line 7\n      line 8\n      line 9\n      line 10\n      [redacted]\n  ! attempt 2 error"),
            "{log}"
        );
        assert!(log.contains("      model call failed: overloaded"), "{log}");
        assert!(!log.contains("line 5") && !log.contains("sk-ant"), "{log}");

        app.update(key(KeyCode::Char('v')));
        let viewer = app.viewer.take().expect("why opens");
        let shown = plain(&viewer.lines).join("\n");
        assert!(shown.contains("  line 1\n") && shown.contains("attempt 1 · tool calls"), "{shown}");
        assert!(!shown.contains("sk-ant"), "{shown}");

        let actions = app.update(key(KeyCode::Char('r')));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("a run, and no design: {actions:?}") };
        assert_eq!((&req.check, &req.check_files), (&first.check, &first.check_files));
        assert_eq!(req.check.as_deref(), Some("cargo test --test commas"));
        assert!(app.prompt.is_none() && app.task.is_some());
    }

    #[test]
    fn typing_after_a_failure_starts_a_new_task_and_the_lanes_stay_viewable() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check make test");
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let run = trace.to_string();
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 0, fork: None }));
        app.update(Msg::Progress(Progress::ToolCall {
            run,
            attempt: 0,
            turn: 1,
            tool: "read_file".into(),
            detail: "read Makefile".into(),
        }));
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(failed(vec![attempt(0, AttemptStatus::Failed, "x")])) });
        assert!(matches!(app.prompt, Some(Prompt::Retry { .. })));
        // A key that is not the offer's goes to the input and ends the offer.
        app.update(key(KeyCode::Char('f')));
        assert!(app.prompt.is_none());
        assert_eq!(app.input.text, "f");
        app.update(ctrl_key('u'));
        type_in(&mut app, "/lanes");
        let viewer = app.viewer.take().expect("the last task's attempts");
        let lines = plain(&viewer.lines);
        assert!(lines[0].starts_with("attempt 1 · tool calls · 0s · $0.10"), "{}", lines[0]);
        assert_eq!(lines[1], " 1 read Makefile");
    }
}
