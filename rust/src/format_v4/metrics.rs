use crate::mapping_cache::Snapshot;

/// Completed metadata I/O, including work performed before a later operation fails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MappingIo {
    /// Reads through checkpoint pointers, including rejected checkpoint candidates.
    pub page_reads: u64,
    /// Foreground mapping read submissions, including adjacent speculative pages.
    pub demand_page_read_ops: u64,
    pub prefetch_rejected: u64,
    pub page_writes: u64,
    /// Index and table page blocks written, excluding descriptors.
    pub index_bytes_written: u64,
    /// All blocks read while scanning the tail, counted separately from page lookups.
    pub tail_blocks_read: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappingStats {
    pub cache: Snapshot,
    pub io: MappingIo,
    pub index_bytes: usize,
    pub pressure_checkpoints: u64,
    pub checkpoint_failures: u64,
    pub recovery_passes: u64,
}

impl MappingStats {
    pub fn summary(self) -> String {
        let cache = self.cache;
        let counters = cache.counters;
        format!(
            "mapping_foreground_hits={} mapping_foreground_misses={} mapping_foreground_hit_rate={} mapping_recovery_hits={} mapping_recovery_misses={} mapping_recovery_hit_rate={} mapping_clean_evictions={} mapping_victim_scans={} mapping_checkpoint_required={} mapping_pressure_checkpoints={} mapping_checkpoint_failures={} mapping_page_reads={} mapping_page_writes={} mapping_index_bytes_written={} mapping_tail_blocks_read={} mapping_recovery_passes={} mapping_resident_slots={} mapping_dirty_slots={} mapping_slot_capacity={} mapping_cache_requested_bytes={} mapping_cache_allocated_bytes={} mapping_index_bytes={} mapping_demand_read_ops={} mapping_prefetched_pages={} mapping_prefetch_hits={} mapping_prefetch_unused_evictions={} mapping_prefetch_rejected={}",
            counters.foreground.hits,
            counters.foreground.misses,
            rate(counters.foreground.hit_rate()),
            counters.recovery.hits,
            counters.recovery.misses,
            rate(counters.recovery.hit_rate()),
            counters.clean_evictions,
            counters.victim_slots_examined,
            counters.checkpoint_required,
            self.pressure_checkpoints,
            self.checkpoint_failures,
            self.io.page_reads,
            self.io.page_writes,
            self.io.index_bytes_written,
            self.io.tail_blocks_read,
            self.recovery_passes,
            cache.resident_slots,
            cache.dirty_slots,
            cache.sizing.capacity,
            cache.sizing.requested_bytes,
            cache.sizing.allocated_bytes,
            self.index_bytes,
            self.io.demand_page_read_ops,
            counters.prefetched_pages,
            counters.prefetch_hits,
            counters.prefetch_unused_evictions,
            self.io.prefetch_rejected,
        )
    }
}

fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |rate| format!("{rate:.6}"))
}

/// Physical and logical space accounting for one volume.
///
/// The backing file is sparse, so its declared length is a ceiling rather than a cost.
/// `allocated_bytes` is what the host filesystem has really given the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceUsage {
    /// Addressable logical capacity presented to the filesystem.
    pub volume_bytes: u64,
    /// Logical bytes that hold data, meaning blocks with a mapping.
    pub live_bytes: u64,
    /// Declared backing length. Sparse, so mostly unallocated.
    pub backing_bytes: u64,
    /// The provisioned bound the engine may use; grows through `BackingGrowth`.
    pub provisioned_bytes: u64,
    /// Backing bytes the host filesystem has actually allocated.
    pub allocated_bytes: u64,
    /// Both descriptors plus every current snapshot, index page and table page.
    pub metadata_bytes: u64,
    /// Total segment capacity.
    pub log_capacity_bytes: u64,
    /// Blocks written in segments that are not free.
    pub log_used_bytes: u64,
    pub segment_bytes: u64,
    pub segment_count: u32,
    pub free_segments: u32,
    pub pending_free_segments: u32,
    /// Free segments foreground writes may not consume.
    pub reserve_segments: u32,
}

impl SpaceUsage {
    /// Segment bytes not yet written in non-free segments plus free segments.
    #[must_use]
    pub const fn log_free_bytes(&self) -> u64 {
        self.log_capacity_bytes.saturating_sub(self.log_used_bytes)
    }

    /// Allocated backing bytes per live logical byte. Zero when nothing is live.
    #[must_use]
    pub fn amplification(&self) -> f64 {
        if self.live_bytes == 0 {
            return 0.0;
        }
        self.allocated_bytes as f64 / self.live_bytes as f64
    }

    /// Live bytes divided by segment capacity.
    #[must_use]
    pub fn fill_factor(&self) -> f64 {
        if self.log_capacity_bytes == 0 {
            return 0.0;
        }
        self.live_bytes as f64 / self.log_capacity_bytes as f64
    }

    /// Dead bytes divided by used bytes, where dead means written but neither live nor metadata.
    #[must_use]
    pub fn garbage_ratio(&self) -> f64 {
        if self.log_used_bytes == 0 {
            return 0.0;
        }
        self.log_used_bytes
            .saturating_sub(self.live_bytes)
            .saturating_sub(self.metadata_bytes) as f64
            / self.log_used_bytes as f64
    }

    /// A single-line summary suitable for emission outside timed work.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "volume_mib={:.1} live_mib={:.1} backing_declared_mib={:.1} backing_provisioned_mib={:.1} backing_allocated_mib={:.1} metadata_mib={:.1} log_used_mib={:.1} log_free_mib={:.1} log_capacity_mib={:.1} log_used_percent={:.1} allocated_per_live={:.3} fill_factor={:.3} garbage_ratio={:.3} segment_mib={:.1} segments={} free_segments={} pending_free_segments={} reserve_segments={}",
            mib(self.volume_bytes),
            mib(self.live_bytes),
            mib(self.backing_bytes),
            mib(self.provisioned_bytes),
            mib(self.allocated_bytes),
            mib(self.metadata_bytes),
            mib(self.log_used_bytes),
            mib(self.log_free_bytes()),
            mib(self.log_capacity_bytes),
            if self.log_capacity_bytes == 0 {
                0.0
            } else {
                100.0 * self.log_used_bytes as f64 / self.log_capacity_bytes as f64
            },
            self.amplification(),
            self.fill_factor(),
            self.garbage_ratio(),
            mib(self.segment_bytes),
            self.segment_count,
            self.free_segments,
            self.pending_free_segments,
            self.reserve_segments,
        )
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
