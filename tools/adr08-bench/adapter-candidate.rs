// Candidate adapter: the ADR-08 engine (format 4). Maintenance ticks follow the daemon's
// scheduler: run when the maximum interval has passed, for at most one idle wait.
use block_storage::{
    DEFAULT_IDLE_WAIT, DEFAULT_MAXIMUM_INTERVAL, HeapBudgetOptions, Scheduler, SelectionMode,
    Volume, VolumeOpenOptions,
};

const VERSION: &str = "candidate-adr08-format4";

fn format_fixture(path: &Path, logical_bytes: u64, _backing_bytes: u64) -> io::Result<()> {
    block_storage::format(path, logical_bytes)
}

fn open_volume(path: &Path, heap_total_bytes: Option<u64>) -> io::Result<Volume> {
    block_storage::open_with_options(
        path,
        VolumeOpenOptions {
            write_buffer_bytes: 4 << 20,
            checkpoint_after_bytes: 64 << 20,
            cache_budget_bytes: None,
            heap: HeapBudgetOptions {
                total_bytes: heap_total_bytes,
                ..HeapBudgetOptions::default()
            },
            ..VolumeOpenOptions::default()
        },
    )
}

struct Maintenance {
    scheduler: Scheduler,
}

impl Maintenance {
    fn new() -> Self {
        Self {
            scheduler: Scheduler::new(Instant::now(), DEFAULT_IDLE_WAIT, DEFAULT_MAXIMUM_INTERVAL),
        }
    }

    /// A driver never idles, so only the maximum interval triggers ticks, as in the daemon
    /// under continuous load.
    fn tick(&mut self, volume: &mut Volume, now: Instant) -> io::Result<Option<Duration>> {
        self.scheduler.request_completed(now);
        if !self.scheduler.maintenance_due(now) {
            return Ok(None);
        }
        let started = Instant::now();
        match volume.maintain_for(SelectionMode::Idle, DEFAULT_IDLE_WAIT) {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {}
            Err(error) => return Err(error),
        }
        self.scheduler.maintained(now);
        Ok(Some(started.elapsed()))
    }
}

fn extra_stats(volume: &Volume) -> String {
    let mapping = volume.mapping_stats();
    let releases = volume.release_stats();
    let reclaim = volume.reclaim_stats();
    let heap = volume.heap_budget();
    let writes = volume.log_writes();
    let space = volume.space_usage().ok();
    format!(
        "stat_cache_bytes={} stat_index_bytes={} stat_page_reads={} stat_page_writes={} stat_pressure_checkpoints={} stat_records={} stat_payloads={} stat_log_used_bytes={} stat_log_capacity_bytes={} stat_live_bytes={} stat_heap_total_bytes={} stat_heap_allocated_bytes={} stat_heap_cleaner_bytes={} stat_heap_cache_bytes={} stat_reclaim_payloads_copied={} stat_reclaim_segments_freed={} stat_reclaim_victims_selected={} stat_reclaim_pressure_steps={} stat_reclaim_pressure_waits={} stat_reclaim_enospc_all_live={} stat_reclaim_barrier_publications={} stat_reclaim_windows_scanned={} stat_reclaim_discards_issued={} stat_log_relocation_payload_blocks={} stat_log_user_payload_blocks={} stat_log_metadata_blocks={} stat_free_segments={} stat_segment_count={}",
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
        heap.total_bytes,
        heap.allocated_bytes(),
        heap.cleaner_bytes,
        heap.cache_bytes,
        reclaim.payloads_copied,
        reclaim.segments_freed,
        reclaim.victims_selected,
        reclaim.pressure_steps,
        reclaim.pressure_waits,
        reclaim.enospc_all_live,
        reclaim.barrier_publications,
        reclaim.windows_scanned,
        reclaim.discards_issued,
        writes.relocation_payload_blocks,
        writes.user_payload_blocks,
        writes.metadata_blocks,
        space.as_ref().map_or(0, |s| s.free_segments),
        space.as_ref().map_or(0, |s| s.segment_count),
    )
}
