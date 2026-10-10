use ledger::{cli, AccountId, Bank};

/// Runs `script` against a fresh bank and returns everything it printed.
fn transcript(script: &str) -> String {
    let mut bank = Bank::new();
    transcript_with(&mut bank, script)
}

fn transcript_with(bank: &mut Bank, script: &str) -> String {
    let mut out = Vec::new();
    cli::run(bank, script.as_bytes(), &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn runs_a_script() {
    let script = "\
open alice
open bob
deposit 1 20.00
transfer 1 2 7.5
withdraw 2 0.25
balance 1
balance 2
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
opened account 2 for bob
ok
ok
ok
12.50
7.25
"
    );
}

#[test]
fn skips_blank_lines_and_comments() {
    let script = "\
# set up
open alice

   # indented comment
deposit 1 3
";
    assert_eq!(transcript(script), "opened account 1 for alice\nok\n");
}

#[test]
fn owners_can_have_several_words() {
    assert_eq!(
        transcript("open Ada   King Lovelace\n"),
        "opened account 1 for Ada King Lovelace\n"
    );
}

#[test]
fn history_lists_transactions_oldest_first() {
    let script = "\
open alice
open bob
history 1
deposit 1 20
transfer 1 2 7.50
withdraw 1 0.05
history 1
history 2
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
opened account 2 for bob
no transactions
ok
ok
ok
deposit 20.00
transfer 7.50 to 2
withdrawal 0.05
transfer 7.50 from 1
"
    );
}

#[test]
fn lines_that_do_not_parse_print_an_error_and_the_script_goes_on() {
    let script = "\
open alice
close 1
deposit 1
deposit x 5
deposit 1 5.555
open
deposit 1 5
balance 1
";
    assert_eq!(
        transcript(script),
        "\
opened account 1 for alice
error: unknown command 'close'
error: usage: deposit <id> <amount>
error: invalid account id 'x'
error: invalid amount '5.555'
error: usage: open <owner>
ok
5.00
"
    );
}

#[test]
fn runs_against_the_bank_it_is_given() {
    let mut bank = Bank::new();
    let alice = bank.open("alice");
    assert_eq!(transcript_with(&mut bank, "deposit 1 2.50\n"), "ok\n");
    assert_eq!(bank.history(alice).len(), 1);
    assert_eq!(bank.balance(AccountId(1)), 250);
}
