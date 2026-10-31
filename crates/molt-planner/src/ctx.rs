//! What every part of one run shares: the bus, the limits, the spend so far,
//! the forks still alive, and typed helpers for the calls a run makes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use molt_api::fs;
use molt_api::model::{self, CompleteRequest, CompleteResponse, Effort, LimitsRequest, LimitsResponse, Role, Usage};
use molt_api::planner::RunRequest;
use molt_api::progress::{self, Progress};
use molt_api::role::PromptUse;
use molt_api::workflow::Workflow;
use molt_proto::{Budget, ErrorCode, RemoteError, TraceId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::checkpoint::{Saved, SavedSpend, Store};
use crate::control::Control;
use crate::fit::Limits;
use crate::memory::Memory;
use crate::pack::{Seat, Seats};
use crate::registry::Registry;
use crate::{Bus, Config};

/// Deadline for reading, writing, listing and searching files.
pub(crate) const FILE_MS: u64 = 60_000;
/// Deadline for fork, diff, merge and drop, which copy or walk whole trees.
pub(crate) const TREE_MS: u64 = 600_000;
/// Added to a command's own timeout for the reply deadline, so the shell
/// service can report the timeout itself.
pub(crate) const SHELL_GRACE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Spend {
    pub usage: Usage,
    pub cost_usd: f64,
    pub unknown_calls: u32,
    /// The most one known call cost: what each call in flight is assumed to
    /// cost when budget is held back for it.
    pub max_call_usd: f64,
}

impl Spend {
    pub fn add(&mut self, resp: &CompleteResponse) {
        self.usage.add(&resp.usage);
        if resp.cost_usd.is_none() {
            self.unknown_calls += 1;
        }
        // This is the known subtotal; unknown calls are reported separately.
        self.cost_usd += resp.cost_usd.unwrap_or(0.0);
        self.max_call_usd = self.max_call_usd.max(resp.cost_usd.unwrap_or(0.0));
    }

    /// Count what another loop spent, such as an attempt's explorer, as this one's too.
    pub fn absorb(&mut self, other: &Spend) {
        self.usage.add(&other.usage);
        self.cost_usd += other.cost_usd;
        self.unknown_calls += other.unknown_calls;
        self.max_call_usd = self.max_call_usd.max(other.max_call_usd);
    }
}

/// The run's spend and its model calls still unanswered, kept together so a
/// reply is always counted in exactly one of them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tally {
    pub spend: Spend,
    pub pending: u32,
    pub unsettled: u32,
    /// Unsettled calls of attempts the run itself stopped while the run went
    /// on, such as the ones that lost the race. What each cost is unknown, so
    /// they are reported, but they do not stop the rest of the run: a run can
    /// pass its budget by up to one call for each attempt it stopped.
    pub abandoned: u32,
}

impl Tally {
    pub fn uncounted_calls(&self) -> u32 {
        self.pending
            .saturating_add(self.unsettled)
            .saturating_add(self.abandoned)
            .saturating_add(self.spend.unknown_calls)
    }
}

/// A `model.complete` error that says the call was refused before anything
/// was billed: a bad key, a wrong URL, a model that does not exist.
pub(crate) fn refused(error: &RemoteError) -> bool {
    matches!(error.code, ErrorCode::Denied | ErrorCode::Invalid)
}

/// What the gateway says calls to one model can take: asked once, before
/// the first call to it, and corrected by what its replies report.
#[derive(Clone, Copy, Debug, Default)]
struct Known {
    /// The gateway was asked, or cannot be.
    asked: bool,
    context: Option<u32>,
    max_output: Option<u32>,
    /// The run was told the window is a guess, so it is told once.
    guess_noted: bool,
    /// The run was told a reply's window differs from what was used.
    differs_noted: bool,
}

#[derive(Default)]
struct WebHealth {
    failures: u32,
    disabled: bool,
}

#[derive(Clone)]
pub(crate) struct Ctx {
    bus: Arc<dyn Bus>,
    pub cfg: Arc<Config>,
    pub trace: TraceId,
    pub task: String,
    pub workspace: String,
    pub model: Option<String>,
    /// The judge's model, when it is not the run's.
    pub judge_model: Option<String>,
    pub effort: Option<Effort>,
    pub max_turns: u32,
    pub max_check_rounds: u32,
    /// What file writes and edits the model asks for do about syntax errors they introduce.
    pub syntax_check: fs::SyntaxCheck,
    pub budget_usd: f64,
    /// The most designing the check may spend, at most `budget_usd`.
    pub design_budget_usd: f64,
    /// While the check is designed: the run's known cost at which designing
    /// stops, so that it spends at most its share (see [`Ctx::capped`]).
    pub spend_cap: Option<f64>,
    pub stream: bool,
    /// A person can answer: attempts are offered `ask_user`.
    pub interactive: bool,
    /// What memory knows about the workspace; empty when memory is not up.
    pub memory: Memory,
    /// The planner may read the web (it holds a capability for `web.fetch`):
    /// attempts are offered `fetch_url`.
    pub web: bool,
    /// Attempts are offered the process tools: the planner's starter turned
    /// them on and the planner holds a capability for `shell.start`.
    pub processes: bool,
    /// Attempts are offered `git`: the planner's starter turned it on and
    /// the planner holds a capability for `fs.git`.
    pub git: bool,
    /// The tools this run may offer: the built-ins, and the provided tools
    /// whose method the planner holds a capability for.
    pub tools: Arc<Registry>,
    web_health: Arc<Mutex<WebHealth>>,
    /// The `<project_instructions>` block, when the project has instruction files.
    pub instructions: Option<String>,
    /// The instruction files of directories each attempt reached as it worked.
    pub dir_instructions: Arc<crate::scoped::Scoped>,
    /// The Agent Skills found when the run started, and their `<skills>` block.
    pub skills: Arc<crate::skills::Catalog>,
    /// The files the user pointed at, and their `<referenced_files>` block.
    pub files: Vec<String>,
    pub referenced: Option<String>,
    tally: watch::Sender<Tally>,
    /// Forks created and not yet dropped or merged, so none outlives the run by accident.
    forks: Arc<Mutex<Vec<String>>>,
    pub cancel: CancellationToken,
    pub run_cancel: CancellationToken,
    /// Where the run's checkpoint is saved, when it keeps one.
    pub checkpoint: Option<Arc<Store>>,
    /// Calls not counted before this resume of the run: they do not stop
    /// it, as calls of its own with an unknown cost do, but are reported.
    pub carried_uncounted: u32,
    /// The run's requirements and whether it is paused.
    pub control: Arc<Control>,
    /// Earlier tasks of the conversation, for the model, or `None`.
    pub earlier: Option<String>,
    /// The stages the run goes through.
    pub workflow: Arc<Workflow>,
    /// The task before this one that the run goes on from.
    pub continues: Option<Arc<molt_api::planner::Continuation>>,
    /// Where the run's forks are made from, and so where its result is
    /// merged: a step of a plan works on the result of the steps before it
    /// (see [`crate::plan`]). `None`: the workspace.
    pub base: Option<String>,
    /// Each role that has made a model call and its prompt, in the order
    /// they first did.
    spoke: Arc<Mutex<Vec<PromptUse>>>,
    /// What each model called in the run can take, by the name calls give
    /// it (`None`: the gateway's default), shared by every part of the run.
    models: Arc<Mutex<HashMap<Option<String>, Known>>>,
    /// Where an attempt of an adaptive run reports what the schedule decides
    /// on (see [`crate::schedule`]); `None` in a fixed run.
    pub signals: Option<tokio::sync::mpsc::UnboundedSender<crate::schedule::Signal>>,
    /// What the attempts before this one tried and how they ended, for an
    /// attempt an adaptive run started after them.
    pub prior: Option<Arc<str>>,
}

/// A conversation that grows until it no longer fits a model call (see
/// [`crate::fit`]). `system` and `tools` stay the same for its whole life:
/// an unchanged prefix keeps the prompt cache warm.
pub(crate) struct Conversation {
    pub(crate) system: Arc<str>,
    pub(crate) tools: Arc<Vec<Value>>,
    pub(crate) messages: Vec<Value>,
    pub(crate) attempt: Option<u32>,
    /// Who holds it, named in its model calls.
    pub(crate) role: Role,
    /// The model for this conversation, in place of the run's.
    pub(crate) model: Option<String>,
    /// The effort for this conversation, in place of the run's.
    pub(crate) effort: Option<Effort>,
    /// The role it speaks as and its prompt's version, for the audit log.
    pub(crate) prompt: Option<PromptUse>,
    /// How many messages the model last measured, and the tokens it counted
    /// for them, its reply included.
    pub(crate) known: Option<(usize, u64)>,
    /// What it keeps of the turns it left out to fit, once it has left some out.
    pub(crate) compacted: Option<crate::compact::Compacted>,
    /// What holds now, from the run's own state, for the record of left-out
    /// turns: set by whoever holds the conversation, never by the model.
    pub(crate) standing: Option<String>,
    /// The spend so far, for the same record, as of the turn it is written.
    pub(crate) budget: Option<String>,
    /// It may read left-out turns back with `earlier_result`, and have
    /// notes written on them.
    pub(crate) recall: bool,
}

impl Conversation {
    pub fn new(
        system: impl Into<Arc<str>>,
        tools: Arc<Vec<Value>>,
        first: Value,
        role: Role,
        attempt: Option<u32>,
    ) -> Self {
        let system = system.into();
        Self {
            system,
            tools,
            messages: vec![first],
            attempt,
            role,
            model: None,
            effort: None,
            prompt: None,
            known: None,
            compacted: None,
            standing: None,
            budget: None,
            recall: false,
        }
    }

    /// A conversation `seat` holds: its system prompt (with `part` after the
    /// pack's text), the tools of `tools` it may have, its model and effort.
    pub fn seated(seat: &Seat, part: &str, tools: Vec<Value>, first: Value, role: Role, attempt: Option<u32>) -> Self {
        let mut conv = Self::new(seat.system(part), Arc::new(seat.offer(tools)), first, role, attempt);
        conv.model = seat.model.clone();
        conv.effort = seat.effort;
        conv.prompt = Some(seat.prompt.clone());
        conv
    }

    /// Who holds it, as a person reads it: `attempt 1`, `reviewer`.
    pub fn speaker(&self) -> String {
        model::speaker(Some(self.role), self.attempt)
    }

    pub fn push(&mut self, message: Value) {
        self.messages.push(message);
    }

    /// Tell the model `text`: after a reply, as a message of its own; after
    /// tool results, with them, so the user's messages never come two in a row.
    pub fn tell(&mut self, text: String) {
        if let Some(last) = self.messages.last_mut().filter(|m| m["role"] == "user") {
            if let Some(content) = last["content"].as_array_mut() {
                content.push(model::text_block(text));
                return;
            }
        }
        self.messages.push(model::user_text(text));
    }
}

impl Ctx {
    pub fn web_available(&self) -> bool {
        self.web && !self.web_health.lock().unwrap_or_else(PoisonError::into_inner).disabled
    }

    pub fn web_succeeded(&self) {
        self.web_health.lock().unwrap_or_else(PoisonError::into_inner).failures = 0;
    }

    /// Only an authenticated service error can disable the run's web tool, never page text.
    pub fn web_failed(&self, error: &RemoteError) -> bool {
        if error.code != ErrorCode::Unavailable || !error.message.starts_with(molt_api::web::ENVIRONMENT_UNAVAILABLE) {
            return false;
        }
        let mut health = self.web_health.lock().unwrap_or_else(PoisonError::into_inner);
        health.failures = health.failures.saturating_add(1);
        if health.failures >= 2 && !health.disabled {
            health.disabled = true;
            return true;
        }
        false
    }

    /// Limits come from the request where it sets them and from `cfg` otherwise.
    pub fn new(bus: Arc<dyn Bus>, cfg: Arc<Config>, trace: TraceId, req: &RunRequest) -> Self {
        let model = req.model.clone().filter(|m| !m.trim().is_empty()).or_else(|| cfg.default_model.clone());
        let web = bus.holds(molt_api::web::FETCH);
        let processes = cfg.processes && bus.holds(molt_api::shell::START);
        let git = cfg.git && bus.holds(fs::GIT);
        let tools = Arc::new(Registry::new(&cfg.tools, |target| bus.holds(target)));
        let budget_usd = req.budget_usd.unwrap_or(cfg.budget_usd);
        // The request was validated, which resolved its workflow.
        let workflow = crate::runner::workflow(&cfg, req.workflow.as_deref()).unwrap_or_else(|_| Workflow::verify());
        // The designer role's share, else the workflow's, else the planner's.
        let role_share = cfg.pack.seats(&workflow).ok().and_then(|s| s.designer.budget_share);
        let share = role_share.or(workflow.stages.design.and_then(|d| d.budget_share)).unwrap_or(cfg.design_share);
        Self {
            bus,
            trace,
            task: req.task.clone(),
            workspace: req.workspace.clone(),
            model,
            judge_model: req.judge_model.clone().filter(|m| !m.trim().is_empty()),
            effort: req.effort,
            max_turns: req.max_turns.unwrap_or(cfg.max_turns),
            max_check_rounds: req.max_check_rounds.unwrap_or(cfg.max_check_rounds),
            syntax_check: req.syntax_check.unwrap_or(cfg.syntax_check),
            budget_usd,
            design_budget_usd: req.design_budget_usd.unwrap_or(share * budget_usd).min(budget_usd),
            spend_cap: None,
            stream: req.stream,
            interactive: req.interactive,
            memory: Memory::default(),
            web,
            processes,
            git,
            tools,
            web_health: Arc::default(),
            instructions: None,
            dir_instructions: Arc::default(),
            skills: Arc::default(),
            files: req.files.clone(),
            referenced: None,
            cfg,
            tally: watch::Sender::new(Tally::default()),
            forks: Arc::default(),
            cancel: CancellationToken::new(),
            run_cancel: CancellationToken::new(),
            checkpoint: None,
            carried_uncounted: 0,
            control: Control::new(Vec::new()),
            earlier: crate::prompts::earlier_tasks(&req.earlier),
            workflow: Arc::new(workflow),
            continues: req.continues.clone().map(Arc::new),
            base: None,
            spoke: Arc::default(),
            models: Arc::default(),
            signals: None,
            prior: None,
        }
    }

    /// Who speaks in each part of the run, as its workflow seats them.
    pub fn seats(&self) -> Seats {
        // The request was validated, which checked the workflow's roles.
        self.cfg.pack.seats(&self.workflow).unwrap_or_else(|why| {
            tracing::warn!(run = %self.trace, %why, "the workflow's roles do not resolve; using the built-in ones");
            crate::pack::Pack::default().seats(&Workflow::verify()).expect("the built-in roles resolve")
        })
    }

    /// Each role that spoke in the run and its prompt, in the order they first spoke.
    pub fn spoke(&self) -> Vec<PromptUse> {
        self.spoke.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Change the run's checkpoint and save it, when it keeps one. A save
    /// that fails is logged: the run goes on without it.
    pub fn save(&self, change: impl FnOnce(&mut Saved)) {
        if let Some(store) = &self.checkpoint {
            if let Err(e) = store.update(change) {
                tracing::warn!(run = %self.trace, error = %e, "could not save the checkpoint");
            }
        }
    }

    /// Start counting from what a checkpoint says the run spent before.
    pub fn restore(&mut self, spend: &SavedSpend, open_calls: u32) {
        self.tally.send_modify(|t| {
            t.spend.usage = spend.usage;
            t.spend.cost_usd = spend.cost_usd;
        });
        self.carried_uncounted = spend.uncounted_calls + open_calls;
    }

    /// The spend to save: everything so far, earlier resumes included.
    fn saved_spend(&self) -> SavedSpend {
        let t = self.tally();
        SavedSpend {
            usage: t.spend.usage,
            cost_usd: t.spend.cost_usd,
            uncounted_calls: self.carried_uncounted + t.unsettled + t.abandoned + t.spend.unknown_calls,
        }
    }

    /// The model `conv`'s calls go to: its own, its role's, else the run's.
    pub fn model_of(&self, conv: &Conversation) -> Option<String> {
        conv.model.clone().or_else(|| self.cfg.role_models.get(&conv.role).cloned()).or_else(|| self.model.clone())
    }

    fn known(&self, model: &Option<String>) -> Known {
        self.models.lock().unwrap_or_else(PoisonError::into_inner).get(model).copied().unwrap_or_default()
    }

    /// Ask the gateway, once for each model, what a call to `model` can
    /// take, so that even the first call to it is fitted to that model and
    /// not to the run's. A gateway that cannot say (an older one, or a
    /// planner without the capability) leaves it to the setting or the guess.
    pub async fn learn(&self, model: &Option<String>) {
        if self.known(model).asked {
            return;
        }
        let mut answer = LimitsResponse::default();
        if self.bus.holds(model::LIMITS) {
            let req = LimitsRequest { model: model.clone() };
            match self.call::<LimitsResponse>(model::LIMITS, req, Budget::new(0, FILE_MS, 0)).await {
                Ok(limits) => answer = limits,
                // Another part of the run asks again: this one was stopped, not answered.
                Err(e) if e.code == ErrorCode::Cancelled => return,
                Err(e) => {
                    tracing::debug!(run = %self.trace, ?model, error = %e, "the gateway did not say the model's limits")
                }
            }
        }
        let mut models = self.models.lock().unwrap_or_else(PoisonError::into_inner);
        let known = models.entry(model.clone()).or_default();
        known.asked = true;
        // A reply may have reported its window while this was asked; the reply's stands.
        known.context = known.context.or(answer.context_tokens.filter(|&n| n > 0));
        known.max_output = known.max_output.or(answer.max_output_tokens.filter(|&n| n > 0));
    }

    /// The `max_tokens` a call to `model` asks for: `MOLT_MAX_TOKENS`,
    /// lowered to the most the gateway says the model writes, so the room
    /// kept for the reply is what the reply can take and no more.
    pub fn output_of(&self, model: &Option<String>) -> u32 {
        self.known(model).max_output.map_or(self.cfg.max_tokens, |cap| cap.min(self.cfg.max_tokens))
    }

    /// What one call to `model` can take (see [`Ctx::learn`]): its context
    /// window less [`Ctx::output_of`]. `MOLT_CONTEXT_TOKENS` wins, then what
    /// the gateway's configuration declares for that model, then a guess
    /// from its name.
    pub fn limits_of(&self, model: &Option<String>) -> Limits {
        let context = self
            .cfg
            .context_tokens
            .or(self.known(model).context)
            .unwrap_or_else(|| Limits::context_of(model.as_deref()));
        Limits::new(context, self.output_of(model), self.cfg.frame_bytes)
    }

    /// Tell the run once when `model`'s window is only guessed, and not from
    /// a name it knows: what is assumed, and how to say otherwise.
    pub async fn note_guess(&self, model: &Option<String>) {
        if self.cfg.context_tokens.is_some() || !Limits::guessed(model.as_deref()) {
            return;
        }
        {
            let mut models = self.models.lock().unwrap_or_else(PoisonError::into_inner);
            let known = models.entry(model.clone()).or_default();
            if known.context.is_some() || std::mem::replace(&mut known.guess_noted, true) {
                return;
            }
        }
        let name = model.as_deref().unwrap_or("the default model");
        let assumed = Limits::context_of(model.as_deref());
        tracing::info!(run = %self.trace, model = name, assumed, "the model's context window is not declared");
        self.note(format!(
            "{name}: its context window is not declared, so its conversations are fitted to {assumed} tokens; declare \
             `context` for it where its endpoint is configured, or set MOLT_CONTEXT_TOKENS"
        ))
        .await;
    }

    /// What `model`'s reply says its window is: it stands from now on. When
    /// it differs from what the call was fitted to, the run is told once.
    async fn reported(&self, model: &Option<String>, context: u32) {
        let differs = {
            let mut models = self.models.lock().unwrap_or_else(PoisonError::into_inner);
            let known = models.entry(model.clone()).or_default();
            let used = self.cfg.context_tokens.or(known.context);
            known.context = Some(context);
            match used.filter(|&used| used != context) {
                Some(used) if !std::mem::replace(&mut known.differs_noted, true) => Some(used),
                _ => None,
            }
        };
        let Some(used) = differs else { return };
        let name = model.as_deref().unwrap_or("the default model");
        let message = if self.cfg.context_tokens.is_some() {
            format!(
                "{name}: MOLT_CONTEXT_TOKENS sets a {used}-token context window, but the gateway declares {context}; \
                 {used} is used"
            )
        } else {
            format!("{name}: the gateway reported a {context}-token context window, not {used}; {context} is used from now on")
        };
        tracing::warn!(run = %self.trace, model = name, used, reported = context, "{message}");
        self.note(message).await;
    }

    /// What the first message of every conversation starts with: the
    /// project's instruction files, what memory knows, the skills' names,
    /// then the tasks before this one in the user's conversation. The same in
    /// every conversation of the run, so the prompt cache can share it.
    pub fn preamble(&self) -> Vec<&str> {
        self.instructions
            .iter()
            .chain(&self.referenced)
            .chain(&self.memory.context)
            .chain(&self.skills.block)
            .chain(&self.earlier)
            .map(String::as_str)
            .collect()
    }

    pub fn scoped(&self, cancel: CancellationToken) -> Arc<Self> {
        let mut scoped = self.clone();
        scoped.cancel = cancel;
        Arc::new(scoped)
    }

    /// The context of an attempt an adaptive run starts: on `model` when
    /// it names one, which then counts as the run's model, reporting to
    /// `signals`, and told `prior`.
    pub fn attempt_scope(
        &self,
        cancel: CancellationToken,
        model: Option<Option<String>>,
        signals: tokio::sync::mpsc::UnboundedSender<crate::schedule::Signal>,
        prior: Option<Arc<str>>,
    ) -> Arc<Self> {
        let mut scoped = self.clone();
        scoped.cancel = cancel;
        if let Some(model) = model.filter(|m| *m != self.model) {
            scoped.model = model;
        }
        scoped.signals = Some(signals);
        scoped.prior = prior;
        Arc::new(scoped)
    }

    /// The run's budget as it stands: what is known spent, what is held back
    /// for the calls in flight at the most one call has cost, and the calls
    /// whose cost is unknown, which leave what is left unknown.
    pub fn budget_state(&self) -> molt_api::planner::BudgetState {
        let t = self.tally();
        let unknown = t
            .unsettled
            .saturating_add(t.abandoned)
            .saturating_add(t.spend.unknown_calls)
            .saturating_add(self.carried_uncounted);
        let reserved = f64::from(t.pending) * t.spend.max_call_usd;
        molt_api::planner::BudgetState {
            limit_usd: self.budget_usd,
            spent_usd: t.spend.cost_usd,
            reserved_usd: reserved,
            in_flight_calls: t.pending,
            unknown_calls: unknown,
            remaining_usd: (unknown == 0).then(|| (self.budget_usd - t.spend.cost_usd - reserved).max(0.0)),
        }
    }

    pub fn run_id(&self) -> String {
        self.trace.to_string()
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        target: &str,
        payload: impl Serialize,
        budget: Budget,
    ) -> Result<T, RemoteError> {
        let payload = serde_json::to_value(payload).map_err(|e| RemoteError {
            code: ErrorCode::Invalid,
            message: format!("encoding a {target} request: {e}"),
        })?;
        // Cleanup and fork bookkeeping must finish even after cancellation.
        let cleanup = matches!(target, fs::FORK | fs::DROP | fs::DIFF | molt_api::shell::STOP);
        if !cleanup && self.cancel.is_cancelled() {
            return Err(RemoteError {
                code: ErrorCode::Cancelled,
                message: "attempt stopped before starting a new action".into(),
            });
        }
        // A provided tool may take as long as a command, and stops with the attempt as one does.
        let waits =
            matches!(target, model::COMPLETE | molt_api::shell::RUN | molt_api::shell::START | molt_api::shell::READ);
        let reply = if waits || self.tools.is_provided_target(target) {
            self.bus.call_cancellable(target, payload, budget, &self.trace, &self.cancel).await?
        } else {
            self.bus.call(target, payload, budget, &self.trace).await?
        };
        // CompleteResponse's legacy serde defaults erase absent usage. Do
        // this check before decoding so neither front end counts such a call
        // as free or treats an invented zero-token answer as settled.
        if target == model::COMPLETE
            && !reply.get("usage").is_some_and(|usage| {
                usage.get("input_tokens").and_then(Value::as_u64).is_some()
                    && usage.get("output_tokens").and_then(Value::as_u64).is_some()
            })
        {
            return Err(RemoteError {
                code: ErrorCode::Failed,
                message: "model reply has no complete usage; settlement is unknown".into(),
            });
        }
        serde_json::from_value(reply)
            .map_err(|e| RemoteError { code: ErrorCode::Failed, message: format!("bad {target} reply: {e}") })
    }

    pub async fn progress(&self, event: Progress) {
        match serde_json::to_value(&event) {
            Ok(v) => self.bus.publish(progress::TOPIC, v).await,
            Err(e) => tracing::debug!(error = %e, "could not encode a progress event"),
        }
    }

    pub async fn note(&self, message: impl Into<String>) {
        self.progress(Progress::Note { run: self.run_id(), message: message.into() }).await;
    }

    /// Run totals over the designer and every attempt, and the model calls not answered yet.
    pub fn tally(&self) -> Tally {
        *self.tally.borrow()
    }

    /// This context, stopping its model calls once the run's known cost
    /// reaches `cap`. The run's own budget still holds; only the calls made
    /// through the copy are capped, and a call already made runs to its end.
    pub fn capped(&self, cap: f64) -> Arc<Self> {
        let mut capped = self.clone();
        capped.spend_cap = Some(cap);
        Arc::new(capped)
    }

    /// The cap of [`Ctx::capped`] is reached.
    pub fn over_cap(&self) -> bool {
        self.spend_cap.is_some_and(|cap| self.tally().spend.cost_usd >= cap)
    }

    pub fn over_budget(&self) -> bool {
        {
            let t = self.tally();
            t.spend.cost_usd >= self.budget_usd || t.spend.unknown_calls > 0 || t.unsettled > 0
        }
    }

    /// [`Ctx::tally`] once every model call is answered, or after `limit`.
    pub async fn settle(&self, limit: Duration) -> Tally {
        let mut tally = self.tally.subscribe();
        // A timeout leaves the calls still pending in the tally.
        let _ = tokio::time::timeout(limit, tally.wait_for(|t| t.pending == 0)).await;
        self.tally()
    }

    /// One model call; its usage counts against the run. The call runs in its
    /// own task: the gateway bills it whether or not the caller still waits,
    /// so a caller that stops waiting leaves it pending until the reply lands.
    pub async fn complete(self: &Arc<Self>, conv: &Conversation, turn: u32) -> Result<CompleteResponse, RemoteError> {
        let model = self.model_of(conv);
        self.learn(&model).await;
        let max_tokens = self.output_of(&model);
        let req = CompleteRequest {
            model: model.clone(),
            system: Some(conv.system.to_string()),
            messages: conv.messages.clone(),
            tools: conv.tools.as_ref().clone(),
            max_tokens: Some(max_tokens),
            effort: conv.effort.or(self.effort),
            output_schema: None,
            stream: self.stream.then_some(model::StreamContext { attempt: conv.attempt, turn }),
            role: Some(conv.role),
            prompt: conv.prompt.clone(),
            attempt: conv.attempt,
        };
        if let Some(prompt) = &conv.prompt {
            let mut spoke = self.spoke.lock().unwrap_or_else(PoisonError::into_inner);
            if !spoke.contains(prompt) {
                spoke.push(prompt.clone());
            }
        }
        let ms = u64::try_from(self.cfg.model_timeout.as_millis()).unwrap_or(u64::MAX);
        let budget = Budget::new(max_tokens.into(), ms, 0);
        self.tally.send_modify(|t| t.pending += 1);
        self.save(|s| s.open_calls += 1);
        let busy = self.control.busy(format!("{}: a model call", conv.speaker()));
        let ctx = self.clone();
        let call = tokio::spawn(async move {
            let resp = ctx.call::<CompleteResponse>(model::COMPLETE, req, budget).await;
            drop(busy);
            ctx.tally.send_modify(|t| {
                t.pending -= 1;
                match &resp {
                    Ok(resp) => t.spend.add(resp),
                    // Refused before anything was billed (see model::COMPLETE): settled, at no cost.
                    Err(e) if refused(e) => {}
                    Err(_) if ctx.cancel.is_cancelled() && !ctx.run_cancel.is_cancelled() => t.abandoned += 1,
                    Err(_) => t.unsettled += 1,
                }
            });
            let spend = ctx.saved_spend();
            ctx.save(|s| {
                s.open_calls = s.open_calls.saturating_sub(1);
                s.spend = spend;
            });
            if ctx.over_budget() && !ctx.cancel.is_cancelled() {
                ctx.run_cancel.cancel();
            }
            // Each model's own reply bounds the conversations that call it.
            if let Some(context) = resp.as_ref().ok().and_then(|r| r.context_tokens).filter(|&n| n > 0) {
                ctx.reported(&model, context).await;
            }
            resp
        });
        call.await.unwrap_or_else(|e| {
            Err(RemoteError { code: ErrorCode::Failed, message: format!("the model call's task failed: {e}") })
        })
    }

    pub async fn fork(&self) -> Result<String, RemoteError> {
        self.fork_from(self.base.as_deref().unwrap_or(&self.workspace)).await
    }

    /// Copy a candidate for checks that can write build artifacts or alter files.
    pub async fn fork_from(&self, workspace: &str) -> Result<String, RemoteError> {
        let req = fs::ForkRequest { workspace: workspace.to_owned() };
        let resp: fs::ForkResponse = self.call(fs::FORK, req, Budget::new(0, TREE_MS, 0)).await?;
        self.forks.lock().unwrap_or_else(PoisonError::into_inner).push(resp.fork.clone());
        Ok(resp.fork)
    }

    /// Best effort: a fork that cannot be dropped is logged and left behind.
    pub async fn drop_fork(&self, fork: &str) {
        self.forget_fork(fork);
        let req = fs::DropRequest { fork: fork.to_owned() };
        if let Err(e) = self.call::<fs::DropResponse>(fs::DROP, req, Budget::new(0, TREE_MS, 0)).await {
            tracing::warn!(fork, error = %e, "could not drop a fork");
        }
    }

    /// Drop every fork still alive except `keep`.
    pub async fn drop_forks_except(&self, keep: Option<&str>) {
        let alive = self.forks.lock().unwrap_or_else(PoisonError::into_inner).clone();
        for fork in alive.iter().filter(|f| Some(f.as_str()) != keep) {
            self.drop_fork(fork).await;
        }
    }

    /// A fork made by an earlier resume of the run: the run owns it now.
    pub fn track_fork(&self, fork: &str) {
        let mut forks = self.forks.lock().unwrap_or_else(PoisonError::into_inner);
        if !forks.iter().any(|f| f == fork) {
            forks.push(fork.to_owned());
        }
    }

    /// Every fork still alive is the caller's now: the run drops none of them.
    pub fn hand_over_forks(&self) {
        let forks = std::mem::take(&mut *self.forks.lock().unwrap_or_else(PoisonError::into_inner));
        if !forks.is_empty() {
            tracing::info!(run = %self.trace, forks = ?forks, "the run was stopped; its forks are left to the caller");
        }
    }

    /// The fork is gone or handed to the caller; the run no longer owns it.
    pub fn forget_fork(&self, fork: &str) {
        self.forks.lock().unwrap_or_else(PoisonError::into_inner).retain(|f| f != fork);
    }

    pub async fn diff(&self, fork: &str) -> Result<fs::DiffResponse, RemoteError> {
        self.call(fs::DIFF, fs::DiffRequest { fork: fork.to_owned(), paths: Vec::new() }, Budget::new(0, TREE_MS, 0))
            .await
    }

    /// Merge a fork into the workspace, then drop it. The drop is a separate
    /// best-effort call, so a fork that cannot be deleted does not make a
    /// merge that succeeded look failed.
    pub async fn merge(&self, fork: &str) -> Result<fs::MergeResponse, RemoteError> {
        let req = fs::MergeRequest { fork: fork.to_owned(), drop: false, paths: Vec::new() };
        let resp = self.call(fs::MERGE, req, Budget::new(0, TREE_MS, 0)).await?;
        self.drop_fork(fork).await;
        Ok(resp)
    }

    /// Write a file of the run's own, such as a check file, which is never checked for syntax.
    pub async fn write(&self, workspace: &str, path: &str, content: &str) -> Result<fs::WriteResponse, RemoteError> {
        self.write_checked(workspace, path, content, fs::SyntaxCheck::Off).await
    }

    pub async fn write_checked(
        &self,
        workspace: &str,
        path: &str,
        content: &str,
        syntax: fs::SyntaxCheck,
    ) -> Result<fs::WriteResponse, RemoteError> {
        let req = fs::WriteRequest {
            workspace: workspace.to_owned(),
            path: path.to_owned(),
            content: content.to_owned(),
            syntax,
        };
        self.call(fs::WRITE, req, Budget::new(0, FILE_MS, 0)).await
    }

    pub async fn read(
        &self,
        workspace: &str,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<fs::ReadResponse, RemoteError> {
        let req = fs::ReadRequest { workspace: workspace.to_owned(), path: path.to_owned(), offset, limit };
        self.call(fs::READ, req, Budget::new(0, FILE_MS, 0)).await
    }

    /// A whole file, paging through reads the service cuts short.
    pub async fn read_all(&self, workspace: &str, path: &str) -> Result<String, RemoteError> {
        let mut content = String::new();
        let mut offset = 1;
        loop {
            let page = self.read(workspace, path, Some(offset), None).await?;
            // More lines follow a cut page, so its last line ends in a newline unless the service cut it.
            if page.truncated && !page.content.is_empty() && !page.content.ends_with('\n') {
                let message = format!("{path} has a line too long to read whole");
                return Err(RemoteError { code: ErrorCode::Failed, message });
            }
            content.push_str(&page.content);
            if !page.truncated || page.lines == 0 {
                return Ok(content);
            }
            offset = page.first_line + page.lines;
        }
    }
}
