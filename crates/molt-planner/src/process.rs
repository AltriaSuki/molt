//! Processes (#110): an attempt starts a command that runs on in the
//! background, such as a dev server, reads what it prints as it goes,
//! writes to its input and stops it, through `shell.start`, `shell.read`,
//! `shell.write` and `shell.stop`.
//!
//! The shell service runs a process as it runs `run`'s commands, in the
//! same sandbox, and keeps it to the attempt's fork: the planner holds no
//! more than what `run` already gives it. A process lives only while the
//! model works: every process of an attempt is stopped when the model gives
//! its final reply, before the work is checked, and when the attempt ends,
//! whatever ended it. Off unless the planner's starter turns it on
//! (`MOLT_PROCESSES=on`), so the built-in tool lists stay as they were.

use molt_api::model::ToolUse;
use molt_api::planner::tools::{self as t, ReadProcess, StartProcess, StopProcess, WriteProcess};
use molt_api::shell::{self, ProcessState, ProcessStatus, StopResponse};
use molt_proto::{Budget, RemoteError};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::prompts;

/// How long `start_process` waits for its ready text when the model says nothing.
const READY_S: u64 = 30;
/// The longest it may wait: what the shell service allows.
const MAX_READY_S: u64 = 120;
/// The longest `read_process` may wait for output.
const MAX_WAIT_S: u64 = 30;
/// Deadline of a call beyond its own wait: a stop waits out its process's grace.
const CALL_MS: u64 = 10_000;

/// The tools' definitions, offered to attempts after the other built-ins.
pub(crate) fn definitions() -> Vec<Value> {
    let id = json!({ "type": "string", "description": "The id start_process returned." });
    vec![
        json!({
            "name": t::START_PROCESS,
            "description": "Start a command that keeps running in the background while you go on working, such as \
                            a dev server, a file watcher or an interactive program, and return its id and what it \
                            printed so far. It runs in your workspace as run's commands do, under the same limits. \
                            Use run for a command that finishes. Give ready, text the program prints once it is \
                            up such as \"Listening on\", to wait for it: the result then says whether it appeared or \
                            the command exited first. Read its output with read_process, give it input with \
                            write_process, and stop it with stop_process once you are done with it. Every process \
                            is stopped when you give your final reply, before your work is checked, and when your \
                            attempt ends; one runs for at most 30 minutes, and at most 4 run at once. Its input and \
                            output are pipes, not a terminal: a program that buffers its output may need a flag \
                            such as python -u. A command run with run may not reach a process's ports, since each \
                            runs isolated: to talk to a server, start bash as the process and run the server and \
                            the client in it with write_process.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command, run with bash -c." },
                    "ready": {
                        "type": "string",
                        "description": "Text the program prints once it is ready, to wait for. Without it the \
                                        result comes after about a second.",
                    },
                    "timeout_s": {
                        "type": "integer",
                        "description": "Seconds to wait for ready. Default 30, at most 120.",
                    },
                },
                "required": ["command"],
                "additionalProperties": false,
            },
            "strict": true,
        }),
        json!({
            "name": t::READ_PROCESS,
            "description": "Read what a process printed since you last read it (its standard output and error \
                            together), and whether it is still running or how it ended. Very long output keeps \
                            its start and end.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "id": id,
                    "wait_s": {
                        "type": "integer",
                        "description": "Seconds to wait for output when there is none yet. Default 0, at most 30.",
                    },
                },
                "required": ["id"],
                "additionalProperties": false,
            },
            "strict": true,
        }),
        json!({
            "name": t::WRITE_PROCESS,
            "description": "Write text to a running process's standard input; end a line with \\n. Read its answer \
                            with read_process. Fails once the process has ended.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "id": id,
                    "input": { "type": "string", "description": "The text, at most 64 KiB." },
                    "close": {
                        "type": "boolean",
                        "description": "Close its input after the text, as at the end of a file. Default false.",
                    },
                },
                "required": ["id", "input"],
                "additionalProperties": false,
            },
            "strict": true,
        }),
        json!({
            "name": t::STOP_PROCESS,
            "description": "Stop a process with everything it started, and return what it printed since you last \
                            read it. Stopping one that already ended says how it ended.",
            "input_schema": {
                "type": "object",
                "properties": { "id": id },
                "required": ["id"],
                "additionalProperties": false,
            },
            "strict": true,
        }),
    ]
}

/// `tools` with the process tools after them.
pub(crate) fn with_processes(mut tools: Vec<Value>) -> Vec<Value> {
    tools.extend(definitions());
    tools
}

/// One call of a process tool.
#[derive(Debug)]
pub(crate) enum Call {
    Start(StartProcess),
    Read(ReadProcess),
    Write(WriteProcess),
    Stop(StopProcess),
}

impl Call {
    /// The call, when it names a process tool.
    pub(crate) fn parse(call: &ToolUse) -> Option<Result<Self, String>> {
        fn input<T: DeserializeOwned>(call: &ToolUse) -> Result<T, String> {
            serde_json::from_value(call.input.clone()).map_err(|e| {
                format!("Invalid input for {}: {e}. Check the tool's input schema and try again.", call.name)
            })
        }
        Some(match call.name.as_str() {
            t::START_PROCESS => input(call).map(Self::Start),
            t::READ_PROCESS => input(call).map(Self::Read),
            t::WRITE_PROCESS => input(call).map(Self::Write),
            t::STOP_PROCESS => input(call).map(Self::Stop),
            _ => return None,
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Start(_) => t::START_PROCESS,
            Self::Read(_) => t::READ_PROCESS,
            Self::Write(_) => t::WRITE_PROCESS,
            Self::Stop(_) => t::STOP_PROCESS,
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Start(s) => format!("start process {}", s.command),
            Self::Read(r) => format!("read process {}", r.id),
            Self::Write(w) => format!("write to process {}: {}", w.id, w.input),
            Self::Stop(s) => format!("stop process {}", s.id),
        }
    }

    /// Make the call in `fork`, and the text for the model.
    pub(crate) async fn run(self, ctx: &Ctx, fork: &str) -> Result<String, RemoteError> {
        let workspace = fork.to_owned();
        match self {
            Self::Start(s) => {
                let wait_s = s.timeout_s.unwrap_or(READY_S).clamp(1, MAX_READY_S);
                let ready = s.ready.filter(|r| !r.is_empty());
                // Without a ready text the service waits its own short while.
                let wait_ms = ready.as_ref().map(|_| wait_s * 1000);
                let req = shell::StartRequest { workspace, command: s.command, lifetime_ms: None, ready, wait_ms };
                let budget = Budget::new(0, wait_ms.unwrap_or(0) + CALL_MS, 0);
                let state: ProcessState = ctx.call(shell::START, req, budget).await?;
                Ok(format!("Started process {}.\n{}", state.id, show(&state)))
            }
            Self::Read(r) => {
                let wait_ms = r.wait_s.map(|s| s.min(MAX_WAIT_S) * 1000).filter(|&ms| ms > 0);
                let req = shell::ReadRequest { workspace, id: r.id, wait_ms };
                let budget = Budget::new(0, wait_ms.unwrap_or(0) + CALL_MS, 0);
                Ok(show(&ctx.call::<ProcessState>(shell::READ, req, budget).await?))
            }
            Self::Write(w) => {
                let req = shell::WriteRequest { workspace, id: w.id, input: w.input, close: w.close };
                let state: ProcessState = ctx.call(shell::WRITE, req, Budget::new(0, CALL_MS, 0)).await?;
                let closed = if w.close { " and closed its input" } else { "" };
                Ok(format!("Wrote the input{closed}.\n{}", show(&state)))
            }
            Self::Stop(s) => {
                let req = shell::StopRequest { workspace, id: Some(s.id) };
                let stopped: StopResponse = ctx.call(shell::STOP, req, Budget::new(0, CALL_MS, 0)).await?;
                Ok(stopped.stopped.iter().map(show).collect::<Vec<_>>().join("\n"))
            }
        }
    }
}

/// Stop every process of `fork`, which the shell service then forgets, and
/// say how many it had. Best effort: a stop that fails is logged, and the
/// service still stops each when its lifetime ends or the fork is dropped.
pub(crate) async fn stop_all(ctx: &Ctx, fork: &str) -> usize {
    let req = shell::StopRequest { workspace: fork.to_owned(), id: None };
    match ctx.call::<StopResponse>(shell::STOP, req, Budget::new(0, CALL_MS * 3, 0)).await {
        Ok(stopped) => stopped.stopped.len(),
        Err(e) => {
            tracing::warn!(run = %ctx.trace, fork, error = %e, "could not stop the attempt's processes");
            0
        }
    }
}

/// A process's state as the model reads it.
fn show(p: &ProcessState) -> String {
    let secs = p.elapsed_ms / 1000;
    let command = prompts::head(p.command.lines().next().unwrap_or(""), 80);
    let mut out = match p.status {
        ProcessStatus::Running => format!("Process {} (`{command}`) is running ({secs} s).", p.id),
        ProcessStatus::Exited => match (p.exit_code, p.signal) {
            (Some(code), _) => format!("Process {} (`{command}`) exited with code {code} after {secs} s.", p.id),
            (None, Some(signal)) => format!("Process {} (`{command}`) was killed by signal {signal}.", p.id),
            (None, None) => format!("Process {} (`{command}`) exited.", p.id),
        },
        ProcessStatus::Stopped => format!("Process {} (`{command}`) was stopped after {secs} s.", p.id),
        ProcessStatus::Expired => {
            format!("Process {} (`{command}`) ran as long as it may ({secs} s) and was stopped.", p.id)
        }
    };
    match p.ready {
        Some(true) => out.push_str(" Its ready text appeared."),
        Some(false) if p.status == ProcessStatus::Running => out.push_str(" Its ready text has not appeared yet."),
        Some(false) => out.push_str(" Its ready text never appeared."),
        None => {}
    }
    if p.omitted > 0 {
        out.push_str(&format!("\n[{} bytes of its output are not shown: they were cut or no longer kept]", p.omitted));
    }
    if p.output.is_empty() {
        out.push_str("\n(no new output)");
    } else {
        out.push_str(&format!("\nOutput (bytes {} to {}):\n{}", p.offset, p.next, p.output));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process tools' definitions as the model API gets them, one line of JSON each.
    const PINNED: &str = include_str!("testdata/process_tools.txt");

    #[test]
    fn the_process_tools_are_pinned_strict_and_closed() {
        let defs = definitions();
        let pinned: Vec<&str> = PINNED.lines().collect();
        assert_eq!(defs.len(), pinned.len());
        for (def, pinned) in defs.iter().zip(pinned) {
            assert_eq!(serde_json::to_string(def).unwrap(), pinned);
            assert_eq!(def["strict"], true);
            assert_eq!(def["input_schema"]["additionalProperties"], false);
        }
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, [t::START_PROCESS, t::READ_PROCESS, t::WRITE_PROCESS, t::STOP_PROCESS]);
        // Appended, they leave the tools before them as they were.
        let before = vec![json!({ "name": "read_file" })];
        assert_eq!(with_processes(before.clone())[..1], before[..]);
    }

    #[test]
    fn a_state_says_how_the_process_stands_and_what_it_printed() {
        let state = ProcessState {
            id: "proc_1".into(),
            command: "npm run dev\nmore".into(),
            status: ProcessStatus::Running,
            exit_code: None,
            signal: None,
            ready: Some(true),
            output: "ready on :3000\n".into(),
            offset: 10,
            next: 25,
            omitted: 0,
            elapsed_ms: 2500,
        };
        assert_eq!(
            show(&state),
            "Process proc_1 (`npm run dev`) is running (2 s). Its ready text appeared.\nOutput (bytes 10 to 25):\n\
             ready on :3000\n"
        );
        let exited = ProcessState {
            status: ProcessStatus::Exited,
            exit_code: Some(1),
            ready: Some(false),
            output: String::new(),
            omitted: 7,
            ..state
        };
        assert_eq!(
            show(&exited),
            "Process proc_1 (`npm run dev`) exited with code 1 after 2 s. Its ready text never appeared.\n[7 bytes of \
             its output are not shown: they were cut or no longer kept]\n(no new output)"
        );
    }
}
