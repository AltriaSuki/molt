//! Fixed-capacity ring buffers.
//!
//! [`RingBuffer`] keeps the most recent `capacity` values pushed into it:
//! once it is full, each push overwrites the oldest value and hands it back.
//! It works as a FIFO queue (`push` / `pop`) and iterates from the oldest
//! value to the newest. [`MovingAverage`] uses one to average a sliding
//! window of samples.

mod iter;
pub mod moving;
mod ring;

pub use iter::Iter;
pub use moving::MovingAverage;
pub use ring::RingBuffer;
