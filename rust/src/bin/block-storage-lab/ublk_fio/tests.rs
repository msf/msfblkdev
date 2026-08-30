use super::daemon::*;
use super::lifecycle::*;
use super::scenario::Scenario;
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "block-storage-lab-lifecycle-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    root
}

fn fake_paths(root: &Path) -> Paths {
    Paths {
        repo: root.join("repo"),
        temp_root: root.join("tmp"),
        dev_root: root.join("dev"),
        sys_root: root.join("sys"),
        control: root.join("dev/ublk-control"),
        current_exe: root.join("bin/block-storage-lab"),
    }
}

fn current_exe_child(name: &str, env_name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env(env_name, "1");
    command
}

#[test]
fn command_construction_is_exact_and_bounded() {
    let daemon = Path::new("/x/block-storage-ublk");
    let backing = Path::new("/tmp/owned/backing.img");
    let format = format_command(daemon, backing);
    assert_eq!(
        format.get_args().collect::<Vec<_>>(),
        ["format", "/tmp/owned/backing.img", "286720", "262144"].map(OsStr::new)
    );
    assert_eq!(
        serve_command(daemon, backing)
            .get_args()
            .collect::<Vec<_>>(),
        ["serve", "/tmp/owned/backing.img", "-1"].map(OsStr::new)
    );
    assert_eq!(
        delete_command(daemon, 7).get_args().collect::<Vec<_>>(),
        ["delete", "7"].map(OsStr::new)
    );
    let fio = Scenario::Sequential.fio_jobs()[0].command(Path::new("/dev/ublkb7"));
    let args = fio
        .get_args()
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>();
    assert!(args.contains(&"--size=65536".into()));
    assert!(args.contains(&"--fsync=16".into()));
    assert!(args.contains(&"--do_verify=1".into()));
}

#[test]
fn control_preflight_reports_missing_node_with_operator_command() {
    let root = root("control-missing");
    let path = root.join("ublk-control");

    let error = validate_control_node_with(&path, |_| unreachable!()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "{} is missing; ask an operator to load the ublk driver with: sudo modprobe ublk_drv; no resources created",
            path.display()
        )
    );
    fs::remove_dir(root).unwrap();
}

#[test]
fn control_preflight_reports_wrong_node_type() {
    let root = root("control-type");
    let path = root.join("ublk-control");
    fs::write(&path, "not a device").unwrap();

    let error = validate_control_node_with(&path, |_| unreachable!()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "{} has the wrong node type; expected a character device; no resources created",
            path.display()
        )
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn control_preflight_reports_inaccessible_node_observations_and_remedy() {
    let path = Path::new("/dev/null");
    let metadata = fs::metadata(path).unwrap();

    let error = validate_control_node_with(path, |_| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test permission denial",
        ))
    })
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let owner = match (metadata.uid(), metadata.gid()) {
        (0, 0) => "0:0 (root:root)".to_owned(),
        (uid, gid) => format!("{uid}:{gid}"),
    };
    assert_eq!(
        error.to_string(),
        format!(
            "/dev/null is present but inaccessible: observed path=/dev/null mode={:04o} owner={owner}; open read/write failed: test permission denial; the already-built lab binary needs appropriate privilege or an operator-installed udev permission rule; no resources created",
            metadata.permissions().mode() & 0o7777,
        )
    );
}

#[test]
fn control_preflight_accepts_accessible_character_device() {
    validate_control_node_with(Path::new("/dev/null"), |path| {
        OpenOptions::new().read(true).write(true).open(path)
    })
    .unwrap();
}

#[test]
fn validates_owned_backing_path_identity_and_size() {
    let root = root("backing");
    let mut owned = OwnedTempDir::create(&root).unwrap();
    let path = owned.path().join("backing.img");
    File::create(&path)
        .unwrap()
        .set_len(Geometry::expected().backing_bytes())
        .unwrap();
    let backing =
        validate_backing(&path, &owned, Geometry::expected().backing_bytes(), None).unwrap();
    backing.validate(&owned).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(1)
        .unwrap();
    assert!(backing.validate(&owned).is_err());
    fs::remove_file(path).unwrap();
    owned.cleanup().unwrap();
    fs::remove_dir(root).unwrap();
}

#[test]
fn readiness_polls_fake_roots_and_current_exe_child() {
    let root = root("ready");
    let paths = fake_paths(&root);
    fs::create_dir_all(&paths.dev_root).unwrap();
    let stdout_path = root.join("ready.stdout");
    create_output(&stdout_path).unwrap();
    let mut command = current_exe_child(
        "ublk_fio::tests::readiness_writer_child",
        "BLOCK_STORAGE_READY_WRITER",
    );
    command
        .env("BLOCK_STORAGE_READY_PATH", &stdout_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    let identity = FileIdentity::from_metadata(&fs::metadata(&root).unwrap());
    let mut attempts = 0;
    let device = poll_readiness_with(
        &mut child,
        &stdout_path,
        &paths,
        Instant::now() + Duration::from_secs(2),
        |_, _| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::new(io::ErrorKind::NotFound, "not ready"))
            } else {
                Ok(identity)
            }
        },
    )
    .unwrap();
    assert_eq!(device.id(), 7);
    child.cleanup().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn readiness_reports_early_exit_and_timeout() {
    let root = root("ready-fail");
    let paths = fake_paths(&root);
    let early_path = root.join("early.stdout");
    create_output(&early_path).unwrap();
    let mut early = current_exe_child("ublk_fio::tests::empty_child", "BLOCK_STORAGE_EMPTY_CHILD");
    early.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut early).unwrap();
    let error = poll_readiness(
        &mut child,
        &early_path,
        &paths,
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap_err();
    assert!(error.to_string().contains("exited before readiness"));
    child.cleanup().unwrap();

    let timeout_path = root.join("timeout.stdout");
    create_output(&timeout_path).unwrap();
    let mut sleeping = current_exe_child(
        "ublk_fio::tests::sleeping_child",
        "BLOCK_STORAGE_READY_SLEEP",
    );
    sleeping.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut sleeping).unwrap();
    let error = poll_readiness(
        &mut child,
        &timeout_path,
        &paths,
        Instant::now() + Duration::from_millis(50),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    child.cleanup().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn kill_daemon_requires_and_records_exact_sigkill() {
    let mut command = current_exe_child(
        "ublk_fio::tests::sleeping_child",
        "BLOCK_STORAGE_READY_SLEEP",
    );
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut resources = Resources {
        child: Some(ManagedChild::spawn(&mut command).unwrap()),
        ..Resources::default()
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut evidence = Evidence::create_ublk(repo, "test").unwrap();
    let evidence_path = evidence.path().to_owned();

    scenario_run::kill_daemon(&mut resources, Duration::from_secs(2), &mut evidence).unwrap();

    assert!(resources.child.is_none());
    assert!(
        fs::read_to_string(&evidence_path)
            .unwrap()
            .contains(&format!("daemon SIGKILL: signal={}", libc::SIGKILL))
    );
    drop(evidence);
    fs::remove_file(evidence_path).unwrap();
}

#[test]
fn kill_daemon_rejects_a_different_unsuccessful_exit() {
    let mut command = current_exe_child(
        "ublk_fio::tests::unsuccessful_child",
        "BLOCK_STORAGE_UNSUCCESSFUL_CHILD",
    );
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    assert!(matches!(
        child
            .wait_bounded(Instant::now() + Duration::from_secs(2))
            .unwrap(),
        Outcome::Exit(status) if status.code() == Some(23)
    ));
    let mut resources = Resources {
        child: Some(child),
        ..Resources::default()
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut evidence = Evidence::create_ublk(repo, "test").unwrap();
    let evidence_path = evidence.path().to_owned();

    let error = scenario_run::kill_daemon(&mut resources, Duration::from_secs(2), &mut evidence)
        .unwrap_err();

    assert!(error.to_string().contains("SIGKILL daemon exit"));
    resources.child.as_mut().unwrap().cleanup().unwrap();
    resources.child = None;
    drop(evidence);
    fs::remove_file(evidence_path).unwrap();
}

#[test]
fn device_identity_states_distinguish_match_absence_and_change() {
    let root = root("identity-states");
    let paths = fake_paths(&root);
    let recorded_identity = FileIdentity::from_metadata(&fs::metadata(&root).unwrap());
    fs::create_dir(&paths.dev_root).unwrap();
    let other = paths.dev_root.join("other");
    fs::write(&other, "different inode").unwrap();
    let changed_identity = FileIdentity::from_metadata(&fs::metadata(other).unwrap());
    let device = ValidatedDevice {
        path: "/dev/ublkb7".parse().unwrap(),
        class_identity: recorded_identity,
    };

    assert_eq!(
        device
            .classify_identity(&paths, Ok(recorded_identity))
            .unwrap(),
        DeviceIdentityState::Matching
    );
    assert_eq!(
        device
            .classify_identity(&paths, Ok(changed_identity))
            .unwrap(),
        DeviceIdentityState::IdentityChanged
    );
    assert_eq!(
        device
            .classify_identity(&paths, Err(io::Error::new(io::ErrorKind::NotFound, "gone")),)
            .unwrap(),
        DeviceIdentityState::Absent
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ambiguous_present_identity_is_not_absence() {
    let root = root("identity-ambiguous");
    let paths = fake_paths(&root);
    fs::create_dir(&paths.dev_root).unwrap();
    fs::write(paths.dev_root.join("ublkb7"), "present").unwrap();
    let device = ValidatedDevice {
        path: "/dev/ublkb7".parse().unwrap(),
        class_identity: FileIdentity::from_metadata(&fs::metadata(&root).unwrap()),
    };
    let error = device
        .classify_identity(
            &paths,
            Err(io::Error::new(io::ErrorKind::InvalidData, "ambiguous")),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot prove recorded device identity")
    );
    fs::remove_dir_all(root).unwrap();
}

fn assert_unsafe_identity_cleanup_preserves(label: &str, ambiguous: bool) {
    let root = root(label);
    let paths = fake_paths(&root);
    let owned = OwnedTempDir::create(&root).unwrap();
    let backing_path = owned.path().join("backing.img");
    File::create(&backing_path)
        .unwrap()
        .set_len(Geometry::expected().backing_bytes())
        .unwrap();
    let backing = validate_backing(
        &backing_path,
        &owned,
        Geometry::expected().backing_bytes(),
        None,
    )
    .unwrap();
    let preserved_path = owned.path().to_owned();
    let identity = FileIdentity::from_metadata(&fs::metadata(&root).unwrap());
    let marker = root.join("delete-was-invoked");
    let daemon = root.join("delete-daemon");
    fs::write(&daemon, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    fs::set_permissions(&daemon, fs::Permissions::from_mode(0o700)).unwrap();
    let preflight = Preflight {
        daemon,
        fio_version: "test".to_owned(),
    };
    let mut resources = Resources {
        device: Some(ValidatedDevice {
            path: "/dev/ublkb7".parse().unwrap(),
            class_identity: identity,
        }),
        temp: Some(owned),
        backing: Some(backing),
        ..Resources::default()
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut evidence = Evidence::create_ublk(repo, "test").unwrap();
    let evidence_path = evidence.path().to_owned();

    let error = cleanup_error_with(
        &paths,
        &preflight,
        Duration::from_millis(50),
        &mut resources,
        &mut evidence,
        |_, _| {
            if ambiguous {
                Err(io::Error::other("identity is ambiguous"))
            } else {
                Ok(DeviceIdentityState::IdentityChanged)
            }
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("refusing delete"));
    assert!(preserved_path.join("backing.img").exists());
    assert!(resources.temp.is_none() && resources.backing.is_none());
    assert!(!marker.exists(), "delete command ran for unsafe identity");
    fs::remove_file(evidence_path).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cleanup_refuses_changed_identity_and_preserves_backing() {
    assert_unsafe_identity_cleanup_preserves("cleanup-changed", false);
}

#[test]
fn cleanup_refuses_ambiguous_identity_and_preserves_backing() {
    assert_unsafe_identity_cleanup_preserves("cleanup-ambiguous", true);
}

#[test]
fn preflight_failure_creates_no_evidence_temp_or_device_resource() {
    let root = root("preflight");
    let paths = fake_paths(&root);
    fs::create_dir_all(paths.current_exe.parent().unwrap()).unwrap();
    fs::create_dir_all(&paths.repo).unwrap();
    fs::create_dir_all(&paths.temp_root).unwrap();
    fs::copy(std::env::current_exe().unwrap(), &paths.current_exe).unwrap();
    fs::copy(
        std::env::current_exe().unwrap(),
        paths
            .current_exe
            .parent()
            .unwrap()
            .join("block-storage-ublk"),
    )
    .unwrap();
    let error = run_at(paths.clone(), Duration::from_secs(2)).unwrap_err();
    assert!(error.to_string().contains("no resources created"));
    assert!(!paths.repo.join("evidence").exists());
    assert_eq!(fs::read_dir(&paths.temp_root).unwrap().count(), 0);
    assert!(!paths.sys_root.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn readiness_writer_child() {
    if std::env::var_os("BLOCK_STORAGE_READY_WRITER").is_some() {
        fs::write(
            std::env::var_os("BLOCK_STORAGE_READY_PATH").unwrap(),
            "/dev/ublkb7\n",
        )
        .unwrap();
        thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn empty_child() {}

#[test]
fn unsuccessful_child() {
    if std::env::var_os("BLOCK_STORAGE_UNSUCCESSFUL_CHILD").is_some() {
        std::process::exit(23);
    }
}

#[test]
fn sleeping_child() {
    if std::env::var_os("BLOCK_STORAGE_READY_SLEEP").is_some() {
        thread::sleep(Duration::from_secs(60));
    }
}
