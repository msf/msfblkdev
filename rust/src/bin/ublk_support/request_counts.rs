use std::collections::BTreeMap;
use std::io::{self, Write};

// The adapter accepts 19 operation/hint combinations; leave room to diagnose unknown ones.
const MAX_COMBINATIONS: usize = 64;

#[derive(Default)]
struct Count {
    requests: u64,
    before_ready: u64,
    errors: u64,
    cancelled: u64,
    sectors: u64,
}

/// Request and byte totals decoded per operation, so daemon evidence can be
/// compared directly against the kernel's `/sys/block/<device>/stat` counters.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Totals {
    pub(super) reads: u64,
    pub(super) writes: u64,
    pub(super) flushes: u64,
    pub(super) other: u64,
    pub(super) read_sectors: u64,
    pub(super) write_sectors: u64,
}

impl Totals {
    pub(super) fn summary(&self) -> String {
        format!(
            "reads={} writes={} flushes={} other={} read_sectors={} write_sectors={} read_bytes={} write_bytes={}",
            self.reads,
            self.writes,
            self.flushes,
            self.other,
            self.read_sectors,
            self.write_sectors,
            self.read_sectors * 512,
            self.write_sectors * 512,
        )
    }
}

pub(super) struct RequestCounts {
    counts: BTreeMap<u32, Count>,
    complete: bool,
}

impl RequestCounts {
    pub(super) fn new() -> Self {
        Self {
            counts: BTreeMap::new(),
            complete: true,
        }
    }

    /// Totals exclude cancellations, which the kernel never saw completed.
    pub(super) fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for (op_flags, count) in &self.counts {
            let completed = count.requests.saturating_sub(count.cancelled);
            match op_flags & 0xff {
                libublk::sys::UBLK_IO_OP_READ => {
                    totals.reads += completed;
                    totals.read_sectors += count.sectors;
                }
                libublk::sys::UBLK_IO_OP_WRITE => {
                    totals.writes += completed;
                    totals.write_sectors += count.sectors;
                }
                libublk::sys::UBLK_IO_OP_FLUSH => totals.flushes += completed,
                _ => totals.other += completed,
            }
        }
        totals
    }

    pub(super) fn record(
        &mut self,
        op_flags: u32,
        nr_sectors: u32,
        before_ready: bool,
        completion: i32,
        cancelled: bool,
    ) {
        assert!(
            !cancelled || completion == -libc::ESHUTDOWN,
            "only drain-rejected requests are cancellations"
        );
        if self.counts.len() == MAX_COMBINATIONS && !self.counts.contains_key(&op_flags) {
            self.complete = false;
            return;
        }
        let count = self.counts.entry(op_flags).or_default();
        let Some(next) = count.requests.checked_add(1) else {
            self.complete = false;
            return;
        };
        count.requests = next;
        count.before_ready += u64::from(before_ready);
        if !cancelled {
            count.sectors += u64::from(nr_sectors);
        }
        if completion < 0 && count.errors == 0 && count.cancelled == 0 {
            eprintln!(
                "first negative completion: op_flags={op_flags:#x} errno={completion} cancelled={cancelled}"
            );
        }
        count.errors += u64::from(completion < 0 && !cancelled);
        count.cancelled += u64::from(cancelled);
    }

    pub(super) fn report(&self, output: &mut impl Write) -> io::Result<()> {
        let mut total = 0_u64;
        for (op_flags, count) in &self.counts {
            writeln!(
                output,
                "request-count op_flags={op_flags:#x} count={} before_ready={} errors={} cancelled={} sectors={}",
                count.requests, count.before_ready, count.errors, count.cancelled, count.sectors
            )?;
            total = total
                .checked_add(count.requests)
                .ok_or_else(|| io::Error::other("request evidence total overflow"))?;
        }
        writeln!(
            output,
            "request-count-end combinations={} total={total} complete={}",
            self.counts.len(),
            self.complete
        )?;
        output.flush()?;
        if !self.complete {
            return Err(io::Error::other("request evidence capacity exceeded"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_decode_operations_and_exclude_cancelled_requests() {
        let mut counts = RequestCounts::new();
        counts.record(0x700, 8, false, 4096, false);
        counts.record(0x700, 256, false, 131_072, false);
        counts.record(0x801, 32, false, 16384, false);
        counts.record(0x1, 8, false, -libc::ESHUTDOWN, true);
        counts.record(0x2, 0, false, 0, false);
        // Opcode 3 is DISCARD, which the adapter does not support.
        counts.record(0x3, 0, false, -95, false);
        assert_eq!(
            counts.totals(),
            Totals {
                reads: 2,
                writes: 1,
                flushes: 1,
                other: 1,
                read_sectors: 264,
                write_sectors: 32,
            }
        );
    }

    #[test]
    fn counts_each_flag_combination_including_discovery_and_unknown_requests() {
        let mut counts = RequestCounts::new();
        counts.record(0x700, 8, true, 4096, false);
        counts.record(0x700, 8, false, -libc::ESHUTDOWN, true);
        counts.record(0x801, 8, false, 4096, false);
        counts.record(0x8000_0000, 0, true, -95, false);
        let mut output = Vec::new();
        counts.report(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "request-count op_flags=0x700 count=2 before_ready=1 errors=0 cancelled=1 sectors=8\nrequest-count op_flags=0x801 count=1 before_ready=0 errors=0 cancelled=0 sectors=8\nrequest-count op_flags=0x80000000 count=1 before_ready=1 errors=1 cancelled=0 sectors=0\nrequest-count-end combinations=3 total=4 complete=true\n"
        );
    }

    #[test]
    fn capacity_and_counter_overflow_cannot_report_complete_evidence() {
        let mut counts = RequestCounts::new();
        for flags in 0..MAX_COMBINATIONS as u32 {
            counts.record(flags, 0, true, 0, false);
        }
        counts.record(0, 0, false, 0, false);
        assert!(counts.complete);
        counts.record(u32::MAX, 0, false, 0, false);
        assert_eq!(counts.counts.len(), MAX_COMBINATIONS);
        let mut output = Vec::new();
        assert!(counts.report(&mut output).is_err());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with("complete=false\n")
        );
        let mut counts = RequestCounts::new();
        counts.record(0, 0, false, 0, false);
        counts.counts.get_mut(&0).unwrap().requests = u64::MAX;
        counts.record(0, 0, true, 0, false);
        assert!(!counts.complete);
    }

    #[test]
    fn report_propagates_output_failures() {
        let counts = RequestCounts::new();
        assert!(
            counts
                .report(&mut io::Cursor::new(&mut [0_u8; 1][..]))
                .is_err()
        );
    }
}
