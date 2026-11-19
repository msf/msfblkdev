//! Test-only rendezvous: let the guest insert blkdebug after B's records sync but
//! before its descriptor write. Neither the hook nor the runner emulates an I/O result.
use super::{CheckpointDescriptor, Log, io};
use crate::format_v4::{geometry::TABLE_ENTRIES, segment_table::SegmentState};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

static VISITED: AtomicBool = AtomicBool::new(false);

pub(super) fn arm(log: &Log, descriptor: &CheckpointDescriptor) -> io::Result<bool> {
    if crate::configured_failpoint() != Some("live-checkpoint-b")
        || VISITED.load(Ordering::Relaxed)
        || log.segments.published_free_transitions() == 0
    {
        return Ok(false);
    }
    VISITED.store(true, Ordering::Relaxed);
    let path = std::env::var_os("ADR05_GATE_SOCKET")
        .ok_or_else(|| io::Error::other("missing gate socket"))?;
    let mut gate = UnixStream::connect(path)?;
    let timeout = Some(Duration::from_secs(8));
    gate.set_read_timeout(timeout)?;
    gate.set_write_timeout(timeout)?;
    writeln!(
        gate,
        "slot={} lsn={}",
        descriptor.slot.block(),
        descriptor.checkpoint_lsn
    )?;
    for table in 0..log.geometry.table_pages {
        let page = log.segments.table_page_image(table)?;
        for i in 0..log.geometry.table_children(table) {
            let segment = table * TABLE_ENTRIES as u32 + i as u32;
            if log.segments.state(segment) == SegmentState::PendingFree
                && page.entries[i].state == SegmentState::Free
            {
                writeln!(gate, "victim={segment}")?;
            }
        }
    }
    gate.shutdown(std::net::Shutdown::Write)?;
    let mut resume = [0];
    gate.read_exact(&mut resume)?;
    if resume != *b"G" {
        return Err(io::Error::other("invalid gate response"));
    }
    Ok(true)
}

pub(super) fn report(armed: bool, op: &str, result: &io::Result<()>) {
    if armed {
        let errno = result.as_ref().err().map_or(Some(0), |e| e.raw_os_error());
        eprintln!("live-checkpoint-b op={op} errno={errno:?}");
    }
}
