//! The parsed form of an INI file.

use std::str::FromStr;

/// A parsed INI document: named sections of `key = value` entries.
///
/// Entries that appear before the first section header live in the unnamed
/// section `""`. Section names and keys are stored lowercased and every
/// lookup ignores case; values are kept as written. Sections and keys keep
/// the order in which they first appeared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ini {
    sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    name: String,
    entries: Vec<(String, String)>,
}

impl Section {
    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The unnamed section only counts once it has an entry.
    fn is_listed(&self) -> bool {
        !self.name.is_empty() || !self.entries.is_empty()
    }
}

impl Ini {
    /// An empty document.
    pub fn new() -> Ini {
        Ini::default()
    }

    /// The value of `key` in `section`, if both exist.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?.get(&key.to_lowercase())
    }

    /// The value of `key` in `section` parsed as a `T`: `None` when the key
    /// is absent, otherwise the result of `str::parse` on the stored value.
    pub fn get_as<T: FromStr>(&self, section: &str, key: &str) -> Option<Result<T, T::Err>> {
        self.get(section, key).map(str::parse)
    }

    /// Section names in the order they first appeared. The unnamed section
    /// `""` is listed only when it has entries.
    pub fn sections(&self) -> Vec<&str> {
        self.sections
            .iter()
            .filter(|s| s.is_listed())
            .map(|s| s.name.as_str())
            .collect()
    }

    /// The keys of `section` in the order they first appeared; empty for a
    /// section that does not exist.
    pub fn keys(&self, section: &str) -> Vec<&str> {
        self.section(section)
            .map(|s| s.entries.iter().map(|(k, _)| k.as_str()).collect())
            .unwrap_or_default()
    }

    /// Whether [`sections`](Ini::sections) lists `section`.
    pub fn has_section(&self, section: &str) -> bool {
        self.section(section).is_some_and(Section::is_listed)
    }

    fn section(&self, name: &str) -> Option<&Section> {
        let name = name.to_lowercase();
        self.sections.iter().find(|s| s.name == name)
    }

    /// Makes sure `section` (already lowercased) exists and returns it.
    fn section_mut(&mut self, name: &str) -> &mut Section {
        let index = match self.sections.iter().position(|s| s.name == name) {
            Some(index) => index,
            None => {
                self.sections.push(Section {
                    name: name.to_string(),
                    entries: Vec::new(),
                });
                self.sections.len() - 1
            }
        };
        &mut self.sections[index]
    }

    /// Makes sure `section` exists, even if it never gets an entry.
    pub(crate) fn add_section(&mut self, section: &str) {
        self.section_mut(&section.to_lowercase());
    }

    /// Sets `key` in `section`, creating the section if needed. A key that
    /// is already there keeps its place and gets the new value.
    pub(crate) fn insert(&mut self, section: &str, key: &str, value: &str) {
        let key = key.to_lowercase();
        let section = self.section_mut(&section.to_lowercase());
        match section.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, old)) => *old = value.to_string(),
            None => section.entries.push((key, value.to_string())),
        }
    }
}
