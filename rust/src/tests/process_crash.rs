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
        backing.create_sized(u64::from(layout.log_start + 4) * BLOCK_SIZE as u64)?;
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

#[test]
fn buffered_write_is_volatile_until_flush_across_sigkill() -> io::Result<()> {
    const BUFFER_MODE_ENV: &str = "BLOCK_STORAGE_BUFFER_CRASH_MODE";
    const FLUSHED_HANDSHAKE: &str = "buffered-flushed";
    const UNFLUSHED_HANDSHAKE: &str = "buffered-unflushed";
    let new = [0xa5; BLOCK_SIZE];
    // A 64 KiB buffer clamps to 16 slots here, so one write stays buffered until flush.
    let options = VolumeOpenOptions {
        write_buffer_bytes: 64 * 1024,
        ..VolumeOpenOptions::default()
    };

    if let Some(backing) = std::env::var_os(CRASH_TEST_BACKING) {
        let flush_write = std::env::var_os(BUFFER_MODE_ENV).as_deref() == Some(OsStr::new("flush"));
        let mut volume = open_with_options(PathBuf::from(backing), options)?;
        volume.write_block(0, &new)?;
        if flush_write {
            volume.flush()?;
            println!("{FLUSHED_HANDSHAKE}");
        } else {
            println!("{UNFLUSHED_HANDSHAKE}");
        }
        io::stdout().flush()?;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    let test_name = "tests::process_crash::buffered_write_is_volatile_until_flush_across_sigkill";
    repeat_process_crash_scenario(&configured_crash_test_plan(1)?, || {
        // Flushed: the write is durable and survives SIGKILL.
        let backing = TemporaryBacking::new()?;
        let (_, layout) = layout_for(4 * BLOCK_SIZE as u64)?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, 4 * BLOCK_SIZE as u64)?;
        run_buffer_crash_child(test_name, &backing.0, FLUSHED_HANDSHAKE, Some("flush"))?;
        let mut volume = open_with_options(&backing.0, options)?;
        let mut actual = [0; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(actual, new, "flushed write must survive SIGKILL");
        volume.close()?;

        // Unflushed: the buffered write is volatile and is lost.
        let backing = TemporaryBacking::new()?;
        backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
        format(&backing.0, 4 * BLOCK_SIZE as u64)?;
        run_buffer_crash_child(test_name, &backing.0, UNFLUSHED_HANDSHAKE, None)?;
        let mut volume = open_with_options(&backing.0, options)?;
        let mut actual = [0xff; BLOCK_SIZE];
        volume.read_block(0, &mut actual)?;
        assert_eq!(
            actual, [0; BLOCK_SIZE],
            "unflushed buffered write must be lost"
        );
        volume.close()
    })
}

fn run_buffer_crash_child(
    test_name: &str,
    backing: &Path,
    handshake: &str,
    mode: Option<&str>,
) -> io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(CRASH_TEST_BACKING, backing)
        .stdout(Stdio::piped());
    if let Some(mode) = mode {
        command.env("BLOCK_STORAGE_BUFFER_CRASH_MODE", mode);
    }
    let mut child = command.spawn()?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_and_reap(&mut child)?;
            return Err(io::Error::other("buffer crash child stdout unavailable"));
        }
    };
    let expected = handshake.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let result = BufReader::new(stdout)
            .lines()
            .find_map(|line| match line {
                Ok(line) if line == expected => Some(Ok(())),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .unwrap_or_else(|| Err(io::Error::other("child exited before handshake")));
        sender.send(result)
    });
    let handshake_result = match receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(io::Error::other(
            "timed out waiting for buffer crash handshake",
        )),
    };
    let status = kill_and_reap(&mut child)?;
    reader
        .join()
        .map_err(|_| io::Error::other("buffer crash reader panicked"))?
        .map_err(|_| io::Error::other("buffer crash handshake receiver disconnected"))?;
    handshake_result?;
    if status.signal() != Some(libc::SIGKILL) {
        return Err(io::Error::other(
            "buffer crash child was not killed by SIGKILL",
        ));
    }
    Ok(())
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
    backing.create_sized(u64::from(layout.log_start + 8) * BLOCK_SIZE as u64)?;
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
    let volume_blocks = 336_u32 * 1024 + 1;
    let (_, layout) = layout_for(u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    assert!(layout.index_blocks > 1);
    backing.create_sized(u64::from(layout.log_start + 64) * BLOCK_SIZE as u64)?;
    format(&backing.0, u64::from(volume_blocks) * BLOCK_SIZE as u64)?;
    let mut volume = open(&backing.0)?;
    volume.write_block(0, &[0xa5; BLOCK_SIZE])?;
    volume.close()?;

    sigkill_child_at_handshake(test_name, &backing.0, failpoint, Some(failpoint))?;

    let expected = [[0x5a; BLOCK_SIZE], [0xe7; BLOCK_SIZE], [0x3c; BLOCK_SIZE]];
    let mut volume = open(&backing.0)?;
    assert_eq!(
        volume.mapping.checkpoint.descriptor.checkpoint_lsn,
        expected_checkpoint_lsn
    );
    assert_eq!(volume.mapping.tail.lsn, 7);
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
    let (_, layout) = layout_for((336 * 1024 + 1) * BLOCK_SIZE as u64)?;
    let body_blocks = layout.index_blocks;
    let plan = configured_crash_test_plan(u64::from(body_blocks))?;
    for block_index in plan.boundary_indices {
        let failpoint = format!("{CHECKPOINT_BODY_BLOCK_COMPLETE_FAILPOINT}-{block_index}");
        for _ in 0..plan.repetitions {
            recover_after_checkpoint_crash(
                "tests::process_crash::recover_after_each_checkpoint_body_block_boundary",
                &failpoint,
                2,
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
            2,
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
            7,
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
            7,
        )?;
    }
    Ok(())
}
