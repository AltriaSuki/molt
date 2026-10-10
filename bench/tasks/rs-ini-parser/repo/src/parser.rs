//! Turning INI source text into an [`Ini`].

use crate::error::ParseError;
use crate::ini::Ini;

/// Parses INI source text.
///
/// Understood so far:
///
/// - `[name]` starts the section `name`;
/// - `key=value` sets `key` in the current section, with the whitespace
///   around the key and the value removed. Entries before the first header
///   go to the unnamed section `""`.
///
/// Any other line is skipped.
pub fn parse(src: &str) -> Result<Ini, ParseError> {
    let mut ini = Ini::new();
    let mut section = String::new();

    for raw in src.lines() {
        let line = raw.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            ini.add_section(&section);
        } else if let Some((key, value)) = line.split_once('=') {
            ini.insert(&section, key.trim(), value.trim());
        }
    }

    Ok(ini)
}
