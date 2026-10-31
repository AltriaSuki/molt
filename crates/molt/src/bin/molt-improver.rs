//! The improver: a candidate service that proposes a new text for one of
//! the planner's prompts from how runs that used it failed
//! (`improver.propose`, `docs/improve.md`).
//!
//! It runs in the sandbox, as any service an agent may replace: it can reach
//! the kernel through its bus connection and nothing else, and holds only
//! the `model.complete` capability `molt improve` grants it, with a budget.
//! It is given the failures of the regression set only, never the held-out
//! tasks, and it answers with a text and why; whoever called it decides
//! what the text is tested on and whether it is promoted.

use std::sync::Arc;

use molt_api::improve::{Pattern, ProposeRequest, ProposeResponse, MAX_TEXT, PROPOSE, SUBMIT_PROMPT};
use molt_api::model::{self, CompleteRequest, CompleteResponse};
use molt_proto::{Budget, Envelope, ErrorCode, RemoteError, Target};
use molt_sdk::{CallOpts, Service};
use serde_json::{json, Value};

/// Model turns one proposal may take: the first, and one reminder to use the tool.
const TURNS: usize = 2;
const MAX_TOKENS: u32 = 16_000;
/// How long one model call may take.
const CALL_MS: u64 = 600_000;
/// The most of a case's evidence put in front of the model.
const EVIDENCE: usize = 6_000;

const SYSTEM: &str = "You improve one system prompt of Molt, a coding agent. You are given the prompt's \
current text and runs of coding tasks that failed while the agent spoke with it: what each task asked, what \
the agent changed, its last words, and the output of the hidden tests that graded it.

Find what the failures have in common that a change to the prompt could fix, such as a habit the prompt \
should forbid or a step it should require. Then write the whole new prompt: keep everything in the current \
text that is not part of the problem, and change or add as little as fixes it.

Keep it general. The new prompt is tested on tasks you have not seen, and on these, so never name these \
tasks, their files, their tests or their answers. Do not tell the agent to skip or weaken checks: a result \
is still checked and reviewed after it.

Hand the new text over with submit_prompt, with a short rationale.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let svc = Arc::new(Service::connect_from_env().await?);
    let caller = svc.clone();
    svc.serve_concurrent(1, move |req| {
        let svc = caller.clone();
        async move { handle(&svc, req).await }
    })
    .await;
    Ok(())
}

async fn handle(svc: &Service, req: Envelope) -> Result<Value, RemoteError> {
    let invalid = |message: String| RemoteError { code: ErrorCode::Invalid, message };
    match &req.to {
        Target::Method { method, .. } if PROPOSE.strip_prefix("improver.") == Some(method.as_str()) => {}
        other => return Err(invalid(format!("the improver has no method {other}"))),
    }
    let ask: ProposeRequest =
        serde_json::from_value(req.payload).map_err(|e| invalid(format!("not a propose request: {e}")))?;
    let proposal = propose(svc, &ask).await?;
    serde_json::to_value(proposal).map_err(|e| RemoteError { code: ErrorCode::Failed, message: e.to_string() })
}

async fn propose(svc: &Service, ask: &ProposeRequest) -> Result<ProposeResponse, RemoteError> {
    let failed = |message: String| RemoteError { code: ErrorCode::Failed, message };
    let tool = json!({
        "name": SUBMIT_PROMPT,
        "description": "Hand over the new text of the prompt, whole, and why it fixes the failures.",
        "input_schema": {
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The whole new prompt." },
                "rationale": { "type": "string", "description": "What the failures had in common, and how the text fixes it." }
            },
            "required": ["text", "rationale"]
        }
    });
    let mut messages = vec![json!({ "role": "user", "content": brief(ask) })];
    for _ in 0..TURNS {
        let req = CompleteRequest {
            model: ask.model.clone(),
            system: Some(SYSTEM.to_owned()),
            messages: messages.clone(),
            tools: vec![tool.clone()],
            max_tokens: Some(MAX_TOKENS),
            ..CompleteRequest::default()
        };
        let payload = serde_json::to_value(&req).map_err(|e| failed(e.to_string()))?;
        let opts = CallOpts { cap: None, budget: Budget::new(MAX_TOKENS.into(), CALL_MS, 0), trace: None };
        let reply =
            svc.call(model::COMPLETE, payload, opts).await.map_err(|e| failed(format!("model.complete: {e}")))?;
        let reply: CompleteResponse =
            serde_json::from_value(reply).map_err(|e| failed(format!("the model's reply: {e}")))?;
        let submitted = reply.content.iter().find(|b| b["type"] == "tool_use" && b["name"] == SUBMIT_PROMPT);
        if let Some(block) = submitted {
            let text = block["input"]["text"].as_str().unwrap_or_default().trim_end_matches(['\n', '\r']);
            let rationale = block["input"]["rationale"].as_str().unwrap_or_default();
            if text.trim().is_empty() || text.len() > MAX_TEXT {
                return Err(failed(format!("the proposed text is empty or larger than {} KiB", MAX_TEXT / 1024)));
            }
            return Ok(ProposeResponse { text: text.to_owned(), rationale: rationale.to_owned() });
        }
        messages.push(json!({ "role": "assistant", "content": reply.content }));
        messages.push(json!({ "role": "user", "content": "Hand the new text over with submit_prompt." }));
    }
    Err(failed("the model proposed no text".into()))
}

/// What the model is told: the current text, then each pattern with its cases.
fn brief(ask: &ProposeRequest) -> String {
    let mut out = format!(
        "The prompt is `{}`. Its current text:\n\n<current_prompt>\n{}\n</current_prompt>\n",
        ask.prompt, ask.current
    );
    for (i, Pattern { what, cases }) in ask.patterns.iter().enumerate() {
        out.push_str(&format!("\n<failure_pattern n=\"{}\" runs=\"{}\">\n{what}\n", i + 1, cases.len()));
        for case in cases {
            let evidence: String = case.evidence.chars().take(EVIDENCE).collect();
            out.push_str(&format!(
                "<run task=\"{}\">\n<asked>\n{}\n</asked>\n<evidence>\n{evidence}\n</evidence>\n</run>\n",
                case.task, case.ticket
            ));
        }
        out.push_str("</failure_pattern>\n");
    }
    out
}
