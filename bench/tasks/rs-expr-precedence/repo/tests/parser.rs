use calc::{parse, parse_statement, CalcError, Statement};

fn show(src: &str) -> String {
    match parse(src) {
        Ok(expr) => expr.to_string(),
        Err(err) => panic!("failed to parse {src:?}: {err}"),
    }
}

#[test]
fn multiplication_binds_tighter_than_addition() {
    assert_eq!(show("1 + 2 * 3"), "(1 + (2 * 3))");
    assert_eq!(show("1 * 2 + 3"), "((1 * 2) + 3)");
    assert_eq!(show("x / 4 - 1"), "((x / 4) - 1)");
}

#[test]
fn parentheses_group() {
    assert_eq!(show("(1 + 2) * 3"), "((1 + 2) * 3)");
    assert_eq!(show("((x))"), "x");
}

#[test]
fn unary_operators() {
    assert_eq!(show("-x"), "(-x)");
    assert_eq!(show("-3"), "(-3)");
    assert_eq!(show("+2.5"), "(+2.5)");
    assert_eq!(show("--x"), "(-(-x))");
    assert_eq!(show("2 * -x"), "(2 * (-x))");
}

#[test]
fn power_binds_tighter_than_multiplication() {
    assert_eq!(show("x ^ 2"), "(x ^ 2)");
    assert_eq!(show("2 * x ^ 2"), "(2 * (x ^ 2))");
    assert_eq!(show("(x + 1) ^ 2"), "((x + 1) ^ 2)");
}

#[test]
fn function_calls() {
    assert_eq!(show("max(1, x + 1)"), "max(1, (x + 1))");
    assert_eq!(show("sqrt(abs(x))"), "sqrt(abs(x))");
    assert_eq!(show("f()"), "f()");
}

#[test]
fn assignment_statement() {
    match parse_statement("area = w * h").unwrap() {
        Statement::Assign { name, value } => {
            assert_eq!(name, "area");
            assert_eq!(value.to_string(), "(w * h)");
        }
        other => panic!("expected an assignment, got {other:?}"),
    }
    assert!(matches!(
        parse_statement("w * h").unwrap(),
        Statement::Expr(_)
    ));
}

#[test]
fn unexpected_token_names_the_token() {
    for (src, text) in [
        ("1 + * 2", "*"),
        ("1 2", "2"),
        ("(1))", ")"),
        ("x = 1", "="),
    ] {
        match parse(src) {
            Err(CalcError::UnexpectedToken { found, .. }) => assert_eq!(found, text, "for {src:?}"),
            other => panic!("expected UnexpectedToken for {src:?}, got {other:?}"),
        }
    }
}

#[test]
fn unexpected_end() {
    for src in ["", "1 +", "(1 + 2", "max(1,", "-"] {
        assert_eq!(parse(src), Err(CalcError::UnexpectedEnd), "for {src:?}");
    }
}

#[test]
fn lexer_errors_pass_through() {
    assert_eq!(
        parse("2 # 3"),
        Err(CalcError::UnexpectedChar { pos: 2, ch: '#' })
    );
}
