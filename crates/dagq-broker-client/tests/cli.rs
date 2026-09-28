//! The `dagq-broker-client` binary as a host process.

use std::process::Command;

#[test]
fn prints_its_version_and_refuses_unknown_arguments() {
    let binary = env!("CARGO_BIN_EXE_dagq-broker-client");
    let output = Command::new(binary).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("dagq-broker-client {}\n", env!("CARGO_PKG_VERSION"))
    );
    let output = Command::new(binary).arg("mcp").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}
