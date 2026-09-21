//! The cleaner thread and the writer's scheduling: waiting without spinning, pressure that
//! blocks on a report, shutdown with a read outstanding, failure latching and scheduled ticks.
use super::*;
use crate::{MaintenanceStep, Scheduler, SelectionMode};
use std::time::{Duration, Instant};

const CLEANER_WAIT: Duration = Duration::from_secs(20);
const SEGMENTS: u64 = 12;

fn fixture(volume_blocks: u32, blocks_per_segment: u32) -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    format_segments(&backing, volume_blocks, SEGMENTS, blocks_per_segment)?;
    Ok(backing)
}

fn options(write_buffer_blocks: u64) -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: write_buffer_blocks * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    }
}

fn value(index: u32) -> [u8; BLOCK_SIZE] {
    let mut block = [0; BLOCK_SIZE];
    for chunk in block.chunks_exact_mut(4) {
        chunk.copy_from_slice(&index.to_le_bytes());
    }
    block
}

/// Seals segment 0 with two live payloads and publishes until it is an eligible victim.
fn eligible_victim(volume: &mut Volume) -> io::Result<()> {
    for lba in 0..3 {
        volume.write_block(lba, &value(lba + 1))?;
    }
    volume.write_block(2, &value(20))?;
    volume.flush()?;
    // Bounded: two barrier publications make the sealed segment eligible.
    for _ in 0..4 {
        match volume.maintain(SelectionMode::Idle)? {
            MaintenanceStep::Published { .. } => {}
            MaintenanceStep::Scanning => return Ok(()),
            other => return Err(io::Error::other(format!("unexpected step {other:?}"))),
        }
    }
    Err(io::Error::other("segment 0 never became a victim"))
}

/// Whether the wake-up descriptor is readable within `timeout`.
fn wakeup_readable(fd: i32, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(timeout.as_millis()).expect("timeout fits");
    // SAFETY: poll reads one initialized pollfd for the duration of the call.
    let ready = unsafe { libc::poll(&mut poll, 1, millis) };
    ready == 1 && poll.revents & libc::POLLIN != 0
}

#[test]
fn delayed_cleaner_reads_make_the_writer_wait_and_wake_it_through_the_descriptor() -> io::Result<()>
{
    let backing = fixture(8, TINY_SEGMENT_BLOCKS)?;
    let mut volume = open_with_options(&backing.0, options(1))?;
    volume.delay_cleaner(Duration::from_millis(300))?;
    let started = Instant::now();
    eligible_victim(&mut volume)?;
    // The window is with the cleaner: the writer reports Waiting at once and does no I/O.
    let before = volume.mapping_stats();
    for _ in 0..3 {
        assert_eq!(
            volume.maintain(SelectionMode::Idle)?,
            MaintenanceStep::Waiting
        );
    }
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "Waiting must not block"
    );
    assert_eq!(
        volume.mapping_stats().io,
        before.io,
        "no writer reads while waiting"
    );
    assert!(volume.cleaner_outstanding() >= 1);
    assert!(wakeup_readable(volume.cleaner_wakeup_fd(), CLEANER_WAIT));
    while volume.cleaner_outstanding() > 0 {
        assert!(volume.wait_for_cleaner(CLEANER_WAIT)?);
    }
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(volume.maintenance_ready());
    assert_eq!(
        volume.maintain(SelectionMode::Idle)?,
        MaintenanceStep::Applied
    );
    assert_eq!(volume.reclaim_stats().payloads_copied, 2);
    let mut bytes = [0; BLOCK_SIZE];
    volume.read_block(0, &mut bytes)?;
    assert_eq!(bytes, value(1));
    volume.close()
}

#[test]
fn pressure_blocks_on_the_delayed_report_instead_of_failing() -> io::Result<()> {
    let backing = fixture(64, 64)?;
    let mut volume = open_with_options(&backing.0, options(4))?;
    volume.delay_cleaner(Duration::from_millis(50))?;
    let capacity = volume.space_usage()?.log_capacity_bytes / BLOCK_SIZE as u64;
    // Three passes over the capacity force reclamation under pressure.
    for index in 1..=(3 * capacity as u32) {
        volume.write_block(index % 16, &value(index))?;
        if index % 5 == 0 {
            volume.flush()?;
        }
    }
    volume.flush()?;
    let stats = volume.reclaim_stats();
    assert!(stats.pressure_steps > 0, "{stats:?}");
    assert!(stats.pressure_waits > 0, "{stats:?}");
    assert_eq!(stats.enospc_all_live, 0, "{stats:?}");
    assert!(stats.segments_freed > 0, "{stats:?}");
    let mut bytes = [0; BLOCK_SIZE];
    for lba in 0..16_u32 {
        volume.read_block(lba, &mut bytes)?;
        let last = (1..=(3 * capacity as u32))
            .rev()
            .find(|index| index % 16 == lba);
        assert_eq!(
            bytes,
            value(last.expect("every LBA was written")),
            "LBA {lba}"
        );
    }
    volume.close()
}

#[test]
fn close_with_a_window_read_outstanding_joins_the_cleaner_and_keeps_every_block() -> io::Result<()>
{
    let backing = fixture(8, TINY_SEGMENT_BLOCKS)?;
    let mut volume = open_with_options(&backing.0, options(1))?;
    volume.delay_cleaner(Duration::from_millis(400))?;
    eligible_victim(&mut volume)?;
    assert_eq!(volume.cleaner_outstanding(), 2, "view swap and one window");
    let started = Instant::now();
    volume.close()?;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(300),
        "close joined the read: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "close is bounded: {elapsed:?}"
    );
    let mut reopened = open_with_options(&backing.0, options(1))?;
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, expected) in [(0, 1), (1, 2), (2, 20)] {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, value(expected));
    }
    reopened.log.segments.check_conservation()?;
    reopened.close()
}

#[test]
fn a_failed_cleaner_read_latches_the_volume_and_leaves_the_backing_consistent() -> io::Result<()> {
    let backing = fixture(8, TINY_SEGMENT_BLOCKS)?;
    let mut volume = open_with_options(&backing.0, options(1))?;
    volume.fail_next_cleaner_scan()?;
    eligible_victim(&mut volume)?;
    while volume.cleaner_outstanding() > 0 {
        match volume.wait_for_cleaner(CLEANER_WAIT) {
            Ok(true) => {}
            Ok(false) => return Err(io::Error::other("cleaner did not report")),
            Err(error) => {
                assert!(error.to_string().contains("injected"), "{error}");
                break;
            }
        }
    }
    assert!(volume.is_failed());
    assert!(volume.write_block(0, &value(9)).is_err());
    assert!(volume.maintain(SelectionMode::Idle).is_err());
    drop(volume);
    let mut reopened = open_with_options(&backing.0, options(1))?;
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, expected) in [(0, 1), (1, 2), (2, 20)] {
        reopened.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, value(expected));
    }
    reopened.close()
}

#[test]
fn scheduled_idle_and_due_ticks_reclaim_ahead_of_pressure() -> io::Result<()> {
    let idle_wait = Duration::from_millis(2);
    let maximum_interval = Duration::from_millis(100);
    let mut pressure_steps = [0_u64; 2];
    for (run, ticks) in [false, true].into_iter().enumerate() {
        let backing = fixture(64, 64)?;
        let mut volume = open_with_options(&backing.0, options(4))?;
        let capacity = volume.space_usage()?.log_capacity_bytes / BLOCK_SIZE as u64;
        let mut now = Instant::now();
        let mut scheduler = Scheduler::new(now, idle_wait, maximum_interval);
        let mut idle_ticks = 0;
        let mut due_ticks = 0;
        for index in 1..=(3 * capacity as u32) {
            volume.write_block(index % 16, &value(index))?;
            now += Duration::from_micros(200);
            scheduler.request_completed(now);
            if index % 5 == 0 {
                volume.flush()?;
            }
            // Every 700 requests (140 ms of continuous traffic, past the maximum interval)
            // the client pauses longer than the idle wait.
            if index % 700 == 0 {
                now += idle_wait;
            }
            if ticks && (scheduler.idle(now) || scheduler.maintenance_due(now)) {
                if scheduler.idle(now) {
                    idle_ticks += 1;
                } else {
                    due_ticks += 1;
                }
                volume.maintain_for(SelectionMode::Idle, idle_wait)?;
                scheduler.maintained(now);
            }
        }
        volume.flush()?;
        if ticks {
            assert!(idle_ticks > 0, "idle ticks ran");
            assert!(due_ticks > 0, "due ticks ran under continuous requests");
            assert!(scheduler.maintenance_runs() == idle_ticks + due_ticks);
        }
        pressure_steps[run] = volume.reclaim_stats().pressure_steps;
        assert_eq!(volume.reclaim_stats().enospc_all_live, 0);
        volume.close()?;
    }
    assert!(
        pressure_steps[1] < pressure_steps[0],
        "scheduled maintenance reduces pressure work: {pressure_steps:?}"
    );
    Ok(())
}
