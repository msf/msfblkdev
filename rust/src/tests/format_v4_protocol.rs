//! File-backed tests of the format-4 on-disk protocol below the public engine.
use super::*;
use crate::format_v4::{
    HeadId, NO_PREVIOUS_BOUNDARY, RecordHeader, RecordKind, checkpoint_load,
    data_record::DataRecord,
    descriptor::{CheckpointDescriptor, CheckpointSlot},
    heads::Head,
    log::Log,
    recovery,
    segment_table::SegmentState,
    storage::{self, FormatOptions},
};
use crate::mapping_cache::{Access, Acquisition, MappingCache};
use std::os::unix::fs::FileExt;

const SEGMENT_BLOCKS: u32 = 8;
const SEGMENTS: u64 = 12;
const VOLUME_BLOCKS: u32 = 1000;

struct Fixture {
    backing: TemporaryBacking,
    file: File,
    io: BackingIo,
}

impl Fixture {
    fn new() -> io::Result<Self> {
        let backing = TemporaryBacking::new()?;
        backing.create_sized((2 + SEGMENTS * u64::from(SEGMENT_BLOCKS)) * BLOCK_SIZE as u64)?;
        storage::format(
            &backing.0,
            u64::from(VOLUME_BLOCKS) * BLOCK_SIZE as u64,
            FormatOptions {
                blocks_per_segment: SEGMENT_BLOCKS,
            },
            None,
        )?;
        let (file, _) = storage::open_backing(&backing.0)?;
        Ok(Self {
            backing,
            file,
            io: BackingIo::new(None)?,
        })
    }

    fn load(&mut self) -> io::Result<checkpoint_load::Loaded> {
        let blocks = self.file.metadata()?.len() / BLOCK_SIZE as u64;
        checkpoint_load::load(&mut self.io, &self.file, blocks, |geometry| {
            geometry.segment_count
        })
    }

    fn recover(
        &mut self,
        log: &mut Log,
        live: &mut u64,
        budget: u64,
    ) -> io::Result<recovery::Recovered> {
        let blocks = self.file.metadata()?.len() / BLOCK_SIZE as u64;
        recovery::recover(&mut self.io, &self.file, log, blocks, Some(budget), live)
    }

    fn cache(&self, log: &Log, budget: u64) -> MappingCache {
        MappingCache::new(
            u64::from(log.geometry.volume_blocks) * BLOCK_SIZE as u64,
            Some(budget),
        )
        .unwrap()
    }

    /// Appends one raw data record with `payloads` (LBA, fill byte) on `head`.
    fn append_data(
        &mut self,
        log: &mut Log,
        head: HeadId,
        payloads: &[(u32, u8)],
    ) -> io::Result<RecordHeader> {
        log.ensure_room(&mut self.io, &self.file, head, payloads.len() as u32 + 1)?;
        let header = log.data_header(head, payloads.len());
        let mut record = DataRecord::empty(header);
        let start = header.start(log.geometry)?;
        for (index, &(lba, fill)) in payloads.iter().enumerate() {
            let block = AlignedBlock([fill; BLOCK_SIZE]);
            let physical = start + index as u32;
            record.lba_ids[index] = lba;
            record.checksums[index] = payload_checksum(log.volume_id, lba, physical, &block.0);
            storage::write_blocks(
                &mut self.io,
                &self.file,
                physical,
                std::slice::from_ref(&block),
            )?;
        }
        let mut footer = AlignedBlock([0; BLOCK_SIZE]);
        record.encode(log.geometry, &mut footer.0)?;
        storage::write_blocks(
            &mut self.io,
            &self.file,
            header.boundary,
            std::slice::from_ref(&footer),
        )?;
        log.data_committed(head, header)?;
        Ok(header)
    }

    fn raw(&self) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.backing.0)
    }

    fn corrupt_block(&self, block: u32, offset: usize) -> io::Result<()> {
        let raw = self.raw()?;
        let mut bytes = [0; BLOCK_SIZE];
        raw.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
        bytes[offset] ^= 0xff;
        raw.write_all_at(&bytes, u64::from(block) * BLOCK_SIZE as u64)?;
        raw.sync_all()
    }

    fn read_block(&self, block: u32) -> io::Result<[u8; BLOCK_SIZE]> {
        let mut bytes = [0; BLOCK_SIZE];
        self.raw()?
            .read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
        Ok(bytes)
    }
}

fn mapping(
    cache: &mut MappingCache,
    log: &mut Log,
    fixture: &mut Fixture,
    lba: u32,
) -> io::Result<(u32, u64)> {
    let page_id = lba / 336;
    let entry = (lba % 336) as usize;
    let page = match cache.acquire(page_id, Access::Recovery).unwrap() {
        Acquisition::Resident(page) => page,
        Acquisition::Vacant(vacant) => {
            let block = log.index.snapshot_block(page_id);
            let entries = if block == 0 {
                crate::mapping_cache::MappingEntries::zeroed()
            } else {
                let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
                read_block_exact(&mut fixture.io, &fixture.file, block, &mut bytes)?;
                crate::format_v4::mapping_page::MappingPageRecord::decode(
                    &bytes.0,
                    &crate::format_v4::mapping_page::PageExpectation {
                        geometry: log.geometry,
                        volume_id: log.volume_id,
                        page_id,
                        boundary: block,
                        maximum_lsn: log.checkpoint_lsn(),
                    },
                )?
                .entries
            };
            vacant.install(entries).unwrap()
        }
        Acquisition::CheckpointRequired(_) => panic!("test cache too small"),
    };
    Ok((
        page.entries().physical_addr[entry],
        page.entries().payload_checksum[entry],
    ))
}

fn set_mapping(
    cache: &mut MappingCache,
    log: &mut Log,
    fixture: &mut Fixture,
    lba: u32,
    physical: u32,
    checksum: u64,
) -> io::Result<()> {
    mapping(cache, log, fixture, lba)?;
    let page_id = lba / 336;
    let Acquisition::Resident(mut page) = cache.acquire(page_id, Access::Recovery).unwrap() else {
        panic!("page resident");
    };
    let old = page.entries().physical_addr[(lba % 336) as usize];
    page.update((lba % 336) as usize, physical, checksum)
        .unwrap();
    if old != 0 {
        log.segments.remove_live(old)?;
    }
    if physical != 0 {
        log.segments.add_live(physical)?;
    }
    Ok(())
}

#[test]
fn format_writes_initial_table_page_and_two_equal_roots() -> io::Result<()> {
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let log = loaded.log;
    assert_eq!(loaded.live_blocks, 0);
    assert_eq!(log.checkpoint_lsn(), 1);
    assert_eq!(log.next_lsn, 2);
    assert_eq!(log.root_lsn, [Some(1), Some(1)]);
    assert_eq!(log.protected_lsn(), 1);
    assert_eq!(log.active_slot, CheckpointSlot::Green);
    let user = log.head(HeadId::User);
    assert_eq!(
        (user.segment, user.next_block, user.last_boundary),
        (0, 3, 2)
    );
    let relocation = log.head(HeadId::Relocation);
    assert_eq!(
        (
            relocation.segment,
            relocation.next_block,
            relocation.last_boundary
        ),
        (1, 2 + 8, NO_PREVIOUS_BOUNDARY)
    );
    assert_eq!(log.segments.free_count(), 10);
    assert_eq!(log.segments.table_block(0), 2);
    assert_eq!(log.segments.metadata_blocks(0), 1);
    assert_eq!(log.segments.used_blocks(0), 1);
    assert_eq!(log.segments.state(0), SegmentState::Open);
    log.segments.check_conservation()?;
    log.check_heads()?;
    for slot in CheckpointSlot::BOTH {
        let descriptor = CheckpointDescriptor::decode(&fixture.read_block(slot.block())?, slot)?;
        assert_eq!(descriptor.checkpoint_lsn, 1);
        assert_eq!(descriptor.head_boundary, [2, 0]);
        assert_eq!(descriptor.table_pages[0], 2);
        assert_eq!(descriptor.blocks_per_segment, SEGMENT_BLOCKS);
    }
    // Format refuses a segment that cannot hold every table page plus its link.
    let tiny = TemporaryBacking::new()?;
    tiny.create_sized((2 + 500 * 4) * BLOCK_SIZE as u64)?;
    assert!(
        storage::format(
            &tiny.0,
            BLOCK_SIZE as u64,
            FormatOptions {
                blocks_per_segment: 4
            },
            None
        )
        .is_ok()
    );
    let wide = TemporaryBacking::new()?;
    wide.create_sized((2 + 4000 * 4) * BLOCK_SIZE as u64)?;
    assert!(
        storage::format(
            &wide.0,
            BLOCK_SIZE as u64,
            FormatOptions {
                blocks_per_segment: 4
            },
            None
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn publication_moves_snapshots_index_and_table_pages_and_older_root_remains_a_fallback()
-> io::Result<()> {
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let mut log = loaded.log;
    let mut cache = fixture.cache(&log, 20_000);
    let header = fixture.append_data(&mut log, HeadId::User, &[(0, 0xa1), (700, 0xa2)])?;
    let start = header.start(log.geometry)?;
    let checksum_0 = payload_checksum(log.volume_id, 0, start, &[0xa1; BLOCK_SIZE]);
    let checksum_700 = payload_checksum(log.volume_id, 700, start + 1, &[0xa2; BLOCK_SIZE]);
    set_mapping(&mut cache, &mut log, &mut fixture, 0, start, checksum_0)?;
    set_mapping(
        &mut cache,
        &mut log,
        &mut fixture,
        700,
        start + 1,
        checksum_700,
    )?;
    assert!(log.publish(&mut fixture.io, &fixture.file, &mut cache)?);
    assert!(!log.publish(&mut fixture.io, &fixture.file, &mut cache)?);
    // Data (LSN 2), two snapshots (3, 4), one direct page (5). No segment changed state, so the
    // format-time table page stays current and is not rewritten.
    assert_eq!(log.checkpoint_lsn(), 5);
    assert_eq!(log.active_slot, CheckpointSlot::Blue);
    assert_eq!(log.root_lsn, [Some(1), Some(5)]);
    assert_eq!(log.protected_lsn(), 1);
    assert_eq!(log.index.snapshot_block(0), header.boundary + 1);
    assert_eq!(log.index.snapshot_block(2), header.boundary + 2);
    assert_eq!(log.index.direct_block(0), header.boundary + 3);
    assert_eq!(log.segments.table_block(0), 2);
    assert_eq!(log.segments.metadata_blocks(0), 4);
    assert_eq!(log.segments.live_blocks(0), 2);
    assert_eq!(cache.snapshot().dirty_slots, 0);
    log.segments.check_conservation()?;

    let reloaded = fixture.load()?;
    assert_eq!(reloaded.live_blocks, 2);
    assert_eq!(reloaded.log.checkpoint_lsn(), 5);
    assert_eq!(reloaded.log.active_slot, CheckpointSlot::Blue);
    assert_eq!(reloaded.log.index.snapshot_block(0), header.boundary + 1);
    assert_eq!(reloaded.log.segments.metadata_blocks(0), 4);
    assert_eq!(reloaded.log.segments.live_blocks(0), 2);
    let user = reloaded.log.head(HeadId::User);
    assert_eq!(
        (user.segment, user.next_block, user.last_boundary),
        (0, header.boundary + 4, header.boundary + 3)
    );
    assert_eq!(reloaded.log.segments.used_blocks(0), 1 + 3 + 3);
    let mut reloaded_log = reloaded.log;
    let mut reload_cache = fixture.cache(&reloaded_log, 20_000);
    assert_eq!(
        mapping(&mut reload_cache, &mut reloaded_log, &mut fixture, 0)?,
        (start, checksum_0)
    );
    assert_eq!(
        mapping(&mut reload_cache, &mut reloaded_log, &mut fixture, 700)?,
        (start + 1, checksum_700)
    );

    // A damaged new descriptor falls back to the format root; its tail then replays the data.
    fixture.corrupt_block(1, 100)?;
    let fallback = fixture.load()?;
    assert_eq!(fallback.log.checkpoint_lsn(), 1);
    assert_eq!(fallback.log.root_lsn, [Some(1), None]);
    assert_eq!(fallback.log.protected_lsn(), 1);
    // A damaged snapshot body invalidates the newer root as well.
    let good = fixture.read_block(1)?;
    fixture.corrupt_block(1, 100)?;
    fixture.raw()?.write_all_at(&good, BLOCK_SIZE as u64)?;
    fixture.corrupt_block(header.boundary + 1, 100)?;
    let fallback = fixture.load()?;
    assert_eq!(fallback.log.checkpoint_lsn(), 1);
    Ok(())
}

#[test]
fn recovery_merges_both_heads_by_global_lsn_and_follows_durable_links() -> io::Result<()> {
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let mut log = loaded.log;
    // Segment 0 holds the table page at block 2 and has 6 writable blocks left before its link.
    let user_a = fixture.append_data(&mut log, HeadId::User, &[(1, 0x11), (2, 0x12)])?; // LSN 2
    assert_eq!(user_a.boundary, 5);
    let relocation_a = fixture.append_data(&mut log, HeadId::Relocation, &[(3, 0x13)])?; // LSN 3
    let user_b = fixture.append_data(&mut log, HeadId::User, &[(1, 0x21), (4, 0x14)])?; // LSN 4
    assert_eq!(log.head(HeadId::User).room(log.geometry), 0);
    // The next user record does not fit: a link (LSN 5) opens segment 2, then the record (LSN 6).
    let user_c = fixture.append_data(&mut log, HeadId::User, &[(5, 0x15), (2, 0x22), (6, 0x16)])?;
    assert_eq!(user_c.lsn, 6);
    assert_eq!(log.head(HeadId::User).segment, 2);
    assert_eq!(log.segments.state(0), SegmentState::Sealed);
    assert_eq!(log.segments.seal_lsn(0), 5);
    let relocation_b = fixture.append_data(&mut log, HeadId::Relocation, &[(7, 0x17)])?; // LSN 7
    assert_eq!(log.writes.link_fsyncs, 1);
    assert_eq!(log.writes.user_payload_blocks, 7);
    assert_eq!(log.writes.relocation_payload_blocks, 2);
    drop(log);

    let mut reopened = fixture.load()?;
    let mut live = reopened.live_blocks;
    let recovered = fixture.recover(&mut reopened.log, &mut live, 20_000)?;
    // Six records, read once by the scan and once by the single replay pass.
    assert_eq!(recovered.stats.records_scanned, 12);
    assert_eq!(recovered.stats.tail_passes, 2);
    assert_eq!(recovered.stats.touched_pages, 1);
    assert_eq!(live, 7);
    let log = &mut reopened.log;
    assert_eq!(log.next_lsn, 8);
    assert_eq!(log.head(HeadId::User).position().boundary, user_c.boundary);
    assert_eq!(
        log.head(HeadId::Relocation).position().boundary,
        relocation_b.boundary
    );
    assert_eq!(log.head(HeadId::User).segment, 2);
    assert_eq!(log.segments.state(0), SegmentState::Sealed);
    assert_eq!(log.segments.seal_lsn(0), 5);
    assert_eq!(log.segments.state(2), SegmentState::Open);
    assert_eq!(log.segments.free_count(), 9);
    assert_eq!(log.segments.used_blocks(0), 8);
    assert_eq!(log.segments.live_blocks(0), 2);
    assert_eq!(log.segments.live_blocks(1), 2);
    assert_eq!(log.segments.live_blocks(2), 3);
    log.segments.check_conservation()?;
    let mut cache = recovered.cache;
    let expect = |lba: u32, header: RecordHeader, index: u32| {
        (header.start(log.geometry).unwrap() + index, lba)
    };
    let cases = [
        expect(1, user_b, 0),
        expect(2, user_c, 1),
        expect(3, relocation_a, 0),
        expect(4, user_b, 1),
        expect(5, user_c, 0),
        expect(6, user_c, 2),
        expect(7, relocation_b, 0),
    ];
    for (physical, lba) in cases {
        assert_eq!(
            mapping(&mut cache, log, &mut fixture, lba)?.0,
            physical,
            "LBA {lba}"
        );
    }
    assert_eq!(mapping(&mut cache, log, &mut fixture, 0)?, (0, 0));
    // Nothing was published: the old root still stands and the tail remains dirty.
    assert_eq!(log.checkpoint_lsn(), 1);
    assert_eq!(cache.snapshot().dirty_slots, 1);
    // Both open segments are sanitized to their ends.
    for block in user_c.boundary + 1..2 + 3 * SEGMENT_BLOCKS {
        assert_eq!(fixture.read_block(block)?, [0; BLOCK_SIZE], "block {block}");
    }
    for block in relocation_b.boundary + 1..2 + 2 * SEGMENT_BLOCKS {
        assert_eq!(fixture.read_block(block)?, [0; BLOCK_SIZE], "block {block}");
    }
    Ok(())
}

#[test]
fn torn_data_footer_ends_the_prefix_but_torn_metadata_fails_closed() -> io::Result<()> {
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let mut log = loaded.log;
    let first = fixture.append_data(&mut log, HeadId::User, &[(1, 0x11)])?;
    let second = fixture.append_data(&mut log, HeadId::Relocation, &[(2, 0x12)])?;
    let third = fixture.append_data(&mut log, HeadId::User, &[(3, 0x13)])?;
    drop(log);
    // Damage the second record's footer: LSN 3 is missing, so LSN 4 on the other head is cut off.
    fixture.corrupt_block(second.boundary, 4090)?;
    let mut reopened = fixture.load()?;
    let mut live = reopened.live_blocks;
    let recovered = fixture.recover(&mut reopened.log, &mut live, 20_000)?;
    assert_eq!(recovered.stats.records_scanned, 2);
    assert_eq!(live, 1);
    assert_eq!(reopened.log.next_lsn, 3);
    assert_eq!(
        reopened.log.head(HeadId::User).position().boundary,
        first.boundary
    );
    // Sanitation zeroed the cut-off third record and the torn second one.
    assert_eq!(fixture.read_block(third.boundary)?, [0; BLOCK_SIZE]);
    assert_eq!(fixture.read_block(second.boundary)?, [0; BLOCK_SIZE]);

    // A recognizable but damaged link fails closed instead of truncating history.
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let mut log = loaded.log;
    for _ in 0..3 {
        fixture.append_data(&mut log, HeadId::User, &[(1, 0x11), (2, 0x12)])?;
    }
    fixture.append_data(&mut log, HeadId::User, &[(3, 0x13)])?;
    let link_block = 2 + SEGMENT_BLOCKS - 1;
    assert_eq!(log.segments.state(0), SegmentState::Sealed);
    drop(log);
    let good = fixture.read_block(link_block)?;
    assert_eq!(good[5], RecordKind::SegmentLink as u8);
    fixture.corrupt_block(link_block, 33)?;
    let mut reopened = fixture.load()?;
    let mut live = reopened.live_blocks;
    assert!(
        fixture
            .recover(&mut reopened.log, &mut live, 20_000)
            .is_err()
    );
    // Header damage without a matching checksum is also closed; the intact link recovers.
    fixture
        .raw()?
        .write_all_at(&good, u64::from(link_block) * BLOCK_SIZE as u64)?;
    let mut reopened = fixture.load()?;
    let mut live = reopened.live_blocks;
    let recovered = fixture.recover(&mut reopened.log, &mut live, 20_000)?;
    assert_eq!(recovered.stats.records_scanned, 10);
    assert_eq!(reopened.log.head(HeadId::User).segment, 2);
    // An unsupported record kind in the tail fails closed as well.
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let mut log = loaded.log;
    let header = fixture.append_data(&mut log, HeadId::User, &[(1, 0x11)])?;
    let mut future = fixture.read_block(header.boundary)?;
    future[5] = 6;
    crate::format_v4::seal_block(&mut future);
    fixture
        .raw()?
        .write_all_at(&future, u64::from(header.boundary) * BLOCK_SIZE as u64)?;
    drop(log);
    let mut reopened = fixture.load()?;
    let mut live = reopened.live_blocks;
    assert!(
        fixture
            .recover(&mut reopened.log, &mut live, 20_000)
            .is_err()
    );
    Ok(())
}

#[test]
fn bounded_recovery_publishes_one_root_after_replaying_windows() -> io::Result<()> {
    let mut fixture = Fixture::new()?;
    let loaded = fixture.load()?;
    let mut log = loaded.log;
    // Three mapping pages touched; a one-slot cache forces three windows.
    let a = fixture.append_data(
        &mut log,
        HeadId::User,
        &[(0, 0x10), (336, 0x11), (672, 0x12)],
    )?;
    let b = fixture.append_data(&mut log, HeadId::Relocation, &[(1, 0x13), (337, 0x14)])?;
    drop(log);
    let mut reopened = fixture.load()?;
    let mut live = reopened.live_blocks;
    let recovered = fixture.recover(&mut reopened.log, &mut live, 1)?;
    assert_eq!(recovered.cache.snapshot().sizing.capacity, 1);
    assert_eq!(recovered.stats.touched_pages, 3);
    assert_eq!(recovered.stats.tail_passes, 4);
    assert_eq!(recovered.stats.snapshot_writes, 3);
    assert_eq!(live, 5);
    let log = &mut reopened.log;
    // Data LSNs 2 and 3; snapshots 4..6; direct page 7; table page 8 after a link.
    assert!(log.checkpoint_lsn() >= 8);
    assert_eq!(log.active_slot, CheckpointSlot::Blue);
    assert_eq!(log.root_lsn, [Some(1), Some(log.checkpoint_lsn())]);
    assert_eq!(recovered.cache.snapshot().dirty_slots, 0);
    log.segments.check_conservation()?;
    let mut fresh = fixture.load()?;
    assert_eq!(fresh.live_blocks, 5);
    assert_eq!(fresh.log.checkpoint_lsn(), log.checkpoint_lsn());
    let mut cache = fixture.cache(&fresh.log, 20_000);
    for (lba, header, index) in [(0, a, 0), (336, a, 1), (672, a, 2), (1, b, 0), (337, b, 1)] {
        let expected = header.start(fresh.log.geometry)? + index;
        assert_eq!(
            mapping(&mut cache, &mut fresh.log, &mut fixture, lba)?.0,
            expected,
            "LBA {lba}"
        );
    }
    let mut live = fresh.live_blocks;
    let again = fixture.recover(&mut fresh.log, &mut live, 1)?;
    assert_eq!(again.stats.records_scanned, 0);
    assert_eq!(live, 5);
    Ok(())
}

#[test]
fn link_fsync_precedes_every_write_into_the_new_segment() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized((2 + SEGMENTS * u64::from(SEGMENT_BLOCKS)) * BLOCK_SIZE as u64)?;
    storage::format(
        &backing.0,
        u64::from(VOLUME_BLOCKS) * BLOCK_SIZE as u64,
        FormatOptions {
            blocks_per_segment: SEGMENT_BLOCKS,
        },
        None,
    )?;
    let ledger = TemporaryBacking::new()?;
    // SAFETY: the ledger file is owned by this test and only used through the module.
    let reader = unsafe { io_accounting::Reader::create(&ledger.0, 1 << 30)? };
    // SAFETY: same ledger, same ownership; the writer is dropped before the reader.
    let writer = unsafe { io_accounting::Writer::open(&ledger.0)? };
    let (file, blocks) = storage::open_backing(&backing.0)?;
    let mut io = BackingIo::new(Some(writer))?;
    let mut log =
        checkpoint_load::load(&mut io, &file, blocks, |geometry| geometry.segment_count)?.log;
    let before = reader.snapshot()?;
    log.advance_head(&mut io, &file, HeadId::Relocation)?;
    let after = reader.snapshot()?;
    assert_eq!(
        after.write_completed_operations,
        before.write_completed_operations + 1
    );
    assert_eq!(
        after.flush_completed_operations,
        before.flush_completed_operations + 1
    );
    assert_eq!(log.head(HeadId::Relocation).segment, 2);
    assert_eq!(log.segments.state(1), SegmentState::Sealed);
    // The first write after the link lands in the new segment only after that fsync.
    let header = log.append_metadata(
        &mut io,
        &file,
        HeadId::Relocation,
        RecordKind::SegmentTablePage,
        |log, header, bytes| {
            log.segments
                .encode_table_page(header, 0)?
                .encode(log.geometry, bytes)
        },
    )?;
    assert_eq!(header.boundary, 2 + 2 * SEGMENT_BLOCKS);
    // The relocation head had no record yet, so its link is the first block of segment 1.
    assert_eq!(header.previous_boundary, 2 + SEGMENT_BLOCKS);
    let last = reader.snapshot()?;
    assert_eq!(
        last.write_completed_operations,
        after.write_completed_operations + 1
    );
    assert_eq!(
        last.flush_completed_operations,
        after.flush_completed_operations
    );
    // Heads exhaust the free segments with ENOSPC before any link is written.
    let mut fixture = Fixture::new()?;
    let mut log = fixture.load()?.log;
    for _ in 0..10 {
        log.advance_head(&mut fixture.io, &fixture.file, HeadId::User)?;
    }
    assert_eq!(log.segments.free_count(), 0);
    let writes = log.writes;
    assert_eq!(
        log.advance_head(&mut fixture.io, &fixture.file, HeadId::User)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(log.writes, writes);
    let _ = Head::opened(HeadId::User, 0, log.geometry)?;
    Ok(())
}
