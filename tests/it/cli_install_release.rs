//! `dagq install --release VERSION` (ADR-t618-1 decision 6): a person's
//! way to put a release of crates.io in place, through the same `cargo
//! install` under the queue's `update/release` and the same check and swap
//! as `install --from`. A stub `cargo` stands in for crates.io.

use crate::common;

use common::cli::*;

use std::{fs, path::Path};

/// A stub `cargo` that appends its arguments to `<stub>.args` and, unless
/// `fails`, leaves the test's dagq binary at `<--root>/bin/dagq` (and no
/// client, as a release without the client on crates.io).
fn stub_cargo(dir: &Path, fails: bool) -> std::path::PathBuf {
    let cargo = dir.join(if fails { "cargo-fails" } else { "cargo" });
    let install = if fails {
        "echo 'error: toolchain 1.95 is required' >&2; exit 101".to_owned()
    } else {
        format!(
            "root=''\nwhile [ $# -gt 0 ]; do [ \"$1\" = --root ] && root=\"$2\"; shift; done\nmkdir -p \"$root/bin\" && cp {} \"$root/bin/dagq\"",
            crate::common::shell_path(env!("CARGO_BIN_EXE_dagq"))
        )
    };
    crate::common::template::script(
        &cargo,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$0.args\"\n{install}\n"),
    );

    cargo
}

#[test]
fn install_release_installs_with_cargo_and_puts_the_binary_in_place() {
    let (dir, db) = queue();
    let fixed = dir.path().join("fixed").join("dagq");
    fs::create_dir_all(fixed.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_dagq"), &fixed).unwrap();
    let previous = dir.path().join("fixed").join("dagq.previous");

    // cargo fails: nothing is replaced.
    let failing = stub_cargo(dir.path(), true);
    let output = invoke(
        &db,
        &[
            "install",
            "--release",
            "9.9.9",
            "--to",
            fixed.to_str().unwrap(),
            "--cargo",
            failing.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("install release 9.9.9") && stderr.contains("exited with"),
        "{stderr}"
    );
    assert!(!previous.exists());

    let cargo = stub_cargo(dir.path(), false);
    let report = ok(
        &db,
        &[
            "install",
            "--to",
            fixed.to_str().unwrap(),
            "--release",
            "9.9.9",
            "--cargo",
            cargo.to_str().unwrap(),
        ],
    );
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["release"], "9.9.9", "{report}");
    assert_eq!(report["version"], dagq::VERSION);
    assert_eq!(report["previous"], previous.to_str().unwrap());
    assert!(previous.is_file());
    let queue_dir = db.parent().unwrap().to_path_buf();
    // dagq, then the worker's client of the same release beside it
    // (ADR-t827-1 decision 8).
    let args = fs::read_to_string(dir.path().join("cargo.args")).unwrap();
    let call = |package: &str| {
        format!(
            "install\n--locked\n{package}@9.9.9\n--root\n{}\n--target-dir\n{}\n",
            queue_dir.join("update/release").display(),
            queue_dir.join("update/target").display()
        )
    };
    assert_eq!(args, call("dagq") + &call("dagq-broker-client"));
    assert!(queue_dir.join("update/release/bin/dagq").is_file());
    // The release left no client, so none stays beside the fixed dagq.
    assert_eq!(report["client"]["outcome"], "absent", "{report}");
    assert!(!dir.path().join("fixed/dagq-broker-client").exists());

    // A version that is not a release is refused before cargo runs.
    let output = invoke(
        &db,
        &[
            "install",
            "--release",
            "1.0.0-rc.1",
            "--to",
            fixed.to_str().unwrap(),
            "--cargo",
            failing.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a release version"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // --release and --from name two sources.
    let output = invoke(
        &db,
        &["install", "--release", "--from", fixed.to_str().unwrap()],
    );
    assert!(!output.status.success());
}
