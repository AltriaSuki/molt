//! Turning INI source text into an [`Ini`].

use crate::error::{ErrorKind, ParseError};
use crate::ini::Ini;

/// Parses INI source text.
///
/// Each line is trimmed, then:
///
/// - an empty line, or one starting with `;` or `#`, is a comment;
/// - `[name]` starts the section `name` (trimmed); only whitespace and a
///   comment may follow the `]`;
/// - anything else is `key = value`, split at the first `=`. Entries before
///   the first header go to the unnamed section `""`.
///
/// A value starting with `"` is quoted and may use the escapes `\"`, `\\`,
/// `\n` and `\t`. In an unquoted value, a `;` or `#` with whitespace right
/// before it starts a comment. Section names and keys are case-insensitive.
///
/// Parsing stops at the first bad line.
pub fn parse(src: &str) -> Result<Ini, ParseError> {
    let mut ini = Ini::new();
    let mut section = String::new();

    for (index, raw) in src.lines().enumerate() {
        let line = raw.trim();
        let at_line = |kind| ParseError {
            line: index + 1,
            kind,
        };
        if line.is_empty() || is_comment(line) {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            section = section_name(header).map_err(at_line)?;
            ini.add_section(&section);
        } else {
            let (key, value) = entry(line).map_err(at_line)?;
            ini.insert(&section, key, &value);
        }
    }

    Ok(ini)
}

fn is_comment(text: &str) -> bool {
    text.starts_with(';') || text.starts_with('#')
}

/// Whether `rest` (what follows a `]` or a closing quote) is only
/// whitespace and possibly a comment.
fn is_blank_or_comment(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.is_empty() || is_comment(rest)
}

/// The name in a header line, given the text after its `[`.
fn section_name(header: &str) -> Result<String, ErrorKind> {
    let (name, rest) = header.split_once(']').ok_or(ErrorKind::BadSection)?;
    let name = name.trim();
    if name.is_empty() || !is_blank_or_comment(rest) {
        return Err(ErrorKind::BadSection);
    }
    Ok(name.to_string())
}

/// Splits an entry line into its key and its (unquoted or unescaped) value.
fn entry(line: &str) -> Result<(&str, String), ErrorKind> {
    let (key, rest) = line.split_once('=').ok_or(ErrorKind::MissingEquals)?;
    let key = key.trim();
    if key.is_empty() {
        return Err(ErrorKind::EmptyKey);
    }
    let value = match rest.trim_start().strip_prefix('"') {
        Some(quoted) => quoted_value(quoted)?,
        None => unquoted_value(rest).to_string(),
    };
    Ok((key, value))
}

/// An unquoted value: `rest` is everything after the `=`, untrimmed, so a
/// comment marker right after `= ` still has whitespace before it.
fn unquoted_value(rest: &str) -> &str {
    let mut after_space = false;
    for (i, c) in rest.char_indices() {
        if after_space && (c == ';' || c == '#') {
            return rest[..i].trim();
        }
        after_space = c.is_whitespace();
    }
    rest.trim()
}

/// A quoted value, given the text after its opening quote.
fn quoted_value(body: &str) -> Result<String, ErrorKind> {
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                return if is_blank_or_comment(&body[i + 1..]) {
                    Ok(value)
                } else {
                    Err(ErrorKind::TrailingAfterQuote)
                };
            }
            '\\' => match chars.next() {
                Some((_, '"')) => value.push('"'),
                Some((_, '\\')) => value.push('\\'),
                Some((_, 'n')) => value.push('\n'),
                Some((_, 't')) => value.push('\t'),
                Some((_, other)) => return Err(ErrorKind::BadEscape(other)),
                None => return Err(ErrorKind::UnterminatedQuote),
            },
            _ => value.push(c),
        }
    }
    Err(ErrorKind::UnterminatedQuote)
}
