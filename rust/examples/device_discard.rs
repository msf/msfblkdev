//! Drives one format-4 volume until its cleaner releases segments to the backing (BUG-1).
//!
//! Writes the whole volume twice, discards it, then runs idle maintenance until the engine has
//! issued backing discards: hole punches on a regular file, `BLKDISCARD` on a block device.
//! `tools/device-backing/nested-ublk.sh` runs it on a ublk device.
//!
//! ```sh
//! device_discard /dev/ublkb0
//! ```
use block_storage::{
    DEFAULT_FORMAT_WRITE_BUFFER_BYTES, SelectionMode, VolumeOpenOptions, open_live_with_options,
};
use std::{env, io, time::Duration};

/// Maintenance steps before giving up; each frees at most a few segments.
const MAX_STEPS: u32 = 1000;
const CLEANER_WAIT: Duration = Duration::from_millis(100);

fn main() -> io::Result<()> {
    let path = env::args()
        .nth(1)
        .ok_or_else(|| io::Error::other("usage: device_discard <backing>"))?;
    let options = VolumeOpenOptions {
        write_buffer_bytes: DEFAULT_FORMAT_WRITE_BUFFER_BYTES,
        ..VolumeOpenOptions::default()
    };
    let mut volume = open_live_with_options(&path, options)?;
    let blocks = volume.volume_blocks();
    let mut payload = [0_u8; 4096];
    for pass in 0..2_u32 {
        for lba in 0..blocks {
            payload[..8].copy_from_slice(&(u64::from(pass) << 32 | u64::from(lba)).to_le_bytes());
            volume.write_block(lba, &payload)?;
        }
        volume.flush()?;
    }
    volume.discard_blocks(0, blocks)?;
    volume.flush()?;
    for _step in 0..MAX_STEPS {
        volume.maintain(SelectionMode::Idle)?;
        volume.wait_for_cleaner(CLEANER_WAIT)?;
        let stats = volume.reclaim_stats();
        if stats.discards_issued > 0 && volume.cleaner_outstanding() == 0 {
            break;
        }
    }
    let stats = volume.reclaim_stats();
    let usage = volume.space_usage()?;
    println!(
        "discards_issued={} discards_failed={} segments_freed={} backing_bytes={} allocated_bytes={}",
        stats.discards_issued,
        stats.discards_failed,
        stats.segments_freed,
        usage.backing_bytes,
        usage.allocated_bytes
    );
    volume.close()?;
    if stats.discards_issued == 0 || stats.discards_failed != 0 {
        return Err(io::Error::other(
            "the volume released no segment to its backing",
        ));
    }
    Ok(())
}
