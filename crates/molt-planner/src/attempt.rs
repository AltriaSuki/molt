//! One attempt: an agent loop in its own fork of the workspace, verified by
//! the done-check whenever the model says it is done.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use molt_api::model::{attempt_name, tool_result, user_blocks, user_text, Role, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{DisputeCheck, DISPUTE_CHECK, EXPLORE};
use molt_api::planner::{AttemptReport, AttemptStatus, CheckDispute, Criterion, Failure, FailureSource, Finding};
use molt_api::progress::Progress;
use molt_api::shell;
use molt_proto::tool::ToolRole;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::checkpoint::{SavedAttempt, SavedEnd};
use crate::compact::{self, Compacted};
use crate::control;
use crate::ctx::{Conversation, Ctx, Spend};
use crate::schedule::{Signal, TOOL_ERROR_TURNS};
use crate::tools::{self, Actor};
use crate::{explore, failure, git, judge, process, prompts};

/// Bytes of check output kept in a failed attempt's note.
const NOTE_TAIL: usize = 2000;
/// Bytes of a failed check's output in its progress event.
const PROGRESS_TAIL: usize = 400;

/// A done-check as the attempts run it.
pub(crate) struct Check {
    pub command: String,
    /// Workspace-relative path and contents of each file the check depends
    /// on, written into every fork and restored before every check run.
    pub files: Vec<(String, String)>,
    /// The project's own test command, run on passing results to compare
    /// them; attempts never run it.
    pub suite: Option<String>,
}

impl Check {
    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|(path, _)| path.clone()).collect()
    }
}

pub(crate) struct Finished {
    pub report: AttemptReport,
    /// The attempt's fork, if it got one.
    pub fork: Option<String>,
    /// The model's final reply, when it said it was done.
    pub summary: String,
    /// How a passing result did on the run's rubric, when it has one.
    pub findings: Vec<Finding>,
    /// Its conversation with the model, when it passed, for a task that
    /// goes on from it.
    pub conversation: Vec<Value>,
}

impl Finished {
    /// An attempt that ended in an earlier resume of the run.
    pub fn saved(saved: &SavedAttempt, end: &SavedEnd) -> Self {
        let report = AttemptReport {
            index: saved.index,
            status: end.status,
            turns: saved.turns,
            check_runs: saved.check_runs,
            usage: saved.usage,
            cost_usd: saved.cost_usd,
            note: end.note.clone(),
            requirements: saved.requirements,
            dispute: end.dispute.clone(),
            failure: end.failure.clone(),
        };
        Self {
            report,
            fork: saved.fork.clone(),
            summary: end.summary.clone(),
            findings: end.findings.clone(),
            conversation: saved.messages.clone(),
        }
    }

    /// The report for an attempt whose task died without returning one.
    pub fn lost(index: u32, note: String) -> Self {
        let report = AttemptReport {
            index,
            status: AttemptStatus::Error,
            turns: 0,
            check_runs: 0,
            usage: Default::default(),
            cost_usd: 0.0,
            note,
            requirements: 0,
            dispute: None,
            failure: None,
        };
        Self { report, fork: None, summary: String::new(), findings: Vec::new(), conversation: Vec::new() }
    }
}

struct End {
    status: AttemptStatus,
    note: String,
    dispute: Option<CheckDispute>,
    /// What kind of failure it was, when it was one the planner could tell;
    /// boxed, as most ends have none.
    failure: Option<Box<Failure>>,
}

impl End {
    fn new(status: AttemptStatus, note: impl Into<String>) -> Self {
        Self { status, note: note.into(), dispute: None, failure: None }
    }

    fn failed(note: impl Into<String>, failure: Option<Failure>) -> Self {
        Self { failure: failure.map(Box::new), ..Self::new(AttemptStatus::Failed, note) }
    }

    /// Stopped on its own dispute of the check: not a failure of the attempt.
    fn disputed(dispute: CheckDispute) -> Self {
        let note = format!("disputed the check, which asserts {}, against the task", dispute.assertion);
        Self { status: AttemptStatus::Cancelled, note, dispute: Some(dispute), failure: None }
    }

    fn error(note: impl Into<String>) -> Self {
        Self::new(AttemptStatus::Error, note)
    }

    fn cancelled() -> Self {
        Self::new(AttemptStatus::Cancelled, "attempt cancelled")
    }
}

impl From<Stop> for End {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Cancelled => Self::cancelled(),
            Stop::Budget => Self::new(AttemptStatus::Cancelled, "budget exhausted"),
            Stop::OutOfTurns => Self::new(AttemptStatus::Failed, "ran out of turns"),
            Stop::Model(e) => {
                let failure = Some(Box::new(failure::of_model(&e)));
                Self { failure, ..Self::error(format!("model call failed: {e}")) }
            }
            Stop::Refused => Self::error("the model declined"),
        }
    }
}

/// Run attempt `index` to its end, or on from where `resume` left it.
/// `check` is `None` for a run judged by its `rubric`, or, without one, an
/// unverified run.
pub(crate) async fn run(
    ctx: Arc<Ctx>,
    index: u32,
    check: Option<Arc<Check>>,
    rubric: Arc<Vec<Criterion>>,
    cancel: CancellationToken,
    resume: Option<SavedAttempt>,
) -> Finished {
    let mut attempt = Attempt {
        ctx: ctx.clone(),
        index,
        check,
        rubric,
        findings: Vec::new(),
        cancel,
        meter: Meter::default(),
        check_runs: 0,
        fork: None,
        memory_tools: ctx.memory.up,
        web_tools: ctx.web,
        skill_tools: !ctx.skills.is_empty(),
        version: ctx.control.version(),
        read_only: ctx.workflow.read_only(),
        conversation: Vec::new(),
        last_failure: None,
        last_blocker: None,
        failing_turns: 0,
        repeats: HashMap::new(),
    };
    if let Some(saved) = &resume {
        attempt.meter = Meter {
            turns: saved.turns,
            spend: Spend { usage: saved.usage, cost_usd: saved.cost_usd, ..Spend::default() },
        };
        attempt.check_runs = saved.check_runs;
        attempt.fork = saved.fork.clone();
        attempt.memory_tools = saved.memory_tools;
        attempt.web_tools = saved.web_tools;
        attempt.skill_tools = saved.skill_tools;
        attempt.version = saved.requirements;
    }
    attempt.check = attempt.check_at(attempt.version);
    let (status, note, summary, dispute, failure) = match attempt.work(resume).await {
        Ok(summary) => (AttemptStatus::Passed, String::new(), summary, None, None),
        Err(end) => (end.status, end.note, String::new(), end.dispute, end.failure.map(|f| *f)),
    };
    // However it ended, nothing it started runs on.
    if let Some(fork) = attempt.fork.as_deref().filter(|_| attempt.processes()) {
        process::stop_all(&ctx, fork).await;
    }
    // Stopped with the run, it can go on when the run is resumed; a dispute is its own end.
    let resumable = status == AttemptStatus::Cancelled && ctx.run_cancel.is_cancelled() && dispute.is_none();
    let (meter, check_runs, version) = (attempt.meter, attempt.check_runs, attempt.version);
    let findings = if status == AttemptStatus::Passed { std::mem::take(&mut attempt.findings) } else { Vec::new() };
    ctx.save(|s| {
        let saved = s.attempt(index);
        saved.requirements = version;
        saved.turns = meter.turns;
        saved.check_runs = check_runs;
        saved.usage = meter.spend.usage;
        saved.cost_usd = meter.spend.cost_usd;
        if !resumable {
            saved.end = Some(SavedEnd {
                status,
                note: note.clone(),
                summary: summary.clone(),
                findings: findings.clone(),
                dispute: dispute.clone(),
                failure: failure.clone(),
            });
        }
    });
    tracing::info!(run = %ctx.trace, attempt = index, ?status, turns = attempt.meter.turns, %note, "attempt finished");
    ctx.progress(Progress::AttemptFinished { run: ctx.run_id(), attempt: index, status }).await;
    let report = AttemptReport {
        index,
        status,
        turns: attempt.meter.turns,
        check_runs: attempt.check_runs,
        usage: attempt.meter.spend.usage,
        cost_usd: attempt.meter.spend.cost_usd,
        note,
        requirements: attempt.version,
        dispute,
        failure,
    };
    let conversation = if status == AttemptStatus::Passed { attempt.conversation } else { Vec::new() };
    Finished { report, fork: attempt.fork, summary, findings, conversation }
}

struct Attempt {
    ctx: Arc<Ctx>,
    index: u32,
    check: Option<Arc<Check>>,
    /// The criteria a run without a check is judged by; empty for an unverified run.
    rubric: Arc<Vec<Criterion>>,
    /// The judge's findings on the result it last judged.
    findings: Vec<Finding>,
    cancel: CancellationToken,
    meter: Meter,
    check_runs: u32,
    fork: Option<String>,
    /// It is offered memory's tools.
    memory_tools: bool,
    /// It is offered `fetch_url`.
    web_tools: bool,
    /// It is offered `load_skill`.
    skill_tools: bool,
    /// The version of the run's requirements it works to.
    version: u32,
    /// It works in the workspace itself with tools that only read.
    read_only: bool,
    /// Its conversation once it passed, fitted to go back over the bus.
    conversation: Vec<Value>,
    /// The output of its last failed check run, to tell a repeat.
    last_failure: Option<String>,
    /// The evidence of its last check run that failed for a reason no change
    /// to the code can fix (see [`Failure::needs_user`]), to tell a repeat.
    last_blocker: Option<String>,
    /// Turns in a row whose tool calls all failed.
    failing_turns: u32,
    /// How often each failed tool call, with the same input and the same
    /// error, was made: a repeat is told it is one.
    repeats: HashMap<u64, u32>,
}

impl Attempt {
    /// The run's check as of requirements `version`.
    fn check_at(&self, version: u32) -> Option<Arc<Check>> {
        let check = self.check.clone()?;
        match self.ctx.control.check(version) {
            Some(command) if command != check.command => {
                Some(Arc::new(Check { command, files: check.files.clone(), suite: check.suite.clone() }))
            }
            _ => Some(check),
        }
    }

    /// Take up the requirements added since the attempt last looked, if
    /// any: tell the model, and say so. `checked`: its work was found done
    /// under the old ones, and must be checked again.
    async fn adopt(&mut self, fork: &str, conv: &mut Conversation, checked: bool) -> bool {
        let ctx = self.ctx.clone();
        let latest = ctx.control.version();
        if latest <= self.version {
            return false;
        }
        let added = ctx.control.since(self.version);
        let changed = self.changed_protected(fork, latest).await;
        self.version = latest;
        self.check = self.check_at(latest);
        self.stand(conv);
        let mut text = prompts::new_requirements(&added, &changed);
        if checked {
            text.push_str(&format!("\n\n{}", prompts::RECHECK));
        }
        conv.tell(text);
        self.save(conv);
        let turn = self.meter.turns + 1;
        ctx.progress(Progress::Adopted { run: ctx.run_id(), attempt: self.index, version: latest, turn }).await;
        true
    }

    /// The protected paths, as of requirements `version`, that the fork has changed.
    async fn changed_protected(&self, fork: &str, version: u32) -> Vec<String> {
        let protected = self.ctx.control.protected(version);
        if protected.is_empty() {
            return Vec::new();
        }
        match self.ctx.diff(fork).await {
            Ok(diff) => diff
                .changes
                .into_iter()
                .map(|c| c.path)
                .filter(|path| protected.iter().any(|p| control::covered(path, p)))
                .collect(),
            Err(e) => {
                tracing::warn!(run = %self.ctx.trace, attempt = self.index, error = %e, "could not list the fork's changes");
                Vec::new()
            }
        }
    }

    /// Wait while the run is paused; an error when it stopped meanwhile.
    async fn go(&self) -> Result<(), End> {
        if self.ctx.control.wait_go(&self.cancel, &self.ctx.cancel).await {
            Ok(())
        } else {
            Err(if self.ctx.over_budget() { End::from(Stop::Budget) } else { End::cancelled() })
        }
    }

    /// The final reply once the attempt is done (and, when there is a check, passed it).
    async fn work(&mut self, resume: Option<SavedAttempt>) -> Result<String, End> {
        let ctx = self.ctx.clone();
        let seats = ctx.seats();
        let seat = if self.read_only { &seats.explorer } else { &seats.attempt };
        // The pack's text, then what holds for the way this attempt's result is verified.
        let part = match (&self.check, self.rubric.is_empty()) {
            _ if self.read_only => "",
            (Some(_), _) => prompts::CHECK_PART,
            (None, false) => prompts::RUBRIC_PART,
            (None, true) => "",
        };
        let tools = match self.check {
            _ if self.read_only => prompts::ask_tools(&ctx.tools, self.memory_tools, self.web_tools, self.skill_tools),
            Some(_) => prompts::checked_attempt_tools(&ctx.tools, self.memory_tools, self.web_tools, self.skill_tools),
            None => prompts::attempt_tools(&ctx.tools, self.memory_tools, self.web_tools, self.skill_tools),
        };
        // A read-only `ask` attempt may only read, so it is not offered the question either.
        let tools = if ctx.interactive && !self.read_only { prompts::with_ask_user(&tools) } else { tools };
        let max_turns = seat.max_turns.unwrap_or(ctx.max_turns);
        let tools = if self.processes() { process::with_processes(tools) } else { tools };
        let tools = if self.reads_history() { git::with_git(tools) } else { tools };
        let tools = if self.reads_outputs() { crate::output::with_outputs(tools) } else { tools };
        // Last, after ask_user: without it the list is what it was.
        let tools = if self.explores() { explore::with_explore(tools) } else { tools };
        // A final reply the model gave before the run stopped, still to be checked.
        let mut given = None;
        let (fork, mut conv) = match resume {
            Some(saved) => {
                let fork = saved.fork.clone().ok_or_else(|| End::error("it stopped before it had a fork"))?;
                ctx.dir_instructions.restore(&ctx, self.index, saved.dir_instructions.clone());
                // The fork must still be there, as the run left it.
                ctx.diff(&fork).await.map_err(|e| End::error(format!("its fork {fork} cannot be used: {e}")))?;
                ctx.track_fork(&fork);
                let started = Some(fork.clone());
                ctx.progress(Progress::AttemptStarted { run: ctx.run_id(), attempt: self.index, fork: started }).await;
                let mut conv = Conversation::seated(seat, part, tools, Value::Null, Role::Attempt, Some(self.index));
                conv.messages = saved.messages;
                conv.known = saved.known;
                conv.compacted = saved.compacted.map(|c| *c);
                given = self.resume_point(&mut conv).await;
                (fork, conv)
            }
            None => {
                let kept = ctx.continues.as_ref().and_then(|c| c.fork.clone());
                let fork = match (self.read_only, kept) {
                    // It only reads: where the result it goes on from is, which is never its to drop.
                    (true, kept) => Ok(kept.unwrap_or_else(|| ctx.workspace.clone())),
                    // The kept result it goes on from: not tracked, so a run that fails leaves it to the user.
                    (false, Some(kept)) => ctx.diff(&kept).await.map(|_| kept),
                    // Not raced against cancellation: a fork created after we stopped waiting would never be dropped.
                    (false, None) => ctx.fork().await,
                };
                // Started once it has a fork, so a caller that stops the run can keep what the attempt did there.
                let started = fork.as_ref().ok().filter(|_| !self.read_only).cloned();
                ctx.progress(Progress::AttemptStarted { run: ctx.run_id(), attempt: self.index, fork: started }).await;
                let fork = fork.map_err(|e| End::error(format!("could not fork the workspace: {e}")))?;
                if !self.read_only {
                    self.fork = Some(fork.clone());
                }
                if self.cancel.is_cancelled() {
                    return Err(if ctx.over_budget() { End::from(Stop::Budget) } else { End::cancelled() });
                }
                if let Some(check) = &self.check {
                    for (path, content) in &check.files {
                        ctx.write(&fork, path, content)
                            .await
                            .map_err(|e| End::error(format!("could not write the check file {path}: {e}")))?;
                    }
                }
                let paths = self.check.as_ref().map(|c| c.paths()).unwrap_or_default();
                let mut first = prompts::attempt_first_message(
                    &ctx.task,
                    self.check.as_ref().map(|c| (c.command.as_str(), paths.as_slice())),
                    &self.rubric,
                    self.index,
                    &ctx.preamble(),
                    &ctx.control.upto(self.version),
                );
                // What the attempts an adaptive run started before this one did, last: the blocks before stay cached.
                if let (Some(prior), Some(blocks)) = (&ctx.prior, first["content"].as_array_mut()) {
                    blocks.push(molt_api::model::text_block(prior.to_string()));
                }
                let mut conv = Conversation::seated(seat, part, tools, first, Role::Attempt, Some(self.index));
                if let Some(earlier) =
                    ctx.continues.as_ref().map(|c| continued(&c.conversation)).filter(|c| !c.is_empty())
                {
                    // The earlier conversation opens with the project's context: only the task and its check are new.
                    let first = prompts::attempt_first_message(
                        &ctx.task,
                        self.check.as_ref().map(|c| (c.command.as_str(), paths.as_slice())),
                        &self.rubric,
                        self.index,
                        &[],
                        &ctx.control.upto(self.version),
                    );
                    conv.messages = earlier;
                    conv.push(prompts::continued_message(&ctx.task, first));
                }
                (fork, conv)
            }
        };
        conv.recall = seat.allows(compact::EARLIER_RESULT);
        self.stand(&mut conv);
        self.save(&conv);
        // The final reply so far. The output limit can split it over several turns.
        let mut reply: Vec<String> = Vec::new();
        // An empty final reply gets one request for a real one before it is accepted.
        let mut asked = false;
        loop {
            let summary = match given.take() {
                Some(text) => text,
                None => {
                    // A clear step: what the user added since takes effect here.
                    self.adopt(&fork, &mut conv, false).await;
                    let resp = agent::turn(&ctx, &mut conv, max_turns, &mut self.meter, &self.cancel).await?;
                    // The fit may have left out results that gave directory instructions.
                    ctx.dir_instructions.sync(&ctx, self.index, &conv.messages);
                    let calls = resp.tool_uses();
                    if !calls.is_empty() {
                        reply.clear();
                        asked = false;
                        // Saved before they run: a run stopped now does not know what they did.
                        self.save(&conv);
                        self.go().await?;
                        let cut_off = agent::cut_off(&resp);
                        if let Some(dispute) = self.dispute(&calls, cut_off) {
                            ctx.note(format!("{} disputed the check: {}", attempt_name(self.index), dispute.assertion))
                                .await;
                            return Err(End::disputed(dispute));
                        }
                        let actor = Actor::Attempt { index: self.index, turn: self.meter.turns };
                        let compacted = conv.compacted.as_ref();
                        let (results, explored) =
                            self.or_cancel(self.execute(&fork, &calls, cut_off, actor, compacted)).await?;
                        // What its explorer spent is the attempt's too; the run already counts it.
                        self.meter.spend.absorb(&explored);
                        self.count_errors(&results);
                        let results = self.mark_repeats(&calls, results);
                        conv.push(user_blocks(results));
                        self.save(&conv);
                        continue;
                    }
                    let text = resp.text();
                    if !text.is_empty() {
                        reply.push(text);
                    }
                    if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
                        conv.push(user_text(prompts::CONTINUE));
                        self.save(&conv);
                        continue;
                    }
                    if reply.is_empty() && !asked {
                        asked = true;
                        conv.push(user_text(prompts::FINAL_REPLY));
                        self.save(&conv);
                        continue;
                    }
                    // Saved before the check: a run stopped during it runs the check again.
                    self.save(&conv);
                    std::mem::take(&mut reply).join("\n\n")
                }
            };
            // The work is checked as it stands, with nothing it started still changing it.
            if self.processes() {
                let stopped = process::stop_all(&ctx, &fork).await;
                if stopped > 0 {
                    let s = if stopped == 1 { "" } else { "es" };
                    ctx.note(format!(
                        "{}: stopped its {stopped} process{s} at its final reply",
                        attempt_name(self.index)
                    ))
                    .await;
                }
            }
            // Added to while the model worked: it goes over its work first.
            if self.adopt(&fork, &mut conv, true).await {
                asked = false;
                continue;
            }
            self.go().await?;
            let changed = self.changed_protected(&fork, self.version).await;
            if !changed.is_empty() {
                ctx.note(format!(
                    "{}: its result changes {}, which must stay as they are",
                    attempt_name(self.index),
                    changed.join(", ")
                ))
                .await;
                asked = false;
                conv.push(user_text(prompts::protected_changed(&changed)));
                self.save(&conv);
                continue;
            }
            let Some(check) = self.check.clone() else {
                if !self.rubric.is_empty() {
                    let met = self.judged(&fork, &summary).await?;
                    if !met {
                        if self.check_runs >= ctx.max_check_rounds {
                            let unmet: Vec<&str> =
                                self.findings.iter().filter(|f| !f.met).map(|f| f.criterion.as_str()).collect();
                            let note = format!("the judge still found criteria not met: {}", unmet.join("; "));
                            return Err(End::new(AttemptStatus::Failed, note));
                        }
                        asked = false;
                        conv.push(user_text(prompts::criteria_unmet(&self.findings)));
                        self.save(&conv);
                        continue;
                    }
                }
                if ctx.control.claim(self.version) {
                    self.keep(&conv);
                    return Ok(summary);
                }
                self.adopt(&fork, &mut conv, true).await;
                asked = false;
                continue;
            };
            let ran = self.or_cancel(self.run_check(&fork, &check)).await??;
            self.check_runs += 1;
            let passed = ran.success();
            // The user's check.after hooks may refuse a pass, and add to what the attempt is told.
            let hooked = crate::hooks::checked(&ctx, &fork, self.index, &check.command, &ran).await;
            let refused = hooked.denied.as_ref().filter(|_| passed);
            let passed = passed && refused.is_none();
            ctx.progress(Progress::CheckRan {
                run: ctx.run_id(),
                attempt: self.index,
                passed,
                exit_code: ran.exit_code,
                tail: if passed { String::new() } else { prompts::output_tail(&ran, PROGRESS_TAIL) },
                judged: false,
            })
            .await;
            if passed {
                // Only a result found under the latest requirements can be the run's.
                if ctx.control.claim(self.version) {
                    self.keep(&conv);
                    return Ok(summary);
                }
                self.adopt(&fork, &mut conv, true).await;
                asked = false;
                continue;
            }
            let output = prompts::output_tail(&ran, NOTE_TAIL);
            let repeated = self.last_failure.as_ref() == Some(&output);
            self.last_failure = Some(output);
            let failed = if refused.is_some() { None } else { failure::of_command(&ran, FailureSource::Check) };
            let blocker = failed.as_ref().filter(|f| f.needs_user()).map(|f| f.evidence.clone());
            // Twice the same thing the code cannot change: a third run would meet it again.
            let blocked = blocker.is_some() && blocker == self.last_blocker;
            self.last_blocker = blocker;
            if let Some(f) = failed.as_ref().filter(|_| blocked) {
                ctx.note(format!(
                    "{}: its done-check failed twice for the same reason in the {}, not the code: {}",
                    attempt_name(self.index),
                    f.class.name(),
                    f.evidence
                ))
                .await;
                let note = format!(
                    "the done-check failed the same way twice ({}), for a reason in the {}, not the code: {}\n{}",
                    prompts::ending(&ran),
                    f.class.name(),
                    f.evidence,
                    prompts::output_tail(&ran, NOTE_TAIL)
                );
                return Err(End::failed(note, failed));
            }
            if self.check_runs >= ctx.max_check_rounds {
                let note = match refused {
                    Some(reason) => format!("the done-check passed, but a check.after hook refused it: {reason}"),
                    None => format!(
                        "the done-check still failed ({}):\n{}",
                        prompts::ending(&ran),
                        prompts::output_tail(&ran, NOTE_TAIL)
                    ),
                };
                return Err(End::failed(note, failed));
            }
            self.signal(Signal::CheckFailed { attempt: self.index, repeated });
            asked = false;
            let feedback = match refused {
                Some(reason) => format!(
                    "The done-check `{}` passed, but the user's check.after hook did not accept the result: \
                     {reason}\n\nFix that, then reply when you are done.",
                    check.command
                ),
                None => {
                    let mut feedback = prompts::check_failed(&check.command, &ran);
                    if self.reads_outputs() {
                        feedback.push_str(&crate::output::mention(&ran.outputs, ran.truncated));
                    }
                    if let Some(f) = failed.as_ref().filter(|f| f.needs_user() && !ran.offline_failure) {
                        feedback.push_str(&prompts::blocked(f));
                    }
                    feedback
                }
            };
            conv.push(user_text(format!("{feedback}{}", hooked.context_text(crate::hooks::Event::CheckAfter))));
            self.save(&conv);
        }
    }

    /// Tell an adaptive run's schedule `signal`; a fixed run has no one to tell.
    fn signal(&self, signal: Signal) {
        if let Some(signals) = &self.ctx.signals {
            // The schedule stops listening once the run is decided: nothing to tell then.
            let _ = signals.send(signal);
        }
    }

    /// Count a turn whose tool calls all failed, and tell the schedule when
    /// [`TOOL_ERROR_TURNS`] come in a row.
    fn count_errors(&mut self, results: &[Value]) {
        let failed = !results.is_empty() && results.iter().all(|r| r["is_error"] == true);
        self.failing_turns = if failed { self.failing_turns + 1 } else { 0 };
        if self.failing_turns >= TOOL_ERROR_TURNS {
            self.signal(Signal::ToolErrors { attempt: self.index, turns: self.failing_turns });
            self.failing_turns = 0;
        }
    }

    /// Tell the model when a call failed exactly as the same call did
    /// before: repeating it unchanged will not help.
    fn mark_repeats(&mut self, calls: &[ToolUse], mut results: Vec<Value>) -> Vec<Value> {
        use std::hash::{Hash, Hasher};
        for result in results.iter_mut().filter(|r| r["is_error"] == true) {
            let Some(call) = calls.iter().find(|c| result["tool_use_id"] == c.id.as_str()) else { continue };
            let Some(text) = result["content"].as_str() else { continue };
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            (&call.name, call.input.to_string(), text).hash(&mut hash);
            let seen = self.repeats.entry(hash.finish()).or_default();
            *seen += 1;
            if *seen > 1 {
                result["content"] = Value::String(format!("{text}{}", prompts::repeated_failure(*seen)));
            }
        }
        results
    }

    /// The first well-formed dispute_check among `calls`, when the attempt
    /// runs against a check.
    fn dispute(&self, calls: &[ToolUse], cut_off: Option<&str>) -> Option<CheckDispute> {
        self.check.as_ref()?;
        calls.iter().filter(|c| c.name == DISPUTE_CHECK && cut_off != Some(c.id.as_str())).find_map(|c| {
            let d = disputed(c).ok()?;
            Some(CheckDispute {
                attempt: self.index,
                assertion: d.assertion,
                task_quote: d.task_quote,
                reproduce: d.reproduce,
            })
        })
    }

    /// Keep the conversation of a result that passed, fitted to what a model
    /// call can take and to a quarter of a frame, so a reply and the request
    /// that goes on from it both carry it.
    fn keep(&mut self, conv: &Conversation) {
        let mut kept = Conversation::new(conv.system.clone(), conv.tools.clone(), Value::Null, conv.role, conv.attempt);
        kept.messages = conv.messages.clone();
        kept.known = conv.known;
        kept.compacted = conv.compacted.clone();
        kept.standing = conv.standing.clone();
        let limits = self.ctx.limits_of(&self.ctx.model_of(conv));
        let bound = crate::fit::Limits { tokens: limits.tokens, bytes: self.ctx.cfg.frame_bytes / 4 };
        if let Err(why) = kept.fit(bound) {
            tracing::warn!(run = %self.ctx.trace, attempt = self.index, %why, "its conversation is not kept");
            return;
        }
        self.conversation = kept.messages;
    }

    /// It is offered the process tools: they are on, it changes files, and
    /// its role may have them.
    fn processes(&self) -> bool {
        !self.read_only
            && self.ctx.processes
            && self.ctx.seats().attempt.allows(molt_api::planner::tools::START_PROCESS)
    }

    /// It is offered `git`: it is on and its role may have it. An attempt
    /// that only reads, as an `ask` run's, may read the history too.
    fn reads_history(&self) -> bool {
        let seats = self.ctx.seats();
        let seat = if self.read_only { &seats.explorer } else { &seats.attempt };
        self.ctx.git && seat.allows(molt_api::planner::tools::GIT)
    }

    /// It is offered `read_output`: the planner may read saved outputs, it
    /// runs commands, and its role may have the tool.
    fn reads_outputs(&self) -> bool {
        !self.read_only && self.ctx.outputs && self.ctx.seats().attempt.allows(molt_api::planner::tools::READ_OUTPUT)
    }

    /// It is offered `explore`: its workflow explores, it changes files,
    /// and its role may have the tool.
    fn explores(&self) -> bool {
        !self.read_only && self.ctx.workflow.explores() && self.ctx.seats().attempt.allows(EXPLORE)
    }

    /// Results for a turn's calls, in order, and what its `explore` calls
    /// spent. A dispute_check that reaches here was not well formed, and
    /// gets an error saying why. An attempt that only reads runs nothing
    /// else: the calls are refused unrun. `explore` calls run after the
    /// turn's other calls, one at a time, each with its slice of what is
    /// left.
    async fn execute(
        &self,
        fork: &str,
        calls: &[ToolUse],
        cut_off: Option<&str>,
        actor: Actor,
        compacted: Option<&Compacted>,
    ) -> (Vec<Value>, Spend) {
        let explores = self.explores();
        let is_dispute = |c: &ToolUse| self.check.is_some() && c.name == DISPUTE_CHECK;
        let is_explore = |c: &ToolUse| explores && c.name == EXPLORE;
        // Offered once turns were left out: it reads them back, so it is no tool before.
        let is_earlier = |c: &ToolUse| compacted.is_some() && c.name == compact::EARLIER_RESULT;
        // `git` only reads, so an attempt that only reads may make it when it is offered.
        let reads_history = |c: &ToolUse| c.name == molt_api::planner::tools::GIT && self.reads_history();
        let refused = |c: &ToolUse| {
            !is_earlier(c)
                && !reads_history(c)
                && self.read_only
                && !self.ctx.tools.offers(ToolRole::Attempt, true, &c.name)
        };
        let others: Vec<ToolUse> = calls
            .iter()
            .filter(|c| !is_dispute(c) && !is_explore(c) && !is_earlier(c) && !refused(c))
            .cloned()
            .collect();
        let mut results = tools::execute(&self.ctx, fork, &others, cut_off, actor).await.into_iter();
        let mut spent = Spend::default();
        let mut explored = Vec::new();
        for call in calls.iter().filter(|c| is_explore(c)) {
            if cut_off == Some(call.id.as_str()) {
                explored.push(tool_result(&call.id, prompts::CUT_OFF, true));
                continue;
            }
            let (ctx, index, turn) = (&self.ctx, self.index, self.meter.turns);
            let e = explore::explore(ctx, fork, index, turn, call, self.memory_tools, &self.cancel).await;
            spent.absorb(&e.spend);
            explored.push(tool_result(&call.id, e.text, e.is_error));
        }
        let mut explored = explored.into_iter();
        let mut earlier = Vec::new();
        for call in calls.iter().filter(|c| is_earlier(c)) {
            let (text, is_error) = if cut_off == Some(call.id.as_str()) {
                (prompts::CUT_OFF.to_owned(), true)
            } else {
                let detail = format!("{} {}", compact::EARLIER_RESULT, call.input["id"].as_str().unwrap_or("?"));
                let ctx = &self.ctx;
                let tool = compact::EARLIER_RESULT.to_owned();
                ctx.progress(Progress::ToolCall {
                    run: ctx.run_id(),
                    attempt: self.index,
                    turn: self.meter.turns,
                    tool,
                    detail,
                })
                .await;
                compact::recall(ctx, fork, compacted, call).await
            };
            earlier.push(tool_result(&call.id, text, is_error));
        }
        let mut earlier = earlier.into_iter();
        let results = calls
            .iter()
            .map(|call| {
                if is_earlier(call) {
                    return earlier
                        .next()
                        .unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true));
                }
                if refused(call) {
                    let why = format!("{} is not available: this task only reads, and changes nothing.", call.name);
                    return tool_result(&call.id, why, true);
                }
                if is_explore(call) {
                    return explored
                        .next()
                        .unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true));
                }
                if !is_dispute(call) {
                    return results
                        .next()
                        .unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true));
                }
                let why = if cut_off == Some(call.id.as_str()) {
                    prompts::CUT_OFF.to_owned()
                } else {
                    disputed(call).err().unwrap_or_else(|| "The tool failed unexpectedly.".to_owned())
                };
                tool_result(&call.id, why, true)
            })
            .collect();
        (results, spent)
    }

    /// Have the judge decide the result against the rubric, and say how it
    /// went: whether every criterion is met. Counts as a check run.
    async fn judged(&mut self, fork: &str, summary: &str) -> Result<bool, End> {
        let ctx = self.ctx.clone();
        let judged = judge::judge(&ctx, fork, &self.rubric, summary, self.version, &self.cancel).await;
        let findings = judged.map_err(|why| match why {
            judge::Unjudged::Stopped(stop) => End::from(stop),
            judge::Unjudged::Failed(why) => End::error(format!("the judge did not finish: {why}")),
        })?;
        self.check_runs += 1;
        let met = findings.iter().all(|f| f.met);
        let unmet: Vec<&str> = findings.iter().filter(|f| !f.met).map(|f| f.criterion.as_str()).collect();
        ctx.progress(Progress::CheckRan {
            run: ctx.run_id(),
            attempt: self.index,
            passed: met,
            exit_code: None,
            tail: if met {
                String::new()
            } else {
                format!("{} of {} criteria not met: {}", unmet.len(), findings.len(), unmet.join("; "))
            },
            judged: true,
        })
        .await;
        self.findings = findings;
        Ok(met)
    }

    /// Where a resumed conversation goes on. One that ends on the model's
    /// tool calls stopped while they ran: whether they took effect is not
    /// known, so the model is told so rather than having them run again. One
    /// that ends on the model's final reply stopped before or during its
    /// check: that reply is returned, to be checked.
    async fn resume_point(&self, conv: &mut Conversation) -> Option<String> {
        let last = conv.messages.last()?;
        if last["role"] != "assistant" {
            return None;
        }
        let blocks = last["content"].as_array().cloned().unwrap_or_default();
        let calls: Vec<&Value> = blocks.iter().filter(|b| b["type"] == "tool_use").collect();
        if calls.is_empty() {
            let text: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
            return Some(text.join("\n\n"));
        }
        let names: Vec<&str> = calls.iter().filter_map(|c| c["name"].as_str()).collect();
        self.ctx
            .note(format!(
                "{}: the run stopped while {} ran; the model is told their effect is unknown",
                attempt_name(self.index),
                names.join(", ")
            ))
            .await;
        let results = calls
            .iter()
            .map(|c| tool_result(c["id"].as_str().unwrap_or_default(), prompts::INTERRUPTED, true))
            .collect();
        conv.push(user_blocks(results));
        None
    }

    /// What holds now for the conversation's record of left-out turns, from
    /// the run's own state: what the user added up to the attempt's
    /// version, the protected paths and the done-check.
    fn stand(&self, conv: &mut Conversation) {
        let added = self.ctx.control.upto(self.version);
        let protected = self.ctx.control.protected(self.version);
        conv.standing =
            Some(prompts::standing(self.version, &added, &protected, self.check.as_ref().map(|c| c.command.as_str())));
    }

    /// Save the attempt as it stands in the run's checkpoint.
    fn save(&self, conv: &Conversation) {
        let (fork, memory_tools, web_tools, skill_tools, meter, check_runs, version) = (
            self.fork.clone(),
            self.memory_tools,
            self.web_tools,
            self.skill_tools,
            self.meter,
            self.check_runs,
            self.version,
        );
        self.ctx.save(|s| {
            let saved = s.attempt(self.index);
            saved.requirements = version;
            saved.fork = fork;
            saved.memory_tools = memory_tools;
            saved.web_tools = web_tools;
            saved.skill_tools = skill_tools;
            saved.messages = conv.messages.clone();
            saved.known = conv.known;
            saved.compacted = conv.compacted.clone().map(Box::new);
            saved.turns = meter.turns;
            saved.check_runs = check_runs;
            saved.usage = meter.spend.usage;
            saved.cost_usd = meter.spend.cost_usd;
            saved.dir_instructions = self.ctx.dir_instructions.saved(self.index);
        });
    }

    /// Restore the check files, so edits to them cannot weaken the check, and run it.
    async fn run_check(&self, fork: &str, check: &Check) -> Result<shell::RunResponse, End> {
        for (path, content) in &check.files {
            self.ctx
                .write(fork, path, content)
                .await
                .map_err(|e| End::error(format!("could not restore the check file {path}: {e}")))?;
        }
        let timeout_ms = u64::try_from(self.ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
        tools::run_command(&self.ctx, fork, &check.command, timeout_ms).await.map_err(|e| {
            // A check the host could not start says which resource it lacked, not that the code failed (#230).
            let failure = failure::of_shell(&e, FailureSource::Check).map(Box::new);
            End { failure, ..End::error(format!("could not run the done-check: {e}")) }
        })
    }

    async fn or_cancel<T>(&self, work: impl Future<Output = T>) -> Result<T, End> {
        tokio::pin!(work);
        tokio::select! {
            biased;
            out = &mut work => Ok(out),
            _ = self.cancel.cancelled() => {
                // The context's token propagates to the shell. Wait for its
                // cleanup reply before the race drops this attempt's fork.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut work).await;
                Err(if self.ctx.over_budget() { End::from(Stop::Budget) } else { End::cancelled() })
            }
        }
    }
}

/// The conversation a task goes on from, as far as it can go on: without a
/// last turn of tool calls that have no results.
fn continued(conversation: &[Value]) -> Vec<Value> {
    let mut messages = conversation.to_vec();
    let calls = |m: &Value| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use"));
    if messages.last().is_some_and(|m| m["role"] == "assistant" && calls(m)) {
        messages.pop();
    }
    if messages.first().is_some_and(|m| m["role"] != "user") {
        return Vec::new();
    }
    messages
}

/// A dispute_check call's input, trimmed, or what is wrong with it.
fn disputed(call: &ToolUse) -> Result<DisputeCheck, String> {
    let d: DisputeCheck = serde_json::from_value(call.input.clone())
        .map_err(|e| format!("Invalid input for dispute_check: {e}. Check the tool's input schema and try again."))?;
    let d = DisputeCheck {
        assertion: d.assertion.trim().to_owned(),
        task_quote: d.task_quote.trim().to_owned(),
        reproduce: d.reproduce.trim().to_owned(),
    };
    let missing = [("assertion", &d.assertion), ("task_quote", &d.task_quote), ("reproduce", &d.reproduce)]
        .into_iter()
        .filter(|(_, v)| v.is_empty())
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "dispute_check needs {}: name the check's assertion, quote the task's words it contradicts, and give a \
             command that shows it.",
            missing.join(", ")
        ));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_dispute_needs_an_assertion_a_quote_and_a_command() {
        let call = |input| ToolUse { id: "toolu_1".into(), name: DISPUTE_CHECK.into(), input };
        let ok = disputed(&call(json!({ "assertion": " col 20 ", "task_quote": "q", "reproduce": "cargo test" })));
        assert_eq!(ok.unwrap().assertion, "col 20");
        let err = disputed(&call(json!({ "assertion": "a", "task_quote": " ", "reproduce": "" }))).unwrap_err();
        assert!(err.starts_with("dispute_check needs task_quote, reproduce:"), "{err}");
        assert!(disputed(&call(json!({ "assertion": "a" }))).unwrap_err().starts_with("Invalid input"));
    }
}
