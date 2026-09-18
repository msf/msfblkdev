use super::*;
use crate::format_v3::{
    checkpoint_load,
    mapping_page::{MappingPageRecord, PageExpectation},
    storage, volume,
};

#[test]
fn paged_threshold_checkpoints_before_exceeding_the_replay_bound() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(2048 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 2 * BLOCK_SIZE as u64, None)?;
    let options = VolumeOpenOptions {
        checkpoint_after_bytes: 340 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    };
    let mut volume = volume::open(&backing.0, options, Some(1), None)?;
    for write in 1..=170 {
        volume.write_block(write % 2, &[write as u8; BLOCK_SIZE])?;
    }
    let before = volume.mapping_stats();
    assert_eq!(before.io.index_bytes_written, 0);
    assert_eq!(volume.space_usage()?.log_used_bytes, 340 * 4096);
    assert_eq!(
        volume.write_block(2, &[0; BLOCK_SIZE]).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(volume.mapping_stats(), before);
    volume.write_block(1, &[171; BLOCK_SIZE])?;
    let after = volume.mapping_stats();
    assert_eq!(after.io.page_writes, 1);
    assert_eq!(after.io.index_bytes_written, 4096);
    assert_eq!(after.pressure_checkpoints, 0);
    assert_eq!(volume.space_usage()?.log_used_bytes, 343 * 4096);

    let mut backing_io = BackingIo::new(None)?;
    let file = backing.reopen()?;
    let (checkpoint, live) = checkpoint_load::load(&mut backing_io, &file, 2048)?;
    let descriptor = checkpoint.descriptor;
    assert_eq!(live, 2);
    assert_eq!(descriptor.generation, 1);
    assert_eq!(descriptor.checkpoint_lsn, 171);
    assert_eq!(descriptor.last_record_boundary, 344);
    let mut raw = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(
        &mut backing_io,
        &file,
        checkpoint.mapping_page_block[0],
        &mut raw,
    )?;
    let page = MappingPageRecord::decode(
        &raw.0,
        &PageExpectation {
            geometry: descriptor.geometry()?,
            volume_id: descriptor.volume_id,
            page_id: 0,
            boundary: checkpoint.mapping_page_block[0],
            maximum_lsn: descriptor.checkpoint_lsn,
        },
    )?;
    assert_eq!(page.entries.physical_addr[..2], [342, 340]);
    drop(volume);

    let mut reopened = volume::open(&backing.0, options, Some(1), None)?;
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, expected) in [(0, 170), (1, 171)] {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [expected; BLOCK_SIZE]);
    }
    for write in 172..=340 {
        reopened.write_block(write % 2, &[write as u8; BLOCK_SIZE])?;
    }
    assert_eq!(reopened.mapping_stats().io.page_writes, 0);
    reopened.write_block(1, &[85; BLOCK_SIZE])?;
    assert_eq!(reopened.mapping_stats().io.page_writes, 1);
    let (checkpoint, _) = checkpoint_load::load(&mut backing_io, &file, 2048)?;
    assert_eq!(checkpoint.descriptor.generation, 2);
    assert_eq!(checkpoint.descriptor.checkpoint_lsn, 342);
    reopened.close()
}

#[test]
fn paged_payload_corruption_preserves_output_and_zero_overwrite_is_mapped() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(64 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 2 * BLOCK_SIZE as u64, None)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    volume.write_block(0, &[1; BLOCK_SIZE])?;
    volume.write_block(1, &[2; BLOCK_SIZE])?;
    volume.flush()?;
    let file = OpenOptions::new().write(true).open(&backing.0)?;
    file.write_all_at(&[2; BLOCK_SIZE], 4 * BLOCK_SIZE as u64)?;
    let mut bytes = [0xff; BLOCK_SIZE];
    assert_eq!(
        volume.read_block(0, &mut bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(bytes, [0xff; BLOCK_SIZE]);
    volume.read_block(1, &mut bytes)?;
    assert_eq!(bytes, [2; BLOCK_SIZE]);
    volume.write_block(0, &[0; BLOCK_SIZE])?;
    assert_eq!(volume.space_usage()?.live_bytes, 2 * 4096);
    volume.close()?;
    let mut reopened = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    reopened.read_block(0, &mut bytes)?;
    assert_eq!(bytes, [0; BLOCK_SIZE]);
    assert_eq!(reopened.space_usage()?.live_bytes, 2 * 4096);
    reopened.close()
}
