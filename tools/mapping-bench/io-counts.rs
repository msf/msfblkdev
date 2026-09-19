//! Count backing operations for bounded writes and report final close separately.
use block_storage::{VolumeOpenOptions, format, io_accounting, open_with_accounting};
use std::{fs, io, path::Path, time::Instant};

include!("options.rs");

fn random(seed: &mut u32) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed
}

fn run(root: &Path, budget: Option<u64>, flush_every: usize, pages: u32) -> io::Result<()> {
    let backing = root.join("backing.img");
    let ledger = root.join("ledger");
    fs::File::create_new(&backing)?.set_len(1 << 30)?;
    format(&backing, 4 << 30)?;
    // SAFETY: the caller owns this private directory; only these handles access
    // the ledger, which remains intact until both handles are dropped.
    let reader = unsafe { io_accounting::Reader::create(&ledger, 512 << 20)? };
    // SAFETY: same inode lifetime and exclusive access contract as above.
    let writer = unsafe { io_accounting::Writer::open(&ledger)? };
    let mut volume = open_with_accounting(&backing, options(budget), writer)?;
    let initial = reader.snapshot()?;
    let mut seed = 0x6a09_e667;
    let mut payload = [0_u8; 4096];
    let mut data_seed = 0x1234_5678;
    for chunk in payload.chunks_exact_mut(4) {
        chunk.copy_from_slice(&random(&mut data_seed).to_le_bytes());
    }
    let start = Instant::now();
    for operation in 0..4096 {
        let lba = (random(&mut seed) % pages) * 320;
        payload[..4].copy_from_slice(&lba.to_le_bytes());
        volume.write_block(lba, &payload)?;
        if (operation + 1) % flush_every == 0 {
            volume.flush()?;
        }
    }
    let elapsed_ns = start.elapsed().as_nanos();
    let final_counts = reader.snapshot()?;
    let release = volume.release_stats();
    volume.close()?;
    let closed = reader.snapshot()?;
    assert!(!closed.completed_totals_incomplete);
    assert!(!closed.budget_exhausted);
    assert_eq!(closed.errors, 0);
    assert_eq!(closed.pending_operations, 0);
    println!(
        "flush_every={flush_every} pages={pages} elapsed_ns={elapsed_ns} reads={} writes={} flushes={} read_bytes={} write_bytes={} records={} payloads={} close_writes={} close_write_bytes={} close_flushes={}",
        final_counts.read_completed_operations - initial.read_completed_operations,
        final_counts.write_completed_operations - initial.write_completed_operations,
        final_counts.flush_completed_operations - initial.flush_completed_operations,
        final_counts.read_completed_bytes - initial.read_completed_bytes,
        final_counts.write_completed_bytes - initial.write_completed_bytes,
        release.records,
        release.payloads,
        closed.write_completed_operations - final_counts.write_completed_operations,
        closed.write_completed_bytes - final_counts.write_completed_bytes,
        closed.flush_completed_operations - final_counts.flush_completed_operations,
    );
    drop(reader);
    fs::remove_file(ledger)?;
    fs::remove_file(backing)
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).take(5).collect();
    let [root, budget, flush, pages] = args.as_slice() else {
        return Err(io::Error::other(
            "usage: io-counts DIR default|CACHE_BYTES FLUSH_EVERY PAGES",
        ));
    };
    let budget = if budget == "default" {
        None
    } else {
        Some(budget.parse::<u64>().map_err(io::Error::other)?)
    };
    let flush = flush.parse::<usize>().map_err(io::Error::other)?;
    let pages = pages.parse::<u32>().map_err(io::Error::other)?;
    if ![1, 32, 256, 1024].contains(&flush) || ![32, 3277].contains(&pages) {
        return Err(io::Error::other("unsupported bounded workload"));
    }
    run(Path::new(root), budget, flush, pages)
}
