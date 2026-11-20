//! FSYNC-01. Keep exact data-record writes until an explicit records barrier succeeds.
use super::*;
use crate::accounted_io::submit_exact;
use io_uring::{opcode, types};
use std::{os::fd::AsRawFd, time::Duration};

// One maximum-sized format-4 data record must fit even in an empty epoch.
const BLOCKS: usize = crate::MAXIMUM_RECORD_BLOCKS as usize;
// Each extent includes at least one payload and its footer.
const EXTENTS: usize = BLOCKS / 2;
const HEAP_BYTES: usize = BLOCKS * BLOCK_SIZE + EXTENTS * size_of::<(u32, u32)>();
const BACKOFF: Duration = Duration::from_millis(50); // ADR-05 first retry.

pub(super) struct Epoch {
    blocks: Vec<AlignedBlock>,
    // (physical block, length in blocks), in original submission order.
    extents: Vec<(u32, u32)>,
}

impl Epoch {
    fn clear(&mut self) {
        self.blocks.clear();
        self.extents.clear();
    }

    fn replay(&self, io: &mut BackingIo, backing: &File) -> io::Result<()> {
        let mut cursor = 0;
        for &(physical, count) in &self.extents {
            let end = cursor + count as usize;
            let blocks = &self.blocks[cursor..end];
            let bytes = std::mem::size_of_val(blocks);
            let write = opcode::Write::new(
                types::Fd(backing.as_raw_fd()),
                blocks.as_ptr().cast(),
                bytes as u32,
            )
            .offset(u64::from(physical) * BLOCK_SIZE as u64)
            .build();
            submit_exact(io, write, u64::from(physical), bytes as i32)?;
            cursor = end;
        }
        storage::sync(io, backing)
    }
}

impl Volume {
    /// Opt in to one ordered replay after a terminal EIO on an explicit records flush.
    /// The caller must already own the backing exclusively, including during open/recovery.
    /// Holds an advisory exclusive lock until drop; does not support the I/O ledger or hang
    /// deadlines. Requires room in the volume heap for one maximum record and its extents.
    /// The first successful client flush establishes a baseline. Epochs containing metadata
    /// or relocation I/O use the default fail-closed policy until another flush succeeds.
    pub fn enable_records_flush_retry(&mut self) -> io::Result<()> {
        self.check_operational()?;
        if self.records_epoch.is_some() {
            return Ok(());
        }
        if !self.backing_io.can_retry_completed_io() {
            return Err(invalid_input(
                "records replay needs a live, unaccounted ring",
            ));
        }
        if self.budget.total_bytes - self.budget.allocated_bytes() < HEAP_BYTES as u64 {
            return Err(invalid_input("insufficient heap for retained records"));
        }
        self.backing.try_lock()?;
        self.records_epoch = Some(Epoch {
            blocks: Vec::with_capacity(BLOCKS),
            extents: Vec::with_capacity(EXTENTS),
        });
        self.budget.supporting_bytes += HEAP_BYTES as u64;
        Ok(())
    }

    pub(super) fn retain_user_record(&mut self, start: u32, lbas: &[u32]) -> io::Result<()> {
        let Some(epoch) = &mut self.records_epoch else {
            return Ok(());
        };
        if !self.backing_io.records_only {
            return Ok(());
        }
        let count = lbas.len() + 1;
        if epoch.blocks.len() + count > BLOCKS {
            // Pay the previous epoch before reusing its memory. If the barrier fails,
            // the enclosing write path latches failure and does not submit the new record.
            storage::sync(&mut self.backing_io, &self.backing)?;
            epoch.clear();
            self.backing_io.records_only = true;
        }
        for &lba in lbas {
            let bytes = self
                .write_buffer
                .get(lba)
                .expect("encoded record has buffered payload");
            epoch.blocks.push(AlignedBlock(*bytes));
        }
        epoch.blocks.push(AlignedBlock(self.footer_scratch.0));
        epoch.extents.push((start, count as u32));
        Ok(())
    }

    pub(super) fn sync_client_records(&mut self) -> io::Result<()> {
        let eligible = self.backing_io.records_only;
        let result = storage::sync(&mut self.backing_io, &self.backing);
        let Some(epoch) = &mut self.records_epoch else {
            return result;
        };
        if let Err(error) = result {
            if !eligible
                || epoch.extents.is_empty()
                || error.raw_os_error() != Some(libc::EIO)
                || !self.backing_io.can_retry_completed_io()
            {
                return Err(error);
            }
            // The original has a terminal completion. &mut Volume serializes later calls;
            // replay uses the same locked fd and changes neither mapping nor log position.
            let through_lsn = self.log.next_lsn - 1;
            crate::log_info!(
                "event=records_replay through_lsn={through_lsn} attempt=2 barrier_errno={} records={} blocks={}",
                libc::EIO,
                epoch.extents.len(),
                epoch.blocks.len()
            );
            let started = std::time::Instant::now();
            std::thread::sleep(BACKOFF);
            let replay = epoch.replay(&mut self.backing_io, &self.backing);
            crate::log_info!(
                "event=records_replay_done through_lsn={through_lsn} attempt=2 elapsed_ms={} success={} errno={:?}",
                started.elapsed().as_millis(),
                replay.is_ok(),
                replay.as_ref().err().and_then(io::Error::raw_os_error)
            );
            replay?;
        }
        epoch.clear();
        self.backing_io.records_only = self.reclaim.discards_in_flight == 0;
        Ok(())
    }
}
