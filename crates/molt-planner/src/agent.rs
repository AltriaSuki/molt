//! The model step shared by the designer's and the attempts' agent loops.

use std::sync::Arc;

use molt_api::model::{CompleteResponse, STOP_MAX_TOKENS};
use molt_proto::{ErrorCode, RemoteError};
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::ctx::{Conversation, Ctx, Spend};

/// Why a loop stopped before the model said it was done.
#[derive(Debug)]
pub(crate) enum Stop {
    Cancelled,
    /// The run's spend reached its budget, or the cap on what this part of
    /// the run may spend (see [`Ctx::capped`]).
    Budget,
    OutOfTurns,
    Model(RemoteError),
    /// The model declined; the response must not be used.
    Refused,
}

/// What one loop has used.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Meter {
    pub turns: u32,
    pub spend: Spend,
}

/// Ask the model for its next turn and append that turn to `conv`. Stops
/// early, without calling the model, when the loop is cancelled, out of
/// turns or the run is over budget. A cancellation during the call stops
/// waiting for it; the call runs on and its reply still counts against the
/// run (see [`Ctx::complete`]).
pub(crate) async fn turn(
    ctx: &Arc<Ctx>,
    conv: &mut Conversation,
    max_turns: u32,
    meter: &mut Meter,
    cancel: &CancellationToken,
) -> Result<CompleteResponse, Stop> {
    if !ctx.web_available() && conv.tools.iter().any(|tool| tool["name"] == molt_api::planner::tools::FETCH_URL) {
        conv.tools = Arc::new(
            conv.tools.iter().filter(|tool| tool["name"] != molt_api::planner::tools::FETCH_URL).cloned().collect(),
        );
    }
    // A paused run starts no new call.
    if cancel.is_cancelled() || !ctx.control.wait_go(cancel, &ctx.cancel).await {
        return Err(if ctx.over_budget() { Stop::Budget } else { Stop::Cancelled });
    }
    if meter.turns >= max_turns {
        return Err(Stop::OutOfTurns);
    }
    // The run's budget, or the share of it this part of the run may spend.
    if ctx.over_budget() || ctx.over_cap() {
        return Err(Stop::Budget);
    }
    if conv.recall {
        conv.budget = Some(budget_line(ctx, meter.turns, max_turns));
    }
    // The limits of the model this call goes to, which may not be the run's.
    let model = ctx.model_of(conv);
    ctx.learn(&model).await;
    ctx.note_guess(&model).await;
    let limits = ctx.limits_of(&model);
    let before = conv.compacted.as_ref().map_or(0, |c| c.next);
    match conv.fit(limits) {
        Ok(None) => {}
        Ok(Some(fitted)) => {
            let who = conv.speaker();
            let (before, after) = fitted.tokens;
            let mut what = format!("{} earlier messages left out", fitted.left_out);
            if fitted.cut > 0 {
                what.push_str(&format!(", {} results cut", fitted.cut));
            }
            ctx.note(format!("{who}: {what} to fit the model's input (about {before} → {after} tokens)")).await;
        }
        Err(why) => return Err(Stop::Model(RemoteError { code: ErrorCode::Invalid, message: why })),
    }
    let left_out = conv.compacted.as_ref().is_some_and(|c| c.next > before);
    if left_out && conv.recall && ctx.cfg.compact_model {
        let summary = compact::summarize(ctx, conv, before, cancel).await;
        if let Some(reply) = &summary.reply {
            meter.spend.add(reply);
        }
        let who = conv.speaker();
        match summary.notes {
            Ok(notes) => {
                if let Some(compacted) = &mut conv.compacted {
                    compacted.notes = Some(notes);
                }
                conv.write_record();
                // The notes fit in the room the record had; should they not, the record is made to fit again.
                conv.fit(limits).map_err(|why| Stop::Model(RemoteError { code: ErrorCode::Invalid, message: why }))?;
                ctx.note(format!("{who}: the compactor wrote notes on the turns left out")).await;
            }
            Err(why) => {
                ctx.note(format!("{who}: no notes on the turns left out ({why}); Molt's own record of them stands"))
                    .await
            }
        }
    }
    // Once turns are left out, an attempt may read them back; offered last, so the tools before stay cached.
    if conv.recall && conv.compacted.is_some() && !conv.tools.iter().any(|t| t["name"] == compact::EARLIER_RESULT) {
        let mut tools = conv.tools.as_ref().clone();
        tools.push(compact::definition());
        conv.tools = Arc::new(tools);
    }
    let resp = tokio::select! {
        // A reply that is already in wins a tie, so the loop's meter counts it too.
        biased;
        resp = ctx.complete(conv, meter.turns + 1) => resp,
        _ = cancel.cancelled() => return Err(if ctx.over_budget() { Stop::Budget } else { Stop::Cancelled }),
    };
    meter.turns += 1;
    let resp = resp.map_err(Stop::Model)?;
    meter.spend.add(&resp);
    if resp.is_refusal() {
        return Err(Stop::Refused);
    }
    // The API rejects an empty assistant message; leaving it out is safe,
    // since consecutive user messages are merged into one turn.
    if !resp.content.is_empty() {
        conv.push(resp.as_turn());
    }
    conv.measured(&resp.usage);
    Ok(resp)
}

/// What the run has spent and the turns the loop has had, for the record of
/// left-out turns: the run's own count, which no turn can change.
fn budget_line(ctx: &Ctx, turns: u32, max_turns: u32) -> String {
    let tally = ctx.tally();
    let unknown = match tally.uncounted_calls() + ctx.carried_uncounted {
        0 => String::new(),
        n => format!(" and {n} calls of unknown cost"),
    };
    format!(
        "- When this record was written, the run had spent ${:.2}{unknown} of its ${:.2} budget, and this \
         conversation had had {turns} of its {max_turns} turns.",
        tally.spend.cost_usd, ctx.budget_usd
    )
}

/// The id of a tool call the output limit cut short. Only the last block of
/// a response can be cut, and a cut input may still parse, so it must not run.
pub(crate) fn cut_off(resp: &CompleteResponse) -> Option<&str> {
    if resp.stop_reason.as_deref() != Some(STOP_MAX_TOKENS) {
        return None;
    }
    let last = resp.content.last()?;
    (last["type"] == "tool_use").then(|| last["id"].as_str()).flatten()
}

#[cfg(test)]
mod tests {
    use molt_api::model::Usage;
    use serde_json::json;

    use super::*;

    #[test]
    fn only_a_trailing_tool_call_is_cut_off() {
        let resp = |content, stop: &str| CompleteResponse {
            id: "msg".into(),
            model: "m".into(),
            content,
            stop_reason: Some(stop.into()),
            stop_details: None,
            usage: Usage::default(),
            cost_usd: None,
            price_source: None,
            context_tokens: None,
            endpoint: None,
        };
        let a = json!({ "type": "tool_use", "id": "a", "name": "run", "input": {} });
        let b = json!({ "type": "tool_use", "id": "b", "name": "write_file", "input": { "path": "x" } });
        let text = json!({ "type": "text", "text": "and" });
        assert_eq!(cut_off(&resp(vec![a.clone(), b.clone()], "max_tokens")), Some("b"));
        assert_eq!(cut_off(&resp(vec![a.clone(), b, text.clone()], "max_tokens")), None);
        assert_eq!(cut_off(&resp(vec![a], "tool_use")), None);
        assert_eq!(cut_off(&resp(vec![], "max_tokens")), None);
    }
}
