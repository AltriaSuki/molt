use linecount::lines;

#[test]
fn counts_lines() {
    assert_eq!(lines(b""), 0);
    assert_eq!(lines(b"a\nb\n"), 2);
    assert_eq!(lines(b"a\nb"), 2);
}
