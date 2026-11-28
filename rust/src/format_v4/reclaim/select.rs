//! Victim selection: greedy under pressure and while the active cleaner runs, cost-benefit
//! when idle (ADR-08, F2FS §2.5).
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
    /// The active cleaner runs between requests: take the segment with the fewest live blocks.
    /// A victim starts only when its copies and two publications fit above the reserve, so the
    /// reserve stays for pressure cleaning. A full log may hold no segment that is a quarter
    /// dead, so any victim that frees more than its copy overhead qualifies.
    Active,
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

/// A lower bound on the blocks that copying `live` payloads appends besides the payloads: one
/// footer per relocation record of at most `relocation_batch` payloads, and a link. The writer
/// also ends a record at every window and head crossing, so real copies append more; the active
/// cleaner's progress window catches victims this bound lets through.
pub(crate) fn copy_overhead_blocks(live: u32, relocation_batch: u64) -> u64 {
    u64::from(live).div_ceil(relocation_batch) + 1
}

/// Whether a sealed segment with `dead` dead blocks is worth cleaning in `mode`. Active cleaning
/// needs a net gain: a victim whose only dead blocks are its own footers and link would be copied
/// into a segment with the same overhead, and the active cleaner would run without freeing space.
fn worth_cleaning(log: &Log, segment: u32, dead: u32, mode: SelectionMode, batch: u64) -> bool {
    match mode {
        SelectionMode::Pressure => dead > 0,
        SelectionMode::Idle => dead >= idle_minimum_dead(log.geometry),
        SelectionMode::Active => {
            u64::from(dead) > copy_overhead_blocks(log.segments.live_blocks(segment), batch)
        }
    }
}

/// Picks the best eligible victim, skipping `excluded`. `relocation_batch` is the most payloads
/// one relocation record holds.
pub(crate) fn select(
    log: &Log,
    mode: SelectionMode,
    excluded: Option<u32>,
    relocation_batch: u64,
) -> Selection {
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
        if !worth_cleaning(log, segment, dead, mode, relocation_batch) {
            continue;
        }
        if !eligible(log, segment) {
            awaiting = true;
            continue;
        }
        let score = match mode {
            SelectionMode::Pressure | SelectionMode::Active => u128::from(dead),
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
