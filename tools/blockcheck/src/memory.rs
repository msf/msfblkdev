//! A simulated disk with a volatile write cache and optional faults. It tests the checker.

use std::io;

use crate::payload::Block;
use crate::rng::SplitMix64;
use crate::target::{BlockTarget, IoFailure};

/// 16 MiB of simulated disk: enough for checker tests, small enough to copy freely.
pub const MAX_BLOCKS: u64 = 4096;

/// A defect the checker must detect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Reads ignore the write cache.
    StaleRead,
    /// Writes land on the neighbouring LBA.
    MisdirectedWrite,
    /// Discards succeed and change nothing.
    IgnoredDiscard,
    /// Flushes succeed and persist nothing.
    LostFlush,
    /// Flushes persist writes but leave discards in the volatile cache.
    UnflushedDiscard,
    /// A crash restores each block's first written contents.
    Resurrection,
    /// A write rejected with `NoSpace` still reaches the medium.
    RejectedWriteLands,
}

/// A fault that is active from request `from_request` onwards. Requests count from zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultPlan {
    pub fault: Fault,
    pub from_request: u64,
}

/// A failure that the next write or discard reports. A correct target can fail like this too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Injected {
    /// Reject with `NoSpace` and leave storage unchanged.
    NoSpace,
    /// Report an I/O error after the request reached the write cache.
    IoErrorApplied,
    /// Report an I/O error before the request reached the write cache.
    IoErrorDropped,
}

/// Which unflushed requests a crash keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Survival {
    Random,
    KeepAll,
    DropAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    Write,
    Discard,
}

/// Each LBA holds at most one unflushed request, so after a crash every LBA is in its durable or
/// its latest state.
#[derive(Debug)]
pub struct MemoryTarget {
    block_count: u64,
    durable: Vec<Block>,
    cached: Vec<Block>,
    pending: Vec<Pending>,
    first: Vec<Option<Block>>,
    plan: Option<FaultPlan>,
    requests: u64,
    injected: Option<Injected>,
    faults_occurred: u64,
    rng: SplitMix64,
}

fn out_of_range(lba: u64) -> IoFailure {
    IoFailure::Other(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("LBA {lba} out of range"),
    ))
}

fn injected_io_error() -> IoFailure {
    IoFailure::Other(io::Error::other("injected I/O error"))
}

fn survives(rng: &mut SplitMix64, survival: Survival) -> bool {
    match survival {
        Survival::KeepAll => true,
        Survival::DropAll => false,
        Survival::Random => rng.next_u64() & 1 != 0,
    }
}

/// Resolves one block's unflushed request and returns what stays pending.
fn settle_block(
    pending: Pending,
    stays: bool,
    keep: bool,
    durable: &mut Block,
    cached: &Block,
) -> Pending {
    if stays {
        return pending;
    }
    match (pending, keep) {
        (Pending::Write, true) => durable.clone_from(cached),
        (Pending::Discard, true) => durable.0.fill(0),
        (Pending::None, _) | (_, false) => (),
    }
    Pending::None
}

/// Restores a block's first contents and reports whether that changed it.
fn restore(durable: &mut Block, first: &Block) -> bool {
    if durable.0 == first.0 {
        return false;
    }
    durable.clone_from(first);
    true
}

fn bump(counter: &mut u64) {
    count_if(counter, true);
}

fn count_if(counter: &mut u64, happened: bool) {
    *counter = counter.saturating_add(u64::from(happened));
}

impl MemoryTarget {
    /// # Errors
    /// Rejects a block count of zero or above [`MAX_BLOCKS`].
    pub fn new(block_count: u64, plan: Option<FaultPlan>, seed: u64) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "block count out of range");
        if block_count == 0 || block_count > MAX_BLOCKS {
            return Err(invalid());
        }
        let len = usize::try_from(block_count).map_err(|_| invalid())?;
        let first = if plan.is_some_and(|plan| plan.fault == Fault::Resurrection) {
            vec![None; len]
        } else {
            Vec::new()
        };
        Ok(Self {
            block_count,
            durable: vec![Block::zeroed(); len],
            cached: vec![Block::zeroed(); len],
            pending: vec![Pending::None; len],
            first,
            plan,
            requests: 0,
            injected: None,
            faults_occurred: 0,
            rng: SplitMix64::new(seed),
        })
    }

    /// Makes the next write or discard fail as `failure`. A crash cancels it.
    pub const fn inject(&mut self, failure: Injected) {
        self.injected = Some(failure);
    }

    /// How many times the planned fault changed a result.
    pub const fn faults_occurred(&self) -> u64 {
        self.faults_occurred
    }

    /// Simulates power loss.
    pub fn crash(&mut self, survival: Survival) {
        self.injected = None;
        self.settle(survival, false);
        if self.active() == Some(Fault::Resurrection) {
            self.resurrect();
        }
    }

    fn active(&self) -> Option<Fault> {
        self.plan
            .filter(|plan| self.requests >= plan.from_request)
            .map(|plan| plan.fault)
    }

    /// Starts a request and returns the fault active for it.
    fn begin(&mut self) -> Option<Fault> {
        let active = self.active();
        bump(&mut self.requests);
        active
    }

    fn settle(&mut self, survival: Survival, discards_stay_cached: bool) {
        let entries = self
            .durable
            .iter_mut()
            .zip(&self.cached)
            .zip(self.pending.iter_mut());
        for ((durable, cached), pending) in entries {
            let stays = discards_stay_cached && *pending == Pending::Discard;
            let keep = !stays && survives(&mut self.rng, survival);
            *pending = settle_block(*pending, stays, keep, durable, cached);
        }
    }

    fn resurrect(&mut self) {
        for (durable, first) in self.durable.iter_mut().zip(&self.first) {
            let restored = first.as_ref().is_some_and(|first| restore(durable, first));
            count_if(&mut self.faults_occurred, restored);
        }
    }

    fn index(&self, lba: u64) -> Result<usize, IoFailure> {
        if lba >= self.block_count {
            return Err(out_of_range(lba));
        }
        usize::try_from(lba).map_err(|_| out_of_range(lba))
    }

    fn reject_write(
        &mut self,
        index: usize,
        block: &Block,
        active: Option<Fault>,
    ) -> Result<(), IoFailure> {
        if active == Some(Fault::RejectedWriteLands)
            && let Some(durable) = self.durable.get_mut(index)
        {
            durable.clone_from(block);
            bump(&mut self.faults_occurred);
        }
        Err(IoFailure::NoSpace)
    }

    fn cache_write(
        &mut self,
        mut index: usize,
        block: &Block,
        active: Option<Fault>,
    ) -> Result<(), IoFailure> {
        if active == Some(Fault::MisdirectedWrite) && index ^ 1 < self.cached.len() {
            index ^= 1;
            bump(&mut self.faults_occurred);
        }
        let (Some(cached), Some(pending)) =
            (self.cached.get_mut(index), self.pending.get_mut(index))
        else {
            return Err(out_of_range(u64::try_from(index).unwrap_or(u64::MAX)));
        };
        cached.clone_from(block);
        *pending = Pending::Write;
        if let Some(first) = self.first.get_mut(index) {
            let _first: &mut Block = first.get_or_insert_with(|| block.clone());
        }
        Ok(())
    }

    fn cache_discard(
        &mut self,
        start: usize,
        end: usize,
        active: Option<Fault>,
    ) -> Result<(), IoFailure> {
        if active == Some(Fault::IgnoredDiscard) {
            bump(&mut self.faults_occurred);
            return Ok(());
        }
        self.pending
            .get_mut(start..end)
            .ok_or_else(|| out_of_range(u64::try_from(start).unwrap_or(u64::MAX)))?
            .fill(Pending::Discard);
        Ok(())
    }
}

impl BlockTarget for MemoryTarget {
    fn block_count(&self) -> u64 {
        self.block_count
    }

    fn read(&mut self, lba: u64, block: &mut Block) -> Result<(), IoFailure> {
        let active = self.begin();
        let index = self.index(lba)?;
        let mut pending = self
            .pending
            .get(index)
            .copied()
            .ok_or_else(|| out_of_range(lba))?;
        if active == Some(Fault::StaleRead) && pending != Pending::None {
            pending = Pending::None;
            bump(&mut self.faults_occurred);
        }
        let source = match pending {
            Pending::Discard => {
                block.0.fill(0);
                return Ok(());
            }
            Pending::Write => self.cached.get(index),
            Pending::None => self.durable.get(index),
        };
        block.clone_from(source.ok_or_else(|| out_of_range(lba))?);
        Ok(())
    }

    fn write(&mut self, lba: u64, block: &Block) -> Result<(), IoFailure> {
        let active = self.begin();
        let index = self.index(lba)?;
        match self.injected.take() {
            Some(Injected::NoSpace) => self.reject_write(index, block, active),
            Some(Injected::IoErrorDropped) => Err(injected_io_error()),
            Some(Injected::IoErrorApplied) => self
                .cache_write(index, block, active)
                .and_then(|()| Err(injected_io_error())),
            None => self.cache_write(index, block, active),
        }
    }

    fn flush(&mut self) -> Result<(), IoFailure> {
        let active = self.begin();
        let has_write = self.pending.contains(&Pending::Write);
        let has_discard = self.pending.contains(&Pending::Discard);
        let changed = match active {
            Some(Fault::LostFlush) => has_write || has_discard,
            Some(Fault::UnflushedDiscard) => has_discard,
            _ => false,
        };
        count_if(&mut self.faults_occurred, changed);
        match active {
            Some(Fault::LostFlush) => (),
            Some(Fault::UnflushedDiscard) => self.settle(Survival::KeepAll, true),
            _ => self.settle(Survival::KeepAll, false),
        }
        Ok(())
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), IoFailure> {
        let active = self.begin();
        let start = self.index(lba)?;
        let end = lba
            .checked_add(blocks)
            .filter(|&end| end <= self.block_count)
            .and_then(|end| usize::try_from(end).ok())
            .ok_or_else(|| out_of_range(lba))?;
        match self.injected.take() {
            Some(Injected::NoSpace) => Err(IoFailure::NoSpace),
            Some(Injected::IoErrorDropped) => Err(injected_io_error()),
            Some(Injected::IoErrorApplied) => self
                .cache_discard(start, end, active)
                .and_then(|()| Err(injected_io_error())),
            None => self.cache_discard(start, end, active),
        }
    }
}
