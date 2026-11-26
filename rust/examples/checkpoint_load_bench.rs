//! Times checkpoint load on a large volume (TODO-2).
//!
//! `fill` writes one block into every mapping page of a formatted backing, so the checkpoint
//! references every mapping page while the payload stays small. `time` then measures
//! `open_readonly` and `open_live` on it.
//!
//! ```sh
//! block-storage-ublk format /tmp/big.img --size-mib 512000 --prealloc-mib 30720 \
//!     --min-logical-fill-factor 5
//! checkpoint_load_bench fill /tmp/big.img
//! checkpoint_load_bench time /tmp/big.img 3
//! ```
use block_storage::{
    DEFAULT_FORMAT_WRITE_BUFFER_BYTES, VolumeOpenOptions, open_live_with_options, open_readonly,
};
use std::{env, io, path::Path, time::Instant};

/// Entries per mapping page (ADR-07).
const ENTRIES_PER_PAGE: u32 = 336;
const BLOCK: usize = 4096;
/// Flush this often so the write buffer releases full records.
const FLUSH_EVERY: u32 = 4096;

fn options() -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: DEFAULT_FORMAT_WRITE_BUFFER_BYTES,
        ..VolumeOpenOptions::default()
    }
}

fn fill(path: &Path) -> io::Result<()> {
    let started = Instant::now();
    let mut volume = open_live_with_options(path, options())?;
    let pages = volume.volume_blocks().div_ceil(ENTRIES_PER_PAGE);
    let mut block = [0_u8; BLOCK];
    for page in 0..pages {
        block[..4].copy_from_slice(&page.to_le_bytes());
        volume.write_block(page * ENTRIES_PER_PAGE, &block)?;
        if (page + 1).is_multiple_of(FLUSH_EVERY) {
            volume.flush()?;
        }
    }
    volume.close()?;
    println!(
        "filled {pages} mapping pages in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn time(path: &Path, repetitions: u32) -> io::Result<()> {
    for repetition in 1..=repetitions {
        let started = Instant::now();
        let view = open_readonly(path)?;
        let readonly = started.elapsed();
        let started = Instant::now();
        let volume = open_live_with_options(path, options())?;
        let live = started.elapsed();
        let reads = volume.mapping_stats().io.page_reads;
        drop(volume);
        println!(
            "repetition {repetition}: open_readonly {:.3} s, open_live {:.3} s, mapped {}, live page reads {reads}",
            readonly.as_secs_f64(),
            live.as_secs_f64(),
            view.mapped_blocks,
        );
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1..).unwrap_or_default() {
        [command, path] if command == "fill" => fill(Path::new(path)),
        [command, path, repetitions] if command == "time" => {
            let repetitions = repetitions
                .parse()
                .map_err(|_| io::Error::other("repetitions must be an integer"))?;
            time(Path::new(path), repetitions)
        }
        _ => Err(io::Error::other(
            "usage: checkpoint_load_bench fill <backing> | time <backing> <repetitions>",
        )),
    }
}
