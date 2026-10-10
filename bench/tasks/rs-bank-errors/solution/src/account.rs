//! Account ids, and the record a [`Bank`](crate::Bank) keeps per account.

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;

use crate::error::BankError;
use crate::txn::Txn;

/// Identifies an account within one [`Bank`](crate::Bank).
///
/// [`Bank::open`](crate::Bank::open) hands out ids in order, starting at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId(pub u32);

impl AccountId {
    /// The id of the account stored at `index` in the bank's account list.
    pub(crate) fn from_index(index: usize) -> AccountId {
        AccountId(index as u32 + 1)
    }

    /// Where this account would sit in the bank's account list. `None` for
    /// `AccountId(0)`, which is never handed out.
    pub(crate) fn index(self) -> Option<usize> {
        (self.0 as usize).checked_sub(1)
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for AccountId {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(AccountId)
    }
}

/// One account: who owns it, what it holds, and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    pub(crate) owner: String,
    pub(crate) balance: u64,
    pub(crate) history: Vec<Txn>,
    pub(crate) frozen: bool,
}

impl Account {
    pub(crate) fn new(owner: &str) -> Account {
        Account {
            owner: owner.to_string(),
            balance: 0,
            history: Vec::new(),
            frozen: false,
        }
    }

    /// `Err(Frozen(id))` if this account (whose id is `id`) is frozen.
    pub(crate) fn ensure_active(&self, id: AccountId) -> Result<(), BankError> {
        if self.frozen {
            Err(BankError::Frozen(id))
        } else {
            Ok(())
        }
    }
}
