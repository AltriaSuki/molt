use ledger::{AccountId, Bank, BankError, Txn};

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

#[test]
fn open_hands_out_ids_in_order() {
    let mut bank = Bank::new();
    assert!(bank.is_empty());
    assert_eq!(bank.open("alice"), AccountId(1));
    assert_eq!(bank.open("bob"), AccountId(2));
    assert_eq!(bank.len(), 2);
    assert_eq!(bank.owner(AccountId(1)), Ok("alice"));
    assert_eq!(bank.owner(AccountId(2)), Ok("bob"));
}

#[test]
fn new_accounts_are_empty() {
    let mut bank = Bank::new();
    let id = bank.open("carol");
    assert_eq!(bank.balance(id), Ok(0));
    assert!(bank.history(id).unwrap().is_empty());
}

#[test]
fn deposit_and_withdraw() {
    let (mut bank, ids) = bank_with(&[0]);
    let a = ids[0];
    bank.deposit(a, 2_000).unwrap();
    bank.withdraw(a, 350).unwrap();
    bank.deposit(a, 5).unwrap();
    assert_eq!(bank.balance(a), Ok(1_655));
    assert_eq!(
        bank.history(a).unwrap(),
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
    bank.withdraw(ids[0], 1_250).unwrap();
    assert_eq!(bank.balance(ids[0]), Ok(0));
}

#[test]
fn transfer_moves_money_and_records_both_sides() {
    let (mut bank, ids) = bank_with(&[2_000, 100]);
    let (a, b) = (ids[0], ids[1]);
    bank.transfer(a, b, 750).unwrap();
    assert_eq!(bank.balance(a), Ok(1_250));
    assert_eq!(bank.balance(b), Ok(850));
    assert_eq!(
        bank.history(a).unwrap(),
        &[
            Txn::Deposit { cents: 2_000 },
            Txn::TransferOut { to: b, cents: 750 },
        ]
    );
    assert_eq!(
        bank.history(b).unwrap(),
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
    bank.withdraw(ids[1], 200).unwrap();
    bank.transfer(ids[2], ids[0], 300).unwrap();
    let balances: Vec<u64> = ids.iter().map(|&id| bank.balance(id).unwrap()).collect();
    assert_eq!(balances, [800, 300, 200]);
}

#[test]
fn overdrawing_is_refused() {
    let (mut bank, ids) = bank_with(&[1_000]);
    assert_eq!(
        bank.withdraw(ids[0], 1_001),
        Err(BankError::InsufficientFunds {
            account: ids[0],
            needed: 1_001,
            available: 1_000,
        })
    );
    assert_eq!(bank.balance(ids[0]), Ok(1_000));
}

#[test]
fn transferring_more_than_the_balance_is_refused() {
    let (mut bank, ids) = bank_with(&[1_000, 0]);
    assert_eq!(
        bank.transfer(ids[0], ids[1], 2_000),
        Err(BankError::InsufficientFunds {
            account: ids[0],
            needed: 2_000,
            available: 1_000,
        })
    );
    assert_eq!(bank.balance(ids[0]), Ok(1_000));
    assert_eq!(bank.balance(ids[1]), Ok(0));
}

#[test]
fn reading_an_unknown_account_is_an_error() {
    let (bank, _) = bank_with(&[0, 0]);
    assert_eq!(
        bank.balance(AccountId(3)),
        Err(BankError::UnknownAccount(AccountId(3)))
    );
}

#[test]
fn depositing_nothing_is_refused() {
    let (mut bank, ids) = bank_with(&[0]);
    assert_eq!(bank.deposit(ids[0], 0), Err(BankError::ZeroAmount));
    assert!(bank.history(ids[0]).unwrap().is_empty());
}

#[test]
fn frozen_accounts_can_be_read_but_not_used() {
    let (mut bank, ids) = bank_with(&[1_000, 0]);
    bank.freeze(ids[0]).unwrap();
    assert_eq!(bank.is_frozen(ids[0]), Ok(true));
    assert_eq!(bank.withdraw(ids[0], 1), Err(BankError::Frozen(ids[0])));
    assert_eq!(bank.balance(ids[0]), Ok(1_000));
    bank.unfreeze(ids[0]).unwrap();
    bank.transfer(ids[0], ids[1], 1).unwrap();
    assert_eq!(bank.balance(ids[1]), Ok(1));
}
