//! The parsed form of an INI file.

use std::collections::HashMap;

/// A parsed INI document: named sections of `key = value` entries.
///
/// Entries that appear before the first section header live in the unnamed
/// section `""`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ini {
    sections: HashMap<String, HashMap<String, String>>,
}

impl Ini {
    /// An empty document.
    pub fn new() -> Ini {
        Ini::default()
    }

    /// The value of `key` in `section`, if both exist.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(String::as_str)
    }

    /// Whether the document has a section called `section`.
    pub fn has_section(&self, section: &str) -> bool {
        self.sections.contains_key(section)
    }

    /// Makes sure `section` exists, even if it never gets an entry.
    pub(crate) fn add_section(&mut self, section: &str) {
        self.sections.entry(section.to_string()).or_default();
    }

    /// Sets `key` in `section`, creating the section if needed.
    pub(crate) fn insert(&mut self, section: &str, key: &str, value: &str) {
        self.sections
            .entry(section.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }
}
