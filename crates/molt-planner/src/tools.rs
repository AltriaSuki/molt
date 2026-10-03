//! Run the model's tool calls in a fork: each call becomes one `fs` or
//! `shell` request, and its outcome a `tool_result` block.

use std::sync::Arc;

use molt_api::fs::{self, EntryKind};
use molt_api::model::{tool_result, ToolUse};
use molt_api::planner::tools as t;
use molt_api::progress::Progress;
use molt_api::shell;
use molt_proto::{Budget, ErrorCode, RemoteError};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::ctx::{Ctx, FILE_MS, SHELL_GRACE_MS};
use crate::prompts::{self, clip};

/// Largest tool result sent back to the model; longer ones lose their middle.
const MAX_RESULT: usize = 50_000;
const RUN_TIMEOUT_S: u64 = 120;
const MAX_RUN_TIMEOUT_S: u64 = 1800;

/// Who made the calls, for progress events.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Actor {
    Designer,
    Attempt { index: u32, turn: u32 },
}

#[derive(Debug)]
enum Tool {
    Read(t::ReadFile),
    Write(t::WriteFile),
    Edit(t::EditFile),
    List(t::ListFiles),
    Search(t::Search),
    Run(t::Run),
}

impl Tool {
    fn parse(call: &ToolUse) -> Result<Self, String> {
        fn input<T: DeserializeOwned>(call: &ToolUse) -> Result<T, String> {
            serde_json::from_value(call.input.clone()).map_err(|e| {
                format!("Invalid input for {}: {e}. Check the tool's input schema and try again.", call.name)
            })
        }
        Ok(match call.name.as_str() {
            t::READ_FILE => Self::Read(input(call)?),
            t::WRITE_FILE => Self::Write(input(call)?),
            t::EDIT_FILE => Self::Edit(input(call)?),
            t::LIST_FILES => Self::List(input(call)?),
            t::SEARCH => Self::Search(input(call)?),
            t::RUN => Self::Run(input(call)?),
            other => return Err(format!("There is no tool named {other:?}.")),
        })
    }

    /// Reads can run side by side; anything that changes the fork runs alone, in order.
    fn read_only(&self) -> bool {
        matches!(self, Self::Read(_) | Self::List(_) | Self::Search(_))
    }

    fn detail(&self) -> String {
        match self {
            Self::Read(r) => format!("read {}", r.path),
            Self::Write(w) => format!("write {}", w.path),
            Self::Edit(e) => format!("edit {}", e.path),
            Self::List(l) => format!("list {}", l.path.as_deref().unwrap_or(".")),
            Self::Search(s) => format!("search {}", s.pattern),
            Self::Run(r) => format!("run {}", r.command),
        }
    }

    /// The text for the model, or the error to report as one.
    async fn execute(self, ctx: &Ctx, fork: &str) -> Result<String, String> {
        let name = self.name();
        let edit = matches!(self, Self::Edit(_));
        self.call(ctx, fork).await.map_err(|e| {
            let mut msg = format!("{name} failed: {}", e.message);
            if edit && e.code == ErrorCode::Invalid {
                msg.push_str(". Read the file again and copy old_string exactly, with enough context to be unique.");
            }
            msg
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Read(_) => t::READ_FILE,
            Self::Write(_) => t::WRITE_FILE,
            Self::Edit(_) => t::EDIT_FILE,
            Self::List(_) => t::LIST_FILES,
            Self::Search(_) => t::SEARCH,
            Self::Run(_) => t::RUN,
        }
    }

    async fn call(self, ctx: &Ctx, fork: &str) -> Result<String, RemoteError> {
        let workspace = fork.to_owned();
        let files = Budget::new(0, FILE_MS, 0);
        match self {
            Self::Read(r) => Ok(show_read(ctx.read(fork, &r.path, r.offset, r.limit).await?)),
            Self::Write(w) => {
                let resp = ctx.write(fork, &w.path, &w.content).await?;
                let how = if resp.created { "created" } else { "replaced" };
                Ok(format!("Wrote {} bytes to {} ({how}).", resp.bytes, w.path))
            }
            Self::Edit(e) => {
                let req = fs::EditRequest {
                    workspace,
                    path: e.path.clone(),
                    old: e.old_string,
                    new: e.new_string,
                    replace_all: e.replace_all,
                };
                let resp: fs::EditResponse = ctx.call(fs::EDIT, req, files).await?;
                let s = if resp.replacements == 1 { "" } else { "s" };
                Ok(format!("Replaced {} occurrence{s} in {}.", resp.replacements, e.path))
            }
            Self::List(l) => {
                let req = fs::ListRequest { workspace, path: l.path, depth: l.depth };
                Ok(show_list(ctx.call(fs::LIST, req, files).await?))
            }
            Self::Search(s) => {
                let req = fs::SearchRequest {
                    workspace,
                    pattern: s.pattern,
                    path: s.path,
                    glob: s.glob,
                    case_insensitive: s.case_insensitive,
                    max_results: None,
                };
                Ok(show_search(ctx.call(fs::SEARCH, req, files).await?))
            }
            Self::Run(r) => {
                let timeout_s = r.timeout_s.unwrap_or(RUN_TIMEOUT_S).clamp(1, MAX_RUN_TIMEOUT_S);
                let resp = run_command(ctx, fork, &r.command, timeout_s * 1000).await?;
                Ok(show_run(&resp, timeout_s))
            }
        }
    }
}

/// `shell.run` in `workspace` with `timeout_ms`, waiting a little longer
/// than the command may take.
pub(crate) async fn run_command(
    ctx: &Ctx,
    workspace: &str,
    command: &str,
    timeout_ms: u64,
) -> Result<shell::RunResponse, RemoteError> {
    let req = shell::RunRequest {
        workspace: workspace.to_owned(),
        command: command.to_owned(),
        timeout_ms: Some(timeout_ms),
    };
    ctx.call(shell::RUN, req, Budget::new(0, timeout_ms.saturating_add(SHELL_GRACE_MS), 0)).await
}

/// Run every call of one assistant turn in `fork` and return their
/// `tool_result` blocks in the order of the calls, to go back in one user
/// message. Consecutive reads run concurrently; a call that changes the fork
/// waits for everything before it. `cut_off` names a call whose input the
/// output limit cut short: it is reported, not run.
pub(crate) async fn execute(
    ctx: &Arc<Ctx>,
    fork: &str,
    calls: &[ToolUse],
    cut_off: Option<&str>,
    actor: Actor,
) -> Vec<Value> {
    let mut results: Vec<Option<(String, bool)>> = vec![None; calls.len()];
    let mut reads = JoinSet::new();
    for (i, call) in calls.iter().enumerate() {
        let tool = if cut_off == Some(call.id.as_str()) { Err(prompts::CUT_OFF.to_owned()) } else { Tool::parse(call) };
        let detail = match &tool {
            Ok(tool) => tool.detail(),
            Err(_) => format!("{} (rejected)", call.name),
        };
        report(ctx, actor, &call.name, &detail).await;
        match tool {
            Err(msg) => results[i] = Some((msg, true)),
            Ok(tool) if tool.read_only() => {
                let (ctx, fork) = (ctx.clone(), fork.to_owned());
                reads.spawn(async move { (i, tool.execute(&ctx, &fork).await) });
            }
            Ok(tool) => {
                collect(&mut reads, &mut results).await;
                results[i] = Some(flatten(tool.execute(ctx, fork).await));
            }
        }
    }
    collect(&mut reads, &mut results).await;
    calls
        .iter()
        .zip(results)
        .map(|(call, result)| {
            let (text, is_error) = result.unwrap_or_else(|| ("The tool failed unexpectedly.".to_owned(), true));
            tool_result(&call.id, clip(text, MAX_RESULT), is_error)
        })
        .collect()
}

fn flatten(result: Result<String, String>) -> (String, bool) {
    match result {
        Ok(text) => (text, false),
        Err(text) => (text, true),
    }
}

async fn collect(reads: &mut JoinSet<(usize, Result<String, String>)>, results: &mut [Option<(String, bool)>]) {
    while let Some(joined) = reads.join_next().await {
        match joined {
            Ok((i, result)) => results[i] = Some(flatten(result)),
            Err(e) => tracing::warn!(error = %e, "a tool call task failed"),
        }
    }
}

async fn report(ctx: &Ctx, actor: Actor, tool: &str, detail: &str) {
    let detail = one_line(detail, 160);
    match actor {
        Actor::Attempt { index, turn } => {
            ctx.progress(Progress::ToolCall { run: ctx.run_id(), attempt: index, turn, tool: tool.to_owned(), detail })
                .await
        }
        Actor::Designer => ctx.note(format!("check designer: {detail}")).await,
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = prompts::head(&flat, max);
    if cut.len() < flat.len() {
        format!("{cut}…")
    } else {
        flat
    }
}

fn show_read(r: fs::ReadResponse) -> String {
    if r.total_lines == 0 {
        return "(empty file)".to_owned();
    }
    let mut out = r.content;
    if r.truncated || r.first_line > 1 {
        let last = r.first_line + r.lines.saturating_sub(1);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[lines {}-{last} of {}", r.first_line, r.total_lines));
        if r.truncated {
            out.push_str(&format!("; read on with offset {}", last + 1));
        }
        out.push(']');
    }
    out
}

fn show_list(l: fs::ListResponse) -> String {
    let mut out: Vec<String> = l
        .entries
        .iter()
        .map(|e| match e.kind {
            EntryKind::Dir => format!("{}/", e.path),
            EntryKind::Symlink => format!("{} (link)", e.path),
            EntryKind::File => format!("{} ({} bytes)", e.path, e.size),
        })
        .collect();
    if out.is_empty() {
        out.push("(no entries)".to_owned());
    }
    if l.truncated {
        out.push("[list cut short: list a subdirectory or use a smaller depth]".to_owned());
    }
    out.join("\n")
}

fn show_search(s: fs::SearchResponse) -> String {
    let mut out: Vec<String> = s.matches.iter().map(|m| format!("{}:{}: {}", m.path, m.line, m.text)).collect();
    if out.is_empty() {
        out.push("No matches.".to_owned());
    }
    if s.truncated {
        out.push("[more matches not shown: narrow the pattern, path or glob]".to_owned());
    }
    out.join("\n")
}

fn show_run(r: &shell::RunResponse, timeout_s: u64) -> String {
    let mut out = if r.timed_out {
        format!("Timed out after {timeout_s} s and was killed.")
    } else {
        let ending = prompts::ending(r);
        let mut chars = ending.chars();
        chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
    };
    if r.truncated {
        out.push_str("\n[the output was cut in the middle]");
    }
    for (name, text) in [("stdout", &r.stdout), ("stderr", &r.stderr)] {
        if !text.is_empty() {
            out.push_str(&format!("\n{name}:\n{text}"));
        }
    }
    if r.stdout.is_empty() && r.stderr.is_empty() {
        out.push_str("\n(no output)");
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(name: &str, input: Value) -> ToolUse {
        ToolUse { id: "toolu_1".into(), name: name.into(), input }
    }

    #[test]
    fn parses_inputs_and_rejects_bad_ones() {
        let edit = json!({ "path": "a", "old_string": "x", "new_string": "y" });
        assert!(matches!(Tool::parse(&call("edit_file", edit)), Ok(Tool::Edit(e)) if !e.replace_all));
        assert!(Tool::parse(&call("read_file", json!({ "file": "a" }))).unwrap_err().contains("Invalid input"));
        assert!(Tool::parse(&call("rm_rf", json!({}))).unwrap_err().contains("no tool named \"rm_rf\""));
        assert!(Tool::parse(&call("submit_check", json!({}))).is_err());
    }

    #[test]
    fn details_are_one_short_line() {
        let run = Tool::parse(&call("run", json!({ "command": "cargo test\n  --all" }))).unwrap();
        assert_eq!(one_line(&run.detail(), 160), "run cargo test --all");
        assert_eq!(one_line(&"x".repeat(500), 10), format!("{}…", "x".repeat(10)));
        let read = Tool::parse(&call("read_file", json!({ "path": "src/lib.rs" }))).unwrap();
        assert_eq!(read.detail(), "read src/lib.rs");
        assert!(read.read_only());
    }

    #[test]
    fn results_say_how_to_read_on() {
        let page =
            fs::ReadResponse { content: "a\nb\n".into(), first_line: 3, lines: 2, total_lines: 9, truncated: true };
        assert_eq!(show_read(page), "a\nb\n[lines 3-4 of 9; read on with offset 5]");
        let whole = fs::ReadResponse { content: "a".into(), first_line: 1, lines: 1, total_lines: 1, truncated: false };
        assert_eq!(show_read(whole), "a");
        let run = shell::RunResponse {
            exit_code: Some(2),
            signal: None,
            timed_out: false,
            stdout: "out\n".into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 3,
        };
        assert_eq!(show_run(&run, 120), "Exit code 2\nstdout:\nout\n");
    }
}
