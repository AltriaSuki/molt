//! The check designer: an agent loop in a throwaway fork that settles on a
//! done-check, possibly writing new test files for it, before any attempt
//! starts.

use std::collections::HashMap;
use std::sync::Arc;

use molt_api::fs::ChangeKind;
use molt_api::model::{tool_result, user_blocks, user_text, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{SubmitCheck, SUBMIT_CHECK};
use molt_proto::RemoteError;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts;
use crate::tools::{self, Actor};

/// The designer's turn limit. The run's `max_turns` counts an attempt's
/// turns; a small one must not starve the designer.
const MAX_TURNS: u32 = 25;

pub(crate) enum Design {
    Check {
        command: String,
        /// Path and full contents of each file the check depends on.
        files: Vec<(String, String)>,
    },
    /// No automated check fits the task: the designer submitted none.
    Unverified(String),
    /// The designer could not do its work (model error, refusal, budget), or
    /// ran out of turns or stopped without submitting a check.
    Failed(String),
}

/// Design the done-check. Errors only when the workspace cannot be forked.
pub(crate) async fn design(ctx: &Arc<Ctx>) -> Result<Design, RemoteError> {
    let fork = ctx.fork().await?;
    let design = work(ctx, &fork).await;
    ctx.drop_fork(&fork).await;
    Ok(design)
}

async fn work(ctx: &Arc<Ctx>, fork: &str) -> Design {
    let tools = Arc::new(prompts::designer_tools(ctx.memory.up));
    let first = prompts::designer_first_message(&ctx.task, ctx.memory.context.as_deref());
    let mut conv = Conversation::new(prompts::DESIGNER_SYSTEM, tools, first);
    let mut meter = Meter::default();
    let never = CancellationToken::new();
    let mut nudged = false;
    loop {
        let resp = match agent::turn(ctx, &mut conv, MAX_TURNS, &mut meter, &never).await {
            Ok(resp) => resp,
            Err(Stop::OutOfTurns) => {
                return Design::Failed("the check designer ran out of turns without submitting a check".into())
            }
            Err(Stop::Budget) => return Design::Failed("budget exhausted".into()),
            Err(Stop::Model(e)) => return Design::Failed(format!("model call failed: {e}")),
            Err(Stop::Refused) => return Design::Failed("the model declined".into()),
            Err(Stop::Cancelled) => return Design::Failed("cancelled".into()),
        };
        let calls = resp.tool_uses();
        if calls.is_empty() {
            if resp.stop_reason.as_deref() == Some(STOP_MAX_TOKENS) {
                conv.push(user_text(prompts::CONTINUE));
            } else if nudged {
                return Design::Failed("the check designer stopped without submitting a check".into());
            } else {
                nudged = true;
                conv.push(user_text(prompts::NUDGE));
            }
            continue;
        }

        // The other calls run first: they may write the files the check names.
        let cut_off = agent::cut_off(&resp);
        let (submits, others): (Vec<ToolUse>, Vec<ToolUse>) =
            calls.iter().cloned().partition(|c| c.name == SUBMIT_CHECK);
        let mut other_results = tools::execute(ctx, fork, &others, cut_off, Actor::Designer).await.into_iter();
        let mut submit_results = Vec::new();
        for call in &submits {
            let checked = if cut_off == Some(call.id.as_str()) {
                Err(prompts::CUT_OFF.to_owned())
            } else {
                submitted(ctx, fork, call).await
            };
            match checked {
                Ok(design) => return design,
                Err(msg) => submit_results.push(tool_result(&call.id, msg, true)),
            }
        }
        let mut submit_results = submit_results.into_iter();
        let results: Vec<Value> = calls
            .iter()
            .map(|call| {
                let next = if call.name == SUBMIT_CHECK { submit_results.next() } else { other_results.next() };
                next.unwrap_or_else(|| tool_result(&call.id, "The tool failed unexpectedly.", true))
            })
            .collect();
        conv.push(user_blocks(results));
    }
}

/// Validate a submit_check call and read the files it names, or say what is wrong with it.
async fn submitted(ctx: &Ctx, fork: &str, call: &ToolUse) -> Result<Design, String> {
    let submit: SubmitCheck = serde_json::from_value(call.input.clone())
        .map_err(|e| format!("Invalid input for submit_check: {e}. Check the tool's input schema and try again."))?;
    let Some(command) = submit.command else { return Ok(Design::Unverified(submit.rationale)) };
    if command.trim().is_empty() {
        return Err("The command is empty. Give the done-check command, or null if no automated check fits.".into());
    }
    // Check files are restored before every check run, which would undo an attempt's work on an existing file.
    let changed: HashMap<String, ChangeKind> = if submit.files.is_empty() {
        HashMap::new()
    } else {
        let diff = ctx
            .diff(fork)
            .await
            .map_err(|e| format!("Could not list the files you created: {}. Try again.", e.message))?;
        diff.changes.into_iter().map(|c| (c.path, c.kind)).collect()
    };
    let mut files: Vec<(String, String)> = Vec::new();
    for path in &submit.files {
        let path = path.trim().trim_start_matches("./");
        if path.is_empty() || files.iter().any(|(p, _)| p == path) {
            continue;
        }
        let problem = match changed.get(path) {
            Some(ChangeKind::Added) => None,
            Some(_) => Some("it already exists in the workspace"),
            None => Some("it is not a file you created"),
        };
        if let Some(problem) = problem {
            return Err(format!(
                "Cannot use {path} as a check file: {problem}. Check files are restored before every check run, \
                 so an existing file among them would undo the other agents' work on it. Put what the check needs \
                 in new files (for a Rust crate, an integration test under tests/) and list only those."
            ));
        }
        let content = ctx.read_all(fork, path).await.map_err(|e| {
            format!(
                "Cannot use {path} as a check file: {}. List only files you wrote, relative to the workspace root.",
                e.message
            )
        })?;
        files.push((path.to_owned(), content));
    }
    Ok(Design::Check { command, files })
}
