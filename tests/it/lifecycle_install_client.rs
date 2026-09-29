//! `dagq install` puts the worker's broker client in place with dagq
//! (ADR-t827-1 decision 5): the client beside the source is checked to name
//! dagq's build before anything is replaced, goes in place first, keeps its
//! `.previous`, and goes back with dagq; a rollback puts both back. The
//! binaries here are files whose contents are the build they name, moved by
//! the real renames of `LocalBinaries`.

use crate::common;

use anyhow::{Result, bail};
use dagq::application::install::{
    Binaries, E2eGate, InstallOptions, Ports, SchemaCheck, Source, install,
};
use dagq::infrastructure::binaries::LocalBinaries;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Binaries whose version is their file's contents; a dagq whose contents
/// say `broken` fails its probe, and a replace of a target named in
/// `replace_fails` fails. Replace, restore and set aside are the real
/// renames.
#[derive(Default)]
struct FileBinaries {
    replace_fails: Option<PathBuf>,
    calls: Mutex<Vec<String>>,
}

impl Binaries for FileBinaries {
    fn build(&self, _: &Path) -> Result<PathBuf> {
        bail!("no build here")
    }
    fn version(&self, binary: &Path) -> Result<String> {
        Ok(fs::read_to_string(binary)?.trim().to_owned())
    }
    fn probe(&self, binary: &Path) -> Result<()> {
        if fs::read_to_string(binary)?.contains("broken") {
            bail!("it does not start");
        }
        Ok(())
    }
    fn takes_handoff(&self, _: &Path) -> bool {
        true
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<SchemaCheck> {
        bail!("no queue here")
    }
    fn migrate(&self, _: &Path, _: &Path) -> Result<Value> {
        bail!("no queue here")
    }
    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("replace {}", name(target)));
        if self.replace_fails.as_deref() == Some(target) {
            bail!("the disk is full");
        }
        LocalBinaries.replace(source, target)
    }
    fn restore(&self, target: &Path) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("restore {}", name(target)));
        LocalBinaries.restore(target)
    }
    fn set_aside(&self, target: &Path) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("set_aside {}", name(target)));
        LocalBinaries.set_aside(target)
    }
    fn run(&self, _: &Path, _: &[String]) -> Result<Value> {
        bail!("no up here")
    }
}

fn name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// A temporary dir with the fixed pair at `bin/` naming `old` (no client
/// when `old_client` is `None`) and a build at `new/`.
struct Dirs {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    new: PathBuf,
}

impl Dirs {
    fn new(old: &str, old_client: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let new = dir.path().join("new");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(bin.join("dagq"), old).unwrap();
        if let Some(client) = old_client {
            fs::write(bin.join("dagq-broker-client"), client).unwrap();
        }
        Self {
            _dir: dir,
            bin,
            new,
        }
    }

    fn build(&self, dagq: &str, client: Option<&str>) -> PathBuf {
        fs::write(self.new.join("dagq"), dagq).unwrap();
        if let Some(client) = client {
            fs::write(self.new.join("dagq-broker-client"), client).unwrap();
        }
        self.new.join("dagq")
    }

    /// The contents of `bin/<file>`, or `None` when there is none.
    fn at(&self, file: &str) -> Option<String> {
        fs::read_to_string(self.bin.join(file)).ok()
    }
}

fn run(binaries: &FileBinaries, dirs: &Dirs, source: Source) -> Result<Value> {
    let queues =
        |_: &Path| -> Arc<dyn dagq::application::QueueOpener> { unreachable!("no queue here") };
    let down = || -> Result<Value> { unreachable!("no drain here") };
    install(
        &Ports {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes: &dagq::infrastructure::adapters::SystemProcesses,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            down: &down,
        },
        None,
        &InstallOptions {
            source,
            target: dirs.bin.join("dagq"),
            allow_breaking: false,
            restart: Vec::new(),
            handoff_timeout: Duration::from_secs(1),
            poll: Duration::from_millis(10),
            e2e: E2eGate::NotApplicable,
        },
    )
}

/// dagq and its client of one build go in place together, the client
/// first, each keeping the one it replaced as `.previous`; `--rollback`
/// puts both back.
#[test]
fn install_puts_dagq_and_its_client_in_place_together_and_rollback_puts_both_back() {
    let _test = common::test();
    let dirs = Dirs::new("0.0.1", Some("0.0.1"));
    let binaries = FileBinaries::default();
    let built = dirs.build("0.0.2", Some("0.0.2"));
    let report = run(&binaries, &dirs, Source::Binary(built)).unwrap();
    assert_eq!(report["version"], "0.0.2", "{report}");
    assert_eq!(report["client"]["outcome"], "replaced", "{report}");
    assert_eq!(
        *binaries.calls.lock().unwrap(),
        ["replace dagq-broker-client", "replace dagq"]
    );
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.2"));
    assert_eq!(dirs.at("dagq-broker-client").as_deref(), Some("0.0.2"));
    assert_eq!(dirs.at("dagq.previous").as_deref(), Some("0.0.1"));
    assert_eq!(
        dirs.at("dagq-broker-client.previous").as_deref(),
        Some("0.0.1")
    );

    let report = run(&binaries, &dirs, Source::Rollback).unwrap();
    assert_eq!(report["version"], "0.0.1", "{report}");
    assert_eq!(report["client"]["outcome"], "replaced", "{report}");
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq.previous").as_deref(), Some("0.0.2"));
    assert_eq!(
        dirs.at("dagq-broker-client.previous").as_deref(),
        Some("0.0.2")
    );
}

/// A build dagq made with its client (a checkout's, the automatic
/// update's, a release) whose client names another build, or a dagq that
/// does not start, replaces neither of them.
#[test]
fn a_failed_check_of_either_binary_replaces_neither() {
    let _test = common::test();
    let dirs = Dirs::new("0.0.1", Some("0.0.1"));
    let binaries = FileBinaries::default();
    let built = dirs.build("0.0.2", Some("0.0.9"));
    let error = run(&binaries, &dirs, Source::Built(built)).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("0.0.9") && message.contains("nothing was replaced"),
        "{message}"
    );

    let built = dirs.build("0.0.2 broken", Some("0.0.2 broken"));
    let error = run(&binaries, &dirs, Source::Built(built)).unwrap_err();
    assert!(format!("{error:#}").contains("does not start"), "{error:#}");

    assert!(binaries.calls.lock().unwrap().is_empty());
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq.previous"), None);
    assert_eq!(dirs.at("dagq-broker-client.previous"), None);
}

/// A binary a person gives, or a rollback, whose neighbouring client names
/// another build (left in a target directory by an earlier build, say)
/// goes in place without it: the client there is set aside, so none of
/// another build stays beside dagq, and the report names the one left.
#[test]
fn a_client_of_another_build_beside_a_given_binary_is_left_behind() {
    let _test = common::test();
    let dirs = Dirs::new("0.0.1", Some("0.0.1"));
    let binaries = FileBinaries::default();
    let built = dirs.build("0.0.2", Some("0.0.9"));
    let report = run(&binaries, &dirs, Source::Binary(built)).unwrap();
    assert_eq!(report["client"]["outcome"], "set_aside", "{report}");
    assert!(
        report["client"]["ignored"]["reason"]
            .as_str()
            .unwrap()
            .contains("0.0.9"),
        "{report}"
    );
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.2"));
    assert_eq!(dirs.at("dagq-broker-client"), None);

    // The kept client is not the kept dagq's build: the rollback puts dagq
    // back and leaves no client beside it.
    fs::write(dirs.bin.join("dagq-broker-client.previous"), "0.0.5").unwrap();
    let report = run(&binaries, &dirs, Source::Rollback).unwrap();
    assert_eq!(report["version"], "0.0.1", "{report}");
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client"), None);
}

/// A `.previous` client left from before is not paired with the dagq that
/// goes to `.previous` when no client was in place: install B with its
/// client over A without one, roll back, install B again; the second
/// rollback still puts A back, with no client.
#[test]
fn a_stale_previous_client_does_not_pair_with_the_previous_dagq() {
    let _test = common::test();
    let dirs = Dirs::new("0.0.1", None);
    let binaries = FileBinaries::default();
    let built = dirs.build("0.0.2", Some("0.0.2"));
    run(&binaries, &dirs, Source::Built(built.clone())).unwrap();
    run(&binaries, &dirs, Source::Rollback).unwrap();
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client"), None);
    run(&binaries, &dirs, Source::Built(built)).unwrap();
    assert_eq!(dirs.at("dagq-broker-client").as_deref(), Some("0.0.2"));
    assert_eq!(dirs.at("dagq-broker-client.previous"), None);
    run(&binaries, &dirs, Source::Rollback).unwrap();
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client"), None);
}

/// A dagq without a client (a build before the client, or a release
/// without it) sets the client there aside, so no client of another build
/// stays beside dagq; the rollback puts the pair back.
#[test]
fn a_dagq_without_a_client_sets_the_old_client_aside() {
    let _test = common::test();
    let dirs = Dirs::new("0.0.1", Some("0.0.1"));
    let binaries = FileBinaries::default();
    let built = dirs.build("0.0.2", None);
    let report = run(&binaries, &dirs, Source::Binary(built)).unwrap();
    assert_eq!(report["client"]["outcome"], "set_aside", "{report}");
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.2"));
    assert_eq!(dirs.at("dagq-broker-client"), None);
    assert_eq!(
        dirs.at("dagq-broker-client.previous").as_deref(),
        Some("0.0.1")
    );

    run(&binaries, &dirs, Source::Rollback).unwrap();
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client").as_deref(), Some("0.0.1"));

    // Neither side has a client: nothing about one changes.
    let dirs = Dirs::new("0.0.1", None);
    let built = dirs.build("0.0.2", None);
    let report = run(&binaries, &dirs, Source::Binary(built)).unwrap();
    assert_eq!(report["client"]["outcome"], "absent", "{report}");
    assert_eq!(dirs.at("dagq-broker-client"), None);
    assert_eq!(dirs.at("dagq-broker-client.previous"), None);
}

/// When dagq cannot be put in place after its client was, the client goes
/// back: neither is left replaced alone.
#[test]
fn a_dagq_that_cannot_be_put_in_place_takes_its_client_back() {
    let _test = common::test();
    let dirs = Dirs::new("0.0.1", Some("0.0.1"));
    let binaries = FileBinaries {
        replace_fails: Some(dirs.bin.join("dagq")),
        ..Default::default()
    };
    let built = dirs.build("0.0.2", Some("0.0.2"));
    let error = run(&binaries, &dirs, Source::Binary(built)).unwrap_err();
    assert!(format!("{error:#}").contains("disk is full"), "{error:#}");
    assert_eq!(
        *binaries.calls.lock().unwrap(),
        [
            "replace dagq-broker-client",
            "replace dagq",
            "restore dagq-broker-client"
        ]
    );
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client").as_deref(), Some("0.0.1"));

    // No client was there: the new one goes rather than stay beside the
    // old dagq.
    let dirs = Dirs::new("0.0.1", None);
    let binaries = FileBinaries {
        replace_fails: Some(dirs.bin.join("dagq")),
        ..Default::default()
    };
    let built = dirs.build("0.0.2", Some("0.0.2"));
    run(&binaries, &dirs, Source::Binary(built)).unwrap_err();
    assert_eq!(dirs.at("dagq").as_deref(), Some("0.0.1"));
    assert_eq!(dirs.at("dagq-broker-client"), None);
}
