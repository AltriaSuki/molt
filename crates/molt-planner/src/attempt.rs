//! One attempt: an agent loop in its own fork of the workspace, verified by
//! the done-check whenever the model says it is done.

use std::future::Future;
use std::sync::Arc;

use molt_api::model::{tool_result, user_blocks, user_text, STOP_MAX_TOKENS};
use molt_api::planner::{AttemptReport, AttemptStatus};
use molt_api::progress::Progress;
use molt_api::shell;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::checkpoint::{SavedAttempt, SavedEnd};
use crate::ctx::{Conversation, Ctx, Spend};
use crate::prompts;
use crate::tools::{self, Actor};

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
        };
        Self { report, fork: saved.fork.clone(), summary: end.summary.clone() }
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
        };
        Self { report, fork: None, summary: String::new() }
    }
}

struct End {
    status: AttemptStatus,
    note: String,
}

impl End {
    fn new(status: AttemptStatus, note: impl Into<String>) -> Self {
        Self { status, note: note.into() }
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
/// `check` is `None` for an unverified run.
pub(crate) async fn run(
    ctx: Arc<Ctx>,
    index: u32,
    check: Option<Arc<Check>>,
    cancel: CancellationToken,
    resume: Option<SavedAttempt>,
) -> Finished {
    let mut attempt = Attempt {
        ctx: ctx.clone(),
        index,
        check,
        cancel,
        meter: Meter::default(),
        check_runs: 0,
        fork: None,
        memory_tools: ctx.memory.up,
    };
    if let Some(saved) = &resume {
        attempt.meter = Meter {
            turns: saved.turns,
            spend: Spend { usage: saved.usage, cost_usd: saved.cost_usd, unknown_calls: 0 },
        };
        attempt.check_runs = saved.check_runs;
        attempt.fork = saved.fork.clone();
        attempt.memory_tools = saved.memory_tools;
    }
    let (status, note, summary) = match attempt.work(resume).await {
        Ok(summary) => (AttemptStatus::Passed, String::new(), summary),
        Err(end) => (end.status, end.note, String::new()),
    };
    // Stopped with the run, it can go on when the run is resumed.
    let resumable = status == AttemptStatus::Cancelled && ctx.run_cancel.is_cancelled();
    let (meter, check_runs) = (attempt.meter, attempt.check_runs);
    ctx.save(|s| {
        let saved = s.attempt(index);
        saved.turns = meter.turns;
        saved.check_runs = check_runs;
        saved.usage = meter.spend.usage;
        saved.cost_usd = meter.spend.cost_usd;
        if !resumable {
            saved.end = Some(SavedEnd { status, note: note.clone(), summary: summary.clone() });
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
    };
    Finished { report, fork: attempt.fork, summary }
}

struct Attempt {
    ctx: Arc<Ctx>,
    index: u32,
    check: Option<Arc<Check>>,
    cancel: CancellationToken,
    meter: Meter,
    check_runs: u32,
    fork: Option<String>,
    /// It is offered memory's tools.
    memory_tools: bool,
}

impl Attempt {
    /// The final reply once the attempt is done (and, when there is a check, passed it).
    async fn work(&mut self, resume: Option<SavedAttempt>) -> Result<String, End> {
        let ctx = self.ctx.clone();
        let system = if self.check.is_some() { prompts::ATTEMPT_SYSTEM } else { prompts::UNVERIFIED_SYSTEM };
        let tools = Arc::new(prompts::attempt_tools(self.memory_tools));
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
                let mut conv = Conversation::new(system, tools, Value::Null, Some(self.index));
                conv.messages = saved.messages;
                conv.known = saved.known;
                given = self.resume_point(&mut conv).await;
                (fork, conv)
            }
            None => {
                // Not raced against cancellation: a fork created after we stopped waiting would never be dropped.
                let fork = ctx.fork().await;
                // Started once it has a fork, so a caller that stops the run can keep what the attempt did there.
                let started = fork.as_ref().ok().cloned();
                ctx.progress(Progress::AttemptStarted { run: ctx.run_id(), attempt: self.index, fork: started }).await;
                let fork = fork.map_err(|e| End::error(format!("could not fork the workspace: {e}")))?;
                self.fork = Some(fork.clone());
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
                    self.index,
                    &ctx.preamble(),
                );
                (fork, Conversation::new(system, tools, first, Some(self.index)))
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
                    let resp = agent::turn(&ctx, &mut conv, ctx.max_turns, &mut self.meter, &self.cancel).await?;
                    let calls = resp.tool_uses();
                    if !calls.is_empty() {
                        reply.clear();
                        asked = false;
                        // Saved before they run: a run stopped now does not know what they did.
                        self.save(&conv);
                        let actor = Actor::Attempt { index: self.index, turn: self.meter.turns };
                        let results =
                            self.or_cancel(tools::execute(&ctx, &fork, &calls, agent::cut_off(&resp), actor)).await?;
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
            let Some(check) = self.check.clone() else { return Ok(summary) };
            let ran = self.or_cancel(self.run_check(&fork, &check)).await??;
            self.check_runs += 1;
            let passed = ran.success();
            ctx.progress(Progress::CheckRan {
                run: ctx.run_id(),
                attempt: self.index,
                passed,
                exit_code: ran.exit_code,
                tail: if passed { String::new() } else { prompts::output_tail(&ran, PROGRESS_TAIL) },
            })
            .await;
            if passed {
                return Ok(summary);
            }
            if self.check_runs >= ctx.max_check_rounds {
                let note = format!(
                    "the done-check still failed ({}):\n{}",
                    prompts::ending(&ran),
                    prompts::output_tail(&ran, NOTE_TAIL)
                );
                return Err(End::new(AttemptStatus::Failed, note));
            }
            asked = false;
            conv.push(user_text(prompts::check_failed(&check.command, &ran)));
            self.save(&conv);
        }
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
                "attempt {}: the run stopped while {} ran; the model is told their effect is unknown",
                self.index,
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
        let (fork, memory_tools, meter, check_runs) =
            (self.fork.clone(), self.memory_tools, self.meter, self.check_runs);
        self.ctx.save(|s| {
            let saved = s.attempt(self.index);
            saved.fork = fork;
            saved.memory_tools = memory_tools;
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
