use super::*;
use crate::format_v4::checkpoint_load;
use crate::io_accounting::{Reader, Snapshot, Writer};
use crate::mapping_cache::{Access, Acquisition};
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

/// Six tiny segments. Open sanitizes 7 blocks of segment 0 and all 8 of segment 1.
const FRESH_SANITATION_BLOCKS: u64 = 7 + 8;

fn create_backing(volume_blocks: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized((2 + 6 * u64::from(TINY_SEGMENT_BLOCKS)) * BLOCK_SIZE as u64)?;
    let _ = volume_blocks;
    Ok(backing)
}

fn format_backing(backing: &TemporaryBacking, volume_blocks: u32) -> io::Result<()> {
    format_with_options(
        &backing.0,
        u64::from(volume_blocks) * BLOCK_SIZE as u64,
        FormatOptions {
            blocks_per_segment: TINY_SEGMENT_BLOCKS,
        },
    )
}

fn tiny_options() -> VolumeOpenOptions {
    VolumeOpenOptions {
        cache_budget_bytes: Some(1),
        ..VolumeOpenOptions::default()
    }
}

#[test]
fn public_accounting_apis_count_format_open_write_checkpoint_and_close() -> io::Result<()> {
    // Default geometry: three 8 MiB segments. The sparse file costs nothing until written.
    let backing = TemporaryBacking::new()?;
    backing.create_sized((2 + 3 * 2048) * BLOCK_SIZE as u64)?;
    let unlimited = 64 * 1024 * 1024;
    let (accounting, reader) = AccountingFixture::new(unlimited)?;

    format_with_accounting(&backing.0, BLOCK_SIZE as u64, accounting.writer()?)?;
    assert_eq!(
        reader.snapshot()?,
        Snapshot {
            write_limit_bytes: unlimited,
            write_reserved_bytes: 3 * BLOCK_SIZE as u64,
            write_completed_bytes: 3 * BLOCK_SIZE as u64,
            read_completed_bytes: 0,
            read_completed_operations: 0,
            write_completed_operations: 3,
            flush_completed_operations: 2,
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
    let after_first = reader.snapshot()?;
    // Sanitation zeroes 2,047 blocks of segment 0 and 2,048 of segment 1 in 32-block batches.
    assert_eq!(
        after_first.write_completed_bytes,
        (3 + 2047 + 2048) * BLOCK_SIZE as u64
    );
    assert_eq!(after_first.write_completed_operations, 3 + 64 + 64);
    // Two descriptors, the table page, then one probe per head: the first block and up to
    // 338 footer candidates each.
    assert_eq!(after_first.read_completed_operations, 3 + 2 * 339);
    assert_eq!(after_first.flush_completed_operations, 2 + 1 + 1);

    let mut volume = open_with_accounting(
        &backing.0,
        VolumeOpenOptions::default(),
        accounting.writer()?,
    )?;
    let opened = reader.snapshot()?;
    let expected = [0xa5; BLOCK_SIZE];
    volume.write_block(0, &expected)?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    volume.close()?;

    let snapshot = reader.snapshot()?;
    // One data writev, then snapshot, direct page and descriptor.
    assert_eq!(
        snapshot.write_completed_operations,
        opened.write_completed_operations + 4
    );
    assert_eq!(
        snapshot.write_completed_bytes,
        opened.write_completed_bytes + 5 * BLOCK_SIZE as u64
    );
    assert_eq!(
        snapshot.write_reserved_bytes,
        snapshot.write_completed_bytes
    );
    assert_eq!(
        snapshot.read_completed_operations,
        opened.read_completed_operations + 1
    );
    assert_eq!(
        snapshot.flush_completed_operations,
        opened.flush_completed_operations + 2
    );
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 0);
    assert!(!snapshot.completed_totals_incomplete);
    assert!(!snapshot.budget_exhausted);
    Ok(())
}

#[test]
fn format_accounts_table_page_descriptors_and_barriers() -> io::Result<()> {
    let backing = create_backing(1)?;
    let (accounting, reader) = AccountingFixture::new(1024 * 1024)?;
    crate::format_v4::storage::format(
        &backing.0,
        4096,
        FormatOptions {
            blocks_per_segment: TINY_SEGMENT_BLOCKS,
        },
        Some(accounting.writer()?),
    )?;
    let snapshot = reader.snapshot()?;
    // One table page, then both descriptors, each group followed by one fsync.
    assert_eq!(snapshot.write_completed_bytes, 3 * 4096);
    assert_eq!(snapshot.write_reserved_bytes, 3 * 4096);
    assert_eq!(snapshot.write_completed_operations, 3);
    assert_eq!(snapshot.flush_completed_operations, 2);
    assert_eq!(snapshot.pending_operations, 0);
    assert_eq!(snapshot.errors, 0);
    Ok(())
}

#[test]
fn open_write_read_and_close_account_exact_deltas() -> io::Result<()> {
    let backing = create_backing(1)?;
    format_backing(&backing, 1)?;
    let (accounting, reader) = AccountingFixture::new(16 * 1024 * 1024)?;
    open_with_accounting(&backing.0, tiny_options(), accounting.writer()?)?.close()?;
    let after_first = reader.snapshot()?;
    // Descriptors and the table page are read, then each head probes its first block and the
    // footer candidates before its link slot (five after the table page, six in segment 1).
    // Both open segments are zeroed and fsynced; close finds nothing dirty and only flushes.
    assert_eq!(after_first.read_completed_operations, 3 + (1 + 5) + (1 + 6));
    assert_eq!(
        after_first.write_completed_bytes,
        FRESH_SANITATION_BLOCKS * BLOCK_SIZE as u64
    );
    assert_eq!(after_first.flush_completed_operations, 2);
    assert_eq!(
        after_first.write_reserved_bytes,
        after_first.write_completed_bytes
    );

    let mut volume = open_with_accounting(&backing.0, tiny_options(), accounting.writer()?)?;
    let opened = reader.snapshot()?;
    let expected = [0xa5; BLOCK_SIZE];
    volume.write_block(0, &expected)?;
    let written = reader.snapshot()?;
    assert_eq!(
        written.write_completed_operations,
        opened.write_completed_operations + 1
    );
    assert_eq!(
        written.write_completed_bytes,
        opened.write_completed_bytes + 2 * BLOCK_SIZE as u64
    );
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert_eq!(actual, expected);
    let read = reader.snapshot()?;
    assert_eq!(
        read.read_completed_operations,
        written.read_completed_operations + 1
    );
    assert_eq!(
        read.read_completed_bytes,
        written.read_completed_bytes + BLOCK_SIZE as u64
    );
    volume.close()?;
    let closed = reader.snapshot()?;
    // Snapshot, direct page, descriptor; one fsync before and one after the descriptor.
    assert_eq!(
        closed.write_completed_operations,
        read.write_completed_operations + 3
    );
    assert_eq!(
        closed.write_completed_bytes,
        read.write_completed_bytes + 3 * BLOCK_SIZE as u64
    );
    assert_eq!(
        closed.flush_completed_operations,
        read.flush_completed_operations + 2
    );
    assert_eq!(closed.write_reserved_bytes, closed.write_completed_bytes);
    assert_eq!(closed.pending_operations, 0);
    assert_eq!(closed.errors, 0);
    assert!(!closed.completed_totals_incomplete);
    assert!(!closed.budget_exhausted);
    Ok(())
}

#[test]
fn checkpoint_load_accounts_reads_and_skips_unmapped_pages() -> io::Result<()> {
    let backing = create_backing(337)?;
    format_backing(&backing, 337)?;
    let (accounting, reader) = AccountingFixture::new(1)?;
    let file = backing.reopen()?;
    let loaded = checkpoint_load::load(
        &mut BackingIo::new(Some(accounting.writer()?))?,
        &file,
        file.metadata()?.len() / BLOCK_SIZE as u64,
        |geometry| geometry.segment_count,
    )?;
    assert_eq!(loaded.log.index.snapshot_blocks(), &[0, 0]);
    assert_eq!(loaded.log.index.direct_block(0), 0);
    let snapshot = reader.snapshot()?;
    // Two descriptors and the table page; no index page or snapshot exists yet.
    assert_eq!(snapshot.read_completed_operations, 3);
    assert_eq!(snapshot.read_completed_bytes, 3 * 4096);
    assert_eq!(snapshot.write_reserved_bytes, 0);
    assert_eq!(snapshot.write_completed_operations, 0);
    assert_eq!(snapshot.flush_completed_operations, 0);
    Ok(())
}

#[test]
fn checkpoint_publish_preserves_the_old_root_until_all_writes_succeed() -> io::Result<()> {
    for blocks in 0..=3 {
        let backing = create_backing(337)?;
        format_backing(&backing, 337)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&backing.0)?;
        let blocks_in_file = file.metadata()?.len() / BLOCK_SIZE as u64;
        let mut loaded = checkpoint_load::load(
            &mut BackingIo::new(None)?,
            &file,
            blocks_in_file,
            |geometry| geometry.segment_count,
        )?;
        let original_lsn = loaded.descriptor.checkpoint_lsn;
        let mut cache = crate::mapping_cache::MappingCache::new(337 * 4096, Some(20_000)).unwrap();
        let Acquisition::Vacant(vacant) = cache.acquire(0, Access::Recovery).unwrap() else {
            panic!("fresh cache");
        };
        let mut page = vacant
            .install(crate::mapping_cache::MappingEntries::zeroed())
            .unwrap();
        page.update(0, 3, 9).unwrap();
        let (accounting, reader) = AccountingFixture::new((blocks * 4096).max(1))?;
        let mut backing_io = BackingIo::new(Some(accounting.writer()?))?;
        let result = loaded.log.publish(&mut backing_io, &file, &mut cache);
        let selected = checkpoint_load::load(
            &mut BackingIo::new(None)?,
            &file,
            blocks_in_file,
            |geometry| geometry.segment_count,
        )?;
        if blocks < 3 {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOSPC));
            assert_eq!(cache.snapshot().dirty_slots, 1);
            assert_eq!(selected.descriptor.checkpoint_lsn, original_lsn);
        } else {
            assert!(result?);
            assert_eq!(cache.snapshot().dirty_slots, 0);
            assert_eq!(selected.descriptor.checkpoint_lsn, original_lsn + 2);
            assert_eq!(selected.descriptor.slot, CheckpointSlot::Blue);
        }
        let snapshot = reader.snapshot()?;
        assert_eq!(snapshot.write_completed_operations, blocks);
        assert_eq!(snapshot.write_completed_bytes, blocks * 4096);
        // The records fsync precedes the descriptor write; the second follows it.
        assert_eq!(
            snapshot.flush_completed_operations,
            u64::from(blocks >= 2) + u64::from(blocks >= 3)
        );
        assert_eq!(backing_io.mapping_io.page_writes, u64::from(blocks >= 1));
        assert_eq!(
            backing_io.mapping_io.index_bytes_written,
            u64::from(blocks >= 2) * 4096
        );
    }
    Ok(())
}

#[test]
fn failed_pressure_publication_counts_completed_io_without_counting_publication() -> io::Result<()>
{
    let backing = create_backing(337)?;
    format_backing(&backing, 337)?;
    // Sanitation uses 15 blocks, the data record two, and the snapshot one; the direct page
    // write then exhausts the budget.
    let (accounting, _) = AccountingFixture::new((FRESH_SANITATION_BLOCKS + 3) * 4096)?;
    let mut volume = open_with_accounting(&backing.0, tiny_options(), accounting.writer()?)?;
    volume.write_block(0, &[1; BLOCK_SIZE])?;
    let mut bytes = [0xff; BLOCK_SIZE];
    assert_eq!(
        volume
            .read_block(336, &mut bytes)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(bytes, [0xff; BLOCK_SIZE]);
    let stats = volume.mapping_stats();
    assert_eq!(stats.checkpoint_failures, 1);
    assert_eq!(stats.pressure_checkpoints, 0);
    assert_eq!(stats.cache.counters.foreground.misses, 2);
    assert_eq!(stats.cache.counters.checkpoint_required, 1);
    assert_eq!(stats.cache.dirty_slots, 1);
    assert_eq!(stats.io.page_writes, 1);
    assert_eq!(stats.io.index_bytes_written, 0);
    assert!(volume.flush().is_err());
    assert_eq!(volume.mapping_stats(), stats);
    assert!(volume.close().is_err());
    Ok(())
}

#[test]
fn record_io_failure_latches_before_mapping_publication() -> io::Result<()> {
    let backing = create_backing(1)?;
    format_backing(&backing, 1)?;
    // Open spends the whole budget on sanitation; the two-block writev cannot be paid.
    let (accounting, reader) = AccountingFixture::new(FRESH_SANITATION_BLOCKS * 4096)?;
    let mut volume = open_with_accounting(&backing.0, tiny_options(), accounting.writer()?)?;
    assert_eq!(
        volume
            .write_block(0, &[0xaa; BLOCK_SIZE])
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(volume.cache_snapshot().dirty_slots, 0);
    assert_eq!(volume.space_usage()?.live_bytes, 0);
    assert_eq!(volume.release_stats().records, 0);
    assert_eq!(
        reader.snapshot()?.write_completed_bytes,
        FRESH_SANITATION_BLOCKS * 4096
    );
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
    let backing = create_backing(1)?;
    format_backing(&backing, 1)?;
    let expected = [0x3c; BLOCK_SIZE];
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &expected)?;
    volume.close()?;
    // Segment 0 holds the table page, the record, a snapshot and a direct page: 3 blocks left.
    let sanitation = (3 + 8) * BLOCK_SIZE as u64;
    let budget = sanitation + BLOCK_SIZE as u64;
    let (accounting, reader) = AccountingFixture::new(budget)?;
    let mut metered = open_with_accounting(&backing.0, tiny_options(), accounting.writer()?)?;
    let append_offset = u64::from(metered.log.head(HeadId::User).next_block) * BLOCK_SIZE as u64;
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
    assert_eq!(snapshot.write_reserved_bytes, sanitation);
    assert_eq!(snapshot.write_completed_bytes, sanitation);
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
    let backing = create_backing(1)?;
    format_backing(&backing, 1)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0x91; BLOCK_SIZE])?;
    volume.close()?;

    let (accounting, reader) = AccountingFixture::new(16 * 1024 * 1024)?;
    let mut metered = open_with_accounting(&backing.0, tiny_options(), accounting.writer()?)?;
    OpenOptions::new()
        .write(true)
        .open(&backing.0)?
        .set_len(u64::from(FIRST_DATA_BLOCK) * BLOCK_SIZE as u64)?;
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
