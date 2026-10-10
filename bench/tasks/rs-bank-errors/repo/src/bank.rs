//! The bank: a set of accounts and the operations that move money between
//! them.

use crate::account::{Account, AccountId};
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
    ///
    /// # Panics
    ///
    /// If there is no account `id`.
    pub fn owner(&self, id: AccountId) -> &str {
        &self.account(id).owner
    }

    /// The balance of account `id`, in cents.
    ///
    /// # Panics
    ///
    /// If there is no account `id`.
    pub fn balance(&self, id: AccountId) -> u64 {
        self.account(id).balance
    }

    /// Everything that happened to account `id`, oldest first.
    ///
    /// # Panics
    ///
    /// If there is no account `id`.
    pub fn history(&self, id: AccountId) -> &[Txn] {
        &self.account(id).history
    }

    /// Pays `cents` into account `id`.
    ///
    /// # Panics
    ///
    /// If `cents` is 0, if there is no account `id`, or if the balance would
    /// overflow.
    pub fn deposit(&mut self, id: AccountId, cents: u64) {
        assert!(cents > 0, "deposit amount must be positive");
        let account = self.account_mut(id);
        account.balance = account
            .balance
            .checked_add(cents)
            .expect("balance overflow");
        account.history.push(Txn::Deposit { cents });
    }

    /// Takes `cents` out of account `id`.
    ///
    /// # Panics
    ///
    /// If `cents` is 0, if there is no account `id`, or if the account holds
    /// less than `cents`.
    pub fn withdraw(&mut self, id: AccountId, cents: u64) {
        assert!(cents > 0, "withdrawal amount must be positive");
        let account = self.account_mut(id);
        assert!(
            account.balance >= cents,
            "insufficient funds in account {id}: needed {cents}, available {}",
            account.balance
        );
        account.balance -= cents;
        account.history.push(Txn::Withdrawal { cents });
    }

    /// Moves `cents` from account `from` to account `to`, recording a
    /// [`Txn::TransferOut`] on `from` and a [`Txn::TransferIn`] on `to`.
    ///
    /// # Panics
    ///
    /// If `cents` is 0, if either account does not exist, if `from` holds
    /// less than `cents`, or if the balance of `to` would overflow.
    pub fn transfer(&mut self, from: AccountId, to: AccountId, cents: u64) {
        assert!(cents > 0, "transfer amount must be positive");

        let source = self.account_mut(from);
        assert!(
            source.balance >= cents,
            "insufficient funds in account {from}: needed {cents}, available {}",
            source.balance
        );
        source.balance -= cents;
        source.history.push(Txn::TransferOut { to, cents });

        let dest = self.account_mut(to);
        dest.balance = dest.balance.checked_add(cents).expect("balance overflow");
        dest.history.push(Txn::TransferIn { from, cents });
    }

    fn account(&self, id: AccountId) -> &Account {
        self.accounts
            .get(id.index())
            .unwrap_or_else(|| panic!("unknown account {id}"))
    }

    fn account_mut(&mut self, id: AccountId) -> &mut Account {
        self.accounts
            .get_mut(id.index())
            .unwrap_or_else(|| panic!("unknown account {id}"))
    }
}
