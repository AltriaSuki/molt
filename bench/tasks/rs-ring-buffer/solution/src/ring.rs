//! The ring buffer itself.

use std::fmt;
use std::ops::Index;

use crate::iter::Iter;

/// A fixed-capacity FIFO buffer that overwrites its oldest element when full.
///
/// The elements live in a circular run of `len` slots that begins at
/// `start` and may wrap past the end of the storage; every slot outside the
/// run is `None`. A push writes just past the newest element, a pop takes the
/// element at `start`, and a push into a full buffer replaces the oldest
/// element and hands it back.
///
/// `T` needs no trait bounds. Every value pushed is dropped exactly once:
/// values handed back by `push`, `pop` and `pop_newest` belong to the caller,
/// `clear` and `extend` drop what they remove, and dropping the buffer drops
/// what is left in it.
///
/// ```
/// use ringbuf::RingBuffer;
///
/// let mut buf = RingBuffer::with_capacity(2);
/// assert_eq!(buf.push('a'), None);
/// assert_eq!(buf.push('b'), None);
/// assert_eq!(buf.push('c'), Some('a'));
/// assert_eq!(buf.pop(), Some('b'));
/// assert_eq!(buf.len(), 1);
/// ```
pub struct RingBuffer<T> {
    slots: Vec<Option<T>>,
    start: usize,
    len: usize,
}

impl<T> RingBuffer<T> {
    /// Creates an empty buffer that holds at most `capacity` elements.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be at least 1");
        RingBuffer {
            slots: (0..capacity).map(|_| None).collect(),
            start: 0,
            len: 0,
        }
    }

    /// The maximum number of elements the buffer holds.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The number of elements in the buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer holds no elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the buffer holds `capacity()` elements, so that the next push
    /// overwrites the oldest one.
    pub fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    /// The storage slot of the element `offset` places after the oldest one.
    /// `offset` must be at most `capacity()`.
    fn slot(&self, offset: usize) -> usize {
        let index = self.start + offset;
        if index >= self.capacity() {
            index - self.capacity()
        } else {
            index
        }
    }

    /// Appends `value` as the newest element.
    ///
    /// When the buffer is full, the oldest element is overwritten and
    /// returned; otherwise the result is `None`.
    pub fn push(&mut self, value: T) -> Option<T> {
        if self.is_full() {
            // The slot just past the newest element is the oldest one's.
            let oldest = self.slots[self.start].replace(value);
            self.start = self.slot(1);
            oldest
        } else {
            let end = self.slot(self.len);
            self.slots[end] = Some(value);
            self.len += 1;
            None
        }
    }

    /// Removes and returns the oldest element, or `None` if the buffer is
    /// empty.
    pub fn pop(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let value = self.slots[self.start].take();
        self.start = self.slot(1);
        self.len -= 1;
        value
    }

    /// Removes and returns the newest element, or `None` if the buffer is
    /// empty.
    pub fn pop_newest(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        self.len -= 1;
        let newest = self.slot(self.len);
        self.slots[newest].take()
    }

    /// The element `index` places after the oldest one (`0` is the oldest,
    /// `len() - 1` the newest), or `None` if `index >= len()`.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len {
            self.slots[self.slot(index)].as_ref()
        } else {
            None
        }
    }

    /// Removes and drops every element. The capacity stays the same.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = None;
        }
        self.start = 0;
        self.len = 0;
    }

    /// Iterates over the elements from the oldest to the newest.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter::new(&self.slots, self.start, self.len)
    }
}

/// `buffer[i]` is `buffer.get(i)`, panicking when `i >= len()`.
impl<T> Index<usize> for RingBuffer<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        match self.get(index) {
            Some(value) => value,
            None => panic!("index {index} out of range for length {}", self.len),
        }
    }
}

/// Pushes every item in order, as `push` would; the elements this overwrites
/// are dropped.
impl<T> Extend<T> for RingBuffer<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        for item in items {
            drop(self.push(item));
        }
    }
}

/// Formats the buffer as a list of its elements, oldest first.
impl<T: fmt::Debug> fmt::Debug for RingBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::RingBuffer;

    fn occupied(buf: &RingBuffer<u8>) -> Vec<bool> {
        buf.slots.iter().map(Option::is_some).collect()
    }

    #[test]
    fn slots_outside_the_run_are_empty() {
        let mut buf = RingBuffer::with_capacity(4);
        buf.push(1);
        buf.push(2);
        buf.push(3);
        assert_eq!(occupied(&buf), [true, true, true, false]);
        assert_eq!(buf.pop(), Some(1));
        assert_eq!(occupied(&buf), [false, true, true, false]);
        assert_eq!(buf.start, 1);
    }

    #[test]
    fn overwriting_moves_the_start() {
        let mut buf = RingBuffer::with_capacity(2);
        buf.push(1);
        buf.push(2);
        assert_eq!(buf.push(3), Some(1));
        assert_eq!(buf.start, 1);
        assert_eq!(buf.len, 2);
        assert_eq!(occupied(&buf), [true, true]);
    }

    #[test]
    fn start_wraps_to_the_front() {
        let mut buf = RingBuffer::with_capacity(3);
        for v in 1..=4 {
            buf.push(v);
        }
        assert_eq!(buf.pop(), Some(2));
        assert_eq!(buf.pop(), Some(3));
        assert_eq!(buf.start, 0);
        assert_eq!(buf.pop(), Some(4));
        assert_eq!(buf.start, 1);
    }

    #[test]
    fn pop_newest_and_clear_leave_empty_slots() {
        let mut buf = RingBuffer::with_capacity(3);
        for v in 1..=4 {
            buf.push(v);
        }
        assert_eq!(buf.pop_newest(), Some(4));
        assert_eq!(occupied(&buf), [false, true, true]);
        buf.clear();
        assert_eq!(occupied(&buf), [false, false, false]);
    }
}
