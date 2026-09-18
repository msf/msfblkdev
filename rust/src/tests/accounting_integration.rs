use super::*;
use crate::io_accounting::{Reader, Snapshot, Writer};
use std::os::unix::fs::FileExt;
use std::time::{SystemTime, UNIX_EPOCH};
struct AccountingFixture {
    ledger: PathBuf,
}

impl AccountingFixture {
    fn new(limit: u64) -> io::Result<(Self, Reader)> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let fixture = Self {
            ledger: std::env::temp_dir().join(format!(
                "block-storage-engine-accounting-{}-{nonce}",
                std::process::id()
            )),
        };
        // SAFETY: this fixture owns the private ledger and does not truncate or
        // externally mutate it while any handle is live.
        let reader = unsafe { Reader::create(&fixture.ledger, limit)? };
        Ok((fixture, reader))
    }

    fn writer(&self) -> io::Result<Writer> {
        // SAFETY: this fixture owns the ledger and all mutation uses accounting handles.
        unsafe { Writer::open(&self.ledger) }
    }
}
impl Drop for AccountingFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.ledger);
    }
}
fn create_backing(volume_blocks: u32) -> io::Result<(TemporaryBacking, Layout)> {
    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    backing.create_sized(
        (u64::from(layout.log_start) + 2 + MAXIMUM_RECORD_BLOCKS) * BLOCK_SIZE as u64,
    )?;
    Ok((backing, layout))
}
#[test]
fn public_accounting_apis_count_format_open_write_checkpoint_and_close() -> io::Result<()> {
    let volume_blocks = 1;
    let (backing, _) = create_backing(volume_blocks)?;
    let unlimited = 16 * 1024 * 1024;
    let (accounting, reader) = AccountingFixture::new(unlimited)?;

    format_with_accounting(
        &backing.0,
        u64::from(volume_blocks) * BLOCK_SIZE as u64,
        accounting.writer()?,
    )?;
    assert_eq!(
        reader.snapshot()?,
        Snapshot {
            write_limit_bytes: unlimited,
            write_reserved_bytes: 2 * BLOCK_SIZE as u64,
            write_completed_bytes: 2 * BLOCK_SIZE as u64,
            read_completed_bytes: 0,
            read_completed_operations: 0,
            write_completed_operations: 1,
            flush_completed_operations: 1,
            pending_operations: 0,
            errors: 0,
            completed_totals_incomplete: false,
            budget_exhausted: false,
        }
    );

    open_with_accounting(
        &backing.0,
        VolumeOpenOptions::default(),
        accounting.writer()?,
    )?
    .close()?;
    let mut volume = open_with_accounting(
        &backing.0,
        VolumeOpenOptions::default(),
        accounting.writer()?,
    )?;
    let expected = [0xa5; BLOCK_SIZE];
    volume.write_block(0, &expected)?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    volume.close()?;

    let block_bytes = BLOCK_SIZE as u64;
    let snapshot = reader.snapshot()?;
    assert_eq!(
        snapshot.write_completed_bytes,
        2 * block_bytes + 2 * MAXIMUM_RECORD_BLOCKS * block_bytes + 5 * block_bytes
    );
    assert_eq!(
        snapshot.write_reserved_bytes,
        snapshot.write_completed_bytes
    );
    // format descriptor (1) + two stale-tail clears + one record writev + one batched checkpoint
    // body chunk + one checkpoint descriptor. The body block writes collapse into one 128 KiB chunk.
    assert_eq!(
        snapshot.write_completed_operations,
        1 + 2 * MAXIMUM_RECORD_BLOCKS + 3
    );
    assert_eq!(
        snapshot.read_completed_bytes,
        2 * (2 + 338) * block_bytes + block_bytes
    );
    assert_eq!(snapshot.read_completed_operations, 2 * 339 + 1);
    assert_eq!(snapshot.flush_completed_operations, 7);
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 0);
    assert!(!snapshot.completed_totals_incomplete);
    assert!(!snapshot.budget_exhausted);
    Ok(())
}

#[test]
fn format_v3_accounts_both_index_bodies_descriptors_and_barriers() -> io::Result<()> {
    let (backing, _) = create_backing(1)?;
    let (accounting, reader) = AccountingFixture::new(1024 * 1024)?;
    crate::format_v3::storage::format(&backing.0, 4096, Some(accounting.writer()?))?;
    let snapshot = reader.snapshot()?;
    assert_eq!(snapshot.write_completed_bytes, 4 * 4096);
    assert_eq!(snapshot.write_reserved_bytes, 4 * 4096);
    assert_eq!(snapshot.write_completed_operations, 4);
    assert_eq!(snapshot.flush_completed_operations, 2);
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 0);
    Ok(())
}

#[test]
fn checkpoint_load_accounts_reads_and_skips_unmapped_pages() -> io::Result<()> {
    let (backing, _) = create_backing(321)?;
    crate::format_v3::storage::format(&backing.0, 321 * 4096, None)?;
    let (accounting, reader) = AccountingFixture::new(1)?;
    let file = backing.reopen()?;
    let checkpoint = crate::format_v3::checkpoint_load::load(
        &mut BackingIo::new(Some(accounting.writer()?))?,
        &file,
        file.metadata()?.len() / BLOCK_SIZE as u64,
    )?;
    assert_eq!(checkpoint.mapping_page_block.as_ref(), &[0, 0]);
    let snapshot = reader.snapshot()?;
    assert_eq!(snapshot.read_completed_operations, 3);
    assert_eq!(snapshot.read_completed_bytes, 3 * 4096);
    assert_eq!(snapshot.write_reserved_bytes, 0);
    assert_eq!(snapshot.write_completed_operations, 0);
    assert_eq!(snapshot.flush_completed_operations, 0);
    Ok(())
}

#[test]
fn checkpoint_publish_preserves_dirty_state_until_all_writes_succeed() -> io::Result<()> {
    use crate::format_v3::{checkpoint_load, checkpoint_publish};

    for blocks in 0..=3 {
        let (backing, mut active) = super::format_v3_load::populated_fixture(321)?;
        let (mut cache, tail) = super::format_v3_publish::pending_update(&backing, &active)?;
        let original = active.try_clone()?;
        let (accounting, reader) = AccountingFixture::new((blocks * 4096).max(1))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&backing.0)?;
        let result = checkpoint_publish::publish(
            &mut BackingIo::new(Some(accounting.writer()?))?,
            &file,
            &mut active,
            &mut cache,
            tail,
        );
        let selected = checkpoint_load::load(&mut BackingIo::new(None)?, &file, 32)?;
        if blocks < 3 {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOSPC));
            assert_eq!(active.descriptor, original.descriptor);
            assert_eq!(active.mapping_page_block, original.mapping_page_block);
            assert_eq!(cache.snapshot().dirty_slots, 1);
            assert_eq!(selected.descriptor, original.descriptor);
        } else {
            assert_eq!(result?.lsn, tail.lsn + 1);
            assert_eq!(cache.snapshot().dirty_slots, 0);
            assert_eq!(selected.descriptor, active.descriptor);
        }
        let snapshot = reader.snapshot()?;
        assert_eq!(snapshot.write_completed_operations, blocks);
        assert_eq!(snapshot.write_completed_bytes, blocks * 4096);
        assert_eq!(snapshot.flush_completed_operations, blocks);
    }
    Ok(())
}

#[test]
fn paged_record_io_failure_latches_before_mapping_publication() -> io::Result<()> {
    use crate::format_v3::{storage, volume};
    let backing = TemporaryBacking::new()?;
    backing.create_sized(64 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 4096, None)?;
    // Open clears 60 blocks. The remaining block cannot pay for a two-block writev.
    let (accounting, reader) = AccountingFixture::new(61 * 4096)?;
    let mut volume = volume::open(
        &backing.0,
        VolumeOpenOptions::default(),
        Some(1),
        Some(accounting.writer()?),
    )?;
    assert_eq!(
        volume
            .write_block(0, &[0xaa; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(volume.cache_snapshot().dirty_slots, 0);
    assert_eq!(volume.release_stats().records, 0);
    assert_eq!(reader.snapshot()?.write_completed_bytes, 60 * 4096);
    let mut bytes = [0xee; BLOCK_SIZE];
    assert!(volume.read_block(0, &mut bytes).is_err());
    assert_eq!(bytes, [0xee; BLOCK_SIZE]);
    assert!(volume.write_block(0, &[0; BLOCK_SIZE]).is_err());
    assert!(volume.flush().is_err());
    assert!(volume.close().is_err());
    Ok(())
}

#[test]
fn exact_budget_exhaustion_rejects_write_before_backing_modification() -> io::Result<()> {
    let (backing, layout) = create_backing(1)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let expected = [0x3c; BLOCK_SIZE];
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &expected)?;
    volume.close()?;

    let budget = MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64;
    let (accounting, reader) = AccountingFixture::new(budget)?;
    let mut metered = open_with_accounting(
        &backing.0,
        VolumeOpenOptions::default(),
        accounting.writer()?,
    )?;
    let append_offset = u64::from(layout.log_start + 2) * BLOCK_SIZE as u64;
    let file = File::open(&backing.0)?;
    let mut before = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut before, append_offset)?;
    let error = metered.write_block(0, &[0x7e; BLOCK_SIZE]).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    let mut after = [0; 2 * BLOCK_SIZE];
    file.read_exact_at(&mut after, append_offset)?;
    assert_eq!(after, before);
    drop(metered);

    let snapshot = reader.snapshot()?;
    assert_eq!(snapshot.write_reserved_bytes, budget);
    assert_eq!(snapshot.write_completed_bytes, budget);
    assert!(snapshot.budget_exhausted);
    assert!(!snapshot.completed_totals_incomplete);

    let mut reopened = open(&backing.0)?;
    let mut actual = [0; BLOCK_SIZE];
    reopened.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    reopened.close()
}

#[test]
fn unknown_accounted_opcode_is_rejected_and_marks_totals_incomplete() -> io::Result<()> {
    let (accounting, reader) = AccountingFixture::new(BLOCK_SIZE as u64)?;
    let mut backing_io = crate::accounted_io::BackingIo::new(Some(accounting.writer()?))?;
    let error =
        crate::accounted_io::submit_exact(&mut backing_io, opcode::Nop::new().build(), 1, 0)
            .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let snapshot = reader.snapshot()?;
    assert_eq!(snapshot.errors, 1);
    assert_eq!(snapshot.pending_operations, 0);
    assert!(snapshot.completed_totals_incomplete);
    Ok(())
}

#[test]
fn actual_short_backing_read_marks_accounting_incomplete() -> io::Result<()> {
    let (backing, layout) = create_backing(1)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0x91; BLOCK_SIZE])?;
    volume.close()?;

    let (accounting, reader) = AccountingFixture::new(16 * 1024 * 1024)?;
    let mut metered = open_with_accounting(
        &backing.0,
        VolumeOpenOptions::default(),
        accounting.writer()?,
    )?;
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .set_len(u64::from(layout.log_start) * BLOCK_SIZE as u64)?;
    let reads_before = reader.snapshot()?.read_completed_operations;
    let error = metered.read_block(0, &mut [0; BLOCK_SIZE]).unwrap_err();
    assert_eq!(error.to_string(), "short io_uring operation");
    let snapshot = reader.snapshot()?;
    assert_eq!(snapshot.read_completed_operations, reads_before);
    assert_eq!(snapshot.errors, 1);
    assert_eq!(snapshot.pending_operations, 0);
    assert!(snapshot.completed_totals_incomplete);
    assert!(metered.close().is_err());
    Ok(())
}
