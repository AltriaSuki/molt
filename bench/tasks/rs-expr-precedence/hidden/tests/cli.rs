//! Runs the `calc` binary end to end.

use std::io::Write;
use std::process::{Command, Stdio};

struct Output {
    stdout: String,
    stderr: String,
    success: bool,
}

fn calc(input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_calc"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start calc");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait for calc");
    Output {
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        success: out.status.success(),
    }
}

#[test]
fn prints_one_result_per_line() {
    let out = calc("1 + 2\n\n# a comment\nx = 4\nx * 2.5\n");
    assert_eq!(out.stdout, "3\n4\n10\n");
    assert_eq!(out.stderr, "");
    assert!(out.success);
}

#[test]
fn knows_the_constants() {
    let out = calc("r = 2\ntau * r\n");
    assert_eq!(out.stdout, format!("2\n{}\n", std::f64::consts::TAU * 2.0));
}

#[test]
fn reports_errors_and_keeps_going() {
    let out = calc("1 / 0\nnope + 1\n2 + 2\n");
    assert_eq!(out.stdout, "4\n");
    assert!(
        out.stderr.contains("line 1: error: division by zero"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("line 2: error: unknown variable `nope`"),
        "{}",
        out.stderr
    );
    assert!(!out.success);
}
