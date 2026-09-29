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
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout, format!("dagq-broker {}\n", dagq_broker::BUILD));
    assert_is_this_checkouts_build(dagq_broker::BUILD);
}

#[test]
fn prints_the_health_answer() {
    let output = broker(&["health"]);
    assert!(output.status.success());
    let health: HealthResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.protocol, PROTOCOL_VERSION);
    assert_eq!(health.build, dagq_broker::BUILD);
}

#[test]
fn refuses_unknown_arguments_with_status_2() {
    let output = broker(&["frobnicate"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown arguments"));
}

/// Assert that `build` is the build identifier of this checkout, by dagq's
/// rule over the repository root (ADR-t827-1 decisions 5 and 7), so it is
/// the one `dagq --version` of the same checkout prints. `.dirty` is not
/// compared: an edit after the build marks the tree dirty without a rebuild.
fn assert_is_this_checkouts_build(build: &str) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let expected = dagq_broker_protocol::build_id::compute(
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        &root,
    )
    .identifier;
    let clean = |id: &str| id.strip_suffix(".dirty").unwrap_or(id).to_owned();
    assert_eq!(clean(build), clean(&expected), "{build} is not {expected}");
    if dagq_broker_protocol::build_id::is_prerelease(env!("CARGO_PKG_VERSION")) {
        assert!(
            build.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")),
            "{build}"
        );
    } else {
        assert_eq!(build, env!("CARGO_PKG_VERSION"));
    }
}
