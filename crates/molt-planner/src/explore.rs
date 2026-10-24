//! `explore` (#42): an attempt asks a question, and a sub-agent answers it
//! from the attempt's fork with tools that only read.
//!
//! The sub-agent is the run's `explorer` role, so a prompt pack can give it
//! a prompt, a model, a turn limit and a budget share of its own. It has a
//! conversation of its own, which is dropped when it ends: only its
//! conclusion goes back to the attempt, as the result of the call, and none
//! of what it read. Its model calls carry the run's trace and the
//! attempt's index, its spend counts toward the run's and the attempt's,
//! and it stops when the attempt is stopped or its share of the budget is
//! spent.

use std::sync::Arc;

use molt_api::model::{speaker, text_block, tool_result, user_blocks, user_text, Role, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{self as t, Explore};
use molt_api::progress::Progress;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx, Spend};
use crate::prompts::{self, clip};
use crate::registry::{self, Builtin};
use crate::tools::{self, Actor};

/// The share of what the attempt has left to spend that one call may
/// spend, when the explorer role sets none.
pub(crate) const SHARE: f64 = 0.1;
/// The sub-agent's turn limit, when the explorer role sets none.
pub(crate) const MAX_TURNS: u32 = 20;
/// Bytes of the conclusion that go back to the attempt.
const MAX_CONCLUSION: usize = 20_000;

/// The tools the sub-agent may be offered, in this order: they only read.
/// `run` is not among them: a command in the fork can change it, and
/// `shell.run` has no way to run one that provably cannot.
const READS: [Builtin; 4] = [Builtin::ReadFile, Builtin::ListFiles, Builtin::Search, Builtin::FindSymbol];

/// What the sub-agent's system prompt says after the pack's `explorer`
/// prompt, which a pack cannot drop by rewriting it.
pub(crate) const EXPLORE_PART: &str = "

Here you answer another agent, not the user. It is carrying out a task in a copy of the user's workspace, which may \
already hold its changes, and asks you the question in the message below about that copy, which is the one your \
tools read. It sees only your final reply, never what you read: make the reply a self-contained conclusion, short, \
with the path and line of each thing it rests on (as path:line), and do not paste whole files.";

/// The `explore` tool's definition, offered last to the attempts of a
/// workflow that explores, so the tools before it stay a cached prefix.
pub(crate) fn definition() -> Value {
    json!({
        "name": t::EXPLORE,
        "description": "Ask a sub-agent a question about the project that needs reading or searching many files: \
                        where something is defined or used, how a part works, which files a change would touch. \
                        It reads your workspace, your changes included, with tools that only read, in a \
                        conversation of its own, and returns only its conclusion with path:line references, so \
                        what it reads does not fill your conversation. It cannot change files or run commands. \
                        Read a file yourself when you already know which one you need. Each call spends part of \
                        your budget.",
        "input_schema": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question, on its own: the sub-agent sees only this and the project's \
                                    context, not your task or your conversation.",
                },
            },
            "required": ["question"],
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// `tools` with `explore` last.
pub(crate) fn with_explore(mut tools: Vec<Value>) -> Vec<Value> {
    tools.push(definition());
    tools
}

/// The sub-agent's tools: the reads of [`READS`], `find_symbol` only with `memory`.
fn reads(memory: bool) -> Vec<Value> {
    READS
        .iter()
        .filter(|b| memory || **b != Builtin::FindSymbol)
        .map(|b| registry::builtin(*b).decl.definition())
        .collect()
}

/// What one `explore` call came to.
pub(crate) struct Explored {
    /// The tool result for the attempt: the conclusion, or why there is none.
    pub text: String,
    pub is_error: bool,
    /// What the sub-agent spent, to count as the attempt's too.
    pub spend: Spend,
}

impl Explored {
    fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, spend: Spend::default() }
    }
}

/// Answer `call`, attempt `index`'s `explore` in its turn `turn`, from
/// `fork`. `memory`: the attempt is offered memory's tools, so the
/// sub-agent gets `find_symbol`. Stops when `cancel`, the attempt's, does.
pub(crate) async fn explore(
    ctx: &Arc<Ctx>,
    fork: &str,
    index: u32,
    turn: u32,
    call: &ToolUse,
    memory: bool,
    cancel: &CancellationToken,
) -> Explored {
    let question = match serde_json::from_value::<Explore>(call.input.clone()) {
        Ok(e) if !e.question.trim().is_empty() => e.question.trim().to_owned(),
        Ok(_) => return Explored::error("explore needs a question: say what you want to find out."),
        Err(e) => {
            return Explored::error(format!(
                "Invalid input for explore: {e}. Check the tool's input schema and try again."
            ))
        }
    };
    let detail = format!("explore {}", flat(&question, 160));
    ctx.progress(Progress::ToolCall { run: ctx.run_id(), attempt: index, turn, tool: t::EXPLORE.into(), detail }).await;
    let seat = ctx.seats().explorer;
    // A slice of what the attempt may still spend: the run's budget, under any cap on this part of it.
    let left = ctx.spend_cap.unwrap_or(f64::INFINITY).min(ctx.budget_usd) - ctx.tally().spend.cost_usd;
    let slice = seat.budget_share.unwrap_or(SHARE) * left;
    if slice.is_nan() || slice <= 0.0 || ctx.over_budget() {
        return Explored::error("explore is not available: the run's budget is spent.");
    }
    // Its own token, so the attempt's stopping stops it, and nothing it does stops the attempt.
    let token = cancel.child_token();
    let child = {
        let mut child = (**ctx).clone();
        child.cancel = token.clone();
        // Its turns are not the attempt's: they are not previewed as the attempt's text.
        child.stream = false;
        Arc::new(child)
    };
    let mut blocks: Vec<Value> = ctx.preamble().iter().map(|b| text_block(*b)).collect();
    blocks.push(text_block(format!("<question>\n{question}\n</question>")));
    let mut conv =
        Conversation::seated(&seat, EXPLORE_PART, reads(memory), user_blocks(blocks), Role::Explorer, Some(index));
    if conv.model.is_none() {
        conv.model = ctx.cfg.explore_model.clone();
    }
    let offered: Vec<String> = conv.tools.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    let max_turns = seat.max_turns.unwrap_or(MAX_TURNS);
    let mut meter = Meter::default();
    let ended = work(&child, fork, index, &mut conv, &offered, max_turns, slice, &mut meter, &token).await;
    let who = speaker(Some(Role::Explorer), Some(index));
    let spent = if meter.spend.unknown_calls > 0 {
        format!("${:.2} and {} calls of unknown cost", meter.spend.cost_usd, meter.spend.unknown_calls)
    } else {
        format!("${:.2}", meter.spend.cost_usd)
    };
    let (text, is_error) = match ended {
        Ok(conclusion) => {
            ctx.note(format!("{who}: concluded after {} turns ({spent})", meter.turns)).await;
            let text = format!("The explorer's conclusion:\n\n{}", clip(conclusion, MAX_CONCLUSION));
            (text, false)
        }
        Err(stop) => {
            let cancelled = matches!(stop, Stop::Cancelled);
            let why = match stop {
                Stop::Budget if ctx.over_budget() => "the run's budget is spent".to_owned(),
                Stop::Budget => format!("it spent its share of the budget (${slice:.2})"),
                Stop::OutOfTurns => format!("it used its {max_turns} turns"),
                Stop::Cancelled => "it was cancelled".to_owned(),
                Stop::Model(e) => format!("a model call failed: {}", e.message),
                Stop::Refused => "the model declined".to_owned(),
            };
            // A call it stopped waiting for runs on and is counted by the run, not here: no amount is known.
            let spent = if cancelled { String::new() } else { format!(" ({spent})") };
            ctx.note(format!("{who}: stopped without a conclusion: {why}{spent}")).await;
            let text =
                format!("explore stopped before it concluded: {why}. Ask a narrower question, or look it up yourself.");
            (text, true)
        }
    };
    tracing::info!(run = %ctx.trace, attempt = index, turns = meter.turns, is_error, "explored");
    Explored { text, is_error, spend: meter.spend }
}

/// The sub-agent's loop: its final reply, or why it stopped without one.
#[allow(clippy::too_many_arguments)] // One call site; a struct would only rename these.
async fn work(
    ctx: &Arc<Ctx>,
    fork: &str,
    index: u32,
    conv: &mut Conversation,
    offered: &[String],
    max_turns: u32,
    slice: f64,
    meter: &mut Meter,
    cancel: &CancellationToken,
) -> Result<String, Stop> {
    let mut reply: Vec<String> = Vec::new();
    let mut asked = false;
    loop {
        // Unknown is never zero: a call whose cost is not known spends the slice.
        if meter.spend.cost_usd >= slice || meter.spend.unknown_calls > 0 {
            return Err(Stop::Budget);
        }
        let resp = agent::turn(ctx, conv, max_turns, meter, cancel).await?;
        let calls = resp.tool_uses();
        if !calls.is_empty() {
            reply.clear();
            asked = false;
            let cut_off = agent::cut_off(&resp);
            // Only the reads it was offered run; anything else is refused unrun, whatever its name.
            let allowed = |c: &ToolUse| offered.contains(&c.name);
            let reads: Vec<ToolUse> = calls.iter().filter(|c| allowed(c)).cloned().collect();
            let actor = Actor::Explorer { index };
            let mut ran = tools::execute(ctx, fork, &reads, cut_off, actor).await.into_iter();
            let results = calls
                .iter()
                .map(|c| match allowed(c) {
                    true => ran.next().unwrap_or_else(|| tool_result(&c.id, "The tool failed unexpectedly.", true)),
                    false => {
                        let why = format!("{} is not available: explore only reads, and changes nothing.", c.name);
                        tool_result(&c.id, why, true)
                    }
                })
                .collect();
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
        return Ok(reply.join("\n\n"));
    }
}

/// `s` on one line, at most `max` bytes.
fn flat(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = prompts::head(&flat, max);
    if cut.len() < flat.len() {
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `explore` definition as the model API gets it, one line of JSON.
    const PINNED: &str = include_str!("testdata/explore_tool.txt");

    #[test]
    fn the_explore_tool_is_pinned_and_held_to_its_schema() {
        let def = definition();
        assert_eq!(serde_json::to_string(&def).unwrap(), PINNED.trim_end());
        assert_eq!(def["strict"], true);
        assert_eq!(def["input_schema"]["additionalProperties"], false);
        // Appended last, it leaves the tools before it as they were.
        let before = vec![registry::builtin(Builtin::ReadFile).decl.definition()];
        let after = with_explore(before.clone());
        assert_eq!((&after[..1], &after[1]["name"]), (&before[..], &json!(t::EXPLORE)));
    }

    #[test]
    fn the_explorer_is_offered_only_reads() {
        let names =
            |memory| -> Vec<String> { reads(memory).iter().map(|d| d["name"].as_str().unwrap().to_owned()).collect() };
        assert_eq!(names(true), [t::READ_FILE, t::LIST_FILES, t::SEARCH, t::FIND_SYMBOL]);
        assert_eq!(names(false), [t::READ_FILE, t::LIST_FILES, t::SEARCH]);
        for b in READS {
            assert!(registry::builtin(b).decl.read_only, "{b:?}");
        }
    }
}
