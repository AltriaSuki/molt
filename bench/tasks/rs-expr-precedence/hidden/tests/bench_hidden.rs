//! Precedence, associativity, the `%` operator and error positions.

use std::io::Write;
use std::process::{Command, Stdio};

use calc::lexer::{tokenize, TokenKind};
use calc::{eval, parse, parse_statement, run, BinOp, CalcError, Env, Expr};

fn value(src: &str, env: &Env) -> f64 {
    match eval(src, env) {
        Ok(v) => v,
        Err(err) => panic!("failed to evaluate {src:?}: {err:?}"),
    }
}

fn check_values(cases: &[(&str, f64)], env: &Env) {
    for &(src, expected) in cases {
        assert_eq!(value(src, env), expected, "value of {src:?}");
    }
}

fn show(src: &str) -> String {
    match parse(src) {
        Ok(expr) => expr.to_string(),
        Err(err) => panic!("failed to parse {src:?}: {err:?}"),
    }
}

fn check_shapes(cases: &[(&str, &str)]) {
    for &(src, expected) in cases {
        assert_eq!(show(src), expected, "grouping of {src:?}");
    }
}

fn check_unexpected(cases: &[(&str, usize, &str)]) {
    for &(src, pos, found) in cases {
        assert_eq!(
            parse(src),
            Err(CalcError::UnexpectedToken {
                pos,
                found: found.to_string()
            }),
            "error for {src:?}"
        );
    }
}

// ---- unary signs and `^` ----

#[test]
fn unary_minus_applies_to_the_whole_power() {
    check_values(
        &[
            ("-2^2", -4.0),
            ("-2 ^ 2", -4.0),
            ("- 3 ^ 2", -9.0),
            ("-(2)^2", -4.0),
            ("(-2)^2", 4.0),
            ("--2^2", 4.0),
            ("-+2^2", -4.0),
            ("+-2^2", -4.0),
            ("2 * -3 ^ 2", -18.0),
            ("1 - -2 ^ 2", 5.0),
            ("-2^2 + 10", 6.0),
            ("-sqrt(4)^2", -4.0),
        ],
        &Env::new(),
    );
}

#[test]
fn unary_minus_grouping() {
    check_shapes(&[
        ("-x ^ 2", "(-(x ^ 2))"),
        ("-x^y^z", "(-(x ^ (y ^ z)))"),
        ("--x^2", "(-(-(x ^ 2)))"),
        ("+x^2", "(+(x ^ 2))"),
        ("2 * -x ^ 2", "(2 * (-(x ^ 2)))"),
        ("-(x) ^ 2", "(-(x ^ 2))"),
        ("(-x) ^ 2", "((-x) ^ 2)"),
    ]);
}

#[test]
fn unary_minus_binds_tighter_than_multiplication() {
    check_shapes(&[
        ("-x * y", "((-x) * y)"),
        ("-x / y", "((-x) / y)"),
        ("-x + y", "((-x) + y)"),
        ("x - -y", "(x - (-y))"),
    ]);
}

#[test]
fn power_is_right_associative() {
    check_values(
        &[
            ("2^3^2", 512.0),
            ("2 ^ 2 ^ 3", 256.0),
            ("2^3^0", 2.0),
            ("(2^3)^2", 64.0),
            ("3^2^0^5", 3.0),
            ("-2^3^2", -512.0),
            ("2 * 2^3^2", 1024.0),
        ],
        &Env::new(),
    );
}

#[test]
fn power_grouping() {
    check_shapes(&[
        ("a ^ b ^ c", "(a ^ (b ^ c))"),
        ("a ^ b ^ c ^ d", "(a ^ (b ^ (c ^ d)))"),
        ("(a ^ b) ^ c", "((a ^ b) ^ c)"),
        ("a * b ^ c ^ d", "(a * (b ^ (c ^ d)))"),
    ]);
}

#[test]
fn exponent_may_have_a_sign() {
    check_values(
        &[
            ("2^-1", 0.5),
            ("2 ^ -2", 0.25),
            ("2^+3", 8.0),
            ("2^--1", 2.0),
            ("-2^-1", -0.5),
            ("2^-1^2", 0.5),
            ("2 ^ -1 * 4", 2.0),
            ("4 * 2 ^ -2", 1.0),
        ],
        &Env::new(),
    );
}

#[test]
fn signed_exponent_grouping() {
    check_shapes(&[
        ("x ^ -y", "(x ^ (-y))"),
        ("x ^ +y", "(x ^ (+y))"),
        ("x ^ -y ^ z", "(x ^ (-(y ^ z)))"),
        ("x ^ -y * z", "((x ^ (-y)) * z)"),
        ("-x ^ -y", "(-(x ^ (-y)))"),
    ]);
}

// ---- left associativity ----

#[test]
fn subtraction_is_left_associative() {
    check_values(
        &[
            ("10 - 2 - 3", 5.0),
            ("1 - 1 - 1 - 1", -2.0),
            ("2 - 3 + 4", 3.0),
            ("0 - 1 + 1", 0.0),
            ("10 - 2 * 3 - 1", 3.0),
            ("1 - -2 - 3", 0.0),
        ],
        &Env::new(),
    );
}

#[test]
fn division_is_left_associative() {
    check_values(
        &[
            ("100 / 10 / 5", 2.0),
            ("8 / 2 * 4", 16.0),
            ("64 / 4 / 4 / 2", 2.0),
            ("1 / 2 / 4", 0.125),
            ("12 / 3 * 2 / 4", 2.0),
            ("1 + 8 / 2 / 2", 3.0),
        ],
        &Env::new(),
    );
}

#[test]
fn left_associative_grouping() {
    check_shapes(&[
        ("a - b - c", "((a - b) - c)"),
        ("a / b / c", "((a / b) / c)"),
        ("a - b + c", "((a - b) + c)"),
        ("a / b * c", "((a / b) * c)"),
        ("a + b * c - d", "((a + (b * c)) - d)"),
        ("a - b - c - d", "(((a - b) - c) - d)"),
        ("a * b / c * d", "(((a * b) / c) * d)"),
    ]);
}

#[test]
fn division_by_zero_with_left_grouping() {
    let env = Env::new();
    assert_eq!(eval("0 / 5 / 2", &env), Ok(0.0));
    assert_eq!(eval("6 / 3 / 0", &env), Err(CalcError::DivisionByZero));
    assert_eq!(
        eval("4 / 2 / (1 - 1)", &env),
        Err(CalcError::DivisionByZero)
    );
}

// ---- `%` ----

#[test]
fn percent_is_a_token() {
    let tokens = tokenize("7 % 2").expect("tokenizes");
    let kinds: Vec<_> = tokens.iter().map(|tok| tok.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            TokenKind::Number(7.0),
            TokenKind::Percent,
            TokenKind::Number(2.0)
        ]
    );
    assert_eq!(tokens[1].span, 2..3);
    let kinds: Vec<_> = tokenize("%%")
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .collect();
    assert_eq!(kinds, [TokenKind::Percent, TokenKind::Percent]);
}

#[test]
fn remainder_operator_in_the_tree() {
    assert_eq!(BinOp::Rem.symbol(), "%");
    assert_eq!(
        parse("7 % 2"),
        Ok(Expr::binary(
            BinOp::Rem,
            Expr::Number(7.0),
            Expr::Number(2.0)
        ))
    );
}

#[test]
fn remainder_values() {
    check_values(
        &[
            ("7 % 3", 1.0),
            ("7.5 % 2", 1.5),
            ("-7 % 3", -1.0),
            ("7 % -3", 1.0),
            ("-7 % -3", -1.0),
            ("6 % 3", 0.0),
            ("0 % 5", 0.0),
            ("2 % 5", 2.0),
            ("5.5 % 1.5", 1.0),
        ],
        &Env::new(),
    );
}

#[test]
fn remainder_precedence() {
    check_values(
        &[
            ("1 + 7 % 4", 4.0),
            ("7 % 4 + 1", 4.0),
            ("10 - 7 % 4", 7.0),
            ("2 * 7 % 4", 2.0),
            ("17 % 5 * 2", 4.0),
            ("10 % 4 % 3", 2.0),
            ("100 / 10 % 3", 1.0),
            ("20 % 6 / 2", 1.0),
            ("2 ^ 3 % 5", 3.0),
            ("7 % 2 ^ 2", 3.0),
            ("-7 % 4", -3.0),
            ("-2 ^ 2 % 3", -1.0),
        ],
        &Env::new(),
    );
}

#[test]
fn remainder_grouping() {
    check_shapes(&[
        ("a % b", "(a % b)"),
        ("a % b % c", "((a % b) % c)"),
        ("a + b % c", "(a + (b % c))"),
        ("a % b - c", "((a % b) - c)"),
        ("a * b % c", "((a * b) % c)"),
        ("a % b / c", "((a % b) / c)"),
        ("a % b ^ c", "(a % (b ^ c))"),
        ("-a % b", "((-a) % b)"),
        ("a % -b", "(a % (-b))"),
    ]);
}

#[test]
fn remainder_by_zero_is_an_error() {
    let env = Env::from_iter([("x", 7.0), ("z", 0.0)]);
    assert_eq!(eval("5 % 0", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("0 % 0", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("5 % (3 - 3)", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("x % z", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("1 + x % z * 2", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("z % x", &env), Ok(0.0));
}

#[test]
fn remainder_with_variables() {
    let mut env = Env::from_iter([("x", 17.0), ("y", 5.0)]);
    assert_eq!(value("x % y", &env), 2.0);
    assert_eq!(value("x % y * 2", &env), 4.0);
    assert_eq!(value("-x % y", &env), -2.0);
    assert_eq!(run("r = x % y", &mut env), Ok(2.0));
    assert_eq!(env.get("r"), Some(2.0));
    assert_eq!(
        eval("2 % q", &env),
        Err(CalcError::UnknownVariable("q".into()))
    );
}

// ---- variables and statements ----

#[test]
fn precedence_with_variables() {
    let env = Env::from_iter([("x", 2.0), ("y", 3.0)]);
    check_values(
        &[
            ("-x^y", -8.0),
            ("x^-y", 0.125),
            ("x^y^2", 512.0),
            ("y - x - 1", 0.0),
            ("-x^2 + y", -1.0),
            ("x * y / x / y", 1.0),
        ],
        &env,
    );
}

#[test]
fn unknown_variables_are_still_reported() {
    let env = Env::from_iter([("x", 2.0)]);
    for src in ["1 + q ^ 2", "-q^2", "x ^ -q", "x - x - q", "q % x"] {
        assert_eq!(
            eval(src, &env),
            Err(CalcError::UnknownVariable("q".into())),
            "for {src:?}"
        );
    }
}

#[test]
fn run_uses_the_same_rules() {
    let mut env = Env::new();
    assert_eq!(run("x = -2^2", &mut env), Ok(-4.0));
    assert_eq!(env.get("x"), Some(-4.0));
    assert_eq!(run("y = 10 - 2 - 3", &mut env), Ok(5.0));
    assert_eq!(run("z = x % 3", &mut env), Ok(-1.0));
    assert_eq!(run("w = 2 ^ y ^ 0", &mut env), Ok(2.0));
    assert_eq!(env.get("w"), Some(2.0));
}

// ---- error positions ----

#[test]
fn unexpected_token_points_at_the_token_start() {
    check_unexpected(&[
        ("1 + * 2", 4, "*"),
        ("1 2", 2, "2"),
        ("(1))", 3, ")"),
        ("(1 2)", 3, "2"),
        ("x = 1", 2, "="),
        (")", 0, ")"),
        ("  )", 2, ")"),
        ("2 ^ ^ 3", 4, "^"),
        ("2 * / 3", 4, "/"),
        ("(,", 1, ","),
        ("f(1,)", 4, ")"),
        ("1 +\t* 2", 4, "*"),
    ]);
}

#[test]
fn unexpected_multi_character_tokens() {
    check_unexpected(&[
        ("foo bar", 4, "bar"),
        ("12 34", 3, "34"),
        ("max(1 2)", 6, "2"),
        ("1 + 2 sqrt(4)", 6, "sqrt"),
        ("(1 + 2) 345", 8, "345"),
        ("2.5 x_1", 4, "x_1"),
    ]);
}

#[test]
fn unexpected_token_positions_are_byte_offsets() {
    check_unexpected(&[("π π", 3, "π"), ("π + * 2", 5, "*"), ("éé 1", 5, "1")]);
}

#[test]
fn unexpected_percent_positions() {
    check_unexpected(&[
        ("% 3", 0, "%"),
        ("1 % % 2", 4, "%"),
        ("1 + % 2", 4, "%"),
        ("(7 %)", 4, ")"),
    ]);
}

#[test]
fn unexpected_token_in_statements() {
    assert_eq!(
        parse_statement("x = = 1"),
        Err(CalcError::UnexpectedToken {
            pos: 4,
            found: "=".into()
        })
    );
    let mut env = Env::new();
    assert_eq!(
        run("value = 3 4", &mut env),
        Err(CalcError::UnexpectedToken {
            pos: 10,
            found: "4".into()
        })
    );
    assert_eq!(env.get("value"), None);
    assert_eq!(
        eval("nope - * 2", &Env::new()),
        Err(CalcError::UnexpectedToken {
            pos: 7,
            found: "*".into()
        })
    );
}

#[test]
fn error_pos_and_message() {
    let err = parse("1 + * 2").unwrap_err();
    assert_eq!(err.pos(), Some(4));
    assert_eq!(err.to_string(), "unexpected \"*\" at offset 4");
    let err = parse("foo bar").unwrap_err();
    assert_eq!(err.pos(), Some(4));
}

#[test]
fn unexpected_end_cases() {
    for src in ["2 ^", "-", "7 %", "2 ^ -", "1 - 2 -", "(1 % 2", "8 / 4 /"] {
        assert_eq!(parse(src), Err(CalcError::UnexpectedEnd), "for {src:?}");
    }
    assert_eq!(parse_statement("x ="), Err(CalcError::UnexpectedEnd));
}

#[test]
fn lexer_error_positions_are_unchanged() {
    assert_eq!(
        parse("2 ^ $"),
        Err(CalcError::UnexpectedChar { pos: 4, ch: '$' })
    );
    assert_eq!(
        parse("1 % 1.2.3"),
        Err(CalcError::InvalidNumber {
            pos: 4,
            text: "1.2.3".into()
        })
    );
}

// ---- command line ----

fn calc_cli(input: &str) -> (String, String, bool) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_calc"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start calc");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait for calc");
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
        out.status.success(),
    )
}

#[test]
fn cli_uses_the_new_rules() {
    let (stdout, stderr, success) =
        calc_cli("-2^2\n2^3^2\n10 - 2 - 3\n100 / 10 / 5\n7 % 4\nx = -7 % 3\n2^-1\n");
    assert_eq!(stdout, "-4\n512\n5\n2\n3\n-1\n0.5\n");
    assert_eq!(stderr, "");
    assert!(success);
}

#[test]
fn cli_caret_is_under_the_token() {
    let (stdout, stderr, success) = calc_cli("1 + * 2\n5 % 0\n");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("line 1: error: unexpected \"*\" at offset 4\n    1 + * 2\n        ^\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("line 2: error: division by zero"),
        "{stderr}"
    );
    assert!(!success);
}
