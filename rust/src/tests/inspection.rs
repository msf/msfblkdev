//! ADR-11: the read-only view against images the engine wrote.
use super::*;
use crate::{RootStatus, SelectionMode, VolumeView};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const VOLUME_BLOCKS: u32 = 512;
const SEGMENTS: u64 = 16;
const SEGMENT_BLOCKS: u32 = 64;
/// Hot LBAs that the churn overwrites; the rest are written once and stay cold.
const HOT_BLOCKS: u32 = 48;

fn options() -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: 8 * BLOCK_SIZE as u64,
        // The smallest interval publishes often, so roots move during the tests.
        checkpoint_after_bytes: crate::MAXIMUM_RECORD_BLOCKS * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    }
}

fn value(lba: u32, version: u32) -> [u8; BLOCK_SIZE] {
    let mut block = [0; BLOCK_SIZE];
    let tag = (u64::from(lba) << 32) | u64::from(version);
    for chunk in block.as_chunks_mut::<8>().0 {
        chunk.copy_from_slice(&tag.to_le_bytes());
    }
    block
}

/// Overwrites the hot LBAs `rounds` times with flushes and idle maintenance.
fn churn(volume: &mut Volume, rounds: u32) -> io::Result<()> {
    for version in 0..rounds {
        for lba in 0..HOT_BLOCKS {
            volume.write_block(lba, &value(lba, version + 2))?;
        }
        volume.flush()?;
        volume.maintain_for(SelectionMode::Idle, Duration::from_millis(1))?;
    }
    Ok(())
}

/// An image with sealed, cleaned and open segments, after a clean close.
fn churned_image() -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, VOLUME_BLOCKS, SEGMENTS, SEGMENT_BLOCKS)?;
    let mut volume = open_live_with_options(&backing.0, options())?;
    for lba in 0..VOLUME_BLOCKS {
        volume.write_block(lba, &value(lba, 1))?;
    }
    volume.flush()?;
    churn(&mut volume, 24)?;
    assert!(
        volume.reclaim_stats().segments_freed > 0,
        "the churn must clean"
    );
    volume.close()?;
    Ok(backing)
}

fn assert_valid_roots(view: &VolumeView) {
    for root in &view.roots {
        assert!(matches!(root, RootStatus::Valid { .. }), "{root:?}");
    }
}

#[test]
fn view_agrees_with_space_usage_after_a_clean_close() -> io::Result<()> {
    let backing = churned_image()?;
    let view = crate::open_readonly(&backing.0)?;
    assert_valid_roots(&view);
    assert_eq!(view.restarts, 0);
    assert_eq!(view.mapped_blocks, u64::from(VOLUME_BLOCKS));
    assert!(
        view.tail.iter().all(|tail| tail.records == 0),
        "{:?}",
        view.tail
    );
    let totals = view.totals();
    assert!(
        totals.sealed_segments > 0 && totals.free_segments > 0,
        "{totals:?}"
    );
    assert_eq!(totals.live_blocks, view.mapped_blocks);
    assert_eq!(
        totals.used_blocks + totals.free_blocks,
        SEGMENTS * u64::from(SEGMENT_BLOCKS)
    );

    let volume = open_live_with_options(&backing.0, options())?;
    let usage = volume.space_usage()?;
    drop(volume);
    let block = BLOCK_SIZE as u64;
    assert_eq!(usage.live_bytes, view.mapped_blocks * block);
    assert_eq!(usage.log_used_bytes, totals.used_blocks * block);
    assert_eq!(usage.metadata_bytes, (totals.metadata_blocks + 2) * block);
    assert_eq!(usage.segment_count as usize, view.segments.len());
    assert_eq!(usage.free_segments, totals.free_segments);
    assert_eq!(usage.provisioned_bytes, view.provisioned_blocks * block);
    assert_eq!(usage.backing_bytes, view.backing.bytes);
    Ok(())
}

#[test]
fn view_leaves_the_image_byte_identical() -> io::Result<()> {
    let backing = churned_image()?;
    let before = std::fs::read(&backing.0)?;
    crate::open_readonly(&backing.0)?;
    assert!(
        std::fs::read(&backing.0)? == before,
        "inspection changed the image"
    );
    Ok(())
}

#[test]
fn view_summarizes_the_tail_that_open_live_replays() -> io::Result<()> {
    let backing = churned_image()?;
    let mut volume = open_live_with_options(&backing.0, options())?;
    for lba in 0..4 {
        volume.write_block(lba, &value(lba, 99))?;
    }
    volume.flush()?;
    volume.discard_blocks(HOT_BLOCKS, 2)?;
    volume.flush()?;
    // Dropping without close leaves the flushed records after the checkpoint.
    drop(volume);
    let view = crate::open_readonly(&backing.0)?;
    let user = &view.tail[HeadId::User.index()];
    assert!(user.records >= 2, "{:?}", view.tail);

    let mut volume = open_live_with_options(&backing.0, options())?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert!(actual == value(0, 99));
    volume.close()?;
    let view = crate::open_readonly(&backing.0)?;
    assert!(
        view.tail.iter().all(|tail| tail.records == 0),
        "{:?}",
        view.tail
    );
    Ok(())
}

#[test]
fn view_reads_a_backing_that_open_live_owns() -> io::Result<()> {
    let backing = churned_image()?;
    let volume = open_live_with_options(&backing.0, options())?;
    assert_valid_roots(&crate::open_readonly(&backing.0)?);
    drop(volume);
    Ok(())
}

#[test]
fn view_rejects_a_backing_that_is_not_format_4() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(64 * BLOCK_SIZE as u64)?;
    let Err(error) = crate::open_readonly(&backing.0) else {
        panic!("a zeroed backing opened");
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    Ok(())
}

fn read_raw(path: &Path, block: u32) -> io::Result<[u8; BLOCK_SIZE]> {
    let mut bytes = [0; BLOCK_SIZE];
    File::open(path)?.read_exact_at(&mut bytes, u64::from(block) * BLOCK_SIZE as u64)?;
    Ok(bytes)
}

fn newest_descriptor(path: &Path) -> io::Result<CheckpointDescriptor> {
    let roots = CheckpointSlot::BOTH.map(|slot| {
        read_raw(path, slot.block()).and_then(|bytes| CheckpointDescriptor::decode(&bytes, slot))
    });
    let [Ok(green), Ok(blue)] = roots else {
        return Err(io::Error::other("both roots must decode"));
    };
    Ok(if blue.checkpoint_lsn > green.checkpoint_lsn {
        blue
    } else {
        green
    })
}

#[test]
fn view_restarts_when_the_writer_replaces_the_selected_root() -> io::Result<()> {
    let backing = churned_image()?;
    let old = newest_descriptor(&backing.0)?;
    let old_table = read_raw(&backing.0, old.table_pages[0])?;
    let mut volume = open_live_with_options(&backing.0, options())?;
    let mut churned = false;
    let mut overwrite_the_root = || {
        if churned {
            return;
        }
        churned = true;
        // Bounded: each round frees and reuses segments; the old table page dies soon.
        for _round in 0..200 {
            churn(&mut volume, 2).expect("churn");
            let current = read_raw(&backing.0, old.table_pages[0]).expect("read");
            if current != old_table {
                return;
            }
        }
        panic!("the writer never reused the old root's table page");
    };
    let view = crate::format_v4::readonly::open_readonly_with(&backing.0, &mut overwrite_the_root)?;
    assert_eq!(view.restarts, 1);
    assert_valid_roots(&view);
    assert!(view.checkpoint_lsn() > old.checkpoint_lsn);
    Ok(())
}

#[test]
fn view_never_reports_damage_while_a_writer_publishes() -> io::Result<()> {
    let backing = churned_image()?;
    let mut volume = open_live_with_options(&backing.0, options())?;
    let done = Arc::new(AtomicBool::new(false));
    let writer_done = Arc::clone(&done);
    let writer = std::thread::spawn(move || -> io::Result<()> {
        let result = churn(&mut volume, 300).and_then(|()| volume.close());
        writer_done.store(true, Ordering::Release);
        result
    });
    let mut views = 0;
    // Bounded by the writer's fixed work.
    while !done.load(Ordering::Acquire) {
        match crate::open_readonly(&backing.0) {
            Ok(view) => assert_valid_roots(&view),
            // The writer replaced the root more often than the walk could keep up.
            Err(error) if error.kind() == io::ErrorKind::Other => {}
            Err(error) => panic!("view {views}: {error}"),
        }
        views += 1;
    }
    writer.join().expect("writer thread")?;
    assert!(views > 0);
    Ok(())
}
