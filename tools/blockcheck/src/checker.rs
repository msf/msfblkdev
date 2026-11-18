//! The reference model and checker. See the checker rules in `docs/BLOCKCHECK-PLAN.md`.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::{error, fmt};

use crate::payload::{Block, Content, PayloadCodec};
use crate::target::{BlockTarget, IoFailure};

/// 16 GiB of 4 KiB blocks: four times the largest planned scale. The model then needs about
/// 36 bytes per block, about 146 MiB.
pub const MAX_BLOCKS: u64 = 1 << 22;
/// Rejected payloads a faulty target could still return. A run that needs more is incomplete.
pub const MAX_REJECTED_WRITES: usize = 1 << 16;
/// Every violation is counted; only the first ones are kept for the report.
const MAX_RECORDED_VIOLATIONS: usize = 64;

/// One LBA's history in per-LBA versions. Every submission takes a new version, so a rejected or
/// failed request never shares a payload with a later one. Version 0 is the unwritten state.
#[derive(Clone, Copy, Debug, Default)]
struct LbaState {
    issued: u32,
    /// The last acknowledged request.
    latest: u32,
    /// The last acknowledged request that left zeros: a zero write or a discard.
    last_zero: u32,
    /// The last acknowledged request when a flush last succeeded.
    durable: u32,
    durable_is_payload: bool,
    /// A failed payload write whose effect is unknown until recovery, or 0.
    uncertain_write: u32,
    /// A failed zero write or discard whose effect is unknown until recovery, or 0.
    uncertain_zero: u32,
    needs_repair: bool,
}

impl LbaState {
    fn expected(self) -> Content {
        match NonZeroU32::new(self.latest) {
            Some(version) if self.latest != self.last_zero => Content::Version(version),
            _ => Content::Zero,
        }
    }

    const fn dirty(self) -> bool {
        self.latest != self.durable
    }
}

/// Before a crash: the latest acknowledged content, or a newer uncertain request.
fn live_violation(state: LbaState, found: Content, rejected: bool) -> Option<ViolationKind> {
    if found == state.expected() {
        return None;
    }
    let version = match found {
        Content::Invalid => return Some(ViolationKind::Corrupt),
        Content::Zero if state.uncertain_zero > state.latest => return None,
        Content::Zero => return Some(ViolationKind::LostData),
        Content::Version(version) => version.get(),
    };
    if version == state.uncertain_write && version > state.latest {
        None
    } else if rejected || version > state.latest {
        Some(ViolationKind::NeverAcknowledged)
    } else {
        Some(ViolationKind::Stale)
    }
}

/// After a crash: the durable content, or any acknowledged or uncertain request after it.
/// A payload for the version of a zero request is never generated, so a target cannot return
/// one; the range check below may therefore accept every version after `durable`.
fn crash_violation(state: LbaState, found: Content, rejected: bool) -> Option<ViolationKind> {
    let version = match found {
        Content::Invalid => return Some(ViolationKind::Corrupt),
        Content::Zero => return zero_after_crash(state),
        Content::Version(version) => version.get(),
    };
    let durable_payload = state.durable_is_payload && version == state.durable;
    let uncertain = version == state.uncertain_write && version > state.durable;
    if rejected || (version > state.latest && !uncertain) {
        Some(ViolationKind::NeverAcknowledged)
    } else if durable_payload || uncertain || version > state.durable {
        None
    } else {
        Some(ViolationKind::Stale)
    }
}

fn zero_after_crash(state: LbaState) -> Option<ViolationKind> {
    let allowed = !state.durable_is_payload
        || state.last_zero > state.durable
        || state.uncertain_zero > state.durable;
    (!allowed).then_some(ViolationKind::LostData)
}

/// Marks the latest acknowledged request of each dirty LBA as durable.
fn make_durable(lbas: &mut [LbaState], dirty: &[u64]) {
    for &lba in dirty {
        // `dirty` holds only LBAs that `slot` validated.
        if let Some(state) = usize::try_from(lba)
            .ok()
            .and_then(|index| lbas.get_mut(index))
        {
            state.durable = state.latest;
            state.durable_is_payload = state.latest != state.last_zero;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViolationKind {
    /// The block does not decode as this LBA's payload.
    Corrupt,
    /// Zeros where a written version must be.
    LostData,
    /// An older version, or data where a discard must have left zeros.
    Stale,
    /// A version the target never acknowledged, including a rejected one.
    NeverAcknowledged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation {
    pub lba: u64,
    pub expected: Content,
    pub found: Content,
    pub kind: ViolationKind,
    pub after_crash: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub writes: u64,
    pub reads: u64,
    pub flushes: u64,
    pub discarded_blocks: u64,
    pub no_space: u64,
    pub uncertain: u64,
    pub repairs: u64,
    pub violation_count: u64,
    pub violations: Vec<Violation>,
}

#[derive(Debug)]
pub enum Error {
    InvalidBlockCount(u64),
    LbaOutOfRange {
        lba: u64,
        block_count: u64,
    },
    InvalidRange {
        lba: u64,
        blocks: u64,
        block_count: u64,
    },
    GeometryMismatch {
        model: u64,
        target: u64,
    },
    VersionOverflow {
        lba: u64,
    },
    /// More rejected writes than [`MAX_REJECTED_WRITES`]: the run is incomplete.
    RejectedWriteLimit,
    /// A second uncertain request of one kind on an LBA before recovery: the run is incomplete.
    UncertainLimit {
        lba: u64,
    },
    Io(IoFailure),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBlockCount(count) => {
                write!(formatter, "block count {count} is not in 1..={MAX_BLOCKS}")
            }
            Self::LbaOutOfRange { lba, block_count } => {
                write!(formatter, "LBA {lba} is outside {block_count} blocks")
            }
            Self::InvalidRange {
                lba,
                blocks,
                block_count,
            } => write!(
                formatter,
                "range {lba}+{blocks} is empty or outside {block_count} blocks"
            ),
            Self::GeometryMismatch { model, target } => {
                write!(formatter, "target has {target} blocks, model has {model}")
            }
            Self::VersionOverflow { lba } => write!(formatter, "LBA {lba} ran out of versions"),
            Self::RejectedWriteLimit => write!(
                formatter,
                "more than {MAX_REJECTED_WRITES} rejected writes; run incomplete"
            ),
            Self::UncertainLimit { lba } => write!(
                formatter,
                "LBA {lba} has two uncertain requests before recovery; run incomplete"
            ),
            Self::Io(failure) => failure.fmt(formatter),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Io(failure) => Some(failure),
            _ => None,
        }
    }
}

/// Drives a target and checks every read against the model. One request at a time.
pub struct Checker {
    codec: PayloadCodec,
    block_count: u64,
    lbas: Vec<LbaState>,
    dirty: Vec<u64>,
    rejected: HashSet<(u64, u32)>,
    scratch: Box<Block>,
    report: Report,
}

impl fmt::Debug for Checker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Checker")
            .field("block_count", &self.block_count)
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

fn bump(counter: &mut u64, amount: u64) {
    *counter = counter.saturating_add(amount);
}

impl Checker {
    /// # Errors
    /// Rejects a block count of zero or above [`MAX_BLOCKS`].
    pub fn new(seed: u64, block_count: u64) -> Result<Self, Error> {
        if block_count == 0 || block_count > MAX_BLOCKS {
            return Err(Error::InvalidBlockCount(block_count));
        }
        let len =
            usize::try_from(block_count).map_err(|_| Error::InvalidBlockCount(block_count))?;
        Ok(Self {
            codec: PayloadCodec::new(seed),
            block_count,
            lbas: vec![LbaState::default(); len],
            dirty: Vec::with_capacity(len),
            rejected: HashSet::with_capacity(MAX_REJECTED_WRITES),
            scratch: Box::new(Block::zeroed()),
            report: Report {
                violations: Vec::with_capacity(MAX_RECORDED_VIOLATIONS),
                ..Report::default()
            },
        })
    }

    pub const fn block_count(&self) -> u64 {
        self.block_count
    }

    pub const fn report(&self) -> &Report {
        &self.report
    }

    /// Writes the next version of `lba`.
    ///
    /// # Errors
    /// Rejects an out-of-range LBA; returns the target's failure. After `Io(Other)`, the effect
    /// is uncertain until the target crashes and [`Checker::recover`] runs.
    pub fn write(&mut self, target: &mut impl BlockTarget, lba: u64) -> Result<(), Error> {
        self.submit_write(target, lba, false)
    }

    /// Writes a zero block, the write-zeroes operation.
    ///
    /// # Errors
    /// As [`Checker::write`].
    pub fn write_zero(&mut self, target: &mut impl BlockTarget, lba: u64) -> Result<(), Error> {
        self.submit_write(target, lba, true)
    }

    /// # Errors
    /// Rejects an empty or out-of-range range; otherwise as [`Checker::write`].
    pub fn discard(
        &mut self,
        target: &mut impl BlockTarget,
        lba: u64,
        blocks: u64,
    ) -> Result<(), Error> {
        let end = lba
            .checked_add(blocks)
            .filter(|&end| blocks > 0 && end <= self.block_count)
            .ok_or(Error::InvalidRange {
                lba,
                blocks,
                block_count: self.block_count,
            })?;
        for each in lba..end {
            let _issued: NonZeroU32 = self.issue(each)?;
        }
        let result = target.discard(lba, blocks);
        for each in lba..end {
            let version = self.slot(each)?.issued;
            self.apply(each, version, true, &result)?;
        }
        self.finish(result)?;
        bump(&mut self.report.discarded_blocks, blocks);
        Ok(())
    }

    /// # Errors
    /// Returns the target's failure; the model then keeps earlier requests non-durable.
    pub fn flush(&mut self, target: &mut impl BlockTarget) -> Result<(), Error> {
        target.flush().map_err(Error::Io)?;
        make_durable(&mut self.lbas, &self.dirty);
        self.dirty.clear();
        bump(&mut self.report.flushes, 1);
        Ok(())
    }

    /// Reads `lba` and records a violation if it is not the latest acknowledged content.
    ///
    /// # Errors
    /// Rejects an out-of-range LBA; returns the target's failure.
    pub fn read(&mut self, target: &mut impl BlockTarget, lba: u64) -> Result<(), Error> {
        let state = *self.slot(lba)?;
        let found = self.read_content(target, lba)?;
        let kind = live_violation(state, found, self.was_rejected(lba, found));
        self.record(kind.map(|kind| Violation {
            lba,
            expected: state.expected(),
            found,
            kind,
            after_crash: false,
        }));
        Ok(())
    }

    /// Checks a target that crashed and restarted. It first reads every LBA without writing and
    /// records the verdicts. Then it rewrites each mismatch with a new version and flushes, so the
    /// model is exact again. On error the run is incomplete.
    ///
    /// # Errors
    /// Rejects a target of another size; returns the target's failure.
    pub fn recover(&mut self, target: &mut impl BlockTarget) -> Result<(), Error> {
        if target.block_count() != self.block_count {
            return Err(Error::GeometryMismatch {
                model: self.block_count,
                target: target.block_count(),
            });
        }
        for lba in 0..self.block_count {
            self.verify_after_crash(target, lba)?;
        }
        for lba in 0..self.block_count {
            self.repair(target, lba)?;
        }
        self.flush(target)
    }

    fn verify_after_crash(&mut self, target: &mut impl BlockTarget, lba: u64) -> Result<(), Error> {
        let found = self.read_content(target, lba)?;
        let rejected = self.was_rejected(lba, found);
        let state = self.slot(lba)?;
        let before = *state;
        state.needs_repair = found != before.expected();
        state.uncertain_write = 0;
        state.uncertain_zero = 0;
        // After recovery, a retired uncertain payload is never a valid result again. Rejecting it
        // keeps every unrejected version after `durable` an acknowledged one.
        if before.uncertain_write != 0 {
            self.reject(lba, before.uncertain_write)?;
        }
        let kind = crash_violation(before, found, rejected);
        self.record(kind.map(|kind| Violation {
            lba,
            expected: before.expected(),
            found,
            kind,
            after_crash: true,
        }));
        Ok(())
    }

    fn repair(&mut self, target: &mut impl BlockTarget, lba: u64) -> Result<(), Error> {
        let state = self.slot(lba)?;
        if !state.needs_repair {
            return Ok(());
        }
        state.needs_repair = false;
        let zero = state.expected() == Content::Zero;
        bump(&mut self.report.repairs, 1);
        self.submit_write(target, lba, zero)
    }

    fn submit_write(
        &mut self,
        target: &mut impl BlockTarget,
        lba: u64,
        zero: bool,
    ) -> Result<(), Error> {
        let version = self.issue(lba)?;
        if zero {
            self.scratch.0.fill(0);
        } else {
            self.codec.fill(&mut self.scratch, lba, version);
        }
        let result = target.write(lba, &self.scratch);
        self.apply(lba, version.get(), zero, &result)?;
        self.finish(result)?;
        bump(&mut self.report.writes, 1);
        Ok(())
    }

    /// Updates one LBA's model with the result of a request that covered it.
    fn apply(
        &mut self,
        lba: u64,
        version: u32,
        zero: bool,
        result: &Result<(), IoFailure>,
    ) -> Result<(), Error> {
        match result {
            Ok(()) => self.acknowledge(lba, version, zero),
            Err(IoFailure::NoSpace | IoFailure::Unsupported) if zero => Ok(()),
            Err(IoFailure::NoSpace | IoFailure::Unsupported) => self.reject(lba, version),
            Err(IoFailure::Other(_)) => self.mark_uncertain(lba, version, zero),
        }
    }

    /// Counts a request's failure once and returns it.
    fn finish(&mut self, result: Result<(), IoFailure>) -> Result<(), Error> {
        let Err(failure) = result else {
            return Ok(());
        };
        match failure {
            IoFailure::NoSpace => bump(&mut self.report.no_space, 1),
            IoFailure::Other(_) => bump(&mut self.report.uncertain, 1),
            IoFailure::Unsupported => {}
        }
        Err(Error::Io(failure))
    }

    fn acknowledge(&mut self, lba: u64, version: u32, zero: bool) -> Result<(), Error> {
        let state = self.slot(lba)?;
        let newly_dirty = !state.dirty();
        state.latest = version;
        if zero {
            state.last_zero = version;
        }
        if newly_dirty {
            self.dirty.push(lba);
        }
        Ok(())
    }

    fn reject(&mut self, lba: u64, version: u32) -> Result<(), Error> {
        if self.rejected.len() >= MAX_REJECTED_WRITES {
            return Err(Error::RejectedWriteLimit);
        }
        let _newly_inserted: bool = self.rejected.insert((lba, version));
        Ok(())
    }

    fn mark_uncertain(&mut self, lba: u64, version: u32, zero: bool) -> Result<(), Error> {
        let state = self.slot(lba)?;
        let slot = if zero {
            &mut state.uncertain_zero
        } else {
            &mut state.uncertain_write
        };
        if *slot != 0 {
            return Err(Error::UncertainLimit { lba });
        }
        *slot = version;
        Ok(())
    }

    fn was_rejected(&self, lba: u64, found: Content) -> bool {
        match found {
            Content::Version(version) => self.rejected.contains(&(lba, version.get())),
            Content::Zero | Content::Invalid => false,
        }
    }

    fn read_content(&mut self, target: &mut impl BlockTarget, lba: u64) -> Result<Content, Error> {
        target.read(lba, &mut self.scratch).map_err(Error::Io)?;
        bump(&mut self.report.reads, 1);
        Ok(self.codec.decode(&self.scratch, lba))
    }

    fn slot(&mut self, lba: u64) -> Result<&mut LbaState, Error> {
        let out_of_range = Error::LbaOutOfRange {
            lba,
            block_count: self.block_count,
        };
        if lba >= self.block_count {
            return Err(out_of_range);
        }
        usize::try_from(lba)
            .ok()
            .and_then(|index| self.lbas.get_mut(index))
            .ok_or(out_of_range)
    }

    fn issue(&mut self, lba: u64) -> Result<NonZeroU32, Error> {
        let state = self.slot(lba)?;
        let version = state
            .issued
            .checked_add(1)
            .and_then(NonZeroU32::new)
            .ok_or(Error::VersionOverflow { lba })?;
        state.issued = version.get();
        Ok(version)
    }

    fn record(&mut self, violation: Option<Violation>) {
        let Some(violation) = violation else {
            return;
        };
        bump(&mut self.report.violation_count, 1);
        if self.report.violations.len() < MAX_RECORDED_VIOLATIONS {
            self.report.violations.push(violation);
        }
    }
}
