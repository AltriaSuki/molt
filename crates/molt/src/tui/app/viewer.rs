//! Text over everything else: viewers, the transcript of a run, and the
//! shortcuts.
use super::*;

/// A run's model turns, tool calls and results, over everything else.
pub struct TranscriptView {
    pub trace: TraceId,
    /// The attempt it was opened on; `None` shows every turn of the run.
    pub only: Option<u32>,
    pub(super) reader: Reader,
    /// Why the audit log could not be read; the run's progress shows instead.
    pub unreadable: Option<String>,
    pub(super) reading: bool,
    /// Read again: the log has more, or the run moved on.
    pub(super) stale: bool,
    pub(super) read_at: u64,
    pub unfold: bool,
    /// In wrapped rows, clamped when the terminal draws it.
    pub scroll: usize,
    pub lines: Vec<Line<'static>>,
    /// Where each turn starts among the lines.
    pub starts: Vec<usize>,
    pub visual_starts: Vec<usize>,
    pub pending_turn: Option<usize>,
}

/// Text shown over everything else: a diff, a check's files, help.
pub struct Viewer {
    pub title: String,
    pub lines: Vec<Line<'static>>,
    pub scroll: usize,
}

impl App {
    pub(super) fn viewer_key(&mut self, key: &KeyEvent) {
        viewer_key(&mut self.viewer, key);
    }

    pub(super) fn can_show_shortcuts(&self) -> bool {
        if let Some(view) = &self.queue_view {
            if view.draft.as_ref().is_some_and(|input| !input.text.is_empty()) {
                return false;
            }
        }
        if self.viewer.is_some()
            || self.transcript.is_some()
            || self.kept_view.is_some()
            || self.changes.is_some()
            || self.queue_view.is_some()
        {
            return true;
        }
        match &self.screen {
            Screen::Main => self.input.text.is_empty(),
            Screen::Memory(view) => match &view.edit {
                Some(MemoryEdit::Filter(i) | MemoryEdit::Reason(i) | MemoryEdit::Promote(i)) => i.text.is_empty(),
                Some(MemoryEdit::Change(c)) => c.input().is_none_or(|i| i.text.is_empty()),
                None => true,
            },
        }
    }

    pub(super) fn shortcut_lines(&self) -> Vec<Line<'static>> {
        let mut pairs = vec![("? / esc", "close shortcuts")];
        let context;
        if self.viewer.is_some() {
            context = "viewer";
            pairs.extend([("j/k, ↑/↓", "scroll"), ("g/G", "start / end"), ("esc / q", "close viewer")]);
        } else if self.queue_view.is_some() {
            context = "task queue";
            pairs.extend([
                ("a", "add a task with current settings"),
                ("j/k, ↑/↓", "select a task"),
                ("J/K", "move later / earlier"),
                ("n / delete", "remove selected task"),
                ("p / r", "pause / resume queue"),
                ("enter / v", "inspect full task"),
                ("esc / q", "back to tasks"),
            ]);
        } else if self.kept_view.is_some() {
            context = "kept results";
            pairs.extend([
                ("j/k, ↑/↓", "select a result"),
                ("d", "inspect its current diff"),
                ("enter", "apply when idle"),
                ("n", "discard when idle"),
                ("esc / q", "back to tasks"),
            ]);
        } else if self.changes.is_some() {
            context = "changes against HEAD";
            pairs.extend([
                ("j/k, ↑/↓", "select a file"),
                ("enter", "its diff"),
                ("r", "list again"),
                ("esc / q", "back to tasks"),
            ]);
        } else if self.transcript.is_some() {
            context = "transcript";
            pairs.extend([
                ("j/k, ↑/↓", "scroll"),
                ("{/}", "previous / next turn"),
                ("e", "fold / unfold"),
                ("v", "open in editor"),
                ("esc / ctrl+o", "close transcript"),
            ]);
        } else if let Screen::Memory(view) = &self.screen {
            context = "memory";
            if view.edit.is_some() {
                pairs.extend([("enter", "accept the field / save"), ("esc", "cancel editing")]);
                if let Some(MemoryEdit::Change(c)) = &view.edit {
                    pairs.push(("shift+tab", "previous field"));
                    if c.step == ChangeStep::Confirm {
                        pairs.push(("t", "toggle temporary"));
                    }
                }
            } else {
                pairs.extend([
                    ("j/k, ↑/↓", "select a note"),
                    ("/", "filter notes"),
                    ("enter / v", "inspect note"),
                    ("a / c", "accept / correct note"),
                    ("f", "forget with a reason"),
                    ("p", "promote to every project"),
                    ("u", "latest task's recalled notes"),
                    ("r", "reload notes"),
                    ("esc / q", "back to tasks"),
                ]);
            }
        } else {
            context = match &self.prompt {
                Some(Prompt::Check(d)) => {
                    pairs.extend([("enter", "run attempts"), ("esc", "cancel task")]);
                    if d.command.is_some() {
                        pairs.extend([("e", "edit check"), ("n", "run without check")]);
                    }
                    if !d.files.is_empty() {
                        pairs.push(("v", "view check files"));
                    }
                    "check confirmation"
                }
                Some(Prompt::Apply { .. }) => {
                    pairs.extend([
                        ("enter", "apply result"),
                        ("d", "view diff"),
                        ("s", "choose which files to apply, then check them again"),
                        ("e", "edit the result in $EDITOR, then check it again"),
                        ("v", "inspect rules and reviewer findings"),
                        ("k / esc", "keep result in its fork"),
                        ("n", "discard result"),
                    ]);
                    "apply confirmation"
                }
                Some(Prompt::Retry { .. }) => {
                    pairs.extend([("r", "retry with the same check"), ("v", "view attempts"), ("esc", "dismiss")]);
                    "retry"
                }
                Some(Prompt::Plan(_)) => {
                    pairs.extend([("enter", "run the steps"), ("e", "edit the steps"), ("esc", "cancel")]);
                    "plan confirmation"
                }
                Some(Prompt::EditPlan(_)) => {
                    pairs.extend([
                        ("enter", "save the steps"),
                        ("shift+enter / ctrl+j", "new line: one step per line"),
                        ("esc", "back to plan confirmation"),
                    ]);
                    "edit plan"
                }
                Some(Prompt::EditCheck(_)) => {
                    pairs.extend([("enter", "save check command"), ("esc", "back to check confirmation")]);
                    "edit check"
                }
                None => {
                    pairs.extend([
                        ("enter", if self.running() { "add input to the running task" } else { "submit task" }),
                        ("shift+enter / ctrl+j", "new line (also \\ then enter)"),
                        ("shift+tab", "switch mode"),
                        ("tab", "complete suggestion / enlarge attempt"),
                        ("↑/↓", "pick suggestion / move line / input history"),
                        ("ctrl+g", "edit task in editor"),
                        ("ctrl+r", "search this workspace's input history"),
                    ]);
                    if self.busy() {
                        pairs.push(("esc twice", "stop task"));
                    }
                    "tasks"
                }
            };
            pairs.extend([("ctrl+o", "open transcript"), ("pgup / pgdn", "scroll log")]);
        }
        pairs.extend([("ctrl+l", "redraw screen"), ("ctrl+z", "suspend interface"), ("ctrl+d twice", "quit")]);
        let mut items = vec![super::command::Help::Head(context)];
        items.extend(pairs.into_iter().map(|(key, what)| super::command::Help::Row(key.to_owned(), what.to_owned())));
        super::command::help_lines(&items)
    }

    /// The transcript of the running task, or else of the last one; of the
    /// attempt drawn large, if one is.
    pub(super) fn open_transcript(&mut self) -> Vec<Action> {
        let trace = match (&self.task, &self.last_run) {
            (Some(task), _) => task.trace.clone(),
            (None, Some(run)) => TraceId::from_raw(run.clone()),
            (None, None) => {
                self.flash = Some("no task has run yet".into());
                return Vec::new();
            }
        };
        self.transcript = Some(TranscriptView {
            trace,
            only: self.zoom,
            reader: Reader::default(),
            unreadable: None,
            reading: false,
            stale: true,
            read_at: self.tick,
            unfold: false,
            scroll: 0,
            lines: Vec::new(),
            starts: Vec::new(),
            visual_starts: Vec::new(),
            pending_turn: None,
        });
        self.render_transcript();
        Vec::new()
    }

    /// The next read of the open transcript: when it opens, while its run
    /// goes on, and once more when the run ends.
    pub(super) fn read_transcript(&mut self) -> Option<Action> {
        let live = self.task.as_ref().map(|t| &t.trace);
        let view = self.transcript.as_mut()?;
        if view.reading || view.unreadable.is_some() {
            return None;
        }
        let due = live == Some(&view.trace) && self.tick.saturating_sub(view.read_at) >= TRANSCRIPT_TICKS;
        if !view.stale && !due {
            return None;
        }
        (view.reading, view.stale, view.read_at) = (true, false, self.tick);
        Some(Action::ReadAudit { trace: view.trace.clone(), cursor: view.reader.cursor })
    }

    pub(super) fn render_transcript(&mut self) {
        let Some(view) = &mut self.transcript else { return };
        let secrets = &self.secrets;
        let (mut lines, mut starts) =
            transcript::render(&view.reader.turns, view.only, view.unfold, &|s: &str| scrub(s, secrets));
        if view.reader.turns.is_empty() {
            // No turns to show: what the run published, as its lanes have it.
            let note = match &view.unreadable {
                Some(e) => format!("the audit log could not be read ({e}); the tool calls the run published:"),
                None if view.reading && view.reader.cursor.is_none() => "reading the audit log".to_owned(),
                None => "nothing for this run in the audit log yet; the tool calls the run published:".to_owned(),
            };
            lines = vec![Line::from(span(note, Tone::Dim)), Line::default()];
            starts.clear();
            let lanes = match &self.task {
                Some(task) if task.trace == view.trace => &task.lanes,
                _ if self.last_run.as_deref() == Some(view.trace.as_str()) => &self.last_lanes,
                _ => &Vec::new(),
            };
            for lane in lanes.iter().filter(|l| view.only.is_none_or(|a| a == l.index)) {
                starts.push(lines.len());
                lines.extend(lane_lines(std::slice::from_ref(lane)));
            }
        }
        (view.lines, view.starts) = (lines, starts);
        view.visual_starts.clear();
    }

    pub(super) fn transcript_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Some(view) = &mut self.transcript else { return Vec::new() };
        let starts = if view.visual_starts.is_empty() { &view.starts } else { &view.visual_starts };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.transcript = None,
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => self.transcript = None,
            KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => view.scroll = view.scroll.saturating_add(20),
            KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(20),
            KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX,
            KeyCode::Char('}') => {
                if let Some(&next) = starts.iter().find(|&&s| s > view.scroll) {
                    view.scroll = next;
                }
            }
            KeyCode::Char('{') => view.scroll = starts.iter().rev().find(|&&s| s < view.scroll).copied().unwrap_or(0),
            KeyCode::Char('e') => {
                // Stay on the same turn.
                let at = starts.iter().rposition(|&s| s <= view.scroll);
                view.unfold = !view.unfold;
                self.render_transcript();
                let view = self.transcript.as_mut().expect("still open");
                view.pending_turn = at;
                view.scroll = at.and_then(|i| view.starts.get(i).copied()).unwrap_or(0);
            }
            KeyCode::Char('v') => {
                let (lines, _) =
                    transcript::render(&view.reader.turns, view.only, true, &|s: &str| scrub(s, &self.secrets));
                let lines = if view.reader.turns.is_empty() { &view.lines } else { &lines };
                let mut text = format!("molt transcript · run {}\n\n", view.trace);
                for line in lines {
                    text.extend(line.spans.iter().map(|s| s.content.as_ref()));
                    text.push('\n');
                }
                return vec![Action::Export { text }];
            }
            _ => {}
        }
        Vec::new()
    }
}

pub(super) fn diff_viewer(patch: &str, truncated: bool) -> Viewer {
    let mut lines = text::diff_lines(patch);
    if truncated {
        lines.push(Line::from(span("(the diff is cut short here)", Tone::Dim)));
    }
    Viewer { title: "diff".into(), lines, scroll: 0 }
}

pub(super) fn review_viewer(review: Option<&molt_api::planner::Review>, secrets: &[String]) -> Viewer {
    let mut lines = Vec::new();
    let verdict = |v| match v {
        Verdict::Approve => "approve",
        Verdict::Concern => "concern",
        Verdict::Reject => "reject",
    };
    if let Some(review) = review {
        lines.extend(block(&format!("aggregate review: {}", verdict(review.verdict)), "", Tone::Shell));
        if let Some(details) = &review.details {
            lines.extend(block("fixed rules", "", Tone::Shell));
            if details.rules.is_empty() {
                lines.extend(block("no findings", "  ", Tone::Dim));
            }
            for finding in &details.rules {
                lines.extend(block(&scrub(finding, secrets), "  ", Tone::Plain));
            }
            let label = details.reviewer.map_or("no verdict", verdict);
            lines.extend(block(&format!("reviewer: {label}"), "", Tone::Shell));
            for finding in &details.findings {
                lines.extend(block(&scrub(finding, secrets), "  ", Tone::Plain));
            }
            if let Some(error) = &details.error {
                lines.extend(block(&scrub(error, secrets), "  ", Tone::Bad));
            }
        } else {
            lines.extend(block("findings (source not recorded by this planner)", "", Tone::Dim));
            for finding in &review.findings {
                lines.extend(block(&scrub(finding, secrets), "  ", Tone::Plain));
            }
        }
    } else {
        lines.extend(block("no review was requested", "", Tone::Dim));
    }
    Viewer { title: "review details".into(), lines, scroll: 0 }
}

pub(super) fn viewer_key(view: &mut Option<Viewer>, key: &KeyEvent) {
    let Some(viewer) = view else { return };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => *view = None,
        KeyCode::Down | KeyCode::Char('j') => viewer.scroll = viewer.scroll.saturating_add(1),
        KeyCode::Up | KeyCode::Char('k') => viewer.scroll = viewer.scroll.saturating_sub(1),
        KeyCode::PageDown | KeyCode::Char(' ') => viewer.scroll = viewer.scroll.saturating_add(20),
        KeyCode::PageUp => viewer.scroll = viewer.scroll.saturating_sub(20),
        KeyCode::Char('g') | KeyCode::Home => viewer.scroll = 0,
        KeyCode::Char('G') | KeyCode::End => viewer.scroll = usize::MAX,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn end_in_diff_and_transcript_viewers_places_the_final_wrapped_row_at_the_bottom() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        let lines = || {
            let mut lines: Vec<_> = (0..90).map(|i| Line::from(format!("line {i}"))).collect();
            lines[20] = Line::from("long wrapped text ".repeat(100));
            lines.push(Line::from("FINAL VISIBLE ROW"));
            lines
        };
        let last_row = 35; // A 35-row panel in the existing 140 x 40 fixture, minus its border.
        app.viewer = Some(Viewer { title: "diff".into(), lines: lines(), scroll: 0 });
        app.update(key(KeyCode::Char('G')));
        let rows = frame_rows(&mut app, 140);
        assert!(rows[last_row].contains("FINAL VISIBLE ROW"), "{}", rows.join("\n"));
        assert!(!rows[3].trim_matches([' ', '│']).is_empty());
        app.update(key(KeyCode::Esc));

        let trace = TraceId::random();
        app.last_run = Some(trace.to_string());
        app.update(ctrl_key('o'));
        app.update(Msg::Audit {
            trace: trace.clone(),
            result: Ok(AuditPage { entries: transcript::tests::two_turns(&trace), cursor: Some(100), more: false }),
        });
        let view = app.transcript.as_mut().unwrap();
        view.lines = lines();
        view.starts = vec![0, 40];
        app.update(key(KeyCode::Char('G')));
        let rows = frame_rows(&mut app, 140);
        assert!(rows[last_row].contains("FINAL VISIBLE ROW"), "{}", rows.join("\n"));
        app.update(key(KeyCode::Char('g')));
        app.update(key(KeyCode::Char('}')));
        let view = app.transcript.as_ref().unwrap();
        assert_eq!(view.scroll, view.visual_starts[1]);
        assert!(view.scroll > view.starts[1], "turn navigation counts wrapped rows");
    }

    #[test]
    fn log_inspection_redacts_credentials_and_does_not_reopen_a_closed_viewer() {
        let mut app = App::new("/w", None);
        app.secrets = vec!["known-sensitive-value".into()];
        assert_eq!(type_in(&mut app, "/log"), [Action::ShowLog]);
        app.update(Msg::ServiceLog(Ok(vec![
            "service ready".into(),
            "key known-sensitive-value".into(),
            "TOKEN=unknown-value".into(),
        ])));
        let shown = plain(&app.viewer.as_ref().unwrap().lines).join("\n");
        assert!(shown.contains("service ready") && shown.contains("[redacted]"));
        assert!(!shown.contains("known-sensitive-value") && !shown.contains("unknown-value"));
        app.update(key(KeyCode::Esc));
        app.update(Msg::ServiceLog(Ok(vec!["late".into()])));
        assert!(app.viewer.is_none());
    }

    #[test]
    fn question_mark_opens_and_closes_shortcuts_but_is_text_in_a_task() {
        let mut app = App::new("/w", None);
        app.update(key(KeyCode::Char('?')));
        assert!(app.input.text.is_empty());
        assert!(frame_text(&mut app).contains("shortcuts"));
        app.update(key(KeyCode::Char('?')));
        assert!(!frame_text(&mut app).contains("shortcuts"));
        app.update(Msg::Paste("why".into()));
        app.update(key(KeyCode::Char('?')));
        assert_eq!(app.input.text, "why?");
        app.input.take();
        app.update(key(KeyCode::Char('?')));
        app.update(key(KeyCode::Esc));
        assert!(!frame_text(&mut app).contains("shortcuts"));
    }

    #[test]
    fn shortcut_help_keeps_confirmation_and_memory_editing_intact() {
        let mut app = App::new("/w", None);
        app.prompt = Some(Prompt::Check(design()));
        app.update(key(KeyCode::Char('?')));
        let text = frame_text(&mut app);
        assert!(text.contains("check confirmation") && text.contains("edit check"));
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.prompt, Some(Prompt::Check(_))));
        app.prompt = Some(Prompt::Apply {
            resp: Box::new(passed(false, Some("/fork"))),
            fork: "/fork".into(),
            part: Part::default(),
        });
        app.update(key(KeyCode::Char('?')));
        let text = frame_text(&mut app);
        assert!(text.contains("apply confirmation") && text.contains("keep result in its fork"));
        app.update(key(KeyCode::Char('?')));
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        app.prompt = None;
        app.screen = Screen::Memory(MemoryView {
            query: String::new(),
            notes: vec![],
            selected: 0,
            loading: false,
            error: None,
            edit: None,
        });
        app.update(key(KeyCode::Char('?')));
        assert!(frame_text(&mut app).contains("filter notes"));
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::Memory(_)));
        app.update(key(KeyCode::Char('/')));
        app.update(Msg::Paste("why".into()));
        app.update(key(KeyCode::Char('?')));
        let Screen::Memory(view) = &app.screen else { panic!("memory closed") };
        assert!(matches!(&view.edit, Some(MemoryEdit::Filter(i)) if i.text == "why?"));
        assert!(app.shortcuts.is_none());
        app.screen = Screen::Main;
        type_in(&mut app, "/help");
        app.update(key(KeyCode::Char('?')));
        assert!(plain(&app.shortcuts.as_ref().unwrap().lines).join("\n").contains("close viewer"));
        app.update(key(KeyCode::Esc));
        assert_eq!(app.viewer.as_ref().unwrap().title, "help");
    }

    #[test]
    fn the_transcript_lists_each_turn_and_keeps_reading_while_the_run_goes_on() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "implement it");
        let trace = app.task.as_ref().unwrap().trace.clone();
        attempt_with_tool_call(&mut app, &trace, "cargo test");
        app.update(key(KeyCode::Tab));
        assert_eq!(reads(&app.update(key(KeyCode::Char('o')))), [(trace.clone(), None)]);
        // Until the log is read, the tool calls the run published show.
        let shown = |app: &App| plain(&app.transcript.as_ref().unwrap().lines);
        assert!(shown(&app).iter().any(|l| l.contains("cargo test")), "{:?}", shown(&app));
        assert!((0..20).all(|_| reads(&app.update(Msg::Tick)).is_empty()), "one read at a time");

        let mut log = transcript::tests::two_turns(&trace);
        log.iter_mut().for_each(transcript::slim);
        let later = log.split_off(5);
        let page = |entries, cursor| Ok(AuditPage { entries, cursor: Some(cursor), more: false });
        app.update(Msg::Audit { trace: trace.clone(), result: page(log, 100) });
        assert!(shown(&app).iter().any(|l| l == "      lexer::eof"), "{:?}", shown(&app));
        assert_eq!(app.transcript.as_ref().unwrap().starts.len(), 1);
        // About a second later it reads on from where it stopped, and the new turn is added.
        let again: Vec<Action> = (0..TRANSCRIPT_TICKS).flat_map(|_| app.update(Msg::Tick)).collect();
        assert_eq!(reads(&again), [(trace.clone(), Some(100))]);
        app.update(Msg::Audit { trace: trace.clone(), result: page(later, 200) });
        let starts = app.transcript.as_ref().unwrap().starts.clone();
        assert_eq!(starts.len(), 2);
        app.update(key(KeyCode::Char('}')));
        assert_eq!(app.transcript.as_ref().unwrap().scroll, starts[1]);
        app.update(key(KeyCode::Char('{')));
        assert_eq!(app.transcript.as_ref().unwrap().scroll, 0);
        assert!(frame_text(&mut app).contains(" transcript · attempt 1 "));

        let Some(Action::Export { text }) = app.update(key(KeyCode::Char('v'))).pop() else { panic!("exported") };
        assert!(text.starts_with(&format!("molt transcript · run {trace}")));
        assert!(text.contains("    command: cargo test") && text.contains("    path: src/lexer.rs"), "{text}");
        // The end of the run is read once more.
        let ended = app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Err("stopped".into())) });
        assert_eq!(reads(&ended), [(trace, Some(200))]);
        app.update(key(KeyCode::Esc));
        assert!(app.transcript.is_none() && app.task.is_none());
    }

    #[test]
    fn a_finished_runs_transcript_still_opens_and_shows_its_progress_without_the_log() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "implement it");
        let trace = app.task.as_ref().unwrap().trace.clone();
        attempt_with_tool_call(&mut app, &trace, "cargo build");
        app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Err("stopped".into())) });
        assert!(app.task.is_none());
        assert_eq!(reads(&app.update(ctrl_key('o'))), [(trace.clone(), None)]);
        app.update(Msg::Audit { trace, result: Err("audit.jsonl: permission denied".into()) });
        let lines = plain(&app.transcript.as_ref().unwrap().lines);
        assert!(lines[0].contains("could not be read (audit.jsonl: permission denied)"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("cargo build")), "{lines:?}");
        assert!((0..20).all(|_| reads(&app.update(Msg::Tick)).is_empty()), "the log is not tried again");
        app.update(ctrl_key('o'));
        assert!(app.transcript.is_none());
    }

    #[test]
    fn terminal_controls_work_over_a_viewer_and_leave_the_task_running() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "fix it");
        type_in(&mut app, "/help");
        assert_eq!(app.update(ctrl_key('l')), [Action::Redraw]);
        assert_eq!(app.update(ctrl_key('z')), [Action::Suspend]);
        assert!(app.task.is_some());
        app.viewer = None;
        assert_eq!(type_in(&mut app, "/theme light"), [Action::SaveTheme(text::Theme::Light), Action::Redraw]);
        assert_eq!(app.theme, text::Theme::Light);
        assert!(app.task.is_some());
    }

    #[test]
    fn the_last_diff_opens_again_after_the_task_ends() {
        let mut app = App::new("/w", None);
        up(&mut app);
        // These are about the prompts themselves.
        app.rules.always_ask = true;
        type_in(&mut app, "/last");
        assert_eq!(app.flash.as_deref(), Some("no task has finished with changes yet"));
        type_in(&mut app, "/check true");
        for end in [KeyCode::Char('y'), KeyCode::Char('k'), KeyCode::Char('n')] {
            let actions = type_in(&mut app, "task");
            let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
            app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(false, Some("/d/work/fork-5")))) });
            app.update(key(end));
            if end == KeyCode::Char('y') {
                app.update(Msg::Applied(Ok(())));
            }
            assert!(app.task.is_none() && app.prompt.is_none());
            type_in(&mut app, "/last");
            let viewer = app.viewer.take().expect("the diff opens");
            assert_eq!(viewer.title, "diff");
            assert!(plain(&viewer.lines).iter().any(|l| l.contains("+c")), "{:?}", plain(&viewer.lines));
        }
    }
}
