//! The error returned by [`parse`](crate::parse).

use std::fmt;

/// Why [`parse`](crate::parse) rejected its input: the first bad line and
/// what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The 1-based number of the line that could not be parsed. Blank and
    /// comment lines count.
    pub line: usize,
    /// What is wrong with that line.
    pub kind: ErrorKind,
}

/// The kinds of problem [`parse`](crate::parse) reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// A `[` line with no `]`, an empty name, or text after the `]` that is
    /// not a comment.
    BadSection,
    /// An entry line without `=`.
    MissingEquals,
    /// An entry line with nothing before the `=`.
    EmptyKey,
    /// A backslash in a quoted value followed by this character, which is
    /// not one of `"`, `\`, `n`, `t`.
    BadEscape(char),
    /// A quoted value whose closing quote never comes.
    UnterminatedQuote,
    /// Something other than whitespace or a comment after a closing quote.
    TrailingAfterQuote,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::BadSection => f.write_str("malformed section header"),
            ErrorKind::MissingEquals => f.write_str("missing '=' in entry"),
            ErrorKind::EmptyKey => f.write_str("empty key"),
            ErrorKind::BadEscape(c) => write!(f, "unknown escape '\\{c}'"),
            ErrorKind::UnterminatedQuote => f.write_str("unterminated quoted value"),
            ErrorKind::TrailingAfterQuote => f.write_str("unexpected text after closing quote"),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.kind)
    }
}

impl std::error::Error for ParseError {}
