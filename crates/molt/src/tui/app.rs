//! What the TUI shows and how it answers keys and results. Nothing here
//! touches the terminal or the bus: [`App::update`] takes a [`Msg`] and
//! returns the [`Action`]s the runtime carries out, so the whole flow can be
//! tested with plain values.

mod apply;
mod changes;
mod check;
mod command;
mod completion;
mod input;
mod kept;
mod memory;
mod plan;
mod progress;
mod queue;
mod retry;
mod runs;
mod settings;
mod shell;
mod status;
mod task;
#[cfg(test)]
mod testing;
mod viewer;

pub(crate) use check::DISPUTED;
pub use input::{HistorySearch, Input};
pub use memory::{memory_lines, ChangeStep, MemoryEdit, MemoryView};
pub use settings::Settings;
pub use shell::Sandbox;
pub use viewer::{TranscriptView, Viewer};

use apply::apply_stops;
use check::{check_head, disputed_check, rubric_lines, tui_next_steps};
use memory::snapshot_lines;
pub(crate) use plan::{plan_card, PlanSteps};
use progress::{bounded, lane_lines};
use settings::task_budget;
use viewer::{diff_viewer, review_viewer, viewer_key};

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use molt_api::fs::ChangeKind;
use molt_api::memory::{ConsolidateResponse, Layer, RecallSnapshot, Recalled, ReviewRequest, ReviewResponse};
use molt_api::model::{attempt_name, Effort, Role};
use molt_api::plan::{PlanRequest, PlanResponse};
use molt_api::planner::{
    AttemptReport, AttemptStatus, Constraint, Continuation, Control, ControlResponse, DesignResponse, Earlier, Outcome,
    RunRequest, RunResponse, RunState, Verdict,
};
use molt_api::progress::Progress;
use molt_api::workflow;
use molt_proto::audit::Logged;
use molt_proto::TraceId;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;

use super::preview::Preview;
use super::text::{self, block, clean_line, span, Tone};
use super::transcript::{self, Reader};
use crate::agent;
use crate::agent::outcome_name;
use crate::config::ConfirmSection;

/// Bytes of one summary written to the log. Only a runaway message reaches
/// this; the log keeps everything a normal task says.
const SUMMARY_BYTES: usize = 1 << 20;
/// Lines kept per attempt while it runs.
const LANE_LINES: usize = 200;
/// Ticks within which a second Ctrl+C or Ctrl+D quits, or a second Esc
/// stops a task: about a second.
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
pub const TICK_MS: u64 = 150;
const STARTUP_RETRY_TICKS: [u64; 3] = [14, 34, 100];

/// Suggestions shown for an `@path` being typed.
const SUGGESTIONS: usize = 8;
/// Workspace files listed for `@` completion; past this, a path is not
/// checked against the list.
pub const LISTED_FILES: usize = 50_000;
/// Ticks between reads of a running task's transcript: about a second.
const TRANSCRIPT_TICKS: u64 = 7;

/// When the interface calls the user from outside the terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Notify {
    Off,
    /// When it stops for the user, or the services go down.
    #[default]
    #[serde(rename = "on", alias = "waiting")]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// Work for the runtime.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Action {
    StartSession,
    RestartSession {
        keep: Vec<String>,
    },
    ShowLog,
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
    /// Plan a task as steps (`req.propose`), or carry its steps out; the
    /// answer comes back as [`Msg::Planned`].
    Plan {
        req: Box<PlanRequest>,
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
    Diff {
        fork: String,
    },
    Apply {
        fork: String,
    },
    /// Apply only `paths` of the result kept in `fork`.
    ApplyOnly {
        fork: String,
        paths: Vec<String>,
    },
    /// Check the result kept in `fork` again, `only` some of its files or
    /// all of them; the answer comes back as [`Msg::Rechecked`].
    Recheck {
        fork: String,
        only: Option<Vec<String>>,
        check: Option<molt_api::planner::CheckSpec>,
    },
    /// Open the result kept in `fork` in the user's editor; [`Msg::ForkEdited`] says when they are done.
    EditFork {
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
    Undo {
        run: Option<String>,
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
        /// The checkpointed run that was stopped: it is not to go on, so
        /// its checkpoint is removed once the services have stopped.
        #[serde(default)]
        drop_run: Option<String>,
    },
    /// List the stopped runs of the workspace, from their checkpoints; they
    /// come back as [`Msg::Runs`]. Nothing is started.
    ListRuns,
    /// Drop the stopped run `run`, its forks and its checkpoint; the answer
    /// comes back as [`Msg::RunDropped`].
    DropRun {
        run: String,
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
    /// Read run `trace`'s messages in the audit log from `cursor` on; they
    /// come back as [`Msg::Audit`].
    ReadAudit {
        trace: TraceId,
        cursor: Option<u64>,
    },
    /// Open `text` in the user's editor, to read or keep; nothing comes back.
    Export {
        text: String,
    },
    /// Run the user's `command` through the shell service (`!cmd`); its
    /// output comes as [`Progress::Output`] events of `trace`, and its end
    /// as [`Msg::CommandRan`].
    Command {
        command: String,
        trace: TraceId,
    },
    /// Ask the shell to stop the command `trace`.
    StopCommand {
        trace: TraceId,
    },
    /// List what the workspace changed against `HEAD`; it comes back as [`Msg::Changes`].
    Changes,
    /// Read one changed file's diff against `HEAD`; it comes back as [`Msg::FileDiff`].
    FileDiff {
        change: crate::git::Changed,
    },
}

/// A stretch of a run's messages in the audit log.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct AuditPage {
    pub entries: Vec<Logged>,
    /// Where to read on.
    pub cursor: Option<u64>,
    /// The read stopped before the end of the log.
    pub more: bool,
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
        /// The model a task that names none runs on, as the gateway resolves it.
        default_model: Option<String>,
    },
    SessionFailed(String),
    BackendFailed(String),
    /// The UI drops messages from a backend replaced by a manual restart.
    Backend {
        generation: u64,
        message: Box<Msg>,
    },
    ServiceLog(Result<Vec<String>, String>),
    /// The services of an interrupted session have stopped; `kept` are the
    /// forks of interrupted attempts kept for their changes, and `notes` say
    /// what could not be told about the others.
    Stopped {
        kept: Vec<String>,
        notes: Vec<String>,
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
        // Boxed: a run's reply is much larger than any other message.
        result: Box<Result<RunResponse, String>>,
    },
    Planned {
        trace: TraceId,
        result: Box<Result<PlanResponse, String>>,
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
    Diff {
        fork: String,
        result: Result<molt_api::fs::DiffResponse, String>,
    },
    Applied(Result<(), String>),
    Rechecked {
        fork: String,
        only: Option<Vec<String>>,
        result: Result<crate::session::Rechecked, String>,
    },
    ForkEdited {
        fork: String,
        result: Result<(), String>,
    },
    /// `None`: the workspace's commit has the files as they are already.
    Committed(Result<Option<crate::git::Committed>, String>),
    Undone(Result<crate::session::Undone, String>),
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
    Audit {
        trace: TraceId,
        result: Result<AuditPage, String>,
    },
    /// The stopped runs of the workspace, newest first.
    Runs(Vec<runs::Stopped>),
    RunDropped {
        run: String,
        result: Result<(), String>,
    },
    /// How the user's command `trace` ended.
    CommandRan {
        trace: TraceId,
        result: Result<molt_api::shell::RunResponse, String>,
    },
    /// What the workspace changed against `HEAD`, or why that cannot be said.
    Changes(Result<Vec<crate::git::Changed>, String>),
    FileDiff {
        path: String,
        result: Result<crate::git::FileDiff, String>,
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
    /// The gateway is about to try one of the attempt's model calls again.
    pub retry: Option<Retrying>,
}

/// A model call the gateway is about to try again, after it failed.
#[derive(Clone, Debug)]
pub struct Retrying {
    pub call: String,
    /// What failed, in a few words: `model API 502`.
    pub failure: String,
    /// When the retry starts.
    pub at: Instant,
    pub retry: u32,
    pub retries: u32,
}

impl Retrying {
    /// `model API 502, retrying in 60s (2/4)`, counting down.
    pub fn line(&self) -> String {
        let left = self.at.saturating_duration_since(Instant::now());
        molt_api::progress::retry_line(&self.failure, left, self.retry, self.retries)
    }
}

pub struct Task {
    pub trace: TraceId,
    pub req: RunRequest,
    pub phase: Phase,
    pub mode: Mode,
    settings: Settings,
    pub lanes: Vec<Lane>,
    pub designer_preview: Preview,
    /// Previews of the other calls of no attempt, one per role (the
    /// reviewer's, the judge's), so one role's turns do not hide another's.
    pub role_previews: BTreeMap<Role, Preview>,
    /// The gateway is about to try a model call of the check's design (or
    /// another call of no attempt) again.
    pub designer_retry: Option<Retrying>,
    /// The whole task's budget, as the user set it; `req.budget_usd` is
    /// what is left for the attempts once the check is designed.
    pub budget_usd: Option<f64>,
    /// The check design: its finished calls while it runs, then its settled
    /// cost.
    pub design_spend: Spend,
    /// Model calls already counted, so a repeated event is not counted twice.
    counted: HashSet<String>,
    /// What finished calls cost that no reply has settled yet: an interrupt
    /// counts it, since the reply that would have will not come.
    unsettled: Spend,
    /// Of those, the calls whose cost is unknown.
    unsettled_unknown: u32,
    /// Calls that have started and not finished, whose cost is unknown if
    /// the task is stopped.
    open_calls: HashSet<String>,
    /// The check is in the log already.
    check_shown: bool,
    recalled_shown: bool,
    notes_seen: HashSet<String>,
    /// Why confirm mode stopped for the user, in short labels.
    pub stops: Vec<String>,
    /// The project command whose template gave the check, while the check is still its.
    pub check_from: Option<String>,
    /// The latest version of the run's requirements; 0 is the task alone.
    pub version: u32,
    pub state: RunState,
    /// What a pausing run waits for before it is paused.
    pub waiting: Vec<String>,
    /// The attempts' questions not answered yet, oldest first: the number
    /// of each and its options. What the user types next answers the first.
    pub asked: Vec<(u32, Vec<String>)>,
    /// Of a resumed run, what it spent before this resume that the session
    /// total does not count yet: its cost, and its calls of unknown cost.
    carried: (f64, u32),
    /// Of a plan, the fork its steps' results go into, once a step has started.
    pub plan_fork: Option<String>,
}

impl Task {
    fn new(trace: TraceId, req: RunRequest, phase: Phase, mode: Mode, settings: Settings) -> Self {
        Self {
            trace,
            budget_usd: req.budget_usd,
            req,
            phase,
            mode,
            settings,
            lanes: Vec::new(),
            design_spend: Spend::default(),
            counted: HashSet::new(),
            unsettled: Spend::default(),
            unsettled_unknown: 0,
            open_calls: HashSet::new(),
            check_shown: false,
            recalled_shown: false,
            notes_seen: HashSet::new(),
            stops: Vec::new(),
            check_from: None,
            designer_preview: Preview::default(),
            role_previews: BTreeMap::new(),
            designer_retry: None,
            version: 0,
            state: RunState::Running,
            waiting: Vec::new(),
            asked: Vec::new(),
            carried: (0.0, 0),
            plan_fork: None,
        }
    }

    /// Who the check is by, for its heading: `designed`, `yours`, or the
    /// project command whose template gave it.
    pub fn check_by(&self, designed: bool) -> String {
        match &self.check_from {
            Some(name) => format!("from /{name}"),
            None if designed => "designed".into(),
            None => "yours".into(),
        }
    }

    /// A reply settled every call so far; later progress is for calls after it.
    fn settle(&mut self) {
        self.unsettled = Spend::default();
        self.unsettled_unknown = 0;
        self.open_calls.clear();
    }

    /// A plan's next step starts: a run of its own, with its own check and
    /// attempts. What the steps before it spent stays in the task's total.
    fn step_starts(&mut self) {
        self.design_spend = self.spend();
        self.lanes.clear();
        self.check_shown = false;
        self.designer_preview = Preview::default();
        self.role_previews.clear();
        self.designer_retry = None;
    }

    /// What this task has cost so far, by finished model calls; the run's
    /// reply settles it.
    pub fn spend(&self) -> Spend {
        self.lanes.iter().fold(self.design_spend, |sum, lane| sum + lane.spend)
    }

    /// The retry to show in the status bar: the one that starts soonest, of
    /// the design's and of attempts still running.
    pub fn retrying(&self) -> Option<&Retrying> {
        let lanes = self.lanes.iter().filter(|l| l.status.is_none()).filter_map(|l| l.retry.as_ref());
        self.designer_retry.iter().chain(lanes).min_by_key(|r| r.at)
    }
}

pub enum Prompt {
    Check(DesignResponse),
    /// The check's command is in the input box.
    EditCheck(DesignResponse),
    /// A plan's steps, as proposed or as the user edited them; the plan
    /// runs them once the user confirms.
    Plan(PlanSteps),
    /// The steps are in the input box, one per line.
    EditPlan(PlanSteps),
    Apply {
        resp: Box<RunResponse>,
        fork: String,
        /// What the user made of the result before applying it.
        part: Part,
    },
    /// No attempt passed. The input stays live; with it empty, `r` runs the
    /// task again with the same check and `v` shows why each attempt ended.
    Retry {
        req: Box<RunRequest>,
        designed: bool,
        /// The user stopped the run, rather than every attempt failing.
        stopped: bool,
        attempts: Vec<AttemptReport>,
        mode: Mode,
        settings: Box<Settings>,
    },
}

/// The part of a result the apply confirmation applies, once the user chose
/// some of its files or edited it, and how it did when checked again.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Part {
    /// Every change of the result as last checked; empty until then, when
    /// they are the result's own.
    pub all: Vec<molt_api::fs::Change>,
    /// The paths to apply; `None`: every change.
    pub only: Option<Vec<String>>,
    /// The user edited the result.
    pub edited: bool,
    /// The file list, while it is open: a mark for each of `all`, and the cursor.
    pub picking: Option<(Vec<bool>, usize)>,
    /// The check is running again; the confirmation waits for it.
    pub checking: bool,
    /// What would be applied now did not pass when checked again, or could not be checked.
    pub failed: bool,
}

/// A finished task's diff, kept after the task is applied, kept or discarded.
struct LastDiff {
    patch: String,
    truncated: bool,
}

pub enum Screen {
    Main,
    Memory(MemoryView),
}

pub struct App {
    /// The workspace as shown, `~` for the home directory.
    pub workspace: String,
    /// The canonical workspace path, as requests name it.
    workspace_path: String,
    pub mode: Mode,
    /// The model a task that names none runs on, once the services are up.
    pub default_model: Option<String>,
    pub settings: Settings,
    preference_defaults: super::settings::Saved,
    preference_changes: Vec<super::settings::Change>,
    reset_preferences: bool,
    pub commands: std::collections::BTreeMap<String, super::commands::Command>,
    pub theme: text::Theme,
    pub session: SessionState,
    pub backend_generation: u64,
    startup_retries: usize,
    retry_at: Option<u64>,
    git_busy: bool,
    quit_after_git: bool,
    pub log: Vec<Line<'static>>,
    /// Lines scrolled up from the end of the log; 0 follows it.
    pub scroll: usize,
    pub input: Input,
    history: Vec<String>,
    history_at: Option<usize>,
    history_draft: Option<Input>,
    history_changes: Vec<super::state::HistoryChange>,
    pub history_search: Option<HistorySearch>,
    pub task: Option<Task>,
    pub prompt: Option<Prompt>,
    pub viewer: Option<Viewer>,
    pub shortcuts: Option<Viewer>,
    pub transcript: Option<TranscriptView>,
    pub screen: Screen,
    pub last_run: Option<String>,
    /// Spent in this session: designs, runs and learning.
    pub spent_usd: f64,
    /// Calls whose usage or price is still unknown; spending is a subtotal.
    pub uncounted_calls: u32,
    /// Stopped work none of whose calls could be counted: a run without
    /// previews, or memory learning, whose reply never came.
    pub uncounted_stops: u32,
    /// Runs memory is learning from.
    pub learning: HashSet<String>,
    /// Results still available in their forks, including restored results.
    pub queue: Vec<queue::Queued>,
    pub queue_paused: bool,
    pub queue_view: Option<queue::View>,
    fork_operations: usize,
    pub kept: Vec<String>,
    kept_entries: Vec<super::kept::Entry>,
    kept_changes: Vec<super::kept::Change>,
    pub kept_view: Option<usize>,
    /// `/diff`: the workspace's changes against `HEAD`.
    pub changes: Option<changes::ChangesView>,
    /// The user's command (`!cmd`) running now, or ending.
    pub shell: Option<shell::ShellRun>,
    /// How the shell service runs commands, as the config says.
    pub sandbox: Sandbox,
    kept_pending: Option<String>,
    pending_result: Option<super::kept::Entry>,
    interrupted_task: Option<String>,
    /// The result waiting to be applied, to commit once it is: `/commit on`.
    to_commit: Option<Action>,
    /// The diff of the last task that finished, for `/last`.
    last: Option<LastDiff>,
    /// The attempts of the last task that finished, for `/lanes`.
    last_lanes: Vec<Lane>,
    /// The conversation so far: the latest finished tasks, oldest first,
    /// which each new task is told about. `/new` empties it.
    pub earlier: Vec<Earlier>,
    /// What `/continue` goes on from: the last finished task's conversation
    /// and, while its result is kept, its fork.
    pub continuation: Option<Continuation>,
    /// The workspace's runs that stopped with a checkpoint, as last listed.
    stopped_runs: Vec<runs::Stopped>,
    /// `/runs` asked for the list, to show in full.
    runs_asked: bool,
    /// The run the last interface stopped in, until it is resumed or dropped.
    restored: Option<super::saved::Active>,
    /// Of the runs that stopped with a checkpoint, what the session total
    /// counts already: their cost and uncounted calls when their reply came.
    settled_runs: std::collections::BTreeMap<String, (f64, u32)>,
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
    /// The tick of a first Esc while a task runs; a second soon after stops it.
    stop_armed: Option<u64>,
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
    pub command_pick: usize,
    command_hidden: Option<String>,
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
            default_model: None,
            settings: Settings::default(),
            preference_defaults: super::settings::Saved::new(Mode::Confirm, Settings::default()),
            preference_changes: Vec::new(),
            reset_preferences: false,
            commands: Default::default(),
            theme: text::Theme::detect(std::env::var("COLORTERM").ok().as_deref()),
            session: SessionState::Starting,
            backend_generation: 0,
            startup_retries: 0,
            retry_at: None,
            git_busy: false,
            quit_after_git: false,
            log: Vec::new(),
            scroll: 0,
            input: Input::default(),
            history: Vec::new(),
            history_at: None,
            history_draft: None,
            history_changes: Vec::new(),
            history_search: None,
            task: None,
            prompt: None,
            viewer: None,
            shortcuts: None,
            transcript: None,
            screen: Screen::Main,
            last_run: None,
            spent_usd: 0.0,
            uncounted_calls: 0,
            uncounted_stops: 0,
            learning: HashSet::new(),
            queue: Vec::new(),
            queue_paused: false,
            queue_view: None,
            fork_operations: 0,
            kept: Vec::new(),
            kept_entries: Vec::new(),
            kept_changes: Vec::new(),
            kept_view: None,
            changes: None,
            shell: None,
            sandbox: Sandbox::Unknown,
            kept_pending: None,
            pending_result: None,
            interrupted_task: None,
            to_commit: None,
            last: None,
            last_lanes: Vec::new(),
            earlier: Vec::new(),
            continuation: None,
            stopped_runs: Vec::new(),
            runs_asked: false,
            restored: None,
            settled_runs: Default::default(),
            secrets: Vec::new(),
            rules: ConfirmSection::default(),
            zoom: None,
            flash: None,
            tick: 0,
            quit_armed: None,
            stop_armed: None,
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
            command_pick: 0,
            command_hidden: None,
        }
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
        if let Some(entry) = &self.pending_result {
            if !keep.contains(&entry.fork) {
                keep.push(entry.fork.clone());
            }
        }
        keep
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        let msg = match msg {
            Msg::Backend { generation, message } if generation == self.backend_generation => *message,
            Msg::Backend { .. } => return Vec::new(),
            msg => msg,
        };

        let before = self.attention();
        if matches!(msg, Msg::Ran { .. } | Msg::Planned { .. } | Msg::Applied(_) | Msg::Stopped { .. }) {
            self.files_stale = true;
        }
        // The last turns of a run that ended are read once more.
        if let (Msg::Ran { trace, .. }, Some(view)) = (&msg, &mut self.transcript) {
            view.stale |= view.trace == *trace;
        }
        let preferences_before =
            matches!(&msg, Msg::Key(_)).then(|| super::settings::Saved::new(self.mode, self.settings.clone()));
        let input_before = matches!(&msg, Msg::Key(_) | Msg::Paste(_) | Msg::Edited(_))
            .then(|| (self.input.text.clone(), self.input.cursor));
        if matches!(&msg, Msg::Applied(_) | Msg::Discarded(_) | Msg::RunDropped { .. }) {
            self.fork_operations = self.fork_operations.saturating_sub(1);
        }
        let mut actions = self.handle(msg);
        if actions.iter().any(|a| matches!(a, Action::Quit { .. })) {
            if let Some(entry) = self.pending_result.clone() {
                self.keep_entry(entry);
            }
        }
        self.git_busy |= actions.iter().any(|a| matches!(a, Action::Commit { .. } | Action::Undo { .. }));
        self.fork_operations += actions
            .iter()
            .filter(|a| matches!(a, Action::Apply { .. } | Action::ApplyOnly { .. } | Action::Discard { .. }))
            .count();
        if self.quit_after_git && !self.git_busy {
            self.quit = true;
            actions.push(Action::Quit { keep: self.keep() });
        }
        actions.extend(self.advance_queue());
        if self.reset_preferences {
            self.preference_changes.clear();
            self.preference_changes.push(super::settings::Change::Reset);
            self.reset_preferences = false;
        } else if preferences_before
            .is_some_and(|before| before != super::settings::Saved::new(self.mode, self.settings.clone()))
        {
            let change = super::settings::Change::Save(super::settings::Saved::new(self.mode, self.settings.clone()));
            if matches!(self.preference_changes.last(), Some(super::settings::Change::Save(_))) {
                self.preference_changes.pop();
            }
            self.preference_changes.push(change);
        }
        if input_before.is_some_and(|(text, cursor)| text != self.input.text || cursor != self.input.cursor) {
            self.command_pick = 0;
            self.command_hidden = None;
        }
        if !self.listing && self.files_stale && self.at_query().is_some() {
            self.listing = true;
            self.files_stale = false;
            actions.push(Action::ListFiles);
        }
        if self.transcript.as_ref().is_some_and(|v| v.reader.turns.is_empty()) {
            // What the run published shows meanwhile, and changes as it goes.
            self.render_transcript();
        }
        actions.extend(self.read_transcript());
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
                if let Some(search) = &mut self.history_search {
                    search.query.insert(&text);
                    search.selected = 0;
                    return Vec::new();
                }
                if let Some(view) = &mut self.queue_view {
                    view.draft.get_or_insert_with(Input::default).paste(&text);
                    return Vec::new();
                }
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
                self.command_settle();
                if self.retry_at.is_some_and(|at| self.tick >= at) {
                    self.retry_at = None;
                    if matches!(self.session, SessionState::Down(_)) && self.task.is_none() && self.learning.is_empty()
                    {
                        self.session = SessionState::Starting;
                        return vec![Action::StartSession];
                    }
                }
                Vec::new()
            }
            Msg::Signal if self.git_busy => {
                self.quit_after_git = true;
                self.flash = Some("waiting for Git and memory before quitting".into());
                Vec::new()
            }
            Msg::Signal => {
                self.quit = true;
                vec![Action::Quit { keep: self.keep() }]
            }
            Msg::SessionUp { versions, memory_problem, default_model } => {
                self.startup_retries = 0;
                self.default_model = default_model.map(|m| clean_line(&m));
                self.retry_at = None;
                if let Some(problem) = &memory_problem {
                    self.push(Line::from(span(format!("running without memory: {}", clean_line(problem)), Tone::Dim)));
                }
                self.session = SessionState::Up { versions, memory_problem };
                Vec::new()
            }
            Msg::SessionFailed(e) => {
                let starting = matches!(self.session, SessionState::Starting);
                let error = scrub(&e, &self.secrets);
                self.error("the services did not start", &error);
                self.session = SessionState::Down(clean_line(&error));
                if starting && self.task.is_none() && self.learning.is_empty() {
                    if retryable_startup_error(&e) && self.startup_retries < STARTUP_RETRY_TICKS.len() {
                        self.retry_at = Some(self.tick + STARTUP_RETRY_TICKS[self.startup_retries]);
                        self.startup_retries += 1;
                    } else {
                        self.retry_at = None;
                        self.flash = Some(if retryable_startup_error(&e) {
                            "startup retries exhausted; /log shows the service output, /restart tries again".into()
                        } else {
                            "check the configuration or credentials and relaunch; /log shows the service output".into()
                        });
                    }
                }
                Vec::new()
            }
            Msg::BackendFailed(error) => {
                self.retry_at = None;
                self.command_lost();
                let error = scrub(&error, &self.secrets);
                self.error("the interface backend stopped", &error);
                self.session = SessionState::Down(clean_line(&error));
                self.flash =
                    Some("backend unavailable; pending operations have unknown settlement; /restart when idle".into());
                Vec::new()
            }
            Msg::Backend { .. } => Vec::new(),
            Msg::ServiceLog(result) => {
                if self.viewer.as_ref().is_some_and(|v| v.title == "service log") {
                    let lines = match result {
                        Ok(lines) => lines
                            .into_iter()
                            .map(|line| {
                                let line = clean_line(&scrub(&line, &self.secrets));
                                if !line.trim().is_empty() && !super::state::keep_history(&line, &[]) {
                                    "[log line omitted: credential-like or oversized content]".into()
                                } else {
                                    line
                                }
                            })
                            .collect::<Vec<_>>(),
                        Err(e) => vec![format!("could not read the service log: {}", clean_line(&e))],
                    };
                    self.viewer.as_mut().unwrap().lines =
                        lines.into_iter().flat_map(|l| block(&l, "", Tone::Plain)).collect();
                }
                Vec::new()
            }
            Msg::Stopped { kept, notes } => {
                self.fork_operations = 0;
                self.command_lost();
                for note in &notes {
                    self.push(Line::from(span(format!("  {}", clean_line(note)), Tone::Warn)));
                }
                if !kept.is_empty() {
                    self.push(Line::from(span(format!("  kept {} · interrupted", kept.len()), Tone::Dim)));
                    for fork in &kept {
                        self.push(Line::from(span(format!("  kept in {}", clean_line(fork)), Tone::Dim)));
                    }
                    let task = self.interrupted_task.take().unwrap_or_else(|| "interrupted task".into());
                    for fork in kept {
                        self.keep_entry(super::kept::Entry::new(fork, &task, "interrupted"));
                    }
                }
                self.session = SessionState::Starting;
                vec![Action::StartSession]
            }
            Msg::Progress(event @ Progress::Output { .. }) => {
                self.command_output(event);
                Vec::new()
            }
            Msg::Progress(event) => {
                self.progress(event);
                Vec::new()
            }
            Msg::CommandRan { trace, result } => {
                self.command_ran(trace, result);
                Vec::new()
            }
            Msg::Changes(result) => {
                self.listed_changes(result);
                Vec::new()
            }
            Msg::FileDiff { path, result } => {
                self.file_diff(path, result);
                Vec::new()
            }
            Msg::Designed { trace, result, existing } => self.designed(trace, result, &existing),
            Msg::Ran { trace, result } => self.ran(trace, *result),
            Msg::Planned { trace, result } => self.planned(trace, *result),
            Msg::Runs(runs) => {
                self.listed_runs(runs);
                Vec::new()
            }
            Msg::RunDropped { run, result } => self.run_dropped(run, result),
            Msg::Learned { trace, result } => {
                self.learning.remove(trace.as_str());
                self.learned(result);
                Vec::new()
            }
            Msg::Controlled { trace, action, result } => {
                self.controlled(&trace, &action, result);
                Vec::new()
            }
            Msg::Diff { fork, result } => {
                if self.viewer.as_ref().is_none_or(|v| v.title != format!("kept diff: {fork}")) {
                    return Vec::new();
                }
                self.viewer = Some(match result {
                    Ok(diff) => {
                        if let Some(entry) = self.kept_entries.iter_mut().find(|entry| entry.fork == fork) {
                            entry.diff(diff.changes.len(), &diff.patch);
                            self.kept_changes.push(super::kept::Change::Save(entry.clone()));
                        }
                        let mut view = diff_viewer(&scrub(&diff.patch, &self.secrets), diff.truncated);
                        view.title = format!("kept diff: {fork}");
                        view
                    }
                    Err(error) => Viewer {
                        title: "kept diff unavailable".into(),
                        lines: block(&scrub(&error, &self.secrets), "", Tone::Bad),
                        scroll: 0,
                    },
                });
                Vec::new()
            }
            Msg::Applied(result) if self.kept_pending.is_some() => {
                self.kept_finished(result, "applied");
                Vec::new()
            }
            Msg::Discarded(result) if self.kept_pending.is_some() => {
                self.kept_finished(result, "discarded");
                Vec::new()
            }
            Msg::Applied(result) => {
                if result.is_err() {
                    if let Some(entry) = self.pending_result.take() {
                        self.keep_entry(entry);
                    }
                } else {
                    self.pending_result = None;
                }
                let commit = self.to_commit.take().filter(|_| result.is_ok());
                match result {
                    Ok(()) => {
                        self.push(Line::from(span("  applied", Tone::Ok)));
                        // The result the next task goes on from is in the workspace now.
                        if let Some(c) = &mut self.continuation {
                            c.fork = None;
                        }
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
                self.git_busy = false;
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
            Msg::Undone(result) => {
                self.git_busy = false;
                match result {
                    Ok(undone) => {
                        let run = undone.git.run().to_owned();
                        let line = match undone.git {
                            crate::git::Undo::Reverted { commit, sha, .. } => format!(
                                "undid run {run}: reverted {} in {}",
                                &commit[..12.min(commit.len())],
                                &sha[..12.min(sha.len())]
                            ),
                            crate::git::Undo::Already { sha, .. } => {
                                format!("run {run} was undone already, in {}", &sha[..12.min(sha.len())])
                            }
                        };
                        self.push(Line::from(span(clean_line(&line), Tone::Ok)));
                        self.earlier.clear();
                        self.files_stale = true;
                        match undone.memory {
                            Some(Ok(response)) => self.push(Line::from(span(
                                format!(
                                    "  memory no longer counts the run: {} notes forgotten, {} adjusted",
                                    response.retracted, response.adjusted
                                ),
                                Tone::Mem,
                            ))),
                            Some(Err(error)) => {
                                self.error(
                                    "Git is undone, but memory could not be told",
                                    &scrub(&error, &self.secrets),
                                );
                                self.flash =
                                    Some(format!("/undo {run} retries the memory notice without another revert"));
                                self.queue_paused |= !self.queue.is_empty();
                            }
                            None => self
                                .push(Line::from(span("  memory is not configured; no notification sent", Tone::Dim))),
                        }
                    }
                    Err(error) => self.error("could not undo", &scrub(&error, &self.secrets)),
                }
                Vec::new()
            }
            Msg::Discarded(result) => {
                if let Err(e) = result {
                    self.error("could not remove the result", &e);
                    if let Some(entry) = self.pending_result.take() {
                        self.keep_entry(entry);
                    }
                } else {
                    self.pending_result = None;
                    // Its result is gone: there is nothing to go on from.
                    if self.continuation.as_ref().is_some_and(|c| c.fork.is_some()) {
                        self.continuation = None;
                    }
                }
                Vec::new()
            }
            Msg::Rechecked { fork, only, result } => self.rechecked(&fork, only, result),
            Msg::ForkEdited { fork, result } => {
                let Some(Prompt::Apply { resp, fork: shown, part }) = &mut self.prompt else { return Vec::new() };
                if *shown != fork {
                    return Vec::new();
                }
                match result {
                    Ok(()) => {
                        part.edited = true;
                        part.checking = true;
                        vec![Action::Recheck { fork, only: part.only.clone(), check: resp.check.clone() }]
                    }
                    Err(e) => {
                        self.flash = Some(format!("the editor failed: {}", clean_line(&e)));
                        Vec::new()
                    }
                }
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
            Msg::Audit { trace, result } => {
                let Some(view) = self.transcript.as_mut().filter(|v| v.trace == trace) else {
                    return Vec::new();
                };
                (view.reading, view.read_at) = (false, self.tick);
                match result {
                    Ok(page) => {
                        view.reader.add(page.entries);
                        view.reader.cursor = page.cursor;
                        view.stale |= page.more;
                    }
                    Err(e) => view.unreadable = Some(clean_line(&e)),
                }
                self.render_transcript();
                Vec::new()
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

    /// A task is waiting on the services.
    fn busy(&self) -> bool {
        self.task.as_ref().is_some_and(|t| matches!(t.phase, Phase::Designing | Phase::Running | Phase::Applying))
    }

    /// The attempts of a task are running.
    fn running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| t.phase == Phase::Running)
    }
}

/// `text` with every secret in it replaced.
pub(super) fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[redacted]");
        }
    }
    out
}

fn retryable_startup_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    ![
        "not set",
        "missing api",
        "api key is missing",
        "invalid api key",
        "authentication failed",
        "unauthorized",
        "invalid credentials",
        "invalid config",
        "invalid configuration",
        "no such file",
        "permission denied",
        "unknown provider",
        "unknown model",
        "unsupported",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

/// Lines as plain text, for tests.
#[cfg(test)]
pub fn plain(lines: &[Line<'static>]) -> Vec<String> {
    lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn startup_retries_back_off_three_times_and_success_cancels_the_timer() {
        let mut app = App::new("/w", None);
        for delay in STARTUP_RETRY_TICKS {
            assert_eq!(
                app.update(Msg::SessionFailed("connection refused".into())),
                [Action::Alert { text: "molt: services down".into() }]
            );
            assert!(app.retry_seconds().is_some());
            assert!(frame_text(&mut app).contains("retrying in"));
            for _ in 1..delay {
                assert!(app.update(Msg::Tick).is_empty());
            }
            assert_eq!(app.update(Msg::Tick), [Action::StartSession]);
            assert!(app.update(Msg::Tick).is_empty());
        }
        app.update(Msg::SessionFailed("connection refused".into()));
        for _ in 0..200 {
            assert!(app.update(Msg::Tick).is_empty());
        }
        assert!(app.retry_seconds().is_none());
        assert!(app.flash.as_ref().is_some_and(|s| s.contains("exhausted")));
        up(&mut app);
        app.session = SessionState::Starting;
        app.update(Msg::SessionFailed("connection refused".into()));
        assert!(app.retry_seconds().is_some());
        up(&mut app);
        for _ in 0..200 {
            assert!(app.update(Msg::Tick).is_empty());
        }
        assert!(matches!(app.session, SessionState::Up { .. }));
    }

    #[test]
    fn configuration_errors_and_a_closed_backend_do_not_retry_automatically() {
        for error in ["ANTHROPIC_API_KEY is not set", "invalid configuration", "permission denied", "unknown provider"]
        {
            let mut app = App::new("/w", None);
            app.update(Msg::SessionFailed(error.into()));
            assert!(app.retry_seconds().is_none(), "{error}");
            assert!(app.flash.as_ref().unwrap().contains("configuration or credentials"));
            for _ in 0..200 {
                assert!(app.update(Msg::Tick).is_empty());
            }
        }
        let mut app = App::new("/w", None);
        app.update(Msg::BackendFailed("pipe closed".into()));
        assert!(app.retry_seconds().is_none());
        for _ in 0..200 {
            assert!(app.update(Msg::Tick).is_empty());
        }
    }

    #[test]
    fn manual_restart_preserves_settings_logs_and_kept_results_and_rejects_busy_tasks() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/attempts 4");
        app.kept = vec!["/fork/kept".into()];
        app.git_busy = true;
        assert!(type_in(&mut app, "/restart").is_empty());
        app.update(Msg::Committed(Ok(None)));
        app.push(Line::from("existing log"));
        assert_eq!(type_in(&mut app, "/restart"), [Action::RestartSession { keep: vec!["/fork/kept".into()] }]);
        assert_eq!(app.settings.attempts, Some(4));
        assert_eq!(app.kept, ["/fork/kept"]);
        assert!(log(&app).contains("existing log"));
        assert!(type_in(&mut app, "/restart").is_empty());
        up(&mut app);
        type_in(&mut app, "implement it");
        assert!(type_in(&mut app, "/restart").is_empty());
        assert!(app.flash.as_ref().unwrap().contains("finish or stop"));
        assert!(app.task.is_some());
    }

    #[test]
    fn messages_from_a_replaced_backend_cannot_take_down_the_new_session() {
        let mut app = App::new("/w", None);
        app.backend_generation = 2;
        up(&mut app);
        app.update(Msg::Backend { generation: 1, message: Box::new(Msg::BackendFailed("old EOF".into())) });
        assert!(matches!(app.session, SessionState::Up { .. }));
        app.update(Msg::Backend { generation: 1, message: Box::new(Msg::SessionFailed("old startup failure".into())) });
        assert!(matches!(app.session, SessionState::Up { .. }));
        app.update(Msg::Backend { generation: 2, message: Box::new(Msg::BackendFailed("current EOF".into())) });
        assert!(matches!(app.session, SessionState::Down(_)));
    }
}
