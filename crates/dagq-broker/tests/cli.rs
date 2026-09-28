//! The `dagq-broker` binary as a host process (no podman).

use std::process::Command;

use dagq_broker_protocol::{HealthResponse, PROTOCOL_VERSION};

fn broker(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dagq-broker"))
        .args(args)
        .output()
        .expect("run dagq-broker")
}

#[test]
fn prints_its_version() {
    let output = broker(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("dagq-broker {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn prints_the_health_answer() {
    let output = broker(&["health"]);
    assert!(output.status.success());
    let health: HealthResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.protocol, PROTOCOL_VERSION);
}

#[test]
fn refuses_unknown_arguments_with_status_2() {
    let output = broker(&["serve"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown arguments"));
}
