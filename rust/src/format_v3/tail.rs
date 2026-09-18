use super::{
    RECORD_MAGIC, RecordHeader, RecordKind, VERSION, checkpoint::CheckpointDescriptor,
    checkpoint_publish::LogPosition, data_record::DataRecord, mapping_page::MappingPageRecord,
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAXIMUM_RECORD_BLOCKS, accounted_io::BackingIo, invalid_data,
    read_block_exact, read_u32, read_u64,
};
use std::{fs::File, io};

#[expect(
    clippy::large_enum_variant,
    reason = "fixed stack storage avoids allocating for each replayed record"
)]
pub(crate) enum LogRecord {
    Data(DataRecord),
    Mapping(RecordHeader),
}

impl LogRecord {
    pub(crate) fn header(&self) -> RecordHeader {
        match self {
            Self::Data(record) => record.header,
            Self::Mapping(header) => *header,
        }
    }

    pub(crate) fn position(&self) -> LogPosition {
        let header = self.header();
        LogPosition {
            lsn: header.lsn,
            boundary: header.boundary,
        }
    }
}

pub(crate) fn read_next(
    backing_io: &mut BackingIo,
    backing: &File,
    checkpoint: CheckpointDescriptor,
    position: LogPosition,
) -> io::Result<Option<LogRecord>> {
    let geometry = checkpoint.geometry()?;
    if position.lsn < checkpoint.checkpoint_lsn
        || position.boundary < checkpoint.last_record_boundary
        || u64::from(position.boundary) >= geometry.backing_blocks
        || position.lsn - checkpoint.checkpoint_lsn
            > u64::from(position.boundary - checkpoint.last_record_boundary)
        || (position.lsn == checkpoint.checkpoint_lsn)
            != (position.boundary == checkpoint.last_record_boundary)
    {
        return Err(invalid_data("replay cursor is outside the checkpoint tail"));
    }
    let first = u64::from(position.boundary) + 1;
    if first == geometry.backing_blocks {
        return Ok(None);
    }
    let mut bytes = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(backing_io, backing, first as u32, &mut bytes)?;
    if current_metadata(&bytes.0, checkpoint.volume_id)
        && let Ok(header) =
            MappingPageRecord::decode_framing(&bytes.0, geometry, checkpoint.volume_id)
        && header.follows(position.lsn, position.boundary)
        && u64::from(header.boundary) == first
    {
        return Ok(Some(LogRecord::Mapping(header)));
    }
    let suspect_mapping = current_metadata(&bytes.0, checkpoint.volume_id)
        && (bytes.0[5] == RecordKind::MappingPage as u8 || unsupported(&bytes.0));
    let limit =
        (u64::from(position.boundary) + MAXIMUM_RECORD_BLOCKS).min(geometry.backing_blocks - 1);
    for boundary in first + 1..=limit {
        read_block_exact(backing_io, backing, boundary as u32, &mut bytes)?;
        if !current_metadata(&bytes.0, checkpoint.volume_id)
            || Some(read_u64(&bytes.0, 16)) != position.lsn.checked_add(1)
            || read_u32(&bytes.0, 24) != position.boundary
            || u64::from(read_u32(&bytes.0, 28)) != boundary
        {
            continue;
        }
        if unsupported(&bytes.0) {
            return Err(invalid_data(
                "unsupported record version or kind in replay tail",
            ));
        }
        if let Ok(record) = DataRecord::decode(&bytes.0, geometry, checkpoint.volume_id) {
            return Ok(Some(LogRecord::Data(record)));
        }
    }
    if suspect_mapping {
        return Err(invalid_data("invalid mapping framing in replay tail"));
    }
    Ok(None)
}

fn current_metadata(bytes: &[u8; BLOCK_SIZE], volume_id: u64) -> bool {
    &bytes[..4] == RECORD_MAGIC && read_u64(bytes, 8) == volume_id
}

fn unsupported(bytes: &[u8; BLOCK_SIZE]) -> bool {
    bytes[4] != VERSION || RecordKind::try_from(bytes[5]).is_err()
}
