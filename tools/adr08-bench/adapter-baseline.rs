// Baseline adapter: the pre-ADR-08 engine at 57d56cc (format 3). No reclamation exists, so
// maintenance ticks are no-ops and the overwrite workloads end in ENOSPC once the log fills.
use block_storage::{Volume, VolumeOpenOptions};

const VERSION: &str = "baseline-57d56cc-format3";

fn format_fixture(path: &Path, logical_bytes: u64, _backing_bytes: u64) -> io::Result<()> {
    block_storage::format(path, logical_bytes)
}

fn open_volume(path: &Path, heap_total_bytes: Option<u64>) -> io::Result<Volume> {
    // Format 3 has no total heap budget; the cache budget is the only bounded share. Give it
    // the requested total minus the write buffer so the comparison is not unfair to it.
    let cache_budget_bytes = heap_total_bytes.map(|total| total.saturating_sub(4 << 20).max(1 << 20));
    block_storage::open_with_options(
        path,
        VolumeOpenOptions {
            write_buffer_bytes: 4 << 20,
            checkpoint_after_bytes: 64 << 20,
            cache_budget_bytes,
        },
    )
}

struct Maintenance;

impl Maintenance {
    fn new() -> Self {
        Self
    }

    fn tick(&mut self, _volume: &mut Volume, _now: Instant) -> io::Result<Option<Duration>> {
        Ok(None)
    }
}

fn extra_stats(volume: &Volume) -> String {
    let mapping = volume.mapping_stats();
    let releases = volume.release_stats();
    let space = volume.space_usage().ok();
    format!(
        "stat_cache_bytes={} stat_index_bytes={} stat_page_reads={} stat_page_writes={} stat_pressure_checkpoints={} stat_records={} stat_payloads={} stat_log_used_bytes={} stat_log_capacity_bytes={} stat_live_bytes={}",
        mapping.cache.sizing.allocated_bytes,
        mapping.index_bytes,
        mapping.io.page_reads,
        mapping.io.page_writes,
        mapping.pressure_checkpoints,
        releases.records,
        releases.payloads,
        space.as_ref().map_or(0, |s| s.log_used_bytes),
        space.as_ref().map_or(0, |s| s.log_capacity_bytes),
        space.as_ref().map_or(0, |s| s.live_bytes),
    )
}
