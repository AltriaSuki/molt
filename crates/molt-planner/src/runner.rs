//! One run from request to response: settle the check, race the attempts,
//! apply the winner.

use std::collections::HashMap;
use std::sync::Arc;

use molt_api::planner::{AttemptStatus, CheckSpec, Outcome, RunRequest, RunResponse};
use molt_api::progress::Progress;
use molt_proto::{ErrorCode, RemoteError, TraceId};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::attempt::{self, Check, Finished};
use crate::ctx::Ctx;
use crate::designer::{self, Design};
use crate::memory;
use crate::{Bus, Config};

const MAX_ATTEMPTS: u32 = 8;

pub(crate) async fn run(
    bus: Arc<dyn Bus>,
    cfg: Arc<Config>,
    req: RunRequest,
    trace: TraceId,
) -> Result<RunResponse, RemoteError> {
    validate(&req)?;
    let mut ctx = Ctx::new(bus, cfg, trace, &req);
    ctx.memory = memory::prepare(&ctx).await;
    let ctx = Arc::new(ctx);
    tracing::info!(run = %ctx.trace, workspace = %ctx.workspace, attempts = req.attempts, "run started");

    let check = match &req.check {
        Some(command) => Some(Check { command: command.clone(), files: Vec::new() }),
        None if req.no_check => None,
        None => match designer::design(&ctx).await? {
            Design::Check { command, files } => Some(Check { command, files }),
            Design::Unverified(reason) => {
                ctx.note(format!("no automated done-check: {reason}")).await;
                None
            }
            Design::Failed(reason) => return Ok(not_designed(&ctx, &reason)),
        },
    };
    let spec = check.as_ref().map(|c| CheckSpec {
        command: c.command.clone(),
        files: c.paths(),
        designed: req.check.is_none(),
    });
    ctx.progress(Progress::CheckReady {
        run: ctx.run_id(),
        command: spec.as_ref().map(|s| s.command.clone()),
        files: spec.as_ref().map(|s| s.files.clone()).unwrap_or_default(),
        designed: req.check.is_none() && !req.no_check,
    })
    .await;

    // Without a check there is nothing to pick a winner by, so one attempt is enough.
    let count = if check.is_some() { req.attempts } else { 1 };
    let (finished, winner) = race(&ctx, count, check.map(Arc::new)).await;
    // Cancelled attempts may have left model calls running, which are billed: give their replies a moment
    // to land and be counted, while the winner is applied.
    let (mut resp, tally) =
        tokio::join!(conclude(&ctx, &req, spec, finished, winner), ctx.settle(ctx.cfg.late_reply_wait));
    resp.usage = tally.spend.usage;
    resp.cost_usd = tally.spend.cost_usd;
    resp.uncounted_calls = tally.pending;
    tracing::info!(
        run = %ctx.trace,
        outcome = ?resp.outcome,
        cost_usd = resp.cost_usd,
        uncounted_calls = resp.uncounted_calls,
        "run finished"
    );
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
    } else if req.no_check && req.check.is_some() {
        "check and no_check cannot both be given".to_owned()
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

/// Run `count` attempts at once. The first to pass wins and cancels the
/// rest; every attempt is waited for. Reports come back in index order.
async fn race(ctx: &Arc<Ctx>, count: u32, check: Option<Arc<Check>>) -> (Vec<Finished>, Option<u32>) {
    let cancel = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let mut indices = HashMap::new();
    for index in 0..count {
        let handle = tasks.spawn(attempt::run(ctx.clone(), index, check.clone(), cancel.child_token()));
        indices.insert(handle.id(), index);
    }
    let mut finished = Vec::new();
    let mut winner = None;
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
    ctx: &Ctx,
    req: &RunRequest,
    spec: Option<CheckSpec>,
    finished: Vec<Finished>,
    winner: Option<u32>,
) -> RunResponse {
    let won =
        winner.and_then(|w| finished.iter().find(|f| f.report.index == w)).and_then(|f| Some((f, f.fork.clone()?)));
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
        attempts: finished.iter().map(|f| f.report.clone()).collect(),
        usage: Default::default(),
        cost_usd: 0.0,
        uncounted_calls: 0,
    };

    match won {
        None => {
            resp.summary = failure_summary(spec.as_ref(), &finished);
            ctx.drop_forks_except(None).await;
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
                    // An unverified attempt that changed nothing (it answered a question) has nothing to merge.
                    if req.apply && (spec.is_some() || !resp.changes.is_empty()) {
                        match ctx.merge(&fork).await {
                            Ok(_) => resp.applied = true,
                            Err(e) => {
                                let what = if e.message.starts_with("partial:") {
                                    "The merge stopped partway, so the workspace has only some of the changes"
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
        attempts: Vec::new(),
        usage: tally.spend.usage,
        cost_usd: tally.spend.cost_usd,
        uncounted_calls: tally.pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            RunRequest { check: Some("make test".into()), no_check: true, ..ok.clone() },
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
