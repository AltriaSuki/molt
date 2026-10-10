//! The error type shared by the lexer, the parser and the evaluator.

use std::fmt;

/// Everything that can go wrong between source text and a number.
///
/// Positions (`pos`) are 0-based byte offsets into the source string, so
/// `&src[pos..]` starts at the character the error is about.
#[derive(Debug, Clone, PartialEq)]
pub enum CalcError {
    /// A character that cannot start any token, such as `$` or `#`.
    UnexpectedChar { pos: usize, ch: char },
    /// A number literal that does not parse, such as `1.2.3`.
    InvalidNumber { pos: usize, text: String },
    /// A token that is not allowed where it appears. `pos` is where the
    /// token starts and `found` is its text as written in the source.
    UnexpectedToken { pos: usize, found: String },
    /// The input ended in the middle of an expression.
    UnexpectedEnd,
    /// A variable that is not defined in the environment.
    UnknownVariable(String),
    /// A call to a function that does not exist.
    UnknownFunction(String),
    /// A built-in function called with the wrong number of arguments.
    WrongArity {
        name: String,
        expected: usize,
        found: usize,
    },
    /// Division by zero.
    DivisionByZero,
}

impl CalcError {
    /// The byte offset the error points at, for errors that have one.
    pub fn pos(&self) -> Option<usize> {
        match self {
            CalcError::UnexpectedChar { pos, .. }
            | CalcError::InvalidNumber { pos, .. }
            | CalcError::UnexpectedToken { pos, .. } => Some(*pos),
            _ => None,
        }
    }
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::UnexpectedChar { pos, ch } => {
                write!(f, "unexpected character {ch:?} at offset {pos}")
            }
            CalcError::InvalidNumber { pos, text } => {
                write!(f, "invalid number {text:?} at offset {pos}")
            }
            CalcError::UnexpectedToken { pos, found } => {
                write!(f, "unexpected {found:?} at offset {pos}")
            }
            CalcError::UnexpectedEnd => f.write_str("unexpected end of input"),
            CalcError::UnknownVariable(name) => write!(f, "unknown variable `{name}`"),
            CalcError::UnknownFunction(name) => write!(f, "unknown function `{name}`"),
            CalcError::WrongArity {
                name,
                expected,
                found,
            } => {
                let plural = if *expected == 1 { "" } else { "s" };
                write!(f, "`{name}` takes {expected} argument{plural}, got {found}")
            }
            CalcError::DivisionByZero => f.write_str("division by zero"),
        }
    }
}

impl std::error::Error for CalcError {}
