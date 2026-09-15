use std::io;

/// Kernel block I/O counters for one ublk device, in the units Linux reports.
///
/// Transfers are counted in 512-byte sectors rather than requests: a wide
/// request path merges many logical blocks into one request, so request counts
/// are not comparable between the kernel and the daemon, while sectors are.
/// The kernel's completed-write counter also includes zero-data flush I/O, so
/// it is never used here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BlockCounters {
    pub(crate) read_sectors: u64,
    pub(crate) write_sectors: u64,
    pub(crate) flushes: u64,
}

pub(crate) fn parse_block_stat(text: &str) -> io::Result<BlockCounters> {
    let fields: Vec<_> = text.split_whitespace().take(18).collect();
    if fields.len() != 17 {
        return Err(io::Error::other("expected 17 Linux block-stat fields"));
    }
    let number = |index: usize| -> io::Result<u64> {
        fields
            .get(index)
            .ok_or_else(|| io::Error::other("missing block-stat field"))?
            .parse()
            .map_err(io::Error::other)
    };
    // The device advertises 4096-byte logical blocks, so every transfer is a
    // whole multiple of eight 512-byte sectors regardless of request width.
    let sectors = |sectors: u64| -> io::Result<u64> {
        if !sectors.is_multiple_of(8) {
            return Err(io::Error::other(
                "sector count is not whole 4096-byte blocks",
            ));
        }
        Ok(sectors)
    };
    Ok(BlockCounters {
        read_sectors: sectors(number(2)?)?,
        write_sectors: sectors(number(6)?)?,
        flushes: number(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_data_sectors_instead_of_flush_inflated_write_completions() {
        // Captured from the first SQLite probe. Field 5 reports 28389 completed
        // writes because it includes flush I/O; the 204856 data sectors are the
        // comparable quantity.
        let stat = "1355 0 10840 31 28389 0 204856 1571 0 1384 1641 0 0 0 0 5558 38";
        assert_eq!(
            parse_block_stat(stat).unwrap(),
            BlockCounters {
                read_sectors: 10840,
                write_sectors: 204856,
                flushes: 5558
            }
        );
    }

    #[test]
    fn rejects_incomplete_or_non_block_aligned_counters() {
        assert!(parse_block_stat("1 2 3").is_err());
        let stat = "0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_block_stat(stat).unwrap(), BlockCounters::default());
        assert!(parse_block_stat(&stat.replacen("0 0 0", "0 0 1", 1)).is_err());
        assert!(parse_block_stat(&format!("{stat} 0")).is_err());
        assert!(parse_block_stat(&stat.replacen('0', "bad", 3)).is_err());
    }
}
