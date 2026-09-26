//! What only dagq's own development needs runs only in dagq's source
//! repository (ADR-t614-1), the one whose root `Cargo.toml` has a
//! `[package]` named `dagq`: `install` without `--from` and `up
//! --auto-update` refuse any other repository, with or without a
//! `Cargo.toml` or a `migrations/` of its own. integrate's migration
//! renumbering is in `runtime_integrate`, and the supervisor's automatic
//! update in `cli_version`.
use crate::common;

use common::{
    Bounded,
    lifecycle::{FakeCmux, FakeLaunchd, FakeProcesses, fixture, git, try_up},
};
use dagq::infrastructure::sqlite::SqliteQueue;
use std::{fs, path::Path, process::Command};

fn manifest(repo: &Path, package: &str) {
    fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
}

/// `dagq install` without `--from`, run in `repo` with a home of its own;
/// its stderr and whether it succeeded.
fn install(repo: &Path, home: &Path) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .current_dir(repo)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("HOME", home)
        .env_remove("DAGQ_ROLE")
        .args(["install", "--to"])
        .arg(home.join("bin").join("dagq"))
        .bounded_output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Without `--from`, `install` builds only dagq's source: another
/// repository (no `Cargo.toml`, or another package with migrations of its
/// own) is refused before any build, with the ways to update dagq there.
/// dagq's source gets past the check to its build (which fails here, as
/// the fixture has no sources).
#[test]
fn install_without_from_builds_only_dagqs_source() {
    let fixture = fixture();
    let repo = &fixture.repo;
    let home = fixture._dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let refused = |stderr: &str| {
        stderr.contains("is not dagq's source")
            && stderr.contains("cargo install dagq")
            && stderr.contains("--from")
    };

    let (success, stderr) = install(repo, &home);
    assert!(!success && refused(&stderr), "{stderr}");

    manifest(repo, "myapp");
    fs::create_dir(repo.join("migrations")).unwrap();
    fs::write(repo.join("migrations/0001_init.sql"), "-- init\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "myapp"]);
    let (success, stderr) = install(repo, &home);
    assert!(!success && refused(&stderr), "{stderr}");
    assert!(!home.join("bin").exists());

    manifest(repo, "dagq");
    let (_, stderr) = install(repo, &home);
    assert!(!refused(&stderr), "{stderr}");
}

/// `up --auto-update` starts no supervisor outside dagq's source; in it,
/// the supervisor starts with the automatic update on.
#[test]
fn up_auto_update_needs_dagqs_source() {
    let mut fixture = fixture();
    fixture.options.auto_update = true;
    let cmux = FakeCmux::default();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    for package in [None, Some("myapp")] {
        if let Some(package) = package {
            manifest(&fixture.repo, package);
        }
        let error = format!(
            "{:#}",
            try_up(&fixture, &cmux, &launchd, &processes).unwrap_err()
        );
        assert!(
            error.contains("--auto-update")
                && error.contains("is not dagq's source")
                && error.contains("cargo install dagq")
                && error.contains("the supervisor was not started"),
            "{error}"
        );
        assert!(
            SqliteQueue::open(&fixture.location.db)
                .unwrap()
                .supervisors()
                .unwrap()
                .is_empty()
        );
    }

    manifest(&fixture.repo, "dagq");
    let report = try_up(&fixture, &cmux, &launchd, &processes).unwrap();
    assert_eq!(report["supervisor"]["outcome"], "started", "{report}");
    assert_eq!(report["supervisor"]["auto_update"], true, "{report}");
}
