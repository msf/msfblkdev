use std::fs::File;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERMINATE_GRACE: Duration = Duration::from_secs(2);
const REAP_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum Outcome {
    Exit(ExitStatus),
    Timeout,
}

pub fn become_child_subreaper() -> io::Result<()> {
    // SAFETY: prctl with PR_SET_CHILD_SUBREAPER only changes process child-reaping behavior.
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn run(command: &mut Command, output: &File, deadline: Instant) -> io::Result<Outcome> {
    command
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output.try_clone()?));
    let mut child = ManagedChild::spawn(command)?;
    let outcome = match child.wait_bounded(deadline) {
        Ok(outcome) => outcome,
        Err(error) => return cleanup_after_error(&mut child, "waiting for child", error),
    };
    child.cleanup()?;
    Ok(outcome)
}

/// Owns a process and the new process group created for it.
///
/// Call `cleanup` when the process is no longer needed. `Drop` is a best-effort
/// fallback which applies the same TERM, KILL, and reap sequence.
pub struct ManagedChild {
    child: Child,
    process_group: libc::pid_t,
    status: Option<ExitStatus>,
    cleaned: bool,
}

impl ManagedChild {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        become_child_subreaper()?;
        command.process_group(0);
        let child = command.spawn()?;
        Ok(Self {
            process_group: child.id() as libc::pid_t,
            child,
            status: None,
            cleaned: false,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status)
    }

    pub fn wait_bounded(&mut self, deadline: Instant) -> io::Result<Outcome> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Outcome::Exit(status));
            }
            if Instant::now() >= deadline {
                return Ok(Outcome::Timeout);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Sends SIGTERM to exactly this child's process group.
    pub fn terminate(&self) -> io::Result<()> {
        signal_group(self.process_group, libc::SIGTERM)
    }

    /// Sends SIGKILL to exactly this child's process group.
    pub fn kill(&self) -> io::Result<()> {
        signal_group(self.process_group, libc::SIGKILL)
    }

    /// Terminates and reaps the group leader and all adopted descendants.
    pub fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }

        let mut first_error = self.terminate().err();
        let grace_deadline = Instant::now() + TERMINATE_GRACE;
        while Instant::now() < grace_deadline {
            match self.try_wait() {
                Ok(Some(_)) => {
                    if let Err(error) = reap_group_available(self.process_group) {
                        first_error.get_or_insert(error);
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                    break;
                }
            }
            match group_exists(self.process_group) {
                Ok(false) => break,
                Ok(true) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    first_error.get_or_insert(error);
                    break;
                }
            }
        }

        if let Err(error) = self.kill() {
            first_error.get_or_insert(error);
        }
        let kill_deadline = Instant::now() + REAP_GRACE;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < kill_deadline => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    first_error.get_or_insert_with(|| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("process-group leader {} did not reap", self.process_group),
                        )
                    });
                    break;
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                    break;
                }
            }
        }
        if let Err(error) = reap_group(self.process_group) {
            first_error.get_or_insert(error);
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            self.cleaned = true;
            Ok(())
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = self.cleanup()
        {
            eprintln!(
                "failed to clean process group {} on drop: {error}",
                self.process_group
            );
        }
    }
}

fn cleanup_after_error<T>(
    child: &mut ManagedChild,
    operation: &str,
    error: io::Error,
) -> io::Result<T> {
    match child.cleanup() {
        Ok(()) => Err(error),
        Err(cleanup_error) => Err(io::Error::other(format!(
            "{operation} failed: {error}; cleanup failed: {cleanup_error}"
        ))),
    }
}

fn signal_group(process_group: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: a negative, test-created process-group ID restricts the signal to that group.
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn group_exists(process_group: libc::pid_t) -> io::Result<bool> {
    // SAFETY: signal zero performs existence/permission checking without sending a signal.
    let result = unsafe { libc::kill(-process_group, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error),
    }
}

fn reap_group_available(process_group: libc::pid_t) -> io::Result<()> {
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes to status and is limited to adopted children in this group.
        let result = unsafe { libc::waitpid(-process_group, &mut status, libc::WNOHANG) };
        if result > 0 {
            continue;
        }
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(()),
            _ if error.kind() == io::ErrorKind::Interrupted => continue,
            _ => return Err(error),
        }
    }
}

fn reap_group(process_group: libc::pid_t) -> io::Result<()> {
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes to status and is limited to adopted children in this group.
        let result = unsafe { libc::waitpid(-process_group, &mut status, libc::WNOHANG) };
        if result > 0 {
            continue;
        }
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process group {process_group} did not reap"),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}
