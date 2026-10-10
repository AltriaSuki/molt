use ringbuf::RingBuffer;

fn contents<T: Clone>(buf: &RingBuffer<T>) -> Vec<T> {
    buf.iter().cloned().collect()
}

#[test]
fn new_buffer_is_empty() {
    let buf: RingBuffer<i32> = RingBuffer::with_capacity(4);
    assert_eq!(buf.capacity(), 4);
    assert_eq!(buf.len(), 0);
    assert!(buf.is_empty());
    assert!(!buf.is_full());
    assert_eq!(buf.iter().next(), None);
}

#[test]
#[should_panic(expected = "capacity must be at least 1")]
fn zero_capacity_panics() {
    let _ = RingBuffer::<i32>::with_capacity(0);
}

#[test]
fn push_until_full() {
    let mut buf = RingBuffer::with_capacity(3);
    assert_eq!(buf.push(1), None);
    assert_eq!(buf.push(2), None);
    assert!(!buf.is_full());
    assert_eq!(buf.push(3), None);
    assert!(buf.is_full());
    assert_eq!(buf.len(), 3);
    assert_eq!(contents(&buf), [1, 2, 3]);
}

#[test]
fn push_into_a_full_buffer_returns_the_oldest() {
    let mut buf = RingBuffer::with_capacity(3);
    for v in 1..=3 {
        buf.push(v);
    }
    assert_eq!(buf.push(4), Some(1));
    assert_eq!(buf.push(5), Some(2));
    assert_eq!(buf.len(), 3);
    assert!(buf.is_full());
    assert_eq!(buf.capacity(), 3);
}

#[test]
fn capacity_one_keeps_the_latest() {
    let mut buf = RingBuffer::with_capacity(1);
    assert_eq!(buf.push("a"), None);
    assert_eq!(buf.push("b"), Some("a"));
    assert_eq!(buf.push("c"), Some("b"));
    assert_eq!(buf.pop(), Some("c"));
    assert_eq!(buf.pop(), None);
}

#[test]
fn pop_returns_the_oldest_first() {
    let mut buf = RingBuffer::with_capacity(5);
    buf.push(10);
    buf.push(20);
    buf.push(30);
    assert_eq!(buf.pop(), Some(10));
    assert_eq!(buf.pop(), Some(20));
    assert_eq!(buf.len(), 1);
    buf.push(40);
    assert_eq!(buf.pop(), Some(30));
    assert_eq!(buf.pop(), Some(40));
    assert_eq!(buf.pop(), None);
    assert!(buf.is_empty());
}

#[test]
fn fill_and_drain() {
    let mut buf = RingBuffer::with_capacity(4);
    for v in 1..=4 {
        buf.push(v);
    }
    for v in 1..=4 {
        assert_eq!(buf.pop(), Some(v));
    }
    assert_eq!(buf.pop(), None);
    assert!(buf.is_empty());
    assert!(!buf.is_full());
}

#[test]
fn pop_after_an_overwrite() {
    let mut buf = RingBuffer::with_capacity(3);
    for v in 1..=4 {
        buf.push(v);
    }
    assert_eq!(buf.pop(), Some(2));
    assert_eq!(buf.len(), 2);
}

#[test]
fn iter_goes_oldest_to_newest() {
    let mut buf = RingBuffer::with_capacity(5);
    for v in 1..=4 {
        buf.push(v);
    }
    buf.pop();
    assert_eq!(contents(&buf), [2, 3, 4]);
    let mut iter = buf.iter();
    assert_eq!(iter.size_hint(), (3, Some(3)));
    iter.next();
    assert_eq!(iter.size_hint(), (2, Some(2)));
}

#[test]
fn borrowing_for_loop() {
    let mut buf = RingBuffer::with_capacity(4);
    buf.push(String::from("x"));
    buf.push(String::from("y"));
    let mut joined = String::new();
    for s in &buf {
        joined.push_str(s);
    }
    assert_eq!(joined, "xy");
}

#[test]
fn debug_lists_the_elements() {
    let mut buf = RingBuffer::with_capacity(4);
    buf.push(1);
    buf.push(2);
    assert_eq!(format!("{buf:?}"), "[1, 2]");
    let empty: RingBuffer<u8> = RingBuffer::with_capacity(2);
    assert_eq!(format!("{empty:?}"), "[]");
}
