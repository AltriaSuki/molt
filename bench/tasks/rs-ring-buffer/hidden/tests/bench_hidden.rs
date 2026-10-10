use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use ringbuf::{Iter, MovingAverage, RingBuffer};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Records the id of every `Tracked` value dropped, in drop order.
#[derive(Default)]
struct DropLog {
    dropped: Rc<RefCell<Vec<u32>>>,
}

impl DropLog {
    fn new() -> Self {
        DropLog::default()
    }

    fn make(&self, id: u32) -> Tracked {
        Tracked {
            id,
            log: Rc::clone(&self.dropped),
        }
    }

    /// The ids dropped so far, sorted.
    fn dropped(&self) -> Vec<u32> {
        let mut ids = self.dropped.borrow().clone();
        ids.sort_unstable();
        ids
    }

    fn count(&self) -> usize {
        self.dropped.borrow().len()
    }
}

/// A value that is not `Clone`, `Copy`, `Default`, `Debug` or `PartialEq`,
/// and that logs its id when it is dropped.
struct Tracked {
    id: u32,
    log: Rc<RefCell<Vec<u32>>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.log.borrow_mut().push(self.id);
    }
}

/// Not `Clone`, but `Debug`.
#[derive(Debug, PartialEq)]
struct Token(u32);

fn ids(buf: &RingBuffer<Tracked>) -> Vec<u32> {
    buf.iter().map(|t| t.id).collect()
}

fn items(buf: &RingBuffer<u32>) -> Vec<u32> {
    buf.iter().copied().collect()
}

fn wrapped(capacity: usize, pushes: std::ops::RangeInclusive<u32>) -> RingBuffer<u32> {
    let mut buf = RingBuffer::with_capacity(capacity);
    for v in pushes {
        buf.push(v);
    }
    buf
}

fn assert_double_ended_exact<I: DoubleEndedIterator + ExactSizeIterator>(_: &I) {}

// ---------------------------------------------------------------------------
// Bug: iteration order after wraparound
// ---------------------------------------------------------------------------

#[test]
fn iter_is_oldest_to_newest_after_overwrite() {
    let buf = wrapped(3, 1..=4);
    assert_eq!(items(&buf), [2, 3, 4]);
    let buf = wrapped(3, 1..=5);
    assert_eq!(items(&buf), [3, 4, 5]);
    let buf = wrapped(4, 1..=11);
    assert_eq!(items(&buf), [8, 9, 10, 11]);
}

#[test]
fn iter_is_oldest_to_newest_after_pops_and_pushes() {
    let mut buf = RingBuffer::with_capacity(4);
    for v in 1..=4 {
        buf.push(v);
    }
    assert_eq!(buf.pop(), Some(1));
    assert_eq!(buf.pop(), Some(2));
    assert_eq!(buf.pop(), Some(3));
    buf.push(5);
    buf.push(6);
    // 4 sits in the last slot, 5 and 6 wrapped to the front.
    assert_eq!(items(&buf), [4, 5, 6]);
    assert_eq!(buf.iter().len(), 3);
}

#[test]
fn for_loop_and_debug_follow_the_logical_order() {
    let buf = wrapped(3, 1..=4);
    let mut seen = Vec::new();
    for x in &buf {
        seen.push(*x);
    }
    assert_eq!(seen, [2, 3, 4]);
    assert_eq!(format!("{buf:?}"), "[2, 3, 4]");
}

// ---------------------------------------------------------------------------
// Bug: pop after wraparound
// ---------------------------------------------------------------------------

#[test]
fn pop_drains_a_wrapped_buffer_in_order() {
    let mut buf = wrapped(3, 1..=4);
    assert_eq!(buf.pop(), Some(2));
    assert_eq!(buf.pop(), Some(3));
    assert_eq!(buf.pop(), Some(4));
    assert_eq!(buf.pop(), None);
    assert!(buf.is_empty());
}

#[test]
fn pop_from_the_last_slot_wraps_to_the_front() {
    let mut buf = RingBuffer::with_capacity(3);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    assert_eq!(buf.pop(), Some(1));
    assert_eq!(buf.pop(), Some(2));
    buf.push(4);
    assert_eq!(buf.pop(), Some(3));
    assert_eq!(buf.pop(), Some(4));
    assert_eq!(buf.pop(), None);
    buf.push(5);
    buf.push(6);
    buf.push(7);
    assert_eq!(buf.push(8), Some(5));
    assert_eq!(items(&buf), [6, 7, 8]);
    assert_eq!(buf.pop(), Some(6));
}

#[test]
fn queue_cycles_many_times_around() {
    let mut buf = RingBuffer::with_capacity(3);
    let mut next_in = 0u32;
    let mut next_out = 0u32;
    for _ in 0..25 {
        buf.push(next_in);
        next_in += 1;
        buf.push(next_in);
        next_in += 1;
        assert_eq!(buf.pop(), Some(next_out));
        next_out += 1;
        assert_eq!(buf.pop(), Some(next_out));
        next_out += 1;
    }
    assert!(buf.is_empty());
    assert_eq!(buf.pop(), None);
}

// ---------------------------------------------------------------------------
// Iter: DoubleEndedIterator and ExactSizeIterator
// ---------------------------------------------------------------------------

#[test]
fn iter_keeps_its_type_and_is_double_ended_and_exact_size() {
    let buf = wrapped(4, 1..=6);
    let iter: Iter<'_, u32> = buf.iter();
    assert_double_ended_exact(&iter);
    assert_eq!(iter.len(), 4);
}

#[test]
fn rev_goes_newest_to_oldest_after_wraparound() {
    let buf = wrapped(4, 1..=6);
    let back: Vec<u32> = buf.iter().rev().copied().collect();
    assert_eq!(back, [6, 5, 4, 3]);
    let mut partial = RingBuffer::with_capacity(5);
    for v in 1..=5 {
        partial.push(v);
    }
    partial.pop();
    partial.pop();
    partial.push(6);
    let back: Vec<u32> = partial.iter().rev().copied().collect();
    assert_eq!(back, [6, 5, 4, 3]);
}

#[test]
fn next_back_on_an_empty_buffer_is_none() {
    let buf: RingBuffer<u32> = RingBuffer::with_capacity(3);
    let mut iter = buf.iter();
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
}

#[test]
fn mixing_next_and_next_back_yields_each_element_once() {
    let buf = wrapped(5, 1..=8); // [4, 5, 6, 7, 8], wrapped
    let mut iter = buf.iter();
    assert_eq!(iter.len(), 5);
    assert_eq!(iter.next(), Some(&4));
    assert_eq!(iter.len(), 4);
    assert_eq!(iter.next_back(), Some(&8));
    assert_eq!(iter.len(), 3);
    assert_eq!(iter.size_hint(), (3, Some(3)));
    assert_eq!(iter.next_back(), Some(&7));
    assert_eq!(iter.next(), Some(&5));
    assert_eq!(iter.len(), 1);
    assert_eq!(iter.next_back(), Some(&6));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.len(), 0);
}

#[test]
fn front_and_back_meet_on_an_even_count() {
    let buf = wrapped(4, 1..=7); // [4, 5, 6, 7]
    let mut iter = buf.iter();
    assert_eq!(iter.next_back(), Some(&7));
    assert_eq!(iter.next(), Some(&4));
    assert_eq!(iter.next_back(), Some(&6));
    assert_eq!(iter.next_back(), Some(&5));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
}

#[test]
fn exact_size_counts_down_from_both_ends() {
    let buf = wrapped(6, 1..=9);
    let mut iter = buf.iter();
    let mut expected = 6;
    let mut from_front = true;
    while iter.len() > 0 {
        assert_eq!(iter.len(), expected);
        let item = if from_front { iter.next() } else { iter.next_back() };
        assert!(item.is_some());
        expected -= 1;
        from_front = !from_front;
    }
    assert_eq!(expected, 0);
    assert_eq!(iter.next(), None);
}

// ---------------------------------------------------------------------------
// pop_newest
// ---------------------------------------------------------------------------

#[test]
fn pop_newest_takes_from_the_back() {
    let mut buf = RingBuffer::with_capacity(3);
    assert_eq!(buf.pop_newest(), None);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    assert_eq!(buf.pop_newest(), Some(3));
    assert_eq!(buf.len(), 2);
    assert!(!buf.is_full());
    assert_eq!(buf.pop_newest(), Some(2));
    assert_eq!(buf.pop_newest(), Some(1));
    assert_eq!(buf.pop_newest(), None);
    assert!(buf.is_empty());
}

#[test]
fn pop_newest_after_wraparound() {
    let mut buf = wrapped(3, 1..=5); // [3, 4, 5], newest in slot 1
    assert_eq!(buf.pop_newest(), Some(5));
    assert_eq!(buf.pop_newest(), Some(4));
    assert_eq!(items(&buf), [3]);
    buf.push(6);
    buf.push(7);
    assert_eq!(items(&buf), [3, 6, 7]);
    assert_eq!(buf.push(8), Some(3));
    assert_eq!(buf.pop_newest(), Some(8));
    assert_eq!(buf.pop(), Some(6));
    assert_eq!(buf.pop_newest(), Some(7));
    assert_eq!(buf.pop_newest(), None);
}

#[test]
fn pop_newest_then_push_reuses_the_slot() {
    let mut buf = RingBuffer::with_capacity(2);
    buf.push(1);
    buf.push(2);
    assert_eq!(buf.pop_newest(), Some(2));
    assert_eq!(buf.push(3), None);
    assert_eq!(buf.push(4), Some(1));
    assert_eq!(items(&buf), [3, 4]);
}

// ---------------------------------------------------------------------------
// get and Index
// ---------------------------------------------------------------------------

#[test]
fn get_counts_from_the_oldest() {
    let buf = wrapped(4, 1..=6); // [3, 4, 5, 6]
    assert_eq!(buf.get(0), Some(&3));
    assert_eq!(buf.get(1), Some(&4));
    assert_eq!(buf.get(3), Some(&6));
    assert_eq!(buf[0], 3);
    assert_eq!(buf[2], 5);
    assert_eq!(buf[3], 6);
}

#[test]
fn get_past_the_end_is_none() {
    let full = wrapped(3, 1..=4); // full, so slot `start + len` holds the oldest
    assert_eq!(full.get(3), None);
    assert_eq!(full.get(4), None);
    assert_eq!(full.get(5), None);
    assert_eq!(full.get(usize::MAX), None);

    let mut partial = RingBuffer::with_capacity(4);
    partial.push(10);
    partial.push(20);
    assert_eq!(partial.get(1), Some(&20));
    assert_eq!(partial.get(2), None);
    assert_eq!(partial.get(4), None);
    assert_eq!(partial.get(usize::MAX), None);

    let empty: RingBuffer<u32> = RingBuffer::with_capacity(2);
    assert_eq!(empty.get(0), None);
}

#[test]
#[should_panic(expected = "index 3 out of range for length 3")]
fn index_past_the_end_of_a_full_buffer_panics() {
    let buf = wrapped(3, 1..=4);
    let _value = &buf[3];
}

#[test]
#[should_panic(expected = "index 0 out of range for length 0")]
fn index_into_an_empty_buffer_panics() {
    let buf: RingBuffer<u32> = RingBuffer::with_capacity(2);
    let _value = &buf[0];
}

#[test]
#[should_panic(expected = "index 2 out of range for length 2")]
fn index_beyond_length_but_within_capacity_panics() {
    let mut buf = RingBuffer::with_capacity(5);
    buf.push(1);
    buf.push(2);
    let _value = &buf[2];
}

#[test]
#[should_panic(expected = "index 18446744073709551615 out of range for length 2")]
fn index_with_a_huge_value_panics_with_the_message() {
    let mut buf = RingBuffer::with_capacity(2);
    buf.push(1);
    buf.push(2);
    buf.push(3);
    let _value = &buf[usize::MAX];
}

// ---------------------------------------------------------------------------
// Extend and clear
// ---------------------------------------------------------------------------

#[test]
fn extend_pushes_in_order_and_overwrites() {
    let mut buf = RingBuffer::with_capacity(3);
    buf.extend(1..=5);
    assert_eq!(items(&buf), [3, 4, 5]);
    assert!(buf.is_full());
    buf.extend(vec![6]);
    assert_eq!(items(&buf), [4, 5, 6]);
    buf.extend(std::iter::empty::<u32>());
    assert_eq!(items(&buf), [4, 5, 6]);

    let mut partial = RingBuffer::with_capacity(5);
    partial.push(1);
    partial.extend([2, 3]);
    assert_eq!(items(&partial), [1, 2, 3]);
    assert_eq!(partial.len(), 3);
    partial.extend(4..=7);
    assert_eq!(items(&partial), [3, 4, 5, 6, 7]);
    assert_eq!(partial.pop(), Some(3));
}

#[test]
fn clear_empties_and_keeps_capacity() {
    let mut buf = wrapped(3, 1..=5);
    buf.clear();
    assert_eq!(buf.len(), 0);
    assert!(buf.is_empty());
    assert!(!buf.is_full());
    assert_eq!(buf.capacity(), 3);
    assert_eq!(buf.iter().next(), None);
    assert_eq!(buf.iter().len(), 0);
    assert_eq!(buf.get(0), None);
    assert_eq!(buf.pop(), None);
    assert_eq!(buf.pop_newest(), None);
    assert_eq!(format!("{buf:?}"), "[]");

    // Usable like a new buffer afterwards, including wrapping again.
    assert_eq!(buf.push(10), None);
    assert_eq!(buf.push(11), None);
    assert_eq!(buf.push(12), None);
    assert_eq!(buf.push(13), Some(10));
    assert_eq!(items(&buf), [11, 12, 13]);
    assert_eq!(buf.pop(), Some(11));
    buf.clear();
    buf.clear();
    assert!(buf.is_empty());
}

// ---------------------------------------------------------------------------
// No trait bounds on T
// ---------------------------------------------------------------------------

#[test]
fn works_with_a_type_that_has_no_traits() {
    let log = DropLog::new();
    let mut buf: RingBuffer<Tracked> = RingBuffer::with_capacity(2);
    assert!(buf.push(log.make(1)).is_none());
    assert!(buf.push(log.make(2)).is_none());
    assert_eq!(buf.push(log.make(3)).map(|t| t.id), Some(1));
    assert_eq!(buf.get(0).map(|t| t.id), Some(2));
    assert_eq!(buf[1].id, 3);
    let mut seen = Vec::new();
    for t in &buf {
        seen.push(t.id);
    }
    assert_eq!(seen, [2, 3]);
    let iter: Iter<'_, Tracked> = buf.iter();
    assert_eq!(iter.rev().map(|t| t.id).collect::<Vec<_>>(), [3, 2]);
    buf.extend(vec![log.make(4)]);
    assert_eq!(buf.pop_newest().map(|t| t.id), Some(4));
    assert_eq!(buf.pop().map(|t| t.id), Some(3));
    buf.clear();
    assert!(buf.is_empty());
    assert_eq!(buf.capacity(), 2);
}

#[test]
fn debug_needs_only_debug() {
    let mut buf = RingBuffer::with_capacity(2);
    buf.push(Token(1));
    buf.push(Token(2));
    assert_eq!(buf.push(Token(3)), Some(Token(1)));
    assert_eq!(format!("{buf:?}"), "[Token(2), Token(3)]");
    assert_eq!(buf.pop(), Some(Token(2)));
    assert_eq!(buf.get(0), Some(&Token(3)));
}

// ---------------------------------------------------------------------------
// Every value is dropped exactly once
// ---------------------------------------------------------------------------

#[test]
fn overwritten_value_belongs_to_the_caller() {
    let log = DropLog::new();
    let mut buf = RingBuffer::with_capacity(2);
    buf.push(log.make(1));
    buf.push(log.make(2));
    let evicted = buf.push(log.make(3)).expect("full buffer hands back the oldest");
    assert_eq!(evicted.id, 1);
    assert_eq!(log.count(), 0, "the buffer must not drop what it hands back");
    drop(evicted);
    assert_eq!(log.dropped(), [1]);
    drop(buf);
    assert_eq!(log.dropped(), [1, 2, 3]);
}

#[test]
fn popped_values_belong_to_the_caller() {
    let log = DropLog::new();
    let mut buf = RingBuffer::with_capacity(3);
    for id in 1..=4 {
        drop(buf.push(log.make(id)));
    }
    assert_eq!(log.dropped(), [1]);
    let oldest = buf.pop().unwrap();
    let newest = buf.pop_newest().unwrap();
    assert_eq!((oldest.id, newest.id), (2, 4));
    assert_eq!(log.dropped(), [1]);
    drop(oldest);
    drop(newest);
    assert_eq!(log.dropped(), [1, 2, 4]);
    assert_eq!(ids(&buf), [3]);
    drop(buf);
    assert_eq!(log.dropped(), [1, 2, 3, 4]);
}

#[test]
fn dropping_a_wrapped_buffer_drops_what_is_left_once() {
    let log = DropLog::new();
    {
        let mut buf = RingBuffer::with_capacity(4);
        for id in 1..=6 {
            drop(buf.push(log.make(id)));
        }
        drop(buf.pop()); // 3
        drop(buf.pop()); // 4
        buf.push(log.make(7));
        assert_eq!(ids(&buf), [5, 6, 7]);
        assert_eq!(log.dropped(), [1, 2, 3, 4]);
    }
    assert_eq!(log.dropped(), [1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn clear_drops_the_elements_right_away() {
    let log = DropLog::new();
    let mut buf = RingBuffer::with_capacity(3);
    for id in 1..=5 {
        drop(buf.push(log.make(id)));
    }
    assert_eq!(log.dropped(), [1, 2]);
    buf.clear();
    assert_eq!(log.dropped(), [1, 2, 3, 4, 5]);
    buf.push(log.make(6));
    buf.push(log.make(7));
    assert_eq!(ids(&buf), [6, 7]);
    assert_eq!(log.count(), 5);
    drop(buf);
    assert_eq!(log.dropped(), [1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn extend_drops_what_it_overwrites() {
    let log = DropLog::new();
    let mut buf = RingBuffer::with_capacity(3);
    buf.push(log.make(1));
    buf.extend((2..=7).map(|id| log.make(id)));
    assert_eq!(ids(&buf), [5, 6, 7]);
    assert_eq!(log.dropped(), [1, 2, 3, 4]);
    drop(buf);
    assert_eq!(log.dropped(), [1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn an_empty_or_drained_buffer_drops_nothing_more() {
    let log = DropLog::new();
    let empty: RingBuffer<Tracked> = RingBuffer::with_capacity(4);
    drop(empty);
    let mut buf = RingBuffer::with_capacity(2);
    buf.push(log.make(1));
    buf.push(log.make(2));
    let a = buf.pop().unwrap();
    let b = buf.pop().unwrap();
    drop(buf);
    assert_eq!(log.count(), 0);
    drop((a, b));
    assert_eq!(log.dropped(), [1, 2]);
}

// ---------------------------------------------------------------------------
// Model check against VecDeque, with a fixed sequence of operations
// ---------------------------------------------------------------------------

/// A small fixed-seed linear congruential generator.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

fn assert_matches(buf: &RingBuffer<Tracked>, model: &VecDeque<u32>, capacity: usize, step: usize) {
    let expected: Vec<u32> = model.iter().copied().collect();
    assert_eq!(buf.len(), model.len(), "len at step {step}");
    assert_eq!(buf.is_empty(), model.is_empty(), "is_empty at step {step}");
    assert_eq!(buf.is_full(), model.len() == capacity, "is_full at step {step}");
    assert_eq!(buf.capacity(), capacity, "capacity at step {step}");
    assert_eq!(ids(buf), expected, "iter at step {step}");
    let mut reversed = expected.clone();
    reversed.reverse();
    let back: Vec<u32> = buf.iter().rev().map(|t| t.id).collect();
    assert_eq!(back, reversed, "iter().rev() at step {step}");
    assert_eq!(buf.iter().len(), model.len(), "iter().len() at step {step}");
    for (i, id) in expected.iter().enumerate() {
        assert_eq!(buf.get(i).map(|t| t.id), Some(*id), "get({i}) at step {step}");
        assert_eq!(buf[i].id, *id, "buf[{i}] at step {step}");
    }
    for i in model.len()..capacity + 2 {
        assert!(buf.get(i).is_none(), "get({i}) past the end at step {step}");
    }
}

fn run_model(capacity: usize, seed: u64, steps: usize) {
    let log = DropLog::new();
    let mut rng = Lcg(seed);
    let mut buf: RingBuffer<Tracked> = RingBuffer::with_capacity(capacity);
    let mut model: VecDeque<u32> = VecDeque::new();
    let mut next_id = 0u32;

    for step in 0..steps {
        match rng.below(20) {
            0..=8 => {
                let id = next_id;
                next_id += 1;
                let expected = if model.len() == capacity { model.pop_front() } else { None };
                model.push_back(id);
                let got = buf.push(log.make(id)).map(|t| t.id);
                assert_eq!(got, expected, "push({id}) at step {step}");
            }
            9..=12 => {
                let got = buf.pop().map(|t| t.id);
                assert_eq!(got, model.pop_front(), "pop at step {step}");
            }
            13..=15 => {
                let got = buf.pop_newest().map(|t| t.id);
                assert_eq!(got, model.pop_back(), "pop_newest at step {step}");
            }
            16..=18 => {
                let n = rng.below(capacity as u32 + 3);
                let first = next_id;
                next_id += n;
                for id in first..next_id {
                    if model.len() == capacity {
                        model.pop_front();
                    }
                    model.push_back(id);
                }
                buf.extend((first..next_id).map(|id| log.make(id)));
            }
            _ => {
                model.clear();
                buf.clear();
            }
        }
        assert_matches(&buf, &model, capacity, step);
        // Everything created so far is either still in the buffer or dropped.
        assert_eq!(
            log.count() + buf.len(),
            next_id as usize,
            "live + dropped at step {step}"
        );
    }

    let live = buf.len();
    drop(buf);
    assert_eq!(log.count(), next_id as usize, "{live} left at the end, all dropped");
    assert_eq!(log.dropped(), (0..next_id).collect::<Vec<_>>(), "each id dropped once");
}

#[test]
fn model_capacity_1() {
    run_model(1, 0x5eed_0001, 200);
}

#[test]
fn model_capacity_2() {
    run_model(2, 0x5eed_0002, 300);
}

#[test]
fn model_capacity_3() {
    run_model(3, 0x5eed_0003, 400);
}

#[test]
fn model_capacity_5() {
    run_model(5, 0x5eed_0005, 500);
}

#[test]
fn model_capacity_8() {
    run_model(8, 0x5eed_0008, 600);
}

// ---------------------------------------------------------------------------
// MovingAverage once the window has slid
// ---------------------------------------------------------------------------

#[test]
fn moving_average_samples_and_delta_after_sliding() {
    let mut avg = MovingAverage::new(3);
    for s in [1.0, 2.0, 3.0, 4.0, 10.0] {
        avg.add(s);
    }
    assert_eq!(avg.samples().collect::<Vec<_>>(), [3.0, 4.0, 10.0]);
    assert_eq!(avg.delta(), Some(7.0));
    assert_eq!(avg.min(), Some(3.0));
    assert_eq!(avg.max(), Some(10.0));
    assert_eq!(avg.average(), Some(17.0 / 3.0));
    avg.add(2.0);
    assert_eq!(avg.samples().collect::<Vec<_>>(), [4.0, 10.0, 2.0]);
    assert_eq!(avg.delta(), Some(-2.0));
}

#[test]
fn moving_average_single_sample_window() {
    let mut avg = MovingAverage::new(1);
    assert_eq!(avg.add(5.0), 5.0);
    assert_eq!(avg.add(9.0), 9.0);
    assert_eq!(avg.samples().collect::<Vec<_>>(), [9.0]);
    assert_eq!(avg.delta(), Some(0.0));
}

#[test]
fn moving_average_reset_then_slide_again() {
    let mut avg = MovingAverage::new(2);
    for s in [1.0, 2.0, 3.0] {
        avg.add(s);
    }
    avg.reset();
    assert!(avg.is_empty());
    assert_eq!(avg.window_size(), 2);
    for s in [4.0, 6.0, 9.0] {
        avg.add(s);
    }
    assert_eq!(avg.samples().collect::<Vec<_>>(), [6.0, 9.0]);
    assert_eq!(avg.delta(), Some(3.0));
    assert_eq!(avg.average(), Some(7.5));
}
