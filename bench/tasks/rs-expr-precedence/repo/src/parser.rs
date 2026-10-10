//! A recursive-descent parser from tokens to [`Expr`].
//!
//! Grammar, from the loosest-binding rule to the tightest:
//!
//! ```text
//! statement := IDENT '=' expr | expr
//! expr      := term (('+' | '-') expr)?
//! term      := power (('*' | '/') term)?
//! power     := unary ('^' unary)*
//! unary     := ('-' | '+') unary | primary
//! primary   := NUMBER | IDENT | IDENT '(' args? ')' | '(' expr ')'
//! args      := expr (',' expr)*
//! ```
//!
//! The whole input must be consumed: anything left over after a complete
//! expression is an `UnexpectedToken` error.

use crate::ast::{BinOp, Expr, Statement, UnaryOp};
use crate::error::CalcError;
use crate::lexer::{tokenize, Token, TokenKind};

/// Parses a single expression.
pub fn parse(src: &str) -> Result<Expr, CalcError> {
    let mut parser = Parser::new(src)?;
    let expr = parser.expr()?;
    parser.finish()?;
    Ok(expr)
}

/// Parses an assignment (`name = expr`) or a bare expression.
pub fn parse_statement(src: &str) -> Result<Statement, CalcError> {
    let mut parser = Parser::new(src)?;
    let statement = parser.statement()?;
    parser.finish()?;
    Ok(statement)
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    next: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Result<Self, CalcError> {
        Ok(Parser {
            src,
            tokens: tokenize(src)?,
            next: 0,
        })
    }

    fn peek_nth(&self, n: usize) -> Option<&TokenKind> {
        self.tokens.get(self.next + n).map(|tok| &tok.kind)
    }

    fn peek(&self) -> Option<&TokenKind> {
        self.peek_nth(0)
    }

    fn bump(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.next).cloned()?;
        self.next += 1;
        Some(tok)
    }

    /// Consumes the next token if it is `kind`.
    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek() == Some(kind) {
            self.next += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind) -> Result<(), CalcError> {
        match self.bump() {
            Some(tok) if &tok.kind == kind => Ok(()),
            Some(tok) => Err(self.unexpected(&tok)),
            None => Err(CalcError::UnexpectedEnd),
        }
    }

    /// Fails if any input is left.
    fn finish(&mut self) -> Result<(), CalcError> {
        match self.bump() {
            Some(tok) => Err(self.unexpected(&tok)),
            None => Ok(()),
        }
    }

    fn unexpected(&self, tok: &Token) -> CalcError {
        CalcError::UnexpectedToken {
            pos: tok.span.end,
            found: self.src[tok.span.clone()].to_string(),
        }
    }

    fn statement(&mut self) -> Result<Statement, CalcError> {
        if let (Some(TokenKind::Ident(name)), Some(TokenKind::Equals)) =
            (self.peek(), self.peek_nth(1))
        {
            let name = name.clone();
            self.next += 2;
            let value = self.expr()?;
            return Ok(Statement::Assign { name, value });
        }
        self.expr().map(Statement::Expr)
    }

    fn expr(&mut self) -> Result<Expr, CalcError> {
        let lhs = self.term()?;
        let op = match self.peek() {
            Some(TokenKind::Plus) => BinOp::Add,
            Some(TokenKind::Minus) => BinOp::Sub,
            _ => return Ok(lhs),
        };
        self.next += 1;
        let rhs = self.expr()?;
        Ok(Expr::binary(op, lhs, rhs))
    }

    fn term(&mut self) -> Result<Expr, CalcError> {
        let lhs = self.power()?;
        let op = match self.peek() {
            Some(TokenKind::Star) => BinOp::Mul,
            Some(TokenKind::Slash) => BinOp::Div,
            _ => return Ok(lhs),
        };
        self.next += 1;
        let rhs = self.term()?;
        Ok(Expr::binary(op, lhs, rhs))
    }

    fn power(&mut self) -> Result<Expr, CalcError> {
        let mut lhs = self.unary()?;
        while self.eat(&TokenKind::Caret) {
            let rhs = self.unary()?;
            lhs = Expr::binary(BinOp::Pow, lhs, rhs);
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, CalcError> {
        let op = match self.peek() {
            Some(TokenKind::Minus) => UnaryOp::Neg,
            Some(TokenKind::Plus) => UnaryOp::Plus,
            _ => return self.primary(),
        };
        self.next += 1;
        let operand = self.unary()?;
        Ok(Expr::unary(op, operand))
    }

    fn primary(&mut self) -> Result<Expr, CalcError> {
        let tok = self.bump().ok_or(CalcError::UnexpectedEnd)?;
        match &tok.kind {
            TokenKind::Number(value) => Ok(Expr::Number(*value)),
            TokenKind::Ident(name) if self.eat(&TokenKind::LParen) => Ok(Expr::Call {
                name: name.clone(),
                args: self.args()?,
            }),
            TokenKind::Ident(name) => Ok(Expr::Var(name.clone())),
            TokenKind::LParen => {
                let inner = self.expr()?;
                self.expect(&TokenKind::RParen)?;
                Ok(inner)
            }
            _ => Err(self.unexpected(&tok)),
        }
    }

    /// The arguments of a call, after its `(`, up to and including the `)`.
    fn args(&mut self) -> Result<Vec<Expr>, CalcError> {
        let mut args = Vec::new();
        if self.eat(&TokenKind::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            match self.bump() {
                Some(tok) if tok.kind == TokenKind::Comma => continue,
                Some(tok) if tok.kind == TokenKind::RParen => return Ok(args),
                Some(tok) => return Err(self.unexpected(&tok)),
                None => return Err(CalcError::UnexpectedEnd),
            }
        }
    }
}
