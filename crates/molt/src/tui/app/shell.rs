//! Commands the user runs from the input (`!cmd`): through the shell
//! service, as the attempts' commands run, with their output in the log.
//! No model is called, and nothing is spent.
use super::*;

use molt_api::shell::RunResponse;

/// Lines of each stream shown from the reply when the live output stopped
/// short or never came.
const TAIL_LINES: usize = 20;

/// How the shell service runs commands, as the config says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sandbox {
    /// In the sandbox the attempts' commands run in.
    Isolated,
    /// Unconfined: the user chose `--no-sandbox`.
    Unconfined,
    /// A shell service of the config's own, which says nothing about it.
    Unknown,
}

impl Sandbox {
    fn label(self) -> &'static str {
        match self {
            Self::Isolated => "in the sandbox",
            Self::Unconfined => "unconfined (--no-sandbox)",
            Self::Unknown => "by the configured shell service",
        }
    }
}

/// The user's command while it runs, and until its end is in the log.
pub struct ShellRun {
    pub trace: TraceId,
    pub command: String,
    /// The number of the next output event; a higher one means some were missed.
    next: u64,
    /// Some output came live.
    live: bool,
    /// The live output stopped at the shell's limit.
    cut: bool,
    pub stopping: bool,
    /// The reply, held for a tick: output published just before it may
    /// still be on its way, and goes first.
    reply: Option<Result<RunResponse, String>>,
}

impl ShellRun {
    /// Running, its reply not come yet.
    pub fn running(&self) -> bool {
        self.reply.is_none()
    }
}

impl App {
    /// `!command`: run it in a fork of the workspace through the shell service.
    pub(super) fn run_command(&mut self, command: &str) -> Vec<Action> {
        let command = command.trim();
        if command.is_empty() {
            self.flash = Some("!COMMAND runs a shell command in a fork of the workspace".into());
            return Vec::new();
        }
        if self.shell.is_some() {
            self.flash = Some("a command is running; Esc stops it".into());
            self.input = Input::with(&format!("!{command}"));
            return Vec::new();
        }
        if !matches!(self.session, SessionState::Up { .. }) {
            self.flash = Some("the services are not up".into());
            self.input = Input::with(&format!("!{command}"));
            return Vec::new();
        }
        let trace = TraceId::random();
        self.gap();
        for (i, l) in text::clean(command).into_iter().enumerate() {
            let mark = if i == 0 { span("! ", Tone::Shell) } else { span("  ", Tone::Plain) };
            self.push(Line::from(vec![mark, span(l, Tone::Plain)]));
        }
        self.push(Line::from(span(
            format!(
                "  {}, in a fork of the workspace whose changes are not kept · no model call · esc stops it",
                self.sandbox.label()
            ),
            Tone::Dim,
        )));
        self.shell = Some(ShellRun {
            trace: trace.clone(),
            command: command.to_owned(),
            next: 0,
            live: false,
            cut: false,
            stopping: false,
            reply: None,
        });
        vec![Action::Command { command: command.to_owned(), trace }]
    }

    /// Esc while the command runs.
    pub(super) fn stop_command(&mut self) -> Vec<Action> {
        let Some(run) = self.shell.as_mut().filter(|run| run.running()) else { return Vec::new() };
        if run.stopping {
            self.flash = Some("stopping the command".into());
            return Vec::new();
        }
        run.stopping = true;
        let trace = run.trace.clone();
        self.push(Line::from(span("  stopping…", Tone::Dim)));
        vec![Action::StopCommand { trace }]
    }

    pub(super) fn command_output(&mut self, event: Progress) {
        let Progress::Output { run, seq, text, stderr, cut } = event else { return };
        let Some(shell) = self.shell.as_mut().filter(|s| s.trace.as_str() == run) else { return };
        if seq < shell.next || shell.cut {
            return;
        }
        let gap = seq > shell.next;
        shell.next = seq + 1;
        shell.live = true;
        shell.cut |= cut;
        if gap {
            self.push(Line::from(span("  [some output was not shown]", Tone::Dim)));
        }
        if cut {
            self.push(Line::from(span("  [live output stops here; its end follows when the command ends]", Tone::Dim)));
            return;
        }
        let tone = if stderr { Tone::Dim } else { Tone::Plain };
        let text = scrub(text.trim_end_matches(['\n', '\r']), &self.secrets);
        for l in text::clean(&text) {
            self.push(Line::from(vec![span("  ", Tone::Plain), span(l, tone)]));
        }
    }

    pub(super) fn command_ran(&mut self, trace: TraceId, result: Result<RunResponse, String>) {
        if let Some(shell) = self.shell.as_mut().filter(|s| s.trace == trace && s.reply.is_none()) {
            shell.reply = Some(result);
        }
    }

    /// On a tick: write the end of a command whose reply came.
    pub(super) fn command_settle(&mut self) {
        if self.shell.as_ref().is_some_and(|s| s.reply.is_some()) {
            let shell = self.shell.take().unwrap();
            self.command_end(shell);
        }
    }

    /// The services stopped: a command whose reply had not come ended with them, how is not known.
    pub(super) fn command_lost(&mut self) {
        match self.shell.take() {
            Some(shell) if shell.reply.is_some() => self.command_end(shell),
            Some(shell) => {
                let line = format!("  ? the services stopped; how `{}` ended is not known", clean_line(&shell.command));
                self.push(Line::from(span(line, Tone::Warn)));
            }
            None => {}
        }
    }

    fn command_end(&mut self, shell: ShellRun) {
        let resp = match shell.reply {
            Some(Ok(resp)) => resp,
            Some(Err(e)) => {
                self.error("the command did not run", &scrub(&e, &self.secrets));
                return;
            }
            None => return,
        };
        if !shell.live || shell.cut {
            // Live output never came (a shell that does not stream) or stopped short: the reply's end shows.
            for (output, tone) in [(&resp.stdout, Tone::Plain), (&resp.stderr, Tone::Dim)] {
                let text = scrub(output.trim_end(), &self.secrets);
                let lines = text::clean(&text);
                let skip = if shell.cut { lines.len().saturating_sub(TAIL_LINES) } else { 0 };
                for l in lines.into_iter().skip(skip).filter(|_| !text.is_empty()) {
                    self.push(Line::from(vec![span("  ", Tone::Plain), span(l, tone)]));
                }
            }
        }
        let took = text::duration(Duration::from_millis(resp.duration_ms));
        let (mark, how, tone) = if resp.cancelled {
            ("■", "stopped".to_owned(), Tone::Warn)
        } else if resp.timed_out {
            ("✗", "timed out".to_owned(), Tone::Bad)
        } else {
            match (resp.exit_code, resp.signal) {
                (Some(0), _) => ("✓", "exit 0".to_owned(), Tone::Ok),
                (Some(code), _) => ("✗", format!("exit {code}"), Tone::Bad),
                (None, Some(signal)) => ("✗", format!("killed by signal {signal}"), Tone::Bad),
                (None, None) => ("✗", "ended without an exit code".to_owned(), Tone::Bad),
            }
        };
        let mut line = format!("  {mark} {how} · {took}");
        if resp.truncated {
            line.push_str(" · output cut to its start and end");
        }
        self.push(Line::from(span(line, tone)));
        if resp.offline_failure {
            self.push(Line::from(span(
                "  the sandbox has no network: a dependency it needed could not be downloaded",
                Tone::Warn,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn output(trace: &TraceId, seq: u64, text: &str) -> Msg {
        Msg::Progress(Progress::Output { run: trace.to_string(), seq, text: text.into(), stderr: false, cut: false })
    }

    fn ran(exit: i32, stdout: &str) -> RunResponse {
        RunResponse {
            exit_code: Some(exit),
            signal: None,
            timed_out: false,
            cancelled: false,
            stdout: stdout.into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 1200,
            offline_failure: false,
        }
    }

    fn up_app() -> App {
        let mut app = App::new("/w", None);
        up(&mut app);
        app
    }

    fn started(app: &mut App, command: &str) -> TraceId {
        let actions = type_in(app, command);
        match actions.as_slice() {
            [Action::Command { command: c, trace }] => {
                assert_eq!(c, command.trim_start_matches('!'));
                trace.clone()
            }
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn a_bang_command_runs_through_the_shell_and_its_output_streams_into_the_log() {
        let mut app = up_app();
        app.sandbox = Sandbox::Isolated;
        let trace = started(&mut app, "!echo hi");
        let shown = log(&app);
        assert!(shown.contains("! echo hi") && shown.contains("in the sandbox"), "{shown}");
        assert!(shown.contains("no model call"), "{shown}");
        assert!(app.task.is_none(), "no task, no model");
        app.update(output(&trace, 0, "hi\n"));
        assert!(log(&app).contains("  hi"));
        // Output of another command is not this one's.
        app.update(output(&TraceId::random(), 1, "forged\n"));
        app.update(Msg::CommandRan { trace: trace.clone(), result: Ok(ran(0, "hi\n")) });
        // Output published just before the reply still goes before its end.
        app.update(output(&trace, 1, "late\n"));
        assert!(!log(&app).contains("exit 0"));
        app.update(Msg::Tick);
        let shown = log(&app);
        assert!(!shown.contains("forged"), "{shown}");
        assert_eq!(
            shown.matches("hi\n").count(),
            2,
            "echoed once and its output once, not again from the reply: {shown}"
        );
        assert!(shown.find("late").unwrap() < shown.find("✓ exit 0 · 1s").unwrap(), "{shown}");
        assert!(app.shell.is_none());
        assert_eq!(app.spent_usd, 0.0);
    }

    #[test]
    fn unconfined_commands_say_so_and_missing_output_is_shown_from_the_reply() {
        let mut app = up_app();
        app.sandbox = Sandbox::Unconfined;
        let trace = started(&mut app, "!make");
        assert!(log(&app).contains("unconfined (--no-sandbox)"));
        app.update(output(&trace, 2, "after a gap\n"));
        app.update(Msg::CommandRan { trace: trace.clone(), result: Ok(ran(2, "after a gap\n")) });
        app.update(Msg::Tick);
        let shown = log(&app);
        assert!(shown.contains("[some output was not shown]") && shown.contains("✗ exit 2"), "{shown}");

        // A shell that does not stream: the reply's output shows at the end.
        let trace = started(&mut app, "!ls");
        app.update(Msg::CommandRan { trace, result: Ok(ran(0, "a.txt\nb.txt\n")) });
        app.update(Msg::Tick);
        let shown = log(&app);
        assert!(shown.contains("  a.txt\n  b.txt\n  ✓ exit 0"), "{shown}");
    }

    #[test]
    fn esc_stops_a_running_command_and_only_one_runs_at_a_time() {
        let mut app = up_app();
        let trace = started(&mut app, "!sleep 100");
        assert!(type_in(&mut app, "!echo again").is_empty());
        assert_eq!(app.flash.as_deref(), Some("a command is running; Esc stops it"));
        assert_eq!(app.input.text, "!echo again", "the refused command stays in the input");
        app.input.take();
        assert_eq!(app.update(key(KeyCode::Esc)), [Action::StopCommand { trace: trace.clone() }]);
        assert!(app.update(key(KeyCode::Esc)).is_empty(), "asked once");
        let mut resp = ran(0, "");
        resp.exit_code = None;
        resp.cancelled = true;
        app.update(Msg::CommandRan { trace, result: Ok(resp) });
        app.update(Msg::Tick);
        assert!(log(&app).contains("■ stopped"), "{}", log(&app));
        // With nothing running, Esc does nothing to a command.
        assert!(app.update(key(KeyCode::Esc)).is_empty());
    }

    #[test]
    fn a_command_needs_the_services_and_one_lost_with_them_is_not_called_finished() {
        let mut app = App::new("/w", None);
        assert!(type_in(&mut app, "!true").is_empty());
        assert_eq!(app.flash.as_deref(), Some("the services are not up"));
        let mut app = up_app();
        started(&mut app, "!cargo test");
        app.update(Msg::BackendFailed("gone".into()));
        let shown = log(&app);
        assert!(shown.contains("how `cargo test` ended is not known"), "{shown}");
        assert!(!shown.contains("exit"), "{shown}");
        assert!(app.shell.is_none());
    }
}
