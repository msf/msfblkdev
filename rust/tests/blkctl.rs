//! `blkctl info` and `blkctl check` exit codes and output on images the engine wrote.
use block_storage::{FormatOptions, format_with_options, open_live};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BLOCK: usize = 4096;
const SEGMENT_BLOCKS: u32 = 64;
const SEGMENTS: u64 = 16;
const VOLUME_BLOCKS: u32 = 256;
/// The LBA whose payload the damage test finds by content.
const MARKED_LBA: u32 = 7;

/// A private directory removed on drop.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("blkctl-{name}-{}", std::process::id()));
        fs::create_dir(&dir)?;
        Ok(Self(dir))
    }

    fn image(&self) -> PathBuf {
        self.0.join("backing.img")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort: Drop cannot report errors.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn payload(lba: u32) -> [u8; BLOCK] {
    let mut block = [0; BLOCK];
    for (index, chunk) in block.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        chunk.copy_from_slice(&(lba ^ (index as u32).wrapping_mul(0x9e37_79b9)).to_le_bytes());
    }
    block
}

/// Formats an image, writes every LBA and closes it cleanly.
fn written_image(fixture: &Fixture) -> io::Result<PathBuf> {
    let path = fixture.image();
    let blocks = 2 + SEGMENTS * u64::from(SEGMENT_BLOCKS);
    File::create_new(&path)?.set_len(blocks * BLOCK as u64)?;
    let options = FormatOptions {
        blocks_per_segment: SEGMENT_BLOCKS,
    };
    format_with_options(&path, u64::from(VOLUME_BLOCKS) * BLOCK as u64, options)?;
    let mut volume = open_live(&path)?;
    for lba in 0..VOLUME_BLOCKS {
        volume.write_block(lba, &payload(lba))?;
    }
    volume.close()?;
    Ok(path)
}

fn blkctl(args: &[&str], backing: &Path) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_blkctl"))
        .args(args)
        .arg(backing)
        .output()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn flip_byte(path: &Path, offset: u64) -> io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0];
    file.read_exact_at(&mut byte, offset)?;
    byte[0] ^= 0x01;
    file.write_all_at(&byte, offset)
}

#[test]
fn clean_image_reports_exit_zero_for_info_and_check() -> io::Result<()> {
    let fixture = Fixture::new("clean")?;
    let image = written_image(&fixture)?;
    let info = blkctl(&["info", "--color=never"], &image)?;
    assert_eq!(info.status.code(), Some(0), "{info:?}");
    let text = stdout(&info);
    for expected in [
        "format 4",
        "  mapped",
        "100.0% of logical",
        "valid   selected",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
    let table = blkctl(&["info", "--segments", "--blocks"], &image)?;
    assert_eq!(table.status.code(), Some(0), "{table:?}");
    let text = stdout(&table);
    let mapped = text
        .lines()
        .find(|line| line.starts_with("  mapped"))
        .map(|line| line.split_whitespace().collect::<Vec<_>>());
    assert_eq!(
        mapped.as_deref(),
        Some(&["mapped", "256", "blocks", "100.0%", "of", "logical"][..])
    );
    assert!(stdout(&table).contains("SEAL_LSN"));
    let check = blkctl(&["check"], &image)?;
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    assert!(stdout(&check).contains("result     no errors"), "{check:?}");
    Ok(())
}

#[test]
fn check_exits_one_while_a_tail_awaits_replay() -> io::Result<()> {
    let fixture = Fixture::new("tail")?;
    let image = written_image(&fixture)?;
    let mut volume = open_live(&image)?;
    volume.write_block(0, &payload(1000))?;
    volume.flush()?;
    drop(volume);
    let check = blkctl(&["check"], &image)?;
    assert_eq!(check.status.code(), Some(1), "{check:?}");
    assert!(stdout(&check).contains("user 1 records"), "{check:?}");
    let info = blkctl(&["info"], &image)?;
    assert_eq!(info.status.code(), Some(0), "{info:?}");
    assert!(stdout(&info).contains("user 1 records"), "{info:?}");
    Ok(())
}

#[test]
fn damage_exits_four_and_names_the_block() -> io::Result<()> {
    let fixture = Fixture::new("damage")?;
    let image = written_image(&fixture)?;
    let bytes = fs::read(&image)?;
    let marked = payload(MARKED_LBA);
    let block = bytes
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .position(|candidate| *candidate == marked)
        .expect("the marked payload is in the image");
    flip_byte(&image, (block * BLOCK + 9) as u64)?;
    let check = blkctl(&["check"], &image)?;
    assert_eq!(check.status.code(), Some(4), "{check:?}");
    let expected = format!("block {block} LSN ");
    let text = stdout(&check);
    assert!(
        text.contains(&expected) && text.contains("payload checksum"),
        "{text}"
    );
    assert!(
        text.contains(&format!("LBA {MARKED_LBA}: payload checksum")),
        "{text}"
    );

    // A damaged descriptor: info still prints the view from the other root.
    flip_byte(&image, 100)?;
    let info = blkctl(&["info"], &image)?;
    assert_eq!(info.status.code(), Some(4), "{info:?}");
    let text = stdout(&info);
    assert!(
        text.contains("invalid") && text.contains("error  block 0: green root"),
        "{text}"
    );
    Ok(())
}

#[test]
fn check_refuses_a_live_backing_that_info_still_reads() -> io::Result<()> {
    let fixture = Fixture::new("live")?;
    let image = written_image(&fixture)?;
    let volume = open_live(&image)?;
    let check = blkctl(&["check"], &image)?;
    assert_eq!(check.status.code(), Some(8), "{check:?}");
    assert!(String::from_utf8_lossy(&check.stderr).contains("owned by a live volume"));
    let info = blkctl(&["info"], &image)?;
    assert_eq!(info.status.code(), Some(0), "{info:?}");
    drop(volume);
    Ok(())
}

#[test]
fn unusable_input_exits_eight() -> io::Result<()> {
    let fixture = Fixture::new("unusable")?;
    let zeroes = fixture.0.join("zeroes.img");
    File::create_new(&zeroes)?.set_len(64 * BLOCK as u64)?;
    let missing = fixture.0.join("missing.img");
    for (args, path) in [
        (&["info"][..], &zeroes),
        (&["check"][..], &zeroes),
        (&["info"][..], &missing),
        (&["check"][..], &missing),
        (&["frobnicate"][..], &zeroes),
        (&["check", "--segments"][..], &zeroes),
    ] {
        let output = blkctl(args, path)?;
        assert_eq!(output.status.code(), Some(8), "{args:?} {output:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    Ok(())
}
