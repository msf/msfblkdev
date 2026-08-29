mod config;
mod evidence;
pub mod process;
mod suite;
pub mod ublk;
mod ublk_fio;

use config::Mode;
use std::io;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    match execute() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("block-storage-lab: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute() -> io::Result<ExitCode> {
    let mode = match config::parse_args(std::env::args_os()) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("{message}");
            return Ok(ExitCode::from(2));
        }
    };
    let config = match config::from_env(mode, |name| std::env::var_os(name)) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}");
            return Ok(ExitCode::from(2));
        }
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rust crate must have a repository parent");

    if mode == Mode::UblkFio {
        ublk_fio::run(repo, config.per_test)?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut evidence = if mode == Mode::EngineCrash {
        let log = evidence::Evidence::create(repo)?;
        println!("evidence: {}", log.path().display());
        Some(log)
    } else {
        None
    };
    let result = suite::run(config, repo, evidence.as_mut());
    match result {
        Ok(passed) => {
            if let Some(log) = evidence.as_mut() {
                log.line(if passed {
                    "result: PASS"
                } else {
                    "result: FAIL"
                })?;
            }
            Ok(if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(error) => {
            if let Some(log) = evidence.as_mut() {
                log.line(&format!("result: ERROR {error}"))?;
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod process_tests {
    use super::process::{self, Outcome};
    use std::fs::{self, OpenOptions};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn output_file(label: &str) -> (PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!(
            "block-storage-lab-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        (path, file)
    }

    fn test_command(name: &str, env_name: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture"])
            .env(env_name, "1");
        command
    }

    #[test]
    fn normal_exit_is_reported() {
        process::become_child_subreaper().unwrap();
        let (path, output) = output_file("normal");
        let outcome = process::run(
            &mut test_command(
                "process_tests::normal_child",
                "BLOCK_STORAGE_LAB_NORMAL_CHILD",
            ),
            &output,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Exit(status) if status.success()));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn timeout_is_reported() {
        process::become_child_subreaper().unwrap();
        let (path, output) = output_file("timeout");
        let outcome = process::run(
            &mut test_command(
                "process_tests::sleeping_child",
                "BLOCK_STORAGE_LAB_SLEEPING_CHILD",
            ),
            &output,
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Timeout));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn managed_child_supports_explicit_term_and_cleanup() {
        process::become_child_subreaper().unwrap();
        let mut command = test_command(
            "process_tests::sleeping_child",
            "BLOCK_STORAGE_LAB_SLEEPING_CHILD",
        );
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = process::ManagedChild::spawn(&mut command).unwrap();
        assert!(child.pid() > 0);
        assert!(child.try_wait().unwrap().is_none());
        child.terminate().unwrap();
        let outcome = child
            .wait_bounded(Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert!(matches!(outcome, Outcome::Exit(status) if status.signal() == Some(libc::SIGTERM)));
        child.cleanup().unwrap();
        child.cleanup().unwrap();
    }

    #[test]
    fn managed_child_supports_exact_kill() {
        process::become_child_subreaper().unwrap();
        let mut command = test_command(
            "process_tests::sleeping_child",
            "BLOCK_STORAGE_LAB_SLEEPING_CHILD",
        );
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = process::ManagedChild::spawn(&mut command).unwrap();
        child.kill().unwrap();
        let outcome = child
            .wait_bounded(Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert!(matches!(outcome, Outcome::Exit(status) if status.signal() == Some(libc::SIGKILL)));
        child.cleanup().unwrap();
    }

    #[test]
    fn managed_child_drop_reaps_the_process() {
        process::become_child_subreaper().unwrap();
        let mut command = test_command(
            "process_tests::sleeping_child",
            "BLOCK_STORAGE_LAB_SLEEPING_CHILD",
        );
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let pid = {
            let child = process::ManagedChild::spawn(&mut command).unwrap();
            child.pid()
        };
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "child {pid} survived ManagedChild::drop"
        );
    }

    #[test]
    fn timeout_cleans_descendant_process_group() {
        process::become_child_subreaper().unwrap();
        let handshake = std::env::temp_dir().join(format!(
            "block-storage-lab-handshake-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (output_path, output) = output_file("descendant");
        let mut command = test_command(
            "process_tests::descendant_parent_child",
            "BLOCK_STORAGE_LAB_DESCENDANT_PARENT",
        );
        command.env("BLOCK_STORAGE_LAB_HANDSHAKE", &handshake);
        let outcome = process::run(
            &mut command,
            &output,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Timeout));
        let descendant = fs::read_to_string(&handshake).unwrap();
        let descendant = descendant.trim();
        assert!(
            !Path::new("/proc").join(descendant).exists(),
            "descendant {descendant} survived group cleanup"
        );
        fs::remove_file(handshake).unwrap();
        fs::remove_file(output_path).unwrap();
    }

    #[test]
    fn normal_child() {}

    #[test]
    fn sleeping_child() {
        if std::env::var_os("BLOCK_STORAGE_LAB_SLEEPING_CHILD").is_some() {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    #[allow(clippy::zombie_processes)] // The parent is killed so the lab runner must adopt and reap this child.
    fn descendant_parent_child() {
        if std::env::var_os("BLOCK_STORAGE_LAB_DESCENDANT_PARENT").is_none() {
            return;
        }
        let handshake = std::env::var_os("BLOCK_STORAGE_LAB_HANDSHAKE").unwrap();
        let mut descendant = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_tests::descendant_leaf_child",
                "--nocapture",
            ])
            .env_remove("BLOCK_STORAGE_LAB_DESCENDANT_PARENT")
            .env("BLOCK_STORAGE_LAB_DESCENDANT_LEAF", "1")
            .env("BLOCK_STORAGE_LAB_HANDSHAKE", &handshake)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !Path::new(&handshake).exists() {
            assert!(Instant::now() < deadline, "descendant handshake timed out");
            if descendant.try_wait().unwrap().is_some() {
                panic!("descendant exited before handshake");
            }
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn descendant_leaf_child() {
        if std::env::var_os("BLOCK_STORAGE_LAB_DESCENDANT_LEAF").is_none() {
            return;
        }
        let handshake = std::env::var_os("BLOCK_STORAGE_LAB_HANDSHAKE").unwrap();
        fs::write(handshake, format!("{}\n", std::process::id())).unwrap();
        thread::sleep(Duration::from_secs(60));
    }
}
