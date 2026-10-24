//! An attempt's question to the person following an interactive run
//! (`ask_user`). The question goes out as a progress event, and the attempt
//! that asked waits for the answer while the others go on. The answer comes
//! back on `planner.control` as the run's next requirements version, so
//! every attempt follows it. Without an answer in time the attempt is told
//! to decide itself.

use molt_api::model::attempt_name;
use molt_api::planner::tools::AskUser;
use molt_api::progress::Progress;

use crate::ctx::Ctx;

/// Bytes of a question.
const MAX_QUESTION: usize = 2_000;
/// Options of a question, and bytes of each.
const MAX_OPTIONS: usize = 8;
const MAX_OPTION: usize = 200;

/// What the model is told when nobody answers, whatever the reason.
pub(crate) const NO_ANSWER: &str = "No answer: decide yourself, and say in your final reply what you decided and why.";

/// Ask the person `ask` for attempt `attempt`, and wait for the answer: the
/// tool's result, or the error to report as one.
pub(crate) async fn ask(ctx: &Ctx, attempt: u32, ask: AskUser) -> Result<String, String> {
    let question = ask.question.trim().to_owned();
    let options: Vec<String> = ask.options.iter().map(|o| o.trim().to_owned()).filter(|o| !o.is_empty()).collect();
    if question.is_empty() {
        return Err("ask_user needs a question.".into());
    }
    if question.len() > MAX_QUESTION {
        return Err(format!("The question is longer than {MAX_QUESTION} bytes: ask it in fewer words."));
    }
    if options.len() > MAX_OPTIONS || options.iter().any(|o| o.len() > MAX_OPTION) {
        return Err(format!("Give at most {MAX_OPTIONS} options, each under {MAX_OPTION} bytes."));
    }
    let id = match ctx.control.ask(ctx.cfg.max_questions) {
        Ok(id) => id,
        Err(why) => {
            ctx.note(format!("{} could not ask: {why}", attempt_name(attempt))).await;
            return Ok(format!("Your question was not asked: {why}. {NO_ANSWER}"));
        }
    };
    tracing::info!(run = %ctx.trace, attempt, question = id, "asked the user");
    ctx.progress(Progress::Asked { run: ctx.run_id(), attempt, question: id, text: question, options }).await;
    match ctx.control.answer(id, ctx.cfg.ask_timeout, &ctx.cancel).await {
        Some(answer) => {
            tracing::info!(run = %ctx.trace, attempt, question = id, version = answer.version, "the user answered");
            Ok(format!(
                "The user answered: {}\n\nThe answer is version {} of the task's requirements: every attempt follows \
                 it from now on.",
                answer.constraint.text, answer.version
            ))
        }
        None => {
            let secs = ctx.cfg.ask_timeout.as_secs();
            ctx.note(format!(
                "{}: no answer to question {id} within {secs}s; it decides itself",
                attempt_name(attempt)
            ))
            .await;
            Ok(NO_ANSWER.to_owned())
        }
    }
}
