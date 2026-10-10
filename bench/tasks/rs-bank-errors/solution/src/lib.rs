//! A small in-memory bank ledger.
//!
//! A [`Bank`] holds accounts, each with an owner, a balance in whole cents
//! and a history of [`Txn`]s. The `ledger` binary runs a script of commands
//! against a fresh bank; see [`cli`] for the script language.
//!
//! ```
//! use ledger::{Bank, Txn};
//!
//! let mut bank = Bank::new();
//! let alice = bank.open("alice");
//! let bob = bank.open("bob");
//! bank.deposit(alice, 2_000)?;
//! bank.transfer(alice, bob, 750)?;
//! assert_eq!(bank.balance(alice)?, 1_250);
//! assert_eq!(bank.history(bob)?, &[Txn::TransferIn { from: alice, cents: 750 }]);
//! # Ok::<(), ledger::BankError>(())
//! ```
//!
//! Operations that can fail return a [`BankError`]; a failed operation
//! changes nothing.

pub mod account;
pub mod bank;
pub mod cli;
pub mod command;
pub mod error;
pub mod money;
pub mod txn;

pub use account::AccountId;
pub use bank::Bank;
pub use error::BankError;
pub use txn::Txn;
