fn options(budget: Option<u64>) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: 256 << 10,
        checkpoint_after_bytes: 64 << 20,
        cache_budget_bytes: budget,
    }
}

fn stats(volume: &Volume) -> CacheStats {
    let value = volume.mapping_stats();
    CacheStats {
        hits: value.cache.counters.foreground.hits,
        misses: value.cache.counters.foreground.misses,
        page_reads: value.io.page_reads,
        page_writes: value.io.page_writes,
        pressure: value.pressure_checkpoints,
        cache_bytes: value.cache.sizing.allocated_bytes,
        index_bytes: value.index_bytes,
        slots: value.cache.sizing.capacity,
        read_ops: value.io.demand_page_read_ops,
        prefetched: value.cache.counters.prefetched_pages,
        useful: value.cache.counters.prefetch_hits,
        unused: value.cache.counters.prefetch_unused_evictions,
        rejected: value.io.prefetch_rejected,
    }
}
