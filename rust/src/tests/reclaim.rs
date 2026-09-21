//! Serialized raw reclamation: wraparound, release, stale sources, discard and growth.
use super::*;
use crate::format_v4::segment_table::SegmentState;
use crate::{BackingDiscard, MaintenanceStep, SelectionMode};

const SEGMENTS: u64 = 12;
/// Wraparound fixtures use 64-block segments so that packed records fill them.
const WRAP_SEGMENT_BLOCKS: u32 = 64;

fn fixture(volume_blocks: u32, segments: u64) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, volume_blocks, segments, TINY_SEGMENT_BLOCKS)?;
    Ok(backing)
}

fn wrap_fixture(volume_blocks: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, volume_blocks, SEGMENTS, WRAP_SEGMENT_BLOCKS)?;
    Ok(backing)
}

fn options(write_buffer_blocks: u64) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: write_buffer_blocks * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    }
}

/// A deterministic xorshift generator for LBAs and payload bytes.
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

fn value(index: u32) -> [u8; BLOCK_SIZE] {
    let mut block = [0; BLOCK_SIZE];
    for chunk in block.chunks_exact_mut(4) {
        chunk.copy_from_slice(&index.to_le_bytes());
    }
    block
}

/// Runs maintenance until it reports Idle, returning the steps taken. Bounded by `limit`.
fn drain(
    volume: &mut Volume,
    mode: SelectionMode,
    limit: usize,
) -> io::Result<Vec<MaintenanceStep>> {
    let mut steps = Vec::new();
    for _ in 0..limit {
        let step = volume.maintain(mode)?;
        steps.push(step);
        if step == MaintenanceStep::Idle {
            return Ok(steps);
        }
    }
    Err(io::Error::other(format!(
        "maintenance did not become idle within {limit} steps: {steps:?}"
    )))
}

fn verify(volume: &mut Volume, oracle: &[Option<u32>]) -> io::Result<()> {
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, expected) in oracle.iter().enumerate() {
        volume.read_block(lba as u32, &mut bytes)?;
        let expected = expected.map_or([0; BLOCK_SIZE], value);
        assert_eq!(bytes, expected, "LBA {lba}");
    }
    Ok(())
}

#[test]
fn overwrites_beyond_capacity_reuse_segments_and_keep_latest_values() -> io::Result<()> {
    let backing = wrap_fixture(128)?;
    let mut volume = open_with_options(&backing.0, options(8))?;
    let capacity_blocks = volume.space_usage()?.log_capacity_bytes / BLOCK_SIZE as u64;
    let mut oracle = [None; 128];
    let mut random = Random(0x9e37_79b9);
    // Cold payloads written once, spread through the churn, must survive every relocation.
    let writes = 3 * capacity_blocks as u32;
    let mut cold = 48;
    for index in 1..=writes {
        let lba = if index % 40 == 0 && cold < 80 {
            cold += 1;
            cold - 1
        } else {
            random.next() % 48
        };
        volume.write_block(lba, &value(index))?;
        oracle[lba as usize] = Some(index);
        if index % 7 == 0 {
            volume.flush()?;
        }
        if index % 50 == 0 {
            verify(&mut volume, &oracle)?;
        }
    }
    volume.flush()?;
    verify(&mut volume, &oracle)?;
    let stats = volume.reclaim_stats();
    let writes_done = volume.log_writes();
    let appended = writes_done.user_payload_blocks
        + writes_done.user_footer_blocks
        + writes_done.relocation_payload_blocks
        + writes_done.relocation_footer_blocks
        + writes_done.metadata_blocks
        + writes_done.link_blocks;
    assert!(
        appended > 2 * capacity_blocks,
        "the log must have wrapped: {appended} blocks"
    );
    assert!(stats.segments_freed >= 3, "{stats:?}");
    assert!(stats.payloads_copied > 0, "{stats:?}");
    assert!(stats.victims_selected >= 3, "{stats:?}");
    assert_eq!(stats.enospc_all_live, 0);
    assert!(writes_done.relocation_payload_blocks > 0);
    volume.log.segments.check_conservation()?;
    let usage = volume.space_usage()?;
    assert!(usage.free_segments >= 1, "{usage:?}");
    assert_eq!(usage.live_bytes, 80 * BLOCK_SIZE as u64);
    volume.close()?;
    let mut reopened = open_with_options(&backing.0, options(8))?;
    verify(&mut reopened, &oracle)?;
    reopened.log.segments.check_conservation()?;
    assert_eq!(reopened.space_usage()?.live_bytes, 80 * BLOCK_SIZE as u64);
    reopened.close()
}

#[test]
fn entirely_dead_victim_is_freed_without_copies_and_its_space_is_punched() -> io::Result<()> {
    let backing = fixture(8, SEGMENTS)?;
    let mut volume = open_with_options(&backing.0, options(1))?;
    for lba in 0..3 {
        volume.write_block(lba, &value(lba + 1))?;
    }
    assert_eq!(volume.log.head(HeadId::User).segment, 0);
    // The fourth record does not fit: segment 0 seals with three dead-to-be payloads.
    for lba in 0..3 {
        volume.write_block(lba, &value(lba + 10))?;
    }
    assert_eq!(volume.log.segments.state(0), SegmentState::Sealed);
    assert_eq!(volume.log.segments.live_blocks(0), 0);
    volume.flush()?;
    let allocated_before = volume.space_usage()?.allocated_bytes;

    let mut steps = Vec::new();
    // Bounded: two publications make segment 0 eligible, one scan finds it dead, two more free it.
    for _ in 0..12 {
        let step = volume.maintain(SelectionMode::Idle)?;
        steps.push(step);
        if matches!(step, MaintenanceStep::Published { freed } if freed > 0) {
            break;
        }
    }
    let stats = volume.reclaim_stats();
    assert!(
        matches!(steps.last(), Some(MaintenanceStep::Published { freed: 1 })),
        "{steps:?}"
    );
    assert_eq!(stats.payloads_copied, 0, "{stats:?}");
    assert_eq!(stats.victims_selected, 1);
    assert_eq!(stats.victims_pending, 1);
    assert_eq!(stats.segments_freed, 1);
    assert_eq!(stats.discards_issued, 1);
    assert_eq!(volume.log.segments.state(0), SegmentState::Free);
    assert!(
        volume.space_usage()?.allocated_bytes < allocated_before,
        "hole punch released space"
    );
    volume.log.segments.check_conservation()?;
    let oracle: Vec<Option<u32>> = (0..8).map(|lba| (lba < 3).then_some(lba + 10)).collect();
    verify(&mut volume, &oracle)?;
    volume.close()?;
    let mut reopened = open_with_options(&backing.0, options(1))?;
    assert_eq!(reopened.log.segments.state(0), SegmentState::Free);
    verify(&mut reopened, &oracle)?;
    reopened.close()
}

#[test]
fn backing_discard_can_be_disabled() -> io::Result<()> {
    let backing = fixture(8, SEGMENTS)?;
    let mut volume = open_with_options(
        &backing.0,
        VolumeOpenOptions {
            backing_discard: BackingDiscard::Off,
            ..options(1)
        },
    )?;
    for round in [1, 10] {
        for lba in 0..3 {
            volume.write_block(lba, &value(lba + round))?;
        }
    }
    let steps = drain(&mut volume, SelectionMode::Idle, 40)?;
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, MaintenanceStep::Published { freed } if *freed > 0))
    );
    assert_eq!(volume.reclaim_stats().discards_issued, 0);
    assert_eq!(volume.reclaim_stats().discards_failed, 0);
    volume.close()
}

#[test]
fn user_overwrite_or_discard_between_scan_and_apply_rejects_the_stale_copy() -> io::Result<()> {
    for discard in [false, true] {
        let backing = fixture(8, SEGMENTS)?;
        let mut volume = open_with_options(&backing.0, options(1))?;
        // Segment 0: LBAs 0..3; the fourth write seals it with LBAs 0 and 1 still live.
        for lba in 0..3 {
            volume.write_block(lba, &value(lba + 1))?;
        }
        volume.write_block(2, &value(20))?;
        assert_eq!(volume.log.segments.state(0), SegmentState::Sealed);
        assert_eq!(volume.log.segments.live_blocks(0), 2);
        volume.flush()?;
        // Publish twice so segment 0 is eligible, then scan it.
        let mut scanned = false;
        for _ in 0..8 {
            if volume.maintain(SelectionMode::Idle)? == MaintenanceStep::Scanned {
                scanned = true;
                break;
            }
        }
        assert!(scanned);
        assert_eq!(volume.reclaim_stats().payloads_proposed, 2);
        // The user changes LBA 1 before the writer applies the window.
        if discard {
            volume.discard_blocks(1, 1)?;
        } else {
            volume.write_block(1, &value(30))?;
        }
        assert_eq!(
            volume.maintain(SelectionMode::Idle)?,
            MaintenanceStep::Applied
        );
        let stats = volume.reclaim_stats();
        assert_eq!(stats.payloads_stale, 1, "{stats:?}");
        assert_eq!(stats.payloads_copied, 1);
        let mut bytes = [0; BLOCK_SIZE];
        volume.read_block(1, &mut bytes)?;
        assert_eq!(bytes, if discard { [0; BLOCK_SIZE] } else { value(30) });
        volume.read_block(0, &mut bytes)?;
        assert_eq!(bytes, value(1));
        volume.close()?;
        let mut reopened = open_with_options(&backing.0, options(1))?;
        reopened.read_block(1, &mut bytes)?;
        assert_eq!(bytes, if discard { [0; BLOCK_SIZE] } else { value(30) });
        reopened.read_block(0, &mut bytes)?;
        assert_eq!(bytes, value(1));
        reopened.close()?;
    }
    Ok(())
}

#[test]
fn proposal_from_a_reused_victim_is_rejected_by_its_seal_lsn() -> io::Result<()> {
    let backing = fixture(8, SEGMENTS)?;
    // Two window buffers: the test holds one while maintenance continues with the other.
    let mut volume = open_with_options(
        &backing.0,
        VolumeOpenOptions {
            heap: HeapBudgetOptions {
                cleaner_bytes: Some(2 * 1024 * 1024),
                ..HeapBudgetOptions::default()
            },
            ..options(1)
        },
    )?;
    for lba in 0..3 {
        volume.write_block(lba, &value(lba + 1))?;
    }
    volume.write_block(3, &value(4))?;
    assert_eq!(volume.log.segments.state(0), SegmentState::Sealed);
    volume.flush()?;
    let mut scanned = false;
    for _ in 0..8 {
        if volume.maintain(SelectionMode::Idle)? == MaintenanceStep::Scanned {
            scanned = true;
            break;
        }
    }
    assert!(scanned);
    let held = volume.take_queued().expect("a scanned window");
    assert_eq!(held.victim, 0);
    let old_seal = held.seal_lsn;
    // Kill the victim's payloads, let maintenance free it and write until segment 0 is reused.
    for lba in 0..3 {
        volume.write_block(lba, &value(lba + 40))?;
    }
    drain(&mut volume, SelectionMode::Idle, 60)?;
    // The barrier freed segment 0; the lowest free segment is reallocated as soon as a head
    // advances, so it is either free or in a later incarnation.
    assert!(volume.reclaim_stats().segments_freed >= 1);
    let mut writes = 50;
    while volume.log.segments.state(0) == SegmentState::Free {
        volume.write_block(5, &value(writes))?;
        writes += 1;
        assert!(writes < 200, "segment 0 was never reallocated");
    }
    assert!(
        volume.log.segments.state(0) != SegmentState::Sealed
            || volume.log.segments.seal_lsn(0) != old_seal
    );
    let before = mapping_arrays(&volume)?;
    volume.apply_buffer(&held)?;
    assert_eq!(volume.reclaim_stats().proposals_rejected_source, 1);
    assert_eq!(mapping_arrays(&volume)?, before);
    assert_ne!(volume.log.segments.seal_lsn(0), old_seal);
    let mut bytes = [0; BLOCK_SIZE];
    for lba in 0..3 {
        volume.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, value(lba + 40));
    }
    volume.close()
}

#[test]
fn fallback_to_the_older_root_replays_a_link_into_a_pending_segment() -> io::Result<()> {
    let backing = fixture(8, SEGMENTS)?;
    let mut volume = open_with_options(&backing.0, options(1))?;
    for round in [1, 10] {
        for lba in 0..3 {
            volume.write_block(lba, &value(lba + round))?;
        }
    }
    // Free segment 0 through the barrier: pending in root A, free in root B.
    let steps = drain(&mut volume, SelectionMode::Idle, 40)?;
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, MaintenanceStep::Published { freed } if *freed > 0))
    );
    assert_eq!(volume.log.segments.state(0), SegmentState::Free);
    let newest_slot = volume.log.active_slot;
    let newest_lsn = volume.log.checkpoint_lsn();
    let older_lsn = volume.log.protected_lsn();
    assert!(older_lsn < newest_lsn);
    // Write until the User head reuses segment 0, then add a marker and flush without publishing.
    let mut writes = 100;
    while volume.log.head(HeadId::User).segment != 0 {
        volume.write_block(4, &value(writes))?;
        writes += 1;
        assert!(writes < 300, "segment 0 was never reused");
    }
    volume.write_block(5, &value(777))?;
    volume.flush()?;
    assert_eq!(
        volume.log.checkpoint_lsn(),
        newest_lsn,
        "no publication after the barrier"
    );
    let last_write = writes - 1;
    drop(volume);

    // Damage root B; recovery must select A, where segment 0 is PendingFree, and still follow
    // the link into it (review finding T7).
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let offset = newest_slot.block() as u64 * BLOCK_SIZE as u64 + 100;
    let mut byte = [0; 1];
    file.read_exact_at(&mut byte, offset)?;
    byte[0] ^= 0xff;
    file.write_all_at(&byte, offset)?;
    file.sync_all()?;
    drop(file);

    let mut reopened = open_with_options(&backing.0, options(1))?;
    assert_eq!(reopened.log.checkpoint_lsn(), older_lsn);
    assert_eq!(reopened.log.segments.state(0), SegmentState::Open);
    let mut bytes = [0; BLOCK_SIZE];
    reopened.read_block(5, &mut bytes)?;
    assert_eq!(bytes, value(777));
    reopened.read_block(4, &mut bytes)?;
    assert_eq!(bytes, value(last_write));
    for lba in 0..3 {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, value(lba + 10));
    }
    reopened.log.segments.check_conservation()?;
    reopened.close()
}

#[test]
fn backing_growth_adds_free_segments_and_is_idempotent_by_bound() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    // Five segments: two heads plus the reserve. One write fits, the second cannot.
    format_segments(&backing, 8, 5, TINY_SEGMENT_BLOCKS)?;
    let grown_blocks = 2 + 9 * u64::from(TINY_SEGMENT_BLOCKS);
    let mut volume = open_with_options(
        &backing.0,
        VolumeOpenOptions {
            growth_capacity_bytes: Some(grown_blocks * BLOCK_SIZE as u64),
            ..options(1)
        },
    )?;
    volume.write_block(0, &value(1))?;
    assert_eq!(
        volume.write_block(1, &value(2)).unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    // An unrecorded extension stays unusable.
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .set_len((grown_blocks + 3) * BLOCK_SIZE as u64)?;
    assert_eq!(
        volume.write_block(1, &value(2)).unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    // Growth must fit the actual file and the preallocated state.
    assert!(
        volume
            .grow_backing((grown_blocks + 4) * BLOCK_SIZE as u64)
            .is_err()
    );
    assert!(
        volume
            .grow_backing((grown_blocks + 2 * u64::from(TINY_SEGMENT_BLOCKS)) * BLOCK_SIZE as u64)
            .is_err()
    );
    assert!(volume.grow_backing(BLOCK_SIZE as u64).is_err());
    volume.grow_backing(grown_blocks * BLOCK_SIZE as u64)?;
    assert_eq!(volume.log.geometry.segment_count, 9);
    assert_eq!(volume.space_usage()?.free_segments, 7);
    // A retry at the recovered bound appends nothing.
    let lsn = last_lsn(&volume);
    volume.grow_backing(grown_blocks * BLOCK_SIZE as u64)?;
    assert_eq!(last_lsn(&volume), lsn);
    // The trailing three blocks are a partial segment and do not count.
    volume.grow_backing((grown_blocks + 3) * BLOCK_SIZE as u64)?;
    assert_eq!(volume.log.geometry.segment_count, 9);
    assert_eq!(volume.log.geometry.backing_blocks, grown_blocks + 3);
    for lba in 1..8 {
        volume.write_block(lba, &value(lba + 1))?;
    }
    volume.close()?;
    let mut reopened = open_with_options(&backing.0, options(1))?;
    assert_eq!(reopened.log.geometry.segment_count, 9);
    let oracle: Vec<Option<u32>> = (0..8).map(|lba| Some(lba + 1)).collect();
    verify(&mut reopened, &oracle)?;
    reopened.log.segments.check_conservation()?;
    reopened.close()?;
    // A backing that shrank below the recorded bound is rejected.
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .set_len((grown_blocks - 1) * BLOCK_SIZE as u64)?;
    assert_eq!(
        open_with_options(&backing.0, options(1))
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}

#[test]
fn growth_record_survives_sigkill_at_both_boundaries() -> io::Result<()> {
    const GROWN_BLOCKS: u64 = 2 + 9 * TINY_SEGMENT_BLOCKS as u64;
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open_with_options(
            PathBuf::from(&path),
            VolumeOpenOptions {
                growth_capacity_bytes: Some(GROWN_BLOCKS * BLOCK_SIZE as u64),
                ..options(1)
            },
        )?;
        OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(GROWN_BLOCKS * BLOCK_SIZE as u64)?;
        volume.grow_backing(GROWN_BLOCKS * BLOCK_SIZE as u64)?;
        return Err(io::Error::other("growth failpoint did not pause"));
    }
    for boundary in ["growth-write-complete", "growth-fsync-complete"] {
        for _ in 0..configured_crash_test_plan(2)?.repetitions {
            let backing = TemporaryBacking::new()?;
            format_segments(&backing, 8, 5, TINY_SEGMENT_BLOCKS)?;
            let mut volume = open_with_options(&backing.0, options(1))?;
            volume.write_block(0, &value(1))?;
            volume.close()?;
            sigkill_child_at_handshake(
                "tests::reclaim::growth_record_survives_sigkill_at_both_boundaries",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            let mut reopened = open_with_options(&backing.0, options(1))?;
            let segments = reopened.log.geometry.segment_count;
            assert!(segments == 5 || segments == 9, "{boundary}: {segments}");
            let mut bytes = [0; BLOCK_SIZE];
            reopened.read_block(0, &mut bytes)?;
            assert_eq!(bytes, value(1));
            if segments == 9 {
                reopened.write_block(1, &value(2))?;
            } else {
                // Retrying at the same bound succeeds exactly once more.
                reopened.grow_backing(GROWN_BLOCKS * BLOCK_SIZE as u64)?;
                assert_eq!(reopened.log.geometry.segment_count, 9);
            }
            reopened.close()?;
        }
    }
    Ok(())
}

#[test]
fn steady_state_writes_reads_and_maintenance_allocate_nothing() -> io::Result<()> {
    let backing = wrap_fixture(128)?;
    let mut volume = open_with_options(&backing.0, options(8))?;
    let mut random = Random(0x2545_f491);
    for index in 1..=300 {
        volume.write_block(random.next() % 48, &value(index))?;
    }
    volume.flush()?;
    let (result, allocations) = super::allocations::measure(|| -> io::Result<()> {
        let mut bytes = [0; BLOCK_SIZE];
        for index in 301..=1500 {
            volume.write_block(random.next() % 48, &value(index))?;
            if index % 5 == 0 {
                volume.flush()?;
            }
            volume.read_block(random.next() % 16, &mut bytes)?;
        }
        for _ in 0..40 {
            if volume.maintain(SelectionMode::Idle)? == MaintenanceStep::Idle {
                break;
            }
        }
        volume.flush()
    });
    result?;
    assert_eq!(allocations, 0);
    assert!(volume.reclaim_stats().segments_freed > 0);
    volume.close()
}
