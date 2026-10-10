use std::error::Error;
use std::io::Write;
use std::process::{Command, Stdio};

use ledger::{cli, AccountId, Bank, BankError, Txn};

const MAX: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A bank with one account per entry of `balances`, each funded by a single
/// deposit (none for a zero balance).
fn bank_with(balances: &[u64]) -> (Bank, Vec<AccountId>) {
    let mut bank = Bank::new();
    let ids = balances
        .iter()
        .enumerate()
        .map(|(i, &cents)| {
            let id = bank.open(&format!("owner{}", i + 1));
            if cents > 0 {
                bank.deposit(id, cents).unwrap();
            }
            id
        })
        .collect();
    (bank, ids)
}

/// Everything observable about one account.
#[derive(Debug, Clone, PartialEq)]
struct AccountState {
    owner: String,
    balance: u64,
    history: Vec<Txn>,
    frozen: bool,
}

/// Everything observable about every account in `ids`.
fn snapshot(bank: &Bank, ids: &[AccountId]) -> Vec<AccountState> {
    ids.iter()
        .map(|&id| AccountState {
            owner: bank.owner(id).unwrap().to_string(),
            balance: bank.balance(id).unwrap(),
            history: bank.history(id).unwrap().to_vec(),
            frozen: bank.is_frozen(id).unwrap(),
        })
        .collect()
}

/// Runs `op` and checks that it fails with `expected` and leaves every
/// account in `ids` exactly as it was.
fn assert_refused(
    bank: &mut Bank,
    ids: &[AccountId],
    expected: BankError,
    op: impl FnOnce(&mut Bank) -> Result<(), BankError>,
) {
    let before = snapshot(bank, ids);
    let len = bank.len();
    assert_eq!(op(bank), Err(expected));
    assert_eq!(
        snapshot(bank, ids),
        before,
        "a refused operation changed the bank"
    );
    assert_eq!(bank.len(), len);
}

fn transcript_with(bank: &mut Bank, script: &str) -> String {
    let mut out = Vec::new();
    cli::run(bank, script.as_bytes(), &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn transcript(script: &str) -> String {
    transcript_with(&mut Bank::new(), script)
}

// ---------------------------------------------------------------------------
// The error type
// ---------------------------------------------------------------------------

#[test]
fn error_messages() {
    let cases = [
        (BankError::UnknownAccount(AccountId(7)), "unknown account 7"),
        (BankError::UnknownAccount(AccountId(0)), "unknown account 0"),
        (
            BankError::InsufficientFunds {
                account: AccountId(1),
                needed: 5_000,
                available: 1_250,
            },
            "insufficient funds in account 1: needed 50.00, available 12.50",
        ),
        (
            BankError::InsufficientFunds {
                account: AccountId(12),
                needed: 5,
                available: 0,
            },
            "insufficient funds in account 12: needed 0.05, available 0.00",
        ),
        (
            BankError::InsufficientFunds {
                account: AccountId(3),
                needed: MAX,
                available: 100_001,
            },
            "insufficient funds in account 3: needed 184467440737095516.15, available 1000.01",
        ),
        (BankError::ZeroAmount, "amount must be greater than zero"),
        (
            BankError::SameAccount,
            "cannot transfer to the same account",
        ),
        (BankError::Frozen(AccountId(3)), "account 3 is frozen"),
        (BankError::Overflow, "balance would overflow"),
    ];
    for (err, message) in cases {
        assert_eq!(err.to_string(), message, "{err:?}");
    }
}

fn withdraw_too_much() -> Result<(), Box<dyn Error>> {
    let mut bank = Bank::new();
    let id = bank.open("dora");
    bank.deposit(id, 300)?;
    bank.withdraw(id, 400)?;
    Ok(())
}

#[test]
fn bank_error_is_a_std_error() {
    let err = withdraw_too_much().unwrap_err();
    assert_eq!(
        err.to_string(),
        "insufficient funds in account 1: needed 4.00, available 3.00"
    );
    let boxed: Box<dyn Error> = Box::new(BankError::Frozen(AccountId(2)));
    assert_eq!(boxed.to_string(), "account 2 is frozen");
}

fn assert_traits<T: std::fmt::Debug + Clone + PartialEq + Eq + Error + 'static>() {}

#[test]
fn bank_error_derives_debug_clone_and_eq() {
    assert_traits::<BankError>();
    let err = BankError::InsufficientFunds {
        account: AccountId(4),
        needed: 10,
        available: 9,
    };
    let copy = err.clone();
    assert_eq!(copy, err);
    assert_ne!(copy, BankError::Overflow);
    assert_ne!(
        BankError::UnknownAccount(AccountId(1)),
        BankError::Frozen(AccountId(1))
    );
    assert!(!format!("{err:?}").is_empty());
}

// ---------------------------------------------------------------------------
// Unknown accounts
// ---------------------------------------------------------------------------

#[test]
fn every_method_reports_unknown_accounts() {
    let (mut bank, ids) = bank_with(&[1_000, 500]);
    for raw in [0, 3, 4, 1_000, u32::MAX] {
        let id = AccountId(raw);
        let unknown = BankError::UnknownAccount(id);
        assert_eq!(bank.owner(id), Err(unknown.clone()), "owner({raw})");
        assert_eq!(bank.balance(id), Err(unknown.clone()), "balance({raw})");
        assert_eq!(bank.history(id), Err(unknown.clone()), "history({raw})");
        assert_eq!(bank.is_frozen(id), Err(unknown.clone()), "is_frozen({raw})");
        assert_refused(&mut bank, &ids, unknown.clone(), |b| b.freeze(id));
        assert_refused(&mut bank, &ids, unknown.clone(), |b| b.unfreeze(id));
        assert_refused(&mut bank, &ids, unknown.clone(), |b| b.deposit(id, 5));
        assert_refused(&mut bank, &ids, unknown.clone(), |b| b.withdraw(id, 5));
    }
}

#[test]
fn an_empty_bank_has_no_accounts() {
    let mut bank = Bank::new();
    let id = AccountId(1);
    assert_eq!(bank.balance(id), Err(BankError::UnknownAccount(id)));
    assert_eq!(bank.history(id), Err(BankError::UnknownAccount(id)));
    assert_eq!(bank.deposit(id, 1), Err(BankError::UnknownAccount(id)));
    assert_eq!(bank.freeze(id), Err(BankError::UnknownAccount(id)));
    assert!(bank.is_empty());
    assert_eq!(bank.open("first"), AccountId(1));
    assert_eq!(bank.balance(id), Ok(0));
}

#[test]
fn transfer_to_an_unknown_account_leaves_the_source_untouched() {
    let (mut bank, ids) = bank_with(&[2_000]);
    let a = ids[0];
    for raw in [0, 2, 99] {
        let to = AccountId(raw);
        assert_refused(&mut bank, &ids, BankError::UnknownAccount(to), |b| {
            b.transfer(a, to, 500)
        });
    }
    assert_eq!(bank.balance(a), Ok(2_000));
    assert_eq!(bank.history(a), Ok(&[Txn::Deposit { cents: 2_000 }][..]));
}

#[test]
fn transfer_from_an_unknown_account() {
    let (mut bank, ids) = bank_with(&[100]);
    let a = ids[0];
    for raw in [0, 2, 77] {
        let from = AccountId(raw);
        assert_refused(&mut bank, &ids, BankError::UnknownAccount(from), |b| {
            b.transfer(from, a, 50)
        });
    }
    // Both unknown: the source is reported.
    assert_refused(
        &mut bank,
        &ids,
        BankError::UnknownAccount(AccountId(5)),
        |b| b.transfer(AccountId(5), AccountId(6), 50),
    );
    assert_refused(
        &mut bank,
        &ids,
        BankError::UnknownAccount(AccountId(0)),
        |b| b.transfer(AccountId(0), AccountId(9), 50),
    );
}

// ---------------------------------------------------------------------------
// Zero amounts
// ---------------------------------------------------------------------------

#[test]
fn zero_amounts_are_refused() {
    let (mut bank, ids) = bank_with(&[1_000, 1_000]);
    let (a, b) = (ids[0], ids[1]);
    assert_refused(&mut bank, &ids, BankError::ZeroAmount, |bk| {
        bk.deposit(a, 0)
    });
    assert_refused(&mut bank, &ids, BankError::ZeroAmount, |bk| {
        bk.withdraw(a, 0)
    });
    assert_refused(&mut bank, &ids, BankError::ZeroAmount, |bk| {
        bk.transfer(a, b, 0)
    });
}

#[test]
fn a_zero_amount_is_reported_before_anything_else() {
    let (mut bank, ids) = bank_with(&[0, MAX]);
    let (a, full) = (ids[0], ids[1]);
    bank.freeze(full).unwrap();
    let zero = BankError::ZeroAmount;
    assert_refused(&mut bank, &ids, zero.clone(), |b| {
        b.deposit(AccountId(9), 0)
    });
    assert_refused(&mut bank, &ids, zero.clone(), |b| {
        b.deposit(AccountId(0), 0)
    });
    assert_refused(&mut bank, &ids, zero.clone(), |b| {
        b.withdraw(AccountId(9), 0)
    });
    assert_refused(&mut bank, &ids, zero.clone(), |b| b.deposit(full, 0));
    assert_refused(&mut bank, &ids, zero.clone(), |b| b.withdraw(full, 0));
    assert_refused(&mut bank, &ids, zero.clone(), |b| {
        b.transfer(AccountId(8), AccountId(9), 0)
    });
    assert_refused(&mut bank, &ids, zero.clone(), |b| b.transfer(a, a, 0));
    assert_refused(&mut bank, &ids, zero.clone(), |b| b.transfer(full, a, 0));
    assert_refused(&mut bank, &ids, zero, |b| b.transfer(a, full, 0));
}

// ---------------------------------------------------------------------------
// Withdrawals
// ---------------------------------------------------------------------------

#[test]
fn overdrawing_reports_the_requested_amount_and_the_balance() {
    let (mut bank, ids) = bank_with(&[1_250]);
    let a = ids[0];
    let expected = BankError::InsufficientFunds {
        account: a,
        needed: 5_000,
        available: 1_250,
    };
    assert_refused(&mut bank, &ids, expected, |b| b.withdraw(a, 5_000));
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: a,
            needed: 1_251,
            available: 1_250,
        },
        |b| b.withdraw(a, 1_251),
    );
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: a,
            needed: MAX,
            available: 1_250,
        },
        |b| b.withdraw(a, MAX),
    );
}

#[test]
fn withdrawing_the_whole_balance_then_one_cent_more() {
    let (mut bank, ids) = bank_with(&[0, 300]);
    let (empty, a) = (ids[0], ids[1]);
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: empty,
            needed: 1,
            available: 0,
        },
        |b| b.withdraw(empty, 1),
    );
    assert_eq!(bank.withdraw(a, 300), Ok(()));
    assert_eq!(bank.balance(a), Ok(0));
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: a,
            needed: 1,
            available: 0,
        },
        |b| b.withdraw(a, 1),
    );
    assert_eq!(
        bank.history(a),
        Ok(&[Txn::Deposit { cents: 300 }, Txn::Withdrawal { cents: 300 }][..])
    );
}

// ---------------------------------------------------------------------------
// Overflow
// ---------------------------------------------------------------------------

#[test]
fn deposits_may_reach_but_not_pass_u64_max() {
    let (mut bank, ids) = bank_with(&[10, 0]);
    let (a, b) = (ids[0], ids[1]);
    assert_eq!(bank.deposit(a, MAX - 10), Ok(()));
    assert_eq!(bank.balance(a), Ok(MAX));
    assert_refused(&mut bank, &ids, BankError::Overflow, |bk| bk.deposit(a, 1));
    assert_refused(&mut bank, &ids, BankError::Overflow, |bk| {
        bk.deposit(a, MAX)
    });
    assert_eq!(
        bank.history(a),
        Ok(&[Txn::Deposit { cents: 10 }, Txn::Deposit { cents: MAX - 10 }][..])
    );
    assert_eq!(bank.deposit(b, MAX), Ok(()));
    assert_refused(&mut bank, &ids, BankError::Overflow, |bk| bk.deposit(b, 7));
}

#[test]
fn transfer_that_would_overflow_leaves_both_sides_untouched() {
    let (mut bank, ids) = bank_with(&[100, MAX]);
    let (a, full) = (ids[0], ids[1]);
    assert_refused(&mut bank, &ids, BankError::Overflow, |b| {
        b.transfer(a, full, 1)
    });
    assert_refused(&mut bank, &ids, BankError::Overflow, |b| {
        b.transfer(a, full, 100)
    });
    assert_eq!(bank.balance(a), Ok(100));
    assert_eq!(bank.history(a), Ok(&[Txn::Deposit { cents: 100 }][..]));
    assert_eq!(bank.history(full), Ok(&[Txn::Deposit { cents: MAX }][..]));
}

#[test]
fn transfer_may_fill_a_balance_to_exactly_u64_max() {
    let (mut bank, ids) = bank_with(&[500, MAX - 100]);
    let (a, b) = (ids[0], ids[1]);
    assert_eq!(bank.transfer(a, b, 100), Ok(()));
    assert_eq!(bank.balance(b), Ok(MAX));
    assert_eq!(bank.balance(a), Ok(400));
    assert_refused(&mut bank, &ids, BankError::Overflow, |bk| {
        bk.transfer(a, b, 1)
    });
    // The other direction still works.
    assert_eq!(bank.transfer(b, a, MAX - 400), Ok(()));
    assert_eq!(bank.balance(a), Ok(MAX));
    assert_eq!(bank.balance(b), Ok(400));
}

#[test]
fn insufficient_funds_is_reported_before_overflow() {
    let (mut bank, ids) = bank_with(&[5, MAX]);
    let (a, full) = (ids[0], ids[1]);
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: a,
            needed: 6,
            available: 5,
        },
        |b| b.transfer(a, full, 6),
    );
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

#[test]
fn transfer_to_the_same_account_is_refused() {
    let (mut bank, ids) = bank_with(&[1_000]);
    let a = ids[0];
    assert_refused(&mut bank, &ids, BankError::SameAccount, |b| {
        b.transfer(a, a, 100)
    });
    // Reported before the funds are looked at...
    assert_refused(&mut bank, &ids, BankError::SameAccount, |b| {
        b.transfer(a, a, 5_000)
    });
    // ...and before the account is looked up.
    assert_refused(&mut bank, &ids, BankError::SameAccount, |b| {
        b.transfer(AccountId(7), AccountId(7), 5)
    });
    assert_refused(&mut bank, &ids, BankError::SameAccount, |b| {
        b.transfer(AccountId(0), AccountId(0), 5)
    });
    assert_eq!(bank.history(a).unwrap().len(), 1);
}

#[test]
fn transfer_with_insufficient_funds_changes_neither_side() {
    let (mut bank, ids) = bank_with(&[1_000, 250]);
    let (a, b) = (ids[0], ids[1]);
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: a,
            needed: 1_001,
            available: 1_000,
        },
        |bk| bk.transfer(a, b, 1_001),
    );
    assert_refused(
        &mut bank,
        &ids,
        BankError::InsufficientFunds {
            account: b,
            needed: 251,
            available: 250,
        },
        |bk| bk.transfer(b, a, 251),
    );
}

#[test]
fn a_successful_transfer_after_failed_ones_records_one_entry_per_side() {
    let (mut bank, ids) = bank_with(&[1_000, 0]);
    let (a, b) = (ids[0], ids[1]);
    let _ = bank.transfer(a, AccountId(3), 400);
    let _ = bank.transfer(a, b, 4_000);
    let _ = bank.transfer(a, a, 400);
    let _ = bank.transfer(a, b, 0);
    assert_eq!(bank.transfer(a, b, 1_000), Ok(()));
    assert_eq!(bank.balance(a), Ok(0));
    assert_eq!(bank.balance(b), Ok(1_000));
    assert_eq!(
        bank.history(a),
        Ok(&[
            Txn::Deposit { cents: 1_000 },
            Txn::TransferOut {
                to: b,
                cents: 1_000
            },
        ][..])
    );
    assert_eq!(
        bank.history(b),
        Ok(&[Txn::TransferIn {
            from: a,
            cents: 1_000
        }][..])
    );
}

// ---------------------------------------------------------------------------
// Freezing
// ---------------------------------------------------------------------------

#[test]
fn freeze_and_unfreeze_toggle_is_frozen() {
    let (mut bank, ids) = bank_with(&[0, 0]);
    let (a, b) = (ids[0], ids[1]);
    assert_eq!(bank.is_frozen(a), Ok(false));
    assert_eq!(bank.freeze(a), Ok(()));
    assert_eq!(bank.is_frozen(a), Ok(true));
    assert_eq!(bank.is_frozen(b), Ok(false));
    assert_eq!(bank.unfreeze(a), Ok(()));
    assert_eq!(bank.is_frozen(a), Ok(false));
}

#[test]
fn freeze_and_unfreeze_are_idempotent_and_leave_no_history() {
    let (mut bank, ids) = bank_with(&[700]);
    let a = ids[0];
    assert_eq!(bank.unfreeze(a), Ok(()));
    assert_eq!(bank.is_frozen(a), Ok(false));
    assert_eq!(bank.freeze(a), Ok(()));
    assert_eq!(bank.freeze(a), Ok(()));
    assert_eq!(bank.is_frozen(a), Ok(true));
    assert_eq!(bank.unfreeze(a), Ok(()));
    assert_eq!(bank.unfreeze(a), Ok(()));
    assert_eq!(bank.is_frozen(a), Ok(false));
    assert_eq!(bank.balance(a), Ok(700));
    assert_eq!(bank.history(a), Ok(&[Txn::Deposit { cents: 700 }][..]));
}

#[test]
fn a_frozen_account_cannot_send_receive_or_withdraw() {
    let (mut bank, ids) = bank_with(&[1_000, 1_000]);
    let (a, b) = (ids[0], ids[1]);
    bank.freeze(a).unwrap();
    let frozen = BankError::Frozen(a);
    assert_refused(&mut bank, &ids, frozen.clone(), |bk| bk.deposit(a, 100));
    assert_refused(&mut bank, &ids, frozen.clone(), |bk| bk.withdraw(a, 100));
    assert_refused(&mut bank, &ids, frozen.clone(), |bk| bk.transfer(a, b, 100));
    assert_refused(&mut bank, &ids, frozen, |bk| bk.transfer(b, a, 100));
}

#[test]
fn a_frozen_account_can_still_be_read() {
    let (mut bank, ids) = bank_with(&[1_000, 0]);
    let (a, b) = (ids[0], ids[1]);
    bank.transfer(a, b, 250).unwrap();
    bank.freeze(a).unwrap();
    assert_eq!(bank.owner(a), Ok("owner1"));
    assert_eq!(bank.balance(a), Ok(750));
    assert_eq!(
        bank.history(a),
        Ok(&[
            Txn::Deposit { cents: 1_000 },
            Txn::TransferOut { to: b, cents: 250 },
        ][..])
    );
    assert_eq!(bank.is_frozen(a), Ok(true));
}

#[test]
fn freezing_one_account_leaves_the_others_alone() {
    let (mut bank, ids) = bank_with(&[1_000, 1_000, 1_000]);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    bank.freeze(b).unwrap();
    assert_eq!(bank.transfer(a, c, 300), Ok(()));
    assert_eq!(bank.deposit(c, 1), Ok(()));
    assert_eq!(bank.withdraw(a, 1), Ok(()));
    assert_eq!(bank.balance(a), Ok(699));
    assert_eq!(bank.balance(c), Ok(1_301));
}

#[test]
fn unfreezing_restores_service() {
    let (mut bank, ids) = bank_with(&[1_000, 0]);
    let (a, b) = (ids[0], ids[1]);
    bank.freeze(a).unwrap();
    bank.freeze(b).unwrap();
    assert_eq!(bank.transfer(a, b, 10), Err(BankError::Frozen(a)));
    bank.unfreeze(a).unwrap();
    assert_eq!(bank.transfer(a, b, 10), Err(BankError::Frozen(b)));
    bank.unfreeze(b).unwrap();
    assert_eq!(bank.transfer(a, b, 10), Ok(()));
    assert_eq!(bank.deposit(b, 5), Ok(()));
    assert_eq!(bank.withdraw(a, 990), Ok(()));
    assert_eq!(bank.balance(a), Ok(0));
    assert_eq!(bank.balance(b), Ok(15));
}

#[test]
fn frozen_is_reported_before_funds_and_overflow() {
    let (mut bank, ids) = bank_with(&[100, MAX]);
    let (a, full) = (ids[0], ids[1]);
    bank.freeze(a).unwrap();
    // withdraw: Frozen before InsufficientFunds.
    assert_refused(&mut bank, &ids, BankError::Frozen(a), |b| {
        b.withdraw(a, 5_000)
    });
    bank.unfreeze(a).unwrap();
    bank.freeze(full).unwrap();
    // deposit: Frozen before Overflow.
    assert_refused(&mut bank, &ids, BankError::Frozen(full), |b| {
        b.deposit(full, 1)
    });
    // transfer: Frozen(to) before InsufficientFunds and Overflow.
    assert_refused(&mut bank, &ids, BankError::Frozen(full), |b| {
        b.transfer(a, full, 5_000)
    });
    assert_refused(&mut bank, &ids, BankError::Frozen(full), |b| {
        b.transfer(a, full, 50)
    });
}

#[test]
fn transfer_check_order_with_frozen_and_unknown_accounts() {
    let (mut bank, ids) = bank_with(&[1_000, 1_000]);
    let (a, b) = (ids[0], ids[1]);
    bank.freeze(a).unwrap();
    bank.freeze(b).unwrap();
    let nobody = AccountId(3);
    // UnknownAccount(to) comes before Frozen(from).
    assert_refused(&mut bank, &ids, BankError::UnknownAccount(nobody), |bk| {
        bk.transfer(a, nobody, 10)
    });
    // UnknownAccount(from) comes before Frozen(to).
    assert_refused(&mut bank, &ids, BankError::UnknownAccount(nobody), |bk| {
        bk.transfer(nobody, b, 10)
    });
    // Frozen(from) comes before Frozen(to).
    assert_refused(&mut bank, &ids, BankError::Frozen(a), |bk| {
        bk.transfer(a, b, 10)
    });
    assert_refused(&mut bank, &ids, BankError::Frozen(b), |bk| {
        bk.transfer(b, a, 10)
    });
    // SameAccount comes before Frozen.
    assert_refused(&mut bank, &ids, BankError::SameAccount, |bk| {
        bk.transfer(a, a, 10)
    });
}

// ---------------------------------------------------------------------------
// A long run against a model of the specification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct ModelAccount {
    owner: String,
    balance: u64,
    history: Vec<Txn>,
    frozen: bool,
}

#[derive(Default)]
struct Model {
    accounts: Vec<ModelAccount>,
}

impl Model {
    fn open(&mut self, owner: &str) -> AccountId {
        self.accounts.push(ModelAccount {
            owner: owner.to_string(),
            balance: 0,
            history: Vec::new(),
            frozen: false,
        });
        AccountId(self.accounts.len() as u32)
    }

    fn slot(&self, id: AccountId) -> Result<usize, BankError> {
        let n = id.0 as usize;
        if n >= 1 && n <= self.accounts.len() {
            Ok(n - 1)
        } else {
            Err(BankError::UnknownAccount(id))
        }
    }

    fn active(&self, id: AccountId) -> Result<usize, BankError> {
        let i = self.slot(id)?;
        if self.accounts[i].frozen {
            Err(BankError::Frozen(id))
        } else {
            Ok(i)
        }
    }

    fn deposit(&mut self, id: AccountId, cents: u64) -> Result<(), BankError> {
        if cents == 0 {
            return Err(BankError::ZeroAmount);
        }
        let i = self.active(id)?;
        let acc = &mut self.accounts[i];
        acc.balance = acc.balance.checked_add(cents).ok_or(BankError::Overflow)?;
        acc.history.push(Txn::Deposit { cents });
        Ok(())
    }

    fn withdraw(&mut self, id: AccountId, cents: u64) -> Result<(), BankError> {
        if cents == 0 {
            return Err(BankError::ZeroAmount);
        }
        let i = self.active(id)?;
        let acc = &mut self.accounts[i];
        if acc.balance < cents {
            return Err(BankError::InsufficientFunds {
                account: id,
                needed: cents,
                available: acc.balance,
            });
        }
        acc.balance -= cents;
        acc.history.push(Txn::Withdrawal { cents });
        Ok(())
    }

    fn transfer(&mut self, from: AccountId, to: AccountId, cents: u64) -> Result<(), BankError> {
        if cents == 0 {
            return Err(BankError::ZeroAmount);
        }
        if from == to {
            return Err(BankError::SameAccount);
        }
        let (f, t) = (self.slot(from)?, self.slot(to)?);
        self.active(from)?;
        self.active(to)?;
        let available = self.accounts[f].balance;
        if available < cents {
            return Err(BankError::InsufficientFunds {
                account: from,
                needed: cents,
                available,
            });
        }
        let new_to = self.accounts[t]
            .balance
            .checked_add(cents)
            .ok_or(BankError::Overflow)?;
        self.accounts[f].balance -= cents;
        self.accounts[f]
            .history
            .push(Txn::TransferOut { to, cents });
        self.accounts[t].balance = new_to;
        self.accounts[t]
            .history
            .push(Txn::TransferIn { from, cents });
        Ok(())
    }

    fn set_frozen(&mut self, id: AccountId, frozen: bool) -> Result<(), BankError> {
        let i = self.slot(id)?;
        self.accounts[i].frozen = frozen;
        Ok(())
    }

    fn state(&self) -> Vec<AccountState> {
        self.accounts
            .iter()
            .map(|a| AccountState {
                owner: a.owner.clone(),
                balance: a.balance,
                history: a.history.clone(),
                frozen: a.frozen,
            })
            .collect()
    }
}

/// xorshift64*, seeded: the same sequence on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_operations_agree_with_the_specification() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut bank = Bank::new();
    let mut model = Model::default();
    for n in 0..4 {
        let owner = format!("user{n}");
        assert_eq!(bank.open(&owner), model.open(&owner));
    }
    let mut failures = 0;
    for step in 0..4_000 {
        let count = model.accounts.len() as u64;
        // Ids from 0 (never valid) to two past the last account.
        let pick_id = |rng: &mut Rng| AccountId(rng.below(count + 3) as u32);
        let id = pick_id(&mut rng);
        let other = pick_id(&mut rng);
        let balance_of = |id: AccountId| {
            model
                .slot(id)
                .map(|i| model.accounts[i].balance)
                .unwrap_or(0)
        };
        let cents = match rng.below(10) {
            0 => 0,
            1 => 1,
            2 => balance_of(id),
            3 => balance_of(id).wrapping_add(1),
            4 => MAX - rng.below(1_000),
            5 => MAX / 3 + rng.below(1_000),
            6 => MAX - balance_of(other),
            _ => 1 + rng.below(50_000),
        };
        let what = rng.below(100);
        let (label, got, want) = match what {
            0 => {
                let owner = format!("user{step}");
                assert_eq!(bank.open(&owner), model.open(&owner));
                ("open", Ok(()), Ok(()))
            }
            1..=25 => ("deposit", bank.deposit(id, cents), model.deposit(id, cents)),
            26..=45 => (
                "withdraw",
                bank.withdraw(id, cents),
                model.withdraw(id, cents),
            ),
            46..=89 => (
                "transfer",
                bank.transfer(id, other, cents),
                model.transfer(id, other, cents),
            ),
            90..=93 => ("freeze", bank.freeze(id), model.set_frozen(id, true)),
            _ => ("unfreeze", bank.unfreeze(id), model.set_frozen(id, false)),
        };
        assert_eq!(
            got, want,
            "step {step}: {label} id={id} other={other} cents={cents}"
        );
        if want.is_err() {
            failures += 1;
        }
        let ids: Vec<AccountId> = (1..=model.accounts.len() as u32).map(AccountId).collect();
        assert_eq!(
            snapshot(&bank, &ids),
            model.state(),
            "step {step}: {label} id={id} other={other} cents={cents} left the bank in the wrong state"
        );
        assert_eq!(bank.len(), model.accounts.len());
    }
    // The run exercised plenty of both outcomes.
    assert!(failures > 500 && failures < 3_500, "{failures} failures");
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

#[test]
fn cli_prints_bank_errors_and_carries_on() {
    let script = "\
open alice
open bob
deposit 1 20.00
deposit 3 5
withdraw 1 50
deposit 1 0
transfer 1 1 5
transfer 1 3 5.00
transfer 3 1 5.00
balance 1
history 1
freeze 2
transfer 1 2 1
deposit 2 1
withdraw 2 1
transfer 2 1 1
balance 2
history 2
unfreeze 2
transfer 1 2 1
balance 1
balance 2
deposit 0 1
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
opened account 2 for bob
ok
error: unknown account 3
error: insufficient funds in account 1: needed 50.00, available 20.00
error: amount must be greater than zero
error: cannot transfer to the same account
error: unknown account 3
error: unknown account 3
20.00
deposit 20.00
ok
error: account 2 is frozen
error: account 2 is frozen
error: account 2 is frozen
error: account 2 is frozen
0.00
no transactions
ok
ok
19.00
1.00
error: unknown account 0
"
    );
}

#[test]
fn cli_reports_overflow() {
    let script = "\
open whale
open minnow
deposit 1 184467440737095516.15
deposit 1 0.01
deposit 2 0.01
transfer 2 1 0.01
balance 1
balance 2
history 2
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for whale
opened account 2 for minnow
ok
error: balance would overflow
ok
error: balance would overflow
184467440737095516.15
0.01
deposit 0.01
"
    );
}

#[test]
fn cli_read_commands_on_unknown_accounts() {
    let script = "\
open alice
balance 2
history 2
freeze 2
unfreeze 2
balance 0
history 1
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
error: unknown account 2
error: unknown account 2
error: unknown account 2
error: unknown account 2
error: unknown account 0
no transactions
"
    );
}

#[test]
fn cli_freeze_and_unfreeze_commands() {
    let script = "\
open alice
deposit 1 3
freeze 1
freeze 1
withdraw 1 1
balance 1
unfreeze 1
unfreeze 1
withdraw 1 1
balance 1
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
ok
ok
ok
error: account 1 is frozen
3.00
ok
ok
ok
2.00
"
    );
}

#[test]
fn cli_freeze_and_unfreeze_usage_errors() {
    let script = "\
open alice
freeze
unfreeze
freeze 1 2
unfreeze 1 1
freeze x
unfreeze -1
deposit 1 1
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
error: usage: freeze <id>
error: usage: unfreeze <id>
error: usage: freeze <id>
error: usage: unfreeze <id>
error: invalid account id 'x'
error: invalid account id '-1'
ok
"
    );
}

#[test]
fn cli_failed_commands_leave_the_bank_unchanged() {
    let mut bank = Bank::new();
    let a = bank.open("alice");
    let b = bank.open("bob");
    bank.deposit(a, 1_000).unwrap();
    bank.deposit(b, MAX).unwrap();
    let out = transcript_with(
        &mut bank,
        "transfer 1 9 1\ntransfer 1 2 1\ntransfer 1 2 11\nwithdraw 1 10.01\n",
    );
    assert_eq!(
        out,
        "\
error: unknown account 9
error: balance would overflow
error: insufficient funds in account 1: needed 11.00, available 10.00
error: insufficient funds in account 1: needed 10.01, available 10.00
"
    );
    assert_eq!(bank.balance(a), Ok(1_000));
    assert_eq!(bank.balance(b), Ok(MAX));
    assert_eq!(bank.history(a), Ok(&[Txn::Deposit { cents: 1_000 }][..]));
    assert_eq!(bank.history(b), Ok(&[Txn::Deposit { cents: MAX }][..]));
}

#[test]
fn the_binary_reports_errors_and_runs_to_the_end() {
    let script = "\
# a script with mistakes in it
open alice
open bob
deposit 1 10
transfer 1 3 4
withdraw 2 1
freeze 1
transfer 1 2 4
unfreeze 1
transfer 1 2 4
frobnicate
balance 1
balance 2
";
    let mut child = Command::new(env!("CARGO_BIN_EXE_ledger"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the ledger binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout,
        "\
opened account 1 for alice
opened account 2 for bob
ok
error: unknown account 3
error: insufficient funds in account 2: needed 1.00, available 0.00
ok
error: account 1 is frozen
ok
ok
error: unknown command 'frobnicate'
6.00
4.00
",
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "exit status {:?}", output.status);
}
