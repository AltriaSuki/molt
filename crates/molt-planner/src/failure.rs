//! Classify what failed (#228): a command by its exit code and output, a
//! model call or a tool call by its error. The class says whether another
//! try can help: a compiler error can be fixed by changing the code, a
//! missing tool or a rejected credential cannot, and a write that timed out
//! may already have happened. When nothing in the output decides it, the
//! class is unknown and the run goes on as it would have without one.
//!
//! A class only ever narrows what happens next: it can stop an attempt or
//! hold back a new one, never give a command more access or a run more budget.

use molt_api::planner::{Failure, FailureAction, FailureClass, FailureSource};
use molt_api::shell::RunResponse;
use molt_proto::{ErrorCode, RemoteError};

/// Bytes of the deciding line kept as evidence.
const EVIDENCE: usize = 300;

/// What the failed command `run` says about why it failed; `None` when it
/// passed or was cancelled.
pub(crate) fn of_command(run: &RunResponse, source: FailureSource) -> Option<Failure> {
    if run.cancelled || (run.exit_code == Some(0) && !run.timed_out) {
        return None;
    }
    let failure = |class, action, evidence: &str| Failure {
        class,
        source,
        action,
        evidence: cut(evidence.trim()),
        exit_code: run.exit_code,
    };
    if run.timed_out {
        return Some(failure(FailureClass::Unknown, FailureAction::None, "timed out"));
    }
    let lines: Vec<&str> = run.stderr.lines().chain(run.stdout.lines()).collect();
    let find = |test: fn(&str) -> bool| lines.iter().copied().find(|l| test(&l.trim().to_lowercase()));
    if let Some(line) = find(missing_tool) {
        return Some(failure(FailureClass::Environment, FailureAction::FixEnvironment, line));
    }
    if run.offline_failure {
        let line = find(|l| l.contains("error") || l.contains("failed")).unwrap_or_else(|| last(&lines));
        return Some(failure(FailureClass::Environment, FailureAction::FixEnvironment, line));
    }
    if let Some(line) = find(exhausted) {
        return Some(failure(FailureClass::Exhausted, FailureAction::FixEnvironment, line));
    }
    if let Some(line) = find(refused) {
        return Some(failure(FailureClass::Policy, FailureAction::ChangeApproach, line));
    }
    if let Some(line) = find(diagnostic) {
        return Some(failure(FailureClass::Code, FailureAction::FixCode, line));
    }
    // The shell's own code for a command it could not find, whatever it printed, unless that was
    // a file of the project's.
    if run.exit_code == Some(127) && find(|l| not_found(l).is_some_and(|name| name.contains('/'))).is_none() {
        return Some(failure(FailureClass::Environment, FailureAction::FixEnvironment, last(&lines)));
    }
    Some(failure(FailureClass::Unknown, FailureAction::None, last(&lines)))
}

/// What a failed model call says about the endpoint.
pub(crate) fn of_model(e: &RemoteError) -> Failure {
    let (class, action) = match e.code {
        // 401 and 403: the credential, or what it may use.
        ErrorCode::Denied => (FailureClass::Endpoint, FailureAction::FixConfig),
        // No such model or path at that endpoint.
        ErrorCode::Invalid if e.message.contains(" returned 404") => (FailureClass::Endpoint, FailureAction::FixConfig),
        // The gateway already retried these; a later call may get through.
        ErrorCode::Busy | ErrorCode::Unavailable | ErrorCode::Timeout => {
            (FailureClass::Endpoint, FailureAction::RetryLater)
        }
        ErrorCode::OverBudget => (FailureClass::Exhausted, FailureAction::None),
        _ => (FailureClass::Unknown, FailureAction::None),
    };
    Failure { class, source: FailureSource::Model, action, evidence: cut(first_line(&e.message)), exit_code: None }
}

/// What a tool call's error says, when it says something the model should
/// act on: a call that may have changed something and was stopped before it
/// answered has an unknown outcome, and must not simply be made again.
pub(crate) fn of_tool(e: &RemoteError, read_only: bool) -> Option<Failure> {
    let (class, action) = match e.code {
        ErrorCode::Timeout | ErrorCode::Cancelled if !read_only => {
            (FailureClass::OutcomeUnknown, FailureAction::VerifyFirst)
        }
        ErrorCode::Timeout | ErrorCode::Busy => (FailureClass::Unknown, FailureAction::RetryLater),
        ErrorCode::Denied => (FailureClass::Policy, FailureAction::ChangeApproach),
        _ => return None,
    };
    Some(Failure { class, source: FailureSource::Tool, action, evidence: cut(first_line(&e.message)), exit_code: None })
}

/// A shell, package manager or toolchain saying a program, package or
/// toolchain is not there. `line` is trimmed and lowercased.
fn missing_tool(line: &str) -> bool {
    if let Some(name) = not_found(line) {
        // A path the attempt was to write is the code's to provide, not the environment's.
        return !name.contains('/');
    }
    // `python3 -m pytest` without pytest; an import the code makes says `ModuleNotFoundError:` instead,
    // and may name a module the attempt has yet to write.
    if line.contains(": no module named ") && !line.contains("error:") {
        return true;
    }
    line.starts_with("error: no such command:")
        || (line.starts_with("error: toolchain '") && line.contains("is not installed"))
        || line.contains("can't find crate for")
        // Node's resolver: a package, never a file of the project.
        || line.contains("cannot find package '")
        || bare_module(line)
}

/// The program a shell could not find: bash's `bash: line 1: pytest: command
/// not found`, dash's `sh: 1: pytest: not found`.
fn not_found(line: &str) -> Option<&str> {
    let shell = line.starts_with("sh: ") || line.starts_with("/bin/sh: ") || line.starts_with("bash: ");
    if !(line.ends_with(": command not found") || (shell && line.ends_with(": not found"))) {
        return None;
    }
    let name = line.trim_end_matches(": command not found").trim_end_matches(": not found");
    Some(name.rsplit(": ").next().unwrap_or(name))
}

/// Node's `Cannot find module 'x'` for a package, not for `./x` or `/x`.
fn bare_module(line: &str) -> bool {
    let Some(at) = line.find("cannot find module '") else { return false };
    let name = &line[at + "cannot find module '".len()..];
    !(name.starts_with('.') || name.starts_with('/'))
}

fn exhausted(line: &str) -> bool {
    ["no space left on device", "disk quota exceeded", "cannot allocate memory", "out of memory"]
        .iter()
        .any(|sign| line.contains(sign))
        || (line.contains("fork") && line.contains("resource temporarily unavailable"))
}

/// The sandbox refused a write or an operation.
fn refused(line: &str) -> bool {
    line.contains("read-only file system") || line.contains("operation not permitted")
}

/// A compiler, type checker or test runner reporting a fault in the code.
fn diagnostic(line: &str) -> bool {
    const STARTS: &[&str] = &[
        // rustc and cargo
        "error[e",
        "error: could not compile",
        "test result: failed",
        // python
        "syntaxerror",
        "indentationerror",
        "traceback (most recent call last)",
        "assertionerror",
        // go test
        "--- fail:",
        "fail\t",
    ];
    STARTS.iter().any(|s| line.starts_with(s))
        // pytest's summary, `FAILED tests/test_a.py::test_b - ...`
        || (line.starts_with("failed ") && line.contains("::"))
        // jest's summary
        || (line.starts_with("tests:") && line.contains(" failed"))
        || line.contains("panicked at")
        // gcc, clang, javac (`main.c:3:5: error:`), tsc (`a.ts(3,5): error ts2322:`)
        || line.contains(": error:")
        || line.contains(": error ts")
}

fn last<'a>(lines: &[&'a str]) -> &'a str {
    lines.iter().rev().copied().find(|l| !l.trim().is_empty()).unwrap_or("(no output)")
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text)
}

fn cut(line: &str) -> String {
    crate::prompts::head(line, EVIDENCE).to_owned()
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
            duration_ms: 5,
            offline_failure: false,
        }
    }

    fn class(run: &RunResponse) -> (FailureClass, FailureAction, String) {
        let f = of_command(run, FailureSource::Check).unwrap();
        (f.class, f.action, f.evidence)
    }

    use FailureAction as A;
    use FailureClass as C;

    #[test]
    fn a_missing_program_or_package_is_the_environment_and_a_missing_project_file_is_not() {
        // dash, as `sh -c` runs on Debian and Ubuntu.
        let dash = ran(127, "", "sh: 1: pytest: not found\n");
        assert_eq!(class(&dash), (C::Environment, A::FixEnvironment, "sh: 1: pytest: not found".into()));
        assert_eq!(of_command(&dash, FailureSource::Check).unwrap().exit_code, Some(127));
        let bash = ran(127, "", "bash: line 1: go: command not found\n");
        assert_eq!(class(&bash).0, C::Environment);
        let module = ran(1, "", "/usr/bin/python3: No module named pytest\n");
        assert_eq!(class(&module).0, C::Environment);
        let cargo =
            ran(101, "", "error: no such command: `nextest`\n\n\tView all installed commands with `cargo --list`\n");
        assert_eq!(class(&cargo), (C::Environment, A::FixEnvironment, "error: no such command: `nextest`".into()));
        let target = ran(
            101,
            "",
            "   Compiling demo v0.1.0 (/work/demo)\nerror[E0463]: can't find crate for `core`\n  |\n  = note: the `wasm32-unknown-unknown` target may not be installed\n",
        );
        assert_eq!(class(&target).0, C::Environment, "missing target outranks the error code");
        let node = ran(
            1,
            "",
            "node:internal/modules/cjs/loader:1228\n  throw err;\n  ^\n\nError: Cannot find module 'lodash'\nRequire stack:\n- /work/app/index.js\n",
        );
        assert_eq!(class(&node), (C::Environment, A::FixEnvironment, "Error: Cannot find module 'lodash'".into()));
        let esm =
            ran(1, "", "Error [ERR_MODULE_NOT_FOUND]: Cannot find package 'chalk' imported from /work/app/a.mjs\n");
        assert_eq!(class(&esm).0, C::Environment);

        // The project's own files the attempt may still write: not the environment.
        let script = ran(127, "", "sh: 1: ./build.sh: not found\n");
        assert_ne!(class(&script).0, C::Environment);
        let relative = ran(1, "", "Error: Cannot find module './parser'\nRequire stack:\n- /work/app/index.js\n");
        assert_ne!(class(&relative).0, C::Environment);
        let import = ran(
            2,
            "",
            "ImportError while importing test module '/work/tests/test_slug.py'.\nE   ModuleNotFoundError: No module named 'slug'\n",
        );
        assert_ne!(class(&import).0, C::Environment, "it may be the module the task asks for");
    }

    #[test]
    fn a_dependency_that_needed_the_network_is_the_environment() {
        let stderr = "    Updating crates.io index\nwarning: spurious network error (3 tries remaining): [6] Could not resolve hostname (Could not resolve host: index.crates.io)\nerror: failed to get `serde` as a dependency of package `demo v0.1.0 (/work/demo)`\n";
        let offline = RunResponse { offline_failure: true, ..ran(101, "", stderr) };
        let (c, a, evidence) = class(&offline);
        assert_eq!((c, a), (C::Environment, A::FixEnvironment));
        assert!(evidence.starts_with("warning: spurious network error"), "{evidence}");
    }

    #[test]
    fn compiler_and_test_failures_are_the_codes_to_fix() {
        let rustc = ran(
            101,
            "",
            "   Compiling demo v0.1.0 (/work/demo)\nerror[E0308]: mismatched types\n --> src/lib.rs:3:5\n  |\n3 |     \"one\"\n  |     ^^^^^ expected `u32`, found `&str`\n\nFor more information about this error, try `rustc --explain E0308`.\nerror: could not compile `demo` (lib) due to 1 previous error\n",
        );
        assert_eq!(class(&rustc), (C::Code, A::FixCode, "error[E0308]: mismatched types".into()));
        let cargo_test = ran(101, "running 2 tests\ntest it_adds ... FAILED\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n", "error: test failed, to rerun pass `--lib`\n");
        assert_eq!(class(&cargo_test).0, C::Code);
        let pytest = ran(1, "FAILED tests/test_slug.py::test_unicode - AssertionError: assert 'a-b' == 'a_b'\n========================= 1 failed, 3 passed in 0.04s =========================\n", "");
        assert_eq!(class(&pytest).0, C::Code);
        let gotest = ran(1, "--- FAIL: TestAdd (0.00s)\n    add_test.go:9: Add(1, 2) = 4, want 3\nFAIL\nFAIL\texample.com/add\t0.002s\n", "");
        assert_eq!(class(&gotest), (C::Code, A::FixCode, "--- FAIL: TestAdd (0.00s)".into()));
        let tsc = ran(2, "src/index.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.\n", "");
        assert_eq!(class(&tsc).0, C::Code);
        let gcc = ran(1, "", "main.c: In function 'main':\nmain.c:3:5: error: expected ';' before 'return'\n");
        assert_eq!(class(&gcc).0, C::Code);
        let jest = ran(1, "", "Tests:       1 failed, 4 passed, 5 total\n");
        assert_eq!(class(&jest).0, C::Code);
    }

    #[test]
    fn resources_and_the_sandbox_and_unclear_output_are_told_apart() {
        let disk = ran(
            1,
            "",
            "error: failed to write /work/target/debug/deps/libdemo.rlib: No space left on device (os error 28)\n",
        );
        assert_eq!(class(&disk).0, C::Exhausted);
        let ro = ran(1, "", "touch: cannot touch '/etc/demo': Read-only file system\n");
        assert_eq!(
            class(&ro),
            (C::Policy, A::ChangeApproach, "touch: cannot touch '/etc/demo': Read-only file system".into())
        );
        let unclear = ran(1, "expected hi, got bye\n", "");
        assert_eq!(class(&unclear), (C::Unknown, A::None, "expected hi, got bye".into()));
        let timeout = RunResponse { timed_out: true, exit_code: None, ..ran(0, "", "") };
        assert_eq!(class(&timeout).0, C::Unknown);
        assert!(of_command(&ran(0, "ok", ""), FailureSource::Check).is_none());
        let cancelled = RunResponse { cancelled: true, exit_code: None, ..ran(0, "", "") };
        assert!(of_command(&cancelled, FailureSource::Check).is_none());
        let long = ran(1, "", &format!("{}\n", "x".repeat(2000)));
        assert_eq!(class(&long).2.len(), EVIDENCE);
    }

    #[test]
    fn model_errors_say_whether_the_endpoint_needs_the_user() {
        let e = |code, message: &str| RemoteError { code, message: message.into() };
        // As the gateway words a 401.
        let auth = of_model(&e(
            ErrorCode::Denied,
            "the Messages API returned 401: authentication_error: invalid x-api-key; the credential was rejected: check ANTHROPIC_API_KEY",
        ));
        assert_eq!((auth.class, auth.action, auth.source), (C::Endpoint, A::FixConfig, FailureSource::Model));
        assert!(auth.needs_user() && auth.evidence.contains("401"), "{auth:?}");
        let gone = of_model(&e(ErrorCode::Invalid, "the Messages API returned 404: not_found_error: model: claude-x"));
        assert_eq!(gone.action, A::FixConfig);
        let busy = of_model(&e(ErrorCode::Busy, "the Messages API returned 529: overloaded_error (3 attempts)"));
        assert_eq!((busy.class, busy.action), (C::Endpoint, A::RetryLater));
        assert!(!busy.needs_user());
        let bad = of_model(&e(ErrorCode::Invalid, "the Messages API returned 400: invalid_request_error"));
        assert_eq!(bad.class, C::Unknown);
    }

    #[test]
    fn a_write_that_timed_out_is_of_unknown_outcome_and_a_read_may_be_tried_again() {
        let timeout = RemoteError { code: ErrorCode::Timeout, message: "no reply before the deadline".into() };
        let write = of_tool(&timeout, false).unwrap();
        assert_eq!((write.class, write.action), (C::OutcomeUnknown, A::VerifyFirst));
        let read = of_tool(&timeout, true).unwrap();
        assert_eq!(read.action, A::RetryLater);
        let cancelled = RemoteError { code: ErrorCode::Cancelled, message: "stopped".into() };
        assert_eq!(of_tool(&cancelled, false).unwrap().class, C::OutcomeUnknown);
        let invalid = RemoteError { code: ErrorCode::Invalid, message: "bad input".into() };
        assert!(of_tool(&invalid, false).is_none());
    }
}
