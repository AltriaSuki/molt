//! `calc`: reads one expression or assignment per line from stdin and prints
//! each result. Blank lines and lines starting with `#` are skipped. Errors go
//! to stderr with a caret under the offending character; the exit status is 1
//! if any line failed.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use calc::{run, CalcError, Env};

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut env = Env::with_constants();
    let mut failed = false;

    for (index, line) in stdin.lock().lines().enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!("calc: cannot read input: {err}");
                return ExitCode::FAILURE;
            }
        };
        let src = line.trim_end();
        let trimmed = src.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match run(src, &mut env) {
            Ok(value) => {
                if writeln!(out, "{value}").is_err() {
                    return ExitCode::FAILURE;
                }
            }
            Err(err) => {
                failed = true;
                report(index + 1, src, &err);
            }
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn report(line_number: usize, src: &str, err: &CalcError) {
    eprintln!("line {line_number}: error: {err}");
    if let Some(pos) = err.pos() {
        // `pos` is a byte offset; the caret needs a column in characters.
        let column = src.get(..pos).map_or(0, |before| before.chars().count());
        eprintln!("    {src}");
        eprintln!("    {}^", " ".repeat(column));
    }
}
