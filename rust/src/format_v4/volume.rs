//! The public format-4 engine: open, read, write, flush, discard, growth and close.
use super::{
    Geometry, HeadId,
    budget::{self, CleanerShape, HeapBudget, SupportingState},
    checkpoint_load,
    geometry::MAX_SEGMENTS,
    heads::Reserve,
    log::Log,
    mapping_state::{CheckpointPolicy, MappingState},
    metrics::{MappingStats, SpaceUsage},
    recovery, storage,
};
use crate::{
    AlignedBlock, BLOCK_SIZE, ReleaseIovecs, ReleaseStats, ReleaseTrigger, VolumeOpenOptions,
    WriteBackBuffer,
    accounted_io::BackingIo,
    invalid_data, invalid_input, io_accounting,
    mapping_cache::{ENTRIES_PER_PAGE, Snapshot},
    payload_checksum, read_block_exact,
};
use std::{fs::File, io, os::unix::fs::MetadataExt, path::Path};

mod discard;
mod growth;
pub(crate) mod reclaim;
mod write;

/// Limit buffer allocation to a quarter of the segment capacity, as format 3 did for its log.
const WRITE_BUFFER_DEVICE_DIVISOR: u64 = 4;

pub struct Volume {
    pub(crate) backing: File,
    pub(crate) backing_io: BackingIo,
    pub(crate) log: Log,
    pub(crate) mapping: MappingState,
    read_scratch: AlignedBlock,
    write_buffer: WriteBackBuffer,
    buffered_pages: usize,
    footer_scratch: AlignedBlock,
    release_iovecs: ReleaseIovecs,
    release_stats: ReleaseStats,
    pub(crate) checkpoint_after_bytes: u64,
    live_blocks: u64,
    pub(crate) reserve: Reserve,
    budget: HeapBudget,
    pub(crate) actual_backing_blocks: u64,
    pub(crate) reclaim: reclaim::Reclaim,
    pub(crate) failed: bool,
}

/// Segment-state slots to preallocate: room for growth to `growth_capacity_bytes`, or twice
/// the provisioned backing when unspecified, within the table's reach.
fn segment_capacity(options: VolumeOpenOptions, geometry: Geometry) -> u32 {
    let target_blocks = options
        .growth_capacity_bytes
        .map_or(geometry.backing_blocks.saturating_mul(2), |bytes| {
            bytes / BLOCK_SIZE as u64
        });
    let segments = target_blocks.saturating_sub(u64::from(super::FIRST_SEGMENT_BLOCK))
        / u64::from(geometry.blocks_per_segment);
    u32::try_from(segments.clamp(u64::from(geometry.segment_count), MAX_SEGMENTS)).expect("bounded")
}

fn clamp_write_buffer_blocks(write_buffer_bytes: u64, geometry: Geometry) -> usize {
    let configured = (write_buffer_bytes / BLOCK_SIZE as u64).max(1);
    let capacity = u64::from(geometry.segment_count) * u64::from(geometry.blocks_per_segment);
    let device_limit = (capacity / WRITE_BUFFER_DEVICE_DIVISOR).max(1);
    usize::try_from(configured.min(device_limit)).unwrap_or(1)
}

pub(crate) fn open(
    path: &Path,
    options: VolumeOpenOptions,
    accounting: Option<io_accounting::Writer>,
) -> io::Result<Volume> {
    options.validate()?;
    let (backing, actual_backing_blocks) = storage::open_backing(path)?;
    let mut backing_io = BackingIo::new(accounting)?;
    let loaded = checkpoint_load::load(
        &mut backing_io,
        &backing,
        actual_backing_blocks,
        |geometry| segment_capacity(options, geometry),
    )?;
    let mut log = loaded.log;
    let mut live_blocks = loaded.live_blocks;
    let geometry = log.geometry;
    if u64::from(geometry.table_pages) + 1 > u64::from(geometry.blocks_per_segment) {
        return Err(invalid_data(
            "a segment must hold every table page plus its link",
        ));
    }
    let buffer_blocks = clamp_write_buffer_blocks(options.write_buffer_bytes, geometry);
    let write_buffer = WriteBackBuffer::with_capacity(buffer_blocks);
    let logical_bytes = u64::from(geometry.volume_blocks) * BLOCK_SIZE as u64;
    let supporting = SupportingState {
        // The cleaner's scan view and the writer's spare are two more copies of the snapshot
        // locations.
        resident_index_bytes: log.index.allocated_bytes() as u64
            + 2 * u64::from(geometry.page_count) * 4,
        segment_state_bytes: log.segments.allocated_bytes() as u64,
        write_buffer_lookup_bytes: write_buffer.lookup_bytes() as u64,
        fixed_scratch_bytes: fixed_scratch_bytes(geometry, buffer_blocks),
    };
    let (budget, sizing, cleaner_shape) = budget::divide(
        options.heap,
        logical_bytes,
        buffer_blocks as u64 * BLOCK_SIZE as u64,
        options.cache_budget_bytes,
        buffer_blocks.min(geometry.page_count as usize),
        geometry,
        supporting,
    )?;
    let payloads = buffer_blocks.min(geometry.volume_blocks as usize);
    let records = payloads.div_ceil(crate::MAXIMUM_PAYLOAD_BLOCKS);
    if (payloads + records) as u64 * BLOCK_SIZE as u64 > options.checkpoint_after_bytes {
        return Err(invalid_input(
            "checkpoint_after_bytes must hold one packed write buffer",
        ));
    }
    let reserve = Reserve::compute(geometry, sizing.capacity as u64);
    let reclaim = reclaim::Reclaim::new(
        &backing,
        geometry,
        log.volume_id,
        cleaner_shape,
        options.backing_discard,
    )?;
    assert!(
        reclaim.allocated_bytes(geometry) as u64
            <= budget.cleaner_bytes + 2 * u64::from(geometry.page_count) * 4,
        "cleaner storage fits its share"
    );
    let recovered = recovery::recover(
        &mut backing_io,
        &backing,
        &mut log,
        actual_backing_blocks,
        Some(sizing.requested_bytes),
        &mut live_blocks,
    )?;
    Ok(Volume {
        backing,
        backing_io,
        log,
        mapping: MappingState {
            cache: recovered.cache,
            recovery: recovered.stats,
            pressure_checkpoints: 0,
            checkpoint_failures: 0,
            last_read_page: None,
        },
        read_scratch: AlignedBlock([0; BLOCK_SIZE]),
        write_buffer,
        buffered_pages: 0,
        footer_scratch: AlignedBlock([0; BLOCK_SIZE]),
        release_iovecs: ReleaseIovecs(Vec::with_capacity(crate::MAXIMUM_PAYLOAD_BLOCKS + 1)),
        release_stats: ReleaseStats::default(),
        checkpoint_after_bytes: options.checkpoint_after_bytes,
        live_blocks,
        reserve,
        budget,
        actual_backing_blocks,
        reclaim,
        failed: false,
    })
}

/// Fixed per-volume scratch: staging batch, read and footer blocks, read-ahead, iovecs and the
/// recovery touched-page bitmap.
fn fixed_scratch_bytes(geometry: Geometry, buffer_blocks: usize) -> u64 {
    let batch = storage::BATCH_BLOCKS as u64 * BLOCK_SIZE as u64;
    let read_ahead = super::page_read::READ_AHEAD_PAGES as u64 * BLOCK_SIZE as u64;
    let iovecs = (crate::MAXIMUM_PAYLOAD_BLOCKS as u64 + 1) * size_of::<libc::iovec>() as u64;
    let touched = u64::from(geometry.page_count).div_ceil(64) * 8;
    let buffer_lbas = buffer_blocks as u64 * size_of::<u32>() as u64;
    batch + 2 * BLOCK_SIZE as u64 + read_ahead + iovecs + touched + buffer_lbas
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
        self.log.geometry.volume_blocks
    }

    pub fn release_stats(&self) -> ReleaseStats {
        self.release_stats
    }

    pub fn heap_budget(&self) -> HeapBudget {
        self.budget
    }

    pub fn heap_options_summary(&self) -> String {
        self.budget.summary()
    }

    pub fn mapping_stats(&self) -> MappingStats {
        MappingStats {
            cache: self.cache_snapshot(),
            io: self.backing_io.mapping_io,
            index_bytes: self.log.index.allocated_bytes(),
            pressure_checkpoints: self.mapping.pressure_checkpoints,
            checkpoint_failures: self.mapping.checkpoint_failures,
            recovery_passes: self.mapping.recovery.tail_passes,
        }
    }

    pub fn space_usage(&self) -> io::Result<SpaceUsage> {
        let geometry = self.log.geometry;
        let metadata = self.backing.metadata()?;
        let segments = &self.log.segments;
        let mut used_blocks = 0;
        let mut metadata_blocks = 2_u64;
        for segment in 0..geometry.segment_count {
            if segments.state(segment) != super::segment_table::SegmentState::Free {
                used_blocks += u64::from(segments.used_blocks(segment));
            }
            metadata_blocks += u64::from(segments.metadata_blocks(segment));
        }
        let segment_bytes = u64::from(geometry.blocks_per_segment) * BLOCK_SIZE as u64;
        Ok(SpaceUsage {
            volume_bytes: u64::from(geometry.volume_blocks) * BLOCK_SIZE as u64,
            live_bytes: self.live_blocks * BLOCK_SIZE as u64,
            backing_bytes: metadata.len(),
            provisioned_bytes: geometry.backing_blocks * BLOCK_SIZE as u64,
            allocated_bytes: metadata.blocks() * 512,
            metadata_bytes: metadata_blocks * BLOCK_SIZE as u64,
            log_capacity_bytes: u64::from(geometry.segment_count) * segment_bytes,
            log_used_bytes: used_blocks * BLOCK_SIZE as u64,
            segment_bytes,
            segment_count: geometry.segment_count,
            free_segments: segments.free_count(),
            pending_free_segments: segments.pending_free_count(),
            reserve_segments: self.reserve.segments,
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
            &mut self.log,
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
        if payload_checksum(self.log.volume_id, lba, physical, &self.read_scratch.0) != checksum {
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
        if !self.log.has_dirty_state(&self.mapping.cache) {
            return self.flush();
        }
        let result = self
            .mapping
            .publish(&mut self.backing_io, &self.backing, &mut self.log);
        self.latch(result).map(|_| ())
    }

    pub(crate) fn check_operational(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("volume failed"));
        }
        Ok(())
    }

    pub(crate) fn latch<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Whether the volume can still serve requests.
    pub fn is_failed(&self) -> bool {
        self.failed
    }
}

impl Volume {
    /// Applies one mapping change and keeps live counts exact.
    pub(crate) fn remap(&mut self, old_physical: u32, new_physical: u32) -> io::Result<()> {
        if old_physical != 0 {
            self.log.segments.remove_live(old_physical)?;
        } else if new_physical != 0 {
            self.live_blocks += 1;
        }
        if new_physical != 0 {
            self.log.segments.add_live(new_physical)?;
        } else if old_physical != 0 {
            self.live_blocks -= 1;
        }
        Ok(())
    }
}
