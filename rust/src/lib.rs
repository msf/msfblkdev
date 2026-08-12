#[cfg(not(target_os = "linux"))]
compile_error!("block-storage requires Linux");

#[cfg(test)]
mod tests {
    use io_uring::{IoUring, opcode, squeue, types};
    use std::fs::{File, OpenOptions, remove_file};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    const BLOCK_SIZE: usize = 4096;

    #[repr(align(4096))]
    struct AlignedBlock([u8; BLOCK_SIZE]);

    struct TemporaryBacking(PathBuf);

    impl TemporaryBacking {
        fn new() -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            Ok(Self(std::env::temp_dir().join(format!(
                "block-storage-{}-{nonce}",
                std::process::id()
            ))))
        }

        fn open_new(&self) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_DIRECT)
                .open(&self.0)
        }

        fn reopen(&self) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(&self.0)
        }
    }

    impl Drop for TemporaryBacking {
        fn drop(&mut self) {
            // Test cleanup is best-effort because Drop cannot report errors.
            let _ = remove_file(&self.0);
        }
    }

    fn submit_exact(
        ring: &mut IoUring,
        entry: squeue::Entry,
        user_data: u64,
        expected_result: i32,
    ) -> io::Result<()> {
        // SAFETY: each caller keeps operation buffers and the file alive until this waits for the CQE.
        unsafe {
            ring.submission()
                .push(&entry.user_data(user_data))
                .map_err(|_| io::Error::other("io_uring submission queue is full"))?;
        }
        if ring.submit_and_wait(1)? != 1 {
            return Err(io::Error::other("unexpected io_uring submission count"));
        }

        let mut completions = ring.completion();
        let completion = completions
            .next()
            .ok_or_else(|| io::Error::other("missing io_uring completion"))?;
        if completion.user_data() != user_data || completion.flags() != 0 {
            return Err(io::Error::other("unexpected io_uring completion"));
        }
        if completion.result() < 0 {
            return Err(io::Error::from_raw_os_error(-completion.result()));
        }
        if completion.result() != expected_result {
            return Err(io::Error::other("short io_uring operation"));
        }
        if completions.next().is_some() {
            return Err(io::Error::other("unexpected extra io_uring completion"));
        }
        Ok(())
    }

    fn write_exact(ring: &mut IoUring, file: &File, block: &AlignedBlock) -> io::Result<()> {
        let entry = opcode::Write::new(
            types::Fd(file.as_raw_fd()),
            block.0.as_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(0)
        .build();
        submit_exact(ring, entry, 1, BLOCK_SIZE as i32)
    }

    fn fsync(ring: &mut IoUring, file: &File) -> io::Result<()> {
        let entry = opcode::Fsync::new(types::Fd(file.as_raw_fd())).build();
        submit_exact(ring, entry, 2, 0)
    }

    fn read_exact(ring: &mut IoUring, file: &File, block: &mut AlignedBlock) -> io::Result<()> {
        let entry = opcode::Read::new(
            types::Fd(file.as_raw_fd()),
            block.0.as_mut_ptr(),
            BLOCK_SIZE as u32,
        )
        .offset(0)
        .build();
        submit_exact(ring, entry, 3, BLOCK_SIZE as i32)
    }

    #[test]
    fn direct_io_uring_write_survives_fsync_and_reopen() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        let mut ring = IoUring::new(4)?;
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
}
