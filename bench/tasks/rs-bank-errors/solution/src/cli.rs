//! The script interpreter behind the `ledger` binary.
//!
//! A script has one command per line; blank lines and lines starting with
//! `#` are skipped. Each command prints one line of output (`history` prints
//! one per transaction):
//!
//! | command                         | prints                              |
//! |---------------------------------|-------------------------------------|
//! | `open <owner>`                  | `opened account <id> for <owner>`   |
//! | `deposit <id> <amount>`         | `ok`                                |
//! | `withdraw <id> <amount>`        | `ok`                                |
//! | `transfer <from> <to> <amount>` | `ok`                                |
//! | `balance <id>`                  | the balance, e.g. `12.50`           |
//! | `history <id>`                  | one line per [`Txn`](crate::Txn), oldest first, or `no transactions` |
//! | `freeze <id>`                   | `ok`                                |
//! | `unfreeze <id>`                 | `ok`                                |
//!
//! Amounts are written as dollars and cents (`12`, `12.5`, `12.50`). A line
//! that is not a valid command, or a command the bank refuses, prints
//! `error: <reason>` instead, and the script carries on with the next line.

use std::io::{self, BufRead, Write};

use crate::bank::Bank;
use crate::command::Command;
use crate::error::BankError;
use crate::money::format_cents;

/// Runs every line of `input` against `bank`, writing the output to `out`.
pub fn run<R: BufRead, W: Write>(bank: &mut Bank, input: R, out: &mut W) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let result = Command::parse(line)
            .map_err(|err| err.to_string())
            .and_then(|command| execute(bank, command).map_err(|err| err.to_string()));
        match result {
            Ok(lines) => {
                for line in lines {
                    writeln!(out, "{line}")?;
                }
            }
            Err(reason) => writeln!(out, "error: {reason}")?,
        }
    }
    out.flush()
}

/// Runs one command, returning the lines it prints.
fn execute(bank: &mut Bank, command: Command) -> Result<Vec<String>, BankError> {
    let ok = || vec!["ok".to_string()];
    Ok(match command {
        Command::Open { owner } => {
            let id = bank.open(&owner);
            vec![format!("opened account {id} for {owner}")]
        }
        Command::Deposit { id, cents } => {
            bank.deposit(id, cents)?;
            ok()
        }
        Command::Withdraw { id, cents } => {
            bank.withdraw(id, cents)?;
            ok()
        }
        Command::Transfer { from, to, cents } => {
            bank.transfer(from, to, cents)?;
            ok()
        }
        Command::Freeze { id } => {
            bank.freeze(id)?;
            ok()
        }
        Command::Unfreeze { id } => {
            bank.unfreeze(id)?;
            ok()
        }
        Command::Balance { id } => vec![format_cents(bank.balance(id)?)],
        Command::History { id } => {
            let history = bank.history(id)?;
            if history.is_empty() {
                vec!["no transactions".to_string()]
            } else {
                history.iter().map(ToString::to_string).collect()
            }
        }
    })
}
