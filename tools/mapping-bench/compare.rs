//! Same driver compiled against each release tag; only adapter.rs differs.
use block_storage::{Volume, VolumeOpenOptions, format, open_with_options};
use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

const BLOCK: usize = 4096;
const LOGICAL_BYTES: u64 = 4 << 30;
const PAGE_COUNT: u32 = 3277;
const READS: usize = 32768;

#[derive(Default)]
struct CacheStats {
    hits: u64,
    misses: u64,
    page_reads: u64,
    page_writes: u64,
    pressure: u64,
    cache_bytes: usize,
    index_bytes: usize,
    slots: usize,
}

include!("adapter.rs");

struct Backing(PathBuf);

impl Drop for Backing {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            eprintln!("backing cleanup failed: {error}");
        }
    }
}

fn payload(lba: u32) -> [u8; BLOCK] {
    let mut bytes = [0; BLOCK];
    let mut seed = 0x1234_5678_u32;
    for chunk in bytes.chunks_exact_mut(4) {
        seed = random(&mut seed);
        chunk.copy_from_slice(&seed.to_le_bytes());
    }
    bytes[..4].copy_from_slice(&lba.to_le_bytes());
    bytes
}

fn random(seed: &mut u32) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed
}

fn trace(workload: &str) -> io::Result<(Vec<u32>, Vec<u32>, usize)> {
    let mut seed = 0x6a09_e667;
    let (count, pages, flush_every) = match workload {
        "seq-read" => (READS, 0, 0),
        "hot-read" => (READS, 32, 0),
        "wide-read" => (READS, PAGE_COUNT, 0),
        "seq-write" => (16384, 0, 256),
        "wide-write" => (4096, PAGE_COUNT, 32),
        _ => return Err(io::Error::other("unknown workload")),
    };
    let addresses = (0..count)
        .map(|i| {
            if pages == 0 {
                (i % 16384) as u32
            } else {
                (random(&mut seed) % pages) * 320
            }
        })
        .collect();
    let warm = if pages == 0 {
        (0..16384).collect()
    } else {
        (0..pages).map(|page| page * 320).collect()
    };
    Ok((addresses, warm, flush_every))
}

fn rss() -> io::Result<u64> {
    let text = fs::read_to_string("/proc/self/smaps_rollup")?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("Rss:"))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| io::Error::other("missing RSS"))?;
    Ok(value.parse::<u64>().map_err(io::Error::other)? * 1024)
}

fn percentile(values: &mut [u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() * percent).div_ceil(100) - 1]
}

fn populate(path: &Path, addresses: &[u32]) -> io::Result<()> {
    let mut volume = open_with_options(path, options(Some(16 << 20)))?;
    for &lba in addresses {
        volume.write_block(lba, &payload(lba))?;
    }
    volume.close()
}

fn read_checked(volume: &mut Volume, lba: u32, bytes: &mut [u8; BLOCK]) -> io::Result<()> {
    volume.read_block(lba, bytes)?;
    if bytes[..4] != lba.to_le_bytes() || bytes[4..8] == [0; 4] {
        return Err(io::Error::other("read verification failed"));
    }
    Ok(())
}

fn measure(
    volume: &mut Volume,
    addresses: &[u32],
    flush_every: usize,
) -> io::Result<(u128, Vec<u64>, Vec<u64>)> {
    let mut bytes = payload(0);
    let mut latencies = Vec::with_capacity(addresses.len());
    let mut flushes = Vec::with_capacity(addresses.len().checked_div(flush_every).unwrap_or(0));
    let start = Instant::now();
    for (i, &lba) in addresses.iter().enumerate() {
        let operation = Instant::now();
        if flush_every == 0 {
            read_checked(volume, lba, &mut bytes)?;
        } else {
            bytes[..4].copy_from_slice(&lba.to_le_bytes());
            volume.write_block(lba, &bytes)?;
        }
        latencies.push(operation.elapsed().as_nanos() as u64);
        if flush_every != 0 && (i + 1).is_multiple_of(flush_every) {
            let flush = Instant::now();
            volume.flush()?;
            flushes.push(flush.elapsed().as_nanos() as u64);
        }
    }
    Ok((start.elapsed().as_nanos(), latencies, flushes))
}

fn run(root: &Path, workload: &str, budget: Option<u64>) -> io::Result<()> {
    let (addresses, warm, flush_every) = trace(workload)?;
    let path = root.join(format!("backing-{}.img", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let backing = Backing(path);
    file.set_len(1 << 30)?;
    drop(file);
    format(&backing.0, LOGICAL_BYTES)?;
    if flush_every == 0 {
        populate(&backing.0, &warm)?;
    }
    let start = Instant::now();
    let mut volume = open_with_options(&backing.0, options(budget))?;
    let open_ns = start.elapsed().as_nanos();
    let mut bytes = payload(0);
    if flush_every == 0 {
        for &lba in &warm {
            read_checked(&mut volume, lba, &mut bytes)?;
        }
    }
    let initial = stats(&volume);
    let rss_bytes = rss()?;
    let (elapsed_ns, mut latencies, mut flushes) = measure(&mut volume, &addresses, flush_every)?;
    let final_stats = stats(&volume);
    let releases = volume.release_stats();
    let close = Instant::now();
    volume.close()?;
    let close_ns = close.elapsed().as_nanos();
    let allocated = fs::metadata(&backing.0)?.blocks() * 512;
    assert!(
        allocated < (512 << 20),
        "fixture must stay far below backing capacity"
    );
    if flush_every != 0 {
        let mut reopened = open_with_options(&backing.0, options(budget))?;
        for &lba in &addresses {
            read_checked(&mut reopened, lba, &mut bytes)?;
        }
        reopened.close()?;
    }
    println!(
        "workload={workload} operations={} elapsed_ns={elapsed_ns} op_p50_ns={} op_p99_ns={} flush_p99_ns={} flushes={} open_ns={open_ns} close_ns={close_ns} rss_bytes={rss_bytes} allocated_bytes={allocated} hits={} misses={} page_reads={} page_writes={} pressure={} cache_bytes={} index_bytes={} slots={} records={} payloads={}",
        addresses.len(),
        percentile(&mut latencies, 50),
        percentile(&mut latencies, 99),
        percentile(&mut flushes, 99),
        flushes.len(),
        final_stats.hits - initial.hits,
        final_stats.misses - initial.misses,
        final_stats.page_reads - initial.page_reads,
        final_stats.page_writes - initial.page_writes,
        final_stats.pressure - initial.pressure,
        final_stats.cache_bytes,
        final_stats.index_bytes,
        final_stats.slots,
        releases.records,
        releases.payloads,
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).take(4).collect();
    let [root, workload, budget] = args.as_slice() else {
        return Err(io::Error::other(
            "usage: compare FIXTURE_DIR WORKLOAD default|CACHE_BYTES",
        ));
    };
    let budget = if budget == "default" {
        None
    } else {
        Some(budget.parse::<u64>().map_err(io::Error::other)?)
    };
    run(Path::new(root), workload, budget)
}
