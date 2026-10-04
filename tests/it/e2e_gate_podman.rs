//! The gate's real flock boundary, using a fake podman and e2e command.
use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

use dagq::{
    application::{
        broker::{HostLock, MachineSpec, RECONNECT},
        install::{E2eSettings, PodmanCheck},
    },
    infrastructure::{broker_podman::FileLock, e2e_gate},
};

#[test]
fn a_busy_machine_lock_skips_broker_and_records_why() {
    let _test = crate::common::within(Duration::from_secs(90), "gate's bounded machine lock");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let podman = root.join("podman");
    fs::write(&podman, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
    let lock = FileLock::machine(root);
    let held = lock.hold().unwrap();
    // The holder stays alive through the entire gate; no sleep or racing
    // release is needed to exercise its deadline.
    let settings = E2eSettings {
        command: Some("echo \"skip [$DAGQ_E2E_SKIP]\"; echo 'test remaining ... ok'".into()),
        timeout: Duration::from_secs(10),
        cmux: None,
        run_env_root: None,
        queue_dir: None,
        scratch: root.join("scratch"),
        log: root.join("gate.log"),
        podman: Some(PodmanCheck {
            executable: Some(podman),
            machine: MachineSpec::default(),
            lock_home: root.into(),
            reconnect: RECONNECT,
        }),
        utc_offset_secs: 0,
        lock: None,
    };
    let outcome = e2e_gate::run(root, None, &settings).unwrap();
    assert!(outcome.passed, "{outcome:?}");
    let skipped = outcome.skipped.unwrap();
    assert_eq!(skipped.tests, ["broker::"]);
    assert!(
        skipped
            .reason
            .contains("timed out waiting for the machine lock after 30s"),
        "{skipped:?}"
    );
    let log = fs::read_to_string(&settings.log).unwrap();
    assert!(log.contains(&skipped.reason), "{log}");
    assert!(log.contains("skip [broker::]"), "{log}");
    assert!(lock.clone().within(Duration::ZERO).hold().is_err());
    drop(held);
    assert!(lock.within(Duration::ZERO).hold().is_ok());
}
