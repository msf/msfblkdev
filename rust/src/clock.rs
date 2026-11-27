//! Time sources. The engine and the daemon read the time and sleep only through [`Clock`], so a
//! test can drive time by hand with [`ManualClock`].
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// A monotonic time source that can also block the caller.
pub trait Clock {
    /// The current instant. Never goes backwards.
    fn now(&self) -> Instant;

    /// Waits for `duration` to pass on this clock.
    fn sleep(&self, duration: Duration);
}

/// `CLOCK_MONOTONIC`, through [`Instant`]. Wall-clock changes don't move it, and io_uring
/// timeouts use the same clock, so a deadline computed here matches a kernel timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A clock that moves only when [`advance`](Self::advance) or [`Clock::sleep`] moves it. For
/// tests: sleeping returns at once, so a test of a deadline doesn't wait for it.
#[derive(Debug)]
pub struct ManualClock {
    start: Instant,
    elapsed_nanos: AtomicU64,
}

impl ManualClock {
    /// A clock that reads `start` until it is advanced.
    #[must_use]
    pub fn new(start: Instant) -> Self {
        Self {
            start,
            elapsed_nanos: AtomicU64::new(0),
        }
    }

    /// Moves the clock forward by `duration`.
    ///
    /// # Panics
    ///
    /// When the total time advanced exceeds `u64::MAX` nanoseconds (about 584 years).
    pub fn advance(&self, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).expect("advance fits u64 nanoseconds");
        let previous = self.elapsed_nanos.fetch_add(nanos, Ordering::Relaxed);
        assert!(
            previous.checked_add(nanos).is_some(),
            "manual clock overflow"
        );
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.start + Duration::from_nanos(self.elapsed_nanos.load(Ordering::Relaxed))
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_advanced_or_slept() {
        let start = Instant::now();
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        assert_eq!(clock.now(), start);
        clock.advance(Duration::from_millis(2));
        assert_eq!(clock.now(), start + Duration::from_millis(2));
        clock.sleep(Duration::from_micros(500));
        assert_eq!(clock.now(), start + Duration::from_micros(2_500));
    }

    #[test]
    fn monotonic_clock_never_goes_backwards() {
        let clock = MonotonicClock;
        let first = clock.now();
        clock.sleep(Duration::from_millis(1));
        assert!(clock.now() >= first + Duration::from_millis(1));
    }
}
