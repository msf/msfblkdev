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
        .stderr(Stdio::from(output.try_clone()?))
        .process_group(0);
    let child = command.spawn()?;
    ManagedGroup::new(child).wait(deadline)
}

struct ManagedGroup {
    child: Child,
    process_group: libc::pid_t,
}

impl ManagedGroup {
    fn new(child: Child) -> Self {
        Self {
            process_group: child.id() as libc::pid_t,
            child,
        }
    }

    fn wait(mut self, deadline: Instant) -> io::Result<Outcome> {
        loop {
            if Instant::now() >= deadline {
                self.terminate_and_reap()?;
                return Ok(Outcome::Timeout);
            }
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.terminate_and_reap()?;
                    return Ok(Outcome::Exit(status));
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    let cleanup = self.terminate_and_reap();
                    return match cleanup {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(io::Error::other(format!(
                            "waiting for child failed: {error}; cleanup failed: {cleanup_error}"
                        ))),
                    };
                }
            }
        }
    }

    fn terminate_and_reap(&mut self) -> io::Result<()> {
        let mut first_error = signal_group(self.process_group, libc::SIGTERM).err();
        let grace_deadline = Instant::now() + TERMINATE_GRACE;
        while Instant::now() < grace_deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    first_error.get_or_insert(error);
                    break;
                }
            }
        }

        if let Err(error) = signal_group(self.process_group, libc::SIGKILL) {
            first_error.get_or_insert(error);
        }
        let kill_deadline = Instant::now() + REAP_GRACE;
        loop {
            match self.child.try_wait() {
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
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
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
