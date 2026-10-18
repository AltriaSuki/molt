use linecount::count_byte;

#[test]
fn counts_every_occurrence() {
    assert_eq!(count_byte(b"", b'a'), 0);
    assert_eq!(count_byte(b"banana", b'a'), 3);
    assert_eq!(count_byte(b"banana", b'z'), 0);
    assert_eq!(count_byte(&[0, 1, 0, 0], 0), 3);
}

#[test]
fn long_inputs_count_too() {
    let mut big = vec![b'x'; 100_000];
    big[7] = b'\n';
    big[99_999] = b'\n';
    assert_eq!(count_byte(&big, b'\n'), 2);
    assert_eq!(count_byte(&big, b'x'), 99_998);
}
