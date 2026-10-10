use calc::lexer::{tokenize, TokenKind};
use calc::CalcError;

fn kinds(src: &str) -> Vec<TokenKind> {
    tokenize(src)
        .expect("tokenizes")
        .into_iter()
        .map(|tok| tok.kind)
        .collect()
}

#[test]
fn operators_and_punctuation() {
    use TokenKind::*;
    assert_eq!(
        kinds("+-*/^(),="),
        [Plus, Minus, Star, Slash, Caret, LParen, RParen, Comma, Equals]
    );
}

#[test]
fn numbers() {
    assert_eq!(
        kinds("42 3.25 .5 7."),
        [
            TokenKind::Number(42.0),
            TokenKind::Number(3.25),
            TokenKind::Number(0.5),
            TokenKind::Number(7.0)
        ]
    );
}

#[test]
fn identifiers() {
    assert_eq!(
        kinds("x rate_2 _tmp π"),
        [
            TokenKind::Ident("x".into()),
            TokenKind::Ident("rate_2".into()),
            TokenKind::Ident("_tmp".into()),
            TokenKind::Ident("π".into())
        ]
    );
}

#[test]
fn whitespace_separates_tokens() {
    assert_eq!(
        kinds("\t1\n+  x "),
        [
            TokenKind::Number(1.0),
            TokenKind::Plus,
            TokenKind::Ident("x".into())
        ]
    );
    assert_eq!(kinds(""), []);
    assert_eq!(kinds("   "), []);
}

#[test]
fn spans_are_byte_ranges() {
    let spans: Vec<_> = tokenize("12 + foo")
        .unwrap()
        .into_iter()
        .map(|t| t.span)
        .collect();
    assert_eq!(spans, [0..2, 3..4, 5..8]);

    // `π` is two bytes long in UTF-8.
    let spans: Vec<_> = tokenize("π*2")
        .unwrap()
        .into_iter()
        .map(|t| t.span)
        .collect();
    assert_eq!(spans, [0..2, 2..3, 3..4]);
}

#[test]
fn unexpected_character() {
    assert_eq!(
        tokenize("1 + $"),
        Err(CalcError::UnexpectedChar { pos: 4, ch: '$' })
    );
}

#[test]
fn invalid_number() {
    assert_eq!(
        tokenize("2 * 1.2.3"),
        Err(CalcError::InvalidNumber {
            pos: 4,
            text: "1.2.3".into()
        })
    );
}
