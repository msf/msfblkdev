use super::*;

#[test]
fn recover_one_flushed_write_after_sigkill_without_close() -> io::Result<()> {
    const HANDSHAKE: &str = "write-flushed";

    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open(PathBuf::from(backing))?;
        volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
        volume.flush()?;
        println!("{HANDSHAKE}");
        io::stdout().flush()?;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 2) * BLOCK_SIZE as u64)?;
        format(&backing.0, BLOCK_SIZE as u64)?;

        sigkill_child_at_handshake(
            "tests::process_crash::recover_one_flushed_write_after_sigkill_without_close",
            &backing.0,
            HANDSHAKE,
            None,
        )?;

        let mut volume = open(&backing.0)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, [0xa5; BLOCK_SIZE]);
        volume.close()
    })
}

#[test]
fn recover_multiple_flushed_writes_and_overwrites_after_sigkill() -> io::Result<()> {
    const HANDSHAKE: &str = "all-writes-flushed";

    let writes = [
        (0, 0xa5),
        (1, 0x5a),
        (0, 0xc3),
        (2, 0x3c),
        (0, 0xd4),
        (1, 0xe7),
    ];
    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open(PathBuf::from(backing))?;
        for (lba, byte) in writes {
            volume.write_block(lba, &[byte; BLOCK_SIZE])?;
        }
        volume.flush()?;
        println!("{HANDSHAKE}");
        io::stdout().flush()?;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 3_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        sigkill_child_at_handshake(
            "tests::process_crash::recover_multiple_flushed_writes_and_overwrites_after_sigkill",
            &backing.0,
            HANDSHAKE,
            None,
        )?;

        let expected = [[0xd4; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
        let mut volume = open(&backing.0)?;
        for (lba, expected) in expected.iter().enumerate() {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, expected);
        }
        volume.close()
    })
}

#[test]
fn recover_after_uncaught_unwinding_panic_without_close() -> io::Result<()> {
    let writes = [(0, 0xa5), (1, 0x5a), (0, 0xc3), (2, 0x3c), (1, 0xe7)];
    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open(PathBuf::from(backing))?;
        for (lba, byte) in writes {
            volume.write_block(lba, &[byte; BLOCK_SIZE])?;
        }
        volume.flush()?;
        panic!("intentional uncaught panic after flushing test values");
    }

    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        let backing = TemporaryBacking::new()?;
        let volume_blocks = 3_u32;
        let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;

        run_unwinding_panic_child(
            "tests::process_crash::recover_after_uncaught_unwinding_panic_without_close",
            &backing.0,
        )?;

        let expected = [[0xc3; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
        let mut volume = open(&backing.0)?;
        for (lba, expected) in expected.iter().enumerate() {
            let mut actual = [0; BLOCK_SIZE];
            volume.read_block(lba as u32, &mut actual)?;
            assert_eq!(&actual, expected);
        }
        volume.close()
    })
}

fn repeat_process_crash_scenario(
    plan: &CrashTestPlan,
    mut scenario: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    for _ in 0..plan.repetitions {
        scenario()?;
    }
    Ok(())
}

fn recover_after_write_crash(
    test_name: &str,
    failpoint: &str,
    flush_write: bool,
    require_new_state: bool,
) -> io::Result<()> {
    let old = [0xa5; BLOCK_SIZE];
    let new = [0x5a; BLOCK_SIZE];
    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open(PathBuf::from(backing))?;
        volume.write_block(0, &new)?;
        if flush_write {
            volume.flush()?;
        }
        return Err(io::Error::other("test failpoint did not pause child"));
    }

    let backing = TemporaryBacking::new()?;
    let (_, layout) = layout_for(BLOCK_SIZE as u64)?;
    backing.create_sized(u64::from(layout.log_start + 6) * BLOCK_SIZE as u64)?;
    format(&backing.0, BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &old)?;
    volume.close()?;

    sigkill_child_at_handshake(test_name, &backing.0, failpoint, Some(failpoint))?;

    let mut volume = open(&backing.0)?;
    let mut actual = [0; BLOCK_SIZE];
    volume.read_block(0, &mut actual)?;
    if require_new_state {
        assert_eq!(actual, new);
    } else {
        assert!(actual == old || actual == new);
    }
    volume.close()
}

#[test]
fn recover_after_kill_at_record_write_boundary() -> io::Result<()> {
    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        recover_after_write_crash(
            "tests::process_crash::recover_after_kill_at_record_write_boundary",
            RECORD_WRITE_COMPLETE_FAILPOINT,
            false,
            false,
        )
    })
}

#[test]
fn recover_after_kill_at_mapping_publication_boundary() -> io::Result<()> {
    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        recover_after_write_crash(
            "tests::process_crash::recover_after_kill_at_mapping_publication_boundary",
            MAPPING_PUBLISHED_FAILPOINT,
            false,
            false,
        )
    })
}

#[test]
fn recover_after_kill_at_log_fsync_boundary() -> io::Result<()> {
    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        recover_after_write_crash(
            "tests::process_crash::recover_after_kill_at_log_fsync_boundary",
            LOG_FSYNC_COMPLETE_FAILPOINT,
            true,
            true,
        )
    })
}

fn recover_after_checkpoint_crash(
    test_name: &str,
    failpoint: &str,
    expected_checkpoint_lsn: u64,
) -> io::Result<()> {
    let writes = [(0, 0x5a), (1, 0xc3), (2, 0x3c), (1, 0xe7)];
    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let mut volume = open(PathBuf::from(backing))?;
        for (lba, byte) in writes {
            volume.write_block(lba, &[byte; BLOCK_SIZE])?;
        }
        volume.flush()?;
        volume.close()?;
        return Err(io::Error::other("test failpoint did not pause child"));
    }

    let backing = TemporaryBacking::new()?;
    let volume_blocks = 1025_u32;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    assert!(layout.physical_map_blocks + layout.checksum_map_blocks > 1);
    backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.close()?;

    sigkill_child_at_handshake(test_name, &backing.0, failpoint, Some(failpoint))?;

    let expected = [[0x5a; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
    let mut volume = open(&backing.0)?;
    assert_eq!(volume.checkpoint_lsn, expected_checkpoint_lsn);
    assert_eq!(volume.last_lsn, 5);
    for (lba, expected) in expected.iter().enumerate() {
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(lba as u32, &mut actual)?;
        assert_eq!(&actual, expected);
    }
    Ok(())
}

#[test]
fn crash_test_modes_select_smoke_and_exhaustive_work() -> io::Result<()> {
    let repeated_scenarios = [
        "one-write SIGKILL",
        "multi-write SIGKILL",
        "uncaught panic",
        "record write boundary",
        "mapping publication boundary",
        "log fsync boundary",
    ];
    let smoke = crash_test_plan(None, 339)?;
    assert_eq!(smoke.boundary_indices, [0, 169, 338]);
    assert_eq!(smoke.repetitions, 1);

    let acceptance = crash_test_plan(Some(OsStr::new(ACCEPTANCE_CRASH_TEST_MODE)), 339)?;
    assert_eq!(acceptance.boundary_indices, (0..339).collect::<Vec<_>>());
    assert_eq!(acceptance.repetitions, 20);
    for scenario in repeated_scenarios {
        let mut runs = 0;
        repeat_process_crash_scenario(&acceptance, || {
            runs += 1;
            Ok(())
        })?;
        assert_eq!(runs, 20, "acceptance repetitions for {scenario}");
    }
    assert!(crash_test_plan(Some(OsStr::new("invalid")), 339).is_err());
    Ok(())
}

#[test]
fn recover_after_each_checkpoint_body_block_boundary() -> io::Result<()> {
    let (_, layout) = layout_for(1025 * BLOCK_SIZE as u64)?;
    let body_blocks = layout.physical_map_blocks + layout.checksum_map_blocks;
    let plan = configured_crash_test_plan(u64::from(body_blocks))?;
    for block_index in plan.boundary_indices {
        let failpoint = format!("{CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT}-{block_index}");
        for _ in 0..plan.repetitions {
            recover_after_checkpoint_crash(
                "tests::process_crash::recover_after_each_checkpoint_body_block_boundary",
                &failpoint,
                1,
            )?;
        }
    }
    Ok(())
}

#[test]
fn recover_after_checkpoint_body_fsync_boundary() -> io::Result<()> {
    for _ in 0..configured_crash_test_plan(1)?.repetitions {
        recover_after_checkpoint_crash(
            "tests::process_crash::recover_after_checkpoint_body_fsync_boundary",
            CHECKPOINT_BODY_FSYNC_COMPLETE_FAILPOINT,
            1,
        )?;
    }
    Ok(())
}

#[test]
fn recover_after_descriptor_write_boundary() -> io::Result<()> {
    for _ in 0..configured_crash_test_plan(1)?.repetitions {
        recover_after_checkpoint_crash(
            "tests::process_crash::recover_after_descriptor_write_boundary",
            DESCRIPTOR_WRITE_COMPLETE_FAILPOINT,
            5,
        )?;
    }
    Ok(())
}

#[test]
fn recover_after_descriptor_fsync_boundary() -> io::Result<()> {
    for _ in 0..configured_crash_test_plan(1)?.repetitions {
        recover_after_checkpoint_crash(
            "tests::process_crash::recover_after_descriptor_fsync_boundary",
            DESCRIPTOR_FSYNC_COMPLETE_FAILPOINT,
            5,
        )?;
    }
    Ok(())
}
