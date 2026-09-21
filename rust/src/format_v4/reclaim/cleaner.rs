//! The cleaner thread. It owns a duplicate backing descriptor, its own io_uring and the scanner
//! scratch. Window buffers and views travel through bounded channels, so every payload byte,
//! view entry and cursor has exactly one owner at a time and no lock guards shared state.
use super::{
    ProposalBuffer, View,
    scanner::{ScanContext, ScanCursor, ScanReport, Scanner},
};
use crate::{BLOCK_SIZE, format_v4::Geometry, wakeup::Wakeup};
use std::{
    fs::File,
    io,
    os::fd::RawFd,
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    thread::JoinHandle,
    time::Duration,
};

/// Backing discards the writer may hand over before waiting for completions.
pub(crate) const DISCARD_SLOTS: usize = 4;

/// Work the writer hands to the cleaner. Each command carries what it needs and nothing shared.
pub(crate) enum Command {
    /// Read one window of `cursor`'s victim into `buffer` against the cleaner's current view.
    Scan {
        cursor: ScanCursor,
        buffer: ProposalBuffer,
    },
    /// Replace the cleaner's view; the old one comes back as `Event::ViewReturned`.
    Refresh(Box<View>),
    /// Punch a hole over a free segment.
    Discard {
        segment: u32,
    },
    /// Sleep before every scan (tests of delayed cleaner I/O).
    #[cfg(test)]
    Delay(Duration),
    /// Report the next scan as failed without reading (tests of cleaner failure).
    #[cfg(test)]
    FailNextScan,
    Shutdown,
}

impl Command {
    /// Whether the cleaner answers this command with an event.
    fn reports(&self) -> bool {
        match self {
            Command::Scan { .. } | Command::Refresh(_) | Command::Discard { .. } => true,
            #[cfg(test)]
            Command::Delay(_) | Command::FailNextScan => false,
            Command::Shutdown => false,
        }
    }
}

/// What the cleaner sends back. Every buffer and view the writer lent returns exactly once.
pub(crate) enum Event {
    Scanned {
        cursor: ScanCursor,
        buffer: ProposalBuffer,
        report: ScanReport,
    },
    ViewReturned(Box<View>),
    Discarded {
        segment: u32,
        result: io::Result<()>,
    },
    /// A backing read failed; the writer latches the volume. The window buffer is lost with it.
    ScanFailed(io::Error),
}

/// The writer's handle. At most one scan, one view refresh and `DISCARD_SLOTS` discards are
/// outstanding, so the bounded channels never fill.
pub(crate) struct CleanerHandle {
    commands: SyncSender<Command>,
    events: Receiver<Event>,
    wakeup: Wakeup,
    thread: Option<JoinHandle<()>>,
    /// Commands sent whose buffer, view or completion has not come back yet.
    outstanding: usize,
}

struct Cleaner {
    backing: File,
    geometry: Geometry,
    volume_id: u64,
    scanner: Scanner,
    view: Box<View>,
    commands: Receiver<Command>,
    events: SyncSender<Event>,
    wakeup: Wakeup,
    #[cfg(test)]
    delay: Duration,
    #[cfg(test)]
    fail_next_scan: bool,
}

impl CleanerHandle {
    pub(crate) fn spawn(
        backing: &File,
        geometry: Geometry,
        volume_id: u64,
        view: Box<View>,
    ) -> io::Result<Self> {
        let capacity = 2 + DISCARD_SLOTS;
        let (command_sender, command_receiver) = mpsc::sync_channel(capacity);
        let (event_sender, event_receiver) = mpsc::sync_channel(capacity);
        let wakeup = Wakeup::new()?;
        let mut cleaner = Cleaner {
            backing: backing.try_clone()?,
            geometry,
            volume_id,
            scanner: Scanner::new()?,
            view,
            commands: command_receiver,
            events: event_sender,
            wakeup: wakeup.try_clone()?,
            #[cfg(test)]
            delay: Duration::ZERO,
            #[cfg(test)]
            fail_next_scan: false,
        };
        let thread = std::thread::Builder::new()
            .name("block-storage-cleaner".to_owned())
            .spawn(move || cleaner.run())?;
        Ok(Self {
            commands: command_sender,
            events: event_receiver,
            wakeup,
            thread: Some(thread),
            outstanding: 0,
        })
    }

    /// The descriptor a request loop polls for readability to learn that events are waiting.
    pub(crate) fn wakeup_fd(&self) -> RawFd {
        self.wakeup.as_raw_fd()
    }

    pub(crate) fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Hands a command over. The caller keeps the outstanding count under the channel bound, so
    /// a full channel is a bug; a closed one means the cleaner thread is gone.
    pub(crate) fn send(&mut self, command: Command) -> io::Result<()> {
        let reports = command.reports();
        match self.commands.try_send(command) {
            Ok(()) => {
                if reports {
                    self.outstanding += 1;
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                unreachable!("cleaner command channel is bounded by outstanding work")
            }
            Err(TrySendError::Disconnected(_)) => Err(cleaner_gone()),
        }
    }

    /// Takes one ready event without blocking.
    pub(crate) fn try_recv(&mut self) -> io::Result<Option<Event>> {
        match self.events.try_recv() {
            Ok(event) => {
                self.outstanding -= 1;
                Ok(Some(event))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(cleaner_gone()),
        }
    }

    /// Waits up to `timeout` for one event; `None` when the wait timed out.
    pub(crate) fn recv_timeout(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => {
                self.outstanding -= 1;
                Ok(Some(event))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(cleaner_gone()),
        }
    }

    /// Clears the wake-up counter so the next poll reports only events sent after this point.
    pub(crate) fn drain_wakeup(&self) {
        self.wakeup.drain();
    }

    /// Stops the thread after the command it is on. Bounded by one window read or one discard.
    pub(crate) fn shutdown(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CleanerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn cleaner_gone() -> io::Error {
    io::Error::other("cleaner thread is gone")
}

impl Cleaner {
    fn run(&mut self) {
        // Bounded by the command stream: Shutdown or a closed channel ends the loop.
        loop {
            let Ok(command) = self.commands.recv() else {
                return;
            };
            let event = match command {
                Command::Shutdown => return,
                Command::Scan { cursor, buffer } => self.scan(cursor, buffer),
                Command::Refresh(view) => {
                    Event::ViewReturned(std::mem::replace(&mut self.view, view))
                }
                Command::Discard { segment } => Event::Discarded {
                    segment,
                    result: self.discard(segment),
                },
                #[cfg(test)]
                Command::Delay(delay) => {
                    self.delay = delay;
                    continue;
                }
                #[cfg(test)]
                Command::FailNextScan => {
                    self.fail_next_scan = true;
                    continue;
                }
            };
            if self.events.send(event).is_err() {
                return;
            }
            self.wakeup.signal();
        }
    }

    fn scan(&mut self, mut cursor: ScanCursor, mut buffer: ProposalBuffer) -> Event {
        #[cfg(test)]
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_scan) {
            return Event::ScanFailed(io::Error::other("injected cleaner read failure"));
        }
        let context = ScanContext {
            backing: &self.backing,
            geometry: self.geometry,
            volume_id: self.volume_id,
            view: &self.view,
        };
        match self.scanner.scan_window(&context, &mut cursor, &mut buffer) {
            Ok(report) => Event::Scanned {
                cursor,
                buffer,
                report,
            },
            Err(error) => Event::ScanFailed(error),
        }
    }

    /// `fallocate(PUNCH_HOLE | KEEP_SIZE)` over the whole segment through the cleaner's ring.
    /// Discards are neither reads nor writes, so the accounting ledger does not see them.
    fn discard(&mut self, segment: u32) -> io::Result<()> {
        let start = u64::from(self.geometry.segment_start(segment)?) * BLOCK_SIZE as u64;
        let length = u64::from(self.geometry.blocks_per_segment) * BLOCK_SIZE as u64;
        let result = self.scanner.punch_hole(&self.backing, start, length);
        #[cfg(any(test, feature = "test-failpoints"))]
        crate::pause_at_test_failpoint("backing-discard-complete")?;
        result
    }
}
