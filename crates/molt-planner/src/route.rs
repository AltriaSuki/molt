//! Picking a workflow for a run that names `auto`: one model call, on the
//! task and a bounded view of the conversation before it (#217), with no
//! tool but the one that answers.
//!
//! The pick can only be one of the workflows the request can run with (see
//! [`crate::runner`]), and anything that goes wrong falls back to the
//! heaviest of them: routing never makes a run lighter by failing.

use std::sync::Arc;

use molt_api::model::{tool_result, user_blocks, user_text, Role, STOP_MAX_TOKENS};
use molt_api::planner::tools::{SubmitRoute, SUBMIT_ROUTE};
use molt_api::planner::RunRequest;

use crate::agent::{self, Meter};
use crate::ctx::{Conversation, Ctx};
use crate::prompts;

/// Turns for the pick: its answer, and one reminder to give it.
const MAX_TURNS: u32 = 2;

pub(crate) struct Routed {
    pub workflow: &'static str,
    /// Why, in the model's few words; empty when the model did not pick.
    pub reason: String,
    /// What the pick's calls cost; `None` when one's cost is not known.
    pub cost_usd: Option<f64>,
}

/// Pick one of `candidates`, lightest first, for the run's task: `req`'s
/// check, earlier tasks and what it goes on from are what the pick reads
/// besides the task itself.
pub(crate) async fn route(ctx: &Arc<Ctx>, candidates: &[&'static str], req: &RunRequest) -> Routed {
    let heaviest = candidates.last().copied().unwrap_or(molt_api::workflow::VERIFY);
    if candidates.len() < 2 {
        return Routed { workflow: heaviest, reason: String::new(), cost_usd: Some(0.0) };
    }
    let first =
        prompts::route_first_message(&ctx.task, candidates, req.check.as_deref(), &req.earlier, req.continues.as_ref());
    let seat = ctx.seats().router;
    let mut conv = Conversation::seated(&seat, "", prompts::route_tools(), first, Role::Router, None);
    conv.model = ctx.cfg.route_model.clone();
    let mut meter = Meter::default();
    let cancel = ctx.run_cancel.clone();
    let cost = |meter: &Meter| (meter.spend.unknown_calls == 0).then_some(meter.spend.cost_usd);
    loop {
        let resp = match agent::turn(ctx, &mut conv, MAX_TURNS, &mut meter, &cancel).await {
            Ok(resp) => resp,
            Err(stop) => {
                tracing::warn!(run = %ctx.trace, ?stop, "could not pick a workflow");
                return Routed { workflow: heaviest, reason: String::new(), cost_usd: cost(&meter) };
            }
        };
        let cut_off = agent::cut_off(&resp);
        let submitted = resp
            .tool_uses()
            .into_iter()
            .filter(|c| c.name == SUBMIT_ROUTE && cut_off != Some(c.id.as_str()))
            .find_map(|c| serde_json::from_value::<SubmitRoute>(c.input).ok());
        if let Some(submit) = submitted {
            let picked = candidates.iter().copied().find(|c| *c == submit.workflow.trim());
            let reason = prompts::head(submit.reason.trim(), 200).to_owned();
            return match picked {
                Some(workflow) => Routed { workflow, reason, cost_usd: cost(&meter) },
                // Named one the request cannot run with: not the model's pick to make.
                None => Routed { workflow: heaviest, reason: String::new(), cost_usd: cost(&meter) },
            };
        }
        let calls = resp.tool_uses();
        if !calls.is_empty() {
            // Every call needs its result before the model can go on.
            let msg = "Invalid input for submit_route: give workflow, one of those offered, and reason.";
            conv.push(user_blocks(calls.iter().map(|c| tool_result(&c.id, msg, true)).collect()));
        } else if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
            conv.push(user_text(prompts::CONTINUE));
        } else {
            conv.push(user_text(prompts::ROUTE_NUDGE));
        }
    }
}
