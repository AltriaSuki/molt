//! The bank: a set of accounts and the operations that move money between
//! them.
//!
//! Every operation is all-or-nothing: it checks everything that could go
//! wrong before it changes anything, so one that returns an error leaves the
//! bank exactly as it was.

use crate::account::{Account, AccountId};
use crate::error::BankError;
use crate::txn::Txn;

/// An in-memory bank. Balances are whole cents.
#[derive(Debug, Clone, Default)]
pub struct Bank {
    accounts: Vec<Account>,
}

impl Bank {
    /// A bank with no accounts.
    pub fn new() -> Bank {
        Bank::default()
    }

    /// Opens an empty account for `owner` and returns its id. Ids are handed
    /// out in order, starting at 1.
    pub fn open(&mut self, owner: &str) -> AccountId {
        self.accounts.push(Account::new(owner));
        AccountId::from_index(self.accounts.len() - 1)
    }

    /// The number of accounts opened so far.
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// Whether no account has been opened yet.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Who owns account `id`.
    pub fn owner(&self, id: AccountId) -> Result<&str, BankError> {
        Ok(&self.account(id)?.owner)
    }

    /// The balance of account `id`, in cents.
    pub fn balance(&self, id: AccountId) -> Result<u64, BankError> {
        Ok(self.account(id)?.balance)
    }

    /// Everything that happened to account `id`, oldest first.
    pub fn history(&self, id: AccountId) -> Result<&[Txn], BankError> {
        Ok(&self.account(id)?.history)
    }

    /// Whether account `id` is frozen.
    pub fn is_frozen(&self, id: AccountId) -> Result<bool, BankError> {
        Ok(self.account(id)?.frozen)
    }

    /// Freezes account `id`: until it is unfrozen it can't take part in a
    /// deposit, withdrawal or transfer. Freezing a frozen account is fine.
    pub fn freeze(&mut self, id: AccountId) -> Result<(), BankError> {
        self.account_mut(id)?.frozen = true;
        Ok(())
    }

    /// Unfreezes account `id`. Unfreezing an account that isn't frozen is
    /// fine.
    pub fn unfreeze(&mut self, id: AccountId) -> Result<(), BankError> {
        self.account_mut(id)?.frozen = false;
        Ok(())
    }

    /// Pays `cents` into account `id`.
    ///
    /// Errors, checked in this order: `ZeroAmount`, `UnknownAccount`,
    /// `Frozen`, `Overflow`.
    pub fn deposit(&mut self, id: AccountId, cents: u64) -> Result<(), BankError> {
        require_nonzero(cents)?;
        let account = self.account_mut(id)?;
        account.ensure_active(id)?;
        account.balance = credit(account.balance, cents)?;
        account.history.push(Txn::Deposit { cents });
        Ok(())
    }

    /// Takes `cents` out of account `id`.
    ///
    /// Errors, checked in this order: `ZeroAmount`, `UnknownAccount`,
    /// `Frozen`, `InsufficientFunds`.
    pub fn withdraw(&mut self, id: AccountId, cents: u64) -> Result<(), BankError> {
        require_nonzero(cents)?;
        let account = self.account_mut(id)?;
        account.ensure_active(id)?;
        account.balance = debit(id, account.balance, cents)?;
        account.history.push(Txn::Withdrawal { cents });
        Ok(())
    }

    /// Moves `cents` from account `from` to account `to`, recording a
    /// [`Txn::TransferOut`] on `from` and a [`Txn::TransferIn`] on `to`.
    ///
    /// Errors, checked in this order: `ZeroAmount`, `SameAccount`,
    /// `UnknownAccount(from)`, `UnknownAccount(to)`, `Frozen(from)`,
    /// `Frozen(to)`, `InsufficientFunds`, `Overflow`.
    pub fn transfer(
        &mut self,
        from: AccountId,
        to: AccountId,
        cents: u64,
    ) -> Result<(), BankError> {
        require_nonzero(cents)?;
        if from == to {
            return Err(BankError::SameAccount);
        }
        let source_slot = self.slot(from)?;
        let dest_slot = self.slot(to)?;
        let (source, dest) = (&self.accounts[source_slot], &self.accounts[dest_slot]);
        source.ensure_active(from)?;
        dest.ensure_active(to)?;
        let source_balance = debit(from, source.balance, cents)?;
        let dest_balance = credit(dest.balance, cents)?;

        // Every check has passed: now commit both sides.
        let source = &mut self.accounts[source_slot];
        source.balance = source_balance;
        source.history.push(Txn::TransferOut { to, cents });
        let dest = &mut self.accounts[dest_slot];
        dest.balance = dest_balance;
        dest.history.push(Txn::TransferIn { from, cents });
        Ok(())
    }

    /// Where account `id` sits in `self.accounts`.
    fn slot(&self, id: AccountId) -> Result<usize, BankError> {
        id.index()
            .filter(|&index| index < self.accounts.len())
            .ok_or(BankError::UnknownAccount(id))
    }

    fn account(&self, id: AccountId) -> Result<&Account, BankError> {
        let slot = self.slot(id)?;
        Ok(&self.accounts[slot])
    }

    fn account_mut(&mut self, id: AccountId) -> Result<&mut Account, BankError> {
        let slot = self.slot(id)?;
        Ok(&mut self.accounts[slot])
    }
}

fn require_nonzero(cents: u64) -> Result<(), BankError> {
    if cents == 0 {
        Err(BankError::ZeroAmount)
    } else {
        Ok(())
    }
}

/// The balance of `account` after taking `cents` out of `balance`.
fn debit(account: AccountId, balance: u64, cents: u64) -> Result<u64, BankError> {
    balance
        .checked_sub(cents)
        .ok_or(BankError::InsufficientFunds {
            account,
            needed: cents,
            available: balance,
        })
}

/// The balance after paying `cents` into `balance`.
fn credit(balance: u64, cents: u64) -> Result<u64, BankError> {
    balance.checked_add(cents).ok_or(BankError::Overflow)
}
