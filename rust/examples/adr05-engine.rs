//! Guest client for tools/adr05-fault-lab/engine-checkpoint.py. No engine failpoints.
use block_storage::{FormatOptions, format_with_options, open_live};
use std::{env, fs::File, io, path::Path};

const BLOCK: usize = 4096;
const LOGICAL_BLOCKS: u32 = 8;
// The two root blocks followed by twelve 32-block segments (format 4).
const BACKING_BYTES: u64 = (2 + 12 * 32) * 4096;

fn format_new(path: &Path) -> io::Result<()> {
    File::create_new(path)?.set_len(BACKING_BYTES)?;
    format_with_options(
        path,
        u64::from(LOGICAL_BLOCKS) * BLOCK as u64,
        FormatOptions {
            blocks_per_segment: 32,
        },
    )
}

// The client history lives in the test binary on the OS disk, outside the backing file.
const WRITES: [(u32, u8); 3] = [(1, 0x17), (5, 0x53), (1, 0xab)];

fn exercise(export: &Path, expect_eio: bool) -> io::Result<()> {
    let mut volume = open_live(export)?;
    for (lba, value) in WRITES {
        volume.write_block(lba, &[value; BLOCK])?;
        println!("op=write_block lba={lba} byte={value:#04x} result=ok");
    }
    volume.flush()?;
    println!("op=flush result=ok");
    match (volume.close(), expect_eio) {
        (Err(error), true) if error.raw_os_error() == Some(libc::EIO) => {
            println!("op=close errno={} result=expected_eio", libc::EIO);
            Ok(())
        }
        (Ok(()), true) => Err(io::Error::other("checkpoint close missed EIO")),
        (Err(error), _) => Err(error),
        (Ok(()), false) => {
            println!("op=close result=ok");
            Ok(())
        }
    }
}

fn verify(copy: &Path) -> io::Result<()> {
    let mut recovered = open_live(copy)?;
    for lba in 0..LOGICAL_BLOCKS {
        let expected = match lba {
            1 => 0xab,
            5 => 0x53,
            _ => 0,
        };
        let mut actual = [0; BLOCK];
        recovered.read_block(lba, &mut actual)?;
        if actual != [expected; BLOCK] {
            return Err(io::Error::other(format!(
                "wrong recovered bytes at LBA {lba}"
            )));
        }
        println!("op=read_block lba={lba} byte={expected:#04x} bytes={BLOCK} match=true");
    }
    recovered.close()
}

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let [mode, path] = &args[..] else {
        return Err(io::Error::other("expected MODE PATH"));
    };
    let path = Path::new(path);
    match mode.to_str() {
        Some("format-new") => format_new(path),
        Some("control") => exercise(path, false),
        Some("descriptor-eio") => exercise(path, true),
        Some("verify-copy") => verify(path),
        _ => Err(io::Error::other("unknown mode")),
    }
}
