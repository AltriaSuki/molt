//! The vetting of a designed check: before any attempt works to it, an
//! independent agent compares the check's command and files with the task
//! and reports the assertions the task contradicts or gives no basis for.
//!
//! The vetter is on the evaluation side, like the reviewer: it sees the task
//! and the check, quoted as data, reads the designer's fork with read-only
//! tools, and can change nothing.

use std::sync::Arc;

use molt_api::model::{tool_result, user_blocks, user_text, STOP_MAX_TOKENS};
use molt_api::planner::tools::{Conflict, SubmitVetting, SUBMIT_VETTING};
use serde_json::Value;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts;
use crate::tools::{self, Actor};

/// The vetter's turn limit.
const MAX_TURNS: u32 = 15;

/// The conflicts the vetter found between the check and the task (none when
/// it fits), or why it did not finish.
pub(crate) async fn vet(
    ctx: &Arc<Ctx>,
    fork: &str,
    command: &str,
    files: &[(String, String)],
) -> Result<Vec<Conflict>, String> {
    let first = prompts::vetter_first_message(&ctx.task, &ctx.preamble(), command, files);
    let mut conv = Conversation::new(prompts::VETTER_SYSTEM, Arc::new(prompts::vetter_tools()), first, None);
    let mut meter = Meter::default();
    let mut nudged = false;
    loop {
        let resp =
            agent::turn(ctx, &mut conv, MAX_TURNS, &mut meter, &ctx.cancel).await.map_err(|stop| match stop {
                Stop::OutOfTurns => "it ran out of turns".to_owned(),
                Stop::Budget if !ctx.over_budget() => "the design budget ran out".to_owned(),
                Stop::Budget => "the budget ran out".to_owned(),
                Stop::Model(e) => format!("the model call failed: {}", e.message),
                Stop::Refused => "the model declined".to_owned(),
                Stop::Cancelled => "the run was stopped".to_owned(),
            })?;
        let calls = resp.tool_uses();
        if calls.is_empty() {
            if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
                conv.push(user_text(prompts::CONTINUE));
            } else if nudged {
                return Err("it stopped without submitting what it found".into());
            } else {
                nudged = true;
                conv.push(user_text(prompts::VET_NUDGE));
            }
            continue;
        }
        let cut_off = agent::cut_off(&resp);
        let reads: Vec<_> =
            calls.iter().filter(|c| prompts::REVIEWER_READS.contains(&c.name.as_str())).cloned().collect();
        let mut read_results = tools::execute(ctx, fork, &reads, cut_off, Actor::Vetter).await.into_iter();
        let mut results: Vec<Value> = Vec::new();
        for call in &calls {
            if prompts::REVIEWER_READS.contains(&call.name.as_str()) {
                let next = read_results.next();
                results.push(next.unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true)));
            } else if call.name == SUBMIT_VETTING && cut_off == Some(call.id.as_str()) {
                results.push(tool_result(&call.id, prompts::CUT_OFF, true));
            } else if call.name == SUBMIT_VETTING {
                match found(call.input.clone()) {
                    Ok(conflicts) => return Ok(conflicts),
                    Err(msg) => results.push(tool_result(&call.id, msg, true)),
                }
            } else {
                let msg = format!("The vetter cannot use {}: it only reads.", call.name);
                results.push(tool_result(&call.id, msg, true));
            }
        }
        conv.push(user_blocks(results));
    }
}

/// The conflicts a submit_vetting call reports, trimmed, or what is wrong with it.
fn found(input: Value) -> Result<Vec<Conflict>, String> {
    let submit: SubmitVetting = serde_json::from_value(input).map_err(|e| {
        format!("Invalid input for submit_vetting: {e}. Give conflicts, each with assertion, task_quote and why.")
    })?;
    let mut out = Vec::new();
    for c in submit.conflicts {
        let conflict = Conflict {
            assertion: c.assertion.trim().to_owned(),
            task_quote: c.task_quote.trim().to_owned(),
            why: c.why.trim().to_owned(),
        };
        if conflict.assertion.is_empty() || conflict.why.is_empty() {
            return Err("A conflict has no assertion or no reason. Name the assertion and say why it conflicts.".into());
        }
        out.push(conflict);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_conflict_needs_an_assertion_and_a_reason() {
        assert_eq!(found(json!({ "conflicts": [] })).unwrap(), []);
        let ok = found(json!({ "conflicts": [{ "assertion": " col 20 ", "task_quote": "", "why": "it is 21" }] }));
        assert_eq!(ok.unwrap()[0].assertion, "col 20");
        let empty = json!({ "conflicts": [{ "assertion": "x", "task_quote": "q", "why": " " }] });
        assert!(found(empty).unwrap_err().contains("no reason"));
        assert!(found(json!({ "verdict": "ok" })).is_ok(), "no conflicts listed is none found");
        assert!(found(json!({ "conflicts": "none" })).unwrap_err().starts_with("Invalid input"));
    }
}
