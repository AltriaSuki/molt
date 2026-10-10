//! One attempt: an agent loop in its own fork of the workspace, verified by
//! the done-check whenever the model says it is done.

use std::future::Future;
use std::sync::Arc;

use molt_api::model::{user_blocks, user_text, STOP_MAX_TOKENS};
use molt_api::planner::{AttemptReport, AttemptStatus};
use molt_api::progress::Progress;
use molt_api::shell;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts;
use crate::tools::{self, Actor};

/// Bytes of check output kept in a failed attempt's note.
const NOTE_TAIL: usize = 2000;

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

/// Run attempt `index` to its end. `check` is `None` for an unverified run.
pub(crate) async fn run(ctx: Arc<Ctx>, index: u32, check: Option<Arc<Check>>, cancel: CancellationToken) -> Finished {
    ctx.progress(Progress::AttemptStarted { run: ctx.run_id(), attempt: index }).await;
    let mut attempt =
        Attempt { ctx: ctx.clone(), index, check, cancel, meter: Meter::default(), check_runs: 0, fork: None };
    let (status, note, summary) = match attempt.work().await {
        Ok(summary) => (AttemptStatus::Passed, String::new(), summary),
        Err(end) => (end.status, end.note, String::new()),
    };
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
}

impl Attempt {
    /// The final reply once the attempt is done (and, when there is a check, passed it).
    async fn work(&mut self) -> Result<String, End> {
        let ctx = self.ctx.clone();
        // Not raced against cancellation: a fork created after we stopped waiting would never be dropped.
        let fork = ctx.fork().await.map_err(|e| End::error(format!("could not fork the workspace: {e}")))?;
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
            ctx.memory.context.as_deref(),
        );
        let tools = Arc::new(prompts::attempt_tools(ctx.memory.up));
        let system = if self.check.is_some() { prompts::ATTEMPT_SYSTEM } else { prompts::UNVERIFIED_SYSTEM };
        let mut conv = Conversation::new(system, tools, first, Some(self.index));
        // The final reply so far. The output limit can split it over several turns.
        let mut reply: Vec<String> = Vec::new();
        // An empty final reply gets one request for a real one before it is accepted.
        let mut asked = false;
        loop {
            let resp = agent::turn(&ctx, &mut conv, ctx.max_turns, &mut self.meter, &self.cancel).await?;
            let calls = resp.tool_uses();
            if !calls.is_empty() {
                reply.clear();
                asked = false;
                let actor = Actor::Attempt { index: self.index, turn: self.meter.turns };
                let results = self.or_cancel(tools::execute(&ctx, &fork, &calls, agent::cut_off(&resp), actor)).await?;
                conv.push(user_blocks(results));
                continue;
            }
            let text = resp.text();
            if !text.is_empty() {
                reply.push(text);
            }
            if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
                conv.push(user_text(prompts::CONTINUE));
                continue;
            }
            if reply.is_empty() && !asked {
                asked = true;
                conv.push(user_text(prompts::FINAL_REPLY));
                continue;
            }

            let summary = std::mem::take(&mut reply).join("\n\n");
            let Some(check) = self.check.clone() else { return Ok(summary) };
            let ran = self.or_cancel(self.run_check(&fork, &check)).await??;
            self.check_runs += 1;
            let passed = ran.success();
            ctx.progress(Progress::CheckRan {
                run: ctx.run_id(),
                attempt: self.index,
                passed,
                exit_code: ran.exit_code,
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
        }
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
