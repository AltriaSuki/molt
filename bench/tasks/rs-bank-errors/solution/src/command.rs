//! Parsing one line of a ledger script into a [`Command`].

use std::fmt;

use crate::account::AccountId;
use crate::money::parse_cents;

/// One command of a ledger script. Amounts are in cents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `open <owner>`
    Open { owner: String },
    /// `deposit <id> <amount>`
    Deposit { id: AccountId, cents: u64 },
    /// `withdraw <id> <amount>`
    Withdraw { id: AccountId, cents: u64 },
    /// `transfer <from> <to> <amount>`
    Transfer {
        from: AccountId,
        to: AccountId,
        cents: u64,
    },
    /// `balance <id>`
    Balance { id: AccountId },
    /// `history <id>`
    History { id: AccountId },
    /// `freeze <id>`
    Freeze { id: AccountId },
    /// `unfreeze <id>`
    Unfreeze { id: AccountId },
}

/// Why a line is not a valid command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The line has no words at all.
    Empty,
    /// The first word is not a command name.
    UnknownCommand(String),
    /// The command got the wrong number of arguments; holds the usage line.
    Usage(&'static str),
    /// An account id is not a number.
    InvalidId(String),
    /// An amount is not of the form `12`, `12.5` or `12.50`.
    InvalidAmount(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty command"),
            ParseError::UnknownCommand(name) => write!(f, "unknown command '{name}'"),
            ParseError::Usage(usage) => write!(f, "usage: {usage}"),
            ParseError::InvalidId(word) => write!(f, "invalid account id '{word}'"),
            ParseError::InvalidAmount(word) => write!(f, "invalid amount '{word}'"),
        }
    }
}

impl std::error::Error for ParseError {}

impl Command {
    /// Parses one line: a command name followed by its arguments, separated
    /// by whitespace.
    pub fn parse(line: &str) -> Result<Command, ParseError> {
        let mut words = line.split_whitespace();
        let name = words.next().ok_or(ParseError::Empty)?;
        let args: Vec<&str> = words.collect();
        match name {
            "open" if args.is_empty() => Err(ParseError::Usage("open <owner>")),
            "open" => Ok(Command::Open {
                owner: args.join(" "),
            }),
            "deposit" => {
                let [id, amount] = arguments(&args, "deposit <id> <amount>")?;
                Ok(Command::Deposit {
                    id: account_id(id)?,
                    cents: amount_in_cents(amount)?,
                })
            }
            "withdraw" => {
                let [id, amount] = arguments(&args, "withdraw <id> <amount>")?;
                Ok(Command::Withdraw {
                    id: account_id(id)?,
                    cents: amount_in_cents(amount)?,
                })
            }
            "transfer" => {
                let [from, to, amount] = arguments(&args, "transfer <from> <to> <amount>")?;
                Ok(Command::Transfer {
                    from: account_id(from)?,
                    to: account_id(to)?,
                    cents: amount_in_cents(amount)?,
                })
            }
            "balance" => {
                let [id] = arguments(&args, "balance <id>")?;
                Ok(Command::Balance {
                    id: account_id(id)?,
                })
            }
            "history" => {
                let [id] = arguments(&args, "history <id>")?;
                Ok(Command::History {
                    id: account_id(id)?,
                })
            }
            "freeze" => {
                let [id] = arguments(&args, "freeze <id>")?;
                Ok(Command::Freeze {
                    id: account_id(id)?,
                })
            }
            "unfreeze" => {
                let [id] = arguments(&args, "unfreeze <id>")?;
                Ok(Command::Unfreeze {
                    id: account_id(id)?,
                })
            }
            other => Err(ParseError::UnknownCommand(other.to_string())),
        }
    }
}

/// The arguments of a command that takes exactly `N` of them.
fn arguments<'a, const N: usize>(
    args: &[&'a str],
    usage: &'static str,
) -> Result<[&'a str; N], ParseError> {
    <[&str; N]>::try_from(args).map_err(|_| ParseError::Usage(usage))
}

fn account_id(word: &str) -> Result<AccountId, ParseError> {
    word.parse()
        .map_err(|_| ParseError::InvalidId(word.to_string()))
}

fn amount_in_cents(word: &str) -> Result<u64, ParseError> {
    parse_cents(word).ok_or_else(|| ParseError::InvalidAmount(word.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        assert_eq!(
            Command::parse("open Ada Lovelace"),
            Ok(Command::Open {
                owner: "Ada Lovelace".to_string()
            })
        );
        assert_eq!(
            Command::parse("deposit 1 20.50"),
            Ok(Command::Deposit {
                id: AccountId(1),
                cents: 2050
            })
        );
        assert_eq!(
            Command::parse("withdraw 2 3"),
            Ok(Command::Withdraw {
                id: AccountId(2),
                cents: 300
            })
        );
        assert_eq!(
            Command::parse("  transfer   1 2   0.5 "),
            Ok(Command::Transfer {
                from: AccountId(1),
                to: AccountId(2),
                cents: 50
            })
        );
        assert_eq!(
            Command::parse("balance 7"),
            Ok(Command::Balance { id: AccountId(7) })
        );
        assert_eq!(
            Command::parse("history 3"),
            Ok(Command::History { id: AccountId(3) })
        );
        assert_eq!(
            Command::parse("freeze 4"),
            Ok(Command::Freeze { id: AccountId(4) })
        );
        assert_eq!(
            Command::parse("unfreeze 4"),
            Ok(Command::Unfreeze { id: AccountId(4) })
        );
    }

    #[test]
    fn reports_what_is_wrong() {
        assert_eq!(Command::parse("   "), Err(ParseError::Empty));
        assert_eq!(
            Command::parse("close 1"),
            Err(ParseError::UnknownCommand("close".to_string()))
        );
        assert_eq!(
            Command::parse("open"),
            Err(ParseError::Usage("open <owner>"))
        );
        assert_eq!(
            Command::parse("deposit 1"),
            Err(ParseError::Usage("deposit <id> <amount>"))
        );
        assert_eq!(
            Command::parse("balance 1 2"),
            Err(ParseError::Usage("balance <id>"))
        );
        assert_eq!(
            Command::parse("freeze"),
            Err(ParseError::Usage("freeze <id>"))
        );
        assert_eq!(
            Command::parse("deposit one 5"),
            Err(ParseError::InvalidId("one".to_string()))
        );
        assert_eq!(
            Command::parse("transfer 1 2 -5"),
            Err(ParseError::InvalidAmount("-5".to_string()))
        );
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            ParseError::UnknownCommand("close".to_string()).to_string(),
            "unknown command 'close'"
        );
        assert_eq!(
            ParseError::Usage("history <id>").to_string(),
            "usage: history <id>"
        );
        assert_eq!(
            ParseError::InvalidId("x".to_string()).to_string(),
            "invalid account id 'x'"
        );
        assert_eq!(
            ParseError::InvalidAmount("1.234".to_string()).to_string(),
            "invalid amount '1.234'"
        );
    }
}
