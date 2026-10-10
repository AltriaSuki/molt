use ledger::{AccountId, Bank, Txn};

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
                bank.deposit(id, cents);
            }
            id
        })
        .collect();
    (bank, ids)
}

#[test]
fn open_hands_out_ids_in_order() {
    let mut bank = Bank::new();
    assert!(bank.is_empty());
    assert_eq!(bank.open("alice"), AccountId(1));
    assert_eq!(bank.open("bob"), AccountId(2));
    assert_eq!(bank.len(), 2);
    assert_eq!(bank.owner(AccountId(1)), "alice");
    assert_eq!(bank.owner(AccountId(2)), "bob");
}

#[test]
fn new_accounts_are_empty() {
    let mut bank = Bank::new();
    let id = bank.open("carol");
    assert_eq!(bank.balance(id), 0);
    assert!(bank.history(id).is_empty());
}

#[test]
fn deposit_and_withdraw() {
    let (mut bank, ids) = bank_with(&[0]);
    let a = ids[0];
    bank.deposit(a, 2_000);
    bank.withdraw(a, 350);
    bank.deposit(a, 5);
    assert_eq!(bank.balance(a), 1_655);
    assert_eq!(
        bank.history(a),
        &[
            Txn::Deposit { cents: 2_000 },
            Txn::Withdrawal { cents: 350 },
            Txn::Deposit { cents: 5 },
        ]
    );
}

#[test]
fn withdrawing_the_whole_balance_leaves_zero() {
    let (mut bank, ids) = bank_with(&[1_250]);
    bank.withdraw(ids[0], 1_250);
    assert_eq!(bank.balance(ids[0]), 0);
}

#[test]
fn transfer_moves_money_and_records_both_sides() {
    let (mut bank, ids) = bank_with(&[2_000, 100]);
    let (a, b) = (ids[0], ids[1]);
    bank.transfer(a, b, 750);
    assert_eq!(bank.balance(a), 1_250);
    assert_eq!(bank.balance(b), 850);
    assert_eq!(
        bank.history(a),
        &[
            Txn::Deposit { cents: 2_000 },
            Txn::TransferOut { to: b, cents: 750 },
        ]
    );
    assert_eq!(
        bank.history(b),
        &[
            Txn::Deposit { cents: 100 },
            Txn::TransferIn {
                from: a,
                cents: 750
            },
        ]
    );
}

#[test]
fn accounts_are_independent() {
    let (mut bank, ids) = bank_with(&[500, 500, 500]);
    bank.withdraw(ids[1], 200);
    bank.transfer(ids[2], ids[0], 300);
    let balances: Vec<u64> = ids.iter().map(|&id| bank.balance(id)).collect();
    assert_eq!(balances, [800, 300, 200]);
}

#[test]
#[should_panic(expected = "insufficient funds")]
fn overdrawing_panics() {
    let (mut bank, ids) = bank_with(&[1_000]);
    bank.withdraw(ids[0], 1_001);
}

#[test]
#[should_panic(expected = "insufficient funds")]
fn transferring_more_than_the_balance_panics() {
    let (mut bank, ids) = bank_with(&[1_000, 0]);
    bank.transfer(ids[0], ids[1], 2_000);
}

#[test]
#[should_panic(expected = "unknown account")]
fn reading_an_unknown_account_panics() {
    let (bank, _) = bank_with(&[0, 0]);
    bank.balance(AccountId(3));
}

#[test]
#[should_panic(expected = "must be positive")]
fn depositing_nothing_panics() {
    let (mut bank, ids) = bank_with(&[0]);
    bank.deposit(ids[0], 0);
}
