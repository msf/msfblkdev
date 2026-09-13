use std::io;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BlockCounters {
    pub(crate) reads: u64,
    pub(crate) writes: u64,
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
    // Linux reports 512-byte sectors. This lab validates 4096-byte logical blocks
    // and a 4096-byte maximum request. Each successful data request is eight sectors.
    // The completed-write counter also includes zero-data flush I/O, so it is not
    // the count of data requests delivered to ublk.
    let requests = |sectors: u64| -> io::Result<u64> {
        if !sectors.is_multiple_of(8) {
            return Err(io::Error::other(
                "sector count is not whole 4096-byte requests",
            ));
        }
        Ok(sectors / 8)
    };
    Ok(BlockCounters {
        reads: requests(number(2)?)?,
        writes: requests(number(6)?)?,
        flushes: number(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_data_sectors_instead_of_flush_inflated_write_completions() {
        // Captured from the first SQLite probe; the daemon counted 25607 writes.
        let stat = "1355 0 10840 31 28389 0 204856 1571 0 1384 1641 0 0 0 0 5558 38";
        assert_eq!(
            parse_block_stat(stat).unwrap(),
            BlockCounters {
                reads: 1355,
                writes: 25607,
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
