//! `calc`: a small calculator for arithmetic expressions with variables.
//!
//! ```
//! use calc::{eval, run, Env};
//!
//! let mut env = Env::new();
//! env.set("x", 3.0);
//! assert_eq!(eval("2 * x + 1", &env), Ok(7.0));
//! assert_eq!(run("y = x * x", &mut env), Ok(9.0));
//! assert_eq!(env.get("y"), Some(9.0));
//! ```

pub mod ast;
pub mod env;
pub mod error;
pub mod evaluator;
pub mod functions;
pub mod lexer;
pub mod parser;

pub use ast::{BinOp, Expr, Statement, UnaryOp};
pub use env::Env;
pub use error::CalcError;
pub use parser::{parse, parse_statement};

/// Parses and evaluates one expression.
///
/// The whole input is parsed before anything is evaluated, so a syntax error
/// is reported even when the expression also uses an unknown variable.
pub fn eval(src: &str, env: &Env) -> Result<f64, CalcError> {
    let expr = parse(src)?;
    evaluator::evaluate(&expr, env)
}

/// Runs one line of input: `name = expr` evaluates `expr`, binds the result to
/// `name` and returns it; anything else is evaluated like [`eval`]. Nothing is
/// bound when evaluation fails.
pub fn run(src: &str, env: &mut Env) -> Result<f64, CalcError> {
    match parse_statement(src)? {
        Statement::Assign { name, value } => {
            let value = evaluator::evaluate(&value, env)?;
            env.set(name, value);
            Ok(value)
        }
        Statement::Expr(expr) => evaluator::evaluate(&expr, env),
    }
}
