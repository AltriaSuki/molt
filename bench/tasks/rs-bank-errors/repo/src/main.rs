//! `ledger`: runs a ledger script read from standard input against a fresh,
//! empty bank and prints the result of each command.
//!
//! ```text
//! $ printf 'open alice\ndeposit 1 20\nbalance 1\n' | ledger
//! opened account 1 for alice
//! ok
//! 20.00
//! ```

use std::io;
use std::process::ExitCode;

use ledger::{cli, Bank};

fn main() -> ExitCode {
    let mut bank = Bank::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    match cli::run(&mut bank, stdin.lock(), &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ledger: {err}");
            ExitCode::FAILURE
        }
    }
}
