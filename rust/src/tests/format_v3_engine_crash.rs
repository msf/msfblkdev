use super::*;
use crate::format_v3::{storage, volume};

fn fixture() -> io::Result<TemporaryBacking> {
    let backing = TemporaryBacking::new()?;
    backing.create_sized(1024 * BLOCK_SIZE as u64)?;
    storage::format(&backing.0, (336 * 1024 + 1) * BLOCK_SIZE as u64, None)?;
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    volume.write_block(0, &[1; BLOCK_SIZE])?;
    volume.close()?;
    Ok(backing)
}

fn buffered_options() -> VolumeOpenOptions {
    VolumeOpenOptions {
        write_buffer_bytes: 4 * BLOCK_SIZE as u64,
        ..VolumeOpenOptions::default()
    }
}

fn verify(backing: &TemporaryBacking, expected: [(u32, u8); 3]) -> io::Result<()> {
    let mut volume = volume::open(&backing.0, VolumeOpenOptions::default(), Some(1), None)?;
    let mut bytes = [0; BLOCK_SIZE];
    for (lba, byte) in expected {
        volume.read_block(lba, &mut bytes)?;
        assert_eq!(bytes, [byte; BLOCK_SIZE], "LBA {lba}");
    }
    volume.close()
}

#[test]
fn paged_grouped_release_survives_foreground_and_pressure_crashes() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume =
            volume::open(&PathBuf::from(path), buffered_options(), Some(20_000), None)?;
        for (lba, byte) in [(0, 2), (336, 3), (672, 4), (0, 5)] {
            volume.write_block(lba, &[byte; BLOCK_SIZE])?;
        }
        volume.flush()?;
        volume.close()?;
        return Err(io::Error::other("foreground failpoint did not pause child"));
    }
    let boundaries = [
        RECORD_WRITE_COMPLETE_FAILPOINT,
        MAPPING_PUBLISHED_FAILPOINT,
        "mapping-page-write-before-0",
        "mapping-page-write-complete-0",
        LOG_FSYNC_COMPLETE_FAILPOINT,
        "checkpoint-body-block-complete-0",
        "checkpoint-body-block-complete-1",
        CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT,
        DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
        DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
    ];
    for boundary in boundaries {
        for _ in 0..configured_crash_test_plan(boundaries.len() as u64)?.repetitions {
            let backing = fixture()?;
            sigkill_child_at_handshake(
                "tests::format_v3_engine_crash::paged_grouped_release_survives_foreground_and_pressure_crashes",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            // Exact record completion survives process death; this is not a power-loss test.
            verify(&backing, [(0, 5), (336, 3), (672, 4)])?;
        }
    }
    Ok(())
}

#[test]
fn paged_buffer_is_volatile_and_flushed_groups_survive_sigkill() -> io::Result<()> {
    if let Some(path) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume =
            volume::open(&PathBuf::from(path), buffered_options(), Some(20_000), None)?;
        for (lba, byte) in [(0, 2), (336, 3), (672, 4), (0, 5)] {
            volume.write_block(lba, &[byte; BLOCK_SIZE])?;
        }
        pause_at_test_failpoint("paged-buffered-ready")?;
        volume.flush()?;
        pause_at_test_failpoint("paged-flushed-ready")?;
        return Err(io::Error::other("buffer failpoint did not pause child"));
    }
    for (boundary, expected) in [
        ("paged-buffered-ready", [(0, 1), (336, 0), (672, 0)]),
        ("paged-flushed-ready", [(0, 5), (336, 3), (672, 4)]),
    ] {
        for _ in 0..configured_crash_test_plan(2)?.repetitions {
            let backing = fixture()?;
            sigkill_child_at_handshake(
                "tests::format_v3_engine_crash::paged_buffer_is_volatile_and_flushed_groups_survive_sigkill",
                &backing.0,
                boundary,
                Some(boundary),
            )?;
            verify(&backing, expected)?;
        }
    }
    Ok(())
}
