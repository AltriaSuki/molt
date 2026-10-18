//! Small text statistics.

/// How many whitespace-separated words `text` has.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}
