use super::*;
use crate::format_v3::{
    RecordHeader, RecordKind,
    checkpoint::CheckpointDescriptor,
    checkpoint_load,
    checkpoint_publish::LogPosition,
    data_record::DataRecord,
    index::Checkpoint,
    seal_metadata, storage,
    tail::{self, LogRecord},
};

fn empty_image() -> io::Result<(TemporaryBacking, Checkpoint)> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(1024 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, 640 * BLOCK_SIZE as u64, None)?;
    let (checkpoint, _) =
        checkpoint_load::load(&mut BackingIo::new(None)?, &backing.reopen()?, 1024)?;
    Ok((backing, checkpoint))
}

pub(super) fn write_zero_payload_record(
    file: &File,
    checkpoint: CheckpointDescriptor,
    count: usize,
) -> io::Result<DataRecord> {
    let mut record = DataRecord {
        header: RecordHeader {
            kind: RecordKind::Data,
            volume_id: checkpoint.volume_id,
            lsn: checkpoint.checkpoint_lsn + 1,
            previous_boundary: checkpoint.last_record_boundary,
            boundary: checkpoint.last_record_boundary + count as u32 + 1,
        },
        lba_ids: [0; MAXIMUM_PAYLOAD_BLOCKS],
        checksums: [0; MAXIMUM_PAYLOAD_BLOCKS],
    };
    for index in 0..count {
        record.lba_ids[index] = index as u32;
        record.checksums[index] = payload_checksum(
            checkpoint.volume_id,
            index as u32,
            checkpoint.last_record_boundary + 1 + index as u32,
            &[0; BLOCK_SIZE],
        );
    }
    let mut bytes = [0; BLOCK_SIZE];
    record.encode(checkpoint.geometry()?, &mut bytes)?;
    file.write_all_at(
        &bytes,
        u64::from(record.header.boundary) * BLOCK_SIZE as u64,
    )?;
    Ok(record)
}

#[test]
fn tail_reader_handles_record_bounds_and_partial_append() -> io::Result<()> {
    for count in [1, 2, 320, MAXIMUM_PAYLOAD_BLOCKS] {
        let (backing, checkpoint) = empty_image()?;
        let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
        let expected = write_zero_payload_record(&file, checkpoint.descriptor, count)?;
        let position = LogPosition {
            lsn: 0,
            boundary: checkpoint.descriptor.last_record_boundary,
        };
        let mut backing_io = BackingIo::new(None)?;
        let direct = backing.reopen()?;
        let record =
            tail::read_next(&mut backing_io, &direct, checkpoint.descriptor, position)?.unwrap();
        let end = record.position();
        let LogRecord::Data(actual) = record else {
            panic!("expected data record")
        };
        assert_eq!(actual, expected);
        assert!(tail::read_next(&mut backing_io, &direct, checkpoint.descriptor, end)?.is_none());
        // A footer without a complete checksum is an incomplete append.
        file.write_all_at(
            &[0; 8],
            u64::from(expected.header.boundary) * BLOCK_SIZE as u64 + 4088,
        )?;
        assert!(
            tail::read_next(&mut backing_io, &direct, checkpoint.descriptor, position)?.is_none()
        );
    }
    Ok(())
}

#[test]
fn tail_reader_checks_cursor_bounds_and_metadata_like_payloads() -> io::Result<()> {
    let (backing, checkpoint) = empty_image()?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let mut data = write_zero_payload_record(&file, checkpoint.descriptor, 1)?;
    let descriptor = checkpoint.descriptor;
    let position = LogPosition {
        lsn: 0,
        boundary: descriptor.last_record_boundary,
    };
    let mut payload = [0; BLOCK_SIZE];
    payload[..4].copy_from_slice(b"VBLF");
    payload[4] = 3;
    payload[5] = 2;
    payload[8..16].copy_from_slice(&descriptor.volume_id.to_le_bytes());
    let physical = position.boundary + 1;
    file.write_all_at(&payload, u64::from(physical) * BLOCK_SIZE as u64)?;
    data.checksums[0] = payload_checksum(descriptor.volume_id, 0, physical, &payload);
    let mut bytes = [0; BLOCK_SIZE];
    data.encode(descriptor.geometry()?, &mut bytes)?;
    file.write_all_at(&bytes, u64::from(data.header.boundary) * BLOCK_SIZE as u64)?;
    let direct = backing.reopen()?;
    let mut backing_io = BackingIo::new(None)?;
    let record = tail::read_next(&mut backing_io, &direct, descriptor, position)?.unwrap();
    let LogRecord::Data(decoded) = record else {
        panic!("payload is not a mapping snapshot")
    };
    assert_eq!(decoded, data);
    for (lsn, boundary) in [(0, 2), (1, 3), (0, 4), (2, 4), (1, 1024)] {
        assert_eq!(
            tail::read_next(
                &mut backing_io,
                &direct,
                descriptor,
                LogPosition { lsn, boundary }
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }
    assert!(
        tail::read_next(
            &mut backing_io,
            &direct,
            descriptor,
            LogPosition {
                lsn: 1,
                boundary: 1023
            }
        )?
        .is_none()
    );
    Ok(())
}

#[test]
fn older_root_replay_traverses_damaged_snapshot_to_later_flushed_data() -> io::Result<()> {
    let (backing, checkpoint) = super::format_v3_load::populated_fixture(321)?;
    let (_, tail) = super::format_v3_publish::pending_update(&backing, &checkpoint)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    file.sync_all()?;
    let snapshot_offset = u64::from(checkpoint.descriptor.last_record_boundary) * BLOCK_SIZE as u64;
    file.write_all_at(&[0xff], snapshot_offset + 40)?;
    file.sync_all()?;
    let direct = backing.reopen()?;
    let mut backing_io = BackingIo::new(None)?;
    let (older, _) = checkpoint_load::load(&mut backing_io, &direct, 32)?;
    assert_eq!(older.descriptor.generation, 0);
    let mut position = LogPosition {
        lsn: older.descriptor.checkpoint_lsn,
        boundary: older.descriptor.last_record_boundary,
    };
    for (kind, lsn, lba, byte) in [
        (RecordKind::Data, 1, 320, 0x5a),
        (RecordKind::MappingPage, 2, 0, 0),
        (RecordKind::Data, 3, 0, 0x9c),
    ] {
        let record =
            tail::read_next(&mut backing_io, &direct, older.descriptor, position)?.unwrap();
        position = record.position();
        assert_eq!(record.header().kind, kind);
        assert_eq!(position.lsn, lsn);
        if let LogRecord::Data(data) = record {
            let physical = data.header.previous_boundary + 1;
            assert_eq!(data.lba_ids[0], lba);
            let mut payload = AlignedBlock([0; BLOCK_SIZE]);
            read_block_exact(&mut backing_io, &direct, physical, &mut payload)?;
            assert_eq!(payload.0, [byte; BLOCK_SIZE]);
            assert_eq!(
                data.checksums[0],
                payload_checksum(older.descriptor.volume_id, lba, physical, &payload.0)
            );
        }
    }
    assert_eq!(position, tail);
    assert!(tail::read_next(&mut backing_io, &direct, older.descriptor, position)?.is_none());
    Ok(())
}

#[test]
fn tail_reader_rejects_unknown_records_and_detected_mapping_framing_damage() -> io::Result<()> {
    let (backing, checkpoint) = empty_image()?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let data = write_zero_payload_record(&file, checkpoint.descriptor, 1)?;
    let position = LogPosition {
        lsn: 0,
        boundary: checkpoint.descriptor.last_record_boundary,
    };
    let mut original = [0; BLOCK_SIZE];
    data.encode(checkpoint.descriptor.geometry()?, &mut original)?;
    let mut backing_io = BackingIo::new(None)?;
    for (offset, value) in [(4, 2), (5, 99)] {
        let mut bytes = original;
        bytes[offset] = value;
        seal_metadata(&mut bytes);
        file.write_all_at(&bytes, u64::from(data.header.boundary) * BLOCK_SIZE as u64)?;
        assert_eq!(
            tail::read_next(
                &mut backing_io,
                &backing.reopen()?,
                checkpoint.descriptor,
                position
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }
    let (backing, checkpoint) = super::format_v3_load::populated_fixture(321)?;
    let file = OpenOptions::new().read(true).write(true).open(&backing.0)?;
    let boundary = checkpoint.descriptor.last_record_boundary;
    let mut bytes = [0; BLOCK_SIZE];
    file.read_exact_at(&mut bytes, u64::from(boundary) * BLOCK_SIZE as u64)?;
    bytes[3880] ^= 1;
    file.write_all_at(&bytes, u64::from(boundary) * BLOCK_SIZE as u64)?;
    let (older, _) = checkpoint_load::load(&mut backing_io, &backing.reopen()?, 32)?;
    let position = LogPosition {
        lsn: 1,
        boundary: boundary - 1,
    };
    assert_eq!(
        tail::read_next(
            &mut backing_io,
            &backing.reopen()?,
            older.descriptor,
            position
        )
        .err()
        .unwrap()
        .kind(),
        io::ErrorKind::InvalidData
    );
    Ok(())
}
