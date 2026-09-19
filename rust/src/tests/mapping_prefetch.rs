use super::*;
use crate::mapping_cache::{ENTRIES_PER_PAGE, sizing::Sizing};

fn options(slots: usize) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: BLOCK_SIZE as u64,
        cache_budget_bytes: Some(Sizing::for_capacity(slots).unwrap().allocated_bytes as u64),
        ..VolumeOpenOptions::default()
    }
}

fn fixture(pages: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(2048 * BLOCK_SIZE as u64)?;
    format(
        &backing.0,
        u64::from(pages) * ENTRIES_PER_PAGE as u64 * BLOCK_SIZE as u64,
    )?;
    let mut volume = open_with_options(&backing.0, options(pages as usize))?;
    // Reverse insertion makes cache-slot order differ from snapshot order.
    for page in (0..pages).rev() {
        volume.write_block(
            page * ENTRIES_PER_PAGE as u32,
            &[page as u8 + 1; BLOCK_SIZE],
        )?;
        volume.flush()?;
    }
    volume.close()?;
    Ok(backing)
}

fn read(volume: &mut Volume, page: u32) -> io::Result<()> {
    let mut bytes = [0; BLOCK_SIZE];
    volume.read_block(page * ENTRIES_PER_PAGE as u32, &mut bytes)?;
    assert_eq!(bytes, [page as u8 + 1; BLOCK_SIZE]);
    Ok(())
}

#[test]
fn sorted_snapshots_prefetch_in_one_read_in_cold_and_full_caches() -> io::Result<()> {
    let backing = fixture(65)?;
    let ledger = TemporaryBacking::new()?;
    // SAFETY: the private ledger outlives all handles and has no external writers.
    let reader = unsafe { crate::io_accounting::Reader::create(&ledger.0, 128 << 20)? };
    // SAFETY: same private ledger lifetime as above.
    let writer = unsafe { crate::io_accounting::Writer::open(&ledger.0)? };
    let mut volume = open_with_accounting(&backing.0, options(4), writer)?;
    let before = reader.snapshot()?;
    let pointers = &volume.mapping.checkpoint.mapping_page_block;
    for pair in pointers.windows(2) {
        assert_eq!(pair[1], pair[0] + 1);
    }
    let initial = volume.mapping_stats();
    for page in 0..65 {
        read(&mut volume, page)?;
    }
    let after = reader.snapshot()?;
    assert_eq!(
        after.read_completed_operations - before.read_completed_operations,
        65 + 18
    );
    assert_eq!(
        after.read_completed_bytes - before.read_completed_bytes,
        130 * BLOCK_SIZE as u64
    );
    let final_stats = volume.mapping_stats();
    assert_eq!(final_stats.io.page_reads - initial.io.page_reads, 65);
    assert_eq!(
        final_stats.io.demand_page_read_ops - initial.io.demand_page_read_ops,
        18
    );
    assert_eq!(final_stats.cache.counters.foreground.misses, 18);
    assert_eq!(final_stats.cache.counters.prefetched_pages, 47);
    assert_eq!(final_stats.cache.counters.prefetch_hits, 47);
    assert_eq!(final_stats.cache.counters.prefetch_unused_evictions, 0);
    assert_eq!(final_stats.pressure_checkpoints, 0);
    volume.close()
}

#[test]
fn prefetch_preserves_dirty_and_resident_pages_and_stops_at_nonadjacent_pointers() -> io::Result<()>
{
    let backing = fixture(8)?;
    let mut volume = open_with_options(&backing.0, options(4))?;
    volume.write_block(2 * ENTRIES_PER_PAGE as u32, &[0xa5; BLOCK_SIZE])?;
    volume.flush()?;
    read(&mut volume, 0)?;
    read(&mut volume, 1)?;
    assert_eq!(volume.mapping_stats().cache.counters.prefetched_pages, 0);
    let mut bytes = [0; BLOCK_SIZE];
    volume.read_block(2 * ENTRIES_PER_PAGE as u32, &mut bytes)?;
    assert_eq!(bytes, [0xa5; BLOCK_SIZE]);
    assert_eq!(volume.mapping_stats().cache.dirty_slots, 1);
    assert_eq!(volume.mapping_stats().pressure_checkpoints, 0);
    volume.close()?;
    let mut reopened = open_with_options(&backing.0, options(4))?;
    let initial = reopened.mapping_stats();
    read(&mut reopened, 0)?;
    read(&mut reopened, 1)?;
    assert_eq!(
        reopened.mapping_stats().io.page_reads - initial.io.page_reads,
        2
    );
    assert_eq!(reopened.mapping_stats().cache.counters.prefetched_pages, 0);
    reopened.close()
}

#[test]
fn speculative_corruption_is_not_installed_and_demand_corruption_still_fails() -> io::Result<()> {
    let backing = fixture(8)?;
    let mut volume = open_with_options(&backing.0, options(5))?;
    let pointer = volume.mapping.checkpoint.mapping_page_block[2];
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    read(&mut volume, 0)?;
    file.write_all_at(&[0xff], u64::from(pointer) * BLOCK_SIZE as u64 + 100)?;
    read(&mut volume, 1)?;
    assert_eq!(volume.mapping_stats().io.prefetch_rejected, 1);
    assert_eq!(volume.mapping_stats().cache.counters.prefetched_pages, 2);
    let mut bytes = [0x66; BLOCK_SIZE];
    assert!(
        volume
            .read_block(2 * ENTRIES_PER_PAGE as u32, &mut bytes)
            .is_err()
    );
    assert_eq!(bytes, [0x66; BLOCK_SIZE]);
    assert!(volume.failed);
    Ok(())
}

#[test]
fn short_prefetch_read_exposes_no_mapping_or_payload() -> io::Result<()> {
    for limit in [0, BLOCK_SIZE, BLOCK_SIZE + 512] {
        let backing = fixture(8)?;
        let mut volume = open_with_options(&backing.0, options(4))?;
        read(&mut volume, 0)?;
        volume.inject_metadata_short_read(limit);
        let mut bytes = [0x66; BLOCK_SIZE];
        assert!(
            volume
                .read_block(ENTRIES_PER_PAGE as u32, &mut bytes)
                .is_err()
        );
        assert!(volume.failed);
        assert_eq!(bytes, [0x66; BLOCK_SIZE]);
        assert_eq!(volume.mapping_stats().cache.resident_slots, 1);
        assert_eq!(volume.mapping_stats().cache.counters.prefetched_pages, 0);
        assert!(volume.flush().is_err());
    }
    Ok(())
}

#[test]
fn nonsequential_misses_do_not_prefetch() -> io::Result<()> {
    let backing = fixture(8)?;
    let mut volume = open_with_options(&backing.0, options(4))?;
    let initial = volume.mapping_stats();
    for page in [0, 3, 6, 2, 5, 1] {
        read(&mut volume, page)?;
    }
    let stats = volume.mapping_stats();
    assert_eq!(
        stats.io.demand_page_read_ops - initial.io.demand_page_read_ops,
        6
    );
    assert_eq!(stats.io.page_reads - initial.io.page_reads, 6);
    assert_eq!(stats.cache.counters.prefetched_pages, 0);
    volume.close()
}
