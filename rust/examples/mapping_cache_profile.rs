//! Reproducible ADR-07 probes using only owned temporary regular files.
use block_storage::{Volume, VolumeOpenOptions, format, open, open_with_options};
use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

const BLOCK_BYTES: usize = 4096;
const ENTRIES_PER_PAGE: u32 = 320; // ADR-07 mapping-page geometry.
const ACCESS_COUNT: u32 = 4096;

struct Backing(PathBuf);

impl Backing {
    fn new(logical_bytes: u64) -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mapping-profile-{}-{nonce}", std::process::id()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let backing = Self(path);
        file.set_len(128 * 1024 * 1024)?;
        format(&backing.0, logical_bytes)?;
        Ok(backing)
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            eprintln!("temporary backing cleanup failed: {error}");
        }
    }
}

fn resident_memory() -> io::Result<u64> {
    let status = fs::read_to_string("/proc/self/smaps_rollup")?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Rss:"))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| io::Error::other("missing resident-memory field"))?;
    value
        .parse::<u64>()
        .map_err(io::Error::other)?
        .checked_mul(1024)
        .ok_or_else(|| io::Error::other("resident-memory size overflow"))
}

fn memory_probe(name: &str, logical_bytes: u64) -> io::Result<()> {
    let backing = Backing::new(logical_bytes)?;
    let rss_before = resident_memory()?;
    let mut volume = open(&backing.0)?;
    let stats = volume.mapping_stats();
    let rss_open = resident_memory()?;
    volume.write_block(0, &[1; BLOCK_BYTES])?;
    volume.close()?;
    let rss_after_close = resident_memory()?;
    let cache = stats.cache.sizing;
    println!(
        "case={name} logical_bytes={logical_bytes} requested_cache_bytes={} effective_cache_bytes={} cache_slots={} slot_bytes={} lookup_bytes={} policy_bytes={} index_bytes={} steady_mapping_heap_bytes={} checkpoint_mapping_heap_bound_bytes={} recovery_mapping_heap_bound_bytes={} volume_inline_bytes={} rss_before_open_bytes={rss_before} rss_open_bytes={rss_open} rss_after_close_bytes={rss_after_close}",
        cache.requested_bytes,
        cache.allocated_bytes,
        cache.capacity,
        cache.slot_bytes,
        cache.lookup_bytes,
        cache.policy_bytes,
        stats.index_bytes,
        cache.allocated_bytes + stats.index_bytes,
        cache.allocated_bytes + 2 * stats.index_bytes,
        cache.allocated_bytes + 2 * stats.index_bytes,
        size_of::<Volume>(),
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Workload {
    Sequential,
    Hot,
    Scattered,
    MovingHot,
}

impl Workload {
    fn lba(self, access: u32) -> u32 {
        match self {
            Self::Sequential => access,
            Self::Hot => (access % 3) * ENTRIES_PER_PAGE,
            Self::Scattered => ((access * 5) % 16) * ENTRIES_PER_PAGE,
            Self::MovingHot => ((access / 1024) * 3 + access % 3) * ENTRIES_PER_PAGE,
        }
    }

    fn expected_misses(self) -> u64 {
        match self {
            Self::Sequential => u64::from(ACCESS_COUNT.div_ceil(ENTRIES_PER_PAGE)),
            Self::Hot => 3,
            Self::Scattered => u64::from(ACCESS_COUNT),
            Self::MovingHot => 12,
        }
    }
}

fn access_probe() -> io::Result<()> {
    let backing = Backing::new(16 * u64::from(ENTRIES_PER_PAGE) * BLOCK_BYTES as u64)?;
    let options = VolumeOpenOptions {
        cache_budget_bytes: Some(16_000),
        ..VolumeOpenOptions::default()
    };
    let mut volume = open_with_options(&backing.0, options)?;
    for page in 0..16 {
        volume.write_block(page * ENTRIES_PER_PAGE, &[page as u8 + 1; BLOCK_BYTES])?;
    }
    volume.close()?;
    for workload in [
        Workload::Sequential,
        Workload::Hot,
        Workload::Scattered,
        Workload::MovingHot,
    ] {
        let mut volume = open_with_options(&backing.0, options)?;
        let initial = volume.mapping_stats();
        assert_eq!(
            initial.cache.sizing.capacity, 4,
            "profile requires four cache slots"
        );
        assert_eq!(
            initial.cache.resident_slots, 0,
            "start with a cold mapping cache"
        );
        let mut bytes = [0; BLOCK_BYTES];
        for access in 0..ACCESS_COUNT {
            let lba = workload.lba(access);
            volume.read_block(lba, &mut bytes)?;
            let expected = if lba.is_multiple_of(ENTRIES_PER_PAGE) {
                (lba / ENTRIES_PER_PAGE) as u8 + 1
            } else {
                0
            };
            if bytes != [expected; BLOCK_BYTES] {
                return Err(io::Error::other("profile payload mismatch"));
            }
        }
        let stats = volume.mapping_stats();
        let lookups = stats.cache.counters.foreground;
        let misses = workload.expected_misses();
        assert_eq!(lookups.misses, misses, "exact workload miss count");
        assert_eq!(
            lookups.hits,
            u64::from(ACCESS_COUNT) - misses,
            "exact workload hit count"
        );
        assert_eq!(
            stats.io.page_reads - initial.io.page_reads,
            misses,
            "each miss loads one snapshot"
        );
        assert_eq!(stats.io.page_writes, 0, "read-only clean workload");
        println!(
            "case={workload:?} accesses={ACCESS_COUNT} cache_slots=4 hits={} misses={} hit_rate={:.9} snapshot_reads={} clean_evictions={}",
            lookups.hits,
            lookups.misses,
            lookups.hit_rate().expect("nonempty workload"),
            stats.io.page_reads - initial.io.page_reads,
            stats.cache.counters.clean_evictions
        );
        volume.close()?;
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).take(2).collect();
    match args.as_slice() {
        [mode] if mode == "4gib" => memory_probe(mode, 4 << 30),
        [mode] if mode == "400gib" => memory_probe(mode, 400 << 30),
        [mode] if mode == "4tib" => memory_probe(mode, 4 << 40),
        [mode] if mode == "accesses" => access_probe(),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: mapping_cache_profile 4gib|400gib|4tib|accesses",
        )),
    }
}
