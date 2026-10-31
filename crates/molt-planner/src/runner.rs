//! One run from request to response: settle the check, race the attempts,
//! apply the winner.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use molt_api::model::attempt_name;
use molt_api::planner::{
    AttemptStatus, Candidate, CheckDispute, CheckFile, CheckSpec, Criterion, DesignResponse, Outcome, Resume, Review,
    RunRequest, RunResponse, Schedule, Select, Selection, Verdict, Verification,
};
use molt_api::progress::Progress;
use molt_api::workflow::{self, Workflow};
use molt_proto::{ErrorCode, RemoteError, TraceId};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::attempt::{self, Check, Finished};
use crate::checkpoint::{self, Saved, SavedAttempt, SavedCheck, SavedEnd, SavedSpend, Store};
use crate::control::{self, Control, Registered};
use crate::ctx::Ctx;
use crate::designer::{self, Design};
use crate::review::{self, Winner};
use crate::schedule::{self, Adaptive, Evidence};
use crate::{instructions, memory, referenced, select, tools};
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
    run_on(bus, cfg, req, trace, cancel, None).await
}

/// A new run whose forks are made from `base` rather than the workspace,
/// so that its result is merged there: a step of a plan ([`crate::plan`]).
pub(crate) async fn run_on(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
    cancel: CancellationToken,
    base: Option<String>,
) -> Result<RunResponse, RemoteError> {
    let mut req = req;
    let routes = routes(&req, &cfg)?;
    let mut state = match (req.checkpoint, &cfg.state_dir) {
        (false, _) => None,
        (true, Some(dir)) => Some(dir.clone()),
        (true, None) => return Err(invalid("this planner keeps no checkpoints: it was started without a state dir")),
    };
    let mut ctx = Ctx::new(bus, cfg, trace, &req);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel;
    ctx.base = base;
    // What the hooks leave is the task, a checkpoint's too: a resume does not ask them again.
    ctx.task = crate::hooks::submit(&ctx).await.map_err(refused)?;
    req.task = ctx.task.clone();
    if let Some(candidates) = routes {
        // Picked before anything else, so a checkpoint saves the workflow the run goes through.
        req.workflow = Some(pick(&mut ctx, &req, &candidates).await.to_owned());
        if ctx.workflow.read_only() {
            // An answer in one go has nothing to go on from: a checkpoint is for the workflows that change things.
            req.checkpoint = false;
            state = None;
        }
    }
    if let Some(dir) = &state {
        let store = Store::create(dir, Saved::new(ctx.trace.as_str(), req.clone())).map_err(failed)?;
        ctx.checkpoint = Some(Arc::new(store));
    }
    announce_prompts(&ctx).await;
    gather_context(&mut ctx).await;
    let ctx = Arc::new(ctx);
    let _steer = steer(&ctx);
    if let Some(dir) = &state {
        prune(&ctx, dir).await;
    }
    tracing::info!(
        run = %ctx.trace, workspace = %ctx.workspace, attempts = req.attempts, workflow = %ctx.workflow.name,
        "run started"
    );
    let more = serde_json::json!({ "task": ctx.task, "workflow": ctx.workflow.name });
    crate::hooks::notify_with(&ctx, crate::hooks::Event::RunStart, &[], more).await;

    let designed = designed_here(&req, &ctx.workflow);
    let settled = match settle_check(&ctx, &req).await? {
        Ok(settled) => settled,
        Err(reason) => return Ok(finish_early(&ctx, not_designed(&ctx, &reason)).await),
    };
    save_check(&ctx, &settled, designed);
    let count = attempt_count(&req, &settled, &ctx.workflow);
    let adaptive = adaptive(&ctx, &req, count).await;
    // An adaptive run's schedule starts its first attempt itself.
    let slots = match adaptive {
        Some(_) => Vec::new(),
        None => (0..count).map(|index| Slot::Run { index, saved: None }).collect(),
    };
    Ok(finish(&ctx, &req, settled, designed, slots, adaptive).await)
}

/// The adaptive schedule of a run that asks for one, or whose planner
/// does by default (#13): `None` for a fixed run, and for one with room for
/// one attempt only, which has nothing to decide.
async fn adaptive(ctx: &Ctx, req: &RunRequest, count: u32) -> Option<Adaptive> {
    if req.schedule.unwrap_or(ctx.cfg.schedule) != Schedule::Adaptive {
        return None;
    }
    if count <= 1 {
        ctx.note("this run has room for one attempt only, so the adaptive schedule has nothing to decide").await;
        return None;
    }
    let asked = req.model.as_deref().filter(|m| !m.trim().is_empty());
    let pinned = ctx.seats().attempt.model.is_some();
    // The request's escalation models win over the planner's.
    let ladder = if req.escalation_models.is_empty() { &ctx.cfg.escalation_models } else { &req.escalation_models };
    let (schedule, note) = Adaptive::new(ladder, ctx.cfg.max_escalations, asked, ctx.model.as_deref(), pinned, count);
    if let Some(note) = note {
        ctx.note(note).await;
    }
    Some(schedule)
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
    run_req.interactive = req.interactive;
    run_req.keep_cancelled_forks = req.keep_cancelled_forks;
    run_req.budget_usd = req.budget_usd.or(run_req.budget_usd);
    run_req.design_budget_usd = req.design_budget_usd.or(run_req.design_budget_usd);
    run_req.checkpoint = true;
    run_req.resume = None;
    validate(&run_req, &cfg)?;

    let mut ctx = Ctx::new(bus, cfg, trace, &run_req);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel;
    announce_prompts(&ctx).await;
    gather_context(&mut ctx).await;
    ctx.restore(&saved.spend, saved.open_calls);
    ctx.control = Control::new(saved.requirements.clone());
    let mut next = saved.clone();
    next.request = run_req.clone();
    next.spend.uncounted_calls = ctx.carried_uncounted;
    next.open_calls = 0;
    next.planner = env!("CARGO_PKG_VERSION").to_owned();
    ctx.checkpoint = Some(Arc::new(Store::create(&dir, next).map_err(failed)?));
    let ctx = Arc::new(ctx);
    let _steer = steer(&ctx);
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
    let more = serde_json::json!({ "task": ctx.task, "workflow": ctx.workflow.name, "resumed": resume.run });
    crate::hooks::notify_with(&ctx, crate::hooks::Event::RunStart, &[], more).await;

    let (settled, designed) = if saved.settled {
        let check = saved.check.as_ref().map(|c| Check {
            command: c.command.clone(),
            files: c.files.iter().map(|f| (f.path.clone(), f.content.clone())).collect(),
            suite: c.suite.clone(),
        });
        let designed = saved.check.as_ref().is_some_and(|c| c.designed);
        (Settled { check, rubric: saved.rubric.clone() }, designed)
    } else {
        // Stopped while the check was being designed: design it again.
        let designed = designed_here(&run_req, &ctx.workflow);
        match settle_check(&ctx, &run_req).await? {
            Ok(settled) => {
                save_check(&ctx, &settled, designed);
                (settled, designed)
            }
            Err(reason) => return Ok(finish_early(&ctx, not_designed(&ctx, &reason)).await),
        }
    };

    let count = attempt_count(&run_req, &settled, &ctx.workflow);
    let mut adaptive = adaptive(&ctx, &run_req, count).await;
    if let Some(schedule) = &mut adaptive {
        schedule.restore(&saved.attempts, saved.escalations, resume.retry);
    }
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
                let end = SavedEnd {
                    status: AttemptStatus::Cancelled,
                    note: why.into(),
                    summary: String::new(),
                    findings: Vec::new(),
                    dispute: None,
                };
                ctx.save(|s| s.attempt(attempt.index).end = Some(end.clone()));
                slots.push(Slot::Done(Finished::saved(attempt, &end)));
            }
            None => slots.push(Slot::Run { index: attempt.index, saved: Some(attempt.clone()) }),
        }
    }
    let first = saved.attempts.iter().map(|a| a.index + 1).max().unwrap_or(0);
    if adaptive.is_some() {
        // The schedule starts the new attempt itself, after the ones that go on.
    } else if resume.retry {
        slots.extend((first..first + count).map(|index| Slot::Run { index, saved: None }));
    } else if saved.attempts.is_empty() {
        // Stopped before any attempt started.
        slots.extend((0..count).map(|index| Slot::Run { index, saved: None }));
    }
    let starts = adaptive.is_some() && (resume.retry || saved.attempts.is_empty());
    if !passed && !starts && !slots.iter().any(|s| matches!(s, Slot::Run { .. })) {
        ctx.note("every attempt of this run had ended; resume with retry to start new ones").await;
    }
    Ok(finish(&ctx, &run_req, settled, designed, slots, adaptive).await)
}

/// Make the run steerable with `planner.control` and report its state, until
/// the guards drop.
fn steer(ctx: &Arc<Ctx>) -> (Registered, tokio_util::sync::DropGuard) {
    ctx.control.keep_in(ctx.checkpoint.clone());
    let stop = CancellationToken::new();
    let publisher = ctx.clone();
    let publish = move |event| {
        let ctx = publisher.clone();
        tokio::spawn(async move { ctx.progress(event).await });
    };
    ctx.control.report(ctx.run_id(), publish, stop.clone());
    (control::register(ctx.trace.as_str(), ctx.control.clone()), stop.drop_guard())
}

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Failed, message: message.into() }
}

/// A run a hook did not let start.
fn refused(message: String) -> RemoteError {
    RemoteError { code: ErrorCode::Denied, message }
}

/// Whether this run designs its check: it is given none (a check with files
/// came from `planner.design`, and so did a rubric), and its workflow designs.
fn designed_here(req: &RunRequest, workflow: &Workflow) -> bool {
    workflow.stages.design.is_some()
        && req.verify
        && ((req.check.is_none() && req.rubric.is_empty()) || !req.check_files.is_empty())
}

/// How many attempts run: the workflow's count, or the request's. Without a
/// check or a rubric there is nothing to pick a winner by, and a kept
/// result to go on from is one fork, so one attempt is enough.
fn attempt_count(req: &RunRequest, settled: &Settled, workflow: &Workflow) -> u32 {
    let continues_kept = req.continues.as_ref().is_some_and(|c| c.fork.is_some());
    if !settled.verified() || workflow.read_only() || continues_kept {
        return 1;
    }
    workflow.stages.attempts.and_then(|a| a.count).unwrap_or(req.attempts)
}

/// For a request that names [`workflow::AUTO`], the workflows it can run
/// with, lightest first; `None` for any other, which is validated here.
fn routes(req: &RunRequest, cfg: &Config) -> Result<Option<Vec<&'static str>>, RemoteError> {
    if req.workflow.as_deref() != Some(workflow::AUTO) {
        validate(req, cfg)?;
        return Ok(None);
    }
    // Under `auto`, a checkpoint is asked for whichever workflow is picked that keeps one.
    let with = |name: &str| RunRequest {
        workflow: Some(name.to_owned()),
        checkpoint: req.checkpoint && !workflow(cfg, Some(name)).is_ok_and(|w| w.read_only()),
        ..req.clone()
    };
    let candidates: Vec<&'static str> =
        workflow::ROUTED.into_iter().filter(|name| validate(&with(name), cfg).is_ok()).collect();
    if candidates.is_empty() {
        // Why the heaviest cannot run says what to change.
        validate(&with(workflow::VERIFY), cfg)?;
    }
    Ok(Some(candidates))
}

/// Pick the run's workflow among `candidates` and make it the run's, saying so.
async fn pick(ctx: &mut Ctx, req: &RunRequest, candidates: &[&'static str]) -> &'static str {
    // The pick is a model call of the run: it counts against its budget like any other.
    let probe = Arc::new(ctx.clone());
    let routed = crate::route::route(&probe, candidates, req.check.as_deref()).await;
    drop(probe);
    let picked = workflow(&ctx.cfg, Some(routed.workflow)).unwrap_or_else(|_| Workflow::verify());
    ctx.workflow = Arc::new(picked);
    tracing::info!(run = %ctx.trace, workflow = routed.workflow, reason = %routed.reason, "picked a workflow");
    ctx.progress(Progress::Routed { run: ctx.run_id(), workflow: routed.workflow.to_owned(), reason: routed.reason })
        .await;
    routed.workflow
}

/// Say which roles of the run speak with a prompt of the pack, by its
/// hash, and which pack prompt is not used for want of a promotion.
async fn announce_prompts(ctx: &Ctx) {
    for note in ctx.seats().notes(&ctx.workflow) {
        ctx.note(note).await;
    }
}

/// The workflow `name` names: a built-in, or one of the planner's own
/// configuration. `None` is [`workflow::VERIFY`].
pub(crate) fn workflow(cfg: &Config, name: Option<&str>) -> Result<Workflow, String> {
    let mut found = match workflow::builtin(name) {
        Some(builtin) => builtin,
        None => {
            let name = name.unwrap_or_default();
            let known = || {
                [workflow::VERIFY, workflow::ASK, workflow::QUICK]
                    .into_iter()
                    .chain(cfg.workflows.iter().chain(&cfg.pack.workflows).map(|w| w.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            cfg.workflows
                .iter()
                .chain(&cfg.pack.workflows)
                .find(|w| w.name == name)
                .cloned()
                .ok_or_else(|| format!("no workflow is named {name:?}: this planner has {}", known()))?
        }
    };
    // The planner's starter can have every workflow whose attempts change files explore (#42).
    if cfg.explore {
        if let Some(attempts) = found.stages.attempts.as_mut().filter(|a| !a.read_only) {
            attempts.explore = true;
        }
    }
    if let Some(problem) = found.problem() {
        return Err(problem);
    }
    // A workflow an embedder configured may seat roles its pack does not declare.
    cfg.pack.seats(&found)?;
    Ok(found)
}

/// The context stage: what the model is given besides the task. A workflow
/// without it gives the model the task, the earlier tasks and the tools only.
pub(crate) async fn gather_context(ctx: &mut Ctx) {
    if ctx.workflow.stages.context.is_none() {
        return;
    }
    // Before memory, whose snapshot of the run's context records the skills it offers.
    ctx.skills = std::sync::Arc::new(crate::skills::discover(ctx).await);
    ctx.memory = memory::prepare(ctx).await;
    ctx.instructions = instructions::gather(ctx).await;
    ctx.referenced = referenced::gather(ctx).await;
}

/// How a run accepts a result: by its done-check, by a rubric an
/// independent judge decides, or, with neither, not at all (unverified).
struct Settled {
    check: Option<Check>,
    rubric: Vec<Criterion>,
}

impl Settled {
    fn verified(&self) -> bool {
        self.check.is_some() || !self.rubric.is_empty()
    }
}

/// What the run goes by: a check or a rubric, given or designed, or
/// neither. An `Err` inside says why the designer could not produce one.
async fn settle_check(ctx: &Arc<Ctx>, req: &RunRequest) -> Result<Result<Settled, String>, RemoteError> {
    let none = Settled { check: None, rubric: Vec::new() };
    Ok(Ok(match &req.check {
        Some(command) => Settled {
            check: Some(Check {
                command: command.clone(),
                files: req.check_files.iter().map(|f| (f.path.clone(), f.content.clone())).collect(),
                suite: req.suite.clone(),
            }),
            rubric: Vec::new(),
        },
        None if !req.rubric.is_empty() => Settled { check: None, rubric: req.rubric.clone() },
        None if !req.verify => {
            ctx.note("running without a done-check, as asked").await;
            none
        }
        None if ctx.workflow.stages.design.is_none() => {
            ctx.note(format!("running without a done-check: workflow {} designs none", ctx.workflow.name)).await;
            none
        }
        None => match designer::design(ctx, false).await? {
            Design::Check { command, files, suite, .. } => {
                Settled { check: Some(Check { command, files, suite }), rubric: Vec::new() }
            }
            Design::Rubric { criteria, .. } => {
                ctx.note(format!("no done-check command fits; a judge decides {} criteria", criteria.len())).await;
                Settled { check: None, rubric: criteria }
            }
            Design::Unverified(reason) => {
                ctx.note(format!("no automated done-check: {reason}")).await;
                none
            }
            Design::Failed(reason) => return Ok(Err(reason)),
        },
    }))
}

fn save_check(ctx: &Ctx, settled: &Settled, designed: bool) {
    let check = settled.check.as_ref();
    let rubric = settled.rubric.clone();
    let saved = check.map(|c| SavedCheck {
        command: c.command.clone(),
        files: c
            .files
            .iter()
            .map(|(path, content)| CheckFile { path: path.clone(), content: content.clone() })
            .collect(),
        designed,
        suite: c.suite.clone(),
    });
    ctx.save(|s| {
        s.settled = true;
        s.check = saved;
        s.rubric = rubric;
    });
    ctx.control.settle(check.is_some(), check.map(Check::paths).unwrap_or_default());
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
    settled: Settled,
    designed: bool,
    slots: Vec<Slot>,
    adaptive: Option<Adaptive>,
) -> RunResponse {
    let Settled { check, rubric } = settled;
    let spec = check.as_ref().map(|c| CheckSpec {
        command: c.command.clone(),
        files: c.paths(),
        designed,
        suite: c.suite.clone(),
    });
    ctx.progress(Progress::CheckReady {
        run: ctx.run_id(),
        command: spec.as_ref().map(|s| s.command.clone()),
        files: spec.as_ref().map(|s| s.files.clone()).unwrap_or_default(),
        designed,
        rubric: rubric.iter().map(|c| c.text.clone()).collect(),
    })
    .await;
    let check = check.map(Arc::new);
    let rubric = Arc::new(rubric);
    let (finished, passed) = race(ctx, slots, check.clone(), rubric.clone(), req.select, adaptive).await;
    ctx.control.freeze("the run is finishing");
    let stopped = ctx.checkpoint.is_some() && ctx.run_cancel.is_cancelled();
    // Cancelled attempts may have left model calls running, which are billed: give their replies a moment
    // to land and be counted, while the winner is applied.
    let (mut resp, _) = tokio::join!(
        conclude(ctx, req, (spec, check), &rubric, finished, passed, stopped),
        ctx.settle(ctx.cfg.late_reply_wait)
    );
    // That wait can end before `conclude` makes its own model calls (the review), so take the totals again
    // once it is done: with nothing pending this returns at once, and a review call still unanswered after
    // the wait stays uncounted rather than free.
    let tally = ctx.settle(ctx.cfg.late_reply_wait).await;
    resp.usage = tally.spend.usage;
    resp.cost_usd = tally.spend.cost_usd;
    resp.uncounted_calls = tally.uncounted_calls() + ctx.carried_uncounted;
    resp.prompts = ctx.spoke();
    crate::hooks::notify(ctx, crate::hooks::Event::RunFinished).await;
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
async fn finish_early(ctx: &Arc<Ctx>, mut resp: RunResponse) -> RunResponse {
    resp.prompts = ctx.spoke();
    if let Some(store) = &ctx.checkpoint {
        if ctx.run_cancel.is_cancelled() {
            resp.checkpoint = Some(ctx.run_id());
        } else {
            store.remove();
        }
    }
    crate::hooks::notify(ctx, crate::hooks::Event::RunFinished).await;
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
    let routes = routes(&req, &cfg)?;
    if req.check.is_some() || !req.rubric.is_empty() || !req.verify {
        let message = "planner.design designs a check: leave out check and rubric, and keep verify".to_owned();
        return Err(RemoteError { code: ErrorCode::Invalid, message });
    }
    let mut ctx = Ctx::new(bus, cfg, trace, &req);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel.clone();
    ctx.task = crate::hooks::submit(&ctx).await.map_err(refused)?;
    let picked = match &routes {
        Some(candidates) => Some(pick(&mut ctx, &req, candidates).await),
        None => None,
    };
    if let Some(name) = picked.filter(|_| ctx.workflow.stages.design.is_none()) {
        // A workflow that designs nothing: the caller runs the task with it.
        let tally = ctx.settle(ctx.cfg.late_reply_wait).await;
        let known = tally.uncounted_calls() == 0;
        return Ok(DesignResponse {
            command: None,
            files: Vec::new(),
            suite: None,
            rubric: Vec::new(),
            rationale: format!("workflow {name} designs no check"),
            baseline: None,
            usage: tally.spend.usage,
            cost_usd: tally.spend.cost_usd,
            uncounted_calls: tally.uncounted_calls(),
            remaining_budget_usd: known.then(|| (ctx.budget_usd - tally.spend.cost_usd).max(0.0)),
            error: None,
            cancelled: false,
            workflow: Some(name.to_owned()),
        });
    }
    if ctx.workflow.stages.design.is_none() {
        let message = format!("workflow {:?} has no design stage: planner.design has nothing to do", ctx.workflow.name);
        return Err(RemoteError { code: ErrorCode::Invalid, message });
    }
    announce_prompts(&ctx).await;
    gather_context(&mut ctx).await;
    let ctx = Arc::new(ctx);
    let design = designer::design(&ctx, true).await;
    // A failed or cancelled design still issued billable calls. Settle it
    // exactly like a run and retain every unanswered/unknown call in the reply.
    let tally = ctx.settle(ctx.cfg.late_reply_wait).await;
    let mut resp = DesignResponse {
        command: None,
        files: Vec::new(),
        suite: None,
        rubric: Vec::new(),
        rationale: String::new(),
        baseline: None,
        usage: tally.spend.usage,
        cost_usd: tally.spend.cost_usd,
        uncounted_calls: tally.uncounted_calls(),
        remaining_budget_usd: (tally.uncounted_calls() == 0).then(|| (ctx.budget_usd - tally.spend.cost_usd).max(0.0)),
        error: None,
        cancelled: false,
        workflow: picked.map(str::to_owned),
    };
    match design {
        Ok(Design::Check { command, files, suite, rationale, baseline }) => {
            resp.command = Some(command);
            resp.suite = suite;
            resp.files = files.into_iter().map(|(path, content)| CheckFile { path, content }).collect();
            resp.rationale = rationale;
            resp.baseline = baseline;
        }
        Ok(Design::Rubric { criteria, rationale }) => {
            resp.rubric = criteria;
            resp.rationale = rationale;
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
        resp.suite = None;
        resp.rubric.clear();
        resp.cancelled = true;
        resp.error = Some("design stopped; remote settlement may remain incomplete".into());
    }
    if let Some(error) = &resp.error {
        resp.rationale = error.clone();
    }
    tracing::info!(run = %ctx.trace, cost_usd = resp.cost_usd, uncounted_calls = resp.uncounted_calls, "design settled");
    Ok(resp)
}

/// A relative path that stays in the workspace.
fn inside(path: &str) -> bool {
    let path = std::path::Path::new(path);
    !path.as_os_str().is_empty()
        && path.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
}

/// Whether the planner can carry out `req`, with `cfg`'s default budget for one that sets none.
pub(crate) fn validate(req: &RunRequest, cfg: &Config) -> Result<(), RemoteError> {
    let budget = req.budget_usd.unwrap_or(cfg.budget_usd);
    let flow = workflow(cfg, req.workflow.as_deref()).map_err(invalid)?;
    let kept = req.continues.as_ref().and_then(|c| c.fork.as_deref());
    let problem = if flow.stages.compare.is_none() && !req.select.is_first() {
        format!(
            "select {} compares attempts that pass, and workflow {:?} has no compare stage: leave select out",
            req.select, flow.name
        )
    } else if flow.read_only() && (req.check.is_some() || !req.rubric.is_empty()) {
        format!(
            "workflow {:?} answers without changing anything, so there is nothing to check: leave out the check",
            flow.name
        )
    } else if flow.read_only() && req.checkpoint {
        format!(
            "workflow {:?} answers in one go without a fork, and keeps no checkpoint: leave checkpoint out",
            flow.name
        )
    } else if kept.is_some_and(|f| f.trim().is_empty()) {
        "continues names an empty fork: leave it out when the result was applied".to_owned()
    } else if req.continues.as_ref().is_some_and(|c| c.fork.is_none() && c.conversation.is_empty()) {
        "continues gives neither a conversation nor a kept result to go on from".to_owned()
    } else if req.task.trim().is_empty() {
        "task is empty".to_owned()
    } else if req.workspace.trim().is_empty() {
        "workspace is empty".to_owned()
    } else if !(1..=MAX_ATTEMPTS).contains(&req.attempts) {
        format!("attempts must be 1 to {MAX_ATTEMPTS}, not {}", req.attempts)
    } else if let Some(why) = escalation_problem(&req.escalation_models) {
        format!("escalation_models: {why}")
    } else if req.check.as_ref().is_some_and(|c| c.trim().is_empty()) {
        "check is empty: leave it out to have one designed".to_owned()
    } else if req.check.is_none() && !req.check_files.is_empty() {
        "check_files without a check".to_owned()
    } else if req.check.is_none() && req.suite.is_some() {
        "a suite without a check: the suite only compares results that pass a check".to_owned()
    } else if req.suite.as_ref().is_some_and(|s| s.trim().is_empty()) {
        "suite is empty: leave it out when the project has none".to_owned()
    } else if req.check.is_some() && !req.verify {
        "a check with verify off: leave out one or the other".to_owned()
    } else if !req.rubric.is_empty() && req.check.is_some() {
        "a check and a rubric: give one, the rubric only for a task no command can check".to_owned()
    } else if !req.rubric.is_empty() && !req.verify {
        "a rubric with verify off: leave out one or the other".to_owned()
    } else if req.rubric.iter().any(|c| c.text.trim().is_empty()) {
        "a criterion of the rubric has no text".to_owned()
    } else if req.rubric.len() > designer::MAX_CRITERIA {
        format!("the rubric has {} criteria; give at most {}", req.rubric.len(), designer::MAX_CRITERIA)
    } else if req.check_files.iter().any(|f| f.path.trim().is_empty()) {
        "a check file has no path".to_owned()
    } else if req.max_turns == Some(0) {
        "max_turns must be at least 1".to_owned()
    } else if req.max_check_rounds == Some(0) {
        "max_check_rounds must be at least 1".to_owned()
    } else if req.budget_usd.is_some_and(|b| !(b.is_finite() && b > 0.0)) {
        "budget_usd must be a positive amount".to_owned()
    } else if req.design_budget_usd.is_some_and(|b| !(b.is_finite() && b > 0.0)) {
        "design_budget_usd must be a positive amount".to_owned()
    } else if let Some(design) = req.design_budget_usd.filter(|d| *d > budget) {
        format!(
            "design_budget_usd (${design:.2}) is more than the run's budget (${budget:.2}): designing is paid out of \
             the run's budget, so give at most that, or raise the budget"
        )
    } else if let Some(f) = req.files.iter().find(|f| !inside(f)) {
        format!("file {f:?} is not a path inside the workspace")
    } else {
        return Ok(());
    };
    Err(RemoteError { code: ErrorCode::Invalid, message: problem })
}

/// What is wrong with a request's escalation models, if anything.
fn escalation_problem(models: &[String]) -> Option<String> {
    for (i, m) in models.iter().enumerate() {
        if m.trim().is_empty() || m.contains(',') {
            return Some(format!("{m:?} is not a model name"));
        }
        if models[..i].contains(m) {
            return Some(format!("{m} is named twice"));
        }
    }
    None
}

/// Run the attempts at once until `select` says to stop: with
/// [`Select::First`] the first to pass cancels the rest. Every attempt is
/// waited for. Ones that passed before a resume count without running.
/// Under an `adaptive` schedule, attempts start as it decides, on what the
/// attempts report and how they end. Reports come back in index order, the
/// passing attempts in the order they passed.
async fn race(
    ctx: &Arc<Ctx>,
    slots: Vec<Slot>,
    check: Option<Arc<Check>>,
    rubric: Arc<Vec<Criterion>>,
    select: Select,
    mut adaptive: Option<Adaptive>,
) -> (Vec<Finished>, Vec<u32>) {
    let cancel = ctx.cancel.child_token();
    let mut racers = Racers {
        tasks: JoinSet::new(),
        indices: HashMap::new(),
        check,
        rubric,
        cancel: cancel.clone(),
        signals: tokio::sync::mpsc::unbounded_channel(),
    };
    // What the run spent before its attempts, which the schedule leaves out of their cost.
    let before = ctx.tally().spend.cost_usd;
    let mut finished = Vec::new();
    let mut passed = Vec::new();
    for slot in slots {
        match slot {
            Slot::Done(done) => {
                if done.report.status == AttemptStatus::Passed && (passed.is_empty() || !select.is_first()) {
                    passed.push(done.report.index);
                }
                finished.push(done);
            }
            Slot::Run { index, saved } => {
                // A resumed attempt of an adaptive run keeps its model.
                let model = adaptive.as_ref().map(|a| a.model_of(index));
                racers.spawn(ctx, index, saved, model.map(|m| (m, None)));
            }
        }
    }
    if let Some(schedule) = adaptive.as_mut() {
        let decided = schedule.start(&view(ctx, &cancel, !passed.is_empty(), before));
        scheduled(ctx, &mut racers, schedule, decided).await;
    }
    // When the others stop being waited for, once one has passed.
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        let wait = deadline.unwrap_or_else(tokio::time::Instant::now);
        let joined = tokio::select! {
            joined = racers.tasks.join_next_with_id() => match joined {
                Some(joined) => joined,
                None => break,
            },
            Some(signal) = racers.signals.1.recv(), if adaptive.is_some() => {
                if let Some(schedule) = adaptive.as_mut() {
                    let view = view(ctx, &cancel, !passed.is_empty(), before);
                    let decided = schedule.decide(Evidence::Heard(signal), &view);
                    scheduled(ctx, &mut racers, schedule, decided).await;
                }
                continue;
            }
            _ = tokio::time::sleep_until(wait), if deadline.is_some() && !cancel.is_cancelled() => {
                ctx.note("the time to wait for more passing attempts is up; the ones still going are cancelled").await;
                cancel.cancel();
                continue;
            }
        };
        let done = match joined {
            Ok((_, done)) => done,
            Err(e) => {
                let index = racers.indices.get(&e.id()).copied().unwrap_or_default();
                tracing::error!(run = %ctx.trace, attempt = index, error = %e, "an attempt task died");
                Finished::lost(index, format!("the attempt died: {e}"))
            }
        };
        if done.report.status == AttemptStatus::Passed && (passed.is_empty() || !select.is_first()) {
            passed.push(done.report.index);
            if passed.len() == 1 && !racers.tasks.is_empty() {
                match stop_after_first(ctx, select, &done).await {
                    None => cancel.cancel(),
                    // A wait too long to represent is no deadline at all.
                    Some(secs) => {
                        deadline = secs.and_then(|s| tokio::time::Instant::now().checked_add(Duration::from_secs(s)))
                    }
                }
            }
        }
        if matches!(done.report.status, AttemptStatus::Failed | AttemptStatus::Error) {
            let more = serde_json::json!({ "attempt": done.report.index, "status": done.report.status });
            crate::hooks::notify_with(ctx, crate::hooks::Event::AttemptFailed, &[], more).await;
        }
        if let Some(schedule) = adaptive.as_mut() {
            let report = &done.report;
            let ended = Evidence::Ended {
                attempt: report.index,
                status: report.status,
                cost_usd: report.cost_usd,
                note: report.note.clone(),
            };
            let decided = schedule.decide(ended, &view(ctx, &cancel, !passed.is_empty(), before));
            scheduled(ctx, &mut racers, schedule, decided).await;
        }
        finished.push(done);
    }
    finished.sort_by_key(|f| f.report.index);
    (finished, passed)
}

/// The attempts of a race still going, and what each new one is given.
struct Racers {
    tasks: JoinSet<Finished>,
    indices: HashMap<tokio::task::Id, u32>,
    check: Option<Arc<Check>>,
    rubric: Arc<Vec<Criterion>>,
    cancel: CancellationToken,
    signals:
        (tokio::sync::mpsc::UnboundedSender<schedule::Signal>, tokio::sync::mpsc::UnboundedReceiver<schedule::Signal>),
}

impl Racers {
    /// Start attempt `index`, on from `saved` when it is resumed. `scheduled`:
    /// it is an adaptive run's, with its model (`None`: the run's) and what
    /// the attempts before it did.
    fn spawn(
        &mut self,
        ctx: &Arc<Ctx>,
        index: u32,
        saved: Option<SavedAttempt>,
        scheduled: Option<(Option<String>, Option<String>)>,
    ) {
        let token = self.cancel.child_token();
        let scope = match scheduled {
            Some((model, prior)) => {
                let model = model.map(Some);
                ctx.attempt_scope(token.clone(), model, self.signals.0.clone(), prior.map(Arc::from))
            }
            None => ctx.scoped(token.clone()),
        };
        let attempt = attempt::run(scope, index, self.check.clone(), self.rubric.clone(), token, saved);
        let handle = self.tasks.spawn(attempt);
        self.indices.insert(handle.id(), index);
    }
}

/// The run as the adaptive schedule sees it now.
fn view(ctx: &Ctx, cancel: &CancellationToken, won: bool, before: f64) -> schedule::View {
    let stopped = if ctx.over_budget() {
        Some("the run's budget is spent, or a call's cost is unknown".to_owned())
    } else if ctx.run_cancel.is_cancelled() || cancel.is_cancelled() {
        Some("the run was stopped".to_owned())
    } else {
        None
    };
    let budget = ctx.budget_state();
    let attempts_spent = (budget.spent_usd - before).max(0.0);
    schedule::View { budget, stopped, won, attempts_spent }
}

/// Say what the schedule decided, in the run's progress and so in the audit
/// log, and start the attempt it starts, keeping its model in the checkpoint.
async fn scheduled(
    ctx: &Arc<Ctx>,
    racers: &mut Racers,
    schedule: &Adaptive,
    decided: Option<(Option<schedule::Launch>, molt_api::planner::Scheduling)>,
) {
    let Some((launch, decision)) = decided else { return };
    tracing::info!(
        run = %ctx.trace, action = ?decision.action, attempt = ?decision.attempt, after = ?decision.after,
        model = ?decision.model, reason = %decision.reason, "scheduled"
    );
    ctx.progress(Progress::Scheduled { run: ctx.run_id(), decision }).await;
    if let Some(launch) = launch {
        let escalations = schedule.escalations();
        ctx.save(|s| {
            s.escalations = escalations;
            s.attempt(launch.index).model = launch.model.clone();
        });
        racers.spawn(ctx, launch.index, None, Some((launch.model, launch.prior)));
    }
}

/// What to do about the attempts still going once `first` has passed:
/// `None` to cancel them now, `Some(None)` to let them run to their end,
/// `Some(Some(secs))` to give them that long. Waiting needs budget: when
/// less is left than the passing attempt cost, another is unlikely to
/// finish, and the run takes the first as `first` would.
async fn stop_after_first(ctx: &Ctx, select: Select, first: &Finished) -> Option<Option<u64>> {
    let wait = match select {
        Select::First | Select::Wait { secs: 0 } => return None,
        Select::Wait { secs } => Some(secs),
        Select::All => None,
    };
    let left = ctx.budget_usd - ctx.tally().spend.cost_usd;
    if left < first.report.cost_usd {
        ctx.note(format!(
            "{} passed; ${left:.2} of the budget is left, less than it cost (${:.2}), so the others are \
             cancelled rather than waited for",
            attempt_name(first.report.index),
            first.report.cost_usd
        ))
        .await;
        return None;
    }
    Some(wait)
}

/// The response, without the run's totals.
async fn conclude(
    ctx: &Arc<Ctx>,
    req: &RunRequest,
    checks: (Option<CheckSpec>, Option<Arc<Check>>),
    rubric: &[Criterion],
    finished: Vec<Finished>,
    passed: Vec<u32>,
    stopped: bool,
) -> RunResponse {
    let (spec, check) = checks;
    if ctx.workflow.read_only() {
        return answered(ctx, rubric, &finished, &passed);
    }
    let candidates: Vec<(&Finished, String)> = passed
        .iter()
        .filter(|_| !ctx.run_cancel.is_cancelled())
        .filter_map(|w| finished.iter().find(|f| f.report.index == *w))
        .filter_map(|f| Some((f, f.fork.clone()?)))
        .collect();
    let mut resp = RunResponse {
        outcome: Outcome::Failed,
        check: spec.clone(),
        winner: None,
        summary: String::new(),
        changes: Vec::new(),
        left_out: None,
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
        requirements: ctx.control.all(),
        selection: None,
        rubric: rubric.to_vec(),
        judgement: Vec::new(),
        dispute: None,
        verification: Some(verification(spec.as_ref(), rubric, reviews_by_rules(ctx, req))),
        workflow: Some(ctx.workflow.name.clone()),
        conversation: Vec::new(),
        prompts: Vec::new(),
    };
    let outcome = match (&spec, rubric.is_empty()) {
        (Some(_), _) => Outcome::Passed,
        (None, false) => Outcome::Judged,
        (None, true) => Outcome::Unverified,
    };
    let Some((first, _)) = candidates.first() else {
        resp.summary = failure_summary(spec.as_ref(), rubric, &finished);
        // A check an attempt found contradicting the task needs a person before anything runs to it again.
        resp.dispute = finished.iter().find_map(|f| f.report.dispute.clone()).map(Box::new);
        if let Some(d) = &resp.dispute {
            resp.summary = format!("{}\n\n{}", dispute_summary(d), resp.summary);
        }
        // A stopped run's forks are its checkpoint's: kept for the resume. A caller that stopped the run
        // and asked for them decides which hold work; dropping them here would race it.
        if !stopped {
            if req.keep_cancelled_forks && ctx.run_cancel.is_cancelled() && !ctx.over_budget() {
                ctx.hand_over_forks();
            } else {
                ctx.drop_forks_except(None).await;
            }
        }
        return resp;
    };
    // The check the result passed: a constraint may have replaced the run's own. Every candidate passed
    // under the same requirements, as the first to pass froze them.
    if let (Some(check), Some(command)) = (&mut resp.check, ctx.control.check(first.report.requirements)) {
        check.command = command;
    }
    // Format and recheck before ranking and review, including kept results.
    // A failing formatter removes that candidate from consideration.
    let mut prepared = Vec::new();
    let mut failures = Vec::new();
    for (done, fork) in candidates {
        match crate::hooks::prepare(ctx, &fork, check.as_deref(), done.report.requirements).await {
            Ok(()) => prepared.push((done, fork)),
            Err(error) => failures.push((done, fork, error)),
        }
    }
    let candidates = prepared;
    if candidates.is_empty() {
        let (done, fork, error) = failures.remove(0);
        ctx.drop_forks_except(Some(&fork)).await;
        ctx.forget_fork(&fork);
        resp.winner = Some(done.report.index);
        resp.summary = format!("Not applied: {error}. The result is kept in {fork}.");
        resp.fork = Some(fork);
        return resp;
    }
    if let [(done, fork)] = candidates.as_slice() {
        ctx.drop_forks_except(Some(fork)).await;
        let diff = ctx.diff(fork).await;
        let review = match &diff {
            Ok(diff) => review_of(ctx, req, resp.check.as_ref(), done, fork, diff).await,
            Err(_) => None,
        };
        deliver(ctx, req, outcome, &mut resp, done, fork.clone(), diff, review).await;
        return resp;
    }
    let check = resp.check.clone();
    let picked = compare(ctx, req, check.as_ref(), !rubric.is_empty(), &candidates).await;
    let (done, fork) = &candidates[picked.position];
    ctx.drop_forks_except(Some(fork)).await;
    resp.selection = Some(Box::new(picked.selection));
    deliver(ctx, req, outcome, &mut resp, done, fork.clone(), picked.diff, picked.review).await;
    resp
}

/// The review of a candidate, when the run asks for one and it changed something.
async fn review_of(
    ctx: &Arc<Ctx>,
    req: &RunRequest,
    check: Option<&CheckSpec>,
    done: &Finished,
    fork: &str,
    diff: &molt_api::fs::DiffResponse,
) -> Option<Review> {
    // Reviewed against the check it passed and the requirements it followed.
    let stage = ctx.workflow.stages.review.filter(|_| req.review && !diff.changes.is_empty())?;
    if stage.rules_only {
        let files = check.map(|c| c.files.as_slice()).unwrap_or_default();
        return Some(review::by_rules(ctx, files, &diff.changes, &diff.patch).await);
    }
    let check = check?;
    let requirements = ctx.control.upto(done.report.requirements);
    let winner = Winner {
        fork,
        check,
        changes: &diff.changes,
        patch: &diff.patch,
        truncated: diff.truncated,
        requirements: &requirements,
    };
    Some(review::review(ctx, &winner).await)
}

/// Fill in `resp` from the winner `done`, and apply its changes when the run
/// asks for it and its review does not hold them back.
#[allow(clippy::too_many_arguments)] // The winner, its fork, diff and review, and where they go.
async fn deliver(
    ctx: &Arc<Ctx>,
    req: &RunRequest,
    outcome: Outcome,
    resp: &mut RunResponse,
    done: &Finished,
    fork: String,
    diff: Result<molt_api::fs::DiffResponse, RemoteError>,
    review: Option<Review>,
) {
    resp.winner = Some(done.report.index);
    resp.outcome = outcome;
    resp.judgement = done.findings.clone();
    resp.conversation = done.conversation.clone();
    resp.summary = if done.summary.is_empty() {
        "The attempt finished without a summary.".to_owned()
    } else {
        done.summary.clone()
    };
    let mut keep = false;
    match diff {
        Ok(diff) => {
            resp.changes = diff.changes;
            resp.left_out = diff.left_out;
            resp.patch = diff.patch;
            resp.patch_truncated = diff.truncated;
            resp.review = review;
            let held = resp.review.as_ref().filter(|r| r.verdict != Verdict::Approve);
            // An attempt without a check that changed nothing (it answered a question) has nothing to merge.
            if let (true, Some(review)) = (req.apply, held) {
                let what = if review.verdict == Verdict::Reject { "rejected" } else { "questioned" };
                resp.summary
                    .push_str(&format!("\n\nNot applied: the review {what} the changes. They are kept in {fork}."));
                keep = true;
            } else if req.apply && (outcome == Outcome::Passed || !resp.changes.is_empty()) {
                let paths: Vec<String> = resp.changes.iter().map(|c| c.path.clone()).collect();
                let applying = crate::hooks::applying(ctx, &fork, done.report.index, &paths).await;
                if let Some(context) = applying.as_ref().ok().filter(|c| !c.is_empty()) {
                    resp.summary.push_str(&format!("\n\nFrom the apply.before hooks: {context}"));
                }
                let merged = match applying {
                    Ok(_) => ctx.merge(&fork).await,
                    Err(reason) => Err(RemoteError {
                        code: ErrorCode::Denied,
                        message: format!("an apply.before hook refused them: {reason}"),
                    }),
                };
                match merged {
                    Ok(_) => {
                        resp.applied = true;
                        let more = serde_json::json!({ "attempt": done.report.index, "changes": paths });
                        crate::hooks::notify_with(ctx, crate::hooks::Event::AfterApply, &paths, more).await;
                    }
                    Err(e) => {
                        let what = if e.message.starts_with("partial:") {
                            "The apply needs recovery; the workspace may contain changes"
                        } else {
                            "The changes were not applied"
                        };
                        resp.summary.push_str(&format!("\n\n{what}: {}. They are all kept in {fork}.", e.message));
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
    // The fixed rules looked at nothing when nothing changed.
    if resp.verification == Some(Verification::Rules) && resp.review.is_none() {
        resp.verification = Some(Verification::None);
    }
}

/// The candidate [`compare`] picked, with its diff and review.
struct Picked {
    /// Where it is in the candidates.
    position: usize,
    selection: Selection,
    diff: Result<molt_api::fs::DiffResponse, RemoteError>,
    review: Option<Review>,
}

/// Rank the passing candidates on what can be audited, and pick the best
/// one the review approves (see [`select::RULE`]). `judged`: the run has a
/// rubric, whose criteria met count first.
async fn compare(
    ctx: &Arc<Ctx>,
    req: &RunRequest,
    check: Option<&CheckSpec>,
    judged: bool,
    candidates: &[(&Finished, String)],
) -> Picked {
    let mut diffs = Vec::new();
    for (_, fork) in candidates {
        diffs.push(ctx.diff(fork).await);
    }
    let suites = match check.and_then(|c| c.suite.as_ref()) {
        Some(suite) => run_suites(ctx, suite, candidates).await,
        None => vec![None; candidates.len()],
    };
    let mut scored: Vec<(usize, Candidate)> = Vec::new();
    for (position, ((done, _), diff)) in candidates.iter().zip(&diffs).enumerate() {
        match diff {
            Ok(diff) => {
                let mut candidate = select::candidate(done.report.index, suites[position], diff);
                if judged {
                    let met = done.findings.iter().filter(|f| f.met).count();
                    candidate.criteria_met = Some(u32::try_from(met).unwrap_or(u32::MAX));
                }
                scored.push((position, candidate));
            }
            Err(e) => {
                ctx.note(format!(
                    "{} is left out of the comparison: its changes could not be listed: {}",
                    attempt_name(done.report.index),
                    e.message
                ))
                .await
            }
        }
    }
    // A stable sort: ties go to the earlier to pass.
    scored.sort_by_key(|(_, a)| select::rank(a));
    let mut reviews: Vec<Option<Review>> = vec![None; candidates.len()];
    let mut picked = None;
    for (position, candidate) in &mut scored {
        let (done, fork) = &candidates[*position];
        let Ok(diff) = &diffs[*position] else { continue };
        let review = review_of(ctx, req, check, done, fork, diff).await;
        candidate.verdict = review.as_ref().map(|r| r.verdict);
        let approved = review.as_ref().is_none_or(|r| r.verdict == Verdict::Approve);
        reviews[*position] = review;
        if approved {
            picked = Some(*position);
            break;
        }
        if ctx.over_budget() {
            ctx.note("the budget ran out before every passing attempt was reviewed").await;
            break;
        }
    }
    // None approved: the one the review held back least, then the best ranked.
    let position = picked
        .or_else(|| scored.iter().filter(|(_, c)| c.verdict.is_some()).min_by_key(|(_, c)| c.verdict).map(|(p, _)| *p));
    let position = position.unwrap_or(0);
    let candidates_ranked: Vec<Candidate> = scored.into_iter().map(|(_, c)| c).collect();
    let winner = candidates[position].0.report.index;
    if let Some(c) = candidates_ranked.iter().find(|c| c.attempt == winner) {
        ctx.note(format!("picked attempt {winner} of {} passing: {}", candidates_ranked.len(), select::describe(c)))
            .await;
    }
    Picked {
        position,
        selection: Selection { rule: select::RULE.to_owned(), candidates: candidates_ranked },
        diff: std::mem::replace(&mut diffs[position], Err(failed("taken"))),
        review: reviews[position].take(),
    }
}

/// Run the project's suite in each candidate's fork at once: whether it
/// passed, or `None` when it could not be run.
async fn run_suites(ctx: &Arc<Ctx>, suite: &str, candidates: &[(&Finished, String)]) -> Vec<Option<bool>> {
    let timeout_ms = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    let mut runs = JoinSet::new();
    for (position, (_, fork)) in candidates.iter().enumerate() {
        let (ctx, fork, suite) = (ctx.clone(), fork.clone(), suite.to_owned());
        runs.spawn(async move {
            let result = match ctx.fork_from(&fork).await {
                Ok(copy) => {
                    let result = tools::run_command(&ctx, &copy, &suite, timeout_ms).await;
                    ctx.drop_fork(&copy).await;
                    result
                }
                Err(error) => Err(error),
            };
            (position, result)
        });
    }
    let mut passed = vec![None; candidates.len()];
    while let Some(joined) = runs.join_next().await {
        let Ok((position, ran)) = joined else { continue };
        match ran {
            Ok(ran) => passed[position] = Some(ran.success()),
            Err(e) => tracing::warn!(run = %ctx.trace, error = %e, "could not run the project's suite"),
        }
    }
    passed
}

fn failure_summary(spec: Option<&CheckSpec>, rubric: &[Criterion], finished: &[Finished]) -> String {
    let mut summary = match spec {
        Some(check) => format!("No attempt passed the done-check `{}`.", check.command),
        None if !rubric.is_empty() => "No attempt met every criterion of the rubric.".to_owned(),
        None => "The attempt did not finish.".to_owned(),
    };
    for f in finished {
        let status = if f.report.dispute.is_some() { "disputed" } else { status_name(f.report.status) };
        summary.push_str(&format!("\n- {}: {status}", attempt_name(f.report.index)));
        if !f.report.note.is_empty() {
            summary.push_str(&format!(": {}", f.report.note));
        }
    }
    summary
}

/// What a dispute says, for the run's summary.
fn dispute_summary(d: &CheckDispute) -> String {
    format!(
        "The done-check was disputed: {} found that it contradicts the task.\n\
         The check asserts: {}\nThe task says: \"{}\"\nTo reproduce: `{}`\n\
         Look at the check, change it if it is wrong, and run the task again.",
        attempt_name(d.attempt),
        d.assertion,
        d.task_quote,
        d.reproduce
    )
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
        left_out: None,
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
        requirements: ctx.control.all(),
        selection: None,
        rubric: Vec::new(),
        judgement: Vec::new(),
        dispute: None,
        verification: Some(Verification::None),
        workflow: Some(ctx.workflow.name.clone()),
        conversation: Vec::new(),
        prompts: Vec::new(),
    }
}

/// The reply of a workflow that only reads: the answer, unverified, with
/// nothing changed, applied or kept.
fn answered(ctx: &Ctx, rubric: &[Criterion], finished: &[Finished], passed: &[u32]) -> RunResponse {
    let mut resp = not_designed(ctx, "");
    resp.attempts = finished.iter().map(|f| f.report.clone()).collect();
    match passed.first().and_then(|w| finished.iter().find(|f| f.report.index == *w)) {
        Some(done) => {
            resp.outcome = Outcome::Unverified;
            resp.winner = Some(done.report.index);
            resp.summary = if done.summary.is_empty() {
                "The attempt finished without an answer.".to_owned()
            } else {
                done.summary.clone()
            };
            resp.conversation = done.conversation.clone();
        }
        None => resp.summary = failure_summary(None, rubric, finished),
    }
    resp
}

/// Whether a result without a check or a rubric is held to the review's fixed rules.
fn reviews_by_rules(ctx: &Ctx, req: &RunRequest) -> bool {
    req.review && ctx.workflow.stages.review.is_some_and(|r| r.rules_only)
}

/// What a run's result is held to: its check, else its rubric, else the
/// review's fixed rules when that is all the workflow reviews by, else nothing.
fn verification(check: Option<&CheckSpec>, rubric: &[Criterion], rules: bool) -> Verification {
    match (check, rubric.is_empty()) {
        (Some(_), _) => Verification::Command,
        (None, false) => Verification::Judged,
        (None, true) if rules => Verification::Rules,
        (None, true) => Verification::None,
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

    /// A bus whose attempts get a fork and then wait on a model call that never answers.
    struct StuckBus {
        started: Notify,
        dropped: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Bus for StuckBus {
        async fn call(&self, target: &str, payload: Value, _: Budget, _: &TraceId) -> Result<Value, RemoteError> {
            match target {
                "fs.fork" => Ok(json!({"fork":"/scratch/fork-a", "files":0})),
                "fs.drop" => {
                    self.dropped.lock().unwrap().push(payload["fork"].as_str().unwrap_or_default().to_owned());
                    Ok(json!({"dropped":true}))
                }
                "model.complete" => {
                    self.started.notify_one();
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Err(RemoteError { code: ErrorCode::Unavailable, message: "no reply".into() })
                }
                _ => Err(RemoteError { code: ErrorCode::Unavailable, message: format!("no {target}") }),
            }
        }
        async fn publish(&self, _: &str, _: Value) {}
    }

    /// Run with an explicit check, cancel it once the attempt asks the model, and return the forks it dropped.
    async fn cancelled_run_drops(keep_cancelled_forks: bool) -> Vec<String> {
        let bus = Arc::new(StuckBus { started: Notify::new(), dropped: Default::default() });
        let cancel = CancellationToken::new();
        let cfg = Arc::new(Config { late_reply_wait: Duration::from_millis(20), ..Config::default() });
        let req = RunRequest {
            check: Some("true".into()),
            attempts: 1,
            review: false,
            keep_cancelled_forks,
            ..RunRequest::new("add a flag", "/w")
        };
        let run = tokio::spawn(run_cancellable(bus.clone(), cfg, req, TraceId::random(), cancel.clone()));
        tokio::time::timeout(Duration::from_secs(5), bus.started.notified())
            .await
            .expect("the attempt asked the model");
        cancel.cancel();
        let resp = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap().unwrap();
        assert_eq!(resp.attempts[0].status, AttemptStatus::Cancelled);
        let dropped = bus.dropped.lock().unwrap();
        dropped.clone()
    }

    #[tokio::test]
    async fn a_cancelled_run_leaves_its_attempts_forks_to_a_caller_that_asked_for_them() {
        assert_eq!(cancelled_run_drops(false).await, ["/scratch/fork-a"], "by default a cancelled run cleans up");
        assert!(cancelled_run_drops(true).await.is_empty(), "the caller decides which forks hold work");
    }

    #[test]
    fn requests_are_validated() {
        let cfg = Config::default();
        let ok = RunRequest::new("add a flag", "/w");
        assert!(validate(&ok, &cfg).is_ok());
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
            RunRequest { files: vec!["../outside".into()], ..ok.clone() },
            RunRequest { files: vec!["/etc/passwd".into()], ..ok.clone() },
            RunRequest { files: vec![String::new()], ..ok.clone() },
            RunRequest { design_budget_usd: Some(0.0), ..ok.clone() },
            RunRequest { design_budget_usd: Some(f64::NAN), ..ok.clone() },
            // A design budget can only split the run's, never grow it.
            RunRequest { budget_usd: Some(1.0), design_budget_usd: Some(1.5), ..ok.clone() },
            RunRequest { design_budget_usd: Some(cfg.budget_usd + 0.01), ..ok.clone() },
        ];
        for req in bad {
            assert_eq!(validate(&req, &cfg).unwrap_err().code, ErrorCode::Invalid, "{req:?}");
        }
        assert!(validate(&RunRequest { files: vec!["src/lexer.rs".into(), "./a".into()], ..ok.clone() }, &cfg).is_ok());
        let all = RunRequest { budget_usd: Some(1.0), design_budget_usd: Some(1.0), ..ok.clone() };
        assert!(validate(&all, &cfg).is_ok());
        assert!(validate(&RunRequest { attempts: 8, ..ok }, &cfg).is_ok());
    }
}
