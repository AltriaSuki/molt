//! What the status bar and the window title say, and when the user is
//! called from outside the terminal.
use super::*;

impl App {
    pub fn retry_seconds(&self) -> Option<u64> {
        self.retry_at.map(|at| (at.saturating_sub(self.tick) * TICK_MS / 1000).max(1))
    }

    pub fn memory_up(&self) -> bool {
        matches!(&self.session, SessionState::Up { memory_problem: None, .. })
    }

    pub(super) fn attention(&self) -> Option<Attention> {
        match (&self.session, &self.prompt) {
            (SessionState::Down(_), _) => Some(Attention::Down),
            (_, Some(Prompt::Check(_))) => Some(Attention::Check),
            (_, Some(Prompt::Apply { .. })) => Some(Attention::Apply),
            _ => None,
        }
    }

    /// The user is at the terminal: it has focus, or a key came just now.
    pub(super) fn looking(&self) -> bool {
        self.focused == Some(true) || self.last_key.is_some_and(|at| self.tick.wrapping_sub(at) <= LOOKING_TICKS)
    }

    /// The terminal window's title: what the interface is doing or waits for.
    pub fn title(&self) -> String {
        let state = match (self.attention(), &self.task) {
            (Some(Attention::Down), _) => "services down".to_owned(),
            (Some(Attention::Check), _) => "waiting: check".to_owned(),
            (Some(Attention::Apply), _) => "waiting: apply".to_owned(),
            (None, Some(task)) => match task.phase {
                Phase::Designing => "designing the check".to_owned(),
                Phase::Running if task.state == RunState::Paused => "paused".to_owned(),
                Phase::Running => {
                    let running = task.lanes.iter().filter(|l| l.status.is_none()).count();
                    format!("running {running}/{}", task.req.attempts)
                }
                Phase::Applying => "applying".to_owned(),
                Phase::Confirming | Phase::Deciding => "waiting".to_owned(),
            },
            (None, None) => "idle".to_owned(),
        };
        format!("molt · {state}")
    }

    /// What a first Ctrl+C or Ctrl+D says: press again, and what quitting keeps.
    pub(super) fn quit_warning(&self, keys: &str) -> String {
        let mut parts = vec![format!("{keys} again to quit")];
        if self.busy() {
            let resumable = self.task.as_ref().is_some_and(|t| t.phase == Phase::Running && t.req.checkpoint);
            parts.push(
                if resumable {
                    "the running task stops; /runs resume goes on with it next time"
                } else {
                    "the running task stops"
                }
                .into(),
            );
        }
        let keep = self.keep().len();
        match keep {
            0 => {}
            1 => parts.push("1 result stays in its fork".into()),
            n => parts.push(format!("{n} results stay in their forks")),
        }
        if !self.queue.is_empty() {
            parts.push(format!("{} queued tasks will not run; their text is printed after exit", self.queue.len()));
        }
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn quitting_waits_for_an_uninterruptible_git_operation_to_settle() {
        let mut app = queue_app();
        type_in(&mut app, "/undo");
        assert!(app.update(ctrl_key('d')).is_empty() && !app.quit);
        assert!(app.update(Msg::Signal).is_empty() && !app.quit);
        assert!(type_in(&mut app, "/queue add not executed").is_empty());
        let actions = app.update(Msg::Undone(Err("no run to undo".into())));
        assert!(actions.iter().any(|action| matches!(action, Action::Quit { .. })));
        assert!(app.quit && app.unexecuted() == ["not executed"]);
    }

    #[test]
    fn the_user_is_called_once_for_each_stop_and_the_title_says_what_waits() {
        let mut app = App::new("/work/project", None);
        assert_eq!(app.title(), "molt · idle");
        up(&mut app);
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "accept trailing commas");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        assert_eq!(app.title(), "molt · designing the check");
        look_away(&mut app);
        let actions = app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()), existing: vec![] });
        assert_eq!(alerts(&actions), ["molt: check ready"]);
        assert_eq!(app.title(), "molt · waiting: check");
        look_away(&mut app);
        assert!(alerts(&app.update(Msg::Tick)).is_empty(), "the same stop calls once");

        app.update(key(KeyCode::Enter));
        assert_eq!(app.title(), "molt · running 0/2");
        app.update(Msg::Progress(Progress::AttemptStarted { run: trace.to_string(), attempt: 0, fork: None }));
        assert_eq!(app.title(), "molt · running 1/2");
        look_away(&mut app);
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/fork"))) });
        assert_eq!(alerts(&actions), ["molt: result ready"], "by default an ended task does not ring");
        assert_eq!(app.title(), "molt · waiting: apply");

        app.update(key(KeyCode::Char('n')));
        assert_eq!(app.title(), "molt · idle");
        look_away(&mut app);
        assert_eq!(alerts(&app.update(Msg::SessionFailed("gone".into()))), ["molt: services down"]);
        assert_eq!(app.title(), "molt · services down");
    }

    #[test]
    fn alerts_wait_for_the_user_to_look_away_and_can_be_turned_off_or_up() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let run = |app: &mut App, outcome: Result<RunResponse, String>| {
            let actions = type_in(app, "task");
            let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
            let trace = trace.clone();
            look_away(app);
            let actions = app.update(Msg::Ran { trace, result: outcome });
            app.prompt = None;
            app.task = None;
            alerts(&actions).into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        app.mode = Mode::Plan;
        assert!(run(&mut app, Ok(passed(false, Some("/d/a")))).is_empty());
        type_in(&mut app, "/notify all");
        assert_eq!(run(&mut app, Ok(passed(false, Some("/d/b")))), ["molt: passed · 1 file"]);
        assert_eq!(run(&mut app, Ok(failed(vec![]))), ["molt: failed"]);
        assert_eq!(run(&mut app, Err("boom".into())), ["molt: run failed"]);
        // Not while the user is at the terminal: just typed, or it has focus.
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(alerts(&app.update(Msg::Ran { trace: trace.clone(), result: Err("x".into()) })).is_empty());
        app.update(Msg::Focus(true));
        assert!(run(&mut app, Err("boom".into())).is_empty());
        app.update(Msg::Focus(false));
        type_in(&mut app, "/notify off");
        assert!(run(&mut app, Err("boom".into())).is_empty());
    }

    #[test]
    fn quitting_takes_a_second_press_soon_after_the_first() {
        let mut app = App::new("/w", None);
        up(&mut app);
        assert!(app.update(ctrl_key('d')).is_empty());
        assert_eq!(app.flash.as_deref(), Some("ctrl+d again to quit"));
        assert!(!app.quit);
        // Too late: the second press only warns again.
        for _ in 0..=QUIT_TICKS {
            app.update(Msg::Tick);
        }
        assert!(app.update(ctrl_key('c')).is_empty());
        assert_eq!(app.flash.as_deref(), Some("ctrl+c again to quit"));
        assert_eq!(app.update(ctrl_key('c')), [Action::Quit { keep: vec![] }]);

        // Another key in between starts over.
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(ctrl_key('d'));
        app.update(key(KeyCode::Left));
        assert!(app.update(ctrl_key('d')).is_empty());
        // A signal still quits at once.
        assert_eq!(app.update(Msg::Signal), [Action::Quit { keep: vec![] }]);
    }

    #[test]
    fn the_status_bar_names_the_resolved_default_model_and_its_sides_never_overlap_at_80_columns() {
        let mut app = App::new("/work/project", None);
        app.update(Msg::SessionUp { versions: vec![], memory_problem: None, default_model: Some("grok-4.5".into()) });
        let status = |app: &mut App| frame_rows_sized(app, 80, 24).pop().unwrap();
        assert!(status(&mut app).contains("grok-4.5 · $0.00"), "{}", status(&mut app));
        assert!(!status(&mut app).contains("default model"));
        // A model the user picked is named instead.
        app.settings.model = Some("haiku".into());
        assert!(status(&mut app).contains("haiku · $0.00") && !status(&mut app).contains("grok"));

        // Both sides long: the left is cut where the right begins, a space between them.
        app.settings.check = Some("grep -q '/// Split' src/lib.rs && cargo test --doc".into());
        app.settings.attempts = Some(3);
        app.settings.budget_usd = Some(2.0);
        app.learning.insert("trace_learn".into());
        app.earlier.push(Earlier {
            task: "fix parser".into(),
            outcome: Outcome::Passed,
            summary: "Parsed.".into(),
            changes: vec![],
            applied: true,
        });
        let row = status(&mut app);
        let right = row.find(['◐', '◓', '◑', '◒']).expect("the right side is shown");
        assert!(row.trim_end().ends_with("learning  1 earlier task · /new") && !row.contains("/help"), "{row}");
        let left = row[..right].trim_end();
        assert!(row[..right].ends_with(' ') && left.ends_with('…'), "{row}");
        let cut = left.trim_end_matches('…');
        let full = " confirm (shift+tab)  haiku · 3 attempts · check: grep -q '/// Split' src/l… · $0.00 · $2.00/task";
        assert!(full.starts_with(cut) && cut.len() > 20, "{row}");

        // A long warning takes no more than half the bar when the left needs the room.
        app.flash = Some("the gateway is retrying a call after a 529 overloaded reply; /log shows more".into());
        let row = status(&mut app);
        assert_eq!(row.chars().count(), 80);
        let warning = row.find("the gateway").expect("the warning starts in view");
        assert!(row[..warning].ends_with(' ') && row[..warning].trim_end().ends_with('…'), "{row}");
        assert!(row.trim_end().ends_with('…') && row[warning..].chars().count() <= 40, "{row}");
        // And all of the bar when the left leaves it.
        app.settings = Settings::default();
        app.flash = Some("ctrl+d again to quit".into());
        assert!(status(&mut app).ends_with(" ctrl+d again to quit "), "{}", status(&mut app));
    }
}
