//! Fast line counting.

/// How many lines `bytes` holds. A last line without a newline counts.
pub fn lines(bytes: &[u8]) -> usize {
    let newlines = memchr::memchr_iter(b'\n', bytes).count();
    if bytes.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}
