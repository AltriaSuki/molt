//! What the TUI shows and how it answers keys and results. Nothing here
//! touches the terminal or the bus: [`App::update`] takes a [`Msg`] and
//! returns the [`Action`]s the runtime carries out, so the whole flow can be
//! tested with plain values.

use std::collections::HashSet;

use molt_api::fs::ChangeKind;
use molt_api::memory::{ConsolidateResponse, Recalled};
use molt_api::model::Effort;
use molt_api::planner::{AttemptStatus, DesignResponse, Outcome, RunRequest, RunResponse};
use molt_api::progress::Progress;
use molt_proto::TraceId;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;

use super::text::{self, block, clean_line, span, Tone};
use crate::agent::outcome_name;

/// Lines of a winner's final message shown in the log; the rest is cut.
const SUMMARY_LINES: usize = 40;
/// Lines kept per attempt while it runs.
const LANE_LINES: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Keep every result in its fork; never change the workspace.
    Plan,
    /// Stop for the check before the attempts, and for the result before it is applied.
    Confirm,
    /// Run the designed check and apply what passes, without stopping.
    Auto,
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Confirm,
            Self::Confirm => Self::Auto,
            Self::Auto => Self::Plan,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "plan only",
            Self::Confirm => "confirm",
            Self::Auto => "auto",
        }
    }
}

/// What the next task runs with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub attempts: Option<u32>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub budget_usd: Option<f64>,
    /// A check command of the user's, used instead of a designed one.
    pub check: Option<String>,
}

/// Work for the runtime.
#[derive(Debug, PartialEq)]
pub enum Action {
    StartSession,
    Design {
        req: RunRequest,
        trace: TraceId,
    },
    Run {
        req: RunRequest,
        trace: TraceId,
    },
    Learn {
        trace: TraceId,
    },
    Apply {
        fork: String,
    },
    Discard {
        fork: String,
    },
    LoadNotes {
        query: String,
    },
    Forget {
        id: String,
        reason: String,
    },
    /// Stop everything in flight: the services are stopped and started again.
    Interrupt {
        keep: Vec<String>,
    },
    Quit {
        keep: Vec<String>,
    },
}

/// What happened.
#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Tick,
    /// SIGTERM or SIGHUP.
    Signal,
    SessionUp {
        versions: Vec<(String, String)>,
        memory_problem: Option<String>,
    },
    SessionFailed(String),
    /// The services of an interrupted session have stopped.
    Stopped,
    Progress(Progress),
    Designed {
        trace: TraceId,
        result: Result<DesignResponse, String>,
    },
    Ran {
        trace: TraceId,
        result: Result<RunResponse, String>,
    },
    Learned {
        trace: TraceId,
        result: Result<ConsolidateResponse, String>,
    },
    Applied(Result<(), String>),
    Discarded(Result<(), String>),
    Notes(Result<Vec<Recalled>, String>),
    Forgot {
        id: String,
        result: Result<bool, String>,
    },
}

#[derive(Debug, PartialEq)]
pub enum SessionState {
    Starting,
    Up { versions: Vec<(String, String)>, memory_problem: Option<String> },
    Down(String),
}

#[derive(Debug, PartialEq)]
pub enum Phase {
    /// planner.design is working on the check.
    Designing,
    /// The check is shown; the user decides.
    Confirming,
    Running,
    /// The result is shown; the user decides.
    Deciding,
    Applying,
}

pub struct Lane {
    pub index: u32,
    pub turn: u32,
    pub status: Option<AttemptStatus>,
    pub lines: Vec<Line<'static>>,
}

pub struct Task {
    pub trace: TraceId,
    pub req: RunRequest,
    pub phase: Phase,
    pub lanes: Vec<Lane>,
    /// The check is in the log already.
    check_shown: bool,
    recalled_shown: bool,
    notes_seen: HashSet<String>,
}

pub enum Prompt {
    Check(DesignResponse),
    /// The check's command is in the input box.
    EditCheck(DesignResponse),
    Apply {
        resp: Box<RunResponse>,
        fork: String,
    },
}

/// Text shown over everything else: a diff, a check's files, help.
pub struct Viewer {
    pub title: String,
    pub lines: Vec<Line<'static>>,
    pub scroll: usize,
}

pub enum Screen {
    Main,
    Memory(MemoryView),
}

pub struct MemoryView {
    pub query: String,
    pub notes: Vec<Recalled>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub edit: Option<MemoryEdit>,
}

pub enum MemoryEdit {
    Filter(Input),
    /// Why the selected note is wrong.
    Reason(Input),
}

/// A one-line text field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Input {
    pub text: String,
    /// In characters.
    pub cursor: usize,
}

impl Input {
    pub fn with(text: &str) -> Self {
        Self { text: text.to_owned(), cursor: text.chars().count() }
    }

    fn byte(&self, at: usize) -> usize {
        self.text.char_indices().nth(at).map_or(self.text.len(), |(i, _)| i)
    }

    pub fn insert(&mut self, s: &str) {
        let at = self.byte(self.cursor);
        let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        self.text.insert_str(at, &s);
        self.cursor += s.chars().count();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Handle an editing key; false when the key is not one.
    pub fn key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.chars().count(),
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte(start), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                let (a, b) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                let (a, b) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.text.replace_range(a..b, "");
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.chars().count(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

pub struct App {
    /// The workspace as shown, `~` for the home directory.
    pub workspace: String,
    /// The canonical workspace path, as requests name it.
    workspace_path: String,
    pub mode: Mode,
    pub settings: Settings,
    pub session: SessionState,
    pub log: Vec<Line<'static>>,
    /// Lines scrolled up from the end of the log; 0 follows it.
    pub scroll: usize,
    pub input: Input,
    history: Vec<String>,
    history_at: Option<usize>,
    pub task: Option<Task>,
    pub prompt: Option<Prompt>,
    pub viewer: Option<Viewer>,
    pub screen: Screen,
    /// Spent in this session: designs, runs and learning.
    pub spent_usd: f64,
    /// Runs memory is learning from.
    pub learning: HashSet<String>,
    /// Results the user kept in their forks; never removed.
    pub kept: Vec<String>,
    /// The attempt drawn alone, large.
    pub zoom: Option<u32>,
    /// A short message in the status bar, until the next key.
    pub flash: Option<String>,
    pub tick: u64,
    pub quit: bool,
}

fn key_is(key: &KeyEvent, code: KeyCode) -> bool {
    key.code == code && !key.modifiers.contains(KeyModifiers::CONTROL)
}

fn ctrl(key: &KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

impl App {
    pub fn new(workspace_path: &str, home: Option<&str>) -> Self {
        let workspace = match home.and_then(|h| workspace_path.strip_prefix(h)) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => workspace_path.to_owned(),
        };
        Self {
            workspace: clean_line(&workspace),
            workspace_path: workspace_path.to_owned(),
            mode: Mode::Confirm,
            settings: Settings::default(),
            session: SessionState::Starting,
            log: Vec::new(),
            scroll: 0,
            input: Input::default(),
            history: Vec::new(),
            history_at: None,
            task: None,
            prompt: None,
            viewer: None,
            screen: Screen::Main,
            spent_usd: 0.0,
            learning: HashSet::new(),
            kept: Vec::new(),
            zoom: None,
            flash: None,
            tick: 0,
            quit: false,
        }
    }

    pub fn memory_up(&self) -> bool {
        matches!(&self.session, SessionState::Up { memory_problem: None, .. })
    }

    fn push(&mut self, line: Line<'static>) {
        self.log.push(line);
    }

    fn gap(&mut self) {
        if self.log.last().is_some_and(|l| l.width() > 0) {
            self.log.push(Line::default());
        }
    }

    fn error(&mut self, what: &str, e: &str) {
        let mut lines = text::clean(e).into_iter();
        let first = lines.next().unwrap_or_default();
        self.push(Line::from(vec![span(format!("✗ {what}: "), Tone::Bad), span(first, Tone::Bad)]));
        for l in lines {
            self.push(Line::from(span(format!("  {l}"), Tone::Bad)));
        }
    }

    /// Forks to keep when the services stop: the kept results, and one waiting on the user.
    fn keep(&self) -> Vec<String> {
        let mut keep = self.kept.clone();
        if let Some(Prompt::Apply { fork, .. }) = &self.prompt {
            keep.push(fork.clone());
        }
        keep
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) if key.kind == KeyEventKind::Release => Vec::new(),
            Msg::Key(key) => {
                self.flash = None;
                self.key(key)
            }
            Msg::Paste(text) => {
                match &mut self.screen {
                    Screen::Memory(view) => match &mut view.edit {
                        Some(MemoryEdit::Filter(input) | MemoryEdit::Reason(input)) => input.insert(&text),
                        None => {}
                    },
                    Screen::Main => self.input.insert(&text),
                }
                Vec::new()
            }
            Msg::Tick => {
                self.tick = self.tick.wrapping_add(1);
                Vec::new()
            }
            Msg::Signal => {
                self.quit = true;
                vec![Action::Quit { keep: self.keep() }]
            }
            Msg::SessionUp { versions, memory_problem } => {
                if let Some(problem) = &memory_problem {
                    self.push(Line::from(span(format!("running without memory: {}", clean_line(problem)), Tone::Dim)));
                }
                self.session = SessionState::Up { versions, memory_problem };
                Vec::new()
            }
            Msg::SessionFailed(e) => {
                self.error("the services did not start", &e);
                self.session = SessionState::Down(clean_line(&e));
                Vec::new()
            }
            Msg::Stopped => {
                self.session = SessionState::Starting;
                vec![Action::StartSession]
            }
            Msg::Progress(event) => {
                self.progress(event);
                Vec::new()
            }
            Msg::Designed { trace, result } => self.designed(trace, result),
            Msg::Ran { trace, result } => self.ran(trace, result),
            Msg::Learned { trace, result } => {
                self.learning.remove(trace.as_str());
                self.learned(result);
                Vec::new()
            }
            Msg::Applied(result) => {
                match result {
                    Ok(()) => self.push(Line::from(span("  applied", Tone::Ok))),
                    Err(e) => self.error("could not apply", &e),
                }
                self.task = None;
                Vec::new()
            }
            Msg::Discarded(result) => {
                if let Err(e) = result {
                    self.error("could not remove the result", &e);
                }
                Vec::new()
            }
            Msg::Notes(result) => {
                if let Screen::Memory(view) = &mut self.screen {
                    view.loading = false;
                    match result {
                        Ok(notes) => {
                            view.selected = view.selected.min(notes.len().saturating_sub(1));
                            view.notes = notes;
                            view.error = None;
                        }
                        Err(e) => view.error = Some(clean_line(&e)),
                    }
                }
                Vec::new()
            }
            Msg::Forgot { id, result } => {
                match result {
                    Ok(true) => self.flash = Some(format!("forgot {id}")),
                    Ok(false) => self.flash = Some(format!("{id} was already forgotten")),
                    Err(e) => self.flash = Some(format!("could not forget {id}: {}", clean_line(&e))),
                }
                match &mut self.screen {
                    Screen::Memory(view) => {
                        view.loading = true;
                        vec![Action::LoadNotes { query: view.query.clone() }]
                    }
                    Screen::Main => Vec::new(),
                }
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Action> {
        if ctrl(&key, 'd') || (ctrl(&key, 'c') && self.task.is_none() && self.input.text.is_empty()) {
            self.quit = true;
            return vec![Action::Quit { keep: self.keep() }];
        }
        if self.viewer.is_some() {
            self.viewer_key(&key);
            return Vec::new();
        }
        if let Screen::Memory(_) = self.screen {
            return self.memory_key(&key);
        }
        if ctrl(&key, 'c') && !self.input.text.is_empty() {
            self.input.take();
            return Vec::new();
        }
        if ctrl(&key, 'c') || (key_is(&key, KeyCode::Esc) && self.busy()) {
            return self.interrupt();
        }
        match &self.prompt {
            Some(Prompt::Check(_)) => return self.check_key(&key),
            Some(Prompt::Apply { .. }) => return self.apply_key(&key),
            Some(Prompt::EditCheck(_)) | None => {}
        }
        match key.code {
            KeyCode::BackTab => {
                self.mode = self.mode.next();
                return Vec::new();
            }
            KeyCode::Tab => {
                if let Some(task) = &self.task {
                    let indices: Vec<u32> = task.lanes.iter().map(|l| l.index).collect();
                    self.zoom = match self.zoom.and_then(|z| indices.iter().position(|&i| i == z)) {
                        None => indices.first().copied(),
                        Some(at) => indices.get(at + 1).copied(),
                    };
                }
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                return Vec::new();
            }
            KeyCode::Up if self.prompt.is_none() => {
                self.history_step(true);
                return Vec::new();
            }
            KeyCode::Down if self.prompt.is_none() => {
                self.history_step(false);
                return Vec::new();
            }
            KeyCode::Esc => {
                if let Some(Prompt::EditCheck(d)) = self.prompt.take() {
                    self.input.take();
                    self.prompt = Some(Prompt::Check(d));
                } else {
                    self.input.take();
                }
                return Vec::new();
            }
            KeyCode::Enter => {
                let line = self.input.take();
                if let Some(Prompt::EditCheck(mut d)) = self.prompt.take() {
                    let command = line.trim();
                    if !command.is_empty() && d.command.as_deref() != Some(command) {
                        d.command = Some(command.to_owned());
                        // The baseline was of the old command.
                        d.baseline = None;
                    }
                    self.prompt = Some(Prompt::Check(d));
                    return Vec::new();
                }
                return self.submit(line);
            }
            _ => {}
        }
        self.input.key(&key);
        Vec::new()
    }

    /// A task is waiting on the services.
    fn busy(&self) -> bool {
        self.task.as_ref().is_some_and(|t| matches!(t.phase, Phase::Designing | Phase::Running | Phase::Applying))
    }

    fn interrupt(&mut self) -> Vec<Action> {
        if self.task.is_none() && self.learning.is_empty() {
            return Vec::new();
        }
        let keep = self.keep();
        if let Some(Prompt::Apply { fork, .. }) = self.prompt.take() {
            self.kept.push(fork.clone());
            self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
        }
        self.prompt = None;
        self.task = None;
        self.zoom = None;
        self.learning.clear();
        self.push(Line::from(span("■ stopped; restarting the services", Tone::Warn)));
        self.session = SessionState::Starting;
        vec![Action::Interrupt { keep }]
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let at = match (self.history_at, back) {
            (None, true) => Some(self.history.len() - 1),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), false) => None,
        };
        self.history_at = at;
        self.input = at.map(|i| Input::with(&self.history[i])).unwrap_or_default();
    }

    fn submit(&mut self, line: String) -> Vec<Action> {
        let line = line.trim().to_owned();
        if line.is_empty() {
            return Vec::new();
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.history_at = None;
        self.scroll = 0;
        if let Some(command) = line.strip_prefix('/') {
            return self.command(command);
        }
        if self.task.is_some() {
            self.flash = Some("a task is running; Esc stops it".into());
            self.input = Input::with(&line);
            return Vec::new();
        }
        if !matches!(self.session, SessionState::Up { .. }) {
            self.flash = Some("the services are not up".into());
            self.input = Input::with(&line);
            return Vec::new();
        }
        let trace = TraceId::random();
        let mut req = RunRequest::new(line.clone(), self.workspace_path.clone());
        if let Some(n) = self.settings.attempts {
            req.attempts = n;
        }
        req.model = self.settings.model.clone();
        req.effort = self.settings.effort;
        req.budget_usd = self.settings.budget_usd;
        req.apply = self.mode == Mode::Auto;

        self.gap();
        self.push(Line::from(vec![span("› ", Tone::Shell), span(clean_line(&line), Tone::Plain)]));
        let mut task = Task {
            trace: trace.clone(),
            req,
            phase: Phase::Running,
            lanes: Vec::new(),
            check_shown: false,
            recalled_shown: false,
            notes_seen: HashSet::new(),
        };
        let action = if let Some(check) = &self.settings.check {
            task.req.check = Some(check.clone());
            Action::Run { req: task.req.clone(), trace }
        } else if self.mode == Mode::Confirm {
            task.phase = Phase::Designing;
            Action::Design { req: task.req.clone(), trace }
        } else {
            Action::Run { req: task.req.clone(), trace }
        };
        self.task = Some(task);
        self.zoom = None;
        vec![action]
    }

    fn command(&mut self, command: &str) -> Vec<Action> {
        let (name, arg) = command.split_once(' ').map_or((command, ""), |(n, a)| (n, a.trim()));
        let set = |what: &str, value: &str| format!("{what}: {value}");
        match name {
            "help" => {
                self.viewer = Some(Viewer { title: "help".into(), lines: help(), scroll: 0 });
            }
            "memory" => {
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
            "check" => {
                self.settings.check = (!arg.is_empty()).then(|| arg.to_owned());
                self.flash = Some(set("check", self.settings.check.as_deref().unwrap_or("designed for each task")));
            }
            "attempts" => match arg.parse::<u32>() {
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
            "model" => {
                self.settings.model = (!arg.is_empty() && arg != "default").then(|| arg.to_owned());
                self.flash = Some(set("model", self.settings.model.as_deref().unwrap_or("default")));
            }
            "effort" => {
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
            "budget" => match arg.trim_start_matches('$').parse::<f64>() {
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
            "clear" => {
                self.log.clear();
                self.scroll = 0;
            }
            "quit" | "exit" => {
                self.quit = true;
                return vec![Action::Quit { keep: self.keep() }];
            }
            other => self.flash = Some(format!("no command /{}; /help lists them", clean_line(other))),
        }
        Vec::new()
    }

    fn progress(&mut self, event: Progress) {
        let Some(task) = &mut self.task else { return };
        if event.run() != task.trace.as_str() {
            return;
        }
        match event {
            Progress::CheckReady { command, files, designed, .. } => {
                if task.check_shown {
                    return;
                }
                task.check_shown = true;
                let mut lines = vec![check_head(designed)];
                match command {
                    Some(c) => lines.push(Line::from(vec![span("  $ ", Tone::Dim), span(clean_line(&c), Tone::Plain)])),
                    None => {
                        lines.push(Line::from(span("  no automated check fits; the result is unverified", Tone::Dim)))
                    }
                }
                for f in files {
                    lines.push(Line::from(span(format!("  + {}", clean_line(&f)), Tone::Dim)));
                }
                self.log.extend(lines);
            }
            Progress::Recalled { notes, map_tokens, .. } => {
                if task.recalled_shown {
                    return;
                }
                task.recalled_shown = true;
                let mut head = vec![span("◇ memory", Tone::Mem)];
                let mut parts = Vec::new();
                if let Some(t) = map_tokens {
                    parts.push(format!("map {t} tokens"));
                }
                parts.push(match notes.len() {
                    1 => "1 note".to_owned(),
                    n => format!("{n} notes"),
                });
                head.push(span(format!("  {}", parts.join(" · ")), Tone::Dim));
                self.log.push(Line::from(head));
                for n in notes.iter().take(5) {
                    self.log.push(Line::from(vec![
                        span(format!("  {:.2} ", n.confidence), Tone::Mem),
                        span(clean_line(&n.text), Tone::Plain),
                    ]));
                }
                if notes.len() > 5 {
                    self.log.push(Line::from(span(format!("  and {} more", notes.len() - 5), Tone::Dim)));
                }
            }
            // Model streaming is opt-in and the terminal interface does not ask for it.
            Progress::ModelStarted { .. } | Progress::ModelText { .. } | Progress::ModelFinished { .. } => {}
            Progress::Note { message, .. } => {
                if task.notes_seen.insert(message.clone()) {
                    self.log.push(Line::from(span(format!("  {}", clean_line(&message)), Tone::Dim)));
                }
            }
            Progress::AttemptStarted { attempt, .. } => {
                if !task.lanes.iter().any(|l| l.index == attempt) {
                    task.lanes.push(Lane { index: attempt, turn: 0, status: None, lines: Vec::new() });
                    task.lanes.sort_by_key(|l| l.index);
                }
            }
            Progress::ToolCall { attempt, turn, detail, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.turn = turn;
                    lane.push(Line::from(vec![
                        span(format!("{turn:>2} "), Tone::Dim),
                        span(clean_line(&detail), Tone::Plain),
                    ]));
                }
            }
            Progress::CheckRan { attempt, passed, exit_code, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    let line = match (passed, exit_code) {
                        (true, _) => span("   check passed", Tone::Ok),
                        (false, Some(code)) => span(format!("   check failed (exit {code})"), Tone::Bad),
                        (false, None) => span("   check failed (killed or timed out)", Tone::Bad),
                    };
                    lane.push(Line::from(line));
                }
            }
            Progress::AttemptFinished { attempt, status, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.status = Some(status);
                }
            }
        }
    }

    fn designed(&mut self, trace: TraceId, result: Result<DesignResponse, String>) -> Vec<Action> {
        let Some(task) = &mut self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        match result {
            Ok(d) => {
                self.spent_usd += d.cost_usd;
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

    fn check_key(&mut self, key: &KeyEvent) -> Vec<Action> {
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
            }
            _ => {
                lines.push(Line::from(vec![
                    span("◆ check", Tone::Shell),
                    span("  none; the result is unverified", Tone::Dim),
                ]));
                task.req.verify = false;
            }
        }
        // The design was paid out of the task's budget.
        if let Some(budget) = task.req.budget_usd {
            task.req.budget_usd = Some((budget - d.cost_usd).max(0.01));
        }
        self.log.extend(lines);
        vec![Action::Run { req: task.req.clone(), trace: task.trace.clone() }]
    }

    fn ran(&mut self, trace: TraceId, result: Result<RunResponse, String>) -> Vec<Action> {
        let Some(task) = &self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        let task = self.task.take().expect("checked above");
        self.zoom = None;
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                self.error("the run failed", &e);
                return Vec::new();
            }
        };
        self.spent_usd += resp.cost_usd;
        let mut actions = Vec::new();

        for a in &resp.attempts {
            let (mark, tone) = match a.status {
                AttemptStatus::Passed => ("✓", Tone::Ok),
                AttemptStatus::Failed => ("✗", Tone::Bad),
                AttemptStatus::Cancelled => ("–", Tone::Dim),
                AttemptStatus::Error => ("!", Tone::Warn),
            };
            let status = match a.status {
                AttemptStatus::Passed => "passed",
                AttemptStatus::Failed => "failed",
                AttemptStatus::Cancelled => "cancelled",
                AttemptStatus::Error => "error",
            };
            let checks = match a.check_runs {
                0 => String::new(),
                1 => " · 1 check".to_owned(),
                n => format!(" · {n} checks"),
            };
            self.push(Line::from(vec![
                span(format!("  {mark} attempt {} ", a.index + 1), tone),
                span(format!("{status} · {} turns{checks} · ${:.2}", a.turns, a.cost_usd), Tone::Dim),
            ]));
        }

        let (added, removed) = text::diff_counts(&resp.patch);
        let files = match resp.changes.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        let size = vec![
            span(format!("  {files}  "), Tone::Dim),
            span(format!("+{added}"), Tone::Ok),
            span(" ", Tone::Plain),
            span(format!("−{removed}"), Tone::Bad),
        ];
        let summary = resp.summary.trim();
        match resp.outcome {
            Outcome::Failed => {
                self.push(Line::from(span("✗ no attempt passed", Tone::Bad)));
                self.log.extend(block(summary, "  ", Tone::Dim).into_iter().take(SUMMARY_LINES));
            }
            Outcome::Passed | Outcome::Unverified => {
                let tone = if resp.outcome == Outcome::Passed { Tone::Ok } else { Tone::Warn };
                let mut head = vec![span(format!("✓ {}", outcome_name(resp.outcome)), tone)];
                if !resp.changes.is_empty() {
                    head.extend(size);
                }
                self.push(Line::from(head));
                let lines = block(summary, "  ", Tone::Plain);
                let cut = lines.len().saturating_sub(SUMMARY_LINES);
                self.log.extend(lines.into_iter().take(SUMMARY_LINES));
                if cut > 0 {
                    self.push(Line::from(span(format!("  ({cut} more lines)"), Tone::Dim)));
                }
                for c in resp.changes.iter().take(20) {
                    let kind = match c.kind {
                        ChangeKind::Added => "added   ",
                        ChangeKind::Modified => "modified",
                        ChangeKind::Deleted => "deleted ",
                    };
                    self.push(Line::from(span(format!("  {kind} {}", clean_line(&c.path)), Tone::Dim)));
                }
                if resp.changes.len() > 20 {
                    self.push(Line::from(span(format!("  and {} more", resp.changes.len() - 20), Tone::Dim)));
                }
            }
        }
        if resp.uncounted_calls > 0 {
            let line = format!("  {} model calls of cancelled attempts are not in the cost", resp.uncounted_calls);
            self.push(Line::from(span(line, Tone::Dim)));
        }

        let mut deciding = false;
        if resp.applied {
            self.push(Line::from(span("  applied", Tone::Ok)));
        } else if let Some(fork) = resp.fork.clone() {
            if resp.changes.is_empty() {
                actions.push(Action::Discard { fork });
            } else if self.mode == Mode::Confirm && !task.req.apply && !matches!(resp.outcome, Outcome::Failed) {
                deciding = true;
                self.prompt = Some(Prompt::Apply { resp: Box::new(resp.clone()), fork });
            } else {
                self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
                self.kept.push(fork);
            }
        }
        if deciding {
            self.task = Some(Task { phase: Phase::Deciding, lanes: Vec::new(), ..task });
        }

        // Memory learns from failures too; the run's budget covers it.
        let spent_all = task_budget(&self.settings).is_some_and(|b| resp.cost_usd >= b);
        if self.memory_up() && !spent_all {
            self.learning.insert(trace.to_string());
            actions.push(Action::Learn { trace });
        }
        actions
    }

    fn apply_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Some(Prompt::Apply { resp, fork }) = &self.prompt else { return Vec::new() };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                let fork = fork.clone();
                self.prompt = None;
                if let Some(task) = &mut self.task {
                    task.phase = Phase::Applying;
                }
                vec![Action::Apply { fork }]
            }
            KeyCode::Char('n') => {
                let fork = fork.clone();
                self.prompt = None;
                self.task = None;
                self.push(Line::from(span("  discarded", Tone::Dim)));
                vec![Action::Discard { fork }]
            }
            KeyCode::Char('d') => {
                let mut lines = text::diff_lines(&resp.patch);
                if resp.patch_truncated {
                    lines.push(Line::from(span("(the diff is cut short here)", Tone::Dim)));
                }
                self.viewer = Some(Viewer { title: "diff".into(), lines, scroll: 0 });
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Esc => {
                let fork = fork.clone();
                self.prompt = None;
                self.task = None;
                self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
                self.kept.push(fork);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn learned(&mut self, result: Result<ConsolidateResponse, String>) {
        let head = |rest: String| Line::from(vec![span("◇ memory", Tone::Mem), span(format!("  {rest}"), Tone::Dim)]);
        match result {
            Err(e) => self.push(head(format!("learned nothing: {}", clean_line(&e)))),
            Ok(r) => {
                self.spent_usd += r.cost_usd;
                let count = |n: usize| if n == 1 { "1 note".to_owned() } else { format!("{n} notes") };
                let mut parts = Vec::new();
                if !r.added.is_empty() {
                    parts.push(format!("learned {}", count(r.added.len())));
                }
                if !r.reinforced.is_empty() {
                    parts.push(format!("confirmed {}", count(r.reinforced.len())));
                }
                if !r.contradicted.is_empty() {
                    parts.push(format!("disputed {}", count(r.contradicted.len())));
                }
                if parts.is_empty() {
                    parts.push("nothing new".into());
                }
                self.push(head(parts.join(" · ")));
                for note in &r.added {
                    self.push(Line::from(vec![span("  + ", Tone::Mem), span(clean_line(&note.text), Tone::Plain)]));
                }
            }
        }
    }

    fn viewer_key(&mut self, key: &KeyEvent) {
        let Some(viewer) = &mut self.viewer else { return };
        let last = viewer.lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.viewer = None,
            KeyCode::Down | KeyCode::Char('j') => viewer.scroll = (viewer.scroll + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => viewer.scroll = viewer.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => viewer.scroll = (viewer.scroll + 20).min(last),
            KeyCode::PageUp => viewer.scroll = viewer.scroll.saturating_sub(20),
            KeyCode::Char('g') | KeyCode::Home => viewer.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => viewer.scroll = last,
            _ => {}
        }
    }

    fn memory_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Screen::Memory(view) = &mut self.screen else { return Vec::new() };
        if let Some(edit) = &mut view.edit {
            match key.code {
                KeyCode::Esc => view.edit = None,
                KeyCode::Enter => match view.edit.take() {
                    Some(MemoryEdit::Filter(mut input)) => {
                        view.query = input.take().trim().to_owned();
                        view.loading = true;
                        view.selected = 0;
                        return vec![Action::LoadNotes { query: view.query.clone() }];
                    }
                    Some(MemoryEdit::Reason(mut input)) => {
                        let reason = input.take().trim().to_owned();
                        let Some(note) = view.notes.get(view.selected) else { return Vec::new() };
                        if reason.is_empty() {
                            view.edit = Some(MemoryEdit::Reason(input));
                            self.flash = Some("say why the note is wrong".into());
                            return Vec::new();
                        }
                        return vec![Action::Forget { id: note.note.id.clone(), reason }];
                    }
                    None => {}
                },
                _ => {
                    let (MemoryEdit::Filter(input) | MemoryEdit::Reason(input)) = edit;
                    input.key(key);
                }
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Main,
            KeyCode::Down | KeyCode::Char('j') => {
                view.selected = (view.selected + 1).min(view.notes.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => view.selected = view.selected.saturating_sub(1),
            KeyCode::Char('/') => view.edit = Some(MemoryEdit::Filter(Input::with(&view.query))),
            KeyCode::Char('f') if !view.notes.is_empty() => view.edit = Some(MemoryEdit::Reason(Input::default())),
            KeyCode::Char('r') => {
                view.loading = true;
                return vec![Action::LoadNotes { query: view.query.clone() }];
            }
            _ => {}
        }
        Vec::new()
    }
}

impl Lane {
    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
        if self.lines.len() > LANE_LINES {
            self.lines.remove(0);
        }
    }
}

fn lane(task: &mut Task, attempt: u32) -> Option<&mut Lane> {
    task.lanes.iter_mut().find(|l| l.index == attempt)
}

fn check_head(designed: bool) -> Line<'static> {
    let by = if designed { "designed" } else { "yours" };
    Line::from(vec![span("◆ check", Tone::Shell), span(format!("  {by}"), Tone::Dim)])
}

/// The budget a task runs with, when the user set one.
fn task_budget(settings: &Settings) -> Option<f64> {
    settings.budget_usd
}

fn help() -> Vec<Line<'static>> {
    let row = |keys: &str, what: &str| {
        Line::from(vec![span(format!("  {keys:<18}"), Tone::Shell), span(what.to_owned(), Tone::Plain)])
    };
    let head = |t: &str| Line::from(span(t.to_owned(), Tone::Dim));
    vec![
        head("tasks"),
        row("enter", "run the task in the input"),
        row("shift+tab", "mode: plan only, confirm, auto"),
        row("tab", "show one attempt large; again for the next"),
        row("esc", "stop the task"),
        row("pgup / pgdn", "scroll the log"),
        row("↑ / ↓", "earlier inputs"),
        row("ctrl+d", "quit"),
        Line::default(),
        head("commands"),
        row("/memory [words]", "notes about this project"),
        row("/check [command]", "use this check for every task; empty: design one"),
        row("/attempts N", "attempts at once, 1 to 8"),
        row("/model NAME", "opus, sonnet, haiku or a model id"),
        row("/effort LEVEL", "low, medium, high, xhigh or max"),
        row("/budget USD", "spending limit per task"),
        row("/clear", "clear the log"),
        row("/quit", "quit"),
        Line::default(),
        head("modes"),
        row("plan only", "results stay in their forks; the workspace is not changed"),
        row("confirm", "stop to accept the check, and again before applying"),
        row("auto", "design the check and apply what passes"),
    ]
}

/// Lines as plain text, for tests.
#[cfg(test)]
pub fn plain(lines: &[Line<'static>]) -> Vec<String> {
    lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
}

#[cfg(test)]
mod tests {
    use molt_api::fs::Change;
    use molt_api::model::Usage;
    use molt_api::planner::{AttemptReport, Baseline, CheckFile, CheckSpec};
    use molt_api::progress::RecalledNote;
    use ratatui::crossterm::event::KeyEventState;

    use super::*;

    fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn ctrl_key(c: char) -> Msg {
        Msg::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn up(app: &mut App) {
        app.update(Msg::SessionUp { versions: vec![("planner".into(), "ab12cd34".into())], memory_problem: None });
    }

    fn type_in(app: &mut App, text: &str) -> Vec<Action> {
        app.update(Msg::Paste(text.into()));
        app.update(key(KeyCode::Enter))
    }

    fn log(app: &App) -> String {
        plain(&app.log).join("\n")
    }

    fn design() -> DesignResponse {
        DesignResponse {
            command: Some("cargo test --test commas".into()),
            files: vec![CheckFile { path: "tests/commas.rs".into(), content: "#[test]\nfn t() {}\n".into() }],
            rationale: "the parser has tests".into(),
            baseline: Some(Baseline { passed: false, exit_code: Some(101), tail: String::new() }),
            usage: Usage::default(),
            cost_usd: 0.25,
        }
    }

    fn passed(apply: bool, fork: Option<&str>) -> RunResponse {
        RunResponse {
            outcome: Outcome::Passed,
            check: Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: true }),
            winner: Some(1),
            summary: "Trailing commas parse.".into(),
            changes: vec![Change { path: "src/parser.rs".into(), kind: ChangeKind::Modified }],
            patch: "--- a/src/parser.rs\n+++ b/src/parser.rs\n@@\n-a\n+b\n+c\n".into(),
            patch_truncated: false,
            applied: apply,
            fork: fork.map(str::to_owned),
            attempts: vec![
                AttemptReport {
                    index: 0,
                    status: AttemptStatus::Cancelled,
                    turns: 4,
                    check_runs: 1,
                    usage: Usage::default(),
                    cost_usd: 0.1,
                    note: String::new(),
                },
                AttemptReport {
                    index: 1,
                    status: AttemptStatus::Passed,
                    turns: 5,
                    check_runs: 1,
                    usage: Usage::default(),
                    cost_usd: 0.2,
                    note: String::new(),
                },
            ],
            usage: Usage::default(),
            cost_usd: 0.3,
            uncounted_calls: 0,
        }
    }

    #[test]
    fn confirm_mode_shows_the_check_then_the_result_before_anything_changes() {
        let mut app = App::new("/home/u/parser", Some("/home/u"));
        assert_eq!(app.workspace, "~/parser");
        assert_eq!(app.mode, Mode::Confirm);
        up(&mut app);
        let actions = type_in(&mut app, "accept trailing commas");
        let [Action::Design { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(!req.apply && req.check.is_none());
        let trace = trace.clone();

        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()) });
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
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 1 }));
        app.update(Msg::Progress(Progress::AttemptStarted { run: run.clone(), attempt: 0 }));
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
        }));
        // Another run's events are not this task's.
        app.update(Msg::Progress(Progress::AttemptStarted { run: "other".into(), attempt: 5 }));
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.lanes.iter().map(|l| l.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(plain(&task.lanes[1].lines), [" 3 edit src/parser.rs", "   check passed"]);

        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-1"))) });
        assert_eq!(actions, [Action::Learn { trace: trace.clone() }]);
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
    fn auto_mode_runs_straight_through_and_plan_mode_keeps_the_result() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(key(KeyCode::BackTab));
        assert_eq!(app.mode, Mode::Auto);
        let actions = type_in(&mut app, "fix it");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(req.apply && req.check.is_none() && req.verify);
        let trace = trace.clone();
        app.update(Msg::Progress(Progress::CheckReady {
            run: trace.to_string(),
            command: Some("make test".into()),
            files: vec![],
            designed: true,
        }));
        app.update(Msg::Ran { trace, result: Ok(passed(true, None)) });
        assert!(app.prompt.is_none() && app.task.is_none());
        assert!(log(&app).contains("  $ make test") && log(&app).ends_with("  applied"), "{}", log(&app));

        app.update(key(KeyCode::BackTab));
        assert_eq!(app.mode, Mode::Plan);
        let actions = type_in(&mut app, "again");
        let [Action::Run { req, trace }] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(!req.apply);
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-2"))) });
        assert!(app.prompt.is_none());
        assert_eq!(app.kept, ["/d/work/fork-2"]);
    }

    #[test]
    fn the_check_can_be_edited_dropped_or_refused() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        let trace = trace.clone();
        app.update(Msg::Designed { trace: trace.clone(), result: Ok(design()) });
        app.update(key(KeyCode::Char('e')));
        assert_eq!(app.input.text, "cargo test --test commas");
        app.update(ctrl_key('u'));
        type_in(&mut app, "cargo test");
        let Some(Prompt::Check(d)) = &app.prompt else { panic!("back to the check") };
        assert_eq!(d.command.as_deref(), Some("cargo test"));
        assert_eq!(d.baseline, None, "the baseline was of the old command");
        let actions = app.update(key(KeyCode::Char('n')));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!() };
        assert!(!req.verify && req.check.is_none() && req.check_files.is_empty());

        // Esc while it runs stops everything.
        let actions = app.update(key(KeyCode::Esc));
        assert_eq!(actions, [Action::Interrupt { keep: vec![] }]);
        assert!(app.task.is_none());
        assert_eq!(app.session, SessionState::Starting);
        assert_eq!(app.update(Msg::Stopped), [Action::StartSession]);

        // A design that fails ends the task.
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        app.update(Msg::Designed { trace: trace.clone(), result: Err("planner.design failed".into()) });
        assert!(app.task.is_none());
        assert!(log(&app).ends_with("✗ no check: planner.design failed"), "{}", log(&app));
    }

    #[test]
    fn recalled_notes_and_learning_are_shown_once() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        let run = trace.to_string();
        let recalled = Progress::Recalled {
            run: run.clone(),
            notes: vec![RecalledNote { id: "note_1".into(), text: "Tests need --features x".into(), confidence: 0.78 }],
            map_tokens: Some(2800),
        };
        app.update(Msg::Progress(recalled.clone()));
        app.update(Msg::Progress(recalled));
        app.update(Msg::Progress(Progress::Note { run: run.clone(), message: "project model: 3 files".into() }));
        app.update(Msg::Progress(Progress::Note { run, message: "project model: 3 files".into() }));
        assert!(
            log(&app).ends_with(
                "◇ memory  map 2800 tokens · 1 note\n  0.78 Tests need --features x\n  project model: 3 files"
            ),
            "{}",
            log(&app)
        );
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
        assert!(type_in(&mut app, "another").is_empty());
        assert_eq!(app.flash.as_deref(), Some("a task is running; Esc stops it"));
        // What was typed stays in the input.
        assert_eq!(app.input.text, "another");
        app.update(ctrl_key('u'));
        assert_eq!(type_in(&mut app, "/quit"), [Action::Quit { keep: vec![] }]);
    }

    #[test]
    fn the_memory_screen_lists_filters_and_forgets() {
        let mut app = App::new("/w", None);
        up(&mut app);
        assert_eq!(type_in(&mut app, "/memory parser"), [Action::LoadNotes { query: "parser".into() }]);
        let note = |id: &str| Recalled {
            note: serde_json::from_value(serde_json::json!({
                "id": id, "kind": "fact", "text": "t", "confidence": 0.5, "created_ms": 0, "updated_ms": 0,
                "provenance": { "trace": "tr", "service": "memory", "version": "v" }, "rev": 1
            }))
            .unwrap(),
            score: 1.0,
            reason: Default::default(),
            details: Default::default(),
        };
        app.update(Msg::Notes(Ok(vec![note("note_a"), note("note_b")])));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('f')));
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "a reason is required");
        app.update(Msg::Paste("the build moved to make".into()));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Action::Forget { id: "note_b".into(), reason: "the build moved to make".into() }]
        );
        assert_eq!(
            app.update(Msg::Forgot { id: "note_b".into(), result: Ok(true) }),
            [Action::LoadNotes { query: "parser".into() }]
        );
        app.update(key(KeyCode::Char('/')));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("lexer".into()));
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::LoadNotes { query: "lexer".into() }]);
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::Main));
    }

    #[test]
    fn quitting_keeps_a_result_still_waiting_on_the_user() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "/check true");
        assert!(actions.is_empty());
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-9"))) });
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        assert_eq!(app.update(ctrl_key('d')), [Action::Quit { keep: vec!["/d/work/fork-9".into()] }]);
    }
}
