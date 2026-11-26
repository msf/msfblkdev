//! ADR-11: the read-only view and the offline check against images the engine wrote.
use super::*;
use crate::format_v4::{
    RecordKind, data_record::DataRecord, decode_metadata_record, segment_table::SegmentState,
};
use crate::{CheckReport, Finding, RootStatus, SelectionMode, Severity, VolumeView};
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

fn run_check(path: &Path) -> io::Result<(CheckReport, Vec<Finding>)> {
    let mut findings = Vec::new();
    let report = crate::check(path, &mut |finding| findings.push(finding.clone()))?;
    Ok((report, findings))
}

fn assert_clean(path: &Path) -> io::Result<CheckReport> {
    let (report, findings) = run_check(path)?;
    assert!(findings.is_empty(), "{findings:#?}");
    assert_eq!((report.errors, report.warnings), (0, 0));
    Ok(report)
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
fn info_and_check_leave_the_image_byte_identical() -> io::Result<()> {
    let backing = churned_image()?;
    let before = std::fs::read(&backing.0)?;
    crate::open_readonly(&backing.0)?;
    let report = assert_clean(&backing.0)?;
    assert!(
        report.segments_walked > 0 && report.payloads > 0,
        "{report:?}"
    );
    assert_eq!(report.mapped_blocks, u64::from(VOLUME_BLOCKS));
    assert!(!report.has_tail());
    assert!(
        std::fs::read(&backing.0)? == before,
        "inspection changed the image"
    );
    Ok(())
}

#[test]
fn check_validates_and_reports_the_tail_that_open_live_replays() -> io::Result<()> {
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
    let report = assert_clean(&backing.0)?;
    assert!(report.has_tail());
    assert_eq!(report.tail, view.tail);

    let mut volume = open_live_with_options(&backing.0, options())?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    assert!(actual == value(0, 99));
    volume.close()?;
    assert!(!assert_clean(&backing.0)?.has_tail());
    Ok(())
}

#[test]
fn check_refuses_a_backing_that_open_live_owns_and_info_reads_it() -> io::Result<()> {
    let backing = churned_image()?;
    let volume = open_live_with_options(&backing.0, options())?;
    let Err(error) = run_check(&backing.0) else {
        panic!("check ran on a live backing");
    };
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_valid_roots(&crate::open_readonly(&backing.0)?);
    drop(volume);
    assert_clean(&backing.0)?;
    Ok(())
}

#[test]
fn inspection_rejects_a_backing_that_is_not_format_4() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(64 * BLOCK_SIZE as u64)?;
    for result in [
        crate::open_readonly(&backing.0).map(|_| ()),
        run_check(&backing.0).map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
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

/// One block of each structure the selected root uses, named for the failure message.
fn structure_blocks(path: &Path, view: &VolumeView) -> io::Result<Vec<(&'static str, u32)>> {
    let descriptor = newest_descriptor(path)?;
    let geometry = descriptor.geometry()?;
    let index = descriptor.direct_pages[0];
    let mapping = read_u32(&read_raw(path, index)?, 40);
    let page = read_raw(path, mapping)?;
    // A cold LBA: written once, so its payload sits in a sealed segment.
    let (lba, payload) = (HOT_BLOCKS..VOLUME_BLOCKS)
        .map(|lba| (lba, read_u32(&page, 40 + lba as usize * 4)))
        .find(|&(_, block)| {
            let segment = geometry
                .segment_of(block)
                .expect("mapped block in a segment");
            view.segments[segment as usize].state == SegmentState::Sealed
        })
        .ok_or_else(|| io::Error::other("no cold payload in a sealed segment"))?;
    let footer = (payload + 1..payload + 340)
        .find(|&block| {
            read_raw(path, block).is_ok_and(|bytes| {
                DataRecord::decode(&bytes, geometry, descriptor.volume_id)
                    .is_ok_and(|record| record.lba_ids.contains(&lba))
            })
        })
        .ok_or_else(|| io::Error::other("payload has no footer"))?;
    let segment = geometry.segment_of(payload)?;
    let seal_lsn = view.segments[segment as usize].seal_lsn;
    let start = geometry.segment_start(segment)?;
    let link = (start..start + SEGMENT_BLOCKS)
        .find(|&block| {
            read_raw(path, block).is_ok_and(|bytes| {
                decode_metadata_record(&bytes, geometry, descriptor.volume_id).is_ok_and(
                    |(header, _)| {
                        header.kind == RecordKind::SegmentLink && Some(header.lsn) == seal_lsn
                    },
                )
            })
        })
        .ok_or_else(|| io::Error::other("sealed segment has no link"))?;
    Ok(vec![
        ("descriptor", descriptor.slot.block()),
        ("index page", index),
        ("mapping page", mapping),
        ("table page", descriptor.table_pages[0]),
        ("live payload", payload),
        ("data footer", footer),
        ("segment link", link),
    ])
}

#[test]
fn check_detects_one_fault_in_each_structure() -> io::Result<()> {
    let backing = churned_image()?;
    assert_clean(&backing.0)?;
    let view = crate::open_readonly(&backing.0)?;
    for (structure, block) in structure_blocks(&backing.0, &view)? {
        let damaged = TemporaryBacking::new()?;
        std::fs::copy(&backing.0, &damaged.0)?;
        let mut bytes = read_raw(&damaged.0, block)?;
        bytes[2000] ^= 0x10;
        OpenOptions::new()
            .write(true)
            .open(&damaged.0)?
            .write_all_at(&bytes, u64::from(block) * BLOCK_SIZE as u64)?;
        let (report, findings) = run_check(&damaged.0)?;
        assert!(report.errors > 0, "{structure} at block {block}: no error");
        let named = findings.iter().any(|finding| {
            finding.severity == Severity::Error
                && (finding.block == Some(u64::from(block)) || structure == "data footer")
        });
        assert!(named, "{structure} at block {block}: {findings:#?}");
    }
    Ok(())
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
    assert_clean(&backing.0)?;
    Ok(())
}
