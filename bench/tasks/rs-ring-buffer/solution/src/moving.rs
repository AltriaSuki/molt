//! Moving statistics over a sliding window of samples.

use crate::RingBuffer;

/// The mean of the most recent `window` samples.
///
/// Keeps a running sum next to the samples, so adding a sample costs the same
/// whatever the window size.
///
/// ```
/// use ringbuf::MovingAverage;
///
/// let mut avg = MovingAverage::new(3);
/// assert_eq!(avg.add(3.0), 3.0);
/// assert_eq!(avg.add(5.0), 4.0);
/// assert_eq!(avg.add(10.0), 6.0);
/// assert_eq!(avg.add(6.0), 7.0); // 3.0 has left the window
/// ```
#[derive(Debug)]
pub struct MovingAverage {
    window: RingBuffer<f64>,
    sum: f64,
}

impl MovingAverage {
    /// Creates a moving average over the last `window` samples.
    ///
    /// # Panics
    ///
    /// Panics if `window` is zero.
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "moving average window must be at least 1");
        MovingAverage {
            window: RingBuffer::with_capacity(window),
            sum: 0.0,
        }
    }

    /// Adds a sample, dropping the oldest one if the window is full, and
    /// returns the average of the samples now in the window.
    pub fn add(&mut self, sample: f64) -> f64 {
        if let Some(evicted) = self.window.push(sample) {
            self.sum -= evicted;
        }
        self.sum += sample;
        self.sum / self.window.len() as f64
    }

    /// The average of the samples in the window, or `None` before the first
    /// sample.
    pub fn average(&self) -> Option<f64> {
        if self.window.is_empty() {
            None
        } else {
            Some(self.sum / self.window.len() as f64)
        }
    }

    /// The number of samples the window holds once it is full.
    pub fn window_size(&self) -> usize {
        self.window.capacity()
    }

    /// The number of samples in the window.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Whether no sample has been added since creation or the last reset.
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Whether the window is full, so the average covers `window_size()`
    /// samples.
    pub fn is_warm(&self) -> bool {
        self.window.is_full()
    }

    /// The samples in the window, oldest first.
    pub fn samples(&self) -> impl Iterator<Item = f64> + '_ {
        self.window.iter().copied()
    }

    /// The smallest sample in the window.
    pub fn min(&self) -> Option<f64> {
        self.samples().reduce(f64::min)
    }

    /// The largest sample in the window.
    pub fn max(&self) -> Option<f64> {
        self.samples().reduce(f64::max)
    }

    /// How far the newest sample in the window is from the oldest one
    /// (`newest - oldest`), or `None` when the window is empty.
    pub fn delta(&self) -> Option<f64> {
        let mut samples = self.window.iter();
        let oldest = *samples.next()?;
        let newest = samples.next_back().copied().unwrap_or(oldest);
        Some(newest - oldest)
    }

    /// Forgets every sample, keeping the window size.
    pub fn reset(&mut self) {
        self.window.clear();
        self.sum = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::MovingAverage;

    #[test]
    fn running_sum_matches_the_samples() {
        let mut avg = MovingAverage::new(4);
        for i in 1..=10 {
            avg.add(f64::from(i));
            assert_eq!(avg.sum, avg.samples().sum::<f64>());
        }
        assert_eq!(avg.sum, 7.0 + 8.0 + 9.0 + 10.0);
    }
}
