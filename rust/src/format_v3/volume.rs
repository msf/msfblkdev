use super::{checkpoint_load, mapping_state::MappingState, recovery, storage};
use crate::{
    AlignedBlock, BLOCK_SIZE, VolumeOpenOptions,
    accounted_io::BackingIo,
    clear_stale_tail, invalid_data, invalid_input, io_accounting,
    mapping_cache::{ENTRIES_PER_PAGE, Snapshot},
    payload_checksum, read_block_exact,
};
use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) struct Volume {
    backing: File,
    backing_io: BackingIo,
    mapping: MappingState,
    read_scratch: AlignedBlock,
    failed: bool,
}

pub(crate) fn open(
    path: &Path,
    options: VolumeOpenOptions,
    cache_budget: Option<u64>,
    accounting: Option<io_accounting::Writer>,
) -> io::Result<Volume> {
    options.validate()?;
    let mut backing = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT | libc::O_CLOEXEC)
        .open(path)?;
    let bytes = backing.seek(SeekFrom::End(0))?;
    if !bytes.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(invalid_input("backing size is not block aligned"));
    }
    let mut backing_io = BackingIo::new(accounting)?;
    let mut checkpoint =
        checkpoint_load::load(&mut backing_io, &backing, bytes / BLOCK_SIZE as u64)?;
    let recovered = recovery::recover(&mut backing_io, &backing, &mut checkpoint, cache_budget)?;
    clear_stale_tail(
        &mut backing_io,
        &backing,
        u64::from(recovered.tail.boundary) + 1,
        checkpoint.descriptor.backing_blocks,
    )?;
    Ok(Volume {
        backing,
        backing_io,
        mapping: MappingState {
            checkpoint,
            cache: recovered.cache,
            tail: recovered.tail,
            recovery: recovered.stats,
            pressure_checkpoints: 0,
            checkpoint_failures: 0,
        },
        read_scratch: AlignedBlock([0; BLOCK_SIZE]),
        failed: false,
    })
}

impl Volume {
    pub(crate) fn volume_blocks(&self) -> u32 {
        self.mapping.checkpoint.descriptor.volume_blocks
    }

    pub(crate) fn cache_snapshot(&self) -> Snapshot {
        let mut snapshot = self.mapping.cache.snapshot();
        snapshot.counters.recovery = self.mapping.recovery.lookups;
        snapshot
    }

    pub(crate) fn pressure_checkpoints(&self) -> u64 {
        self.mapping.pressure_checkpoints
    }

    pub(crate) fn read_block(&mut self, lba: u32, data: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        self.check_operational()?;
        if lba >= self.volume_blocks() {
            return Err(invalid_input("invalid logical block"));
        }
        let page_id = lba / ENTRIES_PER_PAGE as u32;
        let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
        let result = self.mapping.with_page(
            &mut self.backing_io,
            &self.backing,
            page_id,
            |page, _, _| {
                Ok((
                    page.entries().physical_addr[entry],
                    page.entries().payload_checksum[entry],
                ))
            },
        );
        let (physical, checksum) = self.latch(result)?;
        if physical == 0 {
            data.fill(0);
            return Ok(());
        }
        let read = read_block_exact(
            &mut self.backing_io,
            &self.backing,
            physical,
            &mut self.read_scratch,
        );
        self.latch(read)?;
        if payload_checksum(
            self.mapping.checkpoint.descriptor.volume_id,
            lba,
            physical,
            &self.read_scratch.0,
        ) != checksum
        {
            return Err(invalid_data("payload checksum mismatch"));
        }
        data.copy_from_slice(&self.read_scratch.0);
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.check_operational()?;
        let result = storage::sync(&mut self.backing_io, &self.backing);
        self.latch(result)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::LOG_FSYNC_COMPLETE_FAILPOINT)?;
        Ok(())
    }

    pub(crate) fn close(mut self) -> io::Result<()> {
        self.check_operational()?;
        if self.mapping.tail.lsn == self.mapping.checkpoint.descriptor.checkpoint_lsn {
            return self.flush();
        }
        let result = self.mapping.publish(&mut self.backing_io, &self.backing);
        self.latch(result)
    }

    fn check_operational(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        Ok(())
    }

    fn latch<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
