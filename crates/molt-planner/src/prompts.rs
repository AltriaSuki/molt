//! Everything the planner says to the model: system prompts, tool
//! definitions and the user turns it adds.
//!
//! The system prompts and tool lists are cached by the API and shared by
//! every attempt of every run, so they hold no per-run or per-attempt text.

use molt_api::model::{text_block, user_blocks};
use molt_api::planner::tools as t;
use molt_api::shell::RunResponse;
use serde_json::{json, Value};

pub(crate) const ATTEMPT_SYSTEM: &str = "\
You are a software engineer carrying out one task in a project, working on your own without anyone to ask. \
Make reasonable decisions where the task leaves room and mention them in your final reply.

Your workspace:
- It is a private copy of the user's workspace, without .git. Do not run git commands.
- Ignored dependency and build directories (such as target/ or node_modules/) are links to the user's originals. \
Use them, but never modify or delete anything inside them.
- Paths in tool calls are relative to the workspace root, and commands run there.

How to work:
- Use the tools to explore the project, change files and run commands.
- Read a file before you edit it, and change existing files with edit_file rather than rewriting them.
- Keep tool output small: read large files in windows with offset and limit, find code with search \
instead of reading whole trees, and filter long command output (for example with tail or grep).
- When you are done, stop calling tools and reply with a short summary of what you changed.

The done-check:
- When you stop calling tools, the task's done-check command runs in your workspace. If it fails, \
you get its output and continue.
- The check's files are restored to their original contents before every run, so editing them has no effect. \
Fix the code, not the check.";

pub(crate) const DESIGNER_SYSTEM: &str = "\
You design the automated done-check for a software task before anyone works on it. Other agents will then \
carry out the task, each in its own copy of the workspace, and their work is accepted only when the check passes.

The done-check is one shell command, run from the workspace root, that exits 0 exactly when the task is done \
and nonzero otherwise.

Your workspace:
- It is a private copy of the user's workspace, without .git. Do not run git commands.
- Ignored dependency and build directories (such as target/ or node_modules/) are links to the user's originals. \
Use them, but never modify or delete anything inside them.
- Paths in tool calls are relative to the workspace root, and commands run there.

How to design the check:
- Find out how the project builds and runs its tests, and what the task touches.
- Prefer the project's existing build and test commands, narrowed to what matters for the task \
(for example one package or one test file).
- When the task adds or changes behavior, write a focused new test or script that exercises exactly that \
behavior, where the project keeps its tests, and make the command run it. Test what the task asks for, \
not details it leaves open.
- Run the check. If the task is not done yet, confirm that the check fails, and fails for the right reason.
- The check must not need network access, and must be deterministic and reasonably fast.
- Do not implement the task itself. Only the files you list in submit_check reach the other agents; \
any other change you make is thrown away.
- If no automated check fits the task (a question to answer, prose to write), call submit_check with command null.

Finish by calling submit_check.";

/// Extra guidance for attempt `index`, so parallel attempts explore different
/// approaches. It goes in its own block after the task, keeping the shared
/// part of the first message cacheable.
pub(crate) fn hint(index: u32) -> Option<&'static str> {
    match index % 3 {
        1 => Some(
            "Approach: first run the done-check to see how it fails, then make the smallest change that makes it \
             pass without breaking anything else.",
        ),
        2 => Some(
            "Approach: read the relevant code thoroughly first, consider edge cases, and prefer a clean, \
             well-structured solution over a quick patch.",
        ),
        _ => None,
    }
}

pub(crate) fn attempt_first_message(task: &str, check: Option<(&str, &[String])>, index: u32) -> Value {
    let check = match check {
        Some((command, files)) => {
            let mut s = format!("The done-check is `{command}`. It runs from the workspace root and must exit 0.");
            if !files.is_empty() {
                s.push_str(&format!(
                    " It uses these files, which are already in your workspace and restored before every run: {}.",
                    files.join(", ")
                ));
            }
            s
        }
        None => "This task has no automated done-check. Your final reply is returned to the user as the result, \
                 so make it complete: the answer, or what you changed."
            .to_owned(),
    };
    let mut blocks = vec![text_block(format!("<task>\n{task}\n</task>\n\n{check}"))];
    if let Some(hint) = hint(index) {
        blocks.push(text_block(hint));
    }
    user_blocks(blocks)
}

pub(crate) fn designer_first_message(task: &str) -> Value {
    user_blocks(vec![text_block(format!("<task>\n{task}\n</task>\n\nDesign the done-check for this task."))])
}

pub(crate) const CONTINUE: &str = "Your reply was cut off at the output length limit. Continue where you left off. \
Write big files in smaller pieces: several smaller files, or a first part followed by edit_file.";

pub(crate) const CUT_OFF: &str = "This call was cut off because your reply reached the output length limit, \
so it was not run. Write big files in smaller pieces and try again.";

pub(crate) const NUDGE: &str = "You have not called submit_check. Call it now with the done-check command and the \
files it depends on, or with command null if no automated check fits this task.";

/// How a command ended, e.g. `exit code 1`.
pub(crate) fn ending(run: &RunResponse) -> String {
    if run.timed_out {
        "timed out".to_owned()
    } else if let Some(code) = run.exit_code {
        format!("exit code {code}")
    } else if let Some(signal) = run.signal {
        format!("killed by signal {signal}")
    } else {
        "killed".to_owned()
    }
}

/// The end of a command's output, at most about `max` bytes, stderr after stdout.
pub(crate) fn output_tail(run: &RunResponse, max: usize) -> String {
    let stderr = tail(&run.stderr, if run.stdout.is_empty() { max } else { max / 2 });
    let stdout = tail(&run.stdout, max - stderr.len());
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(&format!("stdout:\n{stdout}"));
    }
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("stderr:\n{stderr}"));
    }
    if out.is_empty() {
        out.push_str("(no output)");
    }
    out
}

pub(crate) fn check_failed(command: &str, run: &RunResponse) -> String {
    format!(
        "The done-check failed.\n\nCommand: `{command}`\nResult: {}\n\nEnd of its output:\n{}\n\n\
         Fix the problem, then reply when you are done. The check's files are restored before every run, \
         so change the code, not the check.",
        ending(run),
        output_tail(run, 4000),
    )
}

/// The last `max` bytes of `s` or fewer, cut at a character boundary.
pub(crate) fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// The first `max` bytes of `s` or fewer, cut at a character boundary.
pub(crate) fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` cut to about `max` bytes by dropping the middle.
pub(crate) fn clip(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let (head, tail) = (head(&s, max / 2), tail(&s, max / 2));
    let cut = s.len() - head.len() - tail.len();
    format!("{head}\n[... {cut} bytes cut ...]\n{tail}")
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// The tools of an attempt, in a fixed order.
pub(crate) fn attempt_tools() -> Vec<Value> {
    let path = json!({ "type": "string", "description": "Path relative to the workspace root." });
    vec![
        tool(
            t::READ_FILE,
            "Read a text file. Returns its lines; a long file is cut off with a note saying which lines were \
             returned and how to read on. Use offset and limit to read a window of a large file. Read a file \
             before editing it.",
            json!({
                "path": path,
                "offset": { "type": "integer", "description": "First line to read, 1-based. Default 1." },
                "limit": { "type": "integer", "description": "Most lines to read." },
            }),
            &["path"],
        ),
        tool(
            t::WRITE_FILE,
            "Create a file, or replace a file's whole contents; missing parent directories are created. Use it \
             for new files and complete rewrites, and edit_file for changes to an existing file. A very large \
             file can cut your reply off: write it in smaller pieces.",
            json!({
                "path": path,
                "content": { "type": "string", "description": "The complete new contents." },
            }),
            &["path", "content"],
        ),
        tool(
            t::EDIT_FILE,
            "Replace exact text in a file. old_string must match the file exactly, including whitespace and \
             indentation, and occur exactly once unless replace_all is true: include enough surrounding lines \
             to make it unique. Read the file first.",
            json!({
                "path": path,
                "old_string": { "type": "string", "description": "The exact text to replace." },
                "new_string": { "type": "string", "description": "The text to put in its place." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence. Default false." },
            }),
            &["path", "old_string", "new_string"],
        ),
        tool(
            t::LIST_FILES,
            "List files and directories, skipping ignored files and .git. Use it to get oriented in the project; \
             use search to find code.",
            json!({
                "path": { "type": "string", "description": "Directory relative to the workspace root. Default: the root." },
                "depth": { "type": "integer", "description": "How many levels to descend. Default 2." },
            }),
            &[],
        ),
        tool(
            t::SEARCH,
            "Search file contents with a regular expression (Rust regex syntax), line by line, skipping ignored \
             and binary files. Returns matching lines as path:line: text. Use it to find definitions, usages and \
             messages instead of reading whole files.",
            json!({
                "pattern": { "type": "string", "description": "Regular expression to find." },
                "path": { "type": "string", "description": "File or directory to search, relative to the workspace root. Default: all of it." },
                "glob": { "type": "string", "description": "Only search files whose path matches this glob, e.g. *.rs." },
                "case_insensitive": { "type": "boolean", "description": "Ignore case. Default false." },
            }),
            &["pattern"],
        ),
        tool(
            t::RUN,
            "Run a shell command with bash in the workspace root, and get its exit code and output. Use it to \
             build, run tests and inspect the project. There is no stdin, so do not start interactive programs. \
             Long output is cut in the middle, so filter it (for example with tail or grep) when you expect a lot. \
             Do not run git commands.",
            json!({
                "command": { "type": "string", "description": "The command line." },
                "timeout_s": { "type": "integer", "description": "Seconds before the command is killed. Default 120, maximum 1800." },
            }),
            &["command"],
        ),
    ]
}

/// The designer's tools: an attempt's, plus submit_check.
pub(crate) fn designer_tools() -> Vec<Value> {
    let mut tools = attempt_tools();
    tools.push(tool(
        t::SUBMIT_CHECK,
        "Submit the done-check you designed. This ends your work.",
        json!({
            "command": {
                "anyOf": [{ "type": "string" }, { "type": "null" }],
                "description": "Shell command, run from the workspace root, that exits 0 exactly when the task is \
                                done. null when no automated check fits the task.",
            },
            "files": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Every file you wrote that the check depends on, relative to the workspace root. \
                                Empty when the check uses only existing files.",
            },
            "rationale": { "type": "string", "description": "Briefly, why this check shows the task is done." },
        }),
        &["command", "files", "rationale"],
    ));
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_respect_char_boundaries() {
        let s = "aé€b".repeat(10);
        for max in 0..s.len() {
            assert!(tail(&s, max).len() <= max);
            assert!(head(&s, max).len() <= max);
            assert!(s.ends_with(tail(&s, max)));
        }
        let clipped = clip("x".repeat(100) + &"y".repeat(100), 50);
        assert!(clipped.starts_with("xxxxx") && clipped.ends_with("yyyyy"), "{clipped}");
        assert!(clipped.contains("[... 150 bytes cut ...]"), "{clipped}");
        assert_eq!(clip("short".into(), 50), "short");
    }

    #[test]
    fn output_tail_keeps_both_streams() {
        let run = RunResponse {
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            stdout: "o".repeat(10_000),
            stderr: "error: boom\n".into(),
            truncated: false,
            duration_ms: 1,
        };
        let out = output_tail(&run, 4000);
        assert!(out.len() < 4100);
        assert!(out.ends_with("stderr:\nerror: boom\n"), "{out}");
        assert_eq!(ending(&run), "exit code 1");
        assert_eq!(ending(&RunResponse { timed_out: true, exit_code: None, ..run.clone() }), "timed out");
        assert_eq!(
            output_tail(&RunResponse { stdout: String::new(), stderr: String::new(), ..run }, 10),
            "(no output)"
        );
    }

    #[test]
    fn tools_are_strict_and_closed() {
        let tools = designer_tools();
        assert_eq!(tools.len(), 7);
        for tool in &tools {
            assert_eq!(tool["strict"], true);
            assert_eq!(tool["input_schema"]["additionalProperties"], false);
            let props = tool["input_schema"]["properties"].as_object().unwrap();
            for req in tool["input_schema"]["required"].as_array().unwrap() {
                assert!(props.contains_key(req.as_str().unwrap()));
            }
        }
        assert_eq!(attempt_tools(), tools[..6]);
    }

    #[test]
    fn hints_vary_by_attempt() {
        let blocks = |i| attempt_first_message("t", Some(("make test", &[])), i)["content"].as_array().unwrap().len();
        assert_eq!((blocks(0), blocks(1), blocks(2), blocks(3)), (1, 2, 2, 1));
        assert_ne!(hint(1), hint(2));
        // The shared first block is identical across attempts, so it caches.
        let first = |i| attempt_first_message("t", Some(("make test", &[])), i)["content"][0].clone();
        assert_eq!(first(0), first(1));
    }
}
