# ledger

A small in-memory bank ledger for Rust, and a command-line tool that runs
ledger scripts against it. No dependencies.

```rust
use ledger::{Bank, Txn};

let mut bank = Bank::new();
let alice = bank.open("alice");   // AccountId(1)
let bob = bank.open("bob");       // AccountId(2)
bank.deposit(alice, 2_000)?;      // amounts are whole cents
bank.transfer(alice, bob, 750)?;
assert_eq!(bank.balance(alice)?, 1_250);
assert_eq!(bank.history(bob)?, &[Txn::TransferIn { from: alice, cents: 750 }]);
```

## Library

`Bank`:

- `Bank::new()`: a bank with no accounts.
- `open(owner) -> AccountId`: opens an empty account. Ids are handed out in
  order, starting at `AccountId(1)`.
- `len()`, `is_empty()`: how many accounts have been opened.
- `owner(id)`, `balance(id)` (cents), `history(id)` (a `&[Txn]`, oldest
  first), `is_frozen(id)`.
- `deposit(id, cents)`, `withdraw(id, cents)`, `transfer(from, to, cents)`.
  A transfer records `Txn::TransferOut` on the source and `Txn::TransferIn`
  on the destination.
- `freeze(id)`, `unfreeze(id)`: a frozen account can still be read, but
  can't take part in a deposit, a withdrawal or a transfer.

Everything except `open`, `len` and `is_empty` returns a
`Result<_, BankError>`, and an operation that fails changes nothing.
`BankError` is one of `UnknownAccount(id)`, `InsufficientFunds { account,
needed, available }`, `ZeroAmount`, `SameAccount` (a transfer to the source
account), `Frozen(id)` and `Overflow` (a balance past `u64::MAX` cents).

`Txn` is one history entry: `Deposit { cents }`, `Withdrawal { cents }`,
`TransferOut { to, cents }` or `TransferIn { from, cents }`. Its `Display`
is a statement line such as `transfer 7.50 to 2`.

`money::format_cents(1250)` is `"12.50"`; `money::parse_cents("12.5")` is
`Some(1250)`.

## CLI

`ledger` reads a script from standard input, runs it against a fresh bank and
prints the result of each command on standard output:

```
$ cat demo.ledger
# two accounts
open alice
open bob
deposit 1 20.00
transfer 1 2 7.50
balance 1
history 2
$ cargo run --quiet < demo.ledger
opened account 1 for alice
opened account 2 for bob
ok
ok
12.50
transfer 7.50 from 1
```

| command                         | prints                                       |
|---------------------------------|----------------------------------------------|
| `open <owner>`                  | `opened account <id> for <owner>`            |
| `deposit <id> <amount>`         | `ok`                                         |
| `withdraw <id> <amount>`        | `ok`                                         |
| `transfer <from> <to> <amount>` | `ok`                                         |
| `balance <id>`                  | the balance, e.g. `12.50`                    |
| `history <id>`                  | one line per transaction, or `no transactions` |
| `freeze <id>`                   | `ok`                                         |
| `unfreeze <id>`                 | `ok`                                         |

Amounts are dollars with up to two decimals (`12`, `12.5`, `12.50`). Blank
lines and lines starting with `#` are skipped. A line that is not a valid
command, or a command the bank refuses, prints `error: <reason>` (for
example `error: usage: balance <id>` or `error: unknown account 9`) and the
script carries on.

## Layout

- `src/bank.rs`: `Bank`.
- `src/error.rs`: `BankError`.
- `src/account.rs`: `AccountId` and the per-account record.
- `src/txn.rs`: `Txn`.
- `src/money.rs`: formatting and parsing amounts.
- `src/command.rs`: parsing a script line into a `Command`.
- `src/cli.rs`: running a script (`cli::run`); `src/main.rs` is the binary.

## Tests

Rust 1.80 or newer:

```
cargo test --offline --lib --test bank --test cli
```
