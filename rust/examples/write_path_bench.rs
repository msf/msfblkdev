//! Engine write-path benchmark for hyperfine: seeded random 4 KiB writes through
//! `Volume::write_block`, with a flush every `flush_every` writes, as fio's `fsync=N` does.
//!
//! `format` prepares a fresh backing; time only `write`:
//!
//! ```text
//! hyperfine --prepare 'rm -f B && write_path_bench format B 4096 6144' \
//!     'write_path_bench write B 262144 64 1'
//! ```
use block_storage::{format, open_live};
use std::{env, fs::OpenOptions, io, os::unix::fs::OpenOptionsExt, path::Path};

const MIB: u64 = 1 << 20;
const BLOCK_BYTES: usize = 4096; // The engine's fixed block size.

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, path, logical_mib, raw_mib] if command == "format" => {
            prepare(Path::new(path), number(logical_mib)?, number(raw_mib)?)
        }
        [command, path, writes, flush_every, seed] if command == "write" => write(
            Path::new(path),
            number(writes)?,
            number(flush_every)?,
            number(seed)?,
        ),
        _ => Err(io::Error::other(
            "usage: write_path_bench format <backing> <logical-mib> <raw-mib> | \
             write <backing> <writes> <flush-every> <seed>",
        )),
    }
}

fn number(text: &str) -> io::Result<u64> {
    text.parse()
        .map_err(|_| io::Error::other(format!("not a number: {text}")))
}

fn prepare(path: &Path, logical_mib: u64, raw_mib: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_len(raw_mib * MIB)?;
    format(path, logical_mib * MIB)
}

fn write(path: &Path, writes: u64, flush_every: u64, seed: u64) -> io::Result<()> {
    if flush_every == 0 {
        return Err(io::Error::other("flush-every must be positive"));
    }
    let mut volume = open_live(path)?;
    let blocks = u64::from(volume.volume_blocks());
    let mut state = seed | 1;
    let mut data = [0_u8; BLOCK_BYTES];
    for index in 1..=writes {
        state = xorshift64(state);
        let lba = u32::try_from(state % blocks).map_err(io::Error::other)?;
        data[..8].copy_from_slice(&index.to_le_bytes());
        volume.write_block(lba, &data)?;
        if index % flush_every == 0 {
            volume.flush()?;
        }
    }
    volume.flush()?;
    volume.close()
}

/// Marsaglia's xorshift64: a fixed, dependency-free sequence for a given seed.
const fn xorshift64(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^ (state << 17)
}
