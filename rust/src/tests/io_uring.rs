use super::*;

#[test]
fn direct_io_uring_write_survives_fsync_and_reopen() -> io::Result<()> {
    let backing = TemporaryBacking::new()?;
    let mut ring = Some(IoUring::new(4)?);
    let written = AlignedBlock([0xa5; BLOCK_SIZE]);

    {
        let file = backing.open_new()?;
        write_exact(&mut ring, &file, &written)?;
        fsync(&mut ring, &file)?;
    }

    let mut read = AlignedBlock([0; BLOCK_SIZE]);
    {
        let file = backing.reopen()?;
        read_exact(&mut ring, &file, &mut read)?;
    }

    assert_eq!(written.0, read.0);
    Ok(())
}

#[test]
fn waits_when_submission_succeeds_before_completion_exists() -> io::Result<()> {
    let event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if event_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let event = unsafe { File::from_raw_fd(event_fd) };
    let mut ring = IoUring::new(2)?;
    let poll = opcode::PollAdd::new(types::Fd(event_fd), libc::POLLIN as u32)
        .build()
        .user_data(7);
    unsafe {
        ring.submission()
            .push(&poll)
            .map_err(|_| io::Error::other("io_uring submission queue is full"))?;
    }
    let mut ring = Some(ring);
    let mut calls = 0;
    let mut write_result = 0;
    let result = wait_exact_with(
        &mut ring,
        7,
        libc::POLLIN.into(),
        |ring| -> io::Result<usize> {
            calls += 1;
            if calls == 1 {
                assert_eq!(ring.submit()?, 1);
                return Ok(1);
            }
            let value = 1_u64;
            write_result =
                unsafe { libc::write(event_fd, (&raw const value).cast(), size_of::<u64>()) };
            assert_eq!(ring.submit_and_wait(1)?, 0);
            Ok(0)
        },
    );

    assert_eq!(calls, 2);
    assert_eq!(write_result, size_of::<u64>() as isize);
    drop(event);
    result
}

#[test]
fn destroys_ring_before_returning_permanent_wait_error() -> io::Result<()> {
    let mut ring = Some(IoUring::new(2)?);
    let error = wait_exact_with(&mut ring, 7, 0, |_| {
        Err(io::Error::from_raw_os_error(libc::EIO))
    })
    .unwrap_err();

    assert_eq!(error.raw_os_error(), Some(libc::EIO));
    assert!(ring.is_none());
    Ok(())
}
