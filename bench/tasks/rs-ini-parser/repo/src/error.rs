//! The error returned by [`parse`](crate::parse).

use std::fmt;

/// Why [`parse`](crate::parse) rejected its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The 1-based number of the line that could not be parsed.
    pub line: usize,
    /// A human-readable description of the problem.
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}
