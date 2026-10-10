//! Entries in an account's history.

use std::fmt;

use crate::account::AccountId;
use crate::money::format_cents;

/// One entry in an account's history. Amounts are in cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Txn {
    /// Money paid into the account.
    Deposit { cents: u64 },
    /// Money taken out of the account.
    Withdrawal { cents: u64 },
    /// Money this account sent to account `to`.
    TransferOut { to: AccountId, cents: u64 },
    /// Money this account received from account `from`.
    TransferIn { from: AccountId, cents: u64 },
}

impl Txn {
    /// The amount of money moved, in cents.
    pub fn cents(&self) -> u64 {
        match *self {
            Txn::Deposit { cents }
            | Txn::Withdrawal { cents }
            | Txn::TransferOut { cents, .. }
            | Txn::TransferIn { cents, .. } => cents,
        }
    }
}

/// One line of a statement: `deposit 20.00`, `withdrawal 3.50`,
/// `transfer 7.50 to 2`, `transfer 7.50 from 1`.
impl fmt::Display for Txn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Txn::Deposit { cents } => write!(f, "deposit {}", format_cents(cents)),
            Txn::Withdrawal { cents } => write!(f, "withdrawal {}", format_cents(cents)),
            Txn::TransferOut { to, cents } => {
                write!(f, "transfer {} to {to}", format_cents(cents))
            }
            Txn::TransferIn { from, cents } => {
                write!(f, "transfer {} from {from}", format_cents(cents))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(Txn::Deposit { cents: 2_000 }.to_string(), "deposit 20.00");
        assert_eq!(
            Txn::Withdrawal { cents: 350 }.to_string(),
            "withdrawal 3.50"
        );
        let out = Txn::TransferOut {
            to: AccountId(2),
            cents: 750,
        };
        assert_eq!(out.to_string(), "transfer 7.50 to 2");
        let into = Txn::TransferIn {
            from: AccountId(1),
            cents: 5,
        };
        assert_eq!(into.to_string(), "transfer 0.05 from 1");
    }

    #[test]
    fn cents() {
        assert_eq!(Txn::Withdrawal { cents: 9 }.cents(), 9);
        assert_eq!(
            Txn::TransferIn {
                from: AccountId(4),
                cents: 12
            }
            .cents(),
            12
        );
    }
}
