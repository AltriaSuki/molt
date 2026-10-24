//! The slash commands, and what each one does.
use super::*;

impl App {
    pub(super) fn command(&mut self, command: &str) -> Vec<Action> {
        let (name, arg) = command.split_once(' ').map_or((command, ""), |(n, a)| (n, a.trim()));
        let set = |what: &str, value: &str| format!("{what}: {value}");
        use crate::tui::commands::{builtin, Builtin};
        match builtin(name).map(|c| c.kind) {
            Some(Builtin::Restart) => {
                if self.task.is_some()
                    || self.shell.is_some()
                    || !self.learning.is_empty()
                    || self.git_busy
                    || self.kept_pending.is_some()
                    || self.fork_operations > 0
                {
                    self.flash = Some(
                        "finish or stop the task or command and wait for learning, applies, discards and commits before /restart"
                            .into(),
                    );
                    return Vec::new();
                }
                if matches!(self.session, SessionState::Starting) {
                    self.flash = Some("the services are already starting; /log shows their output".into());
                    return Vec::new();
                }
                self.retry_at = None;
                self.startup_retries = 0;
                self.session = SessionState::Starting;
                self.push(Line::from(span("restarting the services", Tone::Dim)));
                return vec![Action::RestartSession { keep: self.kept.clone() }];
            }
            Some(Builtin::Undo) => {
                if self.task.is_some()
                    || !self.learning.is_empty()
                    || self.git_busy
                    || self.fork_operations > 0
                    || !matches!(self.session, SessionState::Up { .. })
                {
                    self.flash = Some("finish the task and wait for idle services before /undo".into());
                    return Vec::new();
                }
                if arg.split_whitespace().count() > 1 {
                    self.flash = Some("/undo [RUN]: give one run id or leave it empty for the latest run".into());
                    return Vec::new();
                }
                return vec![Action::Undo { run: (!arg.is_empty()).then(|| arg.to_owned()) }];
            }
            Some(Builtin::Queue) => return self.queue_command(arg),
            Some(Builtin::Kept) => {
                self.kept_view = Some(0);
            }
            Some(Builtin::Log) => {
                self.viewer = Some(Viewer {
                    title: "service log".into(),
                    lines: block("reading the service log", "", Tone::Dim),
                    scroll: 0,
                });
                return vec![Action::ShowLog];
            }

            Some(Builtin::Reset) => {
                self.mode = self.preference_defaults.mode;
                self.settings = self.preference_defaults.settings.clone();
                self.reset_preferences = true;
                self.flash = Some("settings reset to the configured defaults".into());
            }

            Some(Builtin::HistoryClear) => {
                self.history.clear();
                self.history_at = None;
                self.history_draft = None;
                self.history_changes.clear();
                self.history_changes.push(crate::tui::state::HistoryChange::Clear);
                self.flash = Some("history cleared for this workspace".into());
            }
            Some(Builtin::MemoryUsed) => {
                let run = if arg.is_empty() { self.last_run.clone() } else { Some(arg.to_owned()) };
                return self.open_used(run);
            }
            Some(Builtin::Theme) => {
                if arg.is_empty() {
                    self.flash = Some(format!("theme: {}; /theme dark|light|ansi", self.theme.name()));
                } else if let Some(theme) = text::Theme::parse(arg) {
                    self.theme = theme;
                    self.flash = Some(format!("theme: {}", theme.name()));
                    return vec![Action::SaveTheme(theme), Action::Redraw];
                } else {
                    self.flash = Some("theme: dark, light or ansi".into());
                }
            }
            Some(Builtin::Help) => {
                self.viewer = Some(Viewer { title: "help".into(), lines: self.help(), scroll: 0 });
            }
            Some(Builtin::Memory) => {
                if !self.memory_up() {
                    self.flash = Some("memory is not running".into());
                    return Vec::new();
                }
                let query = arg.to_owned();
                self.screen = Screen::Memory(MemoryView {
                    query: query.clone(),
                    notes: Vec::new(),
                    selected: 0,
                    loading: true,
                    error: None,
                    edit: None,
                });
                return vec![Action::LoadNotes { query }];
            }
            Some(Builtin::Remember) => {
                if !self.memory_up() {
                    self.flash = Some("memory is not running".into());
                    return Vec::new();
                }
                if arg.is_empty() {
                    self.flash = Some("/remember TEXT: a preference of yours, for every project".into());
                    return Vec::new();
                }
                return vec![Action::Remember { text: arg.to_owned() }];
            }
            Some(Builtin::Check) if self.running() => {
                if arg.is_empty() {
                    self.flash = Some("the running task keeps a check; give the command to use instead".into());
                    return Vec::new();
                }
                self.echo_constraint(&format!("check: {arg}"));
                return self
                    .steer(Control::Constrain(Constraint { check: Some(arg.to_owned()), ..Default::default() }));
            }
            Some(Builtin::Protect) => {
                let paths: Vec<String> = arg.split_whitespace().map(str::to_owned).collect();
                if paths.is_empty() {
                    self.flash = Some("protect: the files or directories to leave as they are".into());
                    return Vec::new();
                }
                if !self.running() {
                    self.flash = Some("no task is running; say what to leave alone in the task".into());
                    return Vec::new();
                }
                self.echo_constraint(&format!("leave {} unchanged", paths.join(", ")));
                return self.steer(Control::Constrain(Constraint { protect: paths, ..Default::default() }));
            }
            Some(Builtin::Runs) => return self.runs_command(arg),
            Some(Builtin::Pause) => return self.steer(Control::Pause),
            Some(Builtin::Resume) => return self.steer(Control::Resume),
            Some(Builtin::New) => {
                self.earlier.clear();
                self.continuation = None;
                self.flash = Some("a new conversation: the next task starts without the earlier ones".into());
            }
            Some(kind @ (Builtin::Ask | Builtin::Quick | Builtin::Verify)) => {
                let name = match kind {
                    Builtin::Ask => workflow::ASK,
                    Builtin::Quick => workflow::QUICK,
                    _ => workflow::VERIFY,
                };
                return self.with_workflow(name, arg);
            }
            Some(Builtin::Continue) => return self.continue_task(arg),
            Some(Builtin::Plan) => return self.plan_command(arg),
            Some(Builtin::Check) => {
                self.settings.check = (!arg.is_empty()).then(|| arg.to_owned());
                self.flash = Some(set("check", self.settings.check.as_deref().unwrap_or("designed for each task")));
            }
            Some(Builtin::Attempts) => match arg.parse::<u32>() {
                Ok(n @ 1..=8) => {
                    self.settings.attempts = Some(n);
                    self.flash = Some(set("attempts", arg));
                }
                _ if arg == "default" || arg.is_empty() => {
                    self.settings.attempts = None;
                    self.flash = Some(set("attempts", "default"));
                }
                _ => self.flash = Some("attempts: 1 to 8".into()),
            },
            Some(Builtin::Commit) => {
                match arg {
                    "on" => self.settings.commit = true,
                    "off" | "default" | "" => self.settings.commit = false,
                    _ => {
                        self.flash = Some("commit: on or off".into());
                        return Vec::new();
                    }
                }
                self.flash = Some(format!("commit: {} (next task)", if self.settings.commit { "on" } else { "off" }));
            }
            Some(Builtin::Stream) => {
                match arg {
                    "on" | "default" | "" => self.settings.stream = true,
                    "off" => self.settings.stream = false,
                    _ => {
                        self.flash = Some("stream: on or off".into());
                        return Vec::new();
                    }
                }
                self.flash = Some(format!("stream: {} (next task)", if self.settings.stream { "on" } else { "off" }));
            }
            Some(Builtin::Notify) => {
                self.settings.notify = match arg {
                    "on" | "default" | "" => Notify::Waiting,
                    "all" => Notify::All,
                    "off" => Notify::Off,
                    _ => {
                        self.flash = Some("notify: on, all or off".into());
                        return Vec::new();
                    }
                };
                self.flash = Some(
                    match self.settings.notify {
                        Notify::Off => "notify: off",
                        Notify::Waiting => "notify: when waiting for you",
                        Notify::All => "notify: when waiting for you and when a task ends",
                    }
                    .into(),
                );
            }
            Some(Builtin::Model) => {
                self.settings.model = (!arg.is_empty() && arg != "default").then(|| arg.to_owned());
                self.flash = Some(set("model", self.settings.model.as_deref().unwrap_or("default")));
            }
            Some(Builtin::MemoryModel) => {
                self.settings.memory_model = (!arg.is_empty() && arg != "default").then(|| arg.to_owned());
                let shown = match self.settings.memory_model.as_deref() {
                    None => "default (MOLT_MEMORY_MODEL, else the attempts' model)",
                    Some(agent::SAME_MODEL) => "the attempts' model",
                    Some(m) => m,
                };
                self.flash = Some(set("memory model", shown));
            }
            Some(Builtin::Effort) => {
                if arg.is_empty() || arg == "default" {
                    self.settings.effort = None;
                    self.flash = Some(set("effort", "default"));
                } else {
                    match serde_json::from_value::<Effort>(serde_json::Value::String(arg.to_owned())) {
                        Ok(e) => {
                            self.settings.effort = Some(e);
                            self.flash = Some(set("effort", arg));
                        }
                        Err(_) => self.flash = Some("effort: low, medium, high, xhigh or max".into()),
                    }
                }
            }
            Some(Builtin::Budget) => match arg.trim_start_matches('$').parse::<f64>() {
                Ok(usd) if usd.is_finite() && usd > 0.0 => {
                    self.settings.budget_usd = Some(usd);
                    self.flash = Some(format!("budget: ${usd:.2} per task"));
                }
                _ if arg == "default" || arg.is_empty() => {
                    self.settings.budget_usd = None;
                    self.flash = Some(set("budget", "default"));
                }
                _ => self.flash = Some("budget: a positive amount of US dollars".into()),
            },
            Some(Builtin::DesignBudget) => match arg.trim_start_matches('$').parse::<f64>() {
                Ok(usd) if usd.is_finite() && usd > 0.0 => {
                    self.settings.design_budget_usd = Some(usd);
                    self.flash = Some(format!("design budget: ${usd:.2} per task, out of its budget"));
                }
                _ if arg == "default" || arg.is_empty() => {
                    self.settings.design_budget_usd = None;
                    self.flash = Some(set("design budget", "default"));
                }
                _ => self.flash = Some("design budget: a positive amount of US dollars".into()),
            },
            Some(Builtin::Lanes) => {
                if self.last_lanes.is_empty() {
                    self.flash = Some("no task has finished yet".into());
                } else {
                    self.viewer =
                        Some(Viewer { title: "attempts".into(), lines: lane_lines(&self.last_lanes), scroll: 0 });
                }
            }
            Some(Builtin::Last) => match &self.last {
                Some(last) => self.viewer = Some(diff_viewer(&last.patch, last.truncated)),
                None => self.flash = Some("no task has finished with changes yet".into()),
            },
            Some(Builtin::Diff) => return self.open_changes(),
            Some(Builtin::Clear) => {
                self.log.clear();
                self.scroll = 0;
            }
            Some(Builtin::Quit) => {
                if self.git_busy {
                    self.quit_after_git = true;
                    self.show_unexecuted();
                    self.flash = Some("waiting for Git and memory before quitting".into());
                    return Vec::new();
                }
                if !self.queue.is_empty() {
                    self.quit_armed = Some(self.tick);
                    self.show_unexecuted();
                    self.flash = Some(self.quit_warning("ctrl+d"));
                    return Vec::new();
                }
                self.quit = true;
                return vec![Action::Quit { keep: self.keep() }];
            }
            None => {
                if let Some(template) = self.commands.get(name).cloned() {
                    if self.task.is_some() {
                        self.flash = Some("a task is waiting; project commands start a new task".into());
                        return Vec::new();
                    }
                    let saved = self.settings.clone();
                    self.settings.attempts = saved.attempts.or(template.attempts);
                    self.settings.budget_usd = match (saved.budget_usd, template.budget) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    self.settings.check = saved.check.clone().or(template.check.clone());
                    let expanded = template.expand(arg);
                    if expanded.trim().is_empty() {
                        self.settings = saved;
                        self.flash = Some("project command produced an empty task".into());
                        return Vec::new();
                    }
                    let from_template = saved.check.is_none() && template.check.is_some();
                    let mut actions = self.submit_task(expanded);
                    self.settings = saved;
                    if let Some(task) = self.task.as_mut().filter(|_| from_template) {
                        task.check_from = Some(name.to_owned());
                    }
                    // A repository-supplied check needs a look before any attempt runs.
                    if self.mode == Mode::Confirm {
                        if let Some(task) = &mut self.task {
                            if let Some(check) = &task.req.check {
                                task.phase = Phase::Confirming;
                                task.stops = vec!["project command check needs confirmation".into()];
                                self.prompt = Some(Prompt::Check(DesignResponse {
                                    command: Some(check.clone()),
                                    files: Vec::new(),
                                    suite: None,
                                    rationale: "Check from the project command; baseline not run yet.".into(),
                                    rubric: Vec::new(),
                                    baseline: None,
                                    usage: Default::default(),
                                    cost_usd: 0.0,
                                    uncounted_calls: 0,
                                    remaining_budget_usd: task.req.budget_usd,
                                    error: None,
                                    cancelled: false,
                                    workflow: None,
                                }));
                                actions.clear();
                            }
                        }
                    }
                    return actions;
                }
                self.flash = Some(format!("no command /{}; /help lists them", clean_line(name)));
            }
        }
        Vec::new()
    }

    fn help(&self) -> Vec<Line<'static>> {
        let mut items = help_items();
        if !self.commands.is_empty() {
            items.push(Help::Head("project commands"));
            for (name, c) in &self.commands {
                let label = match clean_line(&c.hint) {
                    hint if hint.trim().is_empty() => format!("/{name}"),
                    hint => format!("/{name} {hint}"),
                };
                items.push(Help::Row(label, clean_line(&c.description)));
            }
        }
        help_lines(&items)
    }
}

/// The widest the key column of `/help` and the shortcuts gets; a longer
/// key or command has a line of its own, its description under the column.
const KEY_COLUMN: usize = 24;

/// A line of `/help`.
pub(super) enum Help {
    Head(&'static str),
    Row(String, String),
    Blank,
}

/// Keys or commands and what they do, the descriptions lined up in a column
/// as wide as the longest key that fits [`KEY_COLUMN`], at least a space
/// after every key.
pub(super) fn help_lines(items: &[Help]) -> Vec<Line<'static>> {
    let column = items
        .iter()
        .filter_map(|item| match item {
            Help::Row(key, _) => Some(text::width(key)),
            _ => None,
        })
        .filter(|&w| w <= KEY_COLUMN)
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for item in items {
        match item {
            Help::Head(head) => lines.push(Line::from(span(*head, Tone::Dim))),
            Help::Blank => lines.push(Line::default()),
            Help::Row(key, what) if text::width(key) <= column => {
                let pad = " ".repeat(column - text::width(key) + 1);
                lines
                    .push(Line::from(vec![span(format!("  {key}{pad}"), Tone::Shell), span(what.clone(), Tone::Plain)]))
            }
            Help::Row(key, what) => {
                lines.push(Line::from(span(format!("  {key}"), Tone::Shell)));
                lines
                    .push(Line::from(vec![span(" ".repeat(column + 3), Tone::Plain), span(what.clone(), Tone::Plain)]));
            }
        }
    }
    lines
}

fn help_items() -> Vec<Help> {
    let row = |keys: &str, what: &str| Help::Row(keys.to_owned(), what.to_owned());
    let mut items = vec![
        Help::Head("tasks"),
        row("enter", "run the task in the input"),
        row("shift+tab", "mode: plan only, confirm, auto"),
        row("tab", "complete a picked suggestion; otherwise show one attempt large"),
        row("/", "commands and argument hints; arrows pick, Tab completes"),
        row("@path", "point the task at a file; arrows pick, Tab completes"),
        row("? (empty input)", "shortcuts for the current screen"),
        row("ctrl+o", "the transcript: model text, tool arguments and results (o: of the attempt shown large)"),
        row("esc twice", "stop the task"),
        row("enter (running)", "add what you typed to the running task"),
        row("pgup / pgdn", "scroll the log"),
        row("ctrl+home / end", "the start or end of the log"),
        row("shift+enter", "a new line in the task; also ctrl+j, or \\ then enter"),
        row("ctrl+l", "clear and redraw the screen"),
        row("ctrl+z", "suspend the interface; fg returns, services keep running"),
        row("ctrl+g", "write the task in $VISUAL or $EDITOR"),
        row("ctrl+r", "search input history; again: older match; Enter runs; Tab/Esc edits; Ctrl+C cancels"),
        row("↑ / ↓", "the line above or below, then earlier inputs"),
        row("ctrl+d twice", "quit"),
        Help::Blank,
        Help::Head("commands"),
    ];
    items.extend(crate::tui::commands::BUILTINS.iter().map(|c| row(&c.label(), c.description)));
    items.extend([
        Help::Blank,
        Help::Head("modes"),
        row("plan only", "results stay in their forks; the workspace is not changed"),
        row("confirm", "stop for the check or the result when one needs a look"),
        row("auto", "design the check and apply what passes"),
    ]);
    items
}

#[cfg(test)]
pub(super) fn help() -> Vec<Line<'static>> {
    help_lines(&help_items())
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn undo_waits_for_idle_services_and_keeps_partial_memory_settlement_retryable() {
        let mut app = queue_app();
        type_in(&mut app, "busy");
        assert!(type_in(&mut app, "/undo").is_empty());
        app.task = None;
        app.learning.insert("trace_learn".into());
        assert!(type_in(&mut app, "/undo").is_empty());
        app.learning.clear();
        assert_eq!(type_in(&mut app, "/undo"), [Action::Undo { run: None }]);
        assert!(app.git_busy);
        assert!(type_in(&mut app, "another task").is_empty());
        app.input.take();
        type_in(&mut app, "/queue add after undo");
        let actions = app.update(Msg::Undone(Ok(crate::session::Undone {
            git: crate::git::Undo::Reverted {
                run: "trace_test".into(),
                commit: "0123456789abcdef".into(),
                sha: "fedcba9876543210".into(),
            },
            memory: Some(Err("memory unavailable".into())),
        })));
        assert!(actions.is_empty() && !app.git_busy && app.queue_paused);
        assert!(log(&app).contains("Git is undone, but memory could not be told"));
        assert_eq!(type_in(&mut app, "/undo trace_test"), [Action::Undo { run: Some("trace_test".into()) }]);
        app.update(Msg::Undone(Ok(crate::session::Undone {
            git: crate::git::Undo::Already { run: "trace_test".into(), sha: "fedcba9876543210".into() },
            memory: Some(Ok(molt_api::memory::RetractResponse { retracted: 1, adjusted: 0 })),
        })));
        assert!(!app.git_busy && log(&app).contains("memory no longer counts the run"));
        assert!(
            matches!(type_in(&mut app, "/queue resume").as_slice(), [Action::Run { req, .. }] if req.task == "after undo")
        );
    }

    #[test]
    fn every_builtin_is_parsed_documented_and_reserves_its_name() {
        let mut names = HashSet::new();
        let help = plain(&help()).join("\n");
        for definition in crate::tui::commands::BUILTINS {
            assert!(names.insert(definition.name), "duplicate {}", definition.name);
            assert!(help.contains(&definition.label()), "{} missing from help", definition.name);
            assert!(help.contains(definition.description));
            let mut app = App::new("/w", None);
            up(&mut app);
            let arg = definition.values.first().copied().unwrap_or("");
            app.command(&format!("{} {arg}", definition.name));
            assert!(!app.flash.as_deref().is_some_and(|s| s.starts_with("no command")), "{}", definition.name);
        }
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".molt/commands");
        std::fs::create_dir_all(&dir).unwrap();
        for name in &names {
            std::fs::write(dir.join(format!("{name}.md")), "conflict").unwrap();
        }
        let (projects, warnings) = crate::tui::commands::load(root.path());
        assert!(projects.is_empty());
        assert_eq!(warnings.len(), names.len());
    }

    #[test]
    fn project_commands_set_one_request_and_confirm_their_check() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".molt/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fix-issue.md"),
            "---\ndescription: Fix an issue\nargument-hint: ISSUE\ncheck: cargo test\nattempts: 3\nbudget: 2.5\n---\nFix issue $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(dir.join("help.md"), "wrong").unwrap();
        let mut app = App::new(root.path().to_str().unwrap(), None);
        app.load_commands(root.path());
        up(&mut app);
        app.settings.budget_usd = Some(1.0);
        app.update(Msg::Paste("/fix-".into()));
        let frame = frame_text(&mut app);
        assert!(frame.contains("/fix-issue ISSUE") && frame.contains("Fix an issue"));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "/fix-issue ");
        assert!(type_in(&mut app, "12").is_empty());
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.req.task, "Fix issue 12");
        assert_eq!(task.req.attempts, 3);
        assert_eq!(task.req.budget_usd, Some(1.0));
        assert_eq!(task.phase, Phase::Confirming);
        assert_eq!(app.settings.check, None);
        let actions = app.update(key(KeyCode::Enter));
        assert!(matches!(&actions[0], Action::Run { req, .. } if req.check.as_deref() == Some("cargo test")));
        app.task = None;
        type_in(&mut app, "/help");
        let viewer = app.viewer.as_ref().unwrap();
        assert_eq!(viewer.title, "help");
        assert!(plain(&viewer.lines).iter().any(|l| l.contains("/fix-issue")));
    }

    #[test]
    fn commands_change_the_next_task() {
        let mut app = App::new("/w", None);
        up(&mut app);
        for c in ["/attempts 4", "/model sonnet", "/effort high", "/budget 2.5", "/check make test"] {
            assert!(type_in(&mut app, c).is_empty());
        }
        assert_eq!(app.settings.attempts, Some(4));
        assert_eq!(app.settings.effort, Some(Effort::High));
        type_in(&mut app, "/attempts 9");
        assert_eq!(app.flash.as_deref(), Some("attempts: 1 to 8"));
        // With a check of the user's, nothing is designed.
        let actions = type_in(&mut app, "task");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!((req.attempts, req.model.as_deref(), req.budget_usd), (4, Some("sonnet"), Some(2.5)));
        assert_eq!(req.check.as_deref(), Some("make test"));
        // While it runs, what is typed goes to it.
        assert!(matches!(type_in(&mut app, "another").as_slice(), [Action::Control { .. }]));
        assert!(app.input.text.is_empty());
        assert_eq!(type_in(&mut app, "/quit"), [Action::Quit { keep: vec![] }]);
    }

    #[test]
    fn every_help_line_at_80_columns_leaves_a_space_between_command_and_description() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.commands.insert(
            "release-notes-for-the-next-version".into(),
            crate::tui::commands::Command {
                hint: "[VERSION]".into(),
                description: "draft the release notes".into(),
                template: "notes".into(),
                ..Default::default()
            },
        );
        type_in(&mut app, "/help");
        let rows = frame_rows_sized(&mut app, 80, 200);
        let rows_of = |label: &str, what: &str| {
            let at = rows.iter().position(|r| r.contains(&format!("  {label} "))).unwrap_or_else(|| {
                panic!("{label} is not on a line of its own or followed by a space:\n{}", rows.join("\n"))
            });
            let words: String = what.split(' ').take(2).collect::<Vec<_>>().join(" ");
            let after = &rows[at][rows[at].find(label).unwrap() + label.len()..];
            assert!(
                after.trim_start().starts_with(&words)
                    || rows[at + 1].trim_start_matches(['│', ' ']).starts_with(&words),
                "{label}: {what} is neither after it nor on the next line:\n{}\n{}",
                rows[at],
                rows[at + 1]
            );
        };
        for definition in crate::tui::commands::BUILTINS {
            rows_of(&definition.label(), definition.description);
        }
        rows_of("/release-notes-for-the-next-version [VERSION]", "draft the release notes");
        rows_of("? (empty input)", "shortcuts for the current screen");
        // The descriptions share one column; only a key too long for it has a line of its own.
        let column = |label: &str| {
            let row = rows.iter().find(|r| r.contains(&format!("  {label} "))).unwrap();
            let start = row.find(label).unwrap() + label.len();
            row[..start].chars().count() + row[start..].chars().take_while(|&c| c == ' ').count()
        };
        assert_eq!(column("/memory-used [run]"), column("/notify on|all|off"));
        assert_eq!(column("/memory-model NAME"), column("enter"));
    }

    #[test]
    fn a_project_commands_check_is_labelled_with_the_command_until_the_user_edits_it() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.commands.insert(
            "doc".into(),
            crate::tui::commands::Command {
                template: "document $ARGUMENTS".into(),
                check: Some("cargo doc --no-deps".into()),
                ..Default::default()
            },
        );
        assert!(type_in(&mut app, "/doc the parser").is_empty());
        let frame = frame_text(&mut app);
        assert!(frame.contains("◆ check  from /doc"), "{frame}");
        assert!(!frame.contains("designed"), "{frame}");
        app.update(key(KeyCode::Char('e')));
        app.input.take();
        type_in(&mut app, "cargo doc");
        assert!(!frame_text(&mut app).contains("from /doc"));
        // Run without stopping, the planner reports a check it did not design.
        app.task = None;
        app.prompt = None;
        app.mode = Mode::Auto;
        let actions = type_in(&mut app, "/doc the lexer");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        app.update(Msg::Progress(Progress::CheckReady {
            run: trace.to_string(),
            command: Some("cargo doc --no-deps".into()),
            files: vec![],
            designed: false,
            rubric: vec![],
        }));
        assert!(log(&app).contains("◆ check  from /doc"), "{}", log(&app));
        // A check of the user's own wins over the template's, and is theirs.
        app.task = None;
        app.settings.check = Some("make docs".into());
        let actions = type_in(&mut app, "/doc the lexer");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        app.update(Msg::Progress(Progress::CheckReady {
            run: trace.to_string(),
            command: Some("make docs".into()),
            files: vec![],
            designed: false,
            rubric: vec![],
        }));
        assert!(log(&app).ends_with("◆ check  yours\n  $ make docs"), "{}", log(&app));
    }
}
