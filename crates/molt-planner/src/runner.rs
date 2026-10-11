//! One run from request to response: settle the check, race the attempts,
//! apply the winner.

use std::collections::HashMap;
use std::sync::Arc;

use molt_api::planner::{
    AttemptStatus, CheckFile, CheckSpec, DesignResponse, Outcome, Resume, RunRequest, RunResponse, Verdict,
};
use molt_api::progress::Progress;
use molt_proto::{ErrorCode, RemoteError, TraceId};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::attempt::{self, Check, Finished};
use crate::checkpoint::{self, Saved, SavedAttempt, SavedCheck, SavedEnd, SavedSpend, Store};
use crate::ctx::Ctx;
use crate::designer::{self, Design};
use crate::review::{self, Winner};
use crate::{instructions, memory};
use crate::{Bus, Config};

const MAX_ATTEMPTS: u32 = 8;

pub(crate) async fn run(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<RunResponse, RemoteError> {
    run_cancellable(bus, cfg, req, trace, CancellationToken::new()).await
}

pub(crate) async fn run_cancellable(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
    cancel: CancellationToken,
) -> Result<RunResponse, RemoteError> {
    if let Some(resume) = req.resume.clone() {
        return resume_run(bus, cfg, req, resume, trace, cancel).await;
    }
    validate(&req)?;
    let state = match (req.checkpoint, &cfg.state_dir) {
        (false, _) => None,
        (true, Some(dir)) => Some(dir.clone()),
        (true, None) => return Err(invalid("this planner keeps no checkpoints: it was started without a state dir")),
    };
    let mut ctx = Ctx::new(bus, cfg, trace, &req);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel;
    ctx.memory = memory::prepare(&ctx).await;
    if let Some(dir) = &state {
        let store = Store::create(dir, Saved::new(ctx.trace.as_str(), req.clone())).map_err(failed)?;
        ctx.checkpoint = Some(Arc::new(store));
    }
    ctx.instructions = instructions::gather(&ctx).await;
    let ctx = Arc::new(ctx);
    if let Some(dir) = &state {
        prune(&ctx, dir).await;
    }
    tracing::info!(run = %ctx.trace, workspace = %ctx.workspace, attempts = req.attempts, "run started");

    // A check with files came from planner.design.
    let designed = req.verify && (req.check.is_none() || !req.check_files.is_empty());
    let check = match settle_check(&ctx, &req).await? {
        Ok(check) => check,
        Err(reason) => return Ok(finish_early(&ctx, not_designed(&ctx, &reason))),
    };
    save_check(&ctx, check.as_ref(), designed);
    // Without a check there is nothing to pick a winner by, so one attempt is enough.
    let count = if check.is_some() { req.attempts } else { 1 };
    let slots = (0..count).map(|index| Slot::Run { index, saved: None }).collect();
    Ok(finish(&ctx, &req, check, designed, slots).await)
}

/// Go on with the checkpointed run `resume.run`.
async fn resume_run(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    resume: Resume,
    trace: TraceId,
    cancel: CancellationToken,
) -> Result<RunResponse, RemoteError> {
    let dir = cfg.state_dir.clone().ok_or_else(|| invalid("this planner keeps no checkpoints to resume"))?;
    let saved = checkpoint::load(&dir, &resume.run).map_err(invalid)?;
    if req.workspace != saved.request.workspace {
        let message = format!("run {} worked on {}, not {}", resume.run, saved.request.workspace, req.workspace);
        return Err(invalid(message));
    }
    let mut run_req = saved.request.clone();
    run_req.apply = req.apply;
    run_req.stream = req.stream;
    run_req.budget_usd = req.budget_usd.or(run_req.budget_usd);
    run_req.checkpoint = true;
    run_req.resume = None;
    validate(&run_req)?;

    let mut ctx = Ctx::new(bus, cfg, trace, &run_req);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel;
    ctx.memory = memory::prepare(&ctx).await;
    ctx.instructions = instructions::gather(&ctx).await;
    ctx.restore(&saved.spend, saved.open_calls);
    let mut next = saved.clone();
    next.request = run_req.clone();
    next.spend.uncounted_calls = ctx.carried_uncounted;
    next.open_calls = 0;
    next.planner = env!("CARGO_PKG_VERSION").to_owned();
    ctx.checkpoint = Some(Arc::new(Store::create(&dir, next).map_err(failed)?));
    let ctx = Arc::new(ctx);
    ctx.note(format!(
        "resuming run {}: ${:.2} spent before{}",
        resume.run,
        saved.spend.cost_usd,
        match saved.open_calls {
            0 => String::new(),
            n => format!("; {n} model calls were unanswered when it stopped, and their cost is not counted"),
        }
    ))
    .await;
    if saved.planner != env!("CARGO_PKG_VERSION") {
        ctx.note(format!("the checkpoint was saved by planner {}", saved.planner)).await;
    }

    let (check, designed) = if saved.settled {
        let check = saved.check.as_ref().map(|c| Check {
            command: c.command.clone(),
            files: c.files.iter().map(|f| (f.path.clone(), f.content.clone())).collect(),
        });
        (check, saved.check.as_ref().is_some_and(|c| c.designed))
    } else {
        // Stopped while the check was being designed: design it again.
        let designed = run_req.verify && (run_req.check.is_none() || !run_req.check_files.is_empty());
        match settle_check(&ctx, &run_req).await? {
            Ok(check) => {
                save_check(&ctx, check.as_ref(), designed);
                (check, designed)
            }
            Err(reason) => return Ok(finish_early(&ctx, not_designed(&ctx, &reason))),
        }
    };

    let count = if check.is_some() { run_req.attempts } else { 1 };
    let passed = saved.attempts.iter().any(|a| a.end.as_ref().is_some_and(|e| e.status == AttemptStatus::Passed));
    let mut slots = Vec::new();
    for attempt in &saved.attempts {
        if let Some(fork) = &attempt.fork {
            ctx.track_fork(fork);
        }
        match &attempt.end {
            Some(end) => slots.push(Slot::Done(Finished::saved(attempt, end))),
            // A winner is in already: nothing else needs to go on.
            None if passed || resume.retry => {
                let why = if passed { "another attempt had passed" } else { "left as it was for a retry" };
                let end = SavedEnd { status: AttemptStatus::Cancelled, note: why.into(), summary: String::new() };
                ctx.save(|s| s.attempt(attempt.index).end = Some(end.clone()));
                slots.push(Slot::Done(Finished::saved(attempt, &end)));
            }
            None => slots.push(Slot::Run { index: attempt.index, saved: Some(attempt.clone()) }),
        }
    }
    let first = saved.attempts.iter().map(|a| a.index + 1).max().unwrap_or(0);
    if resume.retry {
        slots.extend((first..first + count).map(|index| Slot::Run { index, saved: None }));
    } else if saved.attempts.is_empty() {
        // Stopped before any attempt started.
        slots.extend((0..count).map(|index| Slot::Run { index, saved: None }));
    }
    if !passed && !slots.iter().any(|s| matches!(s, Slot::Run { .. })) {
        ctx.note("every attempt of this run had ended; resume with retry to start new ones").await;
    }
    Ok(finish(&ctx, &run_req, check, designed, slots).await)
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Failed, message: message.into() }
}

/// The check the run goes by: given, designed, or none (`Ok(None)`). An
/// `Err` inside says why the designer could not produce one.
async fn settle_check(ctx: &Arc<Ctx>, req: &RunRequest) -> Result<Result<Option<Check>, String>, RemoteError> {
    Ok(Ok(match &req.check {
        Some(command) => Some(Check {
            command: command.clone(),
            files: req.check_files.iter().map(|f| (f.path.clone(), f.content.clone())).collect(),
        }),
        None if !req.verify => {
            ctx.note("running without a done-check, as asked").await;
            None
        }
        None => match designer::design(ctx, false).await? {
            Design::Check { command, files, .. } => Some(Check { command, files }),
            Design::Unverified(reason) => {
                ctx.note(format!("no automated done-check: {reason}")).await;
                None
            }
            Design::Failed(reason) => return Ok(Err(reason)),
        },
    }))
}

fn save_check(ctx: &Ctx, check: Option<&Check>, designed: bool) {
    let saved = check.map(|c| SavedCheck {
        command: c.command.clone(),
        files: c
            .files
            .iter()
            .map(|(path, content)| CheckFile { path: path.clone(), content: content.clone() })
            .collect(),
        designed,
    });
    ctx.save(|s| {
        s.settled = true;
        s.check = saved;
    });
}

/// Remove the checkpoints beyond the newest [`checkpoint::KEEP`], with the
/// forks they kept.
async fn prune(ctx: &Ctx, dir: &std::path::Path) {
    for (path, forks) in checkpoint::beyond(dir, checkpoint::KEEP) {
        for fork in forks {
            ctx.drop_fork(&fork).await;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), error = %e, "could not remove an old checkpoint");
        }
    }
}

/// An attempt of the run: one to run (on from a checkpoint, with `saved`)
/// or one that ended before this resume.
enum Slot {
    Run { index: u32, saved: Option<SavedAttempt> },
    Done(Finished),
}

/// Race the attempts, apply the winner and total up. A checkpointed run
/// stopped before it finished keeps its checkpoint and forks.
async fn finish(
    ctx: &Arc<Ctx>,
    req: &RunRequest,
    check: Option<Check>,
    designed: bool,
    slots: Vec<Slot>,
) -> RunResponse {
    let spec = check.as_ref().map(|c| CheckSpec { command: c.command.clone(), files: c.paths(), designed });
    ctx.progress(Progress::CheckReady {
        run: ctx.run_id(),
        command: spec.as_ref().map(|s| s.command.clone()),
        files: spec.as_ref().map(|s| s.files.clone()).unwrap_or_default(),
        designed,
    })
    .await;
    let (finished, winner) = race(ctx, slots, check.map(Arc::new)).await;
    let stopped = ctx.checkpoint.is_some() && ctx.run_cancel.is_cancelled();
    // Cancelled attempts may have left model calls running, which are billed: give their replies a moment
    // to land and be counted, while the winner is applied.
    let (mut resp, tally) =
        tokio::join!(conclude(ctx, req, spec, finished, winner, stopped), ctx.settle(ctx.cfg.late_reply_wait));
    resp.usage = tally.spend.usage;
    resp.cost_usd = tally.spend.cost_usd;
    resp.uncounted_calls = tally.uncounted_calls() + ctx.carried_uncounted;
    if stopped {
        let spend = SavedSpend { usage: resp.usage, cost_usd: resp.cost_usd, uncounted_calls: resp.uncounted_calls };
        ctx.save(|s| {
            s.spend = spend;
            s.open_calls = 0;
        });
        resp.checkpoint = Some(ctx.run_id());
        resp.summary.push_str(&format!(
            "\n\nThe run stopped before it finished. Its checkpoint is kept: resume run {}.",
            ctx.run_id()
        ));
    } else if let Some(store) = &ctx.checkpoint {
        store.remove();
    }
    tracing::info!(
        run = %ctx.trace,
        outcome = ?resp.outcome,
        cost_usd = resp.cost_usd,
        uncounted_calls = resp.uncounted_calls,
        "run finished"
    );
    resp
}

/// A run that ended before any attempt: a stopped one keeps its checkpoint,
/// to design the check again when resumed; otherwise nothing is left.
fn finish_early(ctx: &Ctx, mut resp: RunResponse) -> RunResponse {
    if let Some(store) = &ctx.checkpoint {
        if ctx.run_cancel.is_cancelled() {
            resp.checkpoint = Some(ctx.run_id());
        } else {
            store.remove();
        }
    }
    resp
}

/// Design the done-check for `req` and run it once on the workspace as it
/// is, without starting any attempt.
pub(crate) async fn design(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<DesignResponse, RemoteError> {
    design_cancellable(bus, cfg, req, trace, CancellationToken::new()).await
}

pub(crate) async fn design_cancellable(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
    cancel: CancellationToken,
) -> Result<DesignResponse, RemoteError> {
    validate(&req)?;
    if req.check.is_some() || !req.verify {
        let message = "planner.design designs a check: leave out check, and keep verify".to_owned();
        return Err(RemoteError { code: ErrorCode::Invalid, message });
    }
    let mut ctx = Ctx::new(bus, cfg, trace, &req);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel;
    ctx.memory = memory::prepare(&ctx).await;
    ctx.instructions = instructions::gather(&ctx).await;
    let ctx = Arc::new(ctx);
    let design = designer::design(&ctx, true).await;
    // A failed or cancelled design still issued billable calls. Settle it
    // exactly like a run and retain every unanswered/unknown call in the reply.
    let tally = ctx.settle(ctx.cfg.late_reply_wait).await;
    let mut resp = DesignResponse {
        command: None,
        files: Vec::new(),
        rationale: String::new(),
        baseline: None,
        usage: tally.spend.usage,
        cost_usd: tally.spend.cost_usd,
        uncounted_calls: tally.uncounted_calls(),
        remaining_budget_usd: (tally.uncounted_calls() == 0).then(|| (ctx.budget_usd - tally.spend.cost_usd).max(0.0)),
        error: None,
        cancelled: false,
    };
    match design {
        Ok(Design::Check { command, files, rationale, baseline }) => {
            resp.command = Some(command);
            resp.files = files.into_iter().map(|(path, content)| CheckFile { path, content }).collect();
            resp.rationale = rationale;
            resp.baseline = baseline;
        }
        Ok(Design::Unverified(reason)) => resp.rationale = reason,
        Ok(Design::Failed(reason)) => {
            let message = format!("could not design a done-check: {reason}");
            resp.error = Some(message);
        }
        Err(error) => resp.error = Some(error.to_string()),
    }
    if ctx.cancel.is_cancelled() && !ctx.over_budget() {
        resp.command = None;
        resp.files.clear();
        resp.cancelled = true;
        resp.error = Some("design stopped; remote settlement may remain incomplete".into());
    }
    if let Some(error) = &resp.error {
        resp.rationale = error.clone();
    }
    tracing::info!(run = %ctx.trace, cost_usd = resp.cost_usd, uncounted_calls = resp.uncounted_calls, "design settled");
    Ok(resp)
}

fn validate(req: &RunRequest) -> Result<(), RemoteError> {
    let problem = if req.task.trim().is_empty() {
        "task is empty".to_owned()
    } else if req.workspace.trim().is_empty() {
        "workspace is empty".to_owned()
    } else if !(1..=MAX_ATTEMPTS).contains(&req.attempts) {
        format!("attempts must be 1 to {MAX_ATTEMPTS}, not {}", req.attempts)
    } else if req.check.as_ref().is_some_and(|c| c.trim().is_empty()) {
        "check is empty: leave it out to have one designed".to_owned()
    } else if req.check.is_none() && !req.check_files.is_empty() {
        "check_files without a check".to_owned()
    } else if req.check.is_some() && !req.verify {
        "a check with verify off: leave out one or the other".to_owned()
    } else if req.check_files.iter().any(|f| f.path.trim().is_empty()) {
        "a check file has no path".to_owned()
    } else if req.max_turns == Some(0) {
        "max_turns must be at least 1".to_owned()
    } else if req.max_check_rounds == Some(0) {
        "max_check_rounds must be at least 1".to_owned()
    } else if req.budget_usd.is_some_and(|b| !(b.is_finite() && b > 0.0)) {
        "budget_usd must be a positive amount".to_owned()
    } else {
        return Ok(());
    };
    Err(RemoteError { code: ErrorCode::Invalid, message: problem })
}

/// Run the attempts at once. The first to pass wins and cancels the rest;
/// every attempt is waited for. One that passed before a resume wins
/// without running. Reports come back in index order.
async fn race(ctx: &Arc<Ctx>, slots: Vec<Slot>, check: Option<Arc<Check>>) -> (Vec<Finished>, Option<u32>) {
    let cancel = ctx.cancel.child_token();
    let mut tasks = JoinSet::new();
    let mut indices = HashMap::new();
    let mut finished = Vec::new();
    let mut winner = None;
    for slot in slots {
        match slot {
            Slot::Done(done) => {
                if winner.is_none() && done.report.status == AttemptStatus::Passed {
                    winner = Some(done.report.index);
                }
                finished.push(done);
            }
            Slot::Run { index, saved } => {
                let token = cancel.child_token();
                let handle = tasks.spawn(attempt::run(ctx.scoped(token.clone()), index, check.clone(), token, saved));
                indices.insert(handle.id(), index);
            }
        }
    }
    while let Some(joined) = tasks.join_next_with_id().await {
        let done = match joined {
            Ok((_, done)) => done,
            Err(e) => {
                let index = indices.get(&e.id()).copied().unwrap_or_default();
                tracing::error!(run = %ctx.trace, attempt = index, error = %e, "an attempt task died");
                Finished::lost(index, format!("the attempt died: {e}"))
            }
        };
        if winner.is_none() && done.report.status == AttemptStatus::Passed {
            winner = Some(done.report.index);
            cancel.cancel();
        }
        finished.push(done);
    }
    finished.sort_by_key(|f| f.report.index);
    (finished, winner)
}

/// The response, without the run's totals.
async fn conclude(
    ctx: &Arc<Ctx>,
    req: &RunRequest,
    spec: Option<CheckSpec>,
    finished: Vec<Finished>,
    winner: Option<u32>,
    stopped: bool,
) -> RunResponse {
    let won = winner
        .filter(|_| !ctx.run_cancel.is_cancelled())
        .and_then(|w| finished.iter().find(|f| f.report.index == w))
        .and_then(|f| Some((f, f.fork.clone()?)));
    let mut resp = RunResponse {
        outcome: Outcome::Failed,
        check: spec.clone(),
        winner: None,
        summary: String::new(),
        changes: Vec::new(),
        patch: String::new(),
        patch_truncated: false,
        applied: false,
        fork: None,
        review: None,
        attempts: finished.iter().map(|f| f.report.clone()).collect(),
        usage: Default::default(),
        cost_usd: 0.0,
        uncounted_calls: 0,
        checkpoint: None,
    };

    match won {
        None => {
            resp.summary = failure_summary(spec.as_ref(), &finished);
            // A stopped run's forks are its checkpoint's: kept for the resume.
            if !stopped {
                ctx.drop_forks_except(None).await;
            }
        }
        Some((done, fork)) => {
            ctx.drop_forks_except(Some(&fork)).await;
            resp.winner = Some(done.report.index);
            resp.outcome = if spec.is_some() { Outcome::Passed } else { Outcome::Unverified };
            resp.summary = if done.summary.is_empty() {
                "The attempt finished without a summary.".to_owned()
            } else {
                done.summary.clone()
            };
            let mut keep = false;
            match ctx.diff(&fork).await {
                Ok(diff) => {
                    resp.changes = diff.changes;
                    resp.patch = diff.patch;
                    resp.patch_truncated = diff.truncated;
                    if let (true, Some(check), false) = (req.review, spec.as_ref(), resp.changes.is_empty()) {
                        let winner = Winner {
                            fork: &fork,
                            check,
                            changes: &resp.changes,
                            patch: &resp.patch,
                            truncated: resp.patch_truncated,
                        };
                        resp.review = Some(review::review(ctx, &winner).await);
                    }
                    let held = resp.review.as_ref().filter(|r| r.verdict != Verdict::Approve);
                    // An unverified attempt that changed nothing (it answered a question) has nothing to merge.
                    if let (true, Some(review)) = (req.apply, held) {
                        let what = if review.verdict == Verdict::Reject { "rejected" } else { "questioned" };
                        resp.summary.push_str(&format!(
                            "\n\nNot applied: the review {what} the changes. They are kept in {fork}."
                        ));
                        keep = true;
                    } else if req.apply && (spec.is_some() || !resp.changes.is_empty()) {
                        match ctx.merge(&fork).await {
                            Ok(_) => resp.applied = true,
                            Err(e) => {
                                let what = if e.message.starts_with("partial:") {
                                    "The apply needs recovery; the workspace may contain changes"
                                } else {
                                    "The changes were not applied"
                                };
                                resp.summary
                                    .push_str(&format!("\n\n{what}: {}. They are all kept in {fork}.", e.message));
                                keep = true;
                            }
                        }
                    } else {
                        keep = !req.apply;
                    }
                }
                Err(e) => {
                    resp.summary.push_str(&format!(
                        "\n\nThe changes could not be listed or applied: {}. They are kept in {fork}.",
                        e.message
                    ));
                    keep = true;
                }
            }
            if keep {
                ctx.forget_fork(&fork);
                resp.fork = Some(fork);
            } else if !resp.applied {
                ctx.drop_fork(&fork).await;
            }
        }
    }
    resp
}

fn failure_summary(spec: Option<&CheckSpec>, finished: &[Finished]) -> String {
    let mut summary = match spec {
        Some(check) => format!("No attempt passed the done-check `{}`.", check.command),
        None => "The attempt did not finish.".to_owned(),
    };
    for f in finished {
        summary.push_str(&format!("\n- attempt {}: {}", f.report.index, status_name(f.report.status)));
        if !f.report.note.is_empty() {
            summary.push_str(&format!(": {}", f.report.note));
        }
    }
    summary
}

fn status_name(status: AttemptStatus) -> &'static str {
    match status {
        AttemptStatus::Passed => "passed",
        AttemptStatus::Failed => "failed",
        AttemptStatus::Cancelled => "cancelled",
        AttemptStatus::Error => "error",
    }
}

/// The response when the designer could not produce a check and no attempt ran.
fn not_designed(ctx: &Ctx, reason: &str) -> RunResponse {
    let tally = ctx.tally();
    RunResponse {
        outcome: Outcome::Failed,
        check: None,
        winner: None,
        summary: format!("Could not design a done-check: {reason}."),
        changes: Vec::new(),
        patch: String::new(),
        patch_truncated: false,
        applied: false,
        fork: None,
        review: None,
        attempts: Vec::new(),
        usage: tally.spend.usage,
        cost_usd: tally.spend.cost_usd,
        uncounted_calls: tally.uncounted_calls() + ctx.carried_uncounted,
        checkpoint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use molt_proto::Budget;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Clone, Copy)]
    enum Reply {
        Known,
        MissingUsage,
        UnknownPrice,
        Failed,
        Late(Duration),
    }

    struct DesignBus {
        reply: Reply,
        started: Notify,
        dropped: AtomicBool,
    }

    impl DesignBus {
        fn new(reply: Reply) -> Arc<Self> {
            Arc::new(Self { reply, started: Notify::new(), dropped: AtomicBool::new(false) })
        }
    }

    #[async_trait]
    impl Bus for DesignBus {
        async fn call(&self, target: &str, _: Value, _: Budget, _: &TraceId) -> Result<Value, RemoteError> {
            match target {
                "fs.fork" => Ok(json!({"fork":"/scratch/design", "files":0})),
                "fs.drop" => {
                    self.dropped.store(true, Ordering::SeqCst);
                    Ok(json!({"dropped":true}))
                }
                "model.complete" => {
                    self.started.notify_one();
                    if let Reply::Late(delay) = self.reply {
                        tokio::time::sleep(delay).await;
                    }
                    if matches!(self.reply, Reply::Failed) {
                        return Err(RemoteError { code: ErrorCode::Unavailable, message: "reply unavailable".into() });
                    }
                    let mut value = json!({
                        "id":"fake", "model":"fake", "stop_reason":"tool_use",
                        "content":[{"type":"tool_use", "id":"submit", "name":"submit_check", "input":{"command":null, "files":[], "rationale":"a question, without an automated check"}}],
                        "usage":{"input_tokens":9,"output_tokens":3}, "cost_usd":0.25
                    });
                    if matches!(self.reply, Reply::MissingUsage) {
                        value.as_object_mut().unwrap().remove("usage");
                    }
                    if matches!(self.reply, Reply::UnknownPrice) {
                        value.as_object_mut().unwrap().remove("cost_usd");
                    }
                    Ok(value)
                }
                _ => Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {target}") }),
            }
        }
        async fn publish(&self, _: &str, _: Value) {}
    }

    #[tokio::test]
    async fn design_preserves_missing_usage_unknown_prices_and_failed_call_settlement() {
        for reply in [Reply::MissingUsage, Reply::UnknownPrice, Reply::Failed] {
            let bus = DesignBus::new(reply);
            let response =
                design(bus.clone(), Arc::new(Config::default()), RunRequest::new("question", "/w"), TraceId::random())
                    .await
                    .unwrap();
            assert_eq!(response.uncounted_calls, 1);
            assert_eq!(response.cost_usd, 0.0, "this is an empty known subtotal, never a free unknown call");
            assert!(response.remaining_budget_usd.is_none());
            if !matches!(reply, Reply::UnknownPrice) {
                assert!(response.error.is_some());
            }
            assert!(bus.dropped.load(Ordering::SeqCst));
        }
        let response = design(
            DesignBus::new(Reply::Known),
            Arc::new(Config { budget_usd: 2.0, ..Config::default() }),
            RunRequest::new("question", "/w"),
            TraceId::random(),
        )
        .await
        .unwrap();
        assert_eq!((response.cost_usd, response.uncounted_calls, response.remaining_budget_usd), (0.25, 0, Some(1.75)));
        assert_eq!((response.usage.input_tokens, response.usage.output_tokens), (9, 3));
        assert!(response.error.is_none());
    }

    #[tokio::test]
    async fn cancelled_designs_return_pending_or_late_settlement_and_drop_their_fork() {
        for (delay, wait, cost, uncounted) in [
            (Duration::from_secs(10), Duration::ZERO, 0.0, 1),
            (Duration::from_millis(20), Duration::from_millis(100), 0.25, 0),
        ] {
            let bus = DesignBus::new(Reply::Late(delay));
            let cancel = CancellationToken::new();
            let cfg = Arc::new(Config { late_reply_wait: wait, ..Config::default() });
            let task = tokio::spawn(design_cancellable(
                bus.clone(),
                cfg,
                RunRequest::new("question", "/w"),
                TraceId::random(),
                cancel.clone(),
            ));
            bus.started.notified().await;
            cancel.cancel();
            let response = tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
            assert!(response.cancelled && response.error.as_ref().unwrap().contains("stopped"));
            assert_eq!((response.cost_usd, response.uncounted_calls), (cost, uncounted));
            assert!(response.command.is_none() && response.files.is_empty());
            assert!(bus.dropped.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn requests_are_validated() {
        let ok = RunRequest::new("add a flag", "/w");
        assert!(validate(&ok).is_ok());
        let bad = [
            RunRequest { task: " ".into(), ..ok.clone() },
            RunRequest { workspace: String::new(), ..ok.clone() },
            RunRequest { attempts: 0, ..ok.clone() },
            RunRequest { attempts: 9, ..ok.clone() },
            RunRequest { check: Some(" ".into()), ..ok.clone() },
            RunRequest { check_files: vec![CheckFile { path: "t.rs".into(), content: String::new() }], ..ok.clone() },
            RunRequest { check: Some("true".into()), verify: false, ..ok.clone() },
            RunRequest {
                check: Some("true".into()),
                check_files: vec![CheckFile { path: " ".into(), content: String::new() }],
                ..ok.clone()
            },
            RunRequest { max_turns: Some(0), ..ok.clone() },
            RunRequest { max_check_rounds: Some(0), ..ok.clone() },
            RunRequest { budget_usd: Some(0.0), ..ok.clone() },
            RunRequest { budget_usd: Some(f64::INFINITY), ..ok.clone() },
        ];
        for req in bad {
            assert_eq!(validate(&req).unwrap_err().code, ErrorCode::Invalid, "{req:?}");
        }
        assert!(validate(&RunRequest { attempts: 8, ..ok }).is_ok());
    }
}
