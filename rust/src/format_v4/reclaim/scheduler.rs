//! Maintenance timing policy with an injectable clock. The daemon feeds real instants; tests
//! feed synthetic ones. The engine's backing I/O is synchronous, so a due maintenance step runs
//! at the next safe boundary after its deadline, not within a fixed wall-clock delay.
use crate::{BLOCK_SIZE, format_v4::Geometry};
use std::time::{Duration, Instant};

/// Default gap without requests before idle maintenance runs (ADR-08 candidate range 1–5 ms).
pub const DEFAULT_IDLE_WAIT: Duration = Duration::from_millis(2);
/// Default longest stretch of continuous requests before maintenance runs anyway.
pub const DEFAULT_MAXIMUM_INTERVAL: Duration = Duration::from_millis(100);

/// Free segments, in percent of the log, below which the active cleaner starts. Low on purpose:
/// every segment kept free packs the live data into fewer segments, so victims hold more live
/// blocks and cost more to copy. ADR-08 leaves the value to measurement; this is a first guess.
const ACTIVE_START_PERCENT: u32 = 5;
/// Free segments, in percent of the log, at which the active cleaner stops. The gap to the start
/// keeps the cleaner from turning on and off at one threshold.
const ACTIVE_STOP_PERCENT: u32 = 6;
/// Caps on the two thresholds for large logs. The headroom the cleaner needs is write bandwidth
/// times its reaction time, an absolute amount, while 5% of a 2 TiB log is 100 GiB.
const ACTIVE_START_CAP_BYTES: u64 = 2 << 30;
const ACTIVE_STOP_CAP_BYTES: u64 = 3 << 30;

/// Free-segment thresholds of the active cleaner: it starts when free segments fall below
/// `start` and stops when they reach `stop`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActiveWatermarks {
    start: u32,
    stop: u32,
}

impl ActiveWatermarks {
    /// `start` stays above `victim_floor`, the free segments the active cleaner needs to start
    /// a victim, so it can act as soon as it turns on. `stop` stays above `start`.
    pub(crate) fn new(geometry: Geometry, victim_floor: u32) -> Self {
        let count = geometry.segment_count;
        let segment_bytes = u64::from(geometry.blocks_per_segment) * BLOCK_SIZE as u64;
        let start = percent_of(count, ACTIVE_START_PERCENT)
            .min(capped_segments(ACTIVE_START_CAP_BYTES, segment_bytes))
            .max(victim_floor + 1);
        let stop = percent_of(count, ACTIVE_STOP_PERCENT)
            .min(capped_segments(ACTIVE_STOP_CAP_BYTES, segment_bytes))
            .max(start + 1);
        Self { start, stop }
    }

    /// Whether the active cleaner runs with `free_segments` allocatable, given whether it ran
    /// before.
    pub(crate) fn active(self, was_active: bool, free_segments: u32) -> bool {
        let threshold = if was_active { self.stop } else { self.start };
        free_segments < threshold
    }
}

/// Whole segments in `cap_bytes`, at least one.
fn capped_segments(cap_bytes: u64, segment_bytes: u64) -> u32 {
    u32::try_from(cap_bytes / segment_bytes)
        .unwrap_or(u32::MAX)
        .max(1)
}

/// `percent` of `count`, rounded up.
fn percent_of(count: u32, percent: u32) -> u32 {
    assert!(percent <= 100, "a percentage of the log fits the log");
    let scaled = (u64::from(count) * u64::from(percent)).div_ceil(100);
    u32::try_from(scaled).expect("at most 100% of a u32 count fits a u32")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheduler {
    idle_wait: Duration,
    maximum_interval: Duration,
    /// When maintenance last ran or the scheduler started.
    last_maintenance: Instant,
    /// When the last request completed.
    last_request: Option<Instant>,
    /// Maintenance that ran after its deadline, summed.
    total_lateness: Duration,
    worst_lateness: Duration,
    maintenance_runs: u64,
}

impl Scheduler {
    pub fn new(now: Instant, idle_wait: Duration, maximum_interval: Duration) -> Self {
        assert!(!idle_wait.is_zero() && idle_wait <= maximum_interval);
        Self {
            idle_wait,
            maximum_interval,
            last_maintenance: now,
            last_request: None,
            total_lateness: Duration::ZERO,
            worst_lateness: Duration::ZERO,
            maintenance_runs: 0,
        }
    }

    pub fn idle_wait(&self) -> Duration {
        self.idle_wait
    }

    /// The instant by which maintenance must run regardless of traffic.
    pub fn deadline(&self) -> Instant {
        self.last_maintenance + self.maximum_interval
    }

    pub fn request_completed(&mut self, now: Instant) {
        self.last_request = Some(now);
    }

    /// Whether the maximum interval has passed since maintenance last ran.
    pub fn maintenance_due(&self, now: Instant) -> bool {
        now >= self.deadline()
    }

    /// Whether no request completed for the idle wait.
    pub fn idle(&self, now: Instant) -> bool {
        self.last_request
            .is_none_or(|last| now.saturating_duration_since(last) >= self.idle_wait)
    }

    /// Time left until maintenance must run regardless of traffic.
    pub fn until_deadline(&self, now: Instant) -> Duration {
        self.deadline().saturating_duration_since(now)
    }

    /// How long a loop with maintenance work pending may wait before it must wake: the idle
    /// wait after the last request, bounded by the deadline, and at least one microsecond.
    pub fn wait_hint(&self, now: Instant) -> Duration {
        let until_idle = self.last_request.map_or(Duration::ZERO, |last| {
            (last + self.idle_wait).saturating_duration_since(now)
        });
        self.until_deadline(now)
            .min(until_idle.max(Duration::from_micros(1)))
    }

    /// Records that a maintenance step ran at `now` and returns how late it was.
    pub fn maintained(&mut self, now: Instant) -> Duration {
        let lateness = now.saturating_duration_since(self.deadline());
        self.total_lateness += lateness;
        self.worst_lateness = self.worst_lateness.max(lateness);
        self.maintenance_runs += 1;
        self.last_maintenance = now;
        lateness
    }

    pub fn worst_lateness(&self) -> Duration {
        self.worst_lateness
    }

    pub fn total_lateness(&self) -> Duration {
        self.total_lateness
    }

    pub fn maintenance_runs(&self) -> u64 {
        self.maintenance_runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_requests_cannot_postpone_the_maximum_interval() {
        let start = Instant::now();
        let mut scheduler =
            Scheduler::new(start, Duration::from_millis(2), Duration::from_millis(100));
        let mut now = start;
        for _ in 0..200 {
            now += Duration::from_millis(1);
            scheduler.request_completed(now);
            assert!(!scheduler.idle(now));
            if scheduler.maintenance_due(now) {
                break;
            }
        }
        assert_eq!(now, start + Duration::from_millis(100));
        assert_eq!(scheduler.maintained(now), Duration::ZERO);
        assert_eq!(scheduler.deadline(), now + Duration::from_millis(100));
        assert_eq!(scheduler.maintenance_runs(), 1);
    }

    #[test]
    fn idle_wait_is_distinct_from_the_maintenance_deadline() {
        let start = Instant::now();
        let mut scheduler =
            Scheduler::new(start, Duration::from_millis(2), Duration::from_millis(100));
        assert!(scheduler.idle(start), "no request yet counts as idle");
        assert_eq!(scheduler.wait_hint(start), Duration::from_micros(1));
        scheduler.request_completed(start + Duration::from_millis(10));
        assert!(!scheduler.idle(start + Duration::from_millis(11)));
        assert!(scheduler.idle(start + Duration::from_millis(12)));
        assert_eq!(
            scheduler.wait_hint(start + Duration::from_millis(10)),
            Duration::from_millis(2)
        );
        assert!(!scheduler.maintenance_due(start + Duration::from_millis(12)));
        // Late maintenance is measured against the deadline, not the idle wait.
        let late = start + Duration::from_millis(130);
        assert_eq!(scheduler.maintained(late), Duration::from_millis(30));
        assert_eq!(scheduler.worst_lateness(), Duration::from_millis(30));
        assert_eq!(scheduler.total_lateness(), Duration::from_millis(30));
        // Already idle with work pending: wake at once. Without work: wait for the deadline.
        let later = late + Duration::from_millis(95);
        assert_eq!(scheduler.wait_hint(later), Duration::from_micros(1));
        assert_eq!(scheduler.until_deadline(later), Duration::from_millis(5));
        // A request within the idle wait pushes the hint out to the idle wait.
        scheduler.request_completed(later);
        assert_eq!(scheduler.wait_hint(later), Duration::from_millis(2));
        assert_eq!(
            scheduler.wait_hint(later + Duration::from_millis(1)),
            Duration::from_millis(1)
        );
        assert_eq!(
            scheduler.wait_hint(later + Duration::from_millis(4)),
            Duration::from_micros(1)
        );
    }

    /// 32 MiB segments, as `plan_format` selects for the fio benchmark's 9 GiB backing.
    fn geometry(segment_count: u64) -> Geometry {
        let blocks_per_segment = 8192;
        Geometry::new(1 << 20, 2 + segment_count * blocks_per_segment, 8192).expect("geometry")
    }

    #[test]
    fn active_watermarks_are_percentages_of_the_log() {
        // 287 segments: 5% is 14.35 and 6% is 17.22, both rounded up.
        let watermarks = ActiveWatermarks::new(geometry(287), 8);
        assert_eq!(
            watermarks,
            ActiveWatermarks {
                start: 15,
                stop: 18
            }
        );
    }

    #[test]
    fn active_watermarks_are_capped_on_large_logs() {
        // 2 TiB of 32 MiB segments: 5% would be 100 GiB; the caps are 2 GiB and 3 GiB.
        let watermarks = ActiveWatermarks::new(geometry(65_536), 8);
        assert_eq!(
            watermarks,
            ActiveWatermarks {
                start: 64,
                stop: 96
            }
        );
    }

    #[test]
    fn active_watermarks_stay_above_the_victim_floor_and_apart() {
        // 12 segments: 5% and 6% both round up to 1, below a victim floor of 8.
        let watermarks = ActiveWatermarks::new(geometry(12), 8);
        assert_eq!(watermarks, ActiveWatermarks { start: 9, stop: 10 });
    }

    #[test]
    fn active_cleaner_starts_below_start_and_stops_at_stop() {
        let watermarks = ActiveWatermarks::new(geometry(287), 8);
        assert!(!watermarks.active(false, 15), "at start: not yet");
        assert!(watermarks.active(false, 14), "below start: on");
        assert!(watermarks.active(true, 17), "between the two: stays on");
        assert!(!watermarks.active(true, 18), "at stop: off");
        assert!(!watermarks.active(false, 17), "between the two: stays off");
    }

    #[test]
    #[should_panic(expected = "idle_wait")]
    fn rejects_an_idle_wait_longer_than_the_interval() {
        Scheduler::new(
            Instant::now(),
            Duration::from_millis(200),
            Duration::from_millis(100),
        );
    }
}
