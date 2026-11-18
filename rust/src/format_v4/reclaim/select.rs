//! Victim selection: greedy under pressure, cost-benefit when idle (ADR-08, F2FS §2.5).
use crate::format_v4::{Geometry, log::Log, segment_table::SegmentState};

/// Idle cleaning takes a victim only when at least this fraction of the segment is dead. Every
/// sealed segment has a dead link block and dead footers, so without a floor idle cleaning would
/// copy live data from one packed segment to the next forever.
const IDLE_GAIN_DIVISOR: u32 = 4;

/// The fewest dead blocks that make a victim worth cleaning while idle.
pub(crate) fn idle_minimum_dead(geometry: Geometry) -> u32 {
    (geometry.blocks_per_segment / IDLE_GAIN_DIVISOR).max(2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Free space is below the watermark: take the segment with the fewest live blocks.
    Pressure,
    /// Nothing waits: prefer old, mostly dead segments by `(1 - u) * age / (1 + u)`.
    Idle,
}

/// Whether `segment` may be cleaned: sealed, off both roots' replay paths and not empty of
/// reclaimable space. A sealed segment whose records are all dead still counts.
pub(crate) fn eligible(log: &Log, segment: u32) -> bool {
    let segments = &log.segments;
    segments.state(segment) == SegmentState::Sealed
        && !segments.is_stuck(segment)
        && segments.seal_lsn(segment) <= log.protected_lsn()
}

/// Blocks a victim would give back: written blocks minus live payloads and current metadata,
/// with the link counted as reclaimable.
pub(crate) fn dead_blocks(log: &Log, segment: u32) -> u32 {
    let segments = &log.segments;
    segments
        .used_blocks(segment)
        .saturating_sub(segments.live_blocks(segment))
        .saturating_sub(segments.metadata_blocks(segment))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    Victim(u32),
    /// A worthwhile sealed segment exists but is still on a root's replay path; another
    /// publication moves the protected LSN past its seal.
    AwaitPublication,
    /// No sealed segment holds a dead block: the all-live case that may end in `ENOSPC`.
    Nothing,
}

/// Picks the best eligible victim, skipping `excluded`.
pub(crate) fn select(log: &Log, mode: SelectionMode, excluded: Option<u32>) -> Selection {
    let mut best: Option<(u32, u128)> = None;
    let mut awaiting = false;
    for segment in 0..log.geometry.segment_count {
        if Some(segment) == excluded || log.segments.is_stuck(segment) {
            continue;
        }
        if log.segments.state(segment) != SegmentState::Sealed {
            continue;
        }
        let dead = dead_blocks(log, segment);
        if dead == 0 || mode == SelectionMode::Idle && dead < idle_minimum_dead(log.geometry) {
            continue;
        }
        if !eligible(log, segment) {
            awaiting = true;
            continue;
        }
        let score = match mode {
            SelectionMode::Pressure => u128::from(dead),
            SelectionMode::Idle => {
                let used = u128::from(log.segments.used_blocks(segment)).max(1);
                let live = used - u128::from(dead);
                let age = u128::from(log.next_lsn.saturating_sub(log.segments.seal_lsn(segment)));
                // (1 - u) * age / (1 + u) with u = live / used, scaled by used to stay integral.
                (used - live) * age * 1_000_000 / (used + live)
            }
        };
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((segment, score));
        }
    }
    match best {
        Some((segment, _)) => Selection::Victim(segment),
        None if awaiting => Selection::AwaitPublication,
        None => Selection::Nothing,
    }
}
