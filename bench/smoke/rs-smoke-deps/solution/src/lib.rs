//! Fast line counting.

/// How many lines `bytes` holds. A last line without a newline counts.
pub fn lines(bytes: &[u8]) -> usize {
    let newlines = count_byte(bytes, b'\n');
    if bytes.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}

/// How many times `needle` occurs in `haystack`.
pub fn count_byte(haystack: &[u8], needle: u8) -> usize {
    memchr::memchr_iter(needle, haystack).count()
}
