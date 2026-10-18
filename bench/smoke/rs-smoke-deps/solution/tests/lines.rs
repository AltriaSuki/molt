use linecount::{count_byte, lines};

#[test]
fn counts_lines() {
    assert_eq!(lines(b""), 0);
    assert_eq!(lines(b"a\nb\n"), 2);
    assert_eq!(lines(b"a\nb"), 2);
}

#[test]
fn counts_a_byte() {
    assert_eq!(count_byte(b"banana", b'n'), 2);
}
