//! The ring buffer itself.

use std::fmt;

use crate::iter::Iter;

/// A fixed-capacity FIFO buffer that overwrites its oldest element when full.
///
/// The elements live in a circular run of `len` slots that begins at
/// `start` and may wrap past the end of the storage; every slot outside the
/// run is `None`. A push writes just past the newest element, a pop takes the
/// element at `start`, and a push into a full buffer replaces the oldest
/// element and hands it back.
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

impl<T: Clone> RingBuffer<T> {
    /// Creates an empty buffer that holds at most `capacity` elements.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be at least 1");
        RingBuffer {
            slots: vec![None; capacity],
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

    /// Appends `value` as the newest element.
    ///
    /// When the buffer is full, the oldest element is overwritten and
    /// returned; otherwise the result is `None`.
    pub fn push(&mut self, value: T) -> Option<T> {
        let capacity = self.capacity();
        if self.is_full() {
            // The slot just past the newest element is the oldest one's.
            let oldest = self.slots[self.start].replace(value);
            self.start = (self.start + 1) % capacity;
            oldest
        } else {
            let end = (self.start + self.len) % capacity;
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
        self.start += 1;
        if self.start > self.capacity() {
            self.start = 0;
        }
        self.len -= 1;
        value
    }

    /// Iterates over the elements from the oldest to the newest.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter::new(&self.slots, self.len)
    }
}

/// Formats the buffer as a list of its elements, oldest first.
impl<T: Clone + fmt::Debug> fmt::Debug for RingBuffer<T> {
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
}
