//! The error type for [`Bank`](crate::Bank) operations.

use std::error::Error;
use std::fmt;

use crate::account::AccountId;
use crate::money::format_cents;

/// Why a [`Bank`](crate::Bank) operation was refused. A refused operation
/// changes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BankError {
    /// There is no account with this id.
    UnknownAccount(AccountId),
    /// `account` holds `available` cents, less than the `needed` cents the
    /// withdrawal or transfer asked for.
    InsufficientFunds {
        account: AccountId,
        needed: u64,
        available: u64,
    },
    /// The amount was zero.
    ZeroAmount,
    /// A transfer named the same account as source and destination.
    SameAccount,
    /// The account is frozen.
    Frozen(AccountId),
    /// The receiving balance would exceed `u64::MAX` cents.
    Overflow,
}

impl fmt::Display for BankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BankError::UnknownAccount(id) => write!(f, "unknown account {id}"),
            BankError::InsufficientFunds {
                account,
                needed,
                available,
            } => write!(
                f,
                "insufficient funds in account {account}: needed {}, available {}",
                format_cents(*needed),
                format_cents(*available)
            ),
            BankError::ZeroAmount => write!(f, "amount must be greater than zero"),
            BankError::SameAccount => write!(f, "cannot transfer to the same account"),
            BankError::Frozen(id) => write!(f, "account {id} is frozen"),
            BankError::Overflow => write!(f, "balance would overflow"),
        }
    }
}

impl Error for BankError {}
