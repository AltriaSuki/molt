use textstat::longest_word;

#[test]
fn the_first_longest_word_wins() {
    assert_eq!(longest_word("a bb ccc dd eee"), Some("ccc"));
    assert_eq!(longest_word("one"), Some("one"));
}

#[test]
fn punctuation_is_trimmed_from_both_ends() {
    assert_eq!(longest_word("\"hello,\" she said."), Some("hello"));
    assert_eq!(longest_word("--- ... !!"), None);
    assert_eq!(longest_word("don't stop"), Some("don't"));
}

#[test]
fn length_is_in_characters() {
    assert_eq!(longest_word("héllo world!"), Some("héllo"));
    assert_eq!(longest_word("日本語 abcd"), Some("abcd"));
}

#[test]
fn no_words_is_none() {
    assert_eq!(longest_word(""), None);
    assert_eq!(longest_word("   \n\t"), None);
}
