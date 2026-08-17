//! Fixed-capacity, allocation-free measurement history.

/// A ring buffer that keeps the newest values in chronological order.
///
/// Once full, inserting a new value overwrites the oldest value. The storage
/// is supplied by the caller as a `&'static mut` slice instead of being an
/// inline array, so a history can be placed in external PSRAM while the buffer
/// itself stays a plain `no_std` type that never allocates.
///
/// Every stored value is also given a *sequence number*: the first value ever
/// pushed is number zero and each following one is one higher, whether or not
/// the value it overwrote is still retained. A reader that remembers the
/// sequence number it last saw can therefore ask for exactly the values added
/// since then, which is what the web API is built on.
pub(crate) struct MeasurementHistory<T: Copy + 'static> {
    entries: &'static mut [Option<T>],
    next: usize,
    len: usize,
    /// Number of values pushed since boot, across the whole program run.
    pushed: u64,
}

impl<T: Copy + 'static> MeasurementHistory<T> {
    /// Wrap `entries` as an empty history whose capacity is its length.
    ///
    /// The slice must not be empty: a zero-capacity ring buffer could not
    /// retain anything and would divide by zero on the first push.
    pub(crate) fn new(entries: &'static mut [Option<T>]) -> Self {
        assert!(
            !entries.is_empty(),
            "measurement history capacity must not be zero"
        );

        Self {
            entries,
            next: 0,
            len: 0,
            pushed: 0,
        }
    }

    /// Store a value, discarding the oldest value if the history is full.
    pub(crate) fn push(&mut self, value: T) {
        let capacity = self.entries.len();

        self.entries[self.next] = Some(value);
        self.next = (self.next + 1) % capacity;
        self.len = core::cmp::min(self.len + 1, capacity);
        self.pushed += 1;
    }

    /// Return the number of values the buffer can hold.
    pub(crate) fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// Return the number of readings currently retained.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Sequence number of the oldest reading still retained.
    ///
    /// Equals [`Self::next_sequence`] while the history is empty.
    pub(crate) fn first_sequence(&self) -> u64 {
        self.pushed - self.len as u64
    }

    /// Sequence number that the next pushed reading will be given.
    pub(crate) fn next_sequence(&self) -> u64 {
        self.pushed
    }

    /// Return the reading with the given sequence number.
    ///
    /// Yields `None` for a reading that was never stored, or that has already
    /// been overwritten by a newer one.
    pub(crate) fn get(&self, sequence: u64) -> Option<T> {
        let index = sequence.checked_sub(self.first_sequence())?;
        self.get_oldest(usize::try_from(index).ok()?)
    }

    /// Return a reading by age, where index zero is the oldest retained one.
    fn get_oldest(&self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }

        let capacity = self.entries.len();
        let oldest = if self.len == capacity { self.next } else { 0 };
        self.entries[(oldest + index) % capacity]
    }
}
