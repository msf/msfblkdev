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

    pub(super) fn record(
        &mut self,
        op_flags: u32,
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
                "request-count op_flags={op_flags:#x} count={} before_ready={} errors={} cancelled={}",
                count.requests, count.before_ready, count.errors, count.cancelled
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
    fn counts_each_flag_combination_including_discovery_and_unknown_requests() {
        let mut counts = RequestCounts::new();
        counts.record(0x700, true, 4096, false);
        counts.record(0x700, false, -libc::ESHUTDOWN, true);
        counts.record(0x801, false, 4096, false);
        counts.record(0x8000_0000, true, -95, false);
        let mut output = Vec::new();
        counts.report(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "request-count op_flags=0x700 count=2 before_ready=1 errors=0 cancelled=1\nrequest-count op_flags=0x801 count=1 before_ready=0 errors=0 cancelled=0\nrequest-count op_flags=0x80000000 count=1 before_ready=1 errors=1 cancelled=0\nrequest-count-end combinations=3 total=4 complete=true\n"
        );
    }

    #[test]
    fn capacity_and_counter_overflow_cannot_report_complete_evidence() {
        let mut counts = RequestCounts::new();
        for flags in 0..MAX_COMBINATIONS as u32 {
            counts.record(flags, true, 0, false);
        }
        counts.record(0, false, 0, false);
        assert!(counts.complete);
        counts.record(u32::MAX, false, 0, false);
        assert_eq!(counts.counts.len(), MAX_COMBINATIONS);
        let mut output = Vec::new();
        assert!(counts.report(&mut output).is_err());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with("complete=false\n")
        );
        let mut counts = RequestCounts::new();
        counts.record(0, false, 0, false);
        counts.counts.get_mut(&0).unwrap().requests = u64::MAX;
        counts.record(0, true, 0, false);
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
