//! The judge of a rubric: decides, criterion by criterion, whether an
//! attempt's result meets a task's criteria when no command can check it.
//!
//! The judge is on the evaluation side, like the reviewer: it sees the task,
//! the criteria, the attempt's final reply and its diff, quoted as data, and
//! reads the fork with read-only tools. It never sees the attempt's
//! conversation, and can change nothing. Criteria with a command are decided
//! by running it, as the done-check would be.

use std::sync::Arc;

use molt_api::model::{tool_result, user_blocks, user_text, STOP_MAX_TOKENS};
use molt_api::planner::tools::{SubmitJudgement, SUBMIT_JUDGEMENT};
use molt_api::planner::{Criterion, Finding};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts::{self, clip};
use crate::tools::{self, Actor};

/// The judge's turn limit.
const MAX_TURNS: u32 = 15;
/// Bytes of the diff and of the reply in the judge's first message; it can read the rest.
const MAX_DIFF: usize = 60_000;
const MAX_REPLY: usize = 30_000;
/// Bytes of a criterion command's output kept as its reason.
const COMMAND_TAIL: usize = 1_000;

/// Why a judgement did not finish.
pub(crate) enum Unjudged {
    Stopped(Stop),
    Failed(String),
}

/// Judge the result in `fork` against `rubric`: one finding per criterion,
/// in the rubric's order. `reply` is the attempt's final reply.
pub(crate) async fn judge(
    ctx: &Arc<Ctx>,
    fork: &str,
    rubric: &[Criterion],
    reply: &str,
    version: u32,
    cancel: &CancellationToken,
) -> Result<Vec<Finding>, Unjudged> {
    let mut findings: Vec<Option<Finding>> = vec![None; rubric.len()];
    let timeout_ms = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    for (i, c) in rubric.iter().enumerate() {
        let Some(command) = &c.command else { continue };
        let ran = tools::run_command(ctx, fork, command, timeout_ms)
            .await
            .map_err(|e| Unjudged::Failed(format!("could not run the criterion's command `{command}`: {e}")))?;
        let reason = if ran.success() {
            format!("`{command}` exited 0.")
        } else {
            format!("`{command}` failed ({}):\n{}", prompts::ending(&ran), prompts::output_tail(&ran, COMMAND_TAIL))
        };
        findings[i] = Some(Finding { criterion: c.text.clone(), met: ran.success(), reason, by_command: true });
    }
    // The judge model answers by number among the criteria it is given.
    let asked: Vec<(u32, &str)> = rubric
        .iter()
        .enumerate()
        .filter(|(_, c)| c.command.is_none())
        .map(|(i, c)| (i as u32 + 1, c.text.as_str()))
        .collect();
    if !asked.is_empty() {
        let diff = ctx.diff(fork).await.map_err(|e| Unjudged::Failed(format!("could not list the result: {e}")))?;
        let patch = clip(diff.patch.clone(), MAX_DIFF);
        let truncated = diff.truncated || patch.len() < diff.patch.len();
        let reply = clip(reply.to_owned(), MAX_REPLY);
        let requirements = ctx.control.upto(version);
        let first = prompts::judge_first_message(&ctx.task, &requirements, &asked, &reply, &patch, truncated);
        let mut conv = Conversation::new(prompts::JUDGE_SYSTEM, Arc::new(prompts::judge_tools()), first, None);
        conv.model = ctx.judge_model.clone();
        for judged in model(ctx, fork, &mut conv, &asked, cancel).await? {
            let i = judged.criterion as usize - 1;
            let reason = judged.reason.trim().to_owned();
            findings[i] =
                Some(Finding { criterion: rubric[i].text.clone(), met: judged.met, reason, by_command: false });
        }
    }
    let findings: Vec<Finding> = findings.into_iter().flatten().collect();
    let met = findings.iter().filter(|f| f.met).count();
    tracing::info!(run = %ctx.trace, met, criteria = rubric.len(), "judged a result");
    Ok(findings)
}

/// The judge model's findings, one for each of `asked`, or why it gave none.
async fn model(
    ctx: &Arc<Ctx>,
    fork: &str,
    conv: &mut Conversation,
    asked: &[(u32, &str)],
    cancel: &CancellationToken,
) -> Result<Vec<molt_api::planner::tools::Judged>, Unjudged> {
    let mut meter = Meter::default();
    let mut nudged = false;
    loop {
        let resp = agent::turn(ctx, conv, MAX_TURNS, &mut meter, cancel).await.map_err(|stop| match stop {
            Stop::OutOfTurns => Unjudged::Failed("the judge ran out of turns".into()),
            stop => Unjudged::Stopped(stop),
        })?;
        let calls = resp.tool_uses();
        if calls.is_empty() {
            if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
                conv.push(user_text(prompts::CONTINUE));
            } else if nudged {
                return Err(Unjudged::Failed("the judge stopped without a judgement".into()));
            } else {
                nudged = true;
                conv.push(user_text(prompts::JUDGE_NUDGE));
            }
            continue;
        }
        let cut_off = agent::cut_off(&resp);
        let reads: Vec<_> =
            calls.iter().filter(|c| prompts::REVIEWER_READS.contains(&c.name.as_str())).cloned().collect();
        let mut read_results = tools::execute(ctx, fork, &reads, cut_off, Actor::Judge).await.into_iter();
        let mut results: Vec<Value> = Vec::new();
        for call in &calls {
            if prompts::REVIEWER_READS.contains(&call.name.as_str()) {
                let next = read_results.next();
                results.push(next.unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true)));
            } else if call.name == SUBMIT_JUDGEMENT && cut_off != Some(call.id.as_str()) {
                match complete(call.input.clone(), asked) {
                    Ok(judged) => return Ok(judged),
                    Err(msg) => results.push(tool_result(&call.id, msg, true)),
                }
            } else if call.name == SUBMIT_JUDGEMENT {
                results.push(tool_result(&call.id, prompts::CUT_OFF, true));
            } else {
                let msg = format!("The judge cannot use {}: it only reads.", call.name);
                results.push(tool_result(&call.id, msg, true));
            }
        }
        conv.push(user_blocks(results));
    }
}

/// A submitted judgement with exactly one finding for each criterion asked,
/// or what is wrong with it.
fn complete(input: Value, asked: &[(u32, &str)]) -> Result<Vec<molt_api::planner::tools::Judged>, String> {
    let submit: SubmitJudgement = serde_json::from_value(input).map_err(|e| {
        format!("Invalid input for submit_judgement: {e}. Give findings, each with criterion, met and reason.")
    })?;
    let mut out = Vec::new();
    for (n, _) in asked {
        let mut theirs = submit.findings.iter().filter(|f| f.criterion == *n);
        match (theirs.next(), theirs.next()) {
            (Some(f), None) => out.push(f.clone()),
            (None, _) => return Err(format!("There is no finding for criterion {n}. Give one for every criterion.")),
            (Some(_), Some(_)) => return Err(format!("There are two findings for criterion {n}. Give one.")),
        }
    }
    if let Some(f) = submit.findings.iter().find(|f| !asked.iter().any(|(n, _)| *n == f.criterion)) {
        return Err(format!("There is no criterion {}. Judge only the criteria given.", f.criterion));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_judgement_must_answer_each_criterion_asked_once() {
        let asked = [(1, "a"), (3, "c")];
        let finding = |n: u32| json!({ "criterion": n, "met": true, "reason": "ok" });
        assert_eq!(complete(json!({ "findings": [finding(3), finding(1)] }), &asked).unwrap().len(), 2);
        let missing = complete(json!({ "findings": [finding(1)] }), &asked).unwrap_err();
        assert!(missing.contains("no finding for criterion 3"), "{missing}");
        let twice = complete(json!({ "findings": [finding(1), finding(1), finding(3)] }), &asked).unwrap_err();
        assert!(twice.contains("two findings for criterion 1"), "{twice}");
        let extra = complete(json!({ "findings": [finding(1), finding(2), finding(3)] }), &asked).unwrap_err();
        assert!(extra.contains("There is no criterion 2"), "{extra}");
    }
}
