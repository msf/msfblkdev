//! ADR-08 performance driver. The same file is compiled against the pre-ADR-08 baseline
//! (`57d56cc`, format 3) and the candidate (format 4); only `adapter.rs` differs. Every run
//! prints one line of `key=value` fields that `run.py` records as JSON.
use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const BLOCK: usize = 4096;

include!("adapter.rs");

struct Backing(PathBuf);

impl Drop for Backing {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            eprintln!("backing cleanup failed: {error}");
        }
    }
}

struct Random(u32);

impl Random {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

fn payload(lba: u32, generation: u32) -> [u8; BLOCK] {
    let mut bytes = [0; BLOCK];
    for (index, chunk) in bytes.chunks_exact_mut(8).enumerate() {
        let word = (u64::from(lba) << 32) ^ (u64::from(generation) << 8) ^ index as u64;
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// Names the stage an error came from, so a failed run says where it failed.
fn stage<T>(name: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| io::Error::new(error.kind(), format!("{name}: {error}")))
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

/// The `permille`-th per-mille value (500 = median, 990 = p99, 999 = p99.9).
fn percentile(values: &mut [u64], permille: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() * permille).div_ceil(1000).clamp(1, values.len()) - 1]
}

fn percentiles(prefix: &str, values: &mut [u64]) -> String {
    format!(
        "{prefix}_p50_ns={} {prefix}_p99_ns={} {prefix}_p999_ns={} {prefix}_max_ns={}",
        percentile(values, 500),
        percentile(values, 990),
        percentile(values, 999),
        values.iter().copied().max().unwrap_or(0)
    )
}

struct Parameters {
    logical_bytes: u64,
    backing_bytes: u64,
    /// Blocks written (or read) by the fresh workloads.
    operations: u64,
    /// Distinct blocks kept live by the aged workloads.
    working_set_blocks: u64,
    /// Overwrite volume as a multiple of the log capacity (backing bytes).
    capacity_multiple: u64,
    flush_every: u64,
    heap_total_bytes: Option<u64>,
    /// Whether the driver runs scheduled maintenance ticks like the daemon does.
    ticks: bool,
}

fn parse(args: &[String]) -> io::Result<(PathBuf, String, Parameters)> {
    let [root, workload, rest @ ..] = args else {
        return Err(io::Error::other(
            "usage: adr08_bench FIXTURE_DIR WORKLOAD key=value...",
        ));
    };
    let mut parameters = Parameters {
        logical_bytes: 128 << 20,
        backing_bytes: 512 << 20,
        operations: 16384,
        working_set_blocks: 16384,
        capacity_multiple: 2,
        flush_every: 256,
        heap_total_bytes: None,
        ticks: true,
    };
    for pair in rest {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| io::Error::other(format!("bad parameter {pair}")))?;
        let number = || value.parse::<u64>().map_err(io::Error::other);
        match key {
            "logical" => parameters.logical_bytes = number()?,
            "backing" => parameters.backing_bytes = number()?,
            "operations" => parameters.operations = number()?,
            "working_set" => parameters.working_set_blocks = number()?,
            "capacity_multiple" => parameters.capacity_multiple = number()?,
            "flush_every" => parameters.flush_every = number()?,
            "heap_total" => parameters.heap_total_bytes = Some(number()?),
            "ticks" => parameters.ticks = value == "1",
            _ => return Err(io::Error::other(format!("unknown parameter {key}"))),
        }
    }
    Ok((PathBuf::from(root), workload.clone(), parameters))
}

fn create_backing(root: &Path, parameters: &Parameters) -> io::Result<Backing> {
    let path = root.join(format!("backing-{}.img", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let backing = Backing(path);
    stage("set_len", file.set_len(parameters.backing_bytes))?;
    drop(file);
    stage(
        "format",
        format_fixture(&backing.0, parameters.logical_bytes, parameters.backing_bytes),
    )?;
    Ok(backing)
}

struct Sample {
    latencies: Vec<u64>,
    flushes: Vec<u64>,
    /// Ticks the scheduler ran and how long they took.
    ticks: Vec<u64>,
    enospc: u64,
    other_errors: u64,
    completed: u64,
}

impl Sample {
    fn new(capacity: usize) -> Self {
        Self {
            latencies: Vec::with_capacity(capacity),
            flushes: Vec::new(),
            ticks: Vec::new(),
            enospc: 0,
            other_errors: 0,
            completed: 0,
        }
    }
}

/// Runs `operations` writes (or reads when `read` is set) from `addresses`, flushing every
/// `flush_every` writes, and lets the adapter run scheduled maintenance between requests.
fn drive(
    volume: &mut Volume,
    addresses: impl Iterator<Item = u32>,
    read: bool,
    generation: u32,
    parameters: &Parameters,
    maintenance: &mut Maintenance,
    sample: &mut Sample,
) -> io::Result<()> {
    let mut bytes = [0; BLOCK];
    let mut since_flush = 0;
    for (index, lba) in addresses.enumerate() {
        let started = Instant::now();
        let result = if read {
            volume.read_block(lba, &mut bytes).and_then(|()| {
                if bytes != payload(lba, generation) && bytes != [0; BLOCK] {
                    return Err(io::Error::other("verification failed"));
                }
                Ok(())
            })
        } else {
            volume.write_block(lba, &payload(lba, generation))
        };
        sample.latencies.push(started.elapsed().as_nanos() as u64);
        match result {
            Ok(()) => sample.completed += 1,
            Err(error) if error.raw_os_error() == Some(libc::ENOSPC) => {
                sample.enospc += 1;
                if sample.enospc >= 16 {
                    // The baseline cannot free space; stop instead of failing every write.
                    return Ok(());
                }
            }
            Err(error) => {
                sample.other_errors += 1;
                return Err(io::Error::new(
                    error.kind(),
                    format!("operation {index} on lba {lba}: {error}"),
                ));
            }
        }
        since_flush += 1;
        if !read && since_flush == parameters.flush_every {
            since_flush = 0;
            let flush = Instant::now();
            stage("flush", volume.flush())?;
            sample.flushes.push(flush.elapsed().as_nanos() as u64);
        }
        if parameters.ticks && index % 16 == 15 {
            let now = Instant::now();
            if let Some(elapsed) = stage("tick", maintenance.tick(volume, now))? {
                sample.ticks.push(elapsed.as_nanos() as u64);
            }
        }
    }
    Ok(())
}

fn report(prefix: &str, sample: &mut Sample, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64().max(1e-9);
    format!(
        "{prefix}_completed={} {prefix}_enospc={} {prefix}_elapsed_ns={} {prefix}_ops_per_s={:.0} {} {prefix}_flushes={} {} {prefix}_ticks={} {}",
        sample.completed,
        sample.enospc,
        elapsed.as_nanos(),
        sample.completed as f64 / seconds,
        percentiles(&format!("{prefix}_op"), &mut sample.latencies),
        sample.flushes.len(),
        percentiles(&format!("{prefix}_flush"), &mut sample.flushes),
        sample.ticks.len(),
        percentiles(&format!("{prefix}_tick"), &mut sample.ticks),
    )
}

fn run(root: &Path, workload: &str, parameters: &Parameters) -> io::Result<String> {
    let volume_blocks = (parameters.logical_bytes / BLOCK as u64) as u32;
    let backing = create_backing(root, parameters)?;
    let mut maintenance = Maintenance::new();
    let mut fields = Vec::new();
    match workload {
        "fresh-seq-write" | "fresh-rand-write" | "fresh-seq-read" | "fresh-rand-read" => {
            let random = workload.contains("rand");
            let read = workload.ends_with("read");
            let count = parameters.operations;
            let addresses: Vec<u32> = if random {
                let mut generator = Random(0x9e37_79b9);
                (0..count).map(|_| generator.next() % volume_blocks).collect()
            } else {
                (0..count).map(|index| (index % u64::from(volume_blocks)) as u32).collect()
            };
            let mut volume = stage("open", open_volume(&backing.0, parameters.heap_total_bytes))?;
            if read {
                let mut populate = Sample::new(addresses.len());
                drive(
                    &mut volume,
                    addresses.iter().copied(),
                    false,
                    1,
                    parameters,
                    &mut maintenance,
                    &mut populate,
                )?;
                volume.flush()?;
                volume.close()?;
                volume = stage("open", open_volume(&backing.0, parameters.heap_total_bytes))?;
            }
            let mut sample = Sample::new(addresses.len());
            let started = Instant::now();
            drive(
                &mut volume,
                addresses.iter().copied(),
                read,
                1,
                parameters,
                &mut maintenance,
                &mut sample,
            )?;
            if !read {
                volume.flush()?;
            }
            fields.push(report("main", &mut sample, started.elapsed()));
            fields.push(format!("rss_bytes={}", rss()?));
            fields.push(extra_stats(&volume));
            volume.close()?;
        }
        "aged-overwrite" => {
            // Fill the working set, then overwrite random blocks inside it until
            // `capacity_multiple` times the log capacity has been written.
            let working_set = parameters.working_set_blocks.min(u64::from(volume_blocks)) as u32;
            let mut volume = stage("open", open_volume(&backing.0, parameters.heap_total_bytes))?;
            let mut fill = Sample::new(working_set as usize);
            let fill_started = Instant::now();
            drive(
                &mut volume,
                0..working_set,
                false,
                1,
                parameters,
                &mut maintenance,
                &mut fill,
            )?;
            stage("fill flush", volume.flush())?;
            fields.push(report("fill", &mut fill, fill_started.elapsed()));
            let overwrites = parameters.capacity_multiple * parameters.backing_bytes / BLOCK as u64;
            let mut generator = Random(0x2545_f491);
            let addresses = (0..overwrites).map(move |_| generator.next() % working_set);
            let mut sample = Sample::new(overwrites as usize);
            let started = Instant::now();
            drive(
                &mut volume,
                addresses,
                false,
                2,
                parameters,
                &mut maintenance,
                &mut sample,
            )?;
            if sample.enospc == 0 {
                stage("final flush", volume.flush())?;
            }
            fields.push(report("main", &mut sample, started.elapsed()));
            fields.push(format!(
                "working_set_blocks={working_set} overwrites_requested={overwrites} rss_bytes={}",
                rss()?
            ));
            fields.push(extra_stats(&volume));
            fields.push(format!("allocated_bytes={}", fs::metadata(&backing.0)?.blocks() * 512));
            let close = Instant::now();
            let closed = volume.close();
            fields.push(format!("close_ns={} close_ok={}", close.elapsed().as_nanos(), closed.is_ok()));
            // Restart after aging: open time and memory with the same budget.
            let reopen = Instant::now();
            let reopened = stage("open", open_volume(&backing.0, parameters.heap_total_bytes))?;
            fields.push(format!("reopen_ns={} reopen_rss_bytes={}", reopen.elapsed().as_nanos(), rss()?));
            fields.push(extra_stats(&reopened).replace("stat_", "reopen_stat_"));
            reopened.close()?;
        }
        "memory-restart" => {
            // Open a fresh volume of `logical` bytes under the requested budget, write the
            // working set with flushes, close, and measure the restart.
            let before = rss()?;
            let open = Instant::now();
            let mut volume = stage("open", open_volume(&backing.0, parameters.heap_total_bytes))?;
            let open_ns = open.elapsed().as_nanos();
            let after_open = rss()?;
            let working_set = parameters.working_set_blocks.min(u64::from(volume_blocks)) as u32;
            let mut sample = Sample::new(working_set as usize);
            drive(
                &mut volume,
                0..working_set,
                false,
                1,
                parameters,
                &mut maintenance,
                &mut sample,
            )?;
            volume.flush()?;
            let peak = rss()?;
            fields.push(extra_stats(&volume));
            volume.close()?;
            let reopen = Instant::now();
            let reopened = stage("open", open_volume(&backing.0, parameters.heap_total_bytes))?;
            let reopen_ns = reopen.elapsed().as_nanos();
            let reopen_rss = rss()?;
            fields.push(format!(
                "rss_before_open_bytes={before} open_ns={open_ns} rss_after_open_bytes={after_open} rss_after_writes_bytes={peak} reopen_ns={reopen_ns} reopen_rss_bytes={reopen_rss} written_blocks={}",
                sample.completed
            ));
            reopened.close()?;
        }
        _ => return Err(io::Error::other(format!("unknown workload {workload}"))),
    }
    Ok(format!(
        "version={} workload={workload} logical_bytes={} backing_bytes={} flush_every={} heap_total_bytes={} ticks={} {}",
        VERSION,
        parameters.logical_bytes,
        parameters.backing_bytes,
        parameters.flush_every,
        parameters.heap_total_bytes.unwrap_or(0),
        u8::from(parameters.ticks),
        fields.join(" ")
    ))
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (root, workload, parameters) = parse(&args)?;
    let line = run(&root, &workload, &parameters)?;
    println!("{line}");
    Ok(())
}
