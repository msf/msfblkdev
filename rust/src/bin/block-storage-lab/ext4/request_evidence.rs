use libublk::sys::{UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE};
use std::collections::BTreeSet;
use std::io;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CompletedRequests {
    pub(super) reads: u64,
    pub(super) writes: u64,
    pub(super) flushes: u64,
    pub(super) read_sectors: u64,
    pub(super) write_sectors: u64,
}

impl CompletedRequests {
    fn record(&mut self, flags: u32, count: u64, sectors: u64) -> io::Result<()> {
        if count == 0 {
            return Ok(());
        }
        // Linux ublk stores the opcode in the low byte of op_flags. Requests are
        // counted for evidence, and sectors are what the kernel sample can be
        // compared against, because one request carries many logical blocks.
        let (requests, transferred) = match flags & 0xff {
            UBLK_IO_OP_READ => (&mut self.reads, Some(&mut self.read_sectors)),
            UBLK_IO_OP_WRITE => (&mut self.writes, Some(&mut self.write_sectors)),
            UBLK_IO_OP_FLUSH => (&mut self.flushes, None),
            _ => return Err(invalid()),
        };
        *requests = requests.checked_add(count).ok_or_else(invalid)?;
        if let Some(transferred) = transferred {
            *transferred = transferred.checked_add(sectors).ok_or_else(invalid)?;
        } else if sectors != 0 {
            return Err(invalid());
        }
        Ok(())
    }
}

// The caller captures at most MAX_CAPTURE_BYTES from the owned daemon's stderr.
pub(super) fn validate(text: &str) -> io::Result<CompletedRequests> {
    let mut completed = CompletedRequests::default();
    let mut combinations = BTreeSet::new();
    let mut total = 0_u64;
    let mut ended = false;
    for line in text.lines() {
        if !line.starts_with("request-count ") && !line.starts_with("request-count-end ") {
            continue;
        }
        if ended {
            return Err(invalid());
        }
        // One field beyond the longest accepted record, so a longer line fails
        // to match instead of being silently truncated.
        let fields: Vec<_> = line.split_whitespace().take(8).collect();
        if let ["request-count-end", first, second, third] = fields.as_slice() {
            if combinations.is_empty()
                || number(first, "combinations=")? != combinations.len() as u64
                || number(second, "total=")? != total
                || *third != "complete=true"
            {
                return Err(invalid());
            }
            ended = true;
            continue;
        }
        let [
            "request-count",
            first,
            second,
            third,
            errors,
            cancelled,
            transferred,
        ] = fields.as_slice()
        else {
            return Err(invalid());
        };
        let flags = u32::from_str_radix(first.strip_prefix("op_flags=0x").ok_or_else(invalid)?, 16)
            .map_err(|_| invalid())?;
        let count = number(second, "count=")?;
        let before_ready = number(third, "before_ready=")?;
        let cancellations = number(cancelled, "cancelled=")?;
        let sectors = number(transferred, "sectors=")?;
        if count == 0
            || before_ready > count
            || number(errors, "errors=")? != 0
            || cancellations > count
            || !combinations.insert(flags)
        {
            return Err(invalid());
        }
        completed.record(flags, count - cancellations, sectors)?;
        total = total.checked_add(count).ok_or_else(invalid)?;
    }
    if !ended {
        return Err(invalid());
    }
    Ok(completed)
}

fn number(field: &str, prefix: &str) -> io::Result<u64> {
    field
        .strip_prefix(prefix)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())
}

fn invalid() -> io::Error {
    io::Error::other("daemon request/flag evidence is missing, incomplete or inconsistent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_include_hints_but_exclude_shutdown_cancellations() {
        let report = "request-count op_flags=0x700 count=5 before_ready=1 errors=0 cancelled=2 sectors=24\n\
                      request-count op_flags=0x801 count=7 before_ready=0 errors=0 cancelled=0 sectors=1792\n\
                      request-count op_flags=0x2 count=2 before_ready=0 errors=0 cancelled=0 sectors=0\n\
                      request-count-end combinations=3 total=14 complete=true\n";
        assert_eq!(
            validate(report).unwrap(),
            CompletedRequests {
                reads: 3,
                writes: 7,
                flushes: 2,
                read_sectors: 24,
                write_sectors: 1792,
            }
        );
        assert!(validate(&report.replace("0x801", "0x3")).is_err());
        // A flush transfers nothing, so a non-zero sector count is inconsistent.
        assert!(
            validate(&report.replace("cancelled=0 sectors=0", "cancelled=0 sectors=8")).is_err()
        );
    }

    #[test]
    fn requires_complete_consistent_request_and_discovery_counts() {
        let row =
            "request-count op_flags=0x700 count=2 before_ready=1 errors=0 cancelled=0 sectors=16\n";
        let end = "request-count-end combinations=1 total=2 complete=true\n";
        validate(&format!("{row}{end}")).unwrap();
        validate(&format!("{row}{end}").replace("cancelled=0", "cancelled=1")).unwrap();
        for text in [
            String::new(),
            row.into(),
            end.into(),
            format!("{row}{row}{end}"),
            format!("{end}{row}"),
            format!("{row}{end}{end}"),
            format!("{row}{end}").replace("complete=true", "complete=false"),
            format!("{row}{end}").replace("total=2", "total=3"),
            format!("{row}{end}").replace("combinations=1", "combinations=2"),
            format!("{row}{end}").replace("before_ready=1", "before_ready=3"),
            format!("{row}{end}").replace("count=2", "count=0"),
            format!("{row}{end}").replace("errors=0", "errors=1"),
            format!("{row}{end}").replace("cancelled=0", "cancelled=3"),
            format!("{row}{end}").replace(" errors=0", ""),
            format!("{row}{end}").replace(" sectors=16", ""),
            format!("{row}{end}").replace("sectors=16", "sectors=16 extra=1"),
            format!("{row}{end}").replace("0x700", "0x100000000"),
        ] {
            assert!(validate(&text).is_err(), "accepted invalid report: {text}");
        }
    }
}
