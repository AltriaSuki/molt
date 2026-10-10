//! Borrowing iteration over a [`RingBuffer`].

use std::slice;

use crate::RingBuffer;

/// An iterator over the elements of a [`RingBuffer`], oldest first.
///
/// Returned by [`RingBuffer::iter`] and by `for x in &buffer`.
pub struct Iter<'a, T> {
    slots: slice::Iter<'a, Option<T>>,
    remaining: usize,
}

impl<'a, T> Iter<'a, T> {
    pub(crate) fn new(slots: &'a [Option<T>], len: usize) -> Self {
        Iter {
            slots: slots.iter(),
            remaining: len,
        }
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        if self.remaining == 0 {
            return None;
        }
        // Only the slots that hold an element are `Some`; skip the others.
        let item = self.slots.find_map(Option::as_ref)?;
        self.remaining -= 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, T: Clone> IntoIterator for &'a RingBuffer<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}
