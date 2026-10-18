//! The check designer: an agent loop in a throwaway fork that settles on a
//! done-check, possibly writing new test files for it, before any attempt
//! starts.

use std::collections::HashMap;
use std::sync::Arc;

use molt_api::fs::ChangeKind;
use molt_api::model::{tool_result, user_blocks, user_text, ToolUse, STOP_MAX_TOKENS};
use molt_api::planner::tools::{Conflict, SubmitCheck, SUBMIT_CHECK};
use molt_api::planner::{Baseline, Criterion};
use molt_proto::RemoteError;
use serde_json::Value;

use crate::agent::{self, Meter, Stop};
use crate::ctx::{Conversation, Ctx};
use crate::prompts;
use crate::tools::{self, Actor};
use crate::vet;

/// The designer's turn limit. The run's `max_turns` counts an attempt's
/// turns; a small one must not starve the designer.
const MAX_TURNS: u32 = 25;
/// Most criteria in a rubric: each one is judged on every check round.
pub(crate) const MAX_CRITERIA: usize = 12;
/// Bytes of the baseline check's output kept.
const BASELINE_TAIL: usize = 2000;
/// Submissions the vetter may find in conflict with the task before the
/// design fails.
const MAX_CONFLICTED: u32 = 2;

pub(crate) enum Design {
    Check {
        command: String,
        /// Path and full contents of each file the check depends on.
        files: Vec<(String, String)>,
        /// The project's own test command, when the designer found one.
        suite: Option<String>,
        rationale: String,
        /// The check run once in the designer's fork, before any work:
        /// only when asked for, and `None` when it could not be run.
        baseline: Option<Baseline>,
    },
    /// No command checks the task; an independent judge decides these
    /// criteria instead.
    Rubric { criteria: Vec<Criterion>, rationale: String },
    /// No automated check fits the task: the designer submitted none.
    Unverified(String),
    /// The designer could not do its work (model error, refusal, budget), or
    /// ran out of turns or stopped without submitting a check.
    Failed(String),
}

/// Design the done-check, and with `baseline` run it once on the workspace
/// as it is. Errors only when the workspace cannot be forked.
pub(crate) async fn design(ctx: &Arc<Ctx>, baseline: bool) -> Result<Design, RemoteError> {
    let fork = ctx.fork().await?;
    let mut design = work(ctx, &fork).await;
    if let (true, Design::Check { command, files, baseline, .. }) = (baseline, &mut design) {
        *baseline = run_baseline(ctx, &fork, command, files).await;
    }
    ctx.drop_fork(&fork).await;
    Ok(design)
}

/// The check run in the designer's fork with its files as submitted (the
/// designer may have edited them after writing them) and the rest of the
/// project untouched by any attempt.
async fn run_baseline(ctx: &Arc<Ctx>, fork: &str, command: &str, files: &[(String, String)]) -> Option<Baseline> {
    for (path, content) in files {
        if let Err(e) = ctx.write(fork, path, content).await {
            tracing::warn!(run = %ctx.trace, path, error = %e, "could not restore a check file for the baseline");
            return None;
        }
    }
    let timeout_ms = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    match tools::run_command(ctx, fork, command, timeout_ms).await {
        Ok(ran) => Some(Baseline {
            passed: ran.success(),
            exit_code: ran.exit_code,
            tail: prompts::output_tail(&ran, BASELINE_TAIL),
        }),
        Err(e) => {
            tracing::warn!(run = %ctx.trace, error = %e, "could not run the baseline check");
            None
        }
    }
}

async fn work(ctx: &Arc<Ctx>, fork: &str) -> Design {
    let tools = Arc::new(prompts::designer_tools(ctx.memory.up, ctx.web));
    let first = prompts::designer_first_message(&ctx.task, &ctx.preamble());
    let mut conv = Conversation::new(prompts::DESIGNER_SYSTEM, tools, first, None);
    let mut meter = Meter::default();
    let mut nudged = false;
    let mut conflicted = 0;
    loop {
        let resp = match agent::turn(ctx, &mut conv, MAX_TURNS, &mut meter, &ctx.cancel).await {
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
            let design = match checked {
                Ok(design) => design,
                Err(msg) => {
                    submit_results.push(tool_result(&call.id, msg, true));
                    continue;
                }
            };
            match vetted(ctx, fork, design).await {
                Vetted::Accepted(design) => return design,
                Vetted::Conflicts(conflicts) => {
                    conflicted += 1;
                    if conflicted >= MAX_CONFLICTED {
                        return Design::Failed(format!(
                            "an independent vetter found the check in conflict with the task in {conflicted} \
                             submissions; the last one:{}",
                            prompts::conflict_lines(&conflicts)
                        ));
                    }
                    submit_results.push(tool_result(&call.id, prompts::check_conflicts(&conflicts), true));
                }
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

/// What the vetter made of a submitted design.
enum Vetted {
    Accepted(Design),
    /// The check conflicts with the task: it goes back to the designer.
    Conflicts(Vec<Conflict>),
}

/// Have a check with files of its own vetted against the task. A vetter
/// that does not finish holds nothing back: the check is used, and the
/// rationale says it was not vetted.
async fn vetted(ctx: &Arc<Ctx>, fork: &str, design: Design) -> Vetted {
    let Design::Check { command, files, suite, mut rationale, baseline } = design else {
        return Vetted::Accepted(design);
    };
    if files.is_empty() {
        return Vetted::Accepted(Design::Check { command, files, suite, rationale, baseline });
    }
    match vet::vet(ctx, fork, &command, &files).await {
        Ok(conflicts) if conflicts.is_empty() => ctx.note("check vetter: the check fits the task").await,
        Ok(conflicts) => {
            let n = conflicts.len();
            ctx.note(format!("check vetter: {n} conflict{} with the task; back to the designer", plural(n))).await;
            tracing::info!(run = %ctx.trace, conflicts = n, "the vetter sent the check back");
            return Vetted::Conflicts(conflicts);
        }
        Err(why) => {
            tracing::warn!(run = %ctx.trace, %why, "the check vetter did not finish; the check is used unvetted");
            ctx.note(format!("check vetter: did not finish ({why}); the check is used without vetting")).await;
            rationale =
                format!("{}\n\nNot vetted against the task: the vetter did not finish ({why}).", rationale.trim_end());
        }
    }
    Vetted::Accepted(Design::Check { command, files, suite, rationale, baseline })
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Validate a submit_check call and read the files it names, or say what is wrong with it.
async fn submitted(ctx: &Ctx, fork: &str, call: &ToolUse) -> Result<Design, String> {
    let submit: SubmitCheck = serde_json::from_value(call.input.clone())
        .map_err(|e| format!("Invalid input for submit_check: {e}. Check the tool's input schema and try again."))?;
    let Some(command) = submit.command else {
        return match rubric(submit.rubric)? {
            criteria if criteria.is_empty() => Ok(Design::Unverified(submit.rationale)),
            criteria => Ok(Design::Rubric { criteria, rationale: submit.rationale }),
        };
    };
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
    let suite = submit.suite.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    Ok(Design::Check { command, files, suite, rationale: submit.rationale, baseline: None })
}

/// The criteria a designer submitted, trimmed, or what is wrong with them.
fn rubric(criteria: Vec<Criterion>) -> Result<Vec<Criterion>, String> {
    let mut out: Vec<Criterion> = Vec::new();
    for c in criteria {
        let text = c.text.trim().to_owned();
        let command = c.command.map(|c| c.trim().to_owned()).filter(|c| !c.is_empty());
        if text.is_empty() {
            return Err("A criterion has no text. Say in one sentence what the result must satisfy.".into());
        }
        if !out.iter().any(|o| o.text == text) {
            out.push(Criterion { text, command });
        }
    }
    if out.len() > MAX_CRITERIA {
        return Err(format!(
            "The rubric has {} criteria; give at most {MAX_CRITERIA}, the ones that matter most.",
            out.len()
        ));
    }
    Ok(out)
}
