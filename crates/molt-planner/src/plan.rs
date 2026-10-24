//! Staged runs (`planner.plan`, issue #47): a task carried out as ordered
//! steps, each its own run with its own check (see [`molt_api::plan`] and
//! `docs/plan.md`).
//!
//! The plan's result is a fork of the workspace made when the plan starts.
//! Each step is a run whose forks are made from that fork
//! ([`Ctx::base`]), so its attempts start from the steps before it and its
//! winner is merged into it. The steps run one after another under the
//! plan's trace, so the audit log holds the whole plan, every step's model
//! calls, checks and merges included, as one episode, and the person
//! following the plan can steer the step that is running as they would a
//! run. A step that does not pass ends the plan: the steps after it never
//! start, and the result of the steps that passed is kept. Once every step
//! has passed, the overall check runs on the result.

use std::sync::Arc;

use molt_api::model::{tool_result, user_blocks, user_text, Role, Usage, STOP_MAX_TOKENS};
use molt_api::plan::{self as api, PlanRequest, PlanResponse, StepRun, SubmitPlan, SUBMIT_PLAN};
use molt_api::planner::{CheckSpec, Outcome, RunRequest, RunResponse, Verification};
use molt_api::progress::Progress;
use molt_api::role::PromptUse;
use molt_proto::{ErrorCode, RemoteError, TraceId};
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::attempt::Check;
use crate::ctx::{Conversation, Ctx};
use crate::designer::{self, Design};
use crate::{hooks, prompts, runner, tools, Bus, Config};

/// Turns for the step designer: its answer, and reminders to give it.
const MAX_TURNS: u32 = 3;
/// Bytes of a failed overall check's output kept in the summary.
const CHECK_TAIL: usize = 2_000;
/// Bytes of the step designer's rationale kept.
const RATIONALE: usize = 1_000;

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

/// Whether the planner can carry out `req` as a plan.
fn validate(req: &PlanRequest, cfg: &Config) -> Result<(), RemoteError> {
    let run = &req.run;
    let kept = run.continues.as_ref();
    let problem = if run.resume.is_some() {
        "a plan does not resume a run: to go on with a plan, give its remaining steps, with its kept result as \
         continues.fork"
    } else if run.checkpoint {
        "a plan keeps no checkpoint: leave checkpoint out. A plan that stops keeps the result of the steps that \
         passed, and its remaining steps can go on from there"
    } else if !run.rubric.is_empty() {
        "a plan's overall check is a command: give check, or leave it out to have one designed"
    } else if kept.is_some_and(|c| !c.conversation.is_empty()) {
        "a plan goes on from a kept result, not from a conversation: leave out continues.conversation"
    } else if kept.is_some_and(|c| c.fork.is_none()) {
        "continues names no fork: give the kept result of the plan to go on in"
    } else if req.propose && !req.steps.is_empty() {
        "propose asks the planner for the steps: leave it out to run the steps given"
    } else if req.steps.is_empty() && !req.proposed.is_empty() {
        "proposed without steps: give the steps the plan goes by"
    } else {
        // The rest is a run's: what does not hold for the plan would not hold for its steps.
        let mut probe = run.clone();
        probe.continues = None;
        if probe.workflow.as_deref() == Some(molt_api::workflow::AUTO) {
            // Each step picks its own; the full workflow is the one every pick must fit.
            probe.workflow = None;
        }
        runner::validate(&probe, cfg)?;
        if runner::workflow(cfg, probe.workflow.as_deref()).is_ok_and(|w| w.read_only()) {
            return Err(invalid(format!(
                "workflow {:?} only reads, and a plan's steps change the project: name another",
                probe.workflow.unwrap_or_default()
            )));
        }
        for steps in [&req.steps, &req.proposed].into_iter().filter(|s| !s.is_empty()) {
            api::steps(steps).map_err(invalid)?;
        }
        return Ok(());
    };
    Err(invalid(problem))
}

pub(crate) async fn plan(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: PlanRequest,
    trace: TraceId,
    cancel: CancellationToken,
) -> Result<PlanResponse, RemoteError> {
    validate(&req, &cfg)?;
    let given = (!req.steps.is_empty()).then(|| api::steps(&req.steps)).transpose().map_err(invalid)?;
    let proposed = if req.proposed.is_empty() { Vec::new() } else { api::steps(&req.proposed).map_err(invalid)? };
    let mut ctx = Ctx::new(bus.clone(), cfg.clone(), trace.clone(), &req.run);
    ctx.cancel = cancel.clone();
    ctx.run_cancel = cancel.clone();
    let overall_designed = req.run.check.is_none() && req.run.verify && ctx.workflow.stages.design.is_some();
    // Only the step designer and the overall check's designer are given the project's context here.
    if given.is_none() || (!req.propose && overall_designed) {
        runner::gather_context(&mut ctx).await;
    }
    let ctx = Arc::new(ctx);
    tracing::info!(run = %ctx.trace, workspace = %ctx.workspace, propose = req.propose, "plan started");

    let (steps, rationale) = match given {
        Some(steps) => (steps, String::new()),
        None => match propose(&ctx).await {
            Ok(proposal) => proposal,
            Err(why) => {
                let mut resp = answer(&ctx, Vec::new(), Vec::new()).await;
                resp.error = Some(format!("could not plan the steps: {why}"));
                return Ok(resp);
            }
        },
    };
    let edited = !proposed.is_empty() && proposed != steps;
    if req.propose {
        let mut resp = answer(&ctx, steps, Vec::new()).await;
        resp.rationale = rationale;
        return Ok(resp);
    }
    ctx.progress(Progress::Planned { run: ctx.run_id(), steps: steps.clone(), edited }).await;
    if edited {
        ctx.note(format!("the plan as edited: {} steps, {} proposed", steps.len(), proposed.len())).await;
    }

    let mut plan = Plan { steps, proposed, edited, rationale, ran: Vec::new(), spent: Spent::default() };
    let overall = match overall_check(&ctx, &req.run, overall_designed).await {
        Ok(overall) => overall,
        Err(why) => {
            return Ok(plan.stopped_before(&ctx, &req.run, format!("Could not design the overall check: {why}.")).await)
        }
    };
    // The result of the steps: the kept result to go on in, or a new fork of the workspace.
    let continued = req.run.continues.as_ref().and_then(|c| c.fork.clone());
    let staging = match &continued {
        Some(kept) => ctx.diff(kept).await.map(|_| kept.clone()),
        None => ctx.fork().await,
    };
    let staging = match staging {
        Ok(fork) => fork,
        Err(e) => {
            let why = format!("Could not make the fork the steps work in: {}.", e.message);
            return Ok(plan.stopped_before(&ctx, &req.run, why).await);
        }
    };
    // It outlives the plan's context when it is kept, and is dropped below when not.
    ctx.forget_fork(&staging);

    let stop = plan.run_steps(&ctx, &req.run, &bus, &cfg, &staging).await;
    let checked = match (&stop, &overall) {
        (None, Some((check, _))) if !ctx.run_cancel.is_cancelled() => Some(run_check(&ctx, &staging, check).await),
        _ => None,
    };
    let resp = plan.finish(&ctx, &req.run, overall, checked, stop, staging, continued.is_some()).await;
    tracing::info!(run = %ctx.trace, cost_usd = resp.cost_usd, outcome = ?resp.result.as_ref().map(|r| r.outcome), "plan finished");
    Ok(resp)
}

/// Have the step designer split the task: its steps and why.
async fn propose(ctx: &Arc<Ctx>) -> Result<(Vec<String>, String), String> {
    let seat = ctx.seats().steps;
    let first = prompts::steps_first_message(&ctx.task, &ctx.preamble());
    let mut conv = Conversation::seated(&seat, "", prompts::steps_tools(), first, Role::Designer, None);
    let max_turns = seat.max_turns.unwrap_or(MAX_TURNS);
    let mut meter = Meter::default();
    loop {
        let resp =
            agent::turn(ctx, &mut conv, max_turns, &mut meter, &ctx.run_cancel).await.map_err(|stop| match stop {
                Stop::OutOfTurns => "the step designer stopped without submitting the steps".to_owned(),
                Stop::Budget => "the budget ran out before the steps were submitted".to_owned(),
                Stop::Model(e) => format!("model call failed: {e}"),
                Stop::Refused => "the model declined".to_owned(),
                Stop::Cancelled => "cancelled".to_owned(),
            })?;
        let cut_off = agent::cut_off(&resp);
        let mut results = Vec::new();
        for call in resp.tool_uses() {
            let why = if call.name != SUBMIT_PLAN {
                format!("{} is not available: call submit_plan with the steps.", call.name)
            } else if cut_off == Some(call.id.as_str()) {
                prompts::CUT_OFF.to_owned()
            } else {
                match serde_json::from_value::<SubmitPlan>(call.input.clone()).map_err(|e| e.to_string()) {
                    Ok(plan) => match api::steps(&plan.steps) {
                        Ok(steps) => return Ok((steps, prompts::head(plan.rationale.trim(), RATIONALE).to_owned())),
                        Err(why) => format!("Invalid input for submit_plan: {why}."),
                    },
                    Err(why) => format!("Invalid input for submit_plan: {why}."),
                }
            };
            results.push(tool_result(&call.id, why, true));
        }
        if !results.is_empty() {
            conv.push(user_blocks(results));
        } else if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
            conv.push(user_text(prompts::CONTINUE));
        } else {
            conv.push(user_text(prompts::STEPS_NUDGE));
        }
    }
}

/// The check the whole plan is held to once every step has passed, and
/// whether the planner designed it: the request's, a designed one, or none.
async fn overall_check(ctx: &Arc<Ctx>, run: &RunRequest, design: bool) -> Result<Option<(Check, bool)>, String> {
    if let Some(command) = &run.check {
        let files = run.check_files.iter().map(|f| (f.path.clone(), f.content.clone())).collect();
        return Ok(Some((Check { command: command.clone(), files, suite: run.suite.clone() }, false)));
    }
    if !run.verify {
        ctx.note("no overall check, as asked: each step runs without one too").await;
        return Ok(None);
    }
    if !design {
        ctx.note(format!("no overall check: workflow {} designs none", ctx.workflow.name)).await;
        return Ok(None);
    }
    match designer::design(ctx, false).await.map_err(|e| e.message)? {
        Design::Check { command, files, suite, .. } => Ok(Some((Check { command, files, suite }, true))),
        Design::Rubric { .. } => {
            ctx.note("no command checks the whole task: the steps' own checks are what the plan is held to").await;
            Ok(None)
        }
        Design::Unverified(reason) => {
            ctx.note(format!("no overall check: {reason}")).await;
            Ok(None)
        }
        Design::Failed(reason) => Err(reason),
    }
}

/// Run `check` on a copy of the plan's result, with its files restored:
/// whether it passed, and the end of its output when it did not.
async fn run_check(ctx: &Arc<Ctx>, staging: &str, check: &Check) -> (bool, String) {
    let copy = match ctx.fork_from(staging).await {
        Ok(copy) => copy,
        Err(e) => return (false, format!("could not copy the result to run it: {}", e.message)),
    };
    let mut ran = Ok(());
    for (path, content) in &check.files {
        if let Err(e) = ctx.write(&copy, path, content).await {
            ran = Err(format!("could not restore its file {path}: {}", e.message));
            break;
        }
    }
    let timeout_ms = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    let result = match ran {
        Ok(()) => match tools::run_command(ctx, &copy, &check.command, timeout_ms).await {
            Ok(out) if out.success() => (true, String::new()),
            Ok(out) => (false, prompts::output_tail(&out, CHECK_TAIL)),
            Err(e) => (false, format!("could not run it: {}", e.message)),
        },
        Err(why) => (false, why),
    };
    ctx.drop_fork(&copy).await;
    let what = if result.0 { "passed".to_owned() } else { format!("failed: {}", result.1.trim()) };
    ctx.note(format!("overall check `{}` {what}", check.command)).await;
    result
}

/// What the steps spent, from their replies.
#[derive(Default)]
struct Spent {
    usage: Usage,
    cost_usd: f64,
    uncounted_calls: u32,
    prompts: Vec<PromptUse>,
}

impl Spent {
    fn add(&mut self, resp: &RunResponse) {
        self.usage.add(&resp.usage);
        self.cost_usd += resp.cost_usd;
        self.uncounted_calls += resp.uncounted_calls;
        for used in &resp.prompts {
            if !self.prompts.contains(used) {
                self.prompts.push(used.clone());
            }
        }
    }
}

/// A plan going on: its steps, and what has run of them.
struct Plan {
    steps: Vec<String>,
    proposed: Vec<String>,
    edited: bool,
    rationale: String,
    ran: Vec<StepRun>,
    spent: Spent,
}

/// Why the plan stopped before its end: at which step, and why.
struct Stopped {
    at: usize,
    why: String,
}

impl Plan {
    /// Run the steps in order, each as its own run on `staging`, until one
    /// does not pass or the budget, or what is known of it, runs out.
    async fn run_steps(
        &mut self,
        ctx: &Arc<Ctx>,
        run: &RunRequest,
        bus: &Arc<dyn Bus>,
        cfg: &Arc<Config>,
        staging: &str,
    ) -> Option<Stopped> {
        let total = ctx.budget_usd;
        for at in 0..self.steps.len() {
            let own = ctx.settle(ctx.cfg.late_reply_wait).await;
            let unknown = own.uncounted_calls() + self.spent.uncounted_calls;
            let left = total - own.spend.cost_usd - self.spent.cost_usd;
            // The budget first: running out of it stops the plan's own calls too.
            let why = if unknown > 0 {
                // Unknown is never zero: what is left of the budget is not known either.
                Some(format!(
                    "{unknown} model calls of the plan so far have an unknown cost, so what is left of its budget \
                     is not known"
                ))
            } else if left <= 0.0 {
                Some(format!("the plan's ${total:.2} budget is spent"))
            } else if ctx.run_cancel.is_cancelled() {
                Some("the plan was stopped".to_owned())
            } else {
                None
            };
            if let Some(why) = why {
                return Some(Stopped { at, why });
            }
            let goal = self.steps[at].clone();
            ctx.progress(Progress::Step {
                run: ctx.run_id(),
                step: at as u32,
                steps: self.steps.len() as u32,
                goal: goal.clone(),
                fork: Some(staging.to_owned()),
            })
            .await;
            tracing::info!(run = %ctx.trace, step = at, left_usd = left, "plan step started");
            let req = step_request(run, &self.steps, at, left);
            // A child: stopping the plan stops the step, but a step that runs out of its share stops only itself.
            let cancel = ctx.run_cancel.child_token();
            let ran =
                runner::run_on(bus.clone(), cfg.clone(), req, ctx.trace.clone(), cancel, Some(staging.into())).await;
            let mut resp = match ran {
                Ok(resp) => resp,
                Err(e) => return Some(Stopped { at, why: format!("its run could not start: {}", e.message) }),
            };
            self.spent.add(&resp);
            // The conversation is for a task that goes on from a run; a step goes on from the plan's result.
            resp.conversation.clear();
            let why = not_passed(&resp);
            self.ran.push(StepRun { step: at as u32, goal, run: resp });
            if let Some(why) = why {
                return Some(Stopped { at, why });
            }
        }
        None
    }

    /// The reply of a plan that stopped before any step ran.
    async fn stopped_before(self, ctx: &Arc<Ctx>, run: &RunRequest, why: String) -> PlanResponse {
        let mut result = empty_result(ctx, run, &self.spent);
        result.summary = why;
        let mut resp = answer(ctx, self.steps.clone(), Vec::new()).await;
        result.usage = resp.usage;
        result.cost_usd = resp.cost_usd;
        result.uncounted_calls = resp.uncounted_calls;
        resp.remaining = self.steps;
        resp.proposed = self.proposed;
        resp.edited = self.edited;
        resp.rationale = self.rationale;
        resp.result = Some(Box::new(result));
        resp
    }

    /// Deliver the plan's result in `staging` and total it up: applied when
    /// every step and the overall check passed and the request applies,
    /// else kept when it holds anything, else dropped.
    #[allow(clippy::too_many_arguments)] // The plan, its check and how it went, and where its result is.
    async fn finish(
        self,
        ctx: &Arc<Ctx>,
        run: &RunRequest,
        overall: Option<(Check, bool)>,
        checked: Option<(bool, String)>,
        stop: Option<Stopped>,
        staging: String,
        continued: bool,
    ) -> PlanResponse {
        let mut result = empty_result(ctx, run, &self.spent);
        result.check = overall.as_ref().map(|(c, designed)| CheckSpec {
            command: c.command.clone(),
            files: c.paths(),
            designed: *designed,
            suite: c.suite.clone(),
        });
        let passed = stop.is_none() && checked.as_ref().is_none_or(|(ok, _)| *ok);
        result.outcome = match (&stop, &checked) {
            (Some(_), _) | (None, Some((false, _))) => Outcome::Failed,
            (None, Some((true, _))) => Outcome::Passed,
            (None, None) => {
                self.ran.iter().map(|s| s.run.outcome).min_by_key(|o| outcome_rank(*o)).unwrap_or(Outcome::Passed)
            }
        };
        result.verification = Some(match &overall {
            Some(_) => Verification::Command,
            None => self
                .ran
                .iter()
                .map(|s| s.run.verification.unwrap_or(Verification::None))
                .min_by_key(|v| level_rank(*v))
                .unwrap_or(Verification::None),
        });
        let diff = ctx.diff(&staging).await;
        let mut kept_because = None;
        match &diff {
            Ok(diff) => {
                result.changes = diff.changes.clone();
                result.left_out = diff.left_out.clone();
                result.patch = diff.patch.clone();
                result.patch_truncated = diff.truncated;
            }
            Err(e) => kept_because = Some(format!("its changes could not be listed: {}", e.message)),
        }
        if passed && run.apply && kept_because.is_none() && !result.changes.is_empty() {
            let paths: Vec<String> = result.changes.iter().map(|c| c.path.clone()).collect();
            let merged = match hooks::applying(ctx, &staging, 0, &paths).await {
                Ok(_) => ctx.merge(&staging).await.map_err(|e| e.message),
                Err(reason) => Err(format!("an apply.before hook refused them: {reason}")),
            };
            match merged {
                Ok(_) => {
                    result.applied = true;
                    let more = serde_json::json!({ "plan": true, "changes": paths });
                    hooks::notify_with(ctx, hooks::Event::AfterApply, &paths, more).await;
                }
                Err(why) => kept_because = Some(format!("they were not applied: {why}")),
            }
        }
        if !result.applied {
            if result.changes.is_empty() && kept_because.is_none() && !continued {
                ctx.drop_fork(&staging).await;
            } else {
                result.fork = Some(staging.clone());
            }
        }
        // The fixed rules looked at nothing when nothing changed.
        if result.verification == Some(Verification::Rules) && result.changes.is_empty() {
            result.verification = Some(Verification::None);
        }
        result.summary = self.summary(&result, &overall, &checked, stop.as_ref(), kept_because.as_deref());
        let remaining = stop.as_ref().map(|s| self.steps[s.at..].to_vec()).unwrap_or_default();
        let Plan { steps, proposed, edited, rationale, ran, spent } = self;
        let mut resp = answer(ctx, steps, Vec::new()).await;
        resp.usage.add(&spent.usage);
        resp.cost_usd += spent.cost_usd;
        resp.uncounted_calls += spent.uncounted_calls;
        result.usage = resp.usage;
        result.cost_usd = resp.cost_usd;
        result.uncounted_calls = resp.uncounted_calls;
        resp.proposed = proposed;
        resp.edited = edited;
        resp.rationale = rationale;
        resp.ran = ran;
        resp.remaining = remaining;
        resp.result = Some(Box::new(result));
        resp
    }

    /// The plan's summary: each step and how it went, the overall check, and
    /// where the result is.
    fn summary(
        &self,
        result: &RunResponse,
        overall: &Option<(Check, bool)>,
        checked: &Option<(bool, String)>,
        stop: Option<&Stopped>,
        kept_because: Option<&str>,
    ) -> String {
        let mut s = format!("A plan of {} steps:", self.steps.len());
        for (at, goal) in self.steps.iter().enumerate() {
            let how = match (self.ran.iter().find(|r| r.step as usize == at), stop) {
                (Some(_), Some(stop)) if stop.at == at => format!("did not pass: {}", stop_line(&stop.why)),
                (Some(r), _) => format!("{}{}", outcome_word(r.run.outcome), checked_by(&r.run)),
                (None, Some(stop)) if stop.at == at => format!("did not start: {}", stop.why),
                (None, _) => "not started".to_owned(),
            };
            s.push_str(&format!("\n{}. {goal}: {how}", at + 1));
        }
        match (overall, checked) {
            (Some((check, _)), Some((true, _))) => {
                s.push_str(&format!("\nThe overall check `{}` passed.", check.command))
            }
            (Some((check, _)), Some((false, tail))) => {
                s.push_str(&format!("\nThe overall check `{}` failed:\n{}", check.command, tail.trim_end()))
            }
            (Some((check, _)), None) => s.push_str(&format!("\nThe overall check `{}` did not run.", check.command)),
            (None, _) => s.push_str("\nThere is no overall check: the steps' own checks are what the plan is held to."),
        }
        let done = self.ran.iter().filter(|r| not_passed(&r.run).is_none()).count();
        let what = match done {
            0 => "No step passed".to_owned(),
            1 => "The result of step 1".to_owned(),
            n => format!("The result of steps 1 to {n}"),
        };
        s.push_str("\n\n");
        if let Some(stop) = stop {
            s.push_str(&format!("Step {} did not pass, so no step after it started. ", stop.at + 1));
        }
        match (&result.fork, result.applied) {
            (_, true) => s.push_str("Every step passed, and the result is applied."),
            (Some(fork), false) => {
                let why = kept_because.map(|w| format!(" ({w})")).unwrap_or_default();
                s.push_str(&format!("{what} is kept in {fork}{why}: apply it, or keep it."));
                if let Some(stop) = stop {
                    s.push_str(&format!(" To go on from step {}, run the remaining steps in it.", stop.at + 1));
                }
            }
            (None, false) if stop.is_some() => s.push_str(&format!("{what}; nothing was kept, since nothing changed.")),
            (None, false) => s.push_str("Every step passed; nothing changed."),
        }
        s
    }
}

/// Why a step's run does not let the plan go on, if it does not: it did
/// not pass, or its result is not in the plan's (the review held it back,
/// or the merge failed), or it stopped before it finished.
fn not_passed(resp: &RunResponse) -> Option<String> {
    if resp.outcome == Outcome::Failed {
        return Some(first_paragraph(&resp.summary));
    }
    if let Some(fork) = &resp.fork {
        return Some(format!(
            "its result is not in the plan's ({}); it is kept in {fork}",
            last_paragraph(&resp.summary).trim_end_matches('.')
        ));
    }
    if resp.checkpoint.is_some() {
        return Some("its run stopped before it finished".to_owned());
    }
    None
}

fn first_paragraph(text: &str) -> String {
    text.trim().split("\n\n").next().unwrap_or_default().trim().to_owned()
}

fn last_paragraph(text: &str) -> String {
    text.trim().rsplit("\n\n").next().unwrap_or_default().trim().to_owned()
}

/// A stop's reason on one line of the summary.
fn stop_line(why: &str) -> String {
    let line = why.lines().next().unwrap_or_default();
    if line.len() < why.trim().len() {
        format!("{line} …")
    } else {
        line.to_owned()
    }
}

fn outcome_word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Passed => "passed",
        Outcome::Judged => "passed, judged by a model",
        Outcome::Unverified => "done, unverified",
        Outcome::Failed => "failed",
    }
}

/// What a step was checked by, for the summary.
fn checked_by(resp: &RunResponse) -> String {
    match &resp.check {
        Some(check) => format!(" (`{}`)", check.command),
        None if !resp.rubric.is_empty() => format!(" ({} criteria)", resp.rubric.len()),
        None => String::new(),
    }
}

/// Weakest first: the plan without an overall check is as strong as its weakest step.
fn outcome_rank(outcome: Outcome) -> u8 {
    match outcome {
        Outcome::Failed => 0,
        Outcome::Unverified => 1,
        Outcome::Judged => 2,
        Outcome::Passed => 3,
    }
}

fn level_rank(level: Verification) -> u8 {
    match level {
        Verification::None => 0,
        Verification::Rules => 1,
        Verification::Judged => 2,
        Verification::Command => 3,
    }
}

/// The run request of step `at`: the plan's, with the step's task, what is
/// `left` of the budget, no check of its own (it designs one), and its
/// result merged into the plan's.
fn step_request(run: &RunRequest, steps: &[String], at: usize, left: f64) -> RunRequest {
    RunRequest {
        task: prompts::step_task(&run.task, steps, at),
        check: None,
        check_files: Vec::new(),
        suite: None,
        rubric: Vec::new(),
        budget_usd: Some(left),
        design_budget_usd: run.design_budget_usd.map(|d| d.min(left)),
        // Into the plan's result; whether that is applied is the plan's to say.
        apply: true,
        checkpoint: false,
        resume: None,
        continues: None,
        ..run.clone()
    }
}

/// A plan's reply with `steps` and `ran`, and the plan's own spend so far.
async fn answer(ctx: &Arc<Ctx>, steps: Vec<String>, ran: Vec<StepRun>) -> PlanResponse {
    let tally = ctx.settle(ctx.cfg.late_reply_wait).await;
    PlanResponse {
        steps,
        proposed: Vec::new(),
        edited: false,
        rationale: String::new(),
        ran,
        remaining: Vec::new(),
        result: None,
        usage: tally.spend.usage,
        cost_usd: tally.spend.cost_usd,
        uncounted_calls: tally.uncounted_calls(),
        error: None,
    }
}

/// The plan's result before anything is filled in: failed, changing nothing.
fn empty_result(ctx: &Ctx, run: &RunRequest, spent: &Spent) -> RunResponse {
    let mut prompts = ctx.spoke();
    for used in &spent.prompts {
        if !prompts.contains(used) {
            prompts.push(used.clone());
        }
    }
    RunResponse {
        outcome: Outcome::Failed,
        check: None,
        winner: None,
        summary: String::new(),
        changes: Vec::new(),
        left_out: None,
        patch: String::new(),
        patch_truncated: false,
        applied: false,
        fork: None,
        review: None,
        attempts: Vec::new(),
        usage: Usage::default(),
        cost_usd: 0.0,
        uncounted_calls: 0,
        checkpoint: None,
        requirements: Vec::new(),
        selection: None,
        rubric: Vec::new(),
        judgement: Vec::new(),
        dispute: None,
        verification: Some(Verification::None),
        workflow: Some(run.workflow.clone().unwrap_or_else(|| ctx.workflow.name.clone())),
        conversation: Vec::new(),
        prompts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_knows_the_whole_plan_and_does_only_its_own_part() {
        let steps = vec!["add the flag".to_owned(), "use it everywhere".to_owned(), "document it".to_owned()];
        let task = prompts::step_task("Add a --quiet flag.", &steps, 1);
        assert!(task.starts_with("Add a --quiet flag.\n\n<plan>\n"), "{task}");
        assert!(task.contains("1. add the flag (done: its result is in the workspace)"), "{task}");
        assert!(task.contains("2. use it everywhere (this step)"), "{task}");
        assert!(task.contains("3. document it (a later step)"), "{task}");
        assert!(task.contains("Do step 2 now: use it everywhere"), "{task}");
        let req = step_request(&RunRequest { apply: false, ..RunRequest::new("t", "/w") }, &steps, 0, 1.5);
        assert!(req.apply && req.check.is_none() && !req.checkpoint);
        assert_eq!(req.budget_usd, Some(1.5));
    }

    #[test]
    fn a_plan_is_as_strong_as_its_weakest_step() {
        let weakest =
            [Outcome::Passed, Outcome::Unverified, Outcome::Judged].into_iter().min_by_key(|o| outcome_rank(*o));
        assert_eq!(weakest, Some(Outcome::Unverified));
        let levels = [Verification::Command, Verification::Rules].into_iter().min_by_key(|v| level_rank(*v));
        assert_eq!(levels, Some(Verification::Rules));
    }
}
