//! Borrowing iteration over a [`RingBuffer`].

use std::iter::FusedIterator;

use crate::RingBuffer;

/// An iterator over the elements of a [`RingBuffer`], oldest first.
///
/// Returned by [`RingBuffer::iter`] and by `for x in &buffer`. It is
/// double-ended (`next_back` yields the newest remaining element) and knows
/// exactly how many elements are left.
pub struct Iter<'a, T> {
    slots: &'a [Option<T>],
    /// Storage slot of the buffer's oldest element.
    start: usize,
    /// Offset from the oldest element of the next one `next` yields.
    front: usize,
    /// One past the offset of the next element `next_back` yields.
    back: usize,
}

impl<'a, T> Iter<'a, T> {
    pub(crate) fn new(slots: &'a [Option<T>], start: usize, len: usize) -> Self {
        Iter {
            slots,
            start,
            front: 0,
            back: len,
        }
    }

    fn at(&self, offset: usize) -> &'a T {
        let slots: &'a [Option<T>] = self.slots;
        let mut index = self.start + offset;
        if index >= slots.len() {
            index -= slots.len();
        }
        slots[index]
            .as_ref()
            .expect("every slot in the live run holds an element")
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        if self.front == self.back {
            return None;
        }
        let item = self.at(self.front);
        self.front += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }
}

impl<T> DoubleEndedIterator for Iter<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(self.at(self.back))
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}

impl<T> FusedIterator for Iter<'_, T> {}

impl<'a, T> IntoIterator for &'a RingBuffer<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}
