//! Small text statistics.

/// How many whitespace-separated words `text` has.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// The longest word of `text`, counted in characters; the first of the
/// longest when there are several. Words are split at whitespace, with ASCII
/// punctuation trimmed from both ends.
pub fn longest_word(text: &str) -> Option<&str> {
    let mut best: Option<(&str, usize)> = None;
    for piece in text.split_whitespace() {
        let word = piece.trim_matches(|c: char| c.is_ascii_punctuation());
        if word.is_empty() {
            continue;
        }
        let len = word.chars().count();
        if best.is_none_or(|(_, n)| len > n) {
            best = Some((word, len));
        }
    }
    best.map(|(word, _)| word)
}
