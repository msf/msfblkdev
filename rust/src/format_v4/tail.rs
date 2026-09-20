//! Locating the next record on one head's chain inside its open segment.
use super::{
    Geometry, RecordHeader, RecordKind, data_record::DataRecord, decode_metadata_framing,
    heads::Head, looks_like_record, unsupported_record,
};
use crate::{
    AlignedBlock, BLOCK_SIZE, MAXIMUM_PAYLOAD_BLOCKS, accounted_io::BackingIo, invalid_data,
    read_block_exact, read_u32, read_u64,
};
use std::{fs::File, io};

/// A record found on a head chain. Metadata bodies are decoded by the caller when needed.
/// Both variants hold about 4 KiB of fixed stack storage, so no replayed record allocates.
pub(crate) enum Probe {
    Data(DataRecord),
    Metadata {
        header: RecordHeader,
        block: AlignedBlock,
    },
}

impl Probe {
    pub(crate) fn header(&self) -> RecordHeader {
        match self {
            Self::Data(record) => record.header,
            Self::Metadata { header, .. } => *header,
        }
    }
}

/// Finds the record that follows `head`'s last record within its open segment, if any.
///
/// A metadata record sits at the next block. A data record's footer can be up to 338 blocks
/// later but never in the reserved link slot. Damaged recognizable metadata fails closed.
pub(crate) fn probe_next(
    io: &mut BackingIo,
    backing: &File,
    geometry: Geometry,
    volume_id: u64,
    head: Head,
    minimum_lsn: u64,
) -> io::Result<Option<Probe>> {
    let start = u64::from(head.next_block);
    let end = geometry.segment_end(head.segment)?;
    if start >= end {
        return Ok(None);
    }
    let mut block = AlignedBlock([0; BLOCK_SIZE]);
    read_block_exact(io, backing, head.next_block, &mut block)?;
    io.mapping_io.tail_blocks_read += 1;
    let mut suspect = false;
    if looks_like_record(&block.0, volume_id) {
        if unsupported_record(&block.0) {
            return Err(invalid_data(
                "unsupported record version or kind in replay tail",
            ));
        }
        if block.0[5] != RecordKind::Data as u8 {
            match decode_metadata_framing(&block.0, geometry, volume_id) {
                Ok(header)
                    if header.boundary == head.next_block
                        && header.chains_from(head.last_lsn, head.last_boundary)
                        && header.lsn >= minimum_lsn =>
                {
                    return Ok(Some(Probe::Metadata { header, block }));
                }
                _ => suspect = true,
            }
        }
    }
    // Data footers: the link slot at `end - 1` never holds one.
    let limit = (start + MAXIMUM_PAYLOAD_BLOCKS as u64).min(end.saturating_sub(2));
    let mut boundary = start + 1;
    while boundary <= limit {
        let position = u32::try_from(boundary).expect("segment blocks fit u32");
        read_block_exact(io, backing, position, &mut block)?;
        io.mapping_io.tail_blocks_read += 1;
        boundary += 1;
        if !looks_like_record(&block.0, volume_id)
            || read_u64(&block.0, 16) < minimum_lsn
            || read_u32(&block.0, 24) != head.last_boundary
            || read_u32(&block.0, 28) != position
        {
            continue;
        }
        if unsupported_record(&block.0) {
            return Err(invalid_data(
                "unsupported record version or kind in replay tail",
            ));
        }
        if block.0[5] != RecordKind::Data as u8 {
            continue;
        }
        if let Ok(record) = DataRecord::decode(&block.0, geometry, volume_id)
            && record.header.chains_from(head.last_lsn, head.last_boundary)
        {
            return Ok(Some(Probe::Data(record)));
        }
    }
    if suspect {
        return Err(invalid_data("invalid metadata framing in replay tail"));
    }
    Ok(None)
}
