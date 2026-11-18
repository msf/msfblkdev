//! Opt-in regression for premature ENOSPC on the formatter's 128/185 MiB geometry.
use block_storage::{
    DEFAULT_FORMAT_WRITE_BUFFER_BYTES, SelectionMode, Volume, VolumeOpenOptions, open_with_options,
};
use std::{env, io, path::PathBuf, process::Command, time::Duration};

const MIB: usize = 1 << 20;
const BLOCK: usize = 4096;
const LBAS: usize = 128 * MIB / BLOCK;
const OVERWRITES: u32 = 3 * 185 * (MIB / BLOCK) as u32;
const SEED: u64 = 0x4287_ce00_213c_c375;

// Each word identifies the logical block, the version and its position in the block.
fn payload(lba: u32, version: u32) -> [u8; BLOCK] {
    let mut bytes = [0; BLOCK];
    if version != 0 {
        let tag = (u64::from(lba) << 32) | u64::from(version);
        for (i, word) in bytes.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            word.copy_from_slice(&(tag ^ (i as u64).wrapping_mul(0x9e37_79b9)).to_le_bytes());
        }
    }
    bytes
}

fn check_contents(volume: &mut Volume, versions: &[u32]) -> io::Result<()> {
    let mut actual = [0; BLOCK];
    for (lba, &version) in versions.iter().enumerate() {
        volume.read_block(lba as u32, &mut actual)?;
        assert!(
            actual == payload(lba as u32, version),
            "LBA {lba}, expected version {version}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "run make test-compaction-small on Linux with io_uring and O_DIRECT"]
fn overwrites_at_full_logical_fill_survive_restart() -> io::Result<()> {
    // Make owns the private directory. The production formatter refuses existing paths.
    let dir = env::var_os("BLOCK_STORAGE_SMALL_FIXTURE").expect("run make test-compaction-small");
    let path = PathBuf::from(dir).join("backing.img");
    let formatted = Command::new(env!("CARGO_BIN_EXE_block-storage-ublk"))
        .arg("format")
        .arg(&path)
        .args(["--size-mib", "128", "--prealloc-mib", "185"])
        .status()?;
    assert!(formatted.success(), "formatter: {formatted}");
    let options = VolumeOpenOptions {
        write_buffer_bytes: DEFAULT_FORMAT_WRITE_BUFFER_BYTES,
        ..Default::default()
    };
    let mut volume = open_with_options(&path, options)?;
    let geometry = volume.space_usage()?;
    assert_eq!(geometry.volume_bytes, (LBAS * BLOCK) as u64);
    assert_eq!(geometry.backing_bytes, (185 * MIB) as u64);
    assert_eq!(geometry.segment_bytes, (8 * MIB) as u64);

    let mut versions = vec![1; LBAS];
    for lba in 0..LBAS as u32 {
        volume.write_block(lba, &payload(lba, 1))?;
        if (lba + 1).is_multiple_of(256) {
            volume.flush()?;
        }
    }
    assert_eq!(volume.space_usage()?.live_bytes, geometry.volume_bytes);
    let mut rng = SEED;
    for i in 1..=OVERWRITES {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let lba = (rng % LBAS as u64) as u32;
        if i.is_multiple_of(32) {
            volume.maintain_for(SelectionMode::Idle, Duration::from_millis(2))?;
        }
        volume.write_block(lba, &payload(lba, i + 1)).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("overwrite {i}/{OVERWRITES} at LBA {lba}: {e}"),
            )
        })?;
        versions[lba as usize] = i + 1;
        if i.is_multiple_of(256) {
            volume.flush()?;
        }
    }
    volume.flush()?;
    let freed = volume.reclaim_stats().segments_freed;
    assert!(freed > 0, "no segment was reclaimed");
    volume.close()?;

    let mut volume = open_with_options(&path, options)?;
    check_contents(&mut volume, &versions)?;
    volume.discard_blocks(0, 2)?;
    versions[..2].fill(0);
    volume.flush()?;
    volume.write_block(0, &payload(0, OVERWRITES + 2))?;
    versions[0] = OVERWRITES + 2;
    volume.flush()?;
    volume.close()?;
    let mut volume = open_with_options(&path, options)?;
    check_contents(&mut volume, &versions)?;
    volume.close()?;
    println!(
        "{OVERWRITES} overwrites, {freed} segments reclaimed, {LBAS} LBAs verified after reopen"
    );
    Ok(())
}
