use super::{
    checkpoint_load,
    mapping_state::{CheckpointPolicy, MappingState},
    recovery, storage,
};
mod write;
use crate::{
    AlignedBlock, BLOCK_SIZE, ReleaseIovecs, ReleaseStats, ReleaseTrigger, SpaceUsage,
    VolumeOpenOptions, WriteBackBuffer,
    accounted_io::BackingIo,
    clamp_write_buffer_blocks, clear_stale_tail, invalid_data, invalid_input, io_accounting,
    mapping_cache::{ENTRIES_PER_PAGE, Snapshot},
    payload_checksum, read_block_exact,
};
use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

pub struct Volume {
    pub(crate) backing: File,
    backing_io: BackingIo,
    pub(crate) mapping: MappingState,
    read_scratch: AlignedBlock,
    write_buffer: WriteBackBuffer,
    buffered_pages: usize,
    footer_scratch: AlignedBlock,
    release_iovecs: ReleaseIovecs,
    release_stats: ReleaseStats,
    pub(crate) checkpoint_after_bytes: u64,
    live_blocks: u64,
    pub(crate) failed: bool,
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
    let (mut checkpoint, live_blocks) =
        checkpoint_load::load(&mut backing_io, &backing, bytes / BLOCK_SIZE as u64)?;
    let buffer_blocks = clamp_write_buffer_blocks(
        options.write_buffer_bytes,
        checkpoint.descriptor.backing_blocks,
        checkpoint.descriptor.geometry()?.log_start,
    );
    let cache_budget = Some(validate_buffer_mapping_capacity(
        options,
        cache_budget,
        &checkpoint,
        buffer_blocks,
    )?);
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
            last_read_page: None,
        },
        read_scratch: AlignedBlock([0; BLOCK_SIZE]),
        write_buffer: WriteBackBuffer::with_capacity(buffer_blocks),
        buffered_pages: 0,
        footer_scratch: AlignedBlock([0; BLOCK_SIZE]),
        release_iovecs: ReleaseIovecs(Vec::with_capacity(crate::MAXIMUM_PAYLOAD_BLOCKS + 1)),
        release_stats: ReleaseStats::default(),
        checkpoint_after_bytes: options.checkpoint_after_bytes,
        live_blocks: live_blocks + recovered.stats.newly_mapped_blocks,
        failed: false,
    })
}

fn validate_buffer_mapping_capacity(
    options: VolumeOpenOptions,
    budget: Option<u64>,
    checkpoint: &super::index::Checkpoint,
    buffer_blocks: usize,
) -> io::Result<u64> {
    use crate::mapping_cache::sizing::Sizing;
    let logical_bytes = u64::from(checkpoint.descriptor.volume_blocks) * BLOCK_SIZE as u64;
    let required = buffer_blocks.min(checkpoint.descriptor.geometry()?.page_count as usize);
    let minimum = Sizing::for_capacity(required).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mapping cache: {error:?}"),
        )
    })?;
    let budget = budget.unwrap_or((logical_bytes / 10_000).max(minimum.allocated_bytes as u64));
    let sizing = Sizing::new(logical_bytes, Some(budget)).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mapping cache: {error:?}"),
        )
    })?;
    if sizing.capacity < required {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mapping cache has {} slots; write buffer requires {required} slots (at least {} cache bytes)",
                sizing.capacity, minimum.allocated_bytes,
            ),
        ));
    }
    let payloads = buffer_blocks.min(checkpoint.descriptor.volume_blocks as usize);
    let records = payloads.div_ceil(crate::MAXIMUM_PAYLOAD_BLOCKS);
    if (payloads + records) as u64 * BLOCK_SIZE as u64 > options.checkpoint_after_bytes {
        return Err(invalid_input(
            "checkpoint_after_bytes must hold one packed write buffer",
        ));
    }
    Ok(budget)
}

impl Volume {
    #[cfg(test)]
    pub(crate) fn inject_metadata_short_read(&mut self, bytes: usize) {
        self.backing_io.metadata_short_read = Some(bytes);
    }

    #[cfg(test)]
    pub(crate) fn inject_metadata_short_write(&mut self, physical: u32, bytes: usize) {
        self.backing_io.metadata_short_write = Some((physical, bytes));
    }

    pub fn volume_blocks(&self) -> u32 {
        self.mapping.checkpoint.descriptor.volume_blocks
    }

    pub fn release_stats(&self) -> ReleaseStats {
        self.release_stats
    }

    pub fn mapping_stats(&self) -> super::metrics::MappingStats {
        super::metrics::MappingStats {
            cache: self.cache_snapshot(),
            io: self.backing_io.mapping_io,
            index_bytes: self.mapping.checkpoint.allocated_bytes(),
            pressure_checkpoints: self.mapping.pressure_checkpoints,
            checkpoint_failures: self.mapping.checkpoint_failures,
            recovery_passes: self.mapping.recovery.tail_passes,
        }
    }

    pub fn space_usage(&self) -> io::Result<SpaceUsage> {
        let geometry = self.mapping.checkpoint.descriptor.geometry()?;
        let metadata_blocks = u64::from(geometry.log_start);
        let metadata = self.backing.metadata()?;
        Ok(SpaceUsage {
            volume_bytes: u64::from(geometry.volume_blocks) * BLOCK_SIZE as u64,
            live_bytes: self.live_blocks * BLOCK_SIZE as u64,
            backing_bytes: metadata.len(),
            allocated_bytes: metadata.blocks() * 512,
            metadata_bytes: metadata_blocks * BLOCK_SIZE as u64,
            log_capacity_bytes: (geometry.backing_blocks - metadata_blocks) * BLOCK_SIZE as u64,
            log_used_bytes: (u64::from(self.mapping.tail.boundary) + 1 - metadata_blocks)
                * BLOCK_SIZE as u64,
        })
    }

    pub(crate) fn cache_snapshot(&self) -> Snapshot {
        let mut snapshot = self.mapping.cache.snapshot();
        snapshot.counters.recovery = self.mapping.recovery.lookups;
        snapshot
    }

    pub fn read_block(&mut self, lba: u32, data: &mut [u8; BLOCK_SIZE]) -> io::Result<()> {
        self.check_operational()?;
        if lba >= self.volume_blocks() {
            return Err(invalid_input("invalid logical block"));
        }
        if let Some(buffered) = self.write_buffer.get(lba) {
            data.copy_from_slice(buffered);
            return Ok(());
        }
        let page_id = lba / ENTRIES_PER_PAGE as u32;
        let entry = (lba % ENTRIES_PER_PAGE as u32) as usize;
        let result = self.mapping.with_page(
            &mut self.backing_io,
            &self.backing,
            page_id,
            CheckpointPolicy::Allow,
            |page| {
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

    pub fn flush(&mut self) -> io::Result<()> {
        self.check_operational()?;
        self.release_records(ReleaseTrigger::Flush)?;
        let result = storage::sync(&mut self.backing_io, &self.backing);
        self.latch(result)?;
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint(crate::LOG_FSYNC_COMPLETE_FAILPOINT)?;
        Ok(())
    }

    pub fn close(mut self) -> io::Result<()> {
        self.check_operational()?;
        self.release_records(ReleaseTrigger::Flush)?;
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
