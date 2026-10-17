//! What the TUI shows and how it answers keys and results. Nothing here
//! touches the terminal or the bus: [`App::update`] takes a [`Msg`] and
//! returns the [`Action`]s the runtime carries out, so the whole flow can be
//! tested with plain values.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use molt_api::fs::ChangeKind;
use molt_api::memory::{ConsolidateResponse, Layer, RecallSnapshot, Recalled, ReviewRequest, ReviewResponse};
use molt_api::model::Effort;
use molt_api::planner::{
    AttemptReport, AttemptStatus, Constraint, Control, ControlResponse, DesignResponse, Earlier, Outcome, RunRequest,
    RunResponse, RunState, Verdict,
};
use molt_api::progress::Progress;
use molt_proto::TraceId;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;

use super::preview::Preview;
use super::text::{self, block, clean_line, span, Tone};
use crate::agent;
use crate::agent::outcome_name;
use crate::config::ConfirmSection;

/// Bytes of one summary written to the log. Only a runaway message reaches
/// this; the log keeps everything a normal task says.
const SUMMARY_BYTES: usize = 1 << 20;
/// Lines kept per attempt while it runs.
const LANE_LINES: usize = 200;
/// Ticks within which a second Ctrl+C or Ctrl+D quits: about a second.
const QUIT_TICKS: u64 = 7;
/// Lines of a failed attempt's note shown under it in the log.
const NOTE_LINES: usize = 6;
/// Lines of a failed check's output shown in its lane.
const CHECK_TAIL_LINES: usize = 2;
/// Finished tasks of the conversation each new task is told about.
const EARLIER_TASKS: usize = 5;
/// Bytes of an earlier task's summary kept for the tasks after it.
const EARLIER_SUMMARY_BYTES: usize = 4000;
/// Ticks after a key within which the user is taken to be looking: about
/// five seconds.
const LOOKING_TICKS: u64 = 33;
/// Suggestions shown for an `@path` being typed.
const SUGGESTIONS: usize = 8;
/// Workspace files listed for `@` completion; past this, a path is not
/// checked against the list.
pub const LISTED_FILES: usize = 50_000;

/// When the interface calls the user from outside the terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Notify {
    Off,
    /// When it stops for the user, or the services go down.
    #[default]
    Waiting,
    /// Also when a task ends.
    All,
}

/// What the interface is waiting on the user for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attention {
    Check,
    Apply,
    Down,
}

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
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub attempts: Option<u32>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub budget_usd: Option<f64>,
    /// A check command of the user's, used instead of a designed one.
    pub check: Option<String>,
    /// The model memory learns with: a model, [`agent::SAME_MODEL`], or
    /// `None` for memory's default.
    pub memory_model: Option<String>,
    pub stream: bool,
    pub notify: Notify,
    /// Commit each applied result to git, as `molt do --commit` does.
    pub commit: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            attempts: None,
            model: None,
            effort: None,
            budget_usd: None,
            check: None,
            stream: true,
            memory_model: None,
            notify: Notify::default(),
            commit: false,
        }
    }
}

/// Work for the runtime.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Action {
    StartSession,
    Redraw,
    Suspend,
    SaveTheme(text::Theme),
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
        /// `None`: memory's default model.
        model: Option<String>,
    },
    /// Change the running run `trace`; the answer comes back as [`Msg::Controlled`].
    Control {
        trace: TraceId,
        action: Control,
    },
    Apply {
        fork: String,
    },
    /// Look at the work tree before the task `trace` changes it, for `/commit on`.
    PrepareCommit {
        trace: TraceId,
    },
    /// Commit the applied result of the task `trace`; the answer comes back as [`Msg::Committed`].
    Commit {
        trace: TraceId,
        task: String,
        resp: Box<RunResponse>,
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
    /// Keep a preference of the user's for every project.
    Remember {
        text: String,
    },
    /// Copy a project note to the user's layer, for every project.
    Promote {
        id: String,
        reason: String,
    },
    ChangeNote {
        review: ReviewRequest,
        text: Option<String>,
    },
    UsedNotes {
        run: String,
    },
    /// Stop everything in flight: the services are stopped and started again.
    Interrupt {
        keep: Vec<String>,
        /// Forks of the attempts that were running, kept when they hold
        /// changes to more than `check_files`.
        attempts: Vec<String>,
        check_files: Vec<String>,
    },
    Quit {
        keep: Vec<String>,
    },
    /// Edit the input in the user's editor; the result comes back as [`Msg::Edited`].
    Edit {
        text: String,
    },
    /// Call the user from outside the terminal: a bell, and a desktop
    /// notification where the terminal shows one. `text` is a short label,
    /// never the task or its output.
    Alert {
        text: String,
    },
    /// List the workspace's files for `@` completion; they come back as
    /// [`Msg::Files`].
    ListFiles,
}

/// What happened.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum Msg {
    #[serde(skip)]
    Key(KeyEvent),
    Paste(String),
    /// The terminal gained or lost focus, where it reports that.
    Focus(bool),
    /// The workspace's files, relative, as `.gitignore` leaves them;
    /// `complete` is false when the listing stopped at [`LISTED_FILES`].
    Files {
        files: Vec<String>,
        complete: bool,
    },
    Tick,
    /// SIGTERM or SIGHUP.
    Signal,
    SessionUp {
        versions: Vec<(String, String)>,
        memory_problem: Option<String>,
    },
    SessionFailed(String),
    /// The services of an interrupted session have stopped; `kept` are the
    /// forks of interrupted attempts kept for their changes.
    Stopped {
        kept: Vec<String>,
    },
    Progress(Progress),
    Designed {
        trace: TraceId,
        result: Result<DesignResponse, String>,
        /// The check's files that already exist in the workspace.
        existing: Vec<String>,
    },
    Ran {
        trace: TraceId,
        result: Result<RunResponse, String>,
    },
    Learned {
        trace: TraceId,
        result: Result<ConsolidateResponse, String>,
    },
    Controlled {
        trace: TraceId,
        action: Control,
        result: Result<ControlResponse, String>,
    },
    Applied(Result<(), String>),
    /// `None`: the workspace's commit has the files as they are already.
    Committed(Result<Option<crate::git::Committed>, String>),
    Discarded(Result<(), String>),
    Notes(Result<Vec<Recalled>, String>),
    /// The input as the user's editor left it.
    Edited(Result<String, String>),
    Forgot {
        id: String,
        result: Result<bool, String>,
    },
    /// The id of the note `/remember` kept.
    Remembered(Result<String, String>),
    /// The id of the user-layer copy of note `id`.
    Promoted {
        id: String,
        result: Result<String, String>,
    },
    ChangedNote(Result<ReviewResponse, String>),
    UsedNotes {
        run: String,
        result: Result<Vec<RecallSnapshot>, String>,
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

/// What settled model calls cost so far. A call whose cost is unknown makes
/// the sum a lower bound; it is never counted as free.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spend {
    pub usd: f64,
    pub unknown: bool,
}

impl Spend {
    fn add(&mut self, cost: Option<f64>) {
        match cost {
            Some(c) if c.is_finite() && c >= 0.0 => self.usd += c,
            _ => self.unknown = true,
        }
    }

    /// `$0.18`, or `≥ $0.18` when some cost is unknown.
    pub fn label(&self) -> String {
        let at_least = if self.unknown { "≥ " } else { "" };
        format!("{at_least}${:.2}", self.usd)
    }
}

impl std::ops::Add for Spend {
    type Output = Spend;
    fn add(self, other: Spend) -> Spend {
        Spend { usd: self.usd + other.usd, unknown: self.unknown || other.unknown }
    }
}

pub struct Lane {
    pub index: u32,
    /// The attempt's fork, once the planner has said which.
    pub fork: Option<String>,
    pub turn: u32,
    pub status: Option<AttemptStatus>,
    pub lines: Vec<Line<'static>>,
    pub preview: Preview,
    /// The attempt's model calls that have finished.
    pub spend: Spend,
    /// When the TUI first heard of the attempt, and when it ended.
    pub started: Instant,
    pub ended: Option<Instant>,
}

pub struct Task {
    pub trace: TraceId,
    pub req: RunRequest,
    pub phase: Phase,
    pub lanes: Vec<Lane>,
    pub designer_preview: Preview,
    /// The whole task's budget, as the user set it; `req.budget_usd` is
    /// what is left for the attempts once the check is designed.
    pub budget_usd: Option<f64>,
    /// The check design: its finished calls while it runs, then its settled
    /// cost.
    pub design_spend: Spend,
    /// Model calls already counted, so a repeated event is not counted twice.
    counted: HashSet<String>,
    /// The check is in the log already.
    check_shown: bool,
    recalled_shown: bool,
    notes_seen: HashSet<String>,
    /// Why confirm mode stopped for the user, in short labels.
    pub stops: Vec<String>,
    /// The latest version of the run's requirements; 0 is the task alone.
    pub version: u32,
    pub state: RunState,
    /// What a pausing run waits for before it is paused.
    pub waiting: Vec<String>,
}

impl Task {
    fn new(trace: TraceId, req: RunRequest, phase: Phase) -> Self {
        Self {
            trace,
            budget_usd: req.budget_usd,
            req,
            phase,
            lanes: Vec::new(),
            design_spend: Spend::default(),
            counted: HashSet::new(),
            check_shown: false,
            recalled_shown: false,
            notes_seen: HashSet::new(),
            stops: Vec::new(),
            designer_preview: Preview::default(),
            version: 0,
            state: RunState::Running,
            waiting: Vec::new(),
        }
    }

    /// What this task has cost so far, by finished model calls; the run's
    /// reply settles it.
    pub fn spend(&self) -> Spend {
        self.lanes.iter().fold(self.design_spend, |sum, lane| sum + lane.spend)
    }
}

pub enum Prompt {
    Check(DesignResponse),
    /// The check's command is in the input box.
    EditCheck(DesignResponse),
    Apply {
        resp: Box<RunResponse>,
        fork: String,
    },
    /// No attempt passed. The input stays live; with it empty, `r` runs the
    /// task again with the same check and `v` shows why each attempt ended.
    Retry {
        req: Box<RunRequest>,
        designed: bool,
        /// The user stopped the run, rather than every attempt failing.
        stopped: bool,
        attempts: Vec<AttemptReport>,
    },
}

/// A finished task's diff, kept after the task is applied, kept or discarded.
struct LastDiff {
    patch: String,
    truncated: bool,
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
    /// Why the selected note holds in every project.
    Promote(Input),
    Change(Box<NoteChange>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChangeStep {
    Text,
    Reason,
    Dependencies,
    Confirm,
}

pub struct NoteChange {
    pub review: ReviewRequest,
    pub text: Option<Input>,
    pub reason: Input,
    pub dependencies: Input,
    pub step: ChangeStep,
}

impl NoteChange {
    fn new(workspace: &str, note: &Recalled, correct: bool) -> Self {
        Self {
            review: ReviewRequest {
                workspace: workspace.to_owned(),
                id: note.note.id.clone(),
                expected_rev: note.note.rev,
                reason: String::new(),
                depends_on: Vec::new(),
                temporary: note.details.temporary,
            },
            text: correct.then(|| Input::with(&note.note.text)),
            reason: Input::default(),
            dependencies: Input::with(
                &note.details.dependencies.iter().map(|d| d.path.as_str()).collect::<Vec<_>>().join("; "),
            ),
            step: if correct { ChangeStep::Text } else { ChangeStep::Reason },
        }
    }

    pub fn input(&self) -> Option<&Input> {
        match self.step {
            ChangeStep::Text => self.text.as_ref(),
            ChangeStep::Reason => Some(&self.reason),
            ChangeStep::Dependencies => Some(&self.dependencies),
            ChangeStep::Confirm => None,
        }
    }

    fn input_mut(&mut self) -> Option<&mut Input> {
        match self.step {
            ChangeStep::Text => self.text.as_mut(),
            ChangeStep::Reason => Some(&mut self.reason),
            ChangeStep::Dependencies => Some(&mut self.dependencies),
            ChangeStep::Confirm => None,
        }
    }

    fn advance(&mut self) -> Result<Option<Action>, &'static str> {
        self.step = match self.step {
            ChangeStep::Text if self.text.as_ref().is_none_or(|i| i.text.trim().is_empty()) => {
                return Err("enter the corrected note")
            }
            ChangeStep::Text => ChangeStep::Reason,
            ChangeStep::Reason if self.reason.text.trim().is_empty() => {
                return Err("say why this review or correction is justified")
            }
            ChangeStep::Reason => ChangeStep::Dependencies,
            ChangeStep::Dependencies => ChangeStep::Confirm,
            ChangeStep::Confirm => {
                let mut review = self.review.clone();
                review.reason = self.reason.text.trim().to_owned();
                review.depends_on = self
                    .dependencies
                    .text
                    .split(';')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect();
                return Ok(Some(Action::ChangeNote {
                    review,
                    text: self.text.as_ref().map(|i| i.text.trim().to_owned()),
                }));
            }
        };
        Ok(None)
    }
}

/// A text field. The task input holds several lines; the others one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Input {
    pub text: String,
    /// In characters.
    pub cursor: usize,
    /// Long pastes, each shown as its placeholder in `text` and put back by [`Input::take`].
    pastes: Vec<(String, String)>,
}

/// Pasted lines shown as they are; a longer paste shows as one placeholder.
const PASTE_LINES: usize = 20;

impl Input {
    pub fn with(text: &str) -> Self {
        Self { text: text.to_owned(), cursor: text.chars().count(), pastes: Vec::new() }
    }

    fn byte(&self, at: usize) -> usize {
        self.text.char_indices().nth(at).map_or(self.text.len(), |(i, _)| i)
    }

    /// Insert `s` on one line: control characters, line breaks too, become spaces.
    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        self.put(&s);
    }

    /// Insert pasted text with its line breaks and tabs. Escape sequences
    /// and other control characters are dropped, and a paste of more than
    /// [`PASTE_LINES`] lines shows as a placeholder.
    pub fn paste(&mut self, s: &str) {
        let s = pasted(s);
        let lines = s.lines().count();
        if lines <= PASTE_LINES {
            return self.put(&s);
        }
        let placeholder = format!("[pasted #{}: {lines} lines]", self.pastes.len() + 1);
        self.put(&placeholder);
        self.pastes.push((placeholder, s));
    }

    /// The character just before the cursor.
    fn key_before(&self) -> Option<char> {
        self.cursor.checked_sub(1).and_then(|at| self.text.chars().nth(at))
    }

    pub fn newline(&mut self) {
        self.put("\n");
    }

    fn put(&mut self, s: &str) {
        let at = self.byte(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// The text, with every long paste in full, and an empty field.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        let mut text = std::mem::take(&mut self.text);
        for (placeholder, pasted) in std::mem::take(&mut self.pastes) {
            text = text.replacen(&placeholder, &pasted, 1);
        }
        text
    }

    /// The text with every long paste in full, leaving the field as it is.
    pub fn full(&self) -> String {
        self.clone().take()
    }

    /// The character index where the cursor's line starts, and where it ends.
    fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.text.chars().collect();
        let start = chars[..self.cursor].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let end = chars[self.cursor..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| self.cursor + i);
        (start, end)
    }

    /// Move to the line above (`up`) or below, keeping the column; false on the first or last line.
    pub fn line_step(&mut self, up: bool) -> bool {
        let chars: Vec<char> = self.text.chars().collect();
        let (start, end) = self.line_bounds();
        let column = self.cursor - start;
        if up {
            if start == 0 {
                return false;
            }
            let above = chars[..start - 1].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
            self.cursor = above + column.min(start - 1 - above);
        } else {
            if end == chars.len() {
                return false;
            }
            let below_end = chars[end + 1..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| end + 1 + i);
            self.cursor = end + 1 + column.min(below_end - end - 1);
        }
        true
    }

    /// Handle an editing key; false when the key is not one.
    pub fn key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = self.line_bounds().0,
            KeyCode::Char('e') if ctrl => self.cursor = self.line_bounds().1,
            KeyCode::Char('u') if ctrl => {
                let start = self.line_bounds().0;
                let (a, b) = (self.byte(start), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor = start;
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && !matches!(chars[start - 1], ' ' | '\n') {
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
            KeyCode::Home => self.cursor = self.line_bounds().0,
            KeyCode::End => self.cursor = self.line_bounds().1,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

/// Pasted text as the input keeps it: line breaks as `\n`, tabs kept, and
/// escape sequences and other control characters dropped, so a paste cannot
/// reach the terminal or the model as anything but text.
fn pasted(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\u{1b}' => match chars.peek() {
                // CSI: parameters, then one final byte from @ to ~.
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \.
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub struct App {
    /// The workspace as shown, `~` for the home directory.
    pub workspace: String,
    /// The canonical workspace path, as requests name it.
    workspace_path: String,
    pub mode: Mode,
    pub settings: Settings,
    pub commands: std::collections::BTreeMap<String, super::commands::Command>,
    pub theme: text::Theme,
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
    pub last_run: Option<String>,
    /// Spent in this session: designs, runs and learning.
    pub spent_usd: f64,
    /// Calls whose usage or price is still unknown; spending is a subtotal.
    pub uncounted_calls: u32,
    /// Runs memory is learning from.
    pub learning: HashSet<String>,
    /// Results the user kept in their forks; never removed.
    pub kept: Vec<String>,
    /// The result waiting to be applied, to commit once it is: `/commit on`.
    to_commit: Option<Action>,
    /// The diff of the last task that finished, for `/last`.
    last: Option<LastDiff>,
    /// The attempts of the last task that finished, for `/lanes`.
    last_lanes: Vec<Lane>,
    /// The conversation so far: the latest finished tasks, oldest first,
    /// which each new task is told about. `/new` empties it.
    pub earlier: Vec<Earlier>,
    /// Values never to show, such as API keys: replaced wherever text from
    /// commands is shown.
    pub secrets: Vec<String>,
    /// When confirm mode stops.
    pub rules: ConfirmSection,
    /// The attempt drawn alone, large.
    pub zoom: Option<u32>,
    /// A short message in the status bar, until the next key.
    pub flash: Option<String>,
    pub tick: u64,
    /// The tick of a first Ctrl+C or Ctrl+D; a second soon after quits.
    quit_armed: Option<u64>,
    pub quit: bool,
    /// The tick of the last key or paste.
    last_key: Option<u64>,
    /// Whether the terminal has focus; `None` until it says.
    focused: Option<bool>,
    /// How the last task ended, for an alert: a short label.
    ended: Option<String>,
    /// The workspace's files for `@` completion, once listed.
    pub files: Option<Vec<String>>,
    files_complete: bool,
    /// The files may have changed since they were listed.
    files_stale: bool,
    listing: bool,
    /// The suggestion picked with the arrows.
    pub pick: usize,
    /// An `@` query whose suggestions Esc put away.
    hidden: Option<String>,
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
            commands: Default::default(),
            theme: text::Theme::detect(std::env::var("COLORTERM").ok().as_deref()),
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
            last_run: None,
            spent_usd: 0.0,
            uncounted_calls: 0,
            learning: HashSet::new(),
            kept: Vec::new(),
            to_commit: None,
            last: None,
            last_lanes: Vec::new(),
            earlier: Vec::new(),
            secrets: Vec::new(),
            rules: ConfirmSection::default(),
            zoom: None,
            flash: None,
            tick: 0,
            quit_armed: None,
            quit: false,
            last_key: None,
            focused: None,
            ended: None,
            files: None,
            files_complete: false,
            files_stale: true,
            listing: false,
            pick: 0,
            hidden: None,
        }
    }

    pub fn load_commands(&mut self, workspace: &std::path::Path) {
        let (commands, warnings) = super::commands::load(workspace);
        self.commands = commands;
        for warning in warnings {
            self.push(Line::from(span(clean_line(&warning), Tone::Warn)));
        }
    }

    pub fn command_candidates(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<_> = super::commands::BUILTINS
            .iter()
            .copied()
            .chain(self.commands.keys().map(String::as_str))
            .filter(|name| name.starts_with(prefix))
            .map(str::to_owned)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn memory_up(&self) -> bool {
        matches!(&self.session, SessionState::Up { memory_problem: None, .. })
    }

    fn push(&mut self, line: Line<'static>) {
        self.log.push(line);
    }

    /// A task in the log, as typed: its lines under the prompt mark.
    fn echo(&mut self, task: &str) {
        for (i, l) in text::clean(task).into_iter().enumerate() {
            let mark = if i == 0 { span("› ", Tone::Shell) } else { span("  ", Tone::Plain) };
            self.push(Line::from(vec![mark, span(l, Tone::Plain)]));
        }
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
        let before = self.attention();
        if matches!(msg, Msg::Ran { .. } | Msg::Applied(_) | Msg::Stopped { .. }) {
            self.files_stale = true;
        }
        let mut actions = self.handle(msg);
        if !self.listing && self.files_stale && self.at_query().is_some() {
            self.listing = true;
            self.files_stale = false;
            actions.push(Action::ListFiles);
        }
        let now = self.attention();
        let text = match (now, self.ended.take()) {
            (Some(now), _) if Some(now) != before && self.settings.notify != Notify::Off => Some(match now {
                Attention::Check => "molt: check ready".to_owned(),
                Attention::Apply => "molt: result ready".to_owned(),
                Attention::Down => "molt: services down".to_owned(),
            }),
            (_, Some(ended)) if self.settings.notify == Notify::All => Some(ended),
            _ => None,
        };
        if let Some(text) = text.filter(|_| !self.looking()) {
            actions.push(Action::Alert { text });
        }
        actions
    }

    fn attention(&self) -> Option<Attention> {
        match (&self.session, &self.prompt) {
            (SessionState::Down(_), _) => Some(Attention::Down),
            (_, Some(Prompt::Check(_))) => Some(Attention::Check),
            (_, Some(Prompt::Apply { .. })) => Some(Attention::Apply),
            _ => None,
        }
    }

    /// The user is at the terminal: it has focus, or a key came just now.
    fn looking(&self) -> bool {
        self.focused == Some(true) || self.last_key.is_some_and(|at| self.tick.wrapping_sub(at) <= LOOKING_TICKS)
    }

    /// The `@path` being typed where the cursor is: where its `@` is, in
    /// characters, and what follows it.
    fn at_query(&self) -> Option<(usize, String)> {
        if !matches!(self.screen, Screen::Main) || self.prompt.is_some() || self.task.is_some() {
            return None;
        }
        let before: Vec<char> = self.input.text.chars().take(self.input.cursor).collect();
        let start = before.iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1);
        (before.get(start) == Some(&'@')).then(|| (start, before[start + 1..].iter().collect()))
    }

    /// Workspace files matching the `@path` being typed, best first.
    pub fn suggestions(&self) -> Vec<String> {
        let (Some((_, query)), Some(files)) = (self.at_query(), &self.files) else { return Vec::new() };
        if self.hidden.as_ref() == Some(&query) {
            return Vec::new();
        }
        matching(files, &query)
    }

    /// Put `path` in place of the `@path` being typed.
    fn complete(&mut self, path: &str) {
        let Some((start, _)) = self.at_query() else { return };
        let chars: Vec<char> = self.input.text.chars().collect();
        let head: String = chars[..start].iter().collect();
        let tail: String = chars[self.input.cursor..].iter().collect();
        let put = format!("@{path} ");
        self.input.cursor = start + put.chars().count();
        self.input.text = format!("{head}{put}{}", tail.trim_start_matches(' '));
        self.pick = 0;
    }

    /// The files a task points at with `@path`, or why one cannot be used.
    fn referenced(&self, line: &str) -> Result<Vec<String>, String> {
        let mut files: Vec<String> = Vec::new();
        for word in line.split_whitespace() {
            let Some(path) = word.strip_prefix('@') else { continue };
            let path = path.trim_end_matches([',', '.', ';', ':', '!', '?', ')', '`', '\'', '"']);
            let path = path.strip_prefix("./").unwrap_or(path);
            if path.is_empty() {
                continue;
            }
            let inside = std::path::Path::new(path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
            if !inside {
                return Err(format!("@{path} is outside the workspace"));
            }
            if let Some(listed) = self.files.as_ref().filter(|_| self.files_complete) {
                if !listed.iter().any(|f| f == path) {
                    return Err(format!("@{path}: no such file in the workspace"));
                }
            }
            if !files.iter().any(|f| f == path) {
                files.push(path.to_owned());
            }
        }
        Ok(files)
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

    fn handle(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) if key.kind == KeyEventKind::Release => Vec::new(),
            Msg::Key(key) => {
                self.flash = None;
                self.last_key = Some(self.tick);
                self.key(key)
            }
            Msg::Focus(focused) => {
                self.focused = Some(focused);
                Vec::new()
            }
            Msg::Files { files, complete } => {
                self.files = Some(files);
                self.files_complete = complete;
                self.listing = false;
                Vec::new()
            }
            Msg::Paste(text) => {
                self.last_key = Some(self.tick);
                match &mut self.screen {
                    Screen::Memory(view) => match &mut view.edit {
                        Some(MemoryEdit::Filter(input) | MemoryEdit::Reason(input) | MemoryEdit::Promote(input)) => {
                            input.insert(&text)
                        }
                        Some(MemoryEdit::Change(change)) => {
                            if let Some(input) = change.input_mut() {
                                input.insert(&text);
                            }
                        }
                        None => {}
                    },
                    // The check's command is one line; a task can be many.
                    Screen::Main if matches!(self.prompt, Some(Prompt::EditCheck(_))) => self.input.insert(&text),
                    Screen::Main => self.input.paste(&text),
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
            Msg::Stopped { kept } => {
                if !kept.is_empty() {
                    self.push(Line::from(span(format!("  kept {} · interrupted", kept.len()), Tone::Dim)));
                    for fork in &kept {
                        self.push(Line::from(span(format!("  kept in {}", clean_line(fork)), Tone::Dim)));
                    }
                    self.kept.extend(kept);
                }
                self.session = SessionState::Starting;
                vec![Action::StartSession]
            }
            Msg::Progress(event) => {
                self.progress(event);
                Vec::new()
            }
            Msg::Designed { trace, result, existing } => self.designed(trace, result, &existing),
            Msg::Ran { trace, result } => self.ran(trace, result),
            Msg::Learned { trace, result } => {
                self.learning.remove(trace.as_str());
                self.learned(result);
                Vec::new()
            }
            Msg::Controlled { trace, action, result } => {
                self.controlled(&trace, &action, result);
                Vec::new()
            }
            Msg::Applied(result) => {
                let commit = self.to_commit.take().filter(|_| result.is_ok());
                match result {
                    Ok(()) => {
                        self.push(Line::from(span("  applied", Tone::Ok)));
                        if let (Some(task), Some(last)) = (&self.task, self.earlier.last_mut()) {
                            if last.task == task.req.task {
                                last.applied = true;
                            }
                        }
                    }
                    Err(e) => self.error("could not apply", &e),
                }
                self.task = None;
                commit.into_iter().collect()
            }
            Msg::Committed(result) => {
                match result {
                    Ok(Some(c)) => {
                        let on =
                            c.branch.as_deref().map_or_else(|| "a detached HEAD".to_owned(), |b| format!("on {b}"));
                        let line = format!("  committed {} {on}: {}", &c.sha[..12.min(c.sha.len())], c.subject);
                        self.push(Line::from(span(clean_line(&line), Tone::Ok)));
                        if !c.ignored.is_empty() {
                            let line = format!("  left out, as git ignores them: {}", c.ignored.join(", "));
                            self.push(Line::from(span(clean_line(&line), Tone::Dim)));
                        }
                    }
                    Ok(None) => self.push(Line::from(span("  nothing to commit", Tone::Dim))),
                    Err(e) => self.error("not committed; the result is applied", &e),
                }
                Vec::new()
            }
            Msg::Discarded(result) => {
                if let Err(e) = result {
                    self.error("could not remove the result", &e);
                }
                Vec::new()
            }
            Msg::Edited(result) => {
                match result {
                    Ok(text) => self.input = Input::with(text.trim_end_matches('\n')),
                    Err(e) => self.flash = Some(format!("the editor failed: {}", clean_line(&e))),
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
                self.reload_notes()
            }
            Msg::ChangedNote(result) => {
                let Screen::Memory(view) = &mut self.screen else { return Vec::new() };
                view.loading = false;
                match result {
                    Ok(response) => {
                        self.flash = Some(format!("{} revision {} saved", response.note.id, response.note.rev));
                        view.loading = true;
                        vec![Action::LoadNotes { query: view.query.clone() }]
                    }
                    Err(error) => {
                        view.error = Some(clean_line(&error));
                        Vec::new()
                    }
                }
            }
            Msg::UsedNotes { run, result } => {
                let Some(viewer) = self.viewer.as_mut().filter(|v| v.title == format!("memory used: {run}")) else {
                    return Vec::new();
                };
                viewer.lines = match result {
                    Ok(snapshots) => snapshot_lines(&snapshots),
                    Err(error) => block(&error, "", Tone::Bad),
                };
                Vec::new()
            }
            Msg::Remembered(result) => {
                self.flash = Some(match result {
                    Ok(id) => format!("remembered {id} for every project"),
                    Err(e) => format!("could not remember that: {}", clean_line(&e)),
                });
                self.reload_notes()
            }
            Msg::Promoted { id, result } => {
                self.flash = Some(match result {
                    Ok(copy) => format!("{id} now holds in every project as {copy}"),
                    Err(e) => format!("could not promote {id}: {}", clean_line(&e)),
                });
                self.reload_notes()
            }
        }
    }

    /// Load the notes again when the memory screen is open.
    fn reload_notes(&mut self) -> Vec<Action> {
        match &mut self.screen {
            Screen::Memory(view) => {
                view.loading = true;
                vec![Action::LoadNotes { query: view.query.clone() }]
            }
            Screen::Main => Vec::new(),
        }
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Action> {
        if ctrl(&key, 'l') {
            return vec![Action::Redraw];
        }
        if ctrl(&key, 'z') {
            return vec![Action::Suspend];
        }
        if ctrl(&key, 'd') || (ctrl(&key, 'c') && self.task.is_none() && self.input.text.is_empty()) {
            if self.quit_armed.is_some_and(|at| self.tick.wrapping_sub(at) <= QUIT_TICKS) {
                self.quit = true;
                return vec![Action::Quit { keep: self.keep() }];
            }
            self.quit_armed = Some(self.tick);
            self.flash = Some(self.quit_warning(if ctrl(&key, 'd') { "ctrl+d" } else { "ctrl+c" }));
            return Vec::new();
        }
        self.quit_armed = None;
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
            Some(Prompt::Retry { .. }) => {
                if let Some(actions) = self.retry_key(&key) {
                    return actions;
                }
            }
            Some(Prompt::EditCheck(_)) | None => {}
        }
        let editing_check = matches!(self.prompt, Some(Prompt::EditCheck(_)));
        // While an `@path` has suggestions, the arrows pick one, Tab puts it
        // in and Esc puts them away; every other key edits as usual.
        let suggestions = self.suggestions();
        if !suggestions.is_empty() && key.modifiers.is_empty() {
            let n = suggestions.len();
            let at = self.pick.min(n - 1);
            match key.code {
                KeyCode::Up => self.pick = (at + n - 1) % n,
                KeyCode::Down => self.pick = (at + 1) % n,
                KeyCode::Tab => self.complete(&suggestions[at]),
                KeyCode::Esc => self.hidden = self.at_query().map(|(_, q)| q),
                _ => {}
            }
            if matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Esc) {
                return Vec::new();
            }
        }
        self.pick = 0;
        match key.code {
            KeyCode::BackTab => {
                self.mode = self.mode.next();
                return Vec::new();
            }
            KeyCode::Tab => {
                if !editing_check && self.input.text.starts_with('/') && !self.input.text.contains(char::is_whitespace)
                {
                    let names = self.command_candidates(&self.input.text[1..]);
                    if names.len() == 1 {
                        self.input = Input::with(&format!("/{} ", names[0]));
                    } else if !names.is_empty() {
                        self.flash = Some(names.iter().map(|n| format!("/{n}")).collect::<Vec<_>>().join(" · "));
                    }
                    return Vec::new();
                }
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
            // Home and End move the cursor while there is text to move it in.
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) || self.input.text.is_empty() => {
                // The view clamps this to the top of the log.
                self.scroll = usize::MAX;
                return Vec::new();
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) || self.input.text.is_empty() => {
                self.scroll = 0;
                return Vec::new();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) && !editing_check => {
                self.input.newline();
                return Vec::new();
            }
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) && !editing_check => {
                self.input.newline();
                return Vec::new();
            }
            // A backslash before Enter continues the task on a new line, for terminals that send no Shift+Enter.
            KeyCode::Enter if !editing_check && self.input.cursor > 0 && self.input.key_before() == Some('\\') => {
                self.input.key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
                self.input.newline();
                return Vec::new();
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) && !editing_check => {
                return vec![Action::Edit { text: self.input.full() }];
            }
            KeyCode::Up if self.prompt.is_none() => {
                if !self.input.line_step(true) {
                    self.history_step(true);
                }
                return Vec::new();
            }
            KeyCode::Down if self.prompt.is_none() => {
                if !self.input.line_step(false) {
                    self.history_step(false);
                }
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

    /// What a first Ctrl+C or Ctrl+D says: press again, and what quitting keeps.
    fn quit_warning(&self, keys: &str) -> String {
        let mut parts = vec![format!("{keys} again to quit")];
        if self.busy() {
            parts.push("the running task stops".into());
        }
        let keep = self.keep().len();
        match keep {
            0 => {}
            1 => parts.push("1 result stays in its fork".into()),
            n => parts.push(format!("{n} results stay in their forks")),
        }
        parts.join("; ")
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
        let mut attempts = Vec::new();
        let mut check_files = Vec::new();
        if let Some(mut task) = self.task.take() {
            if matches!(task.phase, Phase::Running) {
                attempts = task.lanes.iter().filter_map(|l| l.fork.clone()).collect();
                check_files = task.req.check_files.iter().map(|f| f.path.clone()).collect();
                self.last_lanes = std::mem::take(&mut task.lanes);
                // Starting again needs the check in full; a designed one is only known once accepted.
                if task.req.check.is_some() {
                    let designed = self.settings.check.as_deref() != task.req.check.as_deref();
                    self.prompt =
                        Some(Prompt::Retry { req: Box::new(task.req), designed, stopped: true, attempts: Vec::new() });
                }
            }
        }
        self.zoom = None;
        self.learning.clear();
        self.push(Line::from(span("■ stopped; restarting the services", Tone::Warn)));
        self.session = SessionState::Starting;
        vec![Action::Interrupt { keep, attempts, check_files }]
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
        self.submit_task(line)
    }

    fn submit_task(&mut self, line: String) -> Vec<Action> {
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
        if let Some(n) = self.settings.attempts {
            req.attempts = n;
        }
        req.model = self.settings.model.clone();
        req.effort = self.settings.effort;
        req.budget_usd = self.settings.budget_usd;
        req.apply = self.mode == Mode::Auto;
        req.earlier = self.earlier.clone();
        req.stream = self.settings.stream;

        self.gap();
        self.echo(&line);
        let mut task = Task::new(trace.clone(), req, Phase::Running);
        self.last_run = Some(trace.to_string());
        let action = if let Some(check) = &self.settings.check {
            task.req.check = Some(check.clone());
            Action::Run { req: task.req.clone(), trace }
        } else if self.mode == Mode::Confirm {
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
    fn starting(&self, trace: TraceId, action: Action) -> Vec<Action> {
        if self.settings.commit {
            vec![Action::PrepareCommit { trace }, action]
        } else {
            vec![action]
        }
    }

    fn command(&mut self, command: &str) -> Vec<Action> {
        let (name, arg) = command.split_once(' ').map_or((command, ""), |(n, a)| (n, a.trim()));
        let set = |what: &str, value: &str| format!("{what}: {value}");
        match name {
            "memory-used" => {
                let run = if arg.is_empty() { self.last_run.clone() } else { Some(arg.to_owned()) };
                return self.open_used(run);
            }
            "theme" => {
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
            "help" => {
                self.viewer = Some(Viewer { title: "help".into(), lines: self.help(), scroll: 0 });
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
            "remember" => {
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
            "check" if self.running() => {
                if arg.is_empty() {
                    self.flash = Some("the running task keeps a check; give the command to use instead".into());
                    return Vec::new();
                }
                self.echo_constraint(&format!("check: {arg}"));
                return self
                    .steer(Control::Constrain(Constraint { check: Some(arg.to_owned()), ..Default::default() }));
            }
            "protect" => {
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
            "pause" => return self.steer(Control::Pause),
            "resume" => return self.steer(Control::Resume),
            "new" => {
                self.earlier.clear();
                self.flash = Some("a new conversation: the next task starts without the earlier ones".into());
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
            "commit" => {
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
            "stream" => {
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
            "notify" => {
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
            "model" => {
                self.settings.model = (!arg.is_empty() && arg != "default").then(|| arg.to_owned());
                self.flash = Some(set("model", self.settings.model.as_deref().unwrap_or("default")));
            }
            "memory-model" => {
                self.settings.memory_model = (!arg.is_empty() && arg != "default").then(|| arg.to_owned());
                let shown = match self.settings.memory_model.as_deref() {
                    None => "default",
                    Some(agent::SAME_MODEL) => "the attempts' model",
                    Some(m) => m,
                };
                self.flash = Some(set("memory model", shown));
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
            "lanes" => {
                if self.last_lanes.is_empty() {
                    self.flash = Some("no task has finished yet".into());
                } else {
                    self.viewer =
                        Some(Viewer { title: "attempts".into(), lines: lane_lines(&self.last_lanes), scroll: 0 });
                }
            }
            "last" => match &self.last {
                Some(last) => self.viewer = Some(diff_viewer(&last.patch, last.truncated)),
                None => self.flash = Some("no task has finished with changes yet".into()),
            },
            "clear" => {
                self.log.clear();
                self.scroll = 0;
            }
            "quit" | "exit" => {
                self.quit = true;
                return vec![Action::Quit { keep: self.keep() }];
            }
            other => {
                if let Some(template) = self.commands.get(other).cloned() {
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
                    let mut actions = self.submit_task(expanded);
                    self.settings = saved;
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
                                }));
                                actions.clear();
                            }
                        }
                    }
                    return actions;
                }
                self.flash = Some(format!("no command /{}; /help lists them", clean_line(other)));
            }
        }
        Vec::new()
    }

    fn help(&self) -> Vec<Line<'static>> {
        let mut lines = help();
        if !self.commands.is_empty() {
            lines.push(Line::from(span("project commands", Tone::Dim)));
            for (name, c) in &self.commands {
                lines.push(Line::from(vec![
                    span(format!("  /{name} {}  ", clean_line(&c.hint)), Tone::Shell),
                    span(clean_line(&c.description), Tone::Plain),
                ]));
            }
        }
        lines
    }

    /// The attempts of a task are running.
    fn running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| t.phase == Phase::Running)
    }

    /// What the user added to a running task, in the log.
    fn echo_constraint(&mut self, text: &str) {
        for (i, l) in text::clean(text).into_iter().enumerate() {
            let mark = if i == 0 { span("+ ", Tone::Shell) } else { span("  ", Tone::Plain) };
            self.push(Line::from(vec![mark, span(l, Tone::Plain)]));
        }
    }

    /// Send `action` to the running task.
    fn steer(&mut self, action: Control) -> Vec<Action> {
        let Some(task) = self.task.as_ref().filter(|t| t.phase == Phase::Running) else {
            self.flash = Some("no task is running".into());
            return Vec::new();
        };
        vec![Action::Control { trace: task.trace.clone(), action }]
    }

    fn controlled(&mut self, trace: &TraceId, action: &Control, result: Result<ControlResponse, String>) {
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
    fn remember(&mut self, task: &str, resp: &RunResponse) {
        let summary = scrub(&resp.summary, &self.secrets);
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

    fn progress(&mut self, event: Progress) {
        let Some(task) = &mut self.task else { return };
        if event.run() != task.trace.as_str() {
            return;
        }
        match event {
            Progress::CheckReady { command, files, designed, rubric, .. } => {
                if task.check_shown {
                    return;
                }
                task.check_shown = true;
                let mut lines = vec![check_head(designed)];
                match command {
                    Some(c) => lines.push(Line::from(vec![span("  $ ", Tone::Dim), span(clean_line(&c), Tone::Plain)])),
                    None if !rubric.is_empty() => lines.extend(rubric_lines(&rubric)),
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
                self.log.push(Line::from(span(
                    format!("  /memory-used {} · recorded recall context", task.trace),
                    Tone::Dim,
                )));
                for n in &notes {
                    self.log.push(Line::from(vec![
                        span(format!("  {:.2} ", n.confidence), Tone::Mem),
                        span(clean_line(&n.text), Tone::Plain),
                    ]));
                }
            }
            event @ (Progress::ModelStarted { .. } | Progress::ModelText { .. } | Progress::ModelFinished { .. }) => {
                let context = event.model_call().expect("a model event has a context");
                let attempt = context.attempt.filter(|&a| a < task.req.attempts && a < 8);
                let settles = matches!(event, Progress::ModelFinished { .. });
                if let Some(attempt) = attempt.filter(|_| task.req.stream || settles) {
                    if !task.lanes.iter().any(|l| l.index == attempt) {
                        task.lanes.push(Lane::new(attempt, None));
                        task.lanes.sort_by_key(|l| l.index);
                    }
                }
                // Counted even when previews are off or the lane has ended:
                // a cancelled attempt's last call settles after it stops.
                if let Progress::ModelFinished { cost_usd, .. } = &event {
                    if task.counted.insert(context.call.clone()) {
                        match (context.attempt, attempt) {
                            (None, _) => task.design_spend.add(*cost_usd),
                            (_, Some(attempt)) => {
                                lane(task, attempt).expect("the lane was created").spend.add(*cost_usd)
                            }
                            _ => {}
                        }
                    }
                }
                if !task.req.stream {
                    return;
                }
                match context.attempt {
                    Some(attempt) if attempt < task.req.attempts && attempt < 8 => {
                        let lane = lane(task, attempt).expect("the lane was created");
                        if context.turn < lane.turn || lane.status.is_some() {
                            return;
                        }
                        lane.turn = context.turn;
                        for line in lane.preview.feed(&event) {
                            lane.push(line);
                        }
                    }
                    None => self.log.extend(task.designer_preview.feed(&event)),
                    _ => {}
                }
            }
            Progress::Note { message, .. } => {
                if task.notes_seen.insert(message.clone()) {
                    self.log.push(Line::from(span(format!("  {}", clean_line(&message)), Tone::Dim)));
                }
            }
            Progress::AttemptStarted { attempt, fork, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.fork = lane.fork.take().or(fork);
                } else {
                    task.lanes.push(Lane::new(attempt, fork));
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
            Progress::CheckRan { attempt, passed, exit_code, tail, judged, .. } => {
                let tail = scrub(&tail, &self.secrets);
                if let Some(lane) = lane(task, attempt) {
                    let line = match (passed, exit_code) {
                        (true, _) if judged => span("   judge: criteria met", Tone::Ok),
                        (false, _) if judged => span("   judge: criteria not met", Tone::Bad),
                        (true, _) => span("   check passed", Tone::Ok),
                        (false, Some(code)) => span(format!("   check failed (exit {code})"), Tone::Bad),
                        (false, None) => span("   check failed (killed or timed out)", Tone::Bad),
                    };
                    lane.push(Line::from(line));
                    for l in last_lines(&tail, CHECK_TAIL_LINES) {
                        lane.push(Line::from(span(format!("   {l}"), Tone::Dim)));
                    }
                }
            }
            Progress::AttemptFinished { attempt, status, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    if let Some(line) = lane.preview.finish("ended; final preview event unavailable", true) {
                        lane.push(line);
                    }
                    lane.status = Some(status);
                    lane.ended.get_or_insert_with(Instant::now);
                }
            }
            Progress::Required { requirement, .. } => task.version = task.version.max(requirement.version),
            Progress::Adopted { attempt, version, .. } => {
                if let Some(lane) = lane(task, attempt) {
                    lane.push(Line::from(span(format!("   now follows v{version}"), Tone::Warn)));
                }
            }
            Progress::State { state, waiting, .. } => {
                task.state = state;
                task.waiting = waiting;
            }
        }
    }

    fn designed(&mut self, trace: TraceId, result: Result<DesignResponse, String>, existing: &[String]) -> Vec<Action> {
        let Some(task) = &mut self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        self.log.extend(task.designer_preview.finish("ended; final preview event unavailable", true));
        match result {
            Ok(d) => {
                task.design_spend = Spend { usd: d.cost_usd, unknown: d.uncounted_calls > 0 };
                self.spent_usd += d.cost_usd;
                self.uncounted_calls = self.uncounted_calls.saturating_add(d.uncounted_calls);
                if d.cancelled || d.error.is_some() {
                    self.task = None;
                    self.error(
                        "no check",
                        &format!(
                            "{} · ${:.2} known · {} calls uncounted",
                            d.error.as_deref().unwrap_or("design cancelled"),
                            d.cost_usd,
                            d.uncounted_calls
                        ),
                    );
                    return Vec::new();
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

    fn ran(&mut self, trace: TraceId, result: Result<RunResponse, String>) -> Vec<Action> {
        let Some(task) = &self.task else { return Vec::new() };
        if task.trace != trace {
            return Vec::new();
        }
        let mut task = self.task.take().expect("checked above");
        self.zoom = None;
        let now = Instant::now();
        for lane in &mut task.lanes {
            lane.ended.get_or_insert(now);
        }
        self.last_lanes = std::mem::take(&mut task.lanes);
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                self.ended = Some("molt: run failed".to_owned());
                self.error("the run failed", &e);
                return Vec::new();
            }
        };
        // Progress can be dropped by a busy transport or suspended interface.
        // The final attempt reports settle the archived lane totals as well.
        for lane in &mut self.last_lanes {
            if let Some(attempt) = resp.attempts.iter().find(|a| a.index == lane.index) {
                lane.spend.usd = attempt.cost_usd;
                lane.spend.unknown |= resp.uncounted_calls > 0;
                lane.status = Some(attempt.status);
                lane.turn = attempt.turns;
            }
        }
        self.spent_usd += resp.cost_usd;
        self.ended = Some(match resp.outcome {
            Outcome::Passed => match resp.changes.len() {
                1 => "molt: passed · 1 file".to_owned(),
                n => format!("molt: passed · {n} files"),
            },
            Outcome::Judged => "molt: passed · judged by a model".to_owned(),
            Outcome::Unverified => "molt: done, unverified".to_owned(),
            Outcome::Failed => "molt: failed".to_owned(),
        });
        self.remember(&task.req.task, &resp);
        self.uncounted_calls = self.uncounted_calls.saturating_add(resp.uncounted_calls);
        let mut actions = Vec::new();
        let memory_model = agent::memory_model(self.settings.memory_model.as_deref(), task.req.model.as_deref());

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
            if matches!(a.status, AttemptStatus::Failed | AttemptStatus::Error) {
                for l in last_lines(&scrub(&a.note, &self.secrets), NOTE_LINES) {
                    self.push(Line::from(span(format!("      {l}"), Tone::Dim)));
                }
            }
        }

        if let Some(selection) = &resp.selection {
            let line = format!("  picked among {} passing attempts:", selection.candidates.len());
            self.push(Line::from(span(line, Tone::Dim)));
            for c in &selection.candidates {
                let (mark, tone) = if resp.winner == Some(c.attempt) { ("*", Tone::Ok) } else { (" ", Tone::Dim) };
                self.push(Line::from(vec![
                    span(format!("    {mark} attempt {} ", c.attempt + 1), tone),
                    span(agent::candidate_scores(c), Tone::Dim),
                ]));
            }
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
        let summary = scrub(&resp.summary, &self.secrets);
        let (summary, cut) = bounded(summary.trim(), SUMMARY_BYTES);
        let cut_note = cut.then(|| {
            let note = format!("  (cut at 1 MB; the whole text is in the audit log, trace {trace})");
            Line::from(span(note, Tone::Warn))
        });
        match resp.outcome {
            Outcome::Failed => {
                self.push(Line::from(span("✗ no attempt passed", Tone::Bad)));
                self.log.extend(block(summary, "  ", Tone::Dim));
                self.log.extend(cut_note);
            }
            Outcome::Passed | Outcome::Unverified | Outcome::Judged => {
                let tone = if resp.outcome == Outcome::Passed { Tone::Ok } else { Tone::Warn };
                let mut head = vec![span(format!("✓ {}", outcome_name(resp.outcome)), tone)];
                if !resp.changes.is_empty() {
                    head.extend(size);
                }
                self.push(Line::from(head));
                self.log.extend(block(summary, "  ", Tone::Plain));
                self.log.extend(cut_note);
                for c in &resp.changes {
                    let kind = match c.kind {
                        ChangeKind::Added => "added   ",
                        ChangeKind::Modified => "modified",
                        ChangeKind::Deleted => "deleted ",
                    };
                    self.push(Line::from(span(format!("  {kind} {}", clean_line(&c.path)), Tone::Dim)));
                }
                for f in &resp.judgement {
                    let (mark, tone) = if f.met { ("✓", Tone::Ok) } else { ("✗", Tone::Bad) };
                    self.push(Line::from(vec![
                        span(format!("  {mark} "), tone),
                        span(clean_line(&f.criterion), Tone::Dim),
                    ]));
                }
            }
        }
        if resp.uncounted_calls > 0 {
            let line = format!("  {} model calls of cancelled attempts are not in the cost", resp.uncounted_calls);
            self.push(Line::from(span(line, Tone::Dim)));
        }

        if !resp.patch.is_empty() {
            self.last = Some(LastDiff { patch: resp.patch.clone(), truncated: resp.patch_truncated });
        }
        let mut deciding = false;
        let mut applying = false;
        self.to_commit =
            (self.settings.commit && resp.outcome != Outcome::Failed && !resp.changes.is_empty()).then(|| {
                Action::Commit { trace: trace.clone(), task: task.req.task.clone(), resp: Box::new(resp.clone()) }
            });
        if resp.applied {
            self.push(Line::from(span("  applied", Tone::Ok)));
            actions.extend(self.to_commit.take());
        } else if let Some(fork) = resp.fork.clone() {
            if resp.changes.is_empty() {
                actions.push(Action::Discard { fork });
            } else if self.mode == Mode::Confirm && !task.req.apply && !matches!(resp.outcome, Outcome::Failed) {
                let stops = apply_stops(&resp, &self.rules);
                if stops.is_empty() && !self.rules.always_ask {
                    applying = true;
                    actions.push(Action::Apply { fork });
                } else {
                    deciding = true;
                    task.stops = stops;
                    self.prompt = Some(Prompt::Apply { resp: Box::new(resp.clone()), fork });
                }
            } else {
                self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
                self.kept.push(fork);
            }
        }
        if !(deciding || applying) {
            self.to_commit = None;
        }
        if deciding {
            self.task = Some(Task { phase: Phase::Deciding, lanes: Vec::new(), ..task });
        } else if applying {
            self.task = Some(Task { phase: Phase::Applying, lanes: Vec::new(), stops: Vec::new(), ..task });
        } else if resp.outcome == Outcome::Failed && !resp.attempts.is_empty() {
            // Only a check known in full can be run again without designing it anew.
            let mut req = task.req;
            if req.check.is_none() {
                if let Some(spec) = resp.check.as_ref().filter(|c| c.files.is_empty()) {
                    req.check = Some(spec.command.clone());
                }
            }
            if req.check.is_some() {
                let designed = resp.check.as_ref().is_some_and(|c| c.designed);
                self.prompt = Some(Prompt::Retry {
                    req: Box::new(req),
                    designed,
                    stopped: false,
                    attempts: resp.attempts.clone(),
                });
            }
        }

        // Memory learns from failures too; the run's budget covers it.
        let spent_all = task_budget(&self.settings).is_some_and(|b| resp.cost_usd >= b);
        if self.memory_up() && !spent_all {
            self.learning.insert(trace.to_string());
            actions.push(Action::Learn { trace, model: memory_model });
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
                self.to_commit = None;
                self.task = None;
                self.push(Line::from(span("  discarded", Tone::Dim)));
                vec![Action::Discard { fork }]
            }
            KeyCode::Char('d') => {
                self.viewer = Some(diff_viewer(&resp.patch, resp.patch_truncated));
                Vec::new()
            }
            KeyCode::Char('k') | KeyCode::Esc => {
                let fork = fork.clone();
                self.prompt = None;
                self.to_commit = None;
                self.task = None;
                self.push(Line::from(span(format!("  kept in {}", clean_line(&fork)), Tone::Dim)));
                self.kept.push(fork);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Keys while a failed run offers a retry: the actions, or `None` to
    /// handle the key as usual, once the offer is gone.
    fn retry_key(&mut self, key: &KeyEvent) -> Option<Vec<Action>> {
        let Some(Prompt::Retry { attempts, .. }) = &self.prompt else { return None };
        let empty = self.input.text.is_empty();
        match key.code {
            KeyCode::Char('r') if empty && key.modifiers.is_empty() => {
                if !matches!(self.session, SessionState::Up { .. }) {
                    self.flash = Some("the services are not up".into());
                    return Some(Vec::new());
                }
                let Some(Prompt::Retry { req, designed, .. }) = self.prompt.take() else { return None };
                Some(self.retry(*req, designed))
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
    fn retry(&mut self, req: RunRequest, designed: bool) -> Vec<Action> {
        let trace = TraceId::random();
        self.gap();
        self.echo(&req.task.clone());
        let mut lines = vec![check_head(designed)];
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
        self.task = Some(Task { check_shown: true, ..Task::new(trace.clone(), req.clone(), Phase::Running) });
        self.starting(trace.clone(), Action::Run { req, trace })
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
                if !r.proposed.is_empty() {
                    parts.push(format!("proposed {} for every project (/memory, p)", count(r.proposed.len())));
                }
                if parts.is_empty() {
                    parts.push(match &r.skipped {
                        Some(why) => format!("nothing new: {}", clean_line(why)),
                        None => "nothing new".into(),
                    });
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

    fn open_used(&mut self, run: Option<String>) -> Vec<Action> {
        let Some(run) = run else {
            self.flash = Some("no task has run yet; use /memory-used RUN".into());
            return Vec::new();
        };
        self.viewer = Some(Viewer {
            title: format!("memory used: {run}"),
            lines: block("loading recorded context", "", Tone::Dim),
            scroll: 0,
        });
        vec![Action::UsedNotes { run }]
    }

    fn memory_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Screen::Memory(view) = &mut self.screen else { return Vec::new() };
        if let Some(MemoryEdit::Change(change)) = &mut view.edit {
            match key.code {
                KeyCode::Esc => view.edit = None,
                KeyCode::Enter => match change.advance() {
                    Ok(Some(action)) => {
                        view.edit = None;
                        view.loading = true;
                        view.error = None;
                        return vec![action];
                    }
                    Ok(None) => {}
                    Err(error) => self.flash = Some(error.into()),
                },
                KeyCode::BackTab => {
                    change.step = match change.step {
                        ChangeStep::Confirm => ChangeStep::Dependencies,
                        ChangeStep::Dependencies => ChangeStep::Reason,
                        ChangeStep::Reason if change.text.is_some() => ChangeStep::Text,
                        step => step,
                    }
                }
                KeyCode::Char('t') if change.step == ChangeStep::Confirm => {
                    change.review.temporary = !change.review.temporary
                }
                _ => {
                    if let Some(input) = change.input_mut() {
                        input.key(key);
                    }
                }
            }
            return Vec::new();
        }
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
                    Some(MemoryEdit::Promote(mut input)) => {
                        let reason = input.take().trim().to_owned();
                        let Some(note) = view.notes.get(view.selected) else { return Vec::new() };
                        if reason.is_empty() {
                            view.edit = Some(MemoryEdit::Promote(input));
                            self.flash = Some("say why the note holds in every project".into());
                            return Vec::new();
                        }
                        return vec![Action::Promote { id: note.note.id.clone(), reason }];
                    }
                    Some(MemoryEdit::Change(_)) | None => {}
                },
                _ => {
                    if let MemoryEdit::Filter(input) | MemoryEdit::Reason(input) | MemoryEdit::Promote(input) = edit {
                        input.key(key);
                    }
                }
            }
            return Vec::new();
        }
        if view.loading && matches!(key.code, KeyCode::Char('a' | 'c' | 'f' | 'v') | KeyCode::Enter) {
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
            KeyCode::Char('p') => match view.notes.get(view.selected) {
                Some(note) if note.layer == Layer::User => {
                    self.flash = Some("that note holds everywhere already".into())
                }
                Some(_) => view.edit = Some(MemoryEdit::Promote(Input::default())),
                None => {}
            },
            KeyCode::Char('a') | KeyCode::Char('c') => {
                if let Some(note) = view.notes.get(view.selected) {
                    view.edit = Some(MemoryEdit::Change(Box::new(NoteChange::new(
                        &self.workspace_path,
                        note,
                        key.code == KeyCode::Char('c'),
                    ))));
                }
            }
            KeyCode::Enter | KeyCode::Char('v') => {
                if let Some(note) = view.notes.get(view.selected) {
                    self.viewer = Some(Viewer {
                        title: format!("{} revision {}", clean_line(&note.note.id), note.note.rev),
                        lines: memory_lines(note),
                        scroll: 0,
                    });
                }
            }
            KeyCode::Char('u') => return self.open_used(self.last_run.clone()),
            KeyCode::Char('r') => {
                view.loading = true;
                return vec![Action::LoadNotes { query: view.query.clone() }];
            }
            _ => {}
        }
        Vec::new()
    }
}

/// Shared by the detail pane and its scrollable full view. Every field is
/// display data, including evidence IDs, dependency paths, and note text.
pub fn memory_lines(r: &Recalled) -> Vec<Line<'static>> {
    let n = &r.note;
    let mut fields = vec![
        format!("{} revision {} · {}", n.id, n.rev, n.kind.as_str()),
        format!("score {:.3} · confidence {:.3} · recency {:.3}", r.score, r.reason.confidence, r.reason.recency),
        format!("keywords: {}", r.reason.keywords.as_deref().unwrap_or("none (confidence and recency)")),
        format!("relevance: {}", r.reason.relevance.map_or("unavailable".into(), |v| format!("{v:.3}"))),
    ];
    if r.layer == Layer::User {
        fields.push("layer: yours, for every project".into());
    } else if r.details.everywhere {
        fields.push("layer: this project · proposed for every project (p to promote)".into());
    } else if let Some(to) = &r.details.promoted_to {
        fields.push(format!("layer: this project · promoted as {to}"));
    }
    if r.details.temporary {
        fields.push("temporary experience · verify before reuse".into());
    }
    if !r.details.needs_review.is_empty() {
        fields.push(format!("needs review: {}", r.details.needs_review.join(", ")));
    }
    fields.push(format!("review reason: {}", if r.details.reason.is_empty() { "none" } else { &r.details.reason }));
    if let Some(similarity) = r.reason.similarity {
        fields.push(format!("cosine similarity: {similarity:.3}"));
    }
    if let Some(hash) = &r.reason.model_sha256 {
        fields.push(format!("embedding model SHA-256: {hash}"));
    }
    if let Some(weight) = r.reason.semantic_weight {
        fields.push(format!("semantic weight: {weight:.3}"));
    }
    for d in &r.details.dependencies {
        fields.push(format!("depends on: {} · sha256 {}", d.path, d.sha256));
    }
    if let Some(id) = &r.details.supersedes {
        fields.push(format!("supersedes: {id}"));
    }
    fields.extend([
        format!("source: {}@{} · run {}", n.provenance.service, n.provenance.version, n.provenance.trace),
        format!("learned: {} · confirmed {}× · confidence {:.2}", text::when(n.created_ms), n.reinforced, n.confidence),
        format!("disputes: {}", n.conflicts.join(", ")),
        format!("evidence: {}", n.provenance.events.join(", ")),
        String::new(),
        n.text.clone(),
    ]);
    fields.into_iter().flat_map(|field| block(&field, "", Tone::Plain)).collect()
}

fn snapshot_lines(snapshots: &[RecallSnapshot]) -> Vec<Line<'static>> {
    if snapshots.is_empty() {
        return block("no recorded memory context in this project", "", Tone::Dim);
    }
    let mut lines = Vec::new();
    for s in snapshots {
        lines.extend(block(
            &format!("run {} · call {} · {}\nquery: {}", s.run, s.call, s.purpose, s.query),
            "",
            Tone::Mem,
        ));
        if s.notes.is_empty() {
            lines.extend(block("no notes returned", "", Tone::Dim));
        }
        for r in &s.notes {
            lines.extend(memory_lines(r));
            lines.push(Line::default());
        }
    }
    lines
}

impl Lane {
    fn new(index: u32, fork: Option<String>) -> Self {
        Self {
            index,
            fork,
            turn: 0,
            status: None,
            lines: Vec::new(),
            preview: Preview::default(),
            spend: Spend::default(),
            started: Instant::now(),
            ended: None,
        }
    }

    /// How long the attempt has run, or ran.
    pub fn elapsed(&self) -> Duration {
        self.ended.unwrap_or_else(Instant::now).saturating_duration_since(self.started)
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
        if self.lines.len() > LANE_LINES {
            self.lines.remove(0);
        }
    }
}

/// The files `query` picks out, best first: those it starts, then those
/// whose name it starts, then those containing it, then those holding its
/// characters in order; shorter paths first within each.
fn matching(files: &[String], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let in_order = |path: &str| {
        let mut chars = path.chars();
        query.chars().all(|q| chars.any(|c| c == q))
    };
    let mut found: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|file| {
            let path = file.to_lowercase();
            let name = path.rsplit('/').next().unwrap_or_default();
            let rank = if path.starts_with(&query) {
                0
            } else if name.starts_with(&query) {
                1
            } else if path.contains(&query) {
                2
            } else if in_order(&path) {
                3
            } else {
                return None;
            };
            Some((rank, file.len(), file))
        })
        .collect();
    found.sort();
    found.into_iter().take(SUGGESTIONS).map(|(_, _, f)| f.clone()).collect()
}

fn lane(task: &mut Task, attempt: u32) -> Option<&mut Lane> {
    task.lanes.iter_mut().find(|l| l.index == attempt)
}

fn diff_viewer(patch: &str, truncated: bool) -> Viewer {
    let mut lines = text::diff_lines(patch);
    if truncated {
        lines.push(Line::from(span("(the diff is cut short here)", Tone::Dim)));
    }
    Viewer { title: "diff".into(), lines, scroll: 0 }
}

/// A rubric's criteria, one line each, for the log.
fn rubric_lines(rubric: &[String]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(span("  judged by a model against these criteria:", Tone::Warn))];
    for (i, c) in rubric.iter().enumerate() {
        lines.push(Line::from(span(format!("  {}. {}", i + 1, clean_line(c)), Tone::Plain)));
    }
    lines
}

/// Why a designed check needs the user's look before the attempts start.
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

/// Why a result needs the user's look before it is applied.
fn apply_stops(resp: &RunResponse, rules: &ConfirmSection) -> Vec<String> {
    let mut stops = Vec::new();
    if resp.outcome != Outcome::Passed {
        stops.push(outcome_name(resp.outcome).to_owned());
    }
    // The planner holds back what its reviewer did not approve; so does confirm mode.
    match resp.review.as_ref().map(|r| r.verdict) {
        Some(Verdict::Reject) => stops.push("review rejected".into()),
        Some(Verdict::Concern) => stops.push("review questioned".into()),
        Some(Verdict::Approve) | None => {}
    }
    let (added, removed) = text::diff_counts(&resp.patch);
    if added + removed > rules.max_lines {
        stops.push(format!("{} lines changed", added + removed));
    }
    if resp.changes.len() > rules.max_files {
        stops.push(format!("{} files", resp.changes.len()));
    }
    let deleted = resp.changes.iter().filter(|c| c.kind == ChangeKind::Deleted).count();
    match deleted {
        0 => {}
        1 => stops.push("deletes 1 file".into()),
        n => stops.push(format!("deletes {n} files")),
    }
    let touched: Vec<&str> =
        resp.changes.iter().map(|c| c.path.as_str()).filter(|p| rules.ask_paths.iter().any(|a| covers(a, p))).collect();
    match touched.as_slice() {
        [] => {}
        [one] => stops.push(format!("touches {}", clean_line(one))),
        many => stops.push(format!("touches {} protected paths", many.len())),
    }
    if resp.patch_truncated {
        stops.push("diff cut short".into());
    }
    stops
}

/// `rule` names `path`: the same path, a directory above it (`dir/`), or its file name.
fn covers(rule: &str, path: &str) -> bool {
    match rule.strip_suffix('/') {
        // A directory named alone (`migrations/`) is matched at any depth.
        Some(dir) if !dir.contains('/') => path.starts_with(rule) || path.contains(&format!("/{rule}")),
        Some(_) => path.starts_with(rule),
        None => path == rule || (!rule.contains('/') && path.rsplit('/').next() == Some(rule)),
    }
}

/// Each lane's lines under its attempt's name, for the viewer.
fn lane_lines(lanes: &[Lane]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for lane in lanes {
        lines.push(Line::from(vec![
            span(format!("attempt {} · tool calls", lane.index + 1), Tone::Shell),
            span(format!(" · {} · {}", text::duration(lane.elapsed()), lane.spend.label()), Tone::Dim),
        ]));
        lines.extend(lane.lines.iter().cloned());
        lines.push(Line::default());
    }
    lines
}

/// The last `n` non-empty lines of `text`, made safe to show.
fn last_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<String> = text::clean(text.trim_end()).into_iter().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// `text` with every secret in it replaced.
fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[redacted]");
        }
    }
    out
}

/// `text` cut to at most `max` bytes at a character boundary, and whether it was cut.
fn bounded(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
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
        row("enter (running)", "add what you typed to the running task"),
        row("pgup / pgdn", "scroll the log"),
        row("ctrl+home / end", "the start or end of the log"),
        row("shift+enter", "a new line in the task; also ctrl+j, or \\ then enter"),
        row("ctrl+l", "clear and redraw the screen"),
        row("ctrl+z", "suspend the interface; fg returns, services keep running"),
        row("ctrl+g", "write the task in $VISUAL or $EDITOR"),
        row("↑ / ↓", "the line above or below, then earlier inputs"),
        row("ctrl+d twice", "quit"),
        Line::default(),
        head("commands"),
        row("/memory [words]", "notes about this project"),
        row("/memory-used [run]", "recorded context for this run (default: latest)"),
        row("/remember TEXT", "a preference of yours, for every project"),
        row("/check [command]", "use this check for every task; empty: design one"),
        row("", "while a task runs: check its result with this instead"),
        row("/protect PATH...", "the running task must leave these as they are"),
        row("/pause, /resume", "start nothing new in the running task, then go on"),
        row("/new", "a new conversation: later tasks forget the earlier ones"),
        row("/attempts N", "attempts at once, 1 to 8"),
        row("/stream on|off", "model text previews for the next task"),
        row("@path", "point the task at a file; tab puts in the picked suggestion"),
        row("/notify on|all|off", "bell and desktop notice when waiting (all: also when a task ends)"),
        row("/commit on|off", "commit each applied result to git; `molt undo` takes it back"),
        row("/model NAME", "opus, sonnet, haiku or a model id"),
        row("/memory-model NAME", "model memory learns with; same: the attempts'"),
        row("/effort LEVEL", "low, medium, high, xhigh or max"),
        row("/budget USD", "spending limit per task"),
        row("/last", "the diff of the last finished task"),
        row("/lanes", "the tool calls of the last finished task"),
        row("/theme NAME", "dark, light or terminal ANSI colors; saved for next time"),
        row("/clear", "clear the log"),
        row("/quit", "quit"),
        Line::default(),
        head("modes"),
        row("plan only", "results stay in their forks; the workspace is not changed"),
        row("confirm", "stop for the check or the result when one needs a look"),
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
    use molt_api::planner::{AttemptReport, Baseline, CheckFile, CheckSpec, Requirement};
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

    fn frame_text(app: &mut App) -> String {
        frame_rows(app, 140).concat()
    }

    fn frame_rows(app: &mut App, width: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::super::view::draw(frame, app)).unwrap();
        let cells: Vec<String> = terminal.backend().buffer().content.iter().map(|c| c.symbol().to_owned()).collect();
        cells.chunks(usize::from(width)).map(|row| row.concat()).collect()
    }

    fn model_context(app: &App, attempt: Option<u32>, seq: u64) -> molt_api::progress::ModelCall {
        molt_api::progress::ModelCall {
            run: app.task.as_ref().unwrap().trace.to_string(),
            attempt,
            turn: 1,
            call: "fake-stream".into(),
            seq,
        }
    }

    #[test]
    fn streamed_model_text_updates_the_frame_and_folds_without_becoming_a_result() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        assert!(matches!(&actions[0], Action::Run { req, .. } if req.stream));
        let context = model_context(&app, Some(0), 0);
        app.update(Msg::Progress(Progress::ModelStarted { context }));
        let context = model_context(&app, Some(0), 1);
        assert!(app
            .update(Msg::Progress(Progress::ModelText { context, text: "latest streamed text\x1b\x07".into() }))
            .is_empty());
        let shown = frame_text(&mut app);
        assert!(shown.contains("model preview") && shown.contains("latest streamed text"), "{shown}");
        assert_eq!(app.spent_usd, 0.0, "preview events cannot settle spending");
        let context = model_context(&app, Some(0), 3);
        app.update(Msg::Progress(Progress::ModelText { context, text: " after a gap".into() }));
        assert!(frame_text(&mut app).contains("gaps in preview"));
        let context = model_context(&app, Some(0), 4);
        app.update(Msg::Progress(Progress::ModelFinished {
            context,
            usage: Some(Usage::default()),
            cost_usd: Some(0.02),
            error: None,
        }));
        let shown = frame_text(&mut app);
        assert!(shown.contains("model preview finished · gaps"), "{shown}");
        assert!(app.task.as_ref().unwrap().lanes[0].preview.lines(100, 4).is_empty());
        let context = model_context(&app, Some(0), 5);
        app.update(Msg::Progress(Progress::ModelText { context, text: "late forged result".into() }));
        assert!(!frame_text(&mut app).contains("forged result"));
        let trace = app.task.as_ref().unwrap().trace.clone();
        assert!(app.update(Msg::Ran { trace, result: Err("the model refused".into()) }).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none());
        assert!(log(&app).contains("the model refused"));
    }

    fn finished(app: &App, attempt: Option<u32>, call: &str, cost_usd: Option<f64>) -> Msg {
        let context = molt_api::progress::ModelCall { call: call.into(), ..model_context(app, attempt, 9) };
        Msg::Progress(Progress::ModelFinished { context, usage: None, cost_usd, error: None })
    }

    #[test]
    fn each_lane_shows_what_its_finished_calls_cost_and_unknown_cost_is_a_lower_bound() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.settings.budget_usd = Some(1.0);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        // Each call counts once.
        app.update(finished(&app, Some(0), "a", Some(0.3)));
        app.update(finished(&app, Some(0), "a", Some(0.3)));
        app.update(finished(&app, Some(1), "b", None));
        let task = app.task.as_ref().unwrap();
        assert_eq!(task.lanes[0].spend.label(), "$0.30");
        assert_eq!(task.lanes[1].spend.label(), "≥ $0.00");
        assert_eq!(task.spend().label(), "≥ $0.30");
        let shown = frame_text(&mut app);
        assert!(shown.contains("turn 1 · 0s · $0.30"), "{shown}");
        assert!(shown.contains("task ≥ $0.30 / $1.00"), "{shown}");
        assert_eq!(app.spent_usd, 0.0, "progress events do not settle spending");

        // An attempt stopped by the winner still settles its last call.
        app.update(Msg::Progress(Progress::AttemptFinished {
            run: trace.to_string(),
            attempt: 1,
            status: AttemptStatus::Cancelled,
        }));
        app.update(finished(&app, Some(1), "c", Some(0.05)));
        assert_eq!(app.task.as_ref().unwrap().lanes[1].spend.label(), "≥ $0.05");

        app.update(Msg::Ran { trace, result: Ok(passed(false, Some("fork"))) });
        assert!((app.spent_usd - 0.3).abs() < 1e-9, "the reply settles the run once: {}", app.spent_usd);
    }

    #[test]
    fn final_reports_settle_lane_cost_when_progress_was_lost() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        app.update(Msg::Progress(Progress::AttemptStarted { run: trace.to_string(), attempt: 1, fork: None }));
        let resp = passed(false, Some("fork"));
        let cost = resp.attempts.iter().find(|a| a.index == 1).unwrap().cost_usd;
        app.update(Msg::Ran { trace, result: Ok(resp) });
        assert_eq!(app.last_lanes.iter().find(|a| a.index == 1).unwrap().spend.usd, cost);
        assert!(cost > 0.0);
    }

    #[test]
    fn with_commit_on_an_applied_result_is_committed_once_it_is_applied() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        // Off by default: nothing about git.
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(true, None)) });
        assert!(!actions.iter().any(|a| matches!(a, Action::Commit { .. })), "{actions:?}");

        type_in(&mut app, "/commit on");
        assert_eq!(app.flash.as_deref(), Some("commit: on (next task)"));
        // Applied by the run itself: committed at once.
        let actions = type_in(&mut app, "implement more");
        let [Action::PrepareCommit { trace }, Action::Run { trace: run, .. }] = actions.as_slice() else {
            panic!("{actions:?}")
        };
        assert_eq!(trace, run);
        let trace = trace.clone();
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(true, None)) });
        let Some(Action::Commit { trace: committed, task, resp }) = actions.first() else { panic!("{actions:?}") };
        assert_eq!((committed, task.as_str(), resp.changes.len()), (&trace, "implement more", 1));
        let made = crate::git::Committed {
            sha: "0123456789abcdef".into(),
            subject: "Trailing commas parse.".into(),
            branch: Some("main".into()),
            ignored: vec![],
        };
        app.update(Msg::Committed(Ok(Some(made))));
        assert!(log(&app).ends_with("  committed 0123456789ab on main: Trailing commas parse."), "{}", log(&app));
        app.update(Msg::Committed(Err("a.txt had changes of yours".into())));
        assert!(log(&app).contains("not committed; the result is applied"), "{}", log(&app));

        // Applied after the user's look: committed only once the apply succeeds.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "and more");
        let [Action::PrepareCommit { trace }, Action::Run { .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let trace = trace.clone();
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/fork-3"))) });
        assert!(!actions.iter().any(|a| matches!(a, Action::Commit { .. })), "{actions:?}");
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::Apply { fork: "/d/fork-3".into() }]);
        let actions = app.update(Msg::Applied(Ok(())));
        assert!(matches!(actions.as_slice(), [Action::Commit { trace: t, .. }] if *t == trace), "{actions:?}");

        // A result the user keeps in its fork, or that fails to apply, is not committed.
        let actions = type_in(&mut app, "once more");
        let [Action::PrepareCommit { trace }, ..] = actions.as_slice() else { panic!("{actions:?}") };
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/fork-4"))) });
        app.update(key(KeyCode::Enter));
        assert!(app.update(Msg::Applied(Err("conflict".into()))).is_empty());
        type_in(&mut app, "/commit off");
        assert_eq!(type_in(&mut app, "last one").len(), 1);
    }

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
    fn narrow_lanes_keep_their_status_on_one_row() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        app.settings.attempts = Some(4);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "implement it");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        for attempt in 0..4 {
            app.update(Msg::Progress(Progress::AttemptStarted { run: trace.to_string(), attempt, fork: None }));
            app.update(Msg::Progress(Progress::ToolCall {
                run: trace.to_string(),
                attempt,
                turn: 12,
                tool: "read_file".into(),
                detail: "read".into(),
            }));
            app.update(finished(&app, Some(attempt), &format!("call {attempt}"), Some(0.25)));
        }
        let rows = frame_rows(&mut app, 79);
        let titles = rows.iter().find(|r| r.contains("turn 12")).expect("the lane titles");
        assert_eq!(titles.matches("turn 12").count(), 4, "{titles}");
        let wide = frame_rows(&mut app, 200);
        let titles = wide.iter().find(|r| r.contains("turn 12")).expect("the lane titles");
        assert_eq!(titles.matches("turn 12 · 0s · $0.25").count(), 4, "{titles}");
        assert_eq!(titles.matches("attempt").count(), 4, "{titles}");
    }

    /// Lets more than the looking window pass since the last key.
    fn look_away(app: &mut App) {
        for _ in 0..=LOOKING_TICKS {
            app.update(Msg::Tick);
        }
    }

    fn alerts(actions: &[Action]) -> Vec<&str> {
        actions.iter().filter_map(|a| if let Action::Alert { text } = a { Some(text.as_str()) } else { None }).collect()
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

    fn workspace_files() -> Msg {
        let files = ["Cargo.toml", "src/lexer.rs", "src/lib.rs", "src/parser/lex_table.rs", "tests/lexer.rs"];
        Msg::Files { files: files.map(str::to_owned).to_vec(), complete: true }
    }

    #[test]
    fn an_at_path_completes_from_the_workspace_and_goes_with_the_task() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        // The files are listed once, when an `@` is first typed.
        app.update(Msg::Paste("fix ".into()));
        let actions = app.update(key(KeyCode::Char('@')));
        assert_eq!(actions, [Action::ListFiles]);
        assert!(app.update(key(KeyCode::Char('s'))).is_empty(), "listed once");
        app.update(workspace_files());
        app.update(Msg::Paste("rc/le".into()));
        assert_eq!(app.suggestions(), ["src/lexer.rs", "src/parser/lex_table.rs"]);
        assert!(frame_text(&mut app).contains("› @src/lexer.rs"));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Tab));
        assert_eq!(app.input.text, "fix @src/lexer.rs ");
        assert!(app.suggestions().is_empty());
        app.update(Msg::Paste("and @src/lexer.rs, please".into()));
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.files, ["src/lexer.rs"]);
        assert_eq!(req.task, "fix @src/lexer.rs and @src/lexer.rs, please");
    }

    #[test]
    fn an_at_path_outside_the_workspace_or_not_in_it_is_refused() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        assert!(type_in(&mut app, "read @../outside").is_empty());
        assert_eq!(app.flash.as_deref(), Some("@../outside is outside the workspace"));
        assert_eq!(app.input.text, "read @../outside");
        app.input.take();
        assert!(type_in(&mut app, "read @/etc/passwd").is_empty());
        app.input.take();
        app.update(workspace_files());
        assert!(type_in(&mut app, "read @src/missing.rs").is_empty());
        assert_eq!(app.flash.as_deref(), Some("@src/missing.rs: no such file in the workspace"));
        app.input.take();
        // An address is not a reference, and Esc puts suggestions away.
        let actions = type_in(&mut app, "mail me@example.com about @Cargo.toml");
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.files, ["Cargo.toml"]);
    }

    #[test]
    fn suggestions_rank_the_start_of_the_path_then_the_name_then_any_part() {
        let files: Vec<String> =
            ["docs/lexer.md", "src/lexer.rs", "lexicon.txt", "src/a/l_e_x.rs"].map(str::to_owned).to_vec();
        assert_eq!(matching(&files, "lex"), ["lexicon.txt", "src/lexer.rs", "docs/lexer.md", "src/a/l_e_x.rs"]);
        assert_eq!(matching(&files, "SRC/LE"), ["src/lexer.rs", "src/a/l_e_x.rs"]);
        assert!(matching(&files, "zzz").is_empty());
        assert_eq!(matching(&files, "").len(), 4);
    }

    #[test]
    fn previews_can_be_disabled_and_cancellation_cannot_revive_them() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        type_in(&mut app, "/stream off");
        let actions = type_in(&mut app, "implement it");
        assert!(matches!(&actions[0], Action::Run { req, .. } if !req.stream));
        let context = model_context(&app, Some(0), 1);
        app.update(Msg::Progress(Progress::ModelText { context, text: "hidden preview".into() }));
        assert!(app.task.as_ref().unwrap().lanes.is_empty());
        assert!(!frame_text(&mut app).contains("hidden preview"));
        app.update(key(KeyCode::Esc));
        up(&mut app);
        type_in(&mut app, "/stream on");
        type_in(&mut app, "implement it again");
        let context = model_context(&app, Some(0), 1);
        app.update(Msg::Progress(Progress::ModelText { context: context.clone(), text: "provisional".into() }));
        assert!(frame_text(&mut app).contains("provisional"));
        assert!(matches!(app.update(key(KeyCode::Esc))[0], Action::Interrupt { .. }));
        // Only the offer to start again with the same check is left.
        assert!(app.task.is_none() && matches!(app.prompt, Some(Prompt::Retry { stopped: true, .. })));
        app.update(Msg::Progress(Progress::ModelText { context, text: "late result".into() }));
        assert!(!frame_text(&mut app).contains("late result"));
    }

    #[test]
    fn designer_previews_fold_on_errors_without_creating_a_check() {
        let mut app = App::new("/work/project", None);
        up(&mut app);
        let actions = type_in(&mut app, "implement it");
        assert!(matches!(&actions[0], Action::Design { req, .. } if req.stream));
        let context = model_context(&app, None, 0);
        app.update(Msg::Progress(Progress::ModelStarted { context }));
        let context = model_context(&app, None, 1);
        app.update(Msg::Progress(Progress::ModelText { context, text: "designing stream".into() }));
        assert!(frame_text(&mut app).contains("designing stream"));
        let context = model_context(&app, None, 2);
        app.update(Msg::Progress(Progress::ModelFinished {
            context,
            usage: None,
            cost_usd: None,
            error: Some("cancelled".into()),
        }));
        assert!(log(&app).contains("model preview stopped (cancelled)"));
        let trace = app.task.as_ref().unwrap().trace.clone();
        assert!(app.update(Msg::Designed { trace, result: Err("cancelled".into()), existing: vec![] }).is_empty());
        assert!(app.task.is_none() && app.prompt.is_none());
    }

    #[test]
    fn project_commands_set_one_request_and_confirm_their_check() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".molt/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fix-issue.md"),
            "---\ncheck: cargo test\nattempts: 3\nbudget: 2.5\n---\nFix issue $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(dir.join("help.md"), "wrong").unwrap();
        let mut app = App::new(root.path().to_str().unwrap(), None);
        app.load_commands(root.path());
        up(&mut app);
        app.settings.budget_usd = Some(1.0);
        assert_eq!(app.command_candidates("fix-"), ["fix-issue"]);
        assert!(type_in(&mut app, "/fix-issue 12").is_empty());
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

    fn design() -> DesignResponse {
        DesignResponse {
            command: Some("cargo test --test commas".into()),
            files: vec![CheckFile { path: "tests/commas.rs".into(), content: "#[test]\nfn t() {}\n".into() }],
            suite: None,
            rubric: vec![],
            rationale: "the parser has tests".into(),
            baseline: Some(Baseline { passed: false, exit_code: Some(101), tail: String::new() }),
            usage: Usage::default(),
            cost_usd: 0.25,
            uncounted_calls: 0,
            remaining_budget_usd: None,
            error: None,
            cancelled: false,
        }
    }

    fn passed(apply: bool, fork: Option<&str>) -> RunResponse {
        RunResponse {
            outcome: Outcome::Passed,
            check: Some(CheckSpec { command: "cargo test".into(), files: vec![], designed: true, suite: None }),
            winner: Some(1),
            summary: "Trailing commas parse.".into(),
            changes: vec![Change { path: "src/parser.rs".into(), kind: ChangeKind::Modified }],
            patch: "--- a/src/parser.rs\n+++ b/src/parser.rs\n@@\n-a\n+b\n+c\n".into(),
            patch_truncated: false,
            applied: apply,
            fork: fork.map(str::to_owned),
            review: None,
            attempts: vec![
                AttemptReport {
                    index: 0,
                    status: AttemptStatus::Cancelled,
                    turns: 4,
                    check_runs: 1,
                    usage: Usage::default(),
                    cost_usd: 0.1,
                    note: String::new(),
                    requirements: 0,
                },
                AttemptReport {
                    index: 1,
                    status: AttemptStatus::Passed,
                    turns: 5,
                    check_runs: 1,
                    usage: Usage::default(),
                    cost_usd: 0.2,
                    note: String::new(),
                    requirements: 0,
                },
            ],
            usage: Usage::default(),
            cost_usd: 0.3,
            uncounted_calls: 0,
            checkpoint: None,
            requirements: vec![],
            selection: None,
            rubric: vec![],
            judgement: vec![],
        }
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
            rubric: vec![],
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

    fn control(actions: &[Action]) -> (TraceId, Control) {
        let [Action::Control { trace, action }] = actions else { panic!("{actions:?}") };
        (trace.clone(), action.clone())
    }

    fn answer(state: RunState, version: u32, waiting: &[&str]) -> Result<ControlResponse, String> {
        Ok(ControlResponse { state, version, waiting: waiting.iter().map(|w| w.to_string()).collect() })
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
        app.update(Msg::Ran { trace: run, result: Ok(passed(true, None)) });
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
        app.update(Msg::Ran { trace, result: Ok(passed(false, Some("/d/work/fork-1"))) });
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
        app.update(Msg::Ran { trace, result: Ok(passed(true, None)) });
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
            app.update(Msg::Ran { trace, result: Ok(passed(true, None)) });
        }
        assert_eq!(app.earlier.len(), EARLIER_TASKS);
        assert_eq!(app.earlier[0].task, "task 2");
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
        let [Action::Run { req, .. }] = actions.as_slice() else { panic!() };
        assert!(!req.verify && req.check.is_none() && req.check_files.is_empty());

        // Esc while it runs stops everything.
        let actions = app.update(key(KeyCode::Esc));
        assert_eq!(actions, [Action::Interrupt { keep: vec![], attempts: vec![], check_files: vec![] }]);
        assert!(app.task.is_none());
        assert_eq!(app.session, SessionState::Starting);
        assert_eq!(app.update(Msg::Stopped { kept: vec![] }), [Action::StartSession]);

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
            log(&app).ends_with(&format!(
                "◇ memory  map 2800 tokens · 1 note\n  /memory-used {trace} · recorded recall context\n  0.78 Tests need --features x\n  project model: 3 files"
            )),
            "{}",
            log(&app)
        );
    }

    #[test]
    fn memory_learns_with_the_model_set_for_it() {
        // What the next run's learning asks for, after `commands`.
        let learns_with = |commands: &[&str]| {
            let mut app = App::new("/w", None);
            up(&mut app);
            for c in commands.iter().chain(&["/check make test"]) {
                assert!(type_in(&mut app, c).is_empty(), "{c}");
            }
            let actions = type_in(&mut app, "task");
            let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
            let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/f"))) });
            // Confirm mode applies the passing result first.
            let [Action::Apply { .. }, Action::Learn { model, .. }] = actions.as_slice() else { panic!("{actions:?}") };
            model.clone()
        };
        assert_eq!(learns_with(&[]), None, "memory's own default");
        assert_eq!(learns_with(&["/model opus"]), None, "/model is for the attempts only");
        assert_eq!(learns_with(&["/memory-model glm-4.6"]).as_deref(), Some("glm-4.6"));
        assert_eq!(learns_with(&["/model glm-4.6", "/memory-model same"]).as_deref(), Some("glm-4.6"));
        assert_eq!(learns_with(&["/memory-model same"]), None, "the attempts use the default too");
        assert_eq!(learns_with(&["/memory-model haiku", "/memory-model default"]), None);

        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/memory-model glm-4.6");
        assert_eq!(app.flash.as_deref(), Some("memory model: glm-4.6"));
        type_in(&mut app, "/memory-model same");
        assert_eq!(app.flash.as_deref(), Some("memory model: the attempts' model"));
        assert!(plain(&help()).iter().any(|l| l.contains("/memory-model NAME")));
    }

    #[test]
    fn learning_that_was_skipped_says_why() {
        let mut app = App::new("/w", None);
        let skipped = ConsolidateResponse {
            added: vec![],
            reinforced: vec![],
            contradicted: vec![],
            skipped: Some("the episode held nothing worth keeping".into()),
            proposed: vec![],
            usage: Usage::default(),
            cost_usd: 0.01,
        };
        app.learned(Ok(skipped));
        assert!(log(&app).ends_with("◇ memory  nothing new: the episode held nothing worth keeping"), "{}", log(&app));
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
    fn notes_for_every_project_are_remembered_and_promoted_by_hand() {
        let mut app = App::new("/w", None);
        up(&mut app);
        assert_eq!(
            type_in(&mut app, "/remember Answer in Chinese."),
            [Action::Remember { text: "Answer in Chinese.".into() }]
        );
        assert!(type_in(&mut app, "/remember").is_empty(), "nothing to remember");
        app.update(Msg::Remembered(Ok("note_u".into())));
        assert_eq!(app.flash.as_deref(), Some("remembered note_u for every project"));

        type_in(&mut app, "/memory");
        let note = |id: &str, layer: Layer| Recalled {
            note: serde_json::from_value(serde_json::json!({
                "id": id, "kind": "fact", "text": "t", "confidence": 0.5, "created_ms": 0, "updated_ms": 0,
                "provenance": { "trace": "tr", "service": "memory", "version": "v" }, "rev": 1
            }))
            .unwrap(),
            score: 1.0,
            reason: Default::default(),
            details: Default::default(),
            layer,
        };
        app.update(Msg::Notes(Ok(vec![note("note_u", Layer::User), note("note_p", Layer::Project)])));
        app.update(key(KeyCode::Char('p')));
        assert_eq!(app.flash.as_deref(), Some("that note holds everywhere already"));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('p')));
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "a reason is required");
        app.update(Msg::Paste("all my projects use make".into()));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Action::Promote { id: "note_p".into(), reason: "all my projects use make".into() }]
        );
        assert_eq!(
            app.update(Msg::Promoted { id: "note_p".into(), result: Ok("note_c".into()) }),
            [Action::LoadNotes { query: String::new() }]
        );
        assert_eq!(app.flash.as_deref(), Some("note_p now holds in every project as note_c"));
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
            layer: Default::default(),
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

    fn inspected_note() -> Recalled {
        serde_json::from_value(serde_json::json!({
            "note": { "id":"note_config", "rev":7, "kind":"fact", "text":"Build with cargo", "confidence":0.8,
                "created_ms":0, "updated_ms":0, "provenance":{"trace":"run_source", "service":"memory", "version":"v1", "events":["source_event"]}},
            "score":0.9, "reason":{"keywords":"cargo", "relevance":0.9, "confidence":0.8, "recency":0.7},
            "details":{"dependencies":[{"path":"Cargo.toml", "sha256":"old_hash"}], "needs_review":["Cargo.toml"], "temporary":true, "reason":"verified earlier"}
        })).unwrap()
    }

    #[test]
    fn memory_inspection_shows_the_reason_and_reviews_the_revision_the_person_saw() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/memory cargo");
        app.update(Msg::Notes(Ok(vec![inspected_note()])));
        let frame = frame_text(&mut app);
        for expected in [
            "[review]",
            "[temporary]",
            "revision 7",
            "keywords: cargo",
            "needs review: Cargo.toml",
            "old_hash",
            "source_event",
        ] {
            assert!(frame.contains(expected), "missing {expected}: {frame}");
        }
        app.update(key(KeyCode::Char('v')));
        assert!(plain(&app.viewer.as_ref().unwrap().lines).join("\n").contains("relevance: 0.900"));
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Char('a')));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert!(app.flash.as_ref().unwrap().contains("justified"));
        app.update(Msg::Paste("checked current config".into()));
        app.update(key(KeyCode::Enter));
        // A refresh while editing must not silently raise the saved lease.
        let mut newer = inspected_note();
        newer.note.rev = 8;
        app.update(Msg::Notes(Ok(vec![newer.clone()])));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("Cargo.toml; src/lib.rs".into()));
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Char('t')));
        let actions = app.update(key(KeyCode::Enter));
        assert_eq!(
            actions,
            [Action::ChangeNote {
                review: ReviewRequest {
                    workspace: "/w".into(),
                    id: "note_config".into(),
                    expected_rev: 7,
                    reason: "checked current config".into(),
                    depends_on: vec!["Cargo.toml".into(), "src/lib.rs".into()],
                    temporary: false,
                },
                text: None
            }]
        );
        assert!(app
            .update(Msg::ChangedNote(Err("note changed: expected revision 7, current 8; inspect it again".into())))
            .is_empty());
        let Screen::Memory(view) = &app.screen else { panic!() };
        assert_eq!(view.notes, [newer]);
        assert!(!view.loading && view.error.as_ref().unwrap().contains("current 8"));
        assert!(frame_text(&mut app).contains("current 8"));
    }

    #[test]
    fn correcting_a_note_requires_a_reason_and_confirmation_and_refreshes_after_success() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/memory");
        let note = inspected_note();
        app.update(Msg::Notes(Ok(vec![note.clone()])));
        app.update(key(KeyCode::Char('c')));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("Build with make".into()));
        app.update(key(KeyCode::Enter));
        app.update(Msg::Paste("verified the migration".into()));
        app.update(key(KeyCode::Enter));
        app.update(ctrl_key('u'));
        app.update(key(KeyCode::Enter));
        assert!(frame_text(&mut app).contains("Enter save"));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Action::ChangeNote {
                review: ReviewRequest {
                    workspace: "/w".into(),
                    id: note.note.id.clone(),
                    expected_rev: 7,
                    reason: "verified the migration".into(),
                    depends_on: vec![],
                    temporary: true,
                },
                text: Some("Build with make".into())
            }]
        );
        assert_eq!(
            app.update(Msg::ChangedNote(Ok(ReviewResponse { note: note.note, details: note.details }))),
            [Action::LoadNotes { query: String::new() }]
        );
        // Cancelling a later form never sends a mutation.
        app.update(Msg::Notes(Ok(vec![inspected_note()])));
        app.update(key(KeyCode::Char('c')));
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        let Screen::Memory(view) = &app.screen else { panic!() };
        assert!(view.edit.is_none());
    }

    #[test]
    fn used_memory_shows_the_immutable_run_context_and_ignores_late_viewer_replies() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        let run = trace.to_string();
        assert_eq!(type_in(&mut app, "/memory-used"), [Action::UsedNotes { run: run.clone() }]);
        let record = RecallSnapshot {
            run: run.clone(),
            call: "captured_call".into(),
            workspace: "/w".into(),
            query: "cargo".into(),
            purpose: "context".into(),
            created_ms: 1,
            notes: vec![inspected_note()],
        };
        app.update(Msg::UsedNotes { run: run.clone(), result: Ok(vec![record.clone()]) });
        let frame = frame_text(&mut app);
        assert!(frame.contains("captured_call") && frame.contains("revision 7") && frame.contains("Build with cargo"));
        app.update(key(KeyCode::Esc));
        app.update(Msg::UsedNotes { run: run.clone(), result: Ok(vec![record]) });
        assert!(app.viewer.is_none());
        type_in(&mut app, "/memory");
        app.update(Msg::Notes(Ok(vec![])));
        assert_eq!(app.update(key(KeyCode::Char('u'))), [Action::UsedNotes { run }]);
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
    fn quitting_keeps_a_result_still_waiting_on_the_user() {
        let mut app = App::new("/w", None);
        up(&mut app);
        // These are about the prompts themselves.
        app.rules.always_ask = true;
        let actions = type_in(&mut app, "/check true");
        assert!(actions.is_empty());
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-9"))) });
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        assert!(app.update(ctrl_key('d')).is_empty());
        assert_eq!(app.flash.as_deref(), Some("ctrl+d again to quit; 1 result stays in its fork"));
        assert_eq!(app.update(ctrl_key('d')), [Action::Quit { keep: vec!["/d/work/fork-9".into()] }]);
    }

    #[test]
    fn the_log_shows_long_summaries_every_change_and_every_note_in_full() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "/check true");
        assert!(actions.is_empty());
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let trace = trace.clone();
        let notes = (0..8)
            .map(|i| RecalledNote { id: format!("note_{i}"), text: format!("fact {i}"), confidence: 0.5 })
            .collect();
        app.update(Msg::Progress(Progress::Recalled { run: trace.to_string(), notes, map_tokens: None }));
        let mut resp = passed(false, Some("/d/work/fork-3"));
        resp.summary = (0..200).map(|i| format!("summary line {i}")).collect::<Vec<_>>().join("\n");
        resp.changes = (0..30).map(|i| Change { path: format!("src/f{i}.rs"), kind: ChangeKind::Modified }).collect();
        app.update(Msg::Ran { trace, result: Ok(resp) });
        let log = log(&app);
        for i in 0..8 {
            assert!(log.contains(&format!("0.50 fact {i}")), "{log}");
        }
        for i in 0..200 {
            assert!(log.contains(&format!("  summary line {i}\n")), "{log}");
        }
        for i in 0..30 {
            assert!(log.contains(&format!("  modified src/f{i}.rs")), "{log}");
        }
        assert!(!log.contains("more lines") && !log.contains(" more\n"), "{log}");
    }

    #[test]
    fn a_failed_summary_is_shown_in_full_and_only_a_runaway_one_is_cut() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let mut resp = passed(false, None);
        resp.outcome = Outcome::Failed;
        resp.winner = None;
        resp.changes = vec![];
        resp.summary = (0..200).map(|i| format!("why {i}")).collect::<Vec<_>>().join("\n");
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(resp) });
        assert!(log(&app).contains("  why 0\n") && log(&app).contains("  why 199"), "{}", log(&app));
        assert!(!log(&app).contains("cut at"), "{}", log(&app));

        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!() };
        let mut resp = passed(false, Some("/d/work/fork-4"));
        resp.summary = "é".repeat(SUMMARY_BYTES);
        app.update(Msg::Ran { trace: trace.clone(), result: Ok(resp) });
        let note = format!("  (cut at 1 MB; the whole text is in the audit log, trace {trace})");
        assert!(log(&app).contains(&note), "the cut is said, with where to find the rest");
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
            app.update(Msg::Ran { trace: trace.clone(), result: Ok(passed(false, Some("/d/work/fork-5"))) });
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

    #[test]
    fn home_and_end_jump_through_the_log_unless_there_is_text_to_edit() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(key(KeyCode::Home));
        assert_eq!(app.scroll, usize::MAX);
        app.update(key(KeyCode::End));
        assert_eq!(app.scroll, 0);
        app.update(Msg::Paste("abc".into()));
        app.update(key(KeyCode::Home));
        assert_eq!((app.scroll, app.input.cursor), (0, 0), "home moves the cursor");
        app.update(Msg::Key(KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        assert_eq!(app.scroll, usize::MAX);
    }

    fn failed(attempts: Vec<AttemptReport>) -> RunResponse {
        RunResponse {
            outcome: Outcome::Failed,
            winner: None,
            summary: "No attempt passed the done-check.".into(),
            changes: vec![],
            patch: String::new(),
            attempts,
            ..passed(false, None)
        }
    }

    fn attempt(index: u32, status: AttemptStatus, note: &str) -> AttemptReport {
        AttemptReport {
            index,
            status,
            turns: 3,
            check_runs: 2,
            usage: Usage::default(),
            cost_usd: 0.1,
            note: note.into(),
            requirements: 0,
        }
    }

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
        let actions = app.update(key(KeyCode::Esc));
        assert_eq!(
            actions,
            [Action::Interrupt {
                keep: vec![],
                attempts: vec!["/d/work/fork-a".into(), "/d/work/fork-b".into()],
                check_files: vec!["tests/commas.rs".into()],
            }]
        );
        // The runtime found changes in one of them.
        assert_eq!(app.update(Msg::Stopped { kept: vec!["/d/work/fork-b".into()] }), [Action::StartSession]);
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

    /// The check's prompt, and why it is up, for a design and the files it would replace.
    fn check_stop(d: DesignResponse, existing: &[&str]) -> Vec<String> {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let existing = existing.iter().map(|e| e.to_string()).collect();
        let actions = app.update(Msg::Designed { trace: trace.clone(), result: Ok(d), existing });
        assert!(actions.is_empty() && matches!(app.prompt, Some(Prompt::Check(_))), "{actions:?}");
        app.task.as_ref().unwrap().stops.clone()
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

    /// Why the result's prompt is up, for a result.
    fn apply_stop(resp: RunResponse) -> Vec<String> {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/check true");
        let actions = type_in(&mut app, "task");
        let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        let actions = app.update(Msg::Ran { trace: trace.clone(), result: Ok(resp) });
        assert!(!actions.iter().any(|a| matches!(a, Action::Apply { .. })), "{actions:?}");
        assert!(matches!(app.prompt, Some(Prompt::Apply { .. })));
        app.task.as_ref().unwrap().stops.clone()
    }

    #[test]
    fn confirm_mode_stops_for_a_result_that_needs_a_look() {
        let ok = || passed(false, Some("/d/work/fork-1"));
        assert_eq!(apply_stop(RunResponse { outcome: Outcome::Unverified, ..ok() }), ["unverified"]);
        let patch = format!("--- a/x\n+++ b/x\n@@\n{}", "+line\n".repeat(301));
        assert_eq!(apply_stop(RunResponse { patch, ..ok() }), ["301 lines changed"]);
        let changes = (0..11).map(|i| Change { path: format!("src/f{i}.rs"), kind: ChangeKind::Modified }).collect();
        assert_eq!(apply_stop(RunResponse { changes, ..ok() }), ["11 files"]);
        let changes = vec![
            Change { path: "src/old.rs".into(), kind: ChangeKind::Deleted },
            Change { path: "src/new.rs".into(), kind: ChangeKind::Added },
        ];
        assert_eq!(apply_stop(RunResponse { changes, ..ok() }), ["deletes 1 file"]);
        let changes = vec![Change { path: "crates/a/Cargo.toml".into(), kind: ChangeKind::Modified }];
        assert_eq!(apply_stop(RunResponse { changes, ..ok() }), ["touches crates/a/Cargo.toml"]);
        assert_eq!(apply_stop(RunResponse { patch_truncated: true, ..ok() }), ["diff cut short"]);
        let review = |verdict| Some(molt_api::planner::Review { verdict, findings: vec![] });
        assert_eq!(apply_stop(RunResponse { review: review(Verdict::Reject), ..ok() }), ["review rejected"]);
        assert_eq!(apply_stop(RunResponse { review: review(Verdict::Concern), ..ok() }), ["review questioned"]);
    }

    #[test]
    fn protected_paths_match_files_directories_and_names() {
        assert!(covers(".github/", ".github/workflows/ci.yml"));
        assert!(covers("migrations/", "db/migrations/001.sql"));
        assert!(!covers("migrations/", "src/migrations.rs"));
        assert!(covers("db/schema/", "db/schema/a.sql") && !covers("db/schema/", "x/db/schema/a.sql"));
        assert!(covers("Cargo.toml", "Cargo.toml") && covers("Cargo.toml", "crates/a/Cargo.toml"));
        assert!(!covers("Cargo.toml", "Cargo.toml.bak"));
        assert!(covers("src/lib.rs", "src/lib.rs") && !covers("src/lib.rs", "a/src/lib.rs"));
    }

    fn with_mods(code: KeyCode, modifiers: KeyModifiers) -> Msg {
        Msg::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, state: KeyEventState::NONE })
    }

    #[test]
    fn a_task_can_hold_several_lines() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(Msg::Paste("fix the parser".into()));
        app.update(with_mods(KeyCode::Enter, KeyModifiers::SHIFT));
        app.update(Msg::Paste("keep the API".into()));
        app.update(ctrl_key('j'));
        app.update(Msg::Paste("and the docs\\".into()));
        app.update(key(KeyCode::Enter));
        app.update(Msg::Paste("too".into()));
        assert_eq!(app.input.text, "fix the parser\nkeep the API\nand the docs\ntoo");

        // Up and Down move between the lines before they reach the history.
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Up));
        assert_eq!(app.input.cursor, "fix the parser\nkeep".chars().count() - 1);
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        assert_eq!(app.input.cursor, app.input.text.chars().count());

        let actions = app.update(key(KeyCode::Enter));
        let [Action::Design { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, "fix the parser\nkeep the API\nand the docs\ntoo");
        assert!(log(&app).ends_with("› fix the parser\n  keep the API\n  and the docs\n  too"), "{}", log(&app));
    }

    #[test]
    fn a_paste_keeps_its_lines_and_tabs_and_loses_escape_sequences() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(Msg::Paste("error[E0308]:\r\n\tmismatched \u{1b}[31mtypes\u{1b}[0m\u{1b}]0;title\u{7}\u{8}".into()));
        assert_eq!(app.input.text, "error[E0308]:\n\tmismatched types");
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Design { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, "error[E0308]:\n\tmismatched types");
    }

    #[test]
    fn a_long_paste_shows_folded_and_is_sent_in_full() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let trace = (1..=143).map(|i| format!("frame {i}")).collect::<Vec<_>>().join("\n");
        app.update(Msg::Paste("why does this panic?\n".into()));
        app.update(Msg::Paste(trace.clone()));
        assert_eq!(app.input.text, "why does this panic?\n[pasted #1: 143 lines]");
        // The editor gets it in full.
        let actions = app.update(ctrl_key('g'));
        assert_eq!(actions, [Action::Edit { text: format!("why does this panic?\n{trace}") }]);
        let actions = app.update(key(KeyCode::Enter));
        let [Action::Design { req, .. }] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(req.task, format!("why does this panic?\n{trace}"));
    }

    #[test]
    fn the_editor_fills_the_input() {
        let mut app = App::new("/w", None);
        up(&mut app);
        app.update(Msg::Paste("draft".into()));
        assert_eq!(app.update(ctrl_key('g')), [Action::Edit { text: "draft".into() }]);
        app.update(Msg::Edited(Ok("step one\nstep two\n".into())));
        assert_eq!((app.input.text.as_str(), app.input.cursor), ("step one\nstep two", 17));
        app.update(Msg::Edited(Err("vi exit status: 1".into())));
        assert_eq!(app.flash.as_deref(), Some("the editor failed: vi exit status: 1"));
        assert_eq!(app.input.text, "step one\nstep two", "a failed edit keeps the input");
    }
}
