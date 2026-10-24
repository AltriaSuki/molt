//! Lifecycle hooks: commands from trusted host configuration, never a run,
//! that hear about the run's events and may decide on some of them.
//!
//! A hook gets the event as one JSON object on its standard input and may
//! answer on standard output: `allow`, `deny` with a reason, `modify` with
//! a new tool input or prompt, or `annotate` with context for the model
//! (see [`answer`]). Which events a hook can stop or change is in
//! [`Event`]; every hook runs through the shell service and its sandbox,
//! and each decision is noted on the run's progress.
use crate::{attempt::Check, control, ctx::Ctx, prompts, tools};
use molt_api::shell::{self, RunResponse};
use molt_proto::Budget;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;

/// When a hook runs. The names before #165 (`before_apply`, `after_apply`,
/// `run_finished`, `attempt_failed`) still read, as do the Claude Code
/// names of the events that mean the same here; the variants of the last
/// three keep their names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// A run starts, or resumes, once its context is gathered.
    #[serde(rename = "run.start")]
    RunStart,
    /// A new run's task, before any model sees it: deny, modify or annotate.
    #[serde(rename = "prompt.submit", alias = "UserPromptSubmit")]
    PromptSubmit,
    /// Before a tool call of the model's: deny, modify or annotate.
    #[serde(rename = "tool.before", alias = "PreToolUse")]
    ToolBefore,
    /// After a tool call of the model's: deny (the result becomes an error) or annotate.
    #[serde(rename = "tool.after", alias = "PostToolUse")]
    ToolAfter,
    /// After an attempt ran the done-check: deny (a pass does not count) or annotate.
    #[serde(rename = "check.after")]
    CheckAfter,
    /// Each passing result, in its own fork, before ranking: may change it
    /// (a formatter), and deny drops it. Formerly `before_apply`.
    #[serde(rename = "result.ready", alias = "before_apply")]
    ResultReady,
    /// The chosen result, just before it is merged: deny keeps it in its fork.
    #[serde(rename = "apply.before")]
    ApplyBefore,
    #[serde(rename = "apply.after", alias = "after_apply")]
    AfterApply,
    /// The planner finished the run.
    #[serde(rename = "run.end", alias = "run_finished")]
    RunFinished,
    /// Memory learned from a finished run.
    #[serde(rename = "learn.after")]
    LearnAfter,
    #[serde(rename = "attempt.failed", alias = "attempt_failed")]
    AttemptFailed,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Self::RunStart => "run.start",
            Self::PromptSubmit => "prompt.submit",
            Self::ToolBefore => "tool.before",
            Self::ToolAfter => "tool.after",
            Self::CheckAfter => "check.after",
            Self::ResultReady => "result.ready",
            Self::ApplyBefore => "apply.before",
            Self::AfterApply => "apply.after",
            Self::RunFinished => "run.end",
            Self::LearnAfter => "learn.after",
            Self::AttemptFailed => "attempt.failed",
        }
    }

    /// A hook's `deny` stops what the event is about.
    pub fn can_deny(self) -> bool {
        matches!(
            self,
            Self::PromptSubmit
                | Self::ToolBefore
                | Self::ToolAfter
                | Self::CheckAfter
                | Self::ResultReady
                | Self::ApplyBefore
        )
    }

    /// The event is about a tool call, which `matcher` matches by name.
    pub fn is_tool(self) -> bool {
        matches!(self, Self::ToolBefore | Self::ToolAfter)
    }

    /// The event has paths for `paths` to match: the call's, or the result's changes.
    pub fn has_paths(self) -> bool {
        matches!(self, Self::ToolBefore | Self::ToolAfter | Self::ResultReady | Self::ApplyBefore | Self::AfterApply)
    }

    /// Where a hook's `context` goes: the model, or the run's summary.
    fn reads_context(self) -> bool {
        matches!(self, Self::PromptSubmit | Self::ToolBefore | Self::ToolAfter | Self::CheckAfter | Self::ApplyBefore)
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub event: Event,
    pub command: String,
    /// A hook that fails (cannot run, times out, exits other than 0 or 2,
    /// or answers what cannot be read) denies, rather than being passed over.
    #[serde(default)]
    pub blocking: bool,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    /// Tool events only: a regular expression the whole tool name must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Globs (`*`, `**`, `?`) over workspace paths: the hook runs only when
    /// one of the event's paths matches one of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}
fn timeout() -> u64 {
    30_000
}

impl Hook {
    /// A hook for `event` that runs `command`, with the defaults.
    pub fn new(event: Event, command: impl Into<String>) -> Self {
        Self {
            event,
            command: command.into(),
            blocking: false,
            timeout_ms: timeout(),
            matcher: None,
            paths: Vec::new(),
        }
    }

    /// The hook is for a call of `tool` (on a tool event) touching `paths`.
    pub fn matches(&self, tool: Option<&str>, paths: &[String]) -> bool {
        let named = match (self.matcher.as_deref().filter(|m| !matches!(*m, "" | "*")), tool) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(m), Some(tool)) => name_regex(m).is_ok_and(|re| re.is_match(tool)),
        };
        let globs: Vec<Regex> = self.paths.iter().filter_map(|g| glob(g).ok()).collect();
        let pathed = self.paths.is_empty() || paths.iter().any(|p| globs.iter().any(|g| g.is_match(&normalize(p))));
        named && pathed
    }
}

fn name_regex(matcher: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!("^(?:{matcher})$"))
}

/// `pattern` as an anchored regex over a workspace path: `*` and `?` stay
/// within one directory, `**` crosses them, and `**/` may match nothing.
fn glob(pattern: &str) -> Result<Regex, regex::Error> {
    let mut re = String::from("^");
    let mut chars = normalize(pattern).chars().collect::<Vec<_>>().into_iter().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    re.push('$');
    Regex::new(&re)
}

/// `path` as a glob sees it: without `.` and empty parts, `..` taken back
/// lexically, so `./migrations//a.sql` and `src/../migrations/a.sql` are
/// both `migrations/a.sql`. (The fs service refuses `..` and absolute paths.)
pub fn normalize(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// The paths a tool's input names: its `path`, and its `paths` when that is a list.
pub fn input_paths(input: &Value) -> Vec<String> {
    let mut out: Vec<String> = input.get("path").and_then(Value::as_str).map(str::to_owned).into_iter().collect();
    if let Some(list) = input.get("paths").and_then(Value::as_array) {
        out.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    out
}

pub fn validate(hooks: &[Hook]) -> anyhow::Result<()> {
    anyhow::ensure!(hooks.len() <= 32, "at most 32 lifecycle hooks");
    for h in hooks {
        let event = h.event;
        anyhow::ensure!(
            !h.command.trim().is_empty() && h.command.len() <= 8192,
            "hook command must be 1 to 8192 bytes"
        );
        anyhow::ensure!((1..=600_000).contains(&h.timeout_ms), "hook timeout_ms must be 1 to 600000");
        anyhow::ensure!(
            !h.blocking || event.can_deny(),
            "a {event} hook cannot block: only prompt.submit, tool.before, tool.after, check.after, result.ready \
             (before_apply) and apply.before hooks may"
        );
        if let Some(matcher) = &h.matcher {
            anyhow::ensure!(event.is_tool(), "a {event} hook has no tool to match: leave out matcher");
            anyhow::ensure!(matcher.len() <= 1024, "hook matcher must be at most 1024 bytes");
            if !matches!(matcher.as_str(), "" | "*") {
                name_regex(matcher).map_err(|e| anyhow::anyhow!("hook matcher {matcher:?} is not a regex: {e}"))?;
            }
        }
        if !h.paths.is_empty() {
            anyhow::ensure!(event.has_paths(), "a {event} hook has no paths to match: leave out paths");
            anyhow::ensure!(h.paths.len() <= 64, "a hook has at most 64 paths");
            for path in &h.paths {
                anyhow::ensure!(!path.trim().is_empty() && path.len() <= 512, "hook paths must be 1 to 512 bytes");
                glob(path).map_err(|e| anyhow::anyhow!("hook path {path:?} is not a glob: {e}"))?;
            }
        }
    }
    Ok(())
}

/// What a hook answered, as read from how it ended and what it printed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    /// Why it denies.
    pub deny: Option<String>,
    /// The tool input to use in place of the model's.
    pub tool_input: Option<Value>,
    /// The task to use in place of the user's.
    pub prompt: Option<String>,
    /// More for the model to know.
    pub context: Option<String>,
    /// It failed, and why: a blocking hook's failure denies.
    pub failed: Option<String>,
}

/// The answer Claude Code's hooks give: `hookSpecificOutput`, and
/// `decision: block` (or `approve`), `continue: false` with `stopReason`.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Reply {
    decision: Option<String>,
    reason: Option<String>,
    context: Option<String>,
    tool_input: Option<Value>,
    prompt: Option<String>,
    #[serde(rename = "continue")]
    go_on: Option<bool>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "hookSpecificOutput")]
    specific: Option<Specific>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Specific {
    #[serde(rename = "permissionDecision")]
    permission_decision: Option<String>,
    #[serde(rename = "permissionDecisionReason")]
    permission_decision_reason: Option<String>,
    #[serde(rename = "updatedInput")]
    updated_input: Option<Value>,
    #[serde(rename = "additionalContext")]
    additional_context: Option<String>,
}

/// Longest reason or context taken from a hook.
const MAX_TEXT: usize = 8_000;

/// Read a hook's answer from how its command ended. Exit 0 with nothing,
/// or with output that is not a JSON object, allows; exit 0 with a JSON
/// object says what it decided; exit 2 denies with stderr as the reason
/// (as in Claude Code); anything else, a timeout or a cancel, is a failure.
pub fn answer(ran: &RunResponse) -> Answer {
    let failed = |why: String| Answer { failed: Some(why), ..Answer::default() };
    if ran.timed_out {
        return failed(format!("it timed out after {} ms", ran.duration_ms));
    }
    if ran.cancelled {
        return failed("it was stopped".into());
    }
    match ran.exit_code {
        Some(0) => {}
        Some(2) => {
            let reason = ran.stderr.trim();
            let reason = if reason.is_empty() { "it exited 2 without saying why" } else { reason };
            return Answer { deny: Some(prompts::clip(reason.to_owned(), MAX_TEXT)), ..Answer::default() };
        }
        _ => return failed(format!("it ended: {}", prompts::ending(ran))),
    }
    let out = ran.stdout.trim();
    if !out.starts_with('{') {
        return Answer::default();
    }
    let reply: Reply = match serde_json::from_str(out) {
        Ok(reply) => reply,
        Err(e) => return failed(format!("its answer is not one this reads: {e}")),
    };
    let specific = reply.specific.unwrap_or_default();
    let text = |s: Option<String>| s.map(|s| prompts::clip(s.trim().to_owned(), MAX_TEXT)).filter(|s| !s.is_empty());
    let reason = text(reply.reason.or(specific.permission_decision_reason));
    let mut answer = Answer {
        tool_input: reply.tool_input.or(specific.updated_input),
        prompt: reply.prompt,
        context: text(reply.context.or(specific.additional_context)),
        ..Answer::default()
    };
    let decision = reply.decision.or(specific.permission_decision);
    match decision.as_deref() {
        None | Some("allow" | "approve") => {}
        // There is no one to ask in the middle of a run.
        Some("deny" | "block" | "ask") => {
            answer.deny = Some(reason.clone().unwrap_or_else(|| "no reason given".into()))
        }
        Some("modify") if answer.tool_input.is_none() && answer.prompt.is_none() => {
            return failed("it answered modify without a tool_input or prompt".into())
        }
        Some("annotate") if answer.context.is_none() => return failed("it answered annotate without a context".into()),
        Some("modify" | "annotate") => {}
        Some(other) => {
            return failed(format!("it answered decision {other:?}; the answers are allow, deny, modify and annotate"))
        }
    }
    if reply.go_on == Some(false) && answer.deny.is_none() {
        answer.deny = Some(text(reply.stop_reason).or(reason).unwrap_or_else(|| "the hook stopped it".into()));
    }
    if answer.tool_input.as_ref().is_some_and(|input| !input.is_object()) {
        return failed("its tool_input is not an object".into());
    }
    answer
}

/// What the hooks of one event decided, in order: the first deny ends it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub denied: Option<String>,
    pub tool_input: Option<Value>,
    pub prompt: Option<String>,
    pub context: Vec<String>,
}

impl Decision {
    /// The context, for the model, after what it is added to.
    pub fn context_text(&self, event: Event) -> String {
        self.context.iter().map(|c| format!("\n\n[Context from the user's {event} hook]\n{c}")).collect()
    }
}

/// Where a hook runs.
#[derive(Clone, Copy, Debug)]
pub enum Place<'a> {
    /// In this fork, which it may change, as the model's own commands may.
    In(&'a str),
    /// In a disposable fork of this workspace or fork, dropped after.
    CopyOf(&'a str),
}

/// What a front end gives the hooks to run with: the shell, forks, and a
/// note for each decision (on the run's progress, which the audit records).
pub trait Host {
    fn shell(
        &self,
        workspace: &str,
        command: &str,
        stdin: String,
        timeout_ms: u64,
    ) -> impl Future<Output = Result<RunResponse, String>> + Send;
    fn fork(&self, from: &str) -> impl Future<Output = Result<String, String>> + Send;
    fn drop_fork(&self, fork: &str) -> impl Future<Output = ()> + Send;
    fn note(&self, message: String) -> impl Future<Output = ()> + Send;
}

/// The hooks of `event` that are for `tool` touching `paths`.
pub fn matching<'a>(hooks: &'a [Hook], event: Event, tool: Option<&'a str>, paths: &'a [String]) -> Vec<&'a Hook> {
    hooks.iter().filter(|h| h.event == event && h.matches(tool, paths)).collect()
}

/// Run the hooks of `event` that match, one after another in their order,
/// each with `payload` (and where it runs, as `cwd`, and what the hooks
/// before it changed) on its standard input. `tool` names the call a tool
/// event is about; `paths` are what `paths` globs match, which on a tool
/// event are taken again from an input a hook changed.
pub async fn decide<H: Host + Sync>(
    host: &H,
    hooks: &[Hook],
    event: Event,
    place: Place<'_>,
    tool: Option<&str>,
    paths: &[String],
    mut payload: Value,
) -> Decision {
    let mut decision = Decision::default();
    let mut paths = paths.to_vec();
    if matching(hooks, event, tool, &paths).is_empty() {
        return decision;
    }
    let (at, copy) = match place {
        Place::In(fork) => (fork.to_owned(), false),
        Place::CopyOf(from) => match host.fork(from).await {
            Ok(fork) => (fork, true),
            Err(e) => {
                host.note(format!("hook {event}: no private fork to run in: {e}")).await;
                if matching(hooks, event, tool, &paths).iter().any(|h| h.blocking) {
                    decision.denied = Some(format!("its blocking {event} hook could not run: {e}"));
                }
                return decision;
            }
        },
    };
    payload["hook_event_name"] = json!(event.name());
    payload["cwd"] = json!(at);
    for hook in hooks.iter().filter(|h| h.event == event) {
        if !hook.matches(tool, &paths) {
            continue;
        }
        let ran = host.shell(&at, &hook.command, payload.to_string(), hook.timeout_ms).await;
        let mut got = match &ran {
            Ok(ran) => answer(ran),
            Err(e) => Answer { failed: Some(format!("it could not run: {e}")), ..Answer::default() },
        };
        let mut ignored = Vec::new();
        let mut verdict = Vec::new();
        if let Some(why) = got.failed.take() {
            if hook.blocking {
                got.deny = Some(format!("the blocking hook failed: {why}"));
            } else {
                ignored.push(format!("it failed and does not block ({why})"));
            }
        }
        if let Some(reason) = got.deny.take_if(|_| !event.can_deny()) {
            ignored.push(format!("deny ({reason}), since a {event} hook cannot stop anything"));
        }
        if let Some(input) = got.tool_input.take() {
            if event == Event::ToolBefore {
                verdict.push(format!("modify the input to {}", prompts::head(&input.to_string(), 200)));
                paths = input_paths(&input);
                payload["paths"] = json!(paths);
                payload["tool_input"] = input.clone();
                decision.tool_input = Some(input);
            } else {
                ignored.push(format!("tool_input, since only a tool.before hook can change one; not {event}"));
            }
        }
        if let Some(prompt) = got.prompt.take() {
            if event == Event::PromptSubmit {
                verdict.push("modify the prompt".to_owned());
                payload["prompt"] = json!(prompt);
                decision.prompt = Some(prompt);
            } else {
                ignored.push(format!("prompt, since only a prompt.submit hook can change it; not {event}"));
            }
        }
        if let Some(context) = got.context.take() {
            if event.reads_context() {
                verdict.push(format!("annotate: {}", prompts::head(&context, 200)));
                decision.context.push(context);
            } else {
                ignored.push(format!("context, since nothing reads a {event} hook's context"));
            }
        }
        let verdict = match &got.deny {
            Some(reason) => format!("deny: {reason}"),
            None if verdict.is_empty() => "allow".to_owned(),
            None => verdict.join("; "),
        };
        let how = match &ran {
            Ok(r) => format!("exit {:?}, {} ms, timed_out={}", r.exit_code, r.duration_ms, r.timed_out),
            Err(_) => "did not run".to_owned(),
        };
        let about = tool.map(|t| format!(" {t}")).unwrap_or_default();
        let ignored = if ignored.is_empty() { String::new() } else { format!("; ignored {}", ignored.join("; ")) };
        host.note(format!(
            "hook {event}{about}: {verdict} by `{}` ({how}){ignored}",
            prompts::head(hook.command.lines().next().unwrap_or(""), 80)
        ))
        .await;
        if let Some(reason) = got.deny {
            decision.denied = Some(reason);
            break;
        }
    }
    if copy {
        host.drop_fork(&at).await;
    }
    decision
}

impl Host for Ctx {
    async fn shell(
        &self,
        workspace: &str,
        command: &str,
        stdin: String,
        timeout_ms: u64,
    ) -> Result<RunResponse, String> {
        let req = shell::RunRequest {
            workspace: workspace.to_owned(),
            command: command.to_owned(),
            timeout_ms: Some(timeout_ms),
            stream: false,
            stdin: Some(stdin),
        };
        let budget = Budget::new(0, timeout_ms.saturating_add(crate::ctx::SHELL_GRACE_MS), 0);
        self.call(shell::RUN, req, budget).await.map_err(|e| e.message)
    }

    async fn fork(&self, from: &str) -> Result<String, String> {
        self.fork_from(from).await.map_err(|e| e.message)
    }

    async fn drop_fork(&self, fork: &str) {
        Ctx::drop_fork(self, fork).await
    }

    async fn note(&self, message: String) {
        Ctx::note(self, message).await
    }
}

/// The fields every event of a run has.
fn about_run(ctx: &Ctx) -> Value {
    json!({ "run_id": ctx.run_id(), "workspace": ctx.workspace })
}

fn with(mut base: Value, more: Value) -> Value {
    if let (Some(base), Value::Object(more)) = (base.as_object_mut(), more) {
        base.extend(more);
    }
    base
}

/// Tell the hooks of a notification `event` in a disposable fork of the workspace.
pub(crate) async fn notify(ctx: &Ctx, event: Event) {
    notify_with(ctx, event, &[], json!({})).await
}

/// [`notify`], with `paths` for the hooks' globs, and `more` added to what they are told.
pub(crate) async fn notify_with(ctx: &Ctx, event: Event, paths: &[String], more: Value) {
    let payload = with(about_run(ctx), more);
    decide(ctx, &ctx.cfg.hooks, event, Place::CopyOf(&ctx.workspace), None, paths, payload).await;
}

/// The `prompt.submit` hooks on a new run's task: `Err` with the reason a
/// hook refused it, else the task they leave, with their context after it.
pub(crate) async fn submit(ctx: &Ctx) -> Result<String, String> {
    let payload = with(about_run(ctx), json!({ "prompt": ctx.task }));
    let decision =
        decide(ctx, &ctx.cfg.hooks, Event::PromptSubmit, Place::CopyOf(&ctx.workspace), None, &[], payload).await;
    if let Some(reason) = decision.denied {
        return Err(format!("a prompt.submit hook refused the task: {reason}"));
    }
    let task = decision.prompt.clone().unwrap_or_else(|| ctx.task.clone());
    Ok(format!("{task}{}", decision.context_text(Event::PromptSubmit)))
}

/// Where the hooks of a tool call in `fork` run: in the fork itself, but
/// never in the user's workspace, where an attempt that only reads works.
pub(crate) fn tool_place<'a>(ctx: &Ctx, fork: &'a str) -> Place<'a> {
    if fork == ctx.workspace {
        Place::CopyOf(fork)
    } else {
        Place::In(fork)
    }
}

/// Whether any hook is about tool calls, so a call can skip the work.
pub(crate) fn around_tools(ctx: &Ctx) -> bool {
    ctx.cfg.hooks.iter().any(|h| h.event.is_tool())
}

/// The fields of a tool event: who called which tool with what.
pub(crate) fn tool_payload(ctx: &Ctx, role: &str, attempt: Option<u32>, tool: &str, input: &Value) -> Value {
    with(
        about_run(ctx),
        json!({ "role": role, "attempt": attempt, "tool_name": tool, "tool_input": input, "paths": input_paths(input) }),
    )
}

/// The `check.after` hooks of attempt `index`'s check run `ran`, in its fork.
pub(crate) async fn checked(ctx: &Ctx, fork: &str, index: u32, command: &str, ran: &RunResponse) -> Decision {
    let more = json!({
        "attempt": index,
        "check": {
            "command": command,
            "passed": ran.success(),
            "exit_code": ran.exit_code,
            "timed_out": ran.timed_out,
            "output": prompts::output_tail(ran, 4000),
        },
    });
    decide(ctx, &ctx.cfg.hooks, Event::CheckAfter, Place::In(fork), None, &[], with(about_run(ctx), more)).await
}

/// The `apply.before` hooks of the result of attempt `index` in `fork`,
/// changing `paths`: `Err` with why one refused it, else their context.
pub(crate) async fn applying(ctx: &Ctx, fork: &str, index: u32, paths: &[String]) -> Result<String, String> {
    let more = json!({ "attempt": index, "fork": fork, "changes": paths });
    let payload = with(about_run(ctx), more);
    let decision = decide(ctx, &ctx.cfg.hooks, Event::ApplyBefore, Place::CopyOf(fork), None, paths, payload).await;
    match decision.denied {
        Some(reason) => Err(reason),
        None => Ok(decision.context.join("\n")),
    }
}

/// The `result.ready` hooks (formerly `before_apply`) of a passing result
/// in `fork`: they may change it, its check files are restored after,
/// protected paths are checked and acceptance runs again.
pub(crate) async fn prepare(ctx: &Arc<Ctx>, fork: &str, check: Option<&Check>, version: u32) -> Result<(), String> {
    if !ctx.cfg.hooks.iter().any(|h| h.event == Event::ResultReady) {
        return Ok(());
    }
    let check = check.ok_or("result.ready hooks require a verified result")?;
    let command = ctx.control.check(version).unwrap_or_else(|| check.command.clone());
    let changes: Vec<String> = match ctx.diff(fork).await {
        Ok(diff) => diff.changes.into_iter().map(|c| c.path).collect(),
        Err(e) => return Err(format!("could not list the result's changes for its hooks: {}", e.message)),
    };
    if matching(&ctx.cfg.hooks, Event::ResultReady, None, &changes).is_empty() {
        return Ok(());
    }
    let more = json!({ "fork": fork, "changes": changes, "check": { "command": command } });
    let decision =
        decide(&**ctx, &ctx.cfg.hooks, Event::ResultReady, Place::In(fork), None, &changes, with(about_run(ctx), more))
            .await;
    // Restore authoritative check files even when a blocking hook failed.
    for (path, content) in &check.files {
        ctx.write(fork, path, content)
            .await
            .map_err(|e| format!("could not restore check file {path}: {}", e.message))?;
    }
    if let Some(reason) = decision.denied {
        return Err(format!("a result.ready hook stopped it: {reason}"));
    }
    let diff = ctx.diff(fork).await.map_err(|e| e.message)?;
    let protected = ctx.control.protected(version);
    if let Some(path) = diff.changes.iter().map(|c| &c.path).find(|p| protected.iter().any(|r| control::covered(p, r)))
    {
        return Err(format!("a result.ready hook changed protected path {path}"));
    }
    let timeout = u64::try_from(ctx.cfg.check_timeout.as_millis()).unwrap_or(u64::MAX);
    let ran = tools::run_command(ctx, fork, &command, timeout).await.map_err(|e| e.message)?;
    ctx.note(format!("acceptance after result.ready hooks: exit {:?}, {} ms", ran.exit_code, ran.duration_ms)).await;
    if !ran.success() {
        return Err("acceptance failed after result.ready hooks; no changes were applied".into());
    }
    if ctx.run_cancel.is_cancelled() {
        return Err("run cancelled before application".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(code: i32, stdout: &str, stderr: &str) -> RunResponse {
        RunResponse {
            exit_code: Some(code),
            signal: None,
            timed_out: false,
            cancelled: false,
            stdout: stdout.into(),
            stderr: stderr.into(),
            truncated: false,
            duration_ms: 3,
            offline_failure: false,
        }
    }

    #[test]
    fn notification_hooks_cannot_block_and_unknown_fields_are_rejected() {
        let h = Hook { blocking: true, timeout_ms: 10, ..Hook::new(Event::AfterApply, "true") };
        assert!(validate(&[h]).is_err());
        assert!(
            serde_json::from_str::<Hook>(r#"{"event":"before_apply","command":"true","permissions":"all"}"#).is_err()
        );
        let blocking = Hook { blocking: true, ..Hook::new(Event::ToolBefore, "true") };
        assert!(validate(&[blocking]).is_ok());
    }

    #[test]
    fn the_event_names_before_165_still_read_as_the_events_they_became() {
        for (old, new) in [
            ("before_apply", Event::ResultReady),
            ("after_apply", Event::AfterApply),
            ("run_finished", Event::RunFinished),
            ("attempt_failed", Event::AttemptFailed),
            ("PreToolUse", Event::ToolBefore),
            ("tool.before", Event::ToolBefore),
            ("learn.after", Event::LearnAfter),
        ] {
            let hook: Hook = serde_json::from_value(json!({ "event": old, "command": "true" })).unwrap();
            assert_eq!(hook.event, new, "{old}");
        }
        let hook = Hook { matcher: Some("edit_file".into()), ..Hook::new(Event::ToolBefore, "true") };
        let text = serde_json::to_string(&hook).unwrap();
        assert!(text.contains(r#""event":"tool.before""#) && !text.contains("paths"), "{text}");
    }

    #[test]
    fn a_hook_matches_the_whole_tool_name_and_any_normalized_path_its_globs_cover() {
        let hook = Hook {
            matcher: Some("edit_file|write_file".into()),
            paths: vec!["migrations/**".into(), "**/*.sql".into()],
            ..Hook::new(Event::ToolBefore, "true")
        };
        let p = |s: &str| vec![s.to_owned()];
        assert!(hook.matches(Some("edit_file"), &p("migrations/001_init.rs")));
        assert!(hook.matches(Some("write_file"), &p("./migrations//sub/x")));
        assert!(hook.matches(Some("write_file"), &p("src/../migrations/x")));
        assert!(hook.matches(Some("write_file"), &p("schema.sql")));
        assert!(hook.matches(Some("write_file"), &p("db/schema.sql")));
        assert!(!hook.matches(Some("write_file"), &p("src/migrations.rs")));
        assert!(!hook.matches(Some("read_file"), &p("migrations/001_init.rs")), "the whole name must match");
        assert!(!hook.matches(Some("my_edit_file"), &p("migrations/001_init.rs")));
        assert!(!hook.matches(Some("edit_file"), &[]), "a call without a path is not under migrations/");
        assert!(!hook.matches(None, &p("migrations/x")));
        let any = Hook { matcher: Some("*".into()), ..Hook::new(Event::ToolBefore, "true") };
        assert!(any.matches(Some("run"), &[]));
        let one = Hook { paths: vec!["src/*.rs".into()], ..Hook::new(Event::ApplyBefore, "true") };
        assert!(one.matches(None, &p("src/lib.rs")) && !one.matches(None, &p("src/a/lib.rs")));
        assert_eq!(input_paths(&json!({ "path": "a", "paths": ["b", 3] })), ["a", "b"]);
    }

    #[test]
    fn matchers_and_paths_are_refused_where_the_event_has_none_or_they_do_not_parse() {
        for hook in [
            Hook { matcher: Some("edit_file".into()), ..Hook::new(Event::RunFinished, "true") },
            Hook { matcher: Some("(".into()), ..Hook::new(Event::ToolBefore, "true") },
            Hook { paths: vec!["src/**".into()], ..Hook::new(Event::PromptSubmit, "true") },
            Hook { paths: vec![" ".into()], ..Hook::new(Event::ToolBefore, "true") },
            Hook { blocking: true, ..Hook::new(Event::RunStart, "true") },
        ] {
            assert!(validate(std::slice::from_ref(&hook)).is_err(), "{hook:?}");
        }
    }

    #[test]
    fn answers_are_read_from_the_exit_status_and_a_json_object_on_stdout() {
        assert_eq!(answer(&ran(0, "", "")), Answer::default());
        assert_eq!(answer(&ran(0, "formatted 3 files\n", "")), Answer::default(), "plain output allows");
        let deny = answer(&ran(0, r#"{"decision":"deny","reason":"migrations are frozen"}"#, ""));
        assert_eq!(deny.deny.as_deref(), Some("migrations are frozen"));
        let blocked = answer(&ran(2, "", "no edits under migrations/\n"));
        assert_eq!(blocked.deny.as_deref(), Some("no edits under migrations/"));
        let annotate = answer(&ran(0, r#"{"decision":"annotate","context":"Run sqlx prepare after."}"#, ""));
        assert_eq!(annotate.context.as_deref(), Some("Run sqlx prepare after."));
        assert!(annotate.deny.is_none());
        let modify = answer(&ran(0, r#"{"decision":"modify","tool_input":{"path":"b"}}"#, ""));
        assert_eq!(modify.tool_input, Some(json!({ "path": "b" })));
        let claude = answer(&ran(
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"frozen","additionalContext":"see ADR 7"}}"#,
            "",
        ));
        assert_eq!((claude.deny.as_deref(), claude.context.as_deref()), (Some("frozen"), Some("see ADR 7")));
        assert_eq!(answer(&ran(0, r#"{"decision":"block","reason":"no"}"#, "")).deny.as_deref(), Some("no"));
        assert_eq!(answer(&ran(0, r#"{"continue":false,"stopReason":"halt"}"#, "")).deny.as_deref(), Some("halt"));
        for bad in [
            r#"{"decision":"maybe"}"#,
            r#"{"decision":"modify"}"#,
            r#"{"decision":"annotate"}"#,
            r#"{"decision":"modify","tool_input":"x"}"#,
            "{not json",
        ] {
            assert!(answer(&ran(0, bad, "")).failed.is_some(), "{bad}");
        }
        assert!(answer(&ran(1, "", "")).failed.is_some());
        assert!(answer(&RunResponse { timed_out: true, exit_code: None, ..ran(0, "", "") }).failed.is_some());
    }
}
