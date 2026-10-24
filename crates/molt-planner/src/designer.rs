//! The check designer: an agent loop in a throwaway fork that settles on a
//! done-check, possibly writing new test files for it, before any attempt
//! starts.

use std::collections::HashMap;
use std::sync::Arc;

use molt_api::fs::{Change, ChangeKind};
use molt_api::model::{tool_result, user_blocks, user_text, Role, ToolUse, STOP_MAX_TOKENS};
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
/// as it is. The designer and the vetter together spend at most the run's
/// design budget, checked before each of their model calls as the run's
/// budget is. Errors only when the workspace cannot be forked.
pub(crate) async fn design(ctx: &Arc<Ctx>, baseline: bool) -> Result<Design, RemoteError> {
    let fork = ctx.fork().await?;
    let capped = ctx.capped(ctx.tally().spend.cost_usd + ctx.design_budget_usd);
    let mut design = work(&capped, &fork).await;
    note_overreach(ctx, &fork, &design).await;
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
    let tools = Arc::new(prompts::designer_tools(&ctx.tools, ctx.memory.up, ctx.web));
    let first = prompts::designer_first_message(&ctx.task, &ctx.preamble());
    let mut conv = Conversation::new(prompts::DESIGNER_SYSTEM, tools, first, Role::Designer, None);
    let mut meter = Meter::default();
    let mut nudged = false;
    let mut conflicted = 0;
    loop {
        let resp = match agent::turn(ctx, &mut conv, MAX_TURNS, &mut meter, &ctx.cancel).await {
            Ok(resp) => resp,
            Err(Stop::OutOfTurns) => {
                return Design::Failed("the check designer ran out of turns without submitting a check".into())
            }
            Err(Stop::Budget) if !ctx.over_budget() => return Design::Failed(out_of_share(ctx)),
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

/// Why the design failed when it used up its share of the budget: what it
/// spent, what the run has left, and what to do next.
fn out_of_share(ctx: &Ctx) -> String {
    let spent_total = ctx.tally().spend.cost_usd;
    let start = ctx.spend_cap.unwrap_or(spent_total) - ctx.design_budget_usd;
    format!(
        "the design budget ran out before the designer submitted a check: designing spent ${:.2} of the ${:.2} \
         it may use, and ${:.2} of the run's ${:.2} budget is left. {}",
        spent_total - start,
        ctx.design_budget_usd,
        (ctx.budget_usd - spent_total).max(0.0),
        ctx.budget_usd,
        molt_api::planner::DESIGN_BUDGET_NEXT,
    )
}

/// Tell the user, in the run's progress, about files the designer changed
/// in its fork beyond the check files it submitted, such as the task's own
/// deliverable: designing does not do the task, and that work is thrown
/// away with the fork. Best effort: a fork that cannot be listed says nothing.
async fn note_overreach(ctx: &Ctx, fork: &str, design: &Design) {
    let listed: Vec<&str> = match design {
        Design::Check { files, .. } => files.iter().map(|(path, _)| path.as_str()).collect(),
        _ => Vec::new(),
    };
    let changes = match ctx.diff(fork).await {
        Ok(diff) => diff.changes,
        Err(e) => {
            tracing::debug!(run = %ctx.trace, error = %e, "could not list what the designer changed");
            return;
        }
    };
    let beyond = overreach(&changes, &listed);
    if beyond.is_empty() {
        return;
    }
    tracing::warn!(run = %ctx.trace, files = ?beyond, "the check designer changed files beyond its check files");
    let n = beyond.len();
    let shown: Vec<&str> = beyond.iter().take(MAX_SHOWN).map(String::as_str).collect();
    let more = if n > MAX_SHOWN { format!(" and {} more", n - MAX_SHOWN) } else { String::new() };
    ctx.note(format!(
        "the check's designer changed {n} {} ({}{more}): designing does not do the task, so that work is \
         thrown away and only spent the budget",
        if n == 1 { "file that is not a check file" } else { "files that are not check files" },
        shown.join(", "),
    ))
    .await;
}

/// Most files beyond the check named in that note.
const MAX_SHOWN: usize = 5;

/// The paths in `changes` that are not among `listed`, in order.
fn overreach(changes: &[Change], listed: &[&str]) -> Vec<String> {
    changes.iter().filter(|c| !listed.contains(&c.path.as_str())).map(|c| c.path.clone()).collect()
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
        Ok(conflicts) if conflicts.is_empty() => ctx.note("vetter: the check fits the task").await,
        Ok(conflicts) => {
            let n = conflicts.len();
            ctx.note(format!("vetter: {n} conflict{} with the task; back to the designer", plural(n))).await;
            tracing::info!(run = %ctx.trace, conflicts = n, "the vetter sent the check back");
            return Vetted::Conflicts(conflicts);
        }
        Err(why) => {
            tracing::warn!(run = %ctx.trace, %why, "the check vetter did not finish; the check is used unvetted");
            ctx.note(format!("vetter: did not finish ({why}); the check is used without vetting")).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_the_designer_changed_beyond_its_check_files_are_overreach() {
        let change = |path: &str, kind| Change { path: path.into(), kind };
        let changes = [
            change("docs/errors.md", ChangeKind::Added),
            change("scripts/check_docs.py", ChangeKind::Added),
            change("src/lib.rs", ChangeKind::Modified),
            change("old.txt", ChangeKind::Deleted),
        ];
        assert_eq!(overreach(&changes, &["scripts/check_docs.py"]), ["docs/errors.md", "src/lib.rs", "old.txt"]);
        assert!(overreach(&changes[1..2], &["scripts/check_docs.py"]).is_empty());
        // A rubric or no check lists no files: any change is beyond it.
        assert_eq!(overreach(&changes[..1], &[]), ["docs/errors.md"]);
    }
}
