//! A counting wake-up descriptor (`eventfd`) shared between the cleaner thread and the writer.
//! The writer polls it from its request loop; the cleaner bumps it after every report.
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

pub(crate) struct Wakeup(OwnedFd);

impl Wakeup {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes no pointers; the descriptor it returns is owned right away.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: eventfd just returned this descriptor and nothing else owns it.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// Adds one to the counter; a poller sees the descriptor readable.
    pub(crate) fn signal(&self) {
        let one = 1_u64.to_ne_bytes();
        // SAFETY: the descriptor is open for the value's lifetime and the buffer is the eight
        // bytes an eventfd write requires.
        let _ = unsafe { libc::write(self.0.as_raw_fd(), one.as_ptr().cast(), 8) };
    }

    /// Resets the counter so the next poll reports only signals sent after this point.
    pub(crate) fn drain(&self) {
        let mut value = [0_u8; 8];
        // SAFETY: the descriptor is open for the value's lifetime and the buffer holds the
        // eight bytes an eventfd read requires; EAGAIN on an empty counter is expected.
        let _ = unsafe { libc::read(self.0.as_raw_fd(), value.as_mut_ptr().cast(), 8) };
    }
}
