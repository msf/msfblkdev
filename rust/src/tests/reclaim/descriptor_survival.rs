//! Two crash images for one failed final fsync. Model the descriptor write as atomic at
//! 4 KiB; this explores neither torn writes nor the engine's online EIO response.
use super::*;

const TEST: &str = "tests::reclaim::descriptor_survival::failed_final_sync_preserves_flushed_data";
const HISTORY: [Option<u32>; 8] = [Some(10), Some(11), Some(12), None, None, None, None, None];

/// All six client writes precede a successful flush. Stop with segment 0 pending in A
/// and on the point of being released by the next checkpoint, B.
fn prepare_a() -> io::Result<(TemporaryBacking, CheckpointSlot, u64)> {
    let backing = fixture(HISTORY.len() as u32, SEGMENTS)?;
    let mut volume = open_live_with_options(&backing.0, options(1))?;
    for round in [1, 10] {
        for lba in 0..3 {
            volume.write_block(lba, &value(round + lba))?;
        }
    }
    volume.flush()?;
    // The fixture from entirely_dead_victim... needs at most 16 maintenance steps.
    for _ in 0..16 {
        if volume.log.segments.published_free_transitions() == 1 {
            assert_eq!(volume.log.segments.state(0), SegmentState::PendingFree);
            return Ok((backing, volume.log.active_slot, volume.log.checkpoint_lsn()));
        }
        if volume.maintain(SelectionMode::Pressure)? == MaintenanceStep::Waiting {
            assert!(volume.wait_for_cleaner(CLEANER_WAIT)?);
        }
    }
    Err(io::Error::other("segment 0 never reached checkpoint A"))
}

/// Run B from the same starting bytes in each child. One cut captures all the records
/// covered by the pre-descriptor fsync. The other also has the completed descriptor write.
fn capture_cut(source: &TemporaryBacking, boundary: &str) -> io::Result<Vec<u8>> {
    let copy = TemporaryBacking::new()?;
    copy.create_sized(0)?;
    std::fs::copy(&source.0, &copy.0)?;
    sigkill_child_at_handshake(TEST, &copy.0, boundary, Some(boundary))?;
    std::fs::read(&copy.0)
}

fn reopen_copy(image: &[u8]) -> io::Result<(TemporaryBacking, Volume)> {
    let copy = TemporaryBacking::new()?;
    copy.create_sized(0)?;
    std::fs::write(&copy.0, image)?;
    let volume = open_live_with_options(&copy.0, options(1))?;
    Ok((copy, volume))
}

/// Negative control: reuse one still-referenced block while A is selectable. Find the
/// payload by its known bytes, independently of the recovered mapping.
fn reject_early_reuse(mut image: Vec<u8>, slot_a: CheckpointSlot) -> io::Result<()> {
    let mut matches = image
        .as_chunks_mut::<BLOCK_SIZE>()
        .0
        .iter_mut()
        .enumerate()
        .filter(|(_, block)| **block == value(10));
    let (physical, payload) = matches.next().expect("fixture contains flushed LBA 0");
    payload.copy_from_slice(&value(99));
    assert!(matches.next().is_none(), "fixture payload must be unique");
    let (_copy, mut volume) = reopen_copy(&image)?;
    assert_eq!(volume.log.active_slot, slot_a);
    let error = verify(&mut volume, &HISTORY).expect_err("lost flushed LBA must be detected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "payload checksum mismatch");
    eprintln!("control=early-reuse lba=0 physical={physical} detected={error}");
    Ok(())
}

#[test]
fn failed_final_sync_preserves_flushed_data() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open_live_with_options(PathBuf::from(path), options(1))?;
        assert_eq!(volume.log.segments.state(0), SegmentState::PendingFree);
        assert_eq!(volume.log.segments.published_free_transitions(), 1);
        volume.maintain(SelectionMode::Pressure)?;
        return Err(io::Error::other("checkpoint B missed its failpoint"));
    }
    let (source, slot_a, lsn_a) = prepare_a()?;
    let durable = capture_cut(&source, "checkpoint-records-fsync-complete")?;
    let pending = capture_cut(&source, DESCRIPTOR_WRITE_COMPLETE_FAILPOINT)?;
    let slot_b = slot_a.other();
    let begin = slot_b.block() as usize * BLOCK_SIZE;
    let end = begin + BLOCK_SIZE;
    // A failed fsync promises nothing new. Preserve *all* other bytes in both images;
    // the lost-write case retains the prior slot contents, not zeroes or a broken checksum.
    assert!(durable[..begin] == pending[..begin] && durable[end..] == pending[end..]);
    let mut previous_slot = [0; BLOCK_SIZE];
    File::open(&source.0)?.read_exact_at(&mut previous_slot, begin as u64)?;
    assert!(durable[begin..end] == previous_slot);
    assert!(pending[begin..end] != previous_slot);
    let desc_b = CheckpointDescriptor::decode(&pending[begin..end], slot_b)?;
    assert!(desc_b.checkpoint_lsn > lsn_a);
    for (image, slot, lsn, state) in [
        (&durable, slot_a, lsn_a, SegmentState::PendingFree),
        (&pending, slot_b, desc_b.checkpoint_lsn, SegmentState::Free),
    ] {
        let (_copy, mut volume) = reopen_copy(image)?;
        assert_eq!(volume.log.active_slot, slot);
        assert_eq!(volume.log.checkpoint_lsn(), lsn);
        assert_eq!(volume.log.segments.state(0), state);
        verify(&mut volume, &HISTORY)?;
        eprintln!(
            "descriptor_slot={slot:?} checkpoint_lsn={lsn} segment_0={state:?} verified_lbas=8"
        );
    }
    reject_early_reuse(durable, slot_a)
}
