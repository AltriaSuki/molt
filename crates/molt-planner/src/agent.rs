//! The model step shared by the designer's and the attempts' agent loops.

use molt_api::model::{CompleteResponse, STOP_MAX_TOKENS};
use molt_proto::RemoteError;
use tokio_util::sync::CancellationToken;

use crate::ctx::{Conversation, Ctx, Spend};

/// Why a loop stopped before the model said it was done.
#[derive(Debug)]
pub(crate) enum Stop {
    Cancelled,
    /// The run's spend reached its budget.
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
/// turns or the run is over budget; a cancellation during the call stops
/// waiting for it.
pub(crate) async fn turn(
    ctx: &Ctx,
    conv: &mut Conversation,
    max_turns: u32,
    meter: &mut Meter,
    cancel: &CancellationToken,
) -> Result<CompleteResponse, Stop> {
    if cancel.is_cancelled() {
        return Err(Stop::Cancelled);
    }
    if meter.turns >= max_turns {
        return Err(Stop::OutOfTurns);
    }
    if ctx.over_budget() {
        return Err(Stop::Budget);
    }
    let resp = tokio::select! {
        // A reply that is already in has been paid for, so it wins a tie and gets counted.
        biased;
        resp = ctx.complete(conv) => resp,
        _ = cancel.cancelled() => return Err(Stop::Cancelled),
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
    Ok(resp)
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
