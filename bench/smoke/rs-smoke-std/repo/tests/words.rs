use textstat::word_count;

#[test]
fn counts_words() {
    assert_eq!(word_count("one two  three\n"), 3);
    assert_eq!(word_count(""), 0);
}
