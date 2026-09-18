use crate::mapping_cache::Snapshot;

/// Completed metadata I/O, including work performed before a later operation fails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MappingIo {
    /// Reads through checkpoint pointers, including rejected checkpoint candidates.
    pub(crate) page_reads: u64,
    pub(crate) page_writes: u64,
    /// Complete index blocks written, including padding but excluding descriptors.
    pub(crate) index_bytes_written: u64,
    /// All blocks read while scanning the tail, counted separately from page lookups.
    pub(crate) tail_blocks_read: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MappingStats {
    pub(crate) cache: Snapshot,
    pub(crate) io: MappingIo,
    pub(crate) index_bytes: usize,
    pub(crate) pressure_checkpoints: u64,
    pub(crate) checkpoint_failures: u64,
    pub(crate) recovery_passes: u64,
}

impl MappingStats {
    pub(crate) fn summary(self) -> String {
        let cache = self.cache;
        let counters = cache.counters;
        format!(
            "mapping_foreground_hits={} mapping_foreground_misses={} mapping_foreground_hit_rate={} mapping_recovery_hits={} mapping_recovery_misses={} mapping_recovery_hit_rate={} mapping_clean_evictions={} mapping_victim_scans={} mapping_checkpoint_required={} mapping_pressure_checkpoints={} mapping_checkpoint_failures={} mapping_page_reads={} mapping_page_writes={} mapping_index_bytes_written={} mapping_tail_blocks_read={} mapping_recovery_passes={} mapping_resident_slots={} mapping_dirty_slots={} mapping_slot_capacity={} mapping_cache_requested_bytes={} mapping_cache_allocated_bytes={} mapping_index_bytes={}",
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
        )
    }
}

fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |rate| format!("{rate:.6}"))
}
