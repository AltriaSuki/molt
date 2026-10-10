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
//!
//! Amounts are written as dollars and cents (`12`, `12.5`, `12.50`). A line
//! that is not a valid command prints `error: <reason>`, and the script
//! carries on with the next line.

use std::io::{self, BufRead, Write};

use crate::bank::Bank;
use crate::command::Command;
use crate::money::format_cents;

/// Runs every line of `input` against `bank`, writing the output to `out`.
pub fn run<R: BufRead, W: Write>(bank: &mut Bank, input: R, out: &mut W) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match Command::parse(line) {
            Ok(command) => execute(bank, command, out)?,
            Err(err) => writeln!(out, "error: {err}")?,
        }
    }
    out.flush()
}

fn execute<W: Write>(bank: &mut Bank, command: Command, out: &mut W) -> io::Result<()> {
    match command {
        Command::Open { owner } => {
            let id = bank.open(&owner);
            writeln!(out, "opened account {id} for {owner}")
        }
        Command::Deposit { id, cents } => {
            bank.deposit(id, cents);
            writeln!(out, "ok")
        }
        Command::Withdraw { id, cents } => {
            bank.withdraw(id, cents);
            writeln!(out, "ok")
        }
        Command::Transfer { from, to, cents } => {
            bank.transfer(from, to, cents);
            writeln!(out, "ok")
        }
        Command::Balance { id } => writeln!(out, "{}", format_cents(bank.balance(id))),
        Command::History { id } => {
            let history = bank.history(id);
            if history.is_empty() {
                return writeln!(out, "no transactions");
            }
            for txn in history {
                writeln!(out, "{txn}")?;
            }
            Ok(())
        }
    }
}
