use calc::{eval, run, CalcError, Env};

fn value(src: &str, env: &Env) -> f64 {
    match eval(src, env) {
        Ok(v) => v,
        Err(err) => panic!("failed to evaluate {src:?}: {err}"),
    }
}

#[test]
fn arithmetic() {
    let env = Env::new();
    assert_eq!(value("1 + 2 * 3", &env), 7.0);
    assert_eq!(value("(1 + 2) * 3", &env), 9.0);
    assert_eq!(value("7 / 2", &env), 3.5);
    assert_eq!(value("2 ^ 10", &env), 1024.0);
    assert_eq!(value("-3 + 5", &env), 2.0);
    assert_eq!(value("2 * -3", &env), -6.0);
    assert_eq!(value(".5 + 7.", &env), 7.5);
}

#[test]
fn variables() {
    let env = Env::from_iter([("x", 3.0), ("rate_2", 0.5)]);
    assert_eq!(value("x * x + 1", &env), 10.0);
    assert_eq!(value("rate_2 * (x + 1)", &env), 2.0);
    assert_eq!(value("2 ^ x", &env), 8.0);
}

#[test]
fn constants() {
    let env = Env::with_constants();
    assert_eq!(value("pi", &env), std::f64::consts::PI);
    assert_eq!(value("tau / 2", &env), std::f64::consts::PI);
}

#[test]
fn unknown_variable() {
    assert_eq!(
        eval("x + 1", &Env::new()),
        Err(CalcError::UnknownVariable("x".into()))
    );
}

#[test]
fn functions() {
    let env = Env::from_iter([("x", -2.5)]);
    assert_eq!(value("sqrt(16)", &env), 4.0);
    assert_eq!(value("abs(x)", &env), 2.5);
    assert_eq!(value("max(2, 7) + min(2, 7)", &env), 9.0);
    assert_eq!(value("hypot(3, 4)", &env), 5.0);
    assert_eq!(value("floor(x) + ceil(x)", &env), -5.0);
}

#[test]
fn function_errors() {
    let env = Env::new();
    assert_eq!(
        eval("cbrt(8)", &env),
        Err(CalcError::UnknownFunction("cbrt".into()))
    );
    assert_eq!(
        eval("sqrt(1, 2)", &env),
        Err(CalcError::WrongArity {
            name: "sqrt".into(),
            expected: 1,
            found: 2
        })
    );
}

#[test]
fn division_by_zero() {
    let env = Env::new();
    assert_eq!(eval("1 / 0", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("1 / (2 - 2)", &env), Err(CalcError::DivisionByZero));
    assert_eq!(eval("0 / 4", &env), Ok(0.0));
}

#[test]
fn syntax_errors_come_before_evaluation() {
    assert!(matches!(
        eval("y + * 2", &Env::new()),
        Err(CalcError::UnexpectedToken { .. })
    ));
}

#[test]
fn run_binds_assignments() {
    let mut env = Env::new();
    assert_eq!(run("x = 2 + 3", &mut env), Ok(5.0));
    assert_eq!(env.get("x"), Some(5.0));
    assert_eq!(run("x * 2", &mut env), Ok(10.0));
    assert_eq!(run("x = x + 1", &mut env), Ok(6.0));
    assert_eq!(env.get("x"), Some(6.0));
}

#[test]
fn run_does_not_bind_on_error() {
    let mut env = Env::new();
    assert_eq!(
        run("y = z + 1", &mut env),
        Err(CalcError::UnknownVariable("z".into()))
    );
    assert_eq!(env.get("y"), None);
}
