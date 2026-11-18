//! Format-time fit estimate. This is an admission calculation, not a measured endurance result.
use super::{Geometry, heads::Reserve, reservation::Reservation, volume};
use crate::{
    BLOCK_SIZE, MAX_BACKING_BLOCKS, MAX_VOLUME_BLOCKS, MAXIMUM_PAYLOAD_BLOCKS, invalid_input,
};
use std::io;

/// Default ublk write buffer. Format planning leaves room for this or a smaller buffer.
pub const DEFAULT_FORMAT_WRITE_BUFFER_BYTES: u64 = 4 * 1024 * 1024;

/// Selected segment size and whole percent of advertised logical bytes expected to fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatPlan {
    pub segment_bytes: u64,
    pub expected_max_logical_fill: u8,
}

/// Selects the largest supported segment size that meets a 1..=100 percent logical fill target.
/// `raw_bytes` is declared capacity, not free space on the host filesystem. No backing is opened.
pub fn plan_format(
    logical_bytes: u64,
    raw_bytes: u64,
    min_logical_fill: u8,
) -> io::Result<FormatPlan> {
    let logical = logical_bytes / BLOCK_SIZE as u64;
    let raw = raw_bytes / BLOCK_SIZE as u64;
    if logical == 0
        || logical > MAX_VOLUME_BLOCKS
        || !logical_bytes.is_multiple_of(BLOCK_SIZE as u64)
    {
        return Err(invalid_input("invalid logical size"));
    }
    if raw == 0 || raw > MAX_BACKING_BLOCKS || !raw_bytes.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(invalid_input("invalid raw capacity"));
    }
    if !(1..=100).contains(&min_logical_fill) {
        return Err(invalid_input(
            "minimum logical fill must be within 1..=100 percent",
        ));
    }
    let mut best = 0;
    for mib in [32_u64, 16, 8] {
        let segment_bytes = mib << 20;
        let blocks = (segment_bytes / BLOCK_SIZE as u64) as u32;
        let Ok(geometry) = Geometry::new(logical as u32, raw, blocks) else {
            continue;
        };
        let expected_max_logical_fill = expected_fill(geometry)?;
        if expected_max_logical_fill >= min_logical_fill {
            return Ok(FormatPlan {
                segment_bytes,
                expected_max_logical_fill,
            });
        }
        best = best.max(expected_max_logical_fill);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "no supported segment size meets {min_logical_fill}% logical fill for {logical_bytes} logical bytes and {raw_bytes} raw bytes (best estimate {best}%)"
        ),
    ))
}

fn expected_fill(g: Geometry) -> io::Result<u8> {
    // No cache can hold more dirty mapping pages than exist, for either default buffer size.
    let pages = u64::from(g.page_count);
    let reserve = Reserve::compute(g, pages);
    let usable_segments = g.segment_count.saturating_sub(2 + reserve.segments);
    let writable = u64::from(g.blocks_per_segment - 1);
    let usable = u64::from(usable_segments) * writable;
    let index_and_table = u64::from(g.direct_pages + g.indirect_pages + g.table_pages);
    let metadata = pages + index_and_table;
    let buffer = (DEFAULT_FORMAT_WRITE_BUFFER_BYTES / BLOCK_SIZE as u64)
        .min(u64::from(g.volume_blocks)) as usize;
    let admission = Reservation::for_counts(
        buffer,
        buffer.min(g.page_count as usize),
        g.page_count as usize,
    )?;
    // One pending checkpoint publication (volume::maintenance_debt) plus a full buffer release.
    let debt = metadata + 2;
    let available = usable.saturating_sub(metadata + admission.demanded_blocks(g) + debt);
    if available == 0 || !volume::format_heap_fits(g)? {
        return Ok(0);
    }
    // Each segment crossing can require another packed footer. Deduct one per usable segment,
    // then invert live + ceil(live / MAXIMUM_PAYLOAD_BLOCKS) <= remaining exactly.
    let remaining = available.saturating_sub(u64::from(usable_segments));
    let slots = MAXIMUM_PAYLOAD_BLOCKS as u64;
    let live = (remaining * slots / (slots + 1)).min(u64::from(g.volume_blocks));
    Ok((live * 100 / u64::from(g.volume_blocks)) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1 << 20;
    const HEADER: u64 = 2 * BLOCK_SIZE as u64;

    #[test]
    fn format_plan_selects_largest_qualifying_size() {
        let logical = 128 * MIB;
        for (raw_mib, expected_mib) in [(192, 8), (320, 16), (512, 32)] {
            let plan = plan_format(logical, raw_mib * MIB + HEADER, 100).unwrap();
            assert_eq!(plan.segment_bytes, expected_mib * MIB);
            assert_eq!(plan.expected_max_logical_fill, 100);
        }
        // The same backing has room for only partial logical fill at the larger size.
        let raw = 320 * MIB + HEADER;
        let partial = plan_format(logical, raw, 75).unwrap();
        assert_eq!(partial.segment_bytes, 32 * MIB);
        assert!((75..100).contains(&partial.expected_max_logical_fill));
        assert_eq!(
            plan_format(logical, raw, partial.expected_max_logical_fill).unwrap(),
            partial
        );
        assert_eq!(
            plan_format(logical, raw, partial.expected_max_logical_fill + 1)
                .unwrap()
                .segment_bytes,
            16 * MIB
        );
    }

    #[test]
    fn format_plan_rejects_invalid_sizes_targets_and_insufficient_capacity() {
        for args in [
            (0, MIB, 100),
            (1, MIB, 100),
            (MIB, 0, 100),
            (MIB, MIB + 1, 100),
            ((MAX_VOLUME_BLOCKS + 1) * BLOCK_SIZE as u64, MIB, 1),
            (MIB, (MAX_BACKING_BLOCKS + 1) * BLOCK_SIZE as u64, 1),
            (MIB, 512 * MIB, 0),
            (MIB, 512 * MIB, 101),
            (128 * MIB, 128 * MIB, 100),
        ] {
            assert_eq!(
                plan_format(args.0, args.1, args.2).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        // Eight-MiB candidates need a seventh segment above the reserve for one live block.
        assert!(
            plan_format(
                BLOCK_SIZE as u64,
                56 * MIB + HEADER - BLOCK_SIZE as u64,
                100
            )
            .is_err()
        );
        assert_eq!(
            plan_format(BLOCK_SIZE as u64, 56 * MIB + HEADER, 100)
                .unwrap()
                .segment_bytes,
            8 * MIB
        );
    }

    #[test]
    fn format_plan_caps_fill_at_the_logical_size_and_respects_table_reach() {
        assert_eq!(
            plan_format(4 << 30, 6 << 30, 100).unwrap(),
            FormatPlan {
                segment_bytes: 32 * MIB,
                expected_max_logical_fill: 100
            }
        );
        assert_eq!(
            plan_format(4 << 40, 6 << 40, 100).unwrap().segment_bytes,
            32 * MIB
        );
        // The table can address 12 TiB at 32 MiB, but the tiny logical volume's default heap
        // cannot hold the segment state and the daemon working set. Extra raw bytes do not help.
        assert!(plan_format(128 * MIB, 12 << 40, 100).is_err());
        // More physical segments than representable even at the largest candidate.
        assert!(plan_format(4 << 40, 16 << 40, 1).is_err());
    }
}
