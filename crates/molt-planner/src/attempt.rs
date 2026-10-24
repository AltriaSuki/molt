//! One attempt: an agent loop in its own fork of the workspace, verified by
//! the done-check whenever the model says it is done.

use std::future::Future;
use std::sync::Arc;

use molt_api::model::{attempt_name, tool_result, user_blocks, user_text, Role, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{DisputeCheck, DISPUTE_CHECK};
use molt_api::planner::{AttemptReport, AttemptStatus, CheckDispute, Criterion, Finding};
use molt_api::progress::Progress;
use molt_api::shell;
use molt_proto::tool::ToolRole;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::checkpoint::{SavedAttempt, SavedEnd};
use crate::control;
use crate::ctx::{Conversation, Ctx, Spend};
use crate::tools::{self, Actor};
use crate::{judge, prompts};

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
        };
        Self { report, fork: None, summary: String::new(), findings: Vec::new(), conversation: Vec::new() }
    }
}

struct End {
    status: AttemptStatus,
    note: String,
    dispute: Option<CheckDispute>,
}

impl End {
    fn new(status: AttemptStatus, note: impl Into<String>) -> Self {
        Self { status, note: note.into(), dispute: None }
    }

    /// Stopped on its own dispute of the check: not a failure of the attempt.
    fn disputed(dispute: CheckDispute) -> Self {
        let note = format!("disputed the check, which asserts {}, against the task", dispute.assertion);
        Self { status: AttemptStatus::Cancelled, note, dispute: Some(dispute) }
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
            Stop::Model(e) => Self::error(format!("model call failed: {e}")),
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
        version: ctx.control.version(),
        read_only: ctx.workflow.read_only(),
        conversation: Vec::new(),
    };
    if let Some(saved) = &resume {
        attempt.meter = Meter {
            turns: saved.turns,
            spend: Spend { usage: saved.usage, cost_usd: saved.cost_usd, unknown_calls: 0 },
        };
        attempt.check_runs = saved.check_runs;
        attempt.fork = saved.fork.clone();
        attempt.memory_tools = saved.memory_tools;
        attempt.web_tools = saved.web_tools;
        attempt.version = saved.requirements;
    }
    attempt.check = attempt.check_at(attempt.version);
    let (status, note, summary, dispute) = match attempt.work(resume).await {
        Ok(summary) => (AttemptStatus::Passed, String::new(), summary, None),
        Err(end) => (end.status, end.note, String::new(), end.dispute),
    };
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
    /// The version of the run's requirements it works to.
    version: u32,
    /// It works in the workspace itself with tools that only read.
    read_only: bool,
    /// Its conversation once it passed, fitted to go back over the bus.
    conversation: Vec<Value>,
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
        let system = match (&self.check, self.rubric.is_empty()) {
            _ if self.read_only => prompts::ASK_SYSTEM,
            (Some(_), _) => prompts::ATTEMPT_SYSTEM,
            (None, false) => prompts::RUBRIC_SYSTEM,
            (None, true) => prompts::UNVERIFIED_SYSTEM,
        };
        let tools = Arc::new(match self.check {
            _ if self.read_only => prompts::ask_tools(&ctx.tools, self.memory_tools, self.web_tools),
            Some(_) => prompts::checked_attempt_tools(&ctx.tools, self.memory_tools, self.web_tools),
            None => prompts::attempt_tools(&ctx.tools, self.memory_tools, self.web_tools),
        });
        // A read-only `ask` attempt may only read, so it is not offered the question either.
        let tools = if ctx.interactive && !self.read_only { Arc::new(prompts::with_ask_user(&tools)) } else { tools };
        // A final reply the model gave before the run stopped, still to be checked.
        let mut given = None;
        let (fork, mut conv) = match resume {
            Some(saved) => {
                let fork = saved.fork.clone().ok_or_else(|| End::error("it stopped before it had a fork"))?;
                // The fork must still be there, as the run left it.
                ctx.diff(&fork).await.map_err(|e| End::error(format!("its fork {fork} cannot be used: {e}")))?;
                ctx.track_fork(&fork);
                let started = Some(fork.clone());
                ctx.progress(Progress::AttemptStarted { run: ctx.run_id(), attempt: self.index, fork: started }).await;
                let mut conv = Conversation::new(system, tools, Value::Null, Role::Attempt, Some(self.index));
                conv.messages = saved.messages;
                conv.known = saved.known;
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
                let first = prompts::attempt_first_message(
                    &ctx.task,
                    self.check.as_ref().map(|c| (c.command.as_str(), paths.as_slice())),
                    &self.rubric,
                    self.index,
                    &ctx.preamble(),
                    &ctx.control.upto(self.version),
                );
                let mut conv = Conversation::new(system, tools, first, Role::Attempt, Some(self.index));
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
                    let resp = agent::turn(&ctx, &mut conv, ctx.max_turns, &mut self.meter, &self.cancel).await?;
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
                        let results = self.or_cancel(self.execute(&fork, &calls, cut_off, actor)).await?;
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
            if self.check_runs >= ctx.max_check_rounds {
                let note = match refused {
                    Some(reason) => format!("the done-check passed, but a check.after hook refused it: {reason}"),
                    None => format!(
                        "the done-check still failed ({}):\n{}",
                        prompts::ending(&ran),
                        prompts::output_tail(&ran, NOTE_TAIL)
                    ),
                };
                return Err(End::new(AttemptStatus::Failed, note));
            }
            asked = false;
            let feedback = match refused {
                Some(reason) => format!(
                    "The done-check `{}` passed, but the user's check.after hook did not accept the result: \
                     {reason}\n\nFix that, then reply when you are done.",
                    check.command
                ),
                None => prompts::check_failed(&check.command, &ran),
            };
            conv.push(user_text(format!("{feedback}{}", hooked.context_text(crate::hooks::Event::CheckAfter))));
            self.save(&conv);
        }
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
        let mut kept = Conversation::new(conv.system, conv.tools.clone(), Value::Null, conv.role, conv.attempt);
        kept.messages = conv.messages.clone();
        kept.known = conv.known;
        let limits = self.ctx.limits();
        let bound = crate::fit::Limits { tokens: limits.tokens, bytes: self.ctx.cfg.frame_bytes / 4 };
        if let Err(why) = kept.fit(bound) {
            tracing::warn!(run = %self.ctx.trace, attempt = self.index, %why, "its conversation is not kept");
            return;
        }
        self.conversation = kept.messages;
    }

    /// Results for a turn's calls, in order. A dispute_check that reaches
    /// here was not well formed, and gets an error saying why. An attempt
    /// that only reads runs nothing else: the calls are refused unrun.
    async fn execute(&self, fork: &str, calls: &[ToolUse], cut_off: Option<&str>, actor: Actor) -> Vec<Value> {
        let is_dispute = |c: &ToolUse| self.check.is_some() && c.name == DISPUTE_CHECK;
        let refused = |c: &ToolUse| self.read_only && !self.ctx.tools.offers(ToolRole::Attempt, true, &c.name);
        let others: Vec<ToolUse> = calls.iter().filter(|c| !is_dispute(c) && !refused(c)).cloned().collect();
        let mut results = tools::execute(&self.ctx, fork, &others, cut_off, actor).await.into_iter();
        calls
            .iter()
            .map(|call| {
                if refused(call) {
                    let why = format!("{} is not available: this task only reads, and changes nothing.", call.name);
                    return tool_result(&call.id, why, true);
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
            .collect()
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

    /// Save the attempt as it stands in the run's checkpoint.
    fn save(&self, conv: &Conversation) {
        let (fork, memory_tools, web_tools, meter, check_runs, version) =
            (self.fork.clone(), self.memory_tools, self.web_tools, self.meter, self.check_runs, self.version);
        self.ctx.save(|s| {
            let saved = s.attempt(self.index);
            saved.requirements = version;
            saved.fork = fork;
            saved.memory_tools = memory_tools;
            saved.web_tools = web_tools;
            saved.messages = conv.messages.clone();
            saved.known = conv.known;
            saved.turns = meter.turns;
            saved.check_runs = check_runs;
            saved.usage = meter.spend.usage;
            saved.cost_usd = meter.spend.cost_usd;
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
        tools::run_command(&self.ctx, fork, &check.command, timeout_ms)
            .await
            .map_err(|e| End::error(format!("could not run the done-check: {e}")))
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
