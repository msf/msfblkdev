//! Capture one accounted format-4 append and read the modeled cuts through production recovery.
use block_storage::{
    FormatOptions, VolumeOpenOptions, format_with_options, io_accounting, open_live,
    open_live_with_accounting,
};
use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::DirBuilderExt,
    path::Path,
};

const BLOCK: usize = 4096;
const BLOCKS: u32 = 8;
const IMAGE_BYTES: u64 = (2 + 32 * 12) * 4096;

fn save_copy(source: &Path, target: &Path) -> io::Result<()> {
    let mut output = fs::File::create_new(target)?;
    let bytes = fs::read(source)?;
    if bytes.len() as u64 != IMAGE_BYTES {
        return Err(io::Error::other("unexpected fixture geometry"));
    }
    output.write_all(&bytes)
}

fn capture(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dir)?;
    let backing = dir.join("image");
    fs::File::create_new(&backing)?.set_len(IMAGE_BYTES)?;
    format_with_options(
        &backing,
        u64::from(BLOCKS) * BLOCK as u64,
        FormatOptions {
            blocks_per_segment: 32,
        },
    )?;
    let ledger = dir.join("ledger");
    // SAFETY: this example owns the fresh private directory. It never replaces, truncates,
    // unlinks or otherwise writes the ledger inode while these mappings exist.
    let reader = unsafe { io_accounting::Reader::create(&ledger, 4 * IMAGE_BYTES)? };
    // SAFETY: same inode and lifetime guarantee as the reader; only this handle records I/O.
    let writer = unsafe { io_accounting::Writer::open(&ledger)? };
    let mut v = open_live_with_accounting(&backing, VolumeOpenOptions::default(), writer)?;
    v.write_block(0, &[0x11; BLOCK])?;
    v.write_block(1, &[0x22; BLOCK])?;
    v.flush()?;
    save_copy(&backing, &dir.join("before"))?;
    let before = reader.snapshot()?;
    v.write_block(1, &[0x33; BLOCK])?;
    let after = reader.snapshot()?;
    println!("before={before:?}\nafter={after:?}");
    if before.flush_completed_operations == 0
        || after.pending_operations != 0
        || after.errors != 0
        || after.completed_totals_incomplete
        || after.write_completed_operations != before.write_completed_operations + 1
        || after.write_completed_bytes != before.write_completed_bytes + 2 * BLOCK as u64
        || after.read_completed_operations != before.read_completed_operations
        || after.flush_completed_operations != before.flush_completed_operations
    {
        return Err(io::Error::other(
            "capture wasn't a single unsynced 8192-byte append",
        ));
    }
    save_copy(&backing, &dir.join("after"))?;
    // No Volume::close: it would publish an unrelated checkpoint.
    Ok(())
}

fn check(path: &Path, case: &str) -> io::Result<()> {
    let mut v = open_live(path)?;
    for lba in 0..BLOCKS {
        let mut bytes = [0xca; BLOCK];
        let result = v.read_block(lba, &mut bytes);
        if case == "footer" && lba == 1 {
            if let Err(e) = result
                && e.kind() == io::ErrorKind::InvalidData
                && e.to_string() == "payload checksum mismatch"
                && bytes == [0xca; BLOCK]
            {
                println!("case=footer lba=1 refused={e:?} destination_unchanged=true");
                continue;
            }
            return Err(io::Error::other(
                "expected payload checksum refusal on LBA 1",
            ));
        }
        result?;
        let expected = match lba {
            0 => 0x11,
            1 if case == "both" => 0x33,
            1 => 0x22,
            _ => 0,
        };
        if bytes != [expected; BLOCK] {
            return Err(io::Error::other(format!("case={case} LBA {lba} mismatch")));
        }
        println!("case={case} lba={lba} byte={expected:#04x} verified_bytes={BLOCK}");
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let [action, path] = &args[..] else {
        return Err(io::Error::other(
            "expected capture|neither|payload|footer|both PATH",
        ));
    };
    match action.to_str() {
        Some("capture") => capture(path.as_ref()),
        Some(case @ ("neither" | "payload" | "footer" | "both")) => check(path.as_ref(), case),
        _ => Err(io::Error::other("unknown action")),
    }
}
