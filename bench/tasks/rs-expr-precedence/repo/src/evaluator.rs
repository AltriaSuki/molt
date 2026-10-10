//! Evaluates a syntax tree against an [`Env`].

use crate::ast::{BinOp, Expr, UnaryOp};
use crate::env::Env;
use crate::error::CalcError;
use crate::functions;

/// Evaluates `expr`, operands left to right, stopping at the first error.
pub fn evaluate(expr: &Expr, env: &Env) -> Result<f64, CalcError> {
    match expr {
        Expr::Number(value) => Ok(*value),
        Expr::Var(name) => env
            .get(name)
            .ok_or_else(|| CalcError::UnknownVariable(name.clone())),
        Expr::Unary { op, operand } => {
            let value = evaluate(operand, env)?;
            Ok(match op {
                UnaryOp::Neg => -value,
                UnaryOp::Plus => value,
            })
        }
        Expr::Binary { op, lhs, rhs } => {
            let lhs = evaluate(lhs, env)?;
            let rhs = evaluate(rhs, env)?;
            apply(*op, lhs, rhs)
        }
        Expr::Call { name, args } => {
            let values = args
                .iter()
                .map(|arg| evaluate(arg, env))
                .collect::<Result<Vec<_>, _>>()?;
            functions::call(name, &values)
        }
    }
}

fn apply(op: BinOp, lhs: f64, rhs: f64) -> Result<f64, CalcError> {
    match op {
        BinOp::Add => Ok(lhs + rhs),
        BinOp::Sub => Ok(lhs - rhs),
        BinOp::Mul => Ok(lhs * rhs),
        BinOp::Div if rhs == 0.0 => Err(CalcError::DivisionByZero),
        BinOp::Div => Ok(lhs / rhs),
        BinOp::Pow => Ok(lhs.powf(rhs)),
    }
}
