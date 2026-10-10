//! Splits source text into tokens.
//!
//! Whitespace (anything `char::is_whitespace` accepts) separates tokens and is
//! otherwise ignored. Every token records the byte range it was read from.

use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;

use crate::error::CalcError;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// A number literal: `42`, `3.25`, `.5` or `7.`.
    Number(f64),
    /// A name: a letter or `_`, then letters, digits and `_`.
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
    Comma,
    Equals,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range of the token in the source.
    pub span: Range<usize>,
}

/// Tokenizes the whole input, stopping at the first error.
pub fn tokenize(src: &str) -> Result<Vec<Token>, CalcError> {
    Lexer::new(src).collect()
}

/// An iterator over the tokens of a source string.
pub struct Lexer<'a> {
    src: &'a str,
    chars: Peekable<CharIndices<'a>>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src,
            chars: src.char_indices().peekable(),
        }
    }

    /// Byte offset of the next unread character, or the input length at the end.
    fn offset(&mut self) -> usize {
        self.chars.peek().map_or(self.src.len(), |&(i, _)| i)
    }

    fn skip_whitespace(&mut self) {
        while self.chars.next_if(|&(_, c)| c.is_whitespace()).is_some() {}
    }

    /// Reads the rest of a number whose first character started at `start`.
    fn number(&mut self, start: usize) -> Result<Token, CalcError> {
        while self
            .chars
            .next_if(|&(_, c)| c.is_ascii_digit() || c == '.')
            .is_some()
        {}
        let end = self.offset();
        let text = &self.src[start..end];
        let value = text.parse::<f64>().map_err(|_| CalcError::InvalidNumber {
            pos: start,
            text: text.to_string(),
        })?;
        Ok(Token {
            kind: TokenKind::Number(value),
            span: start..end,
        })
    }

    /// Reads the rest of a name whose first character started at `start`.
    fn ident(&mut self, start: usize) -> Token {
        while self
            .chars
            .next_if(|&(_, c)| c.is_alphanumeric() || c == '_')
            .is_some()
        {}
        let end = self.offset();
        Token {
            kind: TokenKind::Ident(self.src[start..end].to_string()),
            span: start..end,
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Result<Token, CalcError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.skip_whitespace();
        let (start, c) = self.chars.next()?;
        let kind = match c {
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '^' => TokenKind::Caret,
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            ',' => TokenKind::Comma,
            '=' => TokenKind::Equals,
            c if c.is_ascii_digit() || c == '.' => return Some(self.number(start)),
            c if c.is_alphabetic() || c == '_' => return Some(Ok(self.ident(start))),
            ch => return Some(Err(CalcError::UnexpectedChar { pos: start, ch })),
        };
        Some(Ok(Token {
            kind,
            span: start..start + c.len_utf8(),
        }))
    }
}
