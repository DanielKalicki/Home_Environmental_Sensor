//! Fixed-capacity, allocation-free measurement history.

/// A ring buffer that keeps the newest `N` values in chronological order.
///
/// Once full, inserting a new value overwrites the oldest value. The buffer
/// uses only fixed-size storage, so it is suitable for the firmware's
/// `no_std` environment.
pub(crate) struct MeasurementHistory<T: Copy, const N: usize> {
    entries: [Option<T>; N],
    next: usize,
    len: usize,
}

impl<T: Copy, const N: usize> MeasurementHistory<T, N> {
    /// Create an empty measurement history.
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None; N],
            next: 0,
            len: 0,
        }
    }

    /// Store a value, discarding the oldest value if the history is full.
    pub(crate) fn push(&mut self, value: T) {
        assert!(N > 0, "measurement history capacity must not be zero");

        self.entries[self.next] = Some(value);
        self.next = (self.next + 1) % N;
        self.len = core::cmp::min(self.len + 1, N);
    }

    /// Return the number of readings currently retained.
    #[allow(dead_code)]
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Return a reading by age, where index zero is the oldest retained one.
    #[allow(dead_code)]
    pub(crate) fn get_oldest(&self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }

        let oldest = if self.len == N { self.next } else { 0 };
        self.entries[(oldest + index) % N]
    }
}