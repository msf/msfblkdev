//! Client for tools/adr05-fault-lab/traces/run.py. No failpoints or engine internals.
use block_storage::{
    FormatOptions, Volume, VolumeOpenOptions, format_with_options, open_live_with_options,
};
use std::{env, fs, io, path::Path, time::Duration};

const BLOCK: usize = 4096;
const LBAS: u32 = 680; // Three mapping pages, even though the write set is small.
const SEGMENT: u64 = 32 * BLOCK as u64;
const INITIAL: u64 = 2 * BLOCK as u64 + 32 * SEGMENT;
const GROWN: u64 = INITIAL + 4 * SEGMENT;

fn invalid() -> io::Error {
    io::Error::other("invalid adr05-traces client argument or operation")
}

fn number(s: &str) -> io::Result<u32> {
    s.parse().map_err(|_| invalid())
}

fn scan(volume: &mut Volume) -> io::Result<()> {
    let mut data = [0; BLOCK];
    let mut values = String::with_capacity(2 * LBAS as usize);
    for lba in 0..LBAS {
        volume.read_block(lba, &mut data)?;
        if !data.iter().all(|byte| *byte == data[0]) {
            return Err(io::Error::other(format!("nonuniform block at LBA {lba}")));
        }
        use std::fmt::Write;
        write!(values, "{:02x}", data[0]).expect("writing to String");
    }
    println!("state={values}");
    Ok(())
}

fn apply(volume: &mut Volume, path: &Path, op: &str) -> io::Result<()> {
    if let Some(write) = op.strip_prefix('w') {
        let (lba, value) = write.split_once(':').ok_or_else(invalid)?;
        let value = u8::try_from(number(value)?).map_err(|_| invalid())?;
        return volume.write_block(number(lba)?, &[value; BLOCK]);
    }
    if let Some(lba) = op.strip_prefix('d') {
        return volume.discard_blocks(number(lba)?, 1);
    }
    match op {
        "f" => volume.flush(),
        "g" => {
            fs::OpenOptions::new()
                .write(true)
                .open(path)?
                .set_len(GROWN)?;
            volume.grow_backing(GROWN)
        }
        "x" => match volume.write_block(LBAS, &[0x55; BLOCK]) {
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(()),
            _ => Err(io::Error::other(
                "out-of-range write not rejected as InvalidInput",
            )),
        },
        _ => Err(invalid()),
    }
}

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).take(69).collect();
    let [mode, profile, path, ops @ ..] = &args[..] else {
        return Err(invalid());
    };
    if ops.len() > 64 {
        return Err(invalid());
    }
    let path = Path::new(path);
    if mode == "format" {
        fs::File::create_new(path)?.set_len(INITIAL)?;
        return format_with_options(
            path,
            u64::from(LBAS) * BLOCK as u64,
            FormatOptions {
                blocks_per_segment: 32,
            },
        );
    }
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid());
    }
    let (slots, buffer, interval) = match profile.as_str() {
        "small" => (1, 1, 339),
        "wide" => (3, 8, 1024),
        _ => return Err(invalid()),
    };
    let options = VolumeOpenOptions {
        write_buffer_bytes: buffer * BLOCK as u64,
        cache_budget_bytes: Some(if slots == 1 { 4096 } else { 65536 }),
        checkpoint_after_bytes: interval * BLOCK as u64,
        ..VolumeOpenOptions::default()
    };
    let mut volume = open_live_with_options(path, options)?;
    let actual_slots = volume.mapping_stats().cache.sizing.capacity;
    if actual_slots != slots {
        return Err(io::Error::other(format!(
            "expected {slots} cache slots; engine allocated {actual_slots}"
        )));
    }
    println!(
        "open slots={slots} buffer_blocks={buffer} checkpoint_blocks={interval} provisioned={}",
        volume.space_usage()?.provisioned_bytes
    );
    if mode == "scan" {
        return scan(&mut volume);
    }
    if !matches!(mode.as_str(), "close" | "kill") {
        return Err(invalid());
    }
    for op in ops {
        apply(&mut volume, path, op)?;
        println!("ack={op}");
    }
    if mode == "close" {
        return volume.close();
    }
    if ops.last().map(String::as_str) != Some("f") {
        return Err(invalid());
    }
    fs::File::create_new(path.with_extension("ready"))?;
    // The owner must SIGKILL this child; never turn a missed cut into a clean exit.
    std::thread::sleep(Duration::from_secs(12));
    Err(io::Error::other("owner missed the post-flush kill point"))
}
