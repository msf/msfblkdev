use super::*;
use crate::accounted_io::fault_plan::{Action, FaultPlan};
use std::sync::atomic::Ordering;

fn fixture() -> io::Result<(TemporaryBacking, Volume)> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 8, 12, PROBE_SEGMENT_BLOCKS)?;
    let mut volume = open_live(&backing.0)?;
    volume.enable_records_flush_retry()?;
    volume.flush()?;
    Ok((backing, volume))
}

#[test]
fn persistent_records_barrier_error_fences_after_one_retry() -> io::Result<()> {
    let (_backing, mut volume) = fixture()?;
    volume.write_block(0, &[0x71; BLOCK_SIZE])?;
    let hits = FaultPlan::persistent_eio(&mut volume.backing_io, opcode::Fsync::CODE);
    assert_eq!(volume.flush().unwrap_err().raw_os_error(), Some(libc::EIO));
    assert!(volume.is_failed());
    assert!(volume.flush().is_err());
    assert_eq!(hits.load(Ordering::Relaxed), 2);
    Ok(())
}

#[test]
fn discard_invalidates_an_otherwise_replayable_records_epoch() -> io::Result<()> {
    let (_backing, mut volume) = fixture()?;
    volume.write_block(0, &[0x72; BLOCK_SIZE])?;
    volume.discard_blocks(0, 1)?;
    let hits = FaultPlan::persistent_eio(&mut volume.backing_io, opcode::Fsync::CODE);
    assert_eq!(volume.flush().unwrap_err().raw_os_error(), Some(libc::EIO));
    assert!(volume.is_failed());
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn records_capacity_sync_fails_closed_before_evicting_retained_bytes() -> io::Result<()> {
    let (_backing, mut volume) = fixture()?;
    let hits = FaultPlan::install(
        &mut volume.backing_io,
        opcode::Fsync::CODE,
        1,
        Action::Reject(libc::EIO),
    );
    // The default one-block buffer emits one 2-block record per write. The segment
    // fits the entire epoch without a link or checkpoint. No client flush intervenes.
    for i in 0..MAXIMUM_RECORD_BLOCKS / 2 {
        volume.write_block(0, &[i as u8; BLOCK_SIZE])?;
    }
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert_eq!(
        volume
            .write_block(0, &[0x73; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert!(volume.is_failed());
    Ok(())
}

#[test]
fn records_retry_runs_under_the_live_claim_and_requires_heap_headroom() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 8, 12, TINY_SEGMENT_BLOCKS)?;
    let mut volume = open_live(&backing.0)?;
    // Replay relies on exclusive ownership; the live claim already excludes other lockers.
    let other = File::open(&backing.0)?;
    assert!(matches!(
        other.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(other);
    let budget = volume.heap_budget();
    volume.enable_records_flush_retry()?;
    assert!(volume.heap_budget().supporting_bytes > budget.supporting_bytes);
    assert!(volume.heap_budget().allocated_bytes() <= budget.total_bytes);
    drop(volume);
    let mut options = VolumeOpenOptions::default();
    options.heap.total_bytes = Some(budget.allocated_bytes());
    let mut tight = open_live_with_options(&backing.0, options)?;
    assert_eq!(
        tight.enable_records_flush_retry().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    Ok(())
}

#[test]
fn enabling_mid_epoch_does_not_replay_an_incomplete_history() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, 8, 12, TINY_SEGMENT_BLOCKS)?;
    let mut volume = open_live(&backing.0)?;
    volume.write_block(0, &[0x21; BLOCK_SIZE])?;
    volume.enable_records_flush_retry()?;
    volume.write_block(1, &[0x22; BLOCK_SIZE])?;
    let hits = FaultPlan::persistent_eio(&mut volume.backing_io, opcode::Fsync::CODE);
    assert_eq!(volume.flush().unwrap_err().raw_os_error(), Some(libc::EIO));
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert!(volume.is_failed());
    Ok(())
}

#[test]
fn one_shot_report_after_sync_replays_without_another_log_append() -> io::Result<()> {
    let (backing, mut volume) = fixture()?;
    volume.write_block(0, &[0x31; BLOCK_SIZE])?;
    let lsn = last_lsn(&volume);
    let hits = FaultPlan::install(
        &mut volume.backing_io,
        opcode::Fsync::CODE,
        1,
        Action::ErrorAfterSuccess(libc::EIO),
    );
    volume.flush()?;
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert_eq!(last_lsn(&volume), lsn);
    // Resuming must permit a subsequent distinct epoch, not just complete the failed flush.
    volume.write_block(1, &[0x32; BLOCK_SIZE])?;
    volume.flush()?;
    drop(volume);
    let mut restarted = open_live(&backing.0)?;
    let mut bytes = [0; BLOCK_SIZE];
    for lba in 0..2 {
        restarted.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [0x31 + lba as u8; BLOCK_SIZE]);
    }
    Ok(())
}
