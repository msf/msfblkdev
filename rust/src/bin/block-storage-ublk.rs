#[cfg(not(target_os = "linux"))]
compile_error!("block-storage-ublk requires Linux");

use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::helpers::IoBuf;
use libublk::io::{BufDescList, UblkDev, UblkQueue};
use libublk::sys::{
    UBLK_ATTR_VOLATILE_CACHE, UBLK_IO_F_FUA, UBLK_IO_OP_FLUSH, UBLK_IO_OP_READ, UBLK_IO_OP_WRITE,
    UBLK_PARAM_TYPE_BASIC, ublk_param_basic, ublk_params, ublksrv_io_desc,
};
use libublk::{BufDesc, UblkError, UblkFlags, UblkIORes};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BLOCK_SHIFT: u8 = 12;
const IO_BUFFER_BYTES: u32 = 1 << BLOCK_SHIFT;
const SECTOR_SHIFT: u8 = 9;
const SECTORS_PER_BLOCK: u64 = 1 << (BLOCK_SHIFT - SECTOR_SHIFT);
const QUEUE_COUNT: u16 = 1;
const QUEUE_DEPTH: u16 = 1;
const BACKING_LOCKED_MESSAGE: &str = "backing file is already locked";
const QUEUE_RUNNING_IDLE: u8 = 0;
const QUEUE_RUNNING_IN_FLIGHT: u8 = 1;
const QUEUE_STOPPING: u8 = 2;
const SHUTDOWN_COMPLETION: i32 = -libc::ESHUTDOWN;
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(1);
const DEVICE_REMOVAL_TIMEOUT: Duration = Duration::from_secs(2);

static SIGTERM_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn sigterm_handler(_signal: libc::c_int) {
    SIGTERM_REQUESTED.store(true, Ordering::Relaxed);
}

struct SignalGuard {
    previous: libc::sigaction,
    installed: bool,
}

impl SignalGuard {
    fn install() -> io::Result<Self> {
        SIGTERM_REQUESTED.store(false, Ordering::Relaxed);
        // SAFETY: sigaction is initialized before use, the handler has the required ABI, and
        // it performs only a lock-free atomic store.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = sigterm_handler as *const () as usize;
            action.sa_flags = 0;
            if libc::sigemptyset(&mut action.sa_mask) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut previous = std::mem::zeroed();
            if libc::sigaction(libc::SIGTERM, &action, &mut previous) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                previous,
                installed: true,
            })
        }
    }

    fn restore(mut self) -> io::Result<()> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> io::Result<()> {
        if !self.installed {
            return Ok(());
        }
        // SAFETY: previous was populated by a successful sigaction call.
        if unsafe { libc::sigaction(libc::SIGTERM, &self.previous, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.installed = false;
        Ok(())
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

struct DrainState {
    queue_phase: AtomicU8,
    queue_failed: AtomicBool,
    device_ready: AtomicBool,
    target_done: AtomicBool,
}

impl DrainState {
    fn new() -> Self {
        Self {
            queue_phase: AtomicU8::new(QUEUE_RUNNING_IDLE),
            queue_failed: AtomicBool::new(false),
            device_ready: AtomicBool::new(false),
            target_done: AtomicBool::new(false),
        }
    }

    fn begin_engine_work(&self) -> EngineWork<'_> {
        let in_flight = self
            .queue_phase
            .compare_exchange(
                QUEUE_RUNNING_IDLE,
                QUEUE_RUNNING_IN_FLIGHT,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()
            .map(|_| InFlightRequest(self));
        match in_flight {
            Some(in_flight) if !SIGTERM_REQUESTED.load(Ordering::Relaxed) => {
                EngineWork::Accepted(in_flight)
            }
            in_flight => EngineWork::Rejected(in_flight),
        }
    }

    fn stop_if_idle(&self) -> bool {
        match self.queue_phase.compare_exchange(
            QUEUE_RUNNING_IDLE,
            QUEUE_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(QUEUE_STOPPING) => true,
            Err(_) => false,
        }
    }
}

enum EngineWork<'a> {
    Accepted(InFlightRequest<'a>),
    Rejected(Option<InFlightRequest<'a>>),
}

struct InFlightRequest<'a>(&'a DrainState);

impl Drop for InFlightRequest<'_> {
    fn drop(&mut self) {
        self.0
            .queue_phase
            .store(QUEUE_RUNNING_IDLE, Ordering::Release);
    }
}

#[derive(Debug)]
struct BackingLock {
    _backing: File,
}

impl BackingLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let backing = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)?;
        // SAFETY: flock only reads the valid file descriptor and lock operation flags.
        if unsafe { libc::flock(backing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Self { _backing: backing });
        }

        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                BACKING_LOCKED_MESSAGE,
            ))
        } else {
            Err(error)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    Serve {
        backing_path: PathBuf,
        device_id: i32,
    },
    Delete {
        device_id: i32,
    },
    Format {
        backing_path: PathBuf,
        backing_bytes: u64,
        volume_bytes: u64,
    },
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> io::Result<CliCommand> {
    let mut args = args.into_iter();
    let program = args
        .next()
        .unwrap_or_else(|| OsString::from("block-storage-ublk"));
    let usage = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "usage: {} serve <backing> <device-id> | delete <device-id> | format <new-regular-file> <backing-bytes> <volume-bytes>",
                PathBuf::from(&program).display()
            ),
        )
    };

    let command = args.next().ok_or_else(&usage)?;
    let command = match command.to_str() {
        Some("serve") => CliCommand::Serve {
            backing_path: PathBuf::from(args.next().ok_or_else(&usage)?),
            device_id: parse_serve_device_id(&args.next().ok_or_else(&usage)?)?,
        },
        Some("delete") => CliCommand::Delete {
            device_id: parse_delete_device_id(&args.next().ok_or_else(&usage)?)?,
        },
        Some("format") => CliCommand::Format {
            backing_path: PathBuf::from(args.next().ok_or_else(&usage)?),
            backing_bytes: parse_bytes(&args.next().ok_or_else(&usage)?)?,
            volume_bytes: parse_bytes(&args.next().ok_or_else(&usage)?)?,
        },
        _ => return Err(usage()),
    };
    if args.next().is_some() {
        return Err(usage());
    }
    Ok(command)
}

fn parse_i32(value: &OsStr) -> io::Result<i32> {
    value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid device ID"))?
        .parse::<i32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid device ID"))
}

fn parse_serve_device_id(value: &OsStr) -> io::Result<i32> {
    let id = parse_i32(value)?;
    if id < -1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "serve device ID must be -1 or non-negative",
        ));
    }
    Ok(id)
}

fn parse_delete_device_id(value: &OsStr) -> io::Result<i32> {
    let id = parse_i32(value)?;
    if id < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "delete device ID must be non-negative",
        ));
    }
    Ok(id)
}

fn parse_bytes(value: &OsStr) -> io::Result<u64> {
    value
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid byte size"))?
        .parse::<u64>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid byte size"))
}

fn controller_builder(device_id: i32) -> UblkCtrlBuilder<'static> {
    UblkCtrlBuilder::default()
        .name("block-storage")
        .id(device_id)
        .nr_queues(QUEUE_COUNT)
        .depth(QUEUE_DEPTH)
        .io_buf_bytes(IO_BUFFER_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
}

struct TargetConfiguration {
    device_bytes: u64,
    parameters: ublk_params,
}

fn target_configuration(volume_blocks: u32) -> TargetConfiguration {
    TargetConfiguration {
        device_bytes: u64::from(volume_blocks) * u64::from(IO_BUFFER_BYTES),
        parameters: ublk_params {
            types: UBLK_PARAM_TYPE_BASIC,
            basic: ublk_param_basic {
                attrs: UBLK_ATTR_VOLATILE_CACHE,
                logical_bs_shift: BLOCK_SHIFT,
                physical_bs_shift: BLOCK_SHIFT,
                io_opt_shift: BLOCK_SHIFT,
                io_min_shift: BLOCK_SHIFT,
                max_sectors: IO_BUFFER_BYTES >> SECTOR_SHIFT,
                dev_sectors: u64::from(volume_blocks) * SECTORS_PER_BLOCK,
                ..Default::default()
            },
            ..Default::default()
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Read(u32),
    Write(u32),
    Flush,
}

fn decode_request(descriptor: &ublksrv_io_desc, dev_sectors: u64) -> Result<Request, i32> {
    let operation = descriptor.op_flags & 0xff;
    let flags = descriptor.op_flags & !0xff;
    if flags & UBLK_IO_F_FUA != 0 {
        return Err(-libc::EOPNOTSUPP);
    }
    if flags != 0 {
        return Err(-libc::EOPNOTSUPP);
    }

    match operation {
        UBLK_IO_OP_READ | UBLK_IO_OP_WRITE => {
            if descriptor.nr_sectors != SECTORS_PER_BLOCK as u32
                || !descriptor.start_sector.is_multiple_of(SECTORS_PER_BLOCK)
                || descriptor.start_sector > dev_sectors
                || SECTORS_PER_BLOCK > dev_sectors - descriptor.start_sector
            {
                return Err(-libc::EINVAL);
            }
            let lba = u32::try_from(descriptor.start_sector / SECTORS_PER_BLOCK)
                .map_err(|_| -libc::EINVAL)?;
            if operation == UBLK_IO_OP_READ {
                Ok(Request::Read(lba))
            } else {
                Ok(Request::Write(lba))
            }
        }
        UBLK_IO_OP_FLUSH if descriptor.nr_sectors == 0 && descriptor.start_sector == 0 => {
            Ok(Request::Flush)
        }
        UBLK_IO_OP_FLUSH => Err(-libc::EINVAL),
        _ => Err(-libc::EOPNOTSUPP),
    }
}

fn engine_error(error: io::Error) -> i32 {
    if error.raw_os_error() == Some(libc::ENOSPC) {
        -libc::ENOSPC
    } else {
        -libc::EIO
    }
}

fn handle_request(
    volume: &mut block_storage::Volume,
    descriptor: &ublksrv_io_desc,
    buffer: &mut [u8; IO_BUFFER_BYTES as usize],
) -> i32 {
    let dev_sectors = u64::from(volume.volume_blocks()) * SECTORS_PER_BLOCK;
    let request = match decode_request(descriptor, dev_sectors) {
        Ok(request) => request,
        Err(errno) => return errno,
    };
    let result = match request {
        Request::Read(lba) => volume.read_block(lba, buffer),
        Request::Write(lba) => volume.write_block(lba, buffer),
        Request::Flush => volume.flush(),
    };
    match result {
        Ok(()) => match request {
            Request::Read(_) | Request::Write(_) => IO_BUFFER_BYTES as i32,
            Request::Flush => 0,
        },
        Err(error) => engine_error(error),
    }
}

fn handle_buffered_request(
    volume: &mut block_storage::Volume,
    descriptor: &ublksrv_io_desc,
    buffer: &mut [u8],
) -> i32 {
    match <&mut [u8; IO_BUFFER_BYTES as usize]>::try_from(buffer) {
        Ok(buffer) => handle_request(volume, descriptor, buffer),
        Err(_) => -libc::EIO,
    }
}

fn complete_fetched_request<C, E>(
    work: EngineWork<'_>,
    context: &mut C,
    engine_work: impl FnOnce(&mut C) -> i32,
    complete: impl FnOnce(&mut C, i32) -> Result<(), E>,
) -> Result<(), E> {
    let (result, in_flight) = match work {
        EngineWork::Accepted(in_flight) => (engine_work(context), Some(in_flight)),
        EngineWork::Rejected(in_flight) => (SHUTDOWN_COMPLETION, in_flight),
    };
    let completion = complete(context, result);
    drop(in_flight);
    completion
}

fn combine_shutdown_results(
    queue_result: io::Result<()>,
    close_result: io::Result<()>,
) -> io::Result<()> {
    match (queue_result, close_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(queue_error), Err(close_error)) => Err(io::Error::other(format!(
            "queue failed: {queue_error}; volume close failed: {close_error}"
        ))),
    }
}

fn combine_daemon_results(
    target_result: io::Result<()>,
    readiness_result: io::Result<()>,
    stop_result: io::Result<()>,
    shutdown_result: io::Result<()>,
    removal_result: io::Result<()>,
    restore_result: io::Result<()>,
) -> io::Result<()> {
    let results = [
        ("target", target_result),
        ("readiness", readiness_result),
        ("stop", stop_result),
        ("shutdown", shutdown_result),
        ("device removal", removal_result),
        ("signal restoration", restore_result),
    ];
    let failures: Vec<_> = results
        .into_iter()
        .filter_map(|(step, result)| result.err().map(|error| format!("{step} failed: {error}")))
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(failures.join("; ")))
    }
}

struct QueueState {
    volume: Option<block_storage::Volume>,
    result: Option<io::Result<()>>,
}

type SharedQueueState = Arc<Mutex<QueueState>>;

fn ublk_error(error: UblkError) -> io::Error {
    io::Error::other(error)
}

fn store_queue_result(state: &SharedQueueState, result: io::Result<()>) {
    match state.lock() {
        Ok(mut state) => state.result = Some(result),
        Err(_) => eprintln!("block-storage-ublk: queue state lock poisoned"),
    }
}

fn close_after_queue(
    state: &SharedQueueState,
    volume: block_storage::Volume,
    queue_result: io::Result<()>,
) {
    store_queue_result(
        state,
        combine_shutdown_results(queue_result, volume.close()),
    );
}

fn run_queue(qid: u16, dev: &UblkDev, state: SharedQueueState, drain: Arc<DrainState>) {
    let volume = match state.lock() {
        Ok(mut state) => state.volume.take(),
        Err(_) => {
            drain.queue_failed.store(true, Ordering::Release);
            eprintln!("block-storage-ublk: queue state lock poisoned");
            return;
        }
    };
    let Some(mut volume) = volume else {
        drain.queue_failed.store(true, Ordering::Release);
        store_queue_result(&state, Err(io::Error::other("volume is not available")));
        return;
    };

    let mut buffers = vec![IoBuf::<u8>::new(IO_BUFFER_BYTES as usize)];
    let queue = match UblkQueue::new(qid, dev)
        .and_then(|queue| queue.submit_fetch_commands_unified(BufDescList::Slices(Some(&buffers))))
    {
        Ok(queue) => queue,
        Err(error) => {
            drain.queue_failed.store(true, Ordering::Release);
            close_after_queue(&state, volume, Err(ublk_error(error)));
            return;
        }
    };

    let mut queue_error = None;
    queue.wait_and_handle_io(|queue, tag, _context| {
        let mut context = (&mut volume, buffers[0].as_mut_slice());
        let completion = complete_fetched_request(
            drain.begin_engine_work(),
            &mut context,
            |(volume, buffer)| handle_buffered_request(volume, queue.get_iod(tag), buffer),
            |(_, buffer), result| {
                queue.complete_io_cmd_unified(
                    tag,
                    BufDesc::Slice(buffer),
                    Ok(UblkIORes::Result(result)),
                )
            },
        );
        if let Err(error) = completion {
            drain.queue_failed.store(true, Ordering::Release);
            if queue_error.is_none() {
                queue_error = Some(ublk_error(error));
            }
        }
    });

    close_after_queue(&state, volume, queue_error.map_or(Ok(()), Err));
}

fn wait_for_stop(controller: &libublk::ctrl::UblkCtrl, drain: &DrainState) -> io::Result<()> {
    loop {
        let stop_requested =
            SIGTERM_REQUESTED.load(Ordering::Relaxed) || drain.queue_failed.load(Ordering::Acquire);
        if stop_requested && drain.stop_if_idle() && drain.device_ready.load(Ordering::Acquire) {
            return controller.kill_dev().map(|_| ()).map_err(ublk_error);
        }
        if drain.target_done.load(Ordering::Acquire) {
            return Ok(());
        }
        std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
}

fn write_readiness_line(output: &mut impl io::Write, device_path: &str) -> io::Result<()> {
    writeln!(output, "{device_path}")?;
    output.flush()
}

fn wait_for_device_removal(device_id: u32) -> io::Result<()> {
    let path = PathBuf::from(format!("/sys/class/ublk-char/ublkc{device_id}"));
    let deadline = std::time::Instant::now() + DEVICE_REMOVAL_TIMEOUT;
    while path.exists() {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "{} still exists after controller drop",
                path.display()
            )));
        }
        std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
    Ok(())
}

fn serve(backing_path: &Path, device_id: i32) -> io::Result<()> {
    let _backing_lock = BackingLock::acquire(backing_path)?;
    let signal_guard = SignalGuard::install()?;
    let drain = Arc::new(DrainState::new());
    let readiness_error = Arc::new(Mutex::new(None));
    let state = Arc::new(Mutex::new(QueueState {
        volume: Some(block_storage::open(backing_path)?),
        result: None,
    }));
    let target = {
        let state = state
            .lock()
            .map_err(|_| io::Error::other("queue state lock poisoned"))?;
        target_configuration(
            state
                .volume
                .as_ref()
                .ok_or_else(|| io::Error::other("volume is not available"))?
                .volume_blocks(),
        )
    };

    let (target_result, stop_result, shutdown_result, device_id) = {
        let controller = controller_builder(device_id).build().map_err(ublk_error)?;
        let device_id = controller.dev_info().dev_id;
        let queue_state = Arc::clone(&state);
        let queue_drain = Arc::clone(&drain);
        let ready_drain = Arc::clone(&drain);
        let ready_error = Arc::clone(&readiness_error);
        let (target_result, stop_result) = std::thread::scope(|scope| {
            let stop_drain = Arc::clone(&drain);
            let controller_ref = &controller;
            let stop_thread = scope.spawn(move || wait_for_stop(controller_ref, &stop_drain));
            let target_result = controller
                .run_target(
                    move |dev| {
                        dev.tgt.dev_size = target.device_bytes;
                        dev.tgt.params = target.parameters;
                        Ok(())
                    },
                    move |qid, dev| {
                        run_queue(qid, dev, Arc::clone(&queue_state), Arc::clone(&queue_drain))
                    },
                    move |controller| {
                        let result = write_readiness_line(
                            &mut io::stdout().lock(),
                            &controller.get_bdev_path(),
                        );
                        let failed = result.is_err();
                        match ready_error.lock() {
                            Ok(mut error) => *error = result.err(),
                            Err(_) => {
                                eprintln!("block-storage-ublk: readiness error state lock poisoned")
                            }
                        }
                        ready_drain.device_ready.store(true, Ordering::Release);
                        if failed {
                            SIGTERM_REQUESTED.store(true, Ordering::Relaxed);
                        }
                    },
                )
                .map(|_| ())
                .map_err(ublk_error);
            drain.target_done.store(true, Ordering::Release);
            let stop_result = stop_thread
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("stop thread panicked")));
            (target_result, stop_result)
        });
        let shutdown_result = match state.lock() {
            Ok(mut state) => {
                let remaining_volume = state.volume.take();
                let queue_result = state.result.take();
                match (remaining_volume, queue_result) {
                    (Some(volume), _) => volume.close(),
                    (None, Some(result)) => result,
                    (None, None) => Err(io::Error::other("queue stopped without a result")),
                }
            }
            Err(_) => Err(io::Error::other("queue state lock poisoned")),
        };
        (target_result, stop_result, shutdown_result, device_id)
    };

    let removal_result = wait_for_device_removal(device_id);
    let restore_result = signal_guard.restore();
    let readiness_result = match readiness_error.lock() {
        Ok(mut error) => error.take().map_or(Ok(()), Err),
        Err(_) => Err(io::Error::other("readiness error state lock poisoned")),
    };
    combine_daemon_results(
        target_result,
        readiness_result,
        stop_result,
        shutdown_result,
        removal_result,
        restore_result,
    )
}

fn delete(device_id: i32) -> io::Result<()> {
    UblkCtrl::new_simple(device_id)
        .map_err(ublk_error)?
        .del_dev()
        .map(|_| ())
        .map_err(ublk_error)
}

fn format_new(backing_path: &Path, backing_bytes: u64, volume_bytes: u64) -> io::Result<()> {
    let backing = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(backing_path)?;
    let format_error = match backing
        .set_len(backing_bytes)
        .and_then(|()| block_storage::format(backing_path, volume_bytes))
    {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    drop(backing);
    match std::fs::remove_file(backing_path) {
        Ok(()) => Err(format_error),
        Err(cleanup_error) => Err(io::Error::other(format!(
            "{format_error}; failed to remove newly created backing file: {cleanup_error}"
        ))),
    }
}

fn run() -> io::Result<()> {
    match parse_args(std::env::args_os())? {
        CliCommand::Serve {
            backing_path,
            device_id,
        } => serve(&backing_path, device_id),
        CliCommand::Delete { device_id } => delete(device_id),
        CliCommand::Format {
            backing_path,
            backing_bytes,
            volume_bytes,
        } => format_new(&backing_path, backing_bytes, volume_bytes),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("block-storage-ublk: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{hard_link, remove_file};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct ReadinessWriter {
        bytes: Vec<u8>,
        flushes: usize,
        fail_flush: bool,
    }

    impl Write for ReadinessWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout closed"))
            } else {
                Ok(())
            }
        }
    }

    struct TemporaryBacking(PathBuf);

    impl TemporaryBacking {
        fn new() -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let backing = Self(
                std::env::temp_dir()
                    .join(format!("block-storage-ublk-{}-{nonce}", std::process::id())),
            );
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&backing.0)?;
            Ok(backing)
        }

        fn formatted(volume_blocks: u32) -> io::Result<Self> {
            let backing = Self::new()?;
            OpenOptions::new()
                .write(true)
                .open(&backing.0)?
                .set_len(1024 * 1024)?;
            block_storage::format(
                &backing.0,
                u64::from(volume_blocks) * u64::from(IO_BUFFER_BYTES),
            )?;
            Ok(backing)
        }
    }

    impl Drop for TemporaryBacking {
        fn drop(&mut self) {
            let _ = remove_file(&self.0);
        }
    }

    struct TemporaryPath(PathBuf);

    impl TemporaryPath {
        fn new() -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            Ok(Self(std::env::temp_dir().join(format!(
                "block-storage-ublk-new-{}-{nonce}",
                std::process::id()
            ))))
        }
    }

    impl Drop for TemporaryPath {
        fn drop(&mut self) {
            let _ = remove_file(&self.0);
        }
    }

    struct TemporaryLink(PathBuf);

    impl Drop for TemporaryLink {
        fn drop(&mut self) {
            let _ = remove_file(&self.0);
        }
    }

    struct TestChild {
        child: Child,
        output_reader: Option<std::thread::JoinHandle<io::Result<()>>>,
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(reader) = self.output_reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("timed out waiting for lock owner exit"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn descriptor(operation: u32, start_sector: u64, nr_sectors: u32) -> ublksrv_io_desc {
        ublksrv_io_desc {
            op_flags: operation,
            nr_sectors,
            start_sector,
            ..Default::default()
        }
    }

    #[test]
    fn exclusive_backing_lock_tracks_the_inode_and_owner_process() -> io::Result<()> {
        const CHILD_BACKING_ENV: &str = "BLOCK_STORAGE_LOCK_TEST_BACKING";
        const HANDSHAKE: &str = "backing-locked";
        const TIMEOUT: Duration = Duration::from_secs(10);

        if let Some(path) = std::env::var_os(CHILD_BACKING_ENV) {
            let _lock = BackingLock::acquire(Path::new(&path))?;
            println!("{HANDSHAKE}");
            io::stdout().flush()?;
            let mut input = [0; 1];
            let _ = io::stdin().read(&mut input)?;
            return Ok(());
        }

        let backing = TemporaryBacking::new()?;
        let alias = TemporaryLink(backing.0.with_extension("same-inode"));
        hard_link(&backing.0, &alias.0)?;
        let different_backing = TemporaryBacking::new()?;

        let child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tests::exclusive_backing_lock_tracks_the_inode_and_owner_process",
                "--nocapture",
            ])
            .env(CHILD_BACKING_ENV, &backing.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut child = TestChild {
            child,
            output_reader: None,
        };
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("lock owner stdout unavailable"))?;
        let (handshake_sender, handshake_receiver) = mpsc::sync_channel(1);
        let output_reader = std::thread::spawn(move || {
            let mut handshake_sent = false;
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if line == HANDSHAKE && !handshake_sent => {
                        handshake_sender
                            .send(Ok(()))
                            .map_err(|_| io::Error::other("lock handshake receiver dropped"))?;
                        handshake_sent = true;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if !handshake_sent {
                            let _ = handshake_sender.send(Err(error));
                        }
                        return Ok(());
                    }
                }
            }
            if !handshake_sent {
                let _ = handshake_sender
                    .send(Err(io::Error::other("lock owner exited before handshake")));
            }
            Ok(())
        });
        child.output_reader = Some(output_reader);
        handshake_receiver
            .recv_timeout(TIMEOUT)
            .map_err(|_| io::Error::other("timed out waiting for lock owner handshake"))??;

        let competing_error = BackingLock::acquire(&alias.0).unwrap_err();
        assert_eq!(competing_error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(competing_error.to_string(), BACKING_LOCKED_MESSAGE);
        let different_lock = BackingLock::acquire(&different_backing.0)?;

        drop(child.child.stdin.take());
        let status = wait_for_child_exit(&mut child.child, TIMEOUT)?;
        assert!(status.success(), "lock owner failed: {status}");
        child
            .output_reader
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("lock owner output reader panicked"))??;

        let released_lock = BackingLock::acquire(&alias.0)?;
        drop((released_lock, different_lock));
        Ok(())
    }

    #[test]
    fn parses_explicit_commands() {
        assert_eq!(
            parse_args(["daemon", "serve", "volume.img", "-1"].map(OsString::from)).unwrap(),
            CliCommand::Serve {
                backing_path: PathBuf::from("volume.img"),
                device_id: -1,
            }
        );
        assert_eq!(
            parse_args(["daemon", "delete", "7"].map(OsString::from)).unwrap(),
            CliCommand::Delete { device_id: 7 }
        );
        assert_eq!(
            parse_args(["daemon", "format", "volume.img", "1048576", "16384"].map(OsString::from))
                .unwrap(),
            CliCommand::Format {
                backing_path: PathBuf::from("volume.img"),
                backing_bytes: 1_048_576,
                volume_bytes: 16_384,
            }
        );
    }

    #[test]
    fn rejects_invalid_arguments_and_negative_delete_ids() {
        assert!(parse_serve_device_id(OsStr::new("-2")).is_err());
        assert!(parse_delete_device_id(OsStr::new("-1")).is_err());
        assert!(parse_delete_device_id(OsStr::new("-2")).is_err());
        assert!(parse_delete_device_id(OsStr::new("not-a-number")).is_err());
        assert!(parse_args([OsString::from("daemon")]).is_err());
        assert!(parse_args(["daemon", "unknown"].map(OsString::from)).is_err());
        assert!(parse_args(["daemon", "delete", "1", "extra"].map(OsString::from)).is_err());
    }

    #[test]
    fn format_new_sizes_formats_and_opens_exact_image() -> io::Result<()> {
        let backing = TemporaryPath::new()?;
        format_new(&backing.0, 1024 * 1024, 4 * u64::from(IO_BUFFER_BYTES))?;

        assert_eq!(std::fs::metadata(&backing.0)?.len(), 1024 * 1024);
        let volume = block_storage::open(&backing.0)?;
        assert_eq!(volume.volume_blocks(), 4);
        volume.close()
    }

    #[test]
    fn format_new_never_overwrites_an_existing_path() -> io::Result<()> {
        let backing = TemporaryBacking::new()?;
        std::fs::write(&backing.0, b"preserve me")?;

        let error = format_new(&backing.0, 1024 * 1024, 4096).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&backing.0)?, b"preserve me");
        Ok(())
    }

    #[test]
    fn format_new_removes_created_file_after_engine_validation_failure() -> io::Result<()> {
        for (backing_bytes, volume_bytes) in [(1024 * 1024 + 1, 4096), (1024 * 1024, 4097)] {
            let backing = TemporaryPath::new()?;
            let error = format_new(&backing.0, backing_bytes, volume_bytes).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(!backing.0.exists());
        }
        Ok(())
    }

    #[test]
    fn readiness_line_is_explicitly_flushed_and_flush_errors_are_returned() {
        let mut output = ReadinessWriter::default();
        write_readiness_line(&mut output, "/dev/ublkb7").unwrap();
        assert_eq!(output.bytes, b"/dev/ublkb7\n");
        assert_eq!(output.flushes, 1);

        let mut closed_output = ReadinessWriter {
            fail_flush: true,
            ..Default::default()
        };
        let error = write_readiness_line(&mut closed_output, "/dev/ublkb8").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(closed_output.bytes, b"/dev/ublkb8\n");
        assert_eq!(closed_output.flushes, 1);
    }

    #[test]
    fn configures_exact_v07_target_parameters() {
        let target = target_configuration(37);
        let params = target.parameters;
        let basic = params.basic;

        assert_eq!(target.device_bytes, 37 * 4096);
        assert_eq!(params.len, 0);
        assert_eq!(params.types, UBLK_PARAM_TYPE_BASIC);
        assert_eq!(basic.attrs, UBLK_ATTR_VOLATILE_CACHE);
        assert_eq!(basic.logical_bs_shift, BLOCK_SHIFT);
        assert_eq!(basic.physical_bs_shift, BLOCK_SHIFT);
        assert_eq!(basic.io_opt_shift, BLOCK_SHIFT);
        assert_eq!(basic.io_min_shift, BLOCK_SHIFT);
        assert_eq!(basic.max_sectors, 8);
        assert_eq!(basic.chunk_sectors, 0);
        assert_eq!(basic.dev_sectors, 37 * 8);
        assert_eq!(basic.virt_boundary_mask, 0);
        assert_eq!(params.discard.discard_alignment, 0);
        assert_eq!(params.discard.discard_granularity, 0);
        assert_eq!(params.discard.max_discard_sectors, 0);
        assert_eq!(params.discard.max_write_zeroes_sectors, 0);
        assert_eq!(params.discard.max_discard_segments, 0);
        assert_eq!(params.discard.reserved0, 0);
        assert_eq!(params.devt.char_major, 0);
        assert_eq!(params.devt.char_minor, 0);
        assert_eq!(params.devt.disk_major, 0);
        assert_eq!(params.devt.disk_minor, 0);
        assert_eq!(params.zoned.max_open_zones, 0);
        assert_eq!(params.zoned.max_active_zones, 0);
        assert_eq!(params.zoned.max_zone_append_sectors, 0);
        assert_eq!(params.zoned.reserved, [0; 20]);
    }

    #[test]
    fn configures_one_depth_one_4k_queue() {
        assert_eq!(QUEUE_COUNT, 1);
        assert_eq!(QUEUE_DEPTH, 1);
        assert_eq!(IO_BUFFER_BYTES, 4096);

        let expected = UblkCtrlBuilder::default()
            .name("block-storage")
            .id(4)
            .nr_queues(1)
            .depth(1)
            .io_buf_bytes(4096)
            .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV);
        assert_eq!(controller_builder(4), expected);
    }

    #[test]
    fn decodes_only_exact_supported_requests() {
        let dev_sectors = 32;
        assert_eq!(
            decode_request(&descriptor(UBLK_IO_OP_READ, 24, 8), dev_sectors),
            Ok(Request::Read(3))
        );
        assert_eq!(
            decode_request(&descriptor(UBLK_IO_OP_WRITE, 0, 8), dev_sectors),
            Ok(Request::Write(0))
        );
        assert_eq!(
            decode_request(&descriptor(UBLK_IO_OP_FLUSH, 0, 0), dev_sectors),
            Ok(Request::Flush)
        );

        for invalid in [
            descriptor(UBLK_IO_OP_READ, 0, 7),
            descriptor(UBLK_IO_OP_READ, 1, 8),
            descriptor(UBLK_IO_OP_READ, 32, 8),
            descriptor(UBLK_IO_OP_READ, 33, 8),
            descriptor(UBLK_IO_OP_READ, u64::MAX, 8),
            descriptor(UBLK_IO_OP_FLUSH, 8, 0),
            descriptor(UBLK_IO_OP_FLUSH, 0, 8),
        ] {
            assert_eq!(decode_request(&invalid, dev_sectors), Err(-libc::EINVAL));
        }

        let mut fua = descriptor(UBLK_IO_OP_WRITE, 0, 8);
        fua.op_flags |= UBLK_IO_F_FUA;
        assert_eq!(decode_request(&fua, dev_sectors), Err(-libc::EOPNOTSUPP));
        let mut unsupported_flag = descriptor(UBLK_IO_OP_READ, 0, 8);
        unsupported_flag.op_flags |= libublk::sys::UBLK_IO_F_META;
        assert_eq!(
            decode_request(&unsupported_flag, dev_sectors),
            Err(-libc::EOPNOTSUPP)
        );
        assert_eq!(
            decode_request(
                &descriptor(libublk::sys::UBLK_IO_OP_DISCARD, 0, 8),
                dev_sectors
            ),
            Err(-libc::EOPNOTSUPP)
        );
    }

    #[test]
    fn adapter_targets_exact_lba_and_flushes_durable_data() -> io::Result<()> {
        let backing = TemporaryBacking::formatted(4)?;
        let mut volume = block_storage::open(&backing.0)?;
        let write = descriptor(UBLK_IO_OP_WRITE, 16, 8);
        let flush = descriptor(UBLK_IO_OP_FLUSH, 0, 0);
        let read = descriptor(UBLK_IO_OP_READ, 16, 8);
        let read_neighbor = descriptor(UBLK_IO_OP_READ, 8, 8);
        let expected = [0xa5; IO_BUFFER_BYTES as usize];
        let mut buffer = expected;

        assert_eq!(handle_request(&mut volume, &write, &mut buffer), 4096);
        assert_eq!(handle_request(&mut volume, &flush, &mut buffer), 0);
        drop(volume);

        let mut reopened = block_storage::open(&backing.0)?;
        buffer.fill(0);
        assert_eq!(handle_request(&mut reopened, &read, &mut buffer), 4096);
        assert_eq!(buffer, expected);
        buffer.fill(0xff);
        assert_eq!(
            handle_request(&mut reopened, &read_neighbor, &mut buffer),
            4096
        );
        assert_eq!(buffer, [0; IO_BUFFER_BYTES as usize]);
        Ok(())
    }

    #[test]
    fn maps_engine_capacity_and_fatal_errors_to_stable_errno() {
        assert_eq!(
            engine_error(io::Error::from_raw_os_error(libc::ENOSPC)),
            -libc::ENOSPC
        );
        assert_eq!(
            engine_error(io::Error::new(
                io::ErrorKind::InvalidData,
                "payload checksum mismatch"
            )),
            -libc::EIO
        );
        assert_eq!(engine_error(io::Error::other("volume failed")), -libc::EIO);
    }

    #[test]
    fn rejects_an_unexpected_libublk_buffer_size_without_engine_io() -> io::Result<()> {
        let backing = TemporaryBacking::formatted(1)?;
        let mut volume = block_storage::open(&backing.0)?;
        let mut short_buffer = [0xa5; IO_BUFFER_BYTES as usize - 1];

        assert_eq!(
            handle_buffered_request(
                &mut volume,
                &descriptor(UBLK_IO_OP_WRITE, 0, 8),
                &mut short_buffer
            ),
            -libc::EIO
        );

        let mut block = [0xff; IO_BUFFER_BYTES as usize];
        assert_eq!(
            handle_request(&mut volume, &descriptor(UBLK_IO_OP_READ, 0, 8), &mut block),
            IO_BUFFER_BYTES as i32
        );
        assert_eq!(block, [0; IO_BUFFER_BYTES as usize]);
        Ok(())
    }

    #[test]
    fn fetched_requests_complete_once_without_shutdown_engine_io() {
        let drain = DrainState::new();
        let mut accepted = (0, Vec::new());
        complete_fetched_request(
            drain.begin_engine_work(),
            &mut accepted,
            |(engine_calls, _)| {
                *engine_calls += 1;
                4096
            },
            |(_, completions), result| {
                completions.push(result);
                Ok::<(), ()>(())
            },
        )
        .unwrap();
        assert_eq!(accepted, (1, vec![4096]));

        let mut rejected = (0, Vec::new());
        complete_fetched_request(
            EngineWork::Rejected(None),
            &mut rejected,
            |(engine_calls, _)| {
                *engine_calls += 1;
                4096
            },
            |(_, completions), result| {
                completions.push(result);
                Ok::<(), ()>(())
            },
        )
        .unwrap();
        assert_eq!(rejected, (0, vec![SHUTDOWN_COMPLETION]));
    }

    #[test]
    fn signal_state_stops_new_work_and_handler_is_restored() -> io::Result<()> {
        const CHILD_ENV: &str = "BLOCK_STORAGE_SIGTERM_TEST_CHILD";
        const TIMEOUT: Duration = Duration::from_secs(10);

        if std::env::var_os(CHILD_ENV).is_some() {
            // SAFETY: sigaction writes to initialized storage and the signal number is valid.
            let before = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut action) != 0 {
                    return Err(io::Error::last_os_error());
                }
                action
            };
            let guard = SignalGuard::install()?;
            // SAFETY: SIGTERM is handled by sigterm_handler for this process.
            if unsafe { libc::raise(libc::SIGTERM) } != 0 {
                return Err(io::Error::last_os_error());
            }
            assert!(SIGTERM_REQUESTED.load(Ordering::Relaxed));

            let drain = DrainState::new();
            let work = drain.begin_engine_work();
            assert!(matches!(&work, EngineWork::Rejected(Some(_))));
            assert_eq!(
                drain.queue_phase.load(Ordering::Acquire),
                QUEUE_RUNNING_IN_FLIGHT
            );
            drop(work);
            assert!(drain.stop_if_idle());
            assert_eq!(drain.queue_phase.load(Ordering::Acquire), QUEUE_STOPPING);
            guard.restore()?;

            // SAFETY: same query as above, after explicit restoration.
            let after = unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(libc::SIGTERM, std::ptr::null(), &mut action) != 0 {
                    return Err(io::Error::last_os_error());
                }
                action
            };
            assert_eq!(after.sa_sigaction, before.sa_sigaction);
            return Ok(());
        }

        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tests::signal_state_stops_new_work_and_handler_is_restored",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .spawn()?;
        let status = wait_for_child_exit(&mut child, TIMEOUT).inspect_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
        assert!(status.success(), "signal test child failed: {status}");
        Ok(())
    }

    #[test]
    fn shutdown_waits_for_in_flight_work_before_stopping() {
        let drain = Arc::new(DrainState::new());
        let EngineWork::Accepted(in_flight) = drain.begin_engine_work() else {
            panic!("request was unexpectedly rejected");
        };
        let stop_drain = Arc::clone(&drain);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let stopper = std::thread::spawn(move || {
            while !stop_drain.stop_if_idle() {
                std::thread::yield_now();
            }
            stopped_sender.send(()).unwrap();
        });

        assert!(
            stopped_receiver
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        assert_eq!(
            drain.queue_phase.load(Ordering::Acquire),
            QUEUE_RUNNING_IN_FLIGHT
        );
        drop(in_flight);
        stopped_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        stopper.join().unwrap();

        assert_eq!(drain.queue_phase.load(Ordering::Acquire), QUEUE_STOPPING);
        assert!(matches!(
            drain.begin_engine_work(),
            EngineWork::Rejected(None)
        ));
    }

    #[test]
    fn daemon_shutdown_propagates_each_error() {
        let error = |message| Err(io::Error::other(message));
        let combined = combine_daemon_results(
            error("run"),
            error("flush"),
            error("kill"),
            error("checkpoint"),
            error("ublkc remained"),
            error("sigaction"),
        )
        .unwrap_err();
        assert_eq!(
            combined.to_string(),
            "target failed: run; readiness failed: flush; stop failed: kill; shutdown failed: checkpoint; device removal failed: ublkc remained; signal restoration failed: sigaction"
        );
        assert!(combine_daemon_results(Ok(()), Ok(()), Ok(()), Ok(()), Ok(()), Ok(())).is_ok());
    }

    #[test]
    fn shutdown_reports_queue_and_close_errors() {
        assert!(combine_shutdown_results(Ok(()), Ok(())).is_ok());

        let queue_only =
            combine_shutdown_results(Err(io::Error::other("queue")), Ok(())).unwrap_err();
        assert_eq!(queue_only.to_string(), "queue");

        let close_only =
            combine_shutdown_results(Ok(()), Err(io::Error::other("close"))).unwrap_err();
        assert_eq!(close_only.to_string(), "close");

        let both = combine_shutdown_results(
            Err(io::Error::other("completion")),
            Err(io::Error::other("checkpoint")),
        )
        .unwrap_err();
        assert_eq!(
            both.to_string(),
            "queue failed: completion; volume close failed: checkpoint"
        );
    }
}
