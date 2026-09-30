//! `dagq install` and the auto-update job against fake binaries, launchd,
//! cmux and process signals: the migrate, replace and handoff, the restore
//! on a failed handoff, the drain for a breaking migration, and the job's
//! watch, restore and asks.

use crate::common;
use dagq::domain::LeaseToken;

use common::lifecycle::*;

use anyhow::{Result, bail};
use dagq::application::install::{E2eGate, E2eOutcome};
use dagq::{
    VERSION,
    domain::{
        SupervisorMode,
        slot_limits::{Setting, SettingSource, SlotLimits},
    },
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::Duration,
};

/// [`Binaries`] that build, run and move nothing: what each call was, the
/// schema it reports, and whether it answers a probe.
struct FakeBinaries {
    schema: dagq::application::install::SchemaCheck,
    calls: Mutex<Vec<String>>,
    up: Mutex<Vec<Vec<String>>>,
    /// How the e2e of a checkout goes: passes unless set.
    e2e: Mutex<Option<E2eOutcome>>,
}

impl FakeBinaries {
    fn new(pending: &[(i64, bool)], opens: bool) -> Self {
        Self {
            schema: dagq::application::install::SchemaCheck {
                pending: pending
                    .iter()
                    .map(
                        |&(version, compatible)| dagq::application::install::PendingMigration {
                            version,
                            compatible,
                        },
                    )
                    .collect(),
                opens,
            },
            calls: Mutex::default(),
            up: Mutex::default(),
            e2e: Mutex::default(),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

/// An e2e that passed in `secs`.
fn e2e_passed(secs: u64) -> E2eOutcome {
    E2eOutcome {
        passed: true,
        secs,
        cleanup: json!({"removed": true}),
        ..Default::default()
    }
}

/// An e2e whose `failed` tests failed, rerun by name, with `still` failing
/// again, and the marks of `marks` (`(name, until)`) (ADR-t1165-1).
fn e2e_rerun(failed: &[&str], still: &[&str], marks: &[(&str, &str)]) -> E2eOutcome {
    let names = |tests: &[&str]| tests.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
    let text: String = marks
        .iter()
        .map(|(name, until)| {
            format!(
                "[[test]]\nname = \"{name}\"\nreason = \"flaky\"\ntask = 1120\nuntil = {until}\n"
            )
        })
        .collect();
    E2eOutcome {
        failed_tests: names(failed),
        secs: 200,
        cleanup: json!({"removed": true}),
        rerun: Some(dagq::application::install::E2eRerun {
            tests: names(failed),
            failed: names(still),
            secs: 30,
            cleanup: json!({"removed": true}),
            ..Default::default()
        }),
        quarantine: if marks.is_empty() {
            Default::default()
        } else {
            dagq::domain::e2e_quarantine::QuarantineFile::of(&text)
        },
        ..Default::default()
    }
}

/// The e2e gate's settings of the tests: nothing runs them.
fn e2e_settings(dir: &Path) -> dagq::application::install::E2eSettings {
    dagq::application::install::E2eSettings {
        command: None,
        timeout: Duration::from_secs(1800),
        cmux: Some("/opt/cmux".into()),
        run_env_root: None,
        queue_dir: None,
        scratch: dir.join("e2e"),
        log: dir.join("e2e.log"),
        podman: None,
        utc_offset_secs: 0,
    }
}

impl dagq::application::install::Binaries for FakeBinaries {
    fn build(&self, checkout: &Path) -> Result<PathBuf> {
        self.note(format!("build {}", checkout.display()));
        Ok(checkout.join("target/release/dagq"))
    }
    fn version(&self, binary: &Path) -> Result<String> {
        Ok(if binary.ends_with("dagq.previous") {
            "0.0.1".into()
        } else {
            VERSION.into()
        })
    }
    fn probe(&self, binary: &Path) -> Result<()> {
        self.note(format!("probe {}", binary.display()));
        Ok(())
    }
    fn takes_handoff(&self, binary: &Path) -> bool {
        !binary.ends_with("old/dagq")
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<dagq::application::install::SchemaCheck> {
        Ok(self.schema.clone())
    }
    fn migrate(&self, binary: &Path, _: &Path) -> Result<Value> {
        self.note(format!("migrate {}", binary.display()));
        Ok(json!({"applied": self.schema.pending.len()}))
    }
    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        self.note(format!("replace {} {}", source.display(), target.display()));
        Ok(())
    }
    fn restore(&self, target: &Path) -> Result<()> {
        self.note(format!("restore {}", target.display()));
        Ok(())
    }
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value> {
        self.note(format!("run {}", binary.display()));
        self.up.lock().unwrap().push(arguments.to_vec());
        Ok(json!({"supervisor": {"outcome": "started"}}))
    }
    fn e2e(
        &self,
        checkout: &Path,
        target: Option<&Path>,
        _: &dagq::application::install::E2eSettings,
    ) -> Result<E2eOutcome> {
        assert_eq!(target, None, "install's e2e uses the build's own target");
        self.note(format!("e2e {}", checkout.display()));
        Ok(self.e2e.lock().unwrap().clone().unwrap_or(e2e_passed(3)))
    }
}

fn install_with(
    fixture: &Fixture,
    binaries: &FakeBinaries,
    processes: &FakeProcesses,
    down: &dyn Fn() -> Result<Value>,
    options: &dagq::application::install::InstallOptions,
) -> Result<Value> {
    let queues = |db: &Path| -> std::sync::Arc<dyn dagq::application::QueueOpener> {
        std::sync::Arc::new(dagq::infrastructure::runtime_store::SqliteOpener {
            db: db.to_owned(),
            generators: dagq::infrastructure::clock::system(),
            actor: None,
        })
    };
    dagq::application::install::install(
        &dagq::application::install::Ports {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            down,
        },
        Some(&fixture.location.db),
        options,
    )
}

fn install_options(
    source: dagq::application::install::Source,
) -> dagq::application::install::InstallOptions {
    dagq::application::install::InstallOptions {
        source,
        target: "/opt/bin/dagq".into(),
        allow_breaking: false,
        restart: vec!["--cmux".into(), "/opt/cmux".into()],
        handoff_timeout: Duration::from_secs(5),
        poll: Duration::from_millis(20),
        e2e: E2eGate::Run(e2e_settings(Path::new("/queue"))),
    }
}

/// `install` builds the checkout, probes the build, applies compatible
/// migrations with it, puts it in place and hands the live supervisors that
/// take a handoff over to it; one of an older binary is left for `up` to
/// drain. A handoff that fails puts the replaced binary back.
#[test]
fn install_migrates_replaces_and_hands_over_and_restores_on_a_failed_handoff() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "new", SupervisorMode::InCmux);
    let mut old = SqliteQueue::open(&fixture.location.db).unwrap();
    old.register_supervisor(&LeaseToken::new("older"), 424_243, 1, "0.0.1")
        .unwrap();
    let binaries = FakeBinaries::new(&[(27, true)], false);
    let processes = FakeProcesses::default();
    let no_down = || -> Result<Value> { panic!("no drain for compatible migrations") };
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
            wait_until(&processes, std::process::id(), || {
                queue
                    .handoff_request(&LeaseToken::new("new"))
                    .unwrap()
                    .is_some()
            });
            queue
                .resume_registration(&LeaseToken::new("new"), std::process::id(), VERSION)
                .unwrap();
        });
        install_with(
            &fixture,
            &binaries,
            &processes,
            &no_down,
            &install_options(Source::Checkout("/src/dagq".into())),
        )
        .unwrap()
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["migrated"], json!({"applied": 1}));
    assert_eq!(report["supervisors"][0]["token"], "new");
    assert_eq!(report["not_handed_off"][0]["token"], "older");
    assert_eq!(report["previous"], "/opt/bin/dagq.previous");
    assert_eq!(report["e2e"]["status"], "passed", "{report}");
    assert_eq!(report["e2e"]["secs"], 3, "{report}");
    assert_eq!(
        binaries.calls(),
        [
            "build /src/dagq",
            "e2e /src/dagq",
            "probe /src/dagq/target/release/dagq",
            "migrate /src/dagq/target/release/dagq",
            "replace /src/dagq/target/release/dagq /opt/bin/dagq",
        ]
    );

    // Nobody takes this one: the handoff times out and the binary is put back.
    let binaries = FakeBinaries::new(&[], true);
    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.handoff_timeout = Duration::from_millis(200);
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &no_down, &options).unwrap_err()
    );
    assert!(error.contains("the handoff to"), "{error}");
    assert!(error.contains("is back at /opt/bin/dagq"), "{error}");
    assert_eq!(
        queue.handoff_request(&LeaseToken::new("new")).unwrap(),
        None
    );

    // A binary that predates the handoff would end the supervisor it is
    // exec'd in: nothing is replaced.
    let older = FakeBinaries::new(&[], true);
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &older,
            &processes,
            &no_down,
            &install_options(Source::Binary("/old/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("predates the handoff"), "{error}");
    assert_eq!(older.calls(), ["probe /old/dagq"]);
    assert_eq!(
        binaries.calls(),
        [
            "probe /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "restore /opt/bin/dagq",
        ]
    );
    drop(queue);
}

/// `install` of a checkout runs its e2e first (ADR-t963-1 decision 1): one
/// that fails or runs past its timeout replaces nothing and names its
/// failed tests and log; `--skip-e2e` installs without it, and a built
/// binary has none.
#[test]
fn install_of_a_checkout_replaces_nothing_unless_its_e2e_passes() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let processes = FakeProcesses::default();
    let no_down = || -> Result<Value> { panic!("no drain") };
    let checkout = || install_options(Source::Checkout("/src/dagq".into()));

    let failing = FakeBinaries::new(&[], true);
    *failing.e2e.lock().unwrap() = Some(E2eOutcome {
        failed_tests: vec!["e2e::lands".into()],
        secs: 40,
        ..Default::default()
    });
    let error = format!(
        "{:#}",
        install_with(&fixture, &failing, &processes, &no_down, &checkout()).unwrap_err()
    );
    assert!(
        error.contains("the e2e failed: e2e::lands; see /queue/e2e.log")
            && error.contains("nothing was replaced")
            && error.contains("--skip-e2e"),
        "{error}"
    );
    assert_eq!(failing.calls(), ["build /src/dagq", "e2e /src/dagq"]);

    let stopped = FakeBinaries::new(&[], true);
    *stopped.e2e.lock().unwrap() = Some(E2eOutcome {
        timed_out: true,
        secs: 1800,
        ..Default::default()
    });
    let error = format!(
        "{:#}",
        install_with(&fixture, &stopped, &processes, &no_down, &checkout()).unwrap_err()
    );
    assert!(error.contains("did not finish within 1800s"), "{error}");
    assert!(stopped.calls().iter().all(|c| !c.starts_with("replace")));

    let skipped = FakeBinaries::new(&[], true);
    let mut options = checkout();
    options.e2e = E2eGate::Skip;
    let report = install_with(&fixture, &skipped, &processes, &no_down, &options).unwrap();
    assert_eq!(report["e2e"]["status"], "skipped", "{report}");
    assert!(skipped.calls().iter().all(|c| !c.starts_with("e2e")));

    // Passed without the podman tests (ADR-t1162-1): installed, and the
    // report names them and why.
    let without_podman = FakeBinaries::new(&[], true);
    *without_podman.e2e.lock().unwrap() = Some(E2eOutcome {
        skipped: Some(dagq::application::install::E2eSkip {
            tests: vec!["broker::".into()],
            reason: "broker podman_missing".into(),
        }),
        ..e2e_passed(30)
    });
    let report =
        install_with(&fixture, &without_podman, &processes, &no_down, &checkout()).unwrap();
    assert_eq!(report["e2e"]["status"], "passed", "{report}");
    assert_eq!(
        report["e2e"]["skipped"],
        json!({"tests": ["broker::"], "reason": "broker podman_missing"}),
        "{report}"
    );

    // Rerun by name (ADR-t1165-1): a flaky and a quarantined test are
    // installed and named; one failing the rerun under no mark is not.
    let rerun = FakeBinaries::new(&[], true);
    *rerun.e2e.lock().unwrap() = Some(e2e_rerun(
        &["e2e::a", "e2e::b"],
        &["e2e::b"],
        &[("e2e::b", "2999-12-31")],
    ));
    let report = install_with(&fixture, &rerun, &processes, &no_down, &checkout()).unwrap();
    let e2e = &report["e2e"];
    assert_eq!(e2e["status"], "passed", "{report}");
    assert_eq!(e2e["flaky"], json!(["e2e::a"]), "{report}");
    assert_eq!(e2e["quarantined"], json!(["e2e::b"]), "{report}");
    assert_eq!(e2e["rerun"]["log"], "/queue/e2e.rerun.log", "{report}");
    let unmarked = FakeBinaries::new(&[], true);
    *unmarked.e2e.lock().unwrap() = Some(e2e_rerun(&["e2e::a"], &["e2e::a"], &[]));
    let error = format!(
        "{:#}",
        install_with(&fixture, &unmarked, &processes, &no_down, &checkout()).unwrap_err()
    );
    assert!(
        error.contains("the rerun by name failed too: e2e::a")
            && error.contains("see /queue/e2e.log and /queue/e2e.rerun.log")
            && error.contains("nothing was replaced"),
        "{error}"
    );
    assert!(unmarked.calls().iter().all(|c| !c.starts_with("replace")));

    let binary = FakeBinaries::new(&[], true);
    let report = install_with(
        &fixture,
        &binary,
        &processes,
        &no_down,
        &install_options(Source::Binary("/built/dagq".into())),
    )
    .unwrap();
    assert_eq!(report["e2e"]["status"], "not_applicable", "{report}");
    assert!(binary.calls().iter().all(|c| !c.starts_with("e2e")));
}

/// A build with a breaking migration is refused without `--allow-breaking`
/// and replaces nothing; with it, the supervisor is drained, the queue
/// migrated, the binary replaced and `up` run with the drained supervisor's
/// mode, parallelism, automatic update and wait limit. A rollback past a
/// breaking migration is refused, and so is one without a previous binary.
#[test]
fn install_drains_only_for_a_breaking_migration_when_allowed() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    let processes = FakeProcesses::default();
    let binaries = FakeBinaries::new(&[(27, true), (28, false)], false);
    let drained = Mutex::new(0);
    let down = || -> Result<Value> {
        *drained.lock().unwrap() += 1;
        Ok(json!({"outcome": "stopped"}))
    };
    let error = format!(
        "{:#}",
        install_with(
            &fixture,
            &binaries,
            &processes,
            &down,
            &install_options(Source::Binary("/built/dagq".into())),
        )
        .unwrap_err()
    );
    assert!(error.contains("breaking migration(s) 28"), "{error}");
    assert!(error.contains("--allow-breaking"), "{error}");
    assert_eq!(binaries.calls(), ["probe /built/dagq"]);
    assert_eq!(*drained.lock().unwrap(), 0);

    let mut options = install_options(Source::Binary("/built/dagq".into()));
    options.allow_breaking = true;
    let report = install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(*drained.lock().unwrap(), 1);
    assert_eq!(report["drained"]["outcome"], "stopped", "{report}");
    assert_eq!(report["up"]["supervisor"]["outcome"], "started");
    assert_eq!(
        binaries.calls()[2..],
        [
            "migrate /built/dagq",
            "replace /built/dagq /opt/bin/dagq",
            "run /opt/bin/dagq",
        ]
    );
    let db = fixture.location.db.to_str().unwrap();
    assert_eq!(
        binaries.up.lock().unwrap()[0],
        [
            "--db",
            db,
            "up",
            "--parallel",
            "4",
            "--in-cmux",
            "--cmux",
            "/opt/cmux"
        ]
    );

    // A drained supervisor with the automatic update, a wait limit and a
    // limit of the runtime's planners gets them back from the `up`, once
    // each even when the restart names them (task 941).
    queue
        .set_auto_update(&LeaseToken::new("live"), true)
        .unwrap();
    queue
        .set_slot_limits(
            &LeaseToken::new("live"),
            slot_limits(SettingSource::Flag, 2),
        )
        .unwrap();
    let binaries = FakeBinaries::new(&[(28, false)], false);
    install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    options.restart = vec![
        "--auto-update".into(),
        "--max-waiting".into(),
        "6".into(),
        "--runtime-planners".into(),
        "2".into(),
    ];
    install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
    assert_eq!(
        *binaries.up.lock().unwrap(),
        [
            vec![
                "--db",
                db,
                "up",
                "--parallel",
                "4",
                "--in-cmux",
                "--auto-update",
                "--max-waiting",
                "2",
                "--runtime-planners",
                "3",
                "--cmux",
                "/opt/cmux"
            ],
            vec![
                "--db",
                db,
                "up",
                "--parallel",
                "4",
                "--in-cmux",
                "--auto-update",
                "--max-waiting",
                "6",
                "--runtime-planners",
                "2"
            ]
        ]
    );

    // Values the drained supervisor took from `dagq.toml` (or the
    // default) are not baked into the `up`: the started one reads them
    // again (task 698).
    for source in [SettingSource::File, SettingSource::Default] {
        queue
            .set_slot_limits(&LeaseToken::new("live"), slot_limits(source, 2))
            .unwrap();
        let binaries = FakeBinaries::new(&[(28, false)], false);
        options.restart = vec![];
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
        assert_eq!(
            binaries.up.lock().unwrap()[0],
            ["--db", db, "up", "--in-cmux", "--auto-update"],
            "{source:?}"
        );
    }

    let binaries = FakeBinaries::new(&[], false);
    let dir = tempfile::tempdir().unwrap();
    let mut options = install_options(Source::Rollback);
    options.target = dir.path().join("dagq");
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("no previous binary"), "{error}");
    fs::write(dir.path().join("dagq.previous"), "").unwrap();
    let error = format!(
        "{:#}",
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap_err()
    );
    assert!(error.contains("the queue refuses 0.0.1"), "{error}");
    assert_eq!(
        dagq::application::install::parse_version("dagq 1.2.3\n").unwrap(),
        "1.2.3"
    );
    assert!(dagq::application::install::parse_version("").is_err());
}

/// `parallel` 4, `max_waiting` `max_waiting` and `runtime_planners` 3, all
/// from `source`.
fn slot_limits(source: SettingSource, max_waiting: usize) -> SlotLimits {
    SlotLimits {
        parallel: Setting { value: 4, source },
        max_waiting: Setting {
            value: max_waiting,
            source,
        },
        runtime_planners: Setting { value: 3, source },
    }
}

/// [`Binaries`] for the automatic update's job: the build leaves a real
/// file (or fails), everything under `old` (the fixed binary and its
/// `.previous`) is the old build `0.0.1`, anything else this build.
struct UpdateBinaries {
    old: PathBuf,
    built: PathBuf,
    build_fails: bool,
    replace_fails: std::sync::atomic::AtomicBool,
    pending: Vec<(i64, bool)>,
    calls: Mutex<Vec<String>>,
    /// How the build's e2e goes: passes unless set; an `Err` could not
    /// start.
    e2e: Mutex<Option<Result<E2eOutcome, String>>>,
    /// The version the client built beside dagq reports: dagq's unless set.
    client_version: Option<String>,
}

impl UpdateBinaries {
    fn new(dir: &Path, build_fails: bool, pending: &[(i64, bool)]) -> Self {
        let built = dir.join("built").join("dagq");
        fs::create_dir_all(built.parent().unwrap()).unwrap();
        fs::write(&built, "new build").unwrap();
        Self {
            old: dir.join("bin"),
            built,
            build_fails,
            replace_fails: Default::default(),
            pending: pending.to_vec(),
            calls: Mutex::default(),
            e2e: Mutex::default(),
            client_version: None,
        }
    }
    fn with_e2e(self, e2e: Result<E2eOutcome, String>) -> Self {
        *self.e2e.lock().unwrap() = Some(e2e);
        self
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl dagq::application::install::Binaries for UpdateBinaries {
    fn build(&self, _: &Path) -> Result<PathBuf> {
        bail!("the job builds into its own target")
    }
    fn version(&self, binary: &Path) -> Result<String> {
        Ok(if binary.starts_with(&self.old) {
            "0.0.1".into()
        } else if binary.ends_with("dagq-broker-client") {
            self.client_version.clone().unwrap_or(VERSION.into())
        } else {
            VERSION.into()
        })
    }
    fn probe(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn takes_handoff(&self, _: &Path) -> bool {
        true
    }
    fn schema(&self, _: &Path, _: &Path) -> Result<dagq::application::install::SchemaCheck> {
        Ok(dagq::application::install::SchemaCheck {
            pending: self
                .pending
                .iter()
                .map(
                    |&(version, compatible)| dagq::application::install::PendingMigration {
                        version,
                        compatible,
                    },
                )
                .collect(),
            opens: self.pending.is_empty(),
        })
    }
    fn migrate(&self, _: &Path, _: &Path) -> Result<Value> {
        self.note("migrate".into());
        Ok(json!({"applied": self.pending.len()}))
    }
    fn replace(&self, _: &Path, target: &Path) -> Result<()> {
        self.note(format!("replace {}", target.display()));
        if self.replace_fails.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("replace failed");
        }
        Ok(())
    }
    fn restore(&self, target: &Path) -> Result<()> {
        self.note(format!("restore {}", target.display()));
        Ok(())
    }
    fn run(&self, _: &Path, _: &[String]) -> Result<Value> {
        bail!("the job runs no binary")
    }
    fn checkout(&self, _: &Path, checkout: &Path, commit: &str) -> Result<()> {
        self.note(format!("checkout {} {commit}", checkout.display()));
        Ok(())
    }
    fn build_into(&self, _: &Path, target: &Path, _: Option<&str>, _: &Path) -> Result<PathBuf> {
        self.note(format!("build {}", target.display()));
        if self.build_fails {
            bail!("cargo build failed");
        }
        Ok(self.built.clone())
    }
    fn e2e(
        &self,
        checkout: &Path,
        target: Option<&Path>,
        _: &dagq::application::install::E2eSettings,
    ) -> Result<E2eOutcome> {
        self.note(format!(
            "e2e {} {}",
            checkout.display(),
            target.unwrap().display()
        ));
        match self.e2e.lock().unwrap().clone() {
            None => Ok(e2e_passed(170)),
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(error)) => bail!("{error}"),
        }
    }
}

/// The supervisor the job updates: registered under a pid of its own, which
/// the fake process control keeps alive until the test says otherwise.
const UPDATED_PID: u32 = 424_242;

fn run_update_job(
    fixture: &Fixture,
    binaries: &UpdateBinaries,
    processes: &FakeProcesses,
    restarted: &Mutex<Vec<String>>,
) -> Value {
    use dagq::application::update;
    let queues = |db: &Path| -> std::sync::Arc<dyn dagq::application::QueueOpener> {
        std::sync::Arc::new(dagq::infrastructure::runtime_store::SqliteOpener {
            db: db.to_owned(),
            generators: dagq::infrastructure::clock::system(),
            actor: None,
        })
    };
    let restart = |registration: &dagq::domain::SupervisorRegistration| -> Result<Value> {
        restarted
            .lock()
            .unwrap()
            .push(registration.token.to_string());
        Ok(json!({"by": "test"}))
    };
    let dir = fixture._dir.path();
    update::run(
        &update::JobPorts {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            restart: &restart,
        },
        &fixture.location.db,
        &update::JobOptions {
            commit: "c0ffee".repeat(6) + "c0ff",
            token: LeaseToken::new("auto"),
            target: dir.join("bin").join("dagq"),
            repository: fixture.repo.clone(),
            paths: update::UpdatePaths::under(&dir.join("queue-dir")),
            log: dir.join("build.log"),
            build_command: None,
            e2e: e2e_settings(&dir.join("queue-dir")),
            restart: vec!["--cmux".into(), "/opt/cmux".into()],
            handoff_timeout: Duration::from_secs(5),
            watch_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(20),
            pid: 7,
        },
    )
    .unwrap()
}

fn auto_supervisor(fixture: &Fixture) -> SqliteQueue {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    queue
        .register_supervisor(&LeaseToken::new("auto"), UPDATED_PID, 2, "0.0.1")
        .unwrap();
    queue.accept_handoff(&LeaseToken::new("auto")).unwrap();
    queue
        .set_auto_update(&LeaseToken::new("auto"), true)
        .unwrap();
    queue
        .set_supervisor_mode(
            &LeaseToken::new("auto"),
            SupervisorMode::InCmux,
            Some("ws-auto"),
        )
        .unwrap();
    queue
}

/// Wait until the handoff of `pid` is done and the job's watch looked at
/// it, from the looks at whether it lives from now on. Called once the
/// supervisor is in its final state (taken, registered again): at most one
/// look from now on read the queue before that state, each of the
/// others read it, the handoff's first of them takes the supervisor and
/// ends the handoff's looks at it, and the watch's first read the
/// heartbeat it waits to see pass. Three looks cover the handoff's two and
/// the watch's first (task 1048). Stands in for a sleep past the next unix
/// second.
fn until_watched(processes: &FakeProcesses, pid: u32) {
    let before = processes.looks_at(pid);
    wait_until(processes, pid, || processes.looks_at(pid) >= before + 3);
}

/// The supervisor's next heartbeat, written a second later than now: the
/// watch compares unix seconds, and this stands in for the beat a live
/// supervisor writes in the next second (task 1048).
fn heartbeat_later(fixture: &Fixture, token: &str) {
    struct Later;
    impl dagq::application::Clock for Later {
        fn system_time(&self) -> std::time::SystemTime {
            std::time::SystemTime::now() + Duration::from_secs(1)
        }
    }
    use dagq::application::QueueOpener;
    let mut generators = dagq::infrastructure::clock::system();
    generators.clock = std::sync::Arc::new(Later);
    dagq::infrastructure::runtime_store::SqliteOpener {
        db: fixture.location.db.clone(),
        generators,
        actor: None,
    }
    .open()
    .unwrap()
    .heartbeat(&LeaseToken::new(token))
    .unwrap();
}

/// Take the handoff the way the exec'd binary does, and heartbeat on
/// (`alive`) or die once the job's watch looked at it.
fn take_and_heartbeat(fixture: &Fixture, processes: &FakeProcesses, alive: bool) {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    wait_until(processes, UPDATED_PID, || {
        queue
            .handoff_request(&LeaseToken::new("auto"))
            .unwrap()
            .is_some()
    });
    queue
        .resume_registration(&LeaseToken::new("auto"), UPDATED_PID, VERSION)
        .unwrap();
    until_watched(processes, UPDATED_PID);
    if alive {
        heartbeat_later(fixture, "auto");
    } else {
        processes.dead.lock().unwrap().insert(UPDATED_PID);
    }
}

/// The automatic update's job (ADR-0045 decisions 13, 17): a build that
/// the supervisor takes and heartbeats on is installed; one it dies on is
/// put back and the supervisor started again; a failed build replaces
/// nothing; each failure opens the `update_failed` ask, a newer one
/// superseding the older; a breaking migration waits in `approve_update`.
#[test]
fn the_update_job_installs_watches_restores_and_asks() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();

    let binaries = UpdateBinaries::new(dir, false, &[(40, true)]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["version"], VERSION);
    assert_eq!(
        binaries.calls(),
        [
            format!(
                "checkout {} {}",
                dir.join("queue-dir/update/checkout").display(),
                "c0ffee".repeat(6) + "c0ff"
            ),
            format!("build {}", dir.join("queue-dir/update/target").display()),
            format!(
                "e2e {} {}",
                dir.join("queue-dir/update/checkout").display(),
                dir.join("queue-dir/update/target").display()
            ),
            "migrate".to_owned(),
            format!("replace {}", target.display()),
        ]
    );
    let kinds = |queue: &SqliteQueue| {
        queue
            .update_events(10)
            .unwrap()
            .into_iter()
            .map(|u| u.kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(&queue),
        ["update_installed", "update_e2e_passed", "update_built"]
    );

    // The new binary dies after it took the handoff: back to the old one,
    // and the gone in-cmux supervisor is started again.
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, false));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(
        report["supervisors"][0]["supervisor"]["state"], "restarted",
        "{report}"
    );
    assert_eq!(*restarted.lock().unwrap(), ["auto"]);
    assert!(
        binaries
            .calls()
            .contains(&format!("restore {}", target.display()))
    );
    let first_ask = report["ask_id"].as_i64().unwrap();
    // The rollback is a step of its own, before the failure it explains.
    assert_eq!(
        kinds(&queue)[..4],
        [
            "update_failed",
            "update_restored",
            "update_e2e_passed",
            "update_built"
        ]
    );

    // A failed build replaces nothing, and its ask replaces the older one.
    processes.dead.lock().unwrap().clear();
    let binaries = UpdateBinaries::new(dir, true, &[]);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["stage"], "build", "{report}");
    assert!(binaries.calls().iter().all(|c| !c.starts_with("replace")));
    let asks = queue
        .asks(dagq::application::AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    let older = asks.iter().find(|a| a.id.as_i64() == first_ask).unwrap();
    assert_eq!(older.answer.as_deref(), Some("superseded"));
    assert_eq!(older.answered_by.as_deref(), Some("runtime"));
    assert!(older.closed_at.is_some());
    let open: Vec<_> = asks.iter().filter(|a| a.is_open()).collect();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].kind, dagq::domain::AskKind::UpdateFailed);
    assert_eq!(open[0].subject, None);
    assert_eq!(open[0].options, ["retry", "skip"]);
    assert!(
        open[0].question.contains("failed at its build"),
        "{}",
        open[0].question
    );

    // A breaking migration: kept for a person, nothing replaced.
    let binaries = UpdateBinaries::new(dir, false, &[(40, true), (41, false)]);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "awaiting_approval", "{report}");
    assert_eq!(report["migrations"], json!([41]));
    let staged = dir.join("queue-dir/update/staged/dagq");
    assert_eq!(fs::read_to_string(&staged).unwrap(), "new build");
    assert!(
        report["command"]
            .as_str()
            .unwrap()
            .contains("--allow-breaking --cmux /opt/cmux")
    );
    assert!(binaries.calls().iter().all(|c| !c.starts_with("replace")));
    let asks = queue.asks(dagq::application::AskQuery::default()).unwrap();
    let approve = asks
        .iter()
        .find(|a| a.kind == dagq::domain::AskKind::ApproveUpdate)
        .unwrap();
    assert_eq!(approve.options, ["install", "skip"]);
    assert_eq!(kinds(&queue)[0], "update_awaiting_approval");

    // `status` reads where the update stands.
    let status = dagq::compose::status(&fixture.location.db).unwrap();
    assert_eq!(
        status["auto_update"]["state"], "awaiting_approval",
        "{status}"
    );
}

/// The job puts the worker's client built beside dagq in place with it
/// (ADR-t827-1 decision 5): a client of another build replaces nothing; one
/// of dagq's build goes in place first; a watch that fails puts both back;
/// a breaking build is staged with its client.
#[test]
fn the_update_job_puts_the_client_built_beside_dagq_in_place_with_it() {
    let fixture = fixture();
    let _queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    let client = dir.join("bin").join("dagq-broker-client");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    fs::write(&client, "old client").unwrap();
    let replaces = |binaries: &UpdateBinaries| {
        binaries
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("replace"))
            .collect::<Vec<_>>()
    };

    // A client of another build than the dagq beside it: nothing is
    // replaced.
    let mut binaries = UpdateBinaries::new(dir, false, &[]);
    fs::write(dir.join("built/dagq-broker-client"), "new client").unwrap();
    binaries.client_version = Some("0.0.9".into());
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "install", "{report}");
    assert!(
        report["error"].as_str().unwrap().contains("0.0.9"),
        "{report}"
    );
    assert!(replaces(&binaries).is_empty());

    // The client of dagq's build goes in place first, then dagq.
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(
        replaces(&binaries),
        [
            format!("replace {}", client.display()),
            format!("replace {}", target.display())
        ]
    );

    // The new binary dies after it took the handoff: both go back.
    fs::write(dir.join("bin/dagq.previous"), "old build").unwrap();
    fs::write(dir.join("bin/dagq-broker-client.previous"), "old client").unwrap();
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, false));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(report["restored"]["client"]["restored"], true, "{report}");
    let calls = binaries.calls();
    for restored in [&target, &client] {
        assert!(
            calls.contains(&format!("restore {}", restored.display())),
            "{calls:?}"
        );
    }

    // A breaking build waits for a person with its client beside it, for
    // the `install --from` of the staged dagq.
    processes.dead.lock().unwrap().clear();
    let binaries = UpdateBinaries::new(dir, false, &[(41, false)]);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "awaiting_approval", "{report}");
    let staged = dir.join("queue-dir/update/staged");
    assert_eq!(
        fs::read_to_string(staged.join("dagq-broker-client")).unwrap(),
        "new client"
    );
    // A later breaking build without a client leaves none staged.
    fs::remove_file(dir.join("built/dagq-broker-client")).unwrap();
    let binaries = UpdateBinaries::new(dir, false, &[(41, false)]);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "awaiting_approval", "{report}");
    assert!(!staged.join("dagq-broker-client").exists());
}

/// The e2e gate of the job (ADR-t963-1 decision 1): a build whose e2e
/// fails, runs past its timeout or cannot start replaces nothing (a
/// breaking one is not staged either) and opens the `update_failed` ask at
/// the `e2e` stage with its failed tests and log; one whose e2e passes is
/// installed after `update_e2e_passed`; `stats` counts both.
#[test]
fn the_update_job_replaces_nothing_unless_the_build_passes_its_e2e() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let e2e_log = dir.join("queue-dir").join("e2e.log");
    let open_ask = |queue: &SqliteQueue| {
        let asks = queue.asks(dagq::application::AskQuery::default()).unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        asks.into_iter().next().unwrap()
    };
    let replaced_nothing = |binaries: &UpdateBinaries| {
        assert!(
            binaries
                .calls()
                .iter()
                .all(|c| !c.starts_with("replace") && c != "migrate"),
            "{:?}",
            binaries.calls()
        );
    };

    // Failed tests.
    let binaries = UpdateBinaries::new(dir, false, &[(40, true)]).with_e2e(Ok(E2eOutcome {
        failed_tests: vec!["e2e::lands".into(), "e2e::hands_over".into()],
        secs: 200,
        cleanup: json!({"removed": true, "groups": ["G-1"]}),
        ..Default::default()
    }));
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "e2e", "{report}");
    assert_eq!(
        report["failed_tests"],
        json!(["e2e::lands", "e2e::hands_over"])
    );
    assert_eq!(report["timed_out"], false);
    assert_eq!(report["e2e_log"], json!(e2e_log));
    assert_eq!(report["cleanup"]["groups"], json!(["G-1"]));
    replaced_nothing(&binaries);
    let ask = open_ask(&queue);
    assert_eq!(ask.kind, dagq::domain::AskKind::UpdateFailed);
    assert_eq!(ask.options, ["retry", "skip"]);
    assert!(
        ask.question.contains("failed at its e2e")
            && ask.question.contains("e2e::lands, e2e::hands_over")
            && ask.question.contains(&e2e_log.display().to_string())
            && ask.question.contains("nothing was replaced"),
        "{}",
        ask.question
    );
    let status = dagq::compose::status(&fixture.location.db).unwrap();
    assert_eq!(status["auto_update"]["state"], "failed", "{status}");
    assert_eq!(status["auto_update"]["last"]["stage"], "e2e", "{status}");

    // Past its timeout.
    let binaries = UpdateBinaries::new(dir, false, &[]).with_e2e(Ok(E2eOutcome {
        timed_out: true,
        secs: 1800,
        ..Default::default()
    }));
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["stage"], "e2e", "{report}");
    assert_eq!(report["timed_out"], true, "{report}");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("did not finish within 1800s"),
        "{report}"
    );
    replaced_nothing(&binaries);
    assert!(open_ask(&queue).question.contains("did not finish"));

    // It could not start (no cmux).
    let binaries =
        UpdateBinaries::new(dir, false, &[]).with_e2e(Err("the e2e needs a running cmux".into()));
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["stage"], "e2e", "{report}");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("the e2e could not start: the e2e needs a running cmux"),
        "{report}"
    );
    replaced_nothing(&binaries);

    // A breaking build that fails its e2e waits for no person.
    let binaries = UpdateBinaries::new(dir, false, &[(41, false)]).with_e2e(Ok(E2eOutcome {
        failed_tests: vec!["e2e::lands".into()],
        ..Default::default()
    }));
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["stage"], "e2e", "{report}");
    assert!(!dir.join("queue-dir/update/staged/dagq").exists());
    assert_eq!(open_ask(&queue).kind, dagq::domain::AskKind::UpdateFailed);

    // It passes: installed as before.
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    let passed = queue
        .update_events(10)
        .unwrap()
        .into_iter()
        .find(|u| u.kind == "update_e2e_passed")
        .unwrap();
    assert_eq!(passed.payload["secs"], 170);
    assert_eq!(passed.payload["log"], json!(e2e_log));

    let stats = common::cli::ok(&fixture.location.db, &["stats", "--full"]);
    let e2e = &stats["updates"]["e2e"];
    assert_eq!(
        (e2e["passed"].as_i64(), e2e["failed"].as_i64()),
        (Some(1), Some(4)),
        "{stats}"
    );
    assert_eq!(e2e["timed_out"], 1, "{stats}");
    assert_eq!(stats["updates"]["failed_by_stage"]["e2e"], 4, "{stats}");
    assert!(restarted.lock().unwrap().is_empty());
}

/// The gate reruns the failed e2e by name once (ADR-t1165-1): a build whose
/// failed tests passed the rerun (flaky), or failed it under a mark that
/// holds (quarantined), is installed with them named on
/// `update_e2e_passed`; a test failing the rerun under no mark, under an
/// expired mark, under a file of more than 3 marks, or under a mark whose
/// test failed the rerun of 3 gates in a row, fails the gate with why in
/// the error and the ask.
#[test]
fn the_update_job_passes_flaky_and_quarantined_e2e_and_fails_the_rest() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let rerun_log = dir.join("queue-dir").join("e2e.rerun.log");
    let held = [("e2e::b", "2999-12-31")];

    // a passes its rerun, b fails it under a mark: installed.
    let binaries = UpdateBinaries::new(dir, false, &[]).with_e2e(Ok(e2e_rerun(
        &["e2e::a", "e2e::b"],
        &["e2e::b"],
        &held,
    )));
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    let passed = queue
        .update_events(10)
        .unwrap()
        .into_iter()
        .find(|u| u.kind == "update_e2e_passed")
        .unwrap()
        .payload;
    assert_eq!(passed["flaky"], json!(["e2e::a"]), "{passed}");
    assert_eq!(passed["quarantined"], json!(["e2e::b"]), "{passed}");
    assert_eq!(passed["rerun"]["tests"], json!(["e2e::a", "e2e::b"]));
    assert_eq!(passed["rerun"]["failed"], json!(["e2e::b"]));
    assert_eq!(passed["rerun"]["log"], json!(rerun_log));
    assert_eq!(passed["quarantine"]["marks"][0]["task"], 1120, "{passed}");
    assert_eq!(passed["quarantine"]["ignored"], json!([]));

    let failing = |outcome: E2eOutcome, expected: &[&str]| {
        let binaries = UpdateBinaries::new(dir, false, &[]).with_e2e(Ok(outcome));
        let report = run_update_job(&fixture, &binaries, &processes, &restarted);
        assert_eq!(report["stage"], "e2e", "{report}");
        assert!(
            binaries.calls().iter().all(|c| !c.starts_with("replace")),
            "{:?}",
            binaries.calls()
        );
        let asks = queue.asks(dagq::application::AskQuery::default()).unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(asks[0].kind, dagq::domain::AskKind::UpdateFailed);
        let error = report["error"].as_str().unwrap();
        for part in expected {
            assert!(error.contains(part), "{part}: {error}");
            assert!(
                asks[0].question.contains(part),
                "{part}: {}",
                asks[0].question
            );
        }
        assert!(error.contains(&rerun_log.display().to_string()), "{error}");
        report
    };

    // A test failing its rerun under no mark.
    let report = failing(
        e2e_rerun(&["e2e::a"], &["e2e::a"], &held),
        &[
            "the rerun by name failed too: e2e::a",
            "no mark of .config/e2e-quarantine.toml holds for e2e::a",
        ],
    );
    assert_eq!(report["rerun"]["failed"], json!(["e2e::a"]), "{report}");
    assert_eq!(report["quarantined"], json!([]));
    assert_eq!(report["failed_tests"], json!(["e2e::a"]));

    // Under an expired mark.
    let report = failing(
        e2e_rerun(&["e2e::b"], &["e2e::b"], &[("e2e::b", "2020-01-01")]),
        &["e2e::b: its mark expired after 2020-01-01"],
    );
    assert_eq!(
        report["quarantine"]["ignored"][0],
        json!({"name": "e2e::b", "reason": "expired", "detail": "its mark expired after 2020-01-01"}),
        "{report}"
    );

    // Under a file of more than 3 marks.
    let report = failing(
        e2e_rerun(
            &["e2e::b"],
            &["e2e::b"],
            &[
                ("e2e::b", "2999-12-31"),
                ("e2e::c", "2999-12-31"),
                ("e2e::d", "2999-12-31"),
                ("e2e::e", "2999-12-31"),
            ],
        ),
        &[".config/e2e-quarantine.toml has 4 marks, more than 3"],
    );
    assert_eq!(report["quarantine"]["ignored"][3]["reason"], "over_limit");

    // b failed the rerun of the two gates before: the third fails.
    let report = failing(
        e2e_rerun(&["e2e::b"], &["e2e::b"], &held),
        &["e2e::b: it failed the rerun of 3 gates in a row"],
    );
    assert_eq!(
        report["quarantine"]["ignored"][0]["reason"],
        "failed_in_a_row"
    );

    let stats = common::cli::ok(&fixture.location.db, &["stats", "--full"]);
    let e2e = &stats["updates"]["e2e"];
    assert_eq!(
        (e2e["passed"].as_i64(), e2e["failed"].as_i64()),
        (Some(1), Some(4)),
        "{stats}"
    );
}

/// A build whose e2e passed with the podman tests not run (podman could
/// not be reached, ADR-t1162-1) is installed, and the tests and the reason
/// are named on `update_e2e_passed` and `update_installed` (its `message`
/// too, which the inbox passes on): the swap does not pass them silently.
#[test]
fn the_update_job_names_the_e2e_it_did_not_run_for_want_of_podman() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let skipped = dagq::application::install::E2eSkip {
        tests: vec!["broker::".into()],
        reason: "broker machine_failed: ssh: handshake failed".into(),
    };
    let binaries = UpdateBinaries::new(dir, false, &[]).with_e2e(Ok(E2eOutcome {
        skipped: Some(skipped),
        ..e2e_passed(150)
    }));
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    let expected = json!({
        "tests": ["broker::"],
        "reason": "broker machine_failed: ssh: handshake failed",
    });
    assert_eq!(report["e2e_skipped"], expected, "{report}");
    let events = queue.update_events(10).unwrap();
    let event = |kind: &str| events.iter().find(|u| u.kind == kind).unwrap().clone();
    assert_eq!(event("update_e2e_passed").payload["skipped"], expected);
    let installed = event("update_installed");
    assert_eq!(installed.payload["e2e_skipped"], expected);
    let message = installed.payload["message"].as_str().unwrap();
    assert!(
        message.contains("the e2e did not run broker:: because podman could not be reached")
            && message.contains("handshake failed"),
        "{message}"
    );
}

/// The second supervisor of the queue in the watch tests.
const OTHER_PID: u32 = 434_343;

/// What a handed-over supervisor does after it took the handoff.
#[derive(Clone, Copy)]
enum Afterwards {
    Heartbeat,
    Die,
    /// Deregister, the same pid registering again under the new build and
    /// a token of its own, and heartbeat on under that.
    Reregister,
    /// Register again the same way, then die.
    ReregisterAndDie,
    /// Come back under the old build (the exec failed) and heartbeat on.
    ExecFails,
    /// Register again like `Reregister`, next to a stale row of the same pid
    /// and build started before the handoff was asked for.
    ReregisterOverStale,
    /// Deregister instead of taking the handoff, leaving only a stale row
    /// of the same pid and build.
    DeregisterOverStale,
    /// Take the handoff under its own token, then deregister without
    /// heartbeating, leaving only a stale row of the same pid and build.
    TakeThenDeregisterOverStale,
    /// Stop heartbeating while asked, the process living on (hung in the
    /// exec of the new binary); it dies once terminated.
    Hang,
    /// Die while asked, before taking the handoff.
    DieBeforeTaking,
}

/// A row of `pid` under the new build that an earlier process of a reused
/// pid left behind: started and last heartbeating an hour ago.
fn stale_row(fixture: &Fixture, token: &str, pid: u32) {
    rusqlite::Connection::open(&fixture.location.db)
        .unwrap()
        .execute(
            "INSERT INTO supervisors(token,pid,parallel,binary_version,started_at,heartbeat_at)
             VALUES (?1,?2,2,?3,unixepoch()-3600,unixepoch()-3600)",
            rusqlite::params![token, pid, VERSION],
        )
        .unwrap();
}

fn take_as(fixture: &Fixture, processes: &FakeProcesses, token: &str, pid: u32, then: Afterwards) {
    if matches!(
        then,
        Afterwards::ReregisterOverStale
            | Afterwards::DeregisterOverStale
            | Afterwards::TakeThenDeregisterOverStale
    ) {
        stale_row(fixture, &format!("{token}-stale"), pid);
    }
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    wait_until(processes, pid, || {
        queue
            .handoff_request(&LeaseToken::new(token))
            .unwrap()
            .is_some()
    });
    match then {
        Afterwards::DeregisterOverStale => {
            queue
                .deregister_supervisor(&LeaseToken::new(token))
                .unwrap();
            return;
        }
        Afterwards::Hang => {
            rusqlite::Connection::open(&fixture.location.db)
                .unwrap()
                .execute(
                    "UPDATE supervisors SET heartbeat_at = unixepoch() - 3600 WHERE token = ?1",
                    [token],
                )
                .unwrap();
            return;
        }
        Afterwards::DieBeforeTaking => {
            processes.dead.lock().unwrap().insert(pid);
            return;
        }
        _ => {}
    }
    let version = match then {
        Afterwards::ExecFails => "0.0.1",
        _ => VERSION,
    };
    queue
        .resume_registration(&LeaseToken::new(token), pid, version)
        .unwrap();
    let serving = match then {
        Afterwards::Reregister | Afterwards::ReregisterAndDie | Afterwards::ReregisterOverStale => {
            let again = format!("{token}-again");
            queue
                .register_supervisor(&LeaseToken::new(&again), pid, 2, VERSION)
                .unwrap();
            queue
                .deregister_supervisor(&LeaseToken::new(token))
                .unwrap();
            again
        }
        _ => token.to_owned(),
    };
    // Back under the old build, it fails the handoff without the job
    // asking whether it lives: nothing to wait for.
    if !matches!(then, Afterwards::ExecFails) {
        until_watched(processes, pid);
    }
    match then {
        Afterwards::Die | Afterwards::ReregisterAndDie => {
            processes.dead.lock().unwrap().insert(pid);
        }
        Afterwards::TakeThenDeregisterOverStale => {
            queue
                .deregister_supervisor(&LeaseToken::new(token))
                .unwrap();
        }
        _ => {
            heartbeat_later(fixture, &serving);
        }
    }
}

/// Run the job with two supervisors handed over, each doing `auto` and
/// `other` afterwards: the report, the binaries' calls and the tokens
/// started again.
fn update_two(auto: Afterwards, other: Afterwards) -> (Value, Vec<String>, Vec<String>, PathBuf) {
    let fixture = fixture();
    let mut queue = auto_supervisor(&fixture);
    queue
        .register_supervisor(&LeaseToken::new("other"), OTHER_PID, 2, "0.0.1")
        .unwrap();
    queue.accept_handoff(&LeaseToken::new("other")).unwrap();
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let done = std::sync::atomic::AtomicBool::new(false);
    /// Ends the helper below however the job ends, a panic included.
    struct Done<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let report = thread::scope(|scope| {
        scope.spawn(|| take_as(&fixture, &processes, "auto", UPDATED_PID, auto));
        scope.spawn(|| take_as(&fixture, &processes, "other", OTHER_PID, other));
        // A supervisor the job terminates dies at once.
        scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let terminated = processes.terminated.lock().unwrap().clone();
                processes.dead.lock().unwrap().extend(terminated);
                thread::sleep(Duration::from_millis(10));
            }
        });
        let _done = Done(&done);
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    let restarted = restarted.lock().unwrap().clone();
    (report, binaries.calls(), restarted, target)
}

/// The watch follows every supervisor the install handed over (task 497):
/// all of them heartbeating on is an install; one failing keeps the new
/// binary for the others and brings back only the one that failed, with
/// the `update_failed` ask; the binary goes back only when all failed.
#[test]
fn the_update_job_watches_every_supervisor_it_handed_over() {
    let (report, calls, restarted, _) = update_two(Afterwards::Heartbeat, Afterwards::Heartbeat);
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(
        report["supervisors"].as_array().unwrap().len(),
        2,
        "{report}"
    );
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert!(restarted.is_empty());

    let (report, calls, restarted, _) = update_two(Afterwards::Heartbeat, Afterwards::Die);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["kept"], true, "{report}");
    assert_eq!(report["restored"]["restored"], false, "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert_eq!(restarted, ["other"]);
    let supervisors = report["supervisors"].as_array().unwrap();
    let auto = supervisors.iter().find(|s| s["token"] == "auto").unwrap();
    assert_eq!(auto["error"], Value::Null, "{report}");
    let other = supervisors.iter().find(|s| s["token"] == "other").unwrap();
    assert!(
        other["error"].as_str().unwrap().contains("exited"),
        "{report}"
    );
    assert_eq!(other["supervisor"]["state"], "restarted", "{report}");
    assert!(
        !report["error"]
            .as_str()
            .unwrap()
            .contains("supervisor auto"),
        "{report}"
    );

    let (report, calls, mut restarted, target_all) = update_two(Afterwards::Die, Afterwards::Die);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert!(
        calls.contains(&format!("restore {}", target_all.display())),
        "{calls:?}"
    );
    restarted.sort();
    assert_eq!(restarted, ["auto", "other"]);
}

/// A supervisor whose token deregistered after the handoff but whose pid
/// registered again under the new build is handed over (task 497).
#[test]
fn the_update_job_follows_a_pid_that_registered_again_under_the_new_build() {
    let (report, calls, restarted, _) = update_two(Afterwards::Heartbeat, Afterwards::Reregister);
    assert_eq!(report["outcome"], "installed", "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert!(restarted.is_empty());
}

/// Of two rows of the pid under the new build, the one it made after the
/// handoff was asked for is its successor, not a stale one of a reused pid
/// started before (task 633): both the install's wait and the watch follow
/// it.
#[test]
fn the_update_job_follows_the_row_registered_after_the_handoff_over_a_stale_one() {
    let (report, calls, restarted, _) =
        update_two(Afterwards::Heartbeat, Afterwards::ReregisterOverStale);
    assert_eq!(report["outcome"], "installed", "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert!(restarted.is_empty());
    let other = report["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["pid"] == OTHER_PID)
        .unwrap();
    assert_eq!(other["token"], "other-again", "{report}");
}

/// A stale row of the pid under the new build, started before the handoff
/// was asked for, is not a successor (task 633): a supervisor that
/// deregistered leaving only that is deregistered, both in the install's
/// wait and in the watch.
#[test]
fn a_stale_row_of_the_same_pid_is_not_a_successor() {
    let (report, _, _, _) = update_two(Afterwards::Heartbeat, Afterwards::DeregisterOverStale);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "handoff", "{report}");
    let other = report["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["token"] == "other")
        .unwrap();
    assert!(
        other["error"]
            .as_str()
            .unwrap()
            .contains("deregistered instead of taking the handoff"),
        "{report}"
    );

    let (report, _, _, _) = update_two(
        Afterwards::Heartbeat,
        Afterwards::TakeThenDeregisterOverStale,
    );
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "watch", "{report}");
    let other = report["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["token"] == "other")
        .unwrap();
    assert!(
        other["error"]
            .as_str()
            .unwrap()
            .contains("deregistered after it took the handoff"),
        "{report}"
    );
}

/// A supervisor that registered again under the new build and then died is
/// brought back under the registration the watch followed (task 497).
#[test]
fn the_update_job_brings_back_a_pid_that_registered_again_and_died() {
    let (report, calls, restarted, _) =
        update_two(Afterwards::Heartbeat, Afterwards::ReregisterAndDie);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["kept"], true, "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert_eq!(restarted, ["other-again"]);
    let other = report["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["pid"] == OTHER_PID)
        .unwrap();
    assert_eq!(other["now"], "other-again", "{report}");
    assert_eq!(other["supervisor"]["state"], "restarted", "{report}");
}

/// A handoff only some supervisors take keeps the new binary for them
/// (ADR-t632-1): the job watches the ones that took it and opens the
/// `update_failed` ask with `kept: true`, leaving the one still running
/// its old build as it is; when none takes it, the binary goes back.
#[test]
fn the_update_job_keeps_the_binary_when_only_some_take_the_handoff() {
    let (report, calls, restarted, _) = update_two(Afterwards::Heartbeat, Afterwards::ExecFails);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "handoff", "{report}");
    assert_eq!(report["kept"], true, "{report}");
    assert_eq!(report["restored"]["restored"], false, "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert!(restarted.is_empty(), "{restarted:?}");
    let supervisors = report["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 2, "{report}");
    let auto = supervisors.iter().find(|s| s["token"] == "auto").unwrap();
    assert_eq!(auto["error"], Value::Null, "{report}");
    let other = supervisors.iter().find(|s| s["token"] == "other").unwrap();
    assert!(
        other["error"]
            .as_str()
            .unwrap()
            .contains("came back as 0.0.1"),
        "{report}"
    );
    assert_eq!(other["supervisor"]["state"], "running", "{report}");
    assert!(report["ask_id"].as_i64().is_some(), "{report}");

    let (report, calls, restarted, target) =
        update_two(Afterwards::ExecFails, Afterwards::ExecFails);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "install", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert!(
        calls.contains(&format!("restore {}", target.display())),
        "{calls:?}"
    );
    assert!(restarted.is_empty(), "{restarted:?}");
    let supervisors = report["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 2, "{report}");
    for supervisor in supervisors {
        assert_eq!(supervisor["supervisor"]["state"], "running", "{report}");
    }
}

/// A handoff every supervisor failed puts the binary back in `install`,
/// and the job brings back each of them (task 716), not only its own: one
/// hung with the new binary is stopped and started again, one gone is
/// started again, and each one's outcome is in the `update_failed` ask.
#[test]
fn the_update_job_brings_back_every_supervisor_when_none_takes_the_handoff() {
    let (report, calls, mut restarted, target) =
        update_two(Afterwards::Hang, Afterwards::DieBeforeTaking);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "install", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(report["restored"]["version"], "0.0.1", "{report}");
    assert!(
        calls.contains(&format!("restore {}", target.display())),
        "{calls:?}"
    );
    restarted.sort();
    assert_eq!(restarted, ["auto", "other"]);
    let supervisors = report["supervisors"].as_array().unwrap();
    assert_eq!(supervisors.len(), 2, "{report}");
    let auto = supervisors.iter().find(|s| s["token"] == "auto").unwrap();
    assert_eq!(auto["pid"], UPDATED_PID, "{report}");
    assert!(
        auto["error"]
            .as_str()
            .unwrap()
            .contains("stopped heartbeating"),
        "{report}"
    );
    assert_eq!(auto["supervisor"]["state"], "restarted", "{report}");
    assert_eq!(auto["supervisor"]["stopped"], true, "{report}");
    let other = supervisors.iter().find(|s| s["token"] == "other").unwrap();
    assert_eq!(other["pid"], OTHER_PID, "{report}");
    assert!(other["error"].as_str().is_some(), "{report}");
    assert_eq!(other["supervisor"]["state"], "restarted", "{report}");
    assert_eq!(other["supervisor"]["stopped"], false, "{report}");
    assert!(report["ask_id"].as_i64().is_some(), "{report}");
}

/// An install that fails before any handoff (here its replace) brings back
/// only the job's own supervisor, as before task 716.
#[test]
fn the_update_job_brings_back_only_its_supervisor_when_the_install_fails_before_the_handoff() {
    let fixture = fixture();
    let mut queue = auto_supervisor(&fixture);
    queue
        .register_supervisor(&LeaseToken::new("other"), OTHER_PID, 2, "0.0.1")
        .unwrap();
    queue.accept_handoff(&LeaseToken::new("other")).unwrap();
    let processes = FakeProcesses::default();
    processes
        .dead
        .lock()
        .unwrap()
        .extend([UPDATED_PID, OTHER_PID]);
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let binaries = UpdateBinaries::new(dir, false, &[]);
    binaries
        .replace_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let report = run_update_job(&fixture, &binaries, &processes, &restarted);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "install", "{report}");
    assert!(
        report["error"].as_str().unwrap().contains("replace failed"),
        "{report}"
    );
    assert_eq!(report["supervisors"], Value::Null, "{report}");
    assert_eq!(report["supervisor"]["state"], "restarted", "{report}");
    assert_eq!(*restarted.lock().unwrap(), ["auto"]);
    assert!(
        binaries.calls().iter().all(|c| !c.starts_with("restore")),
        "{:?}",
        binaries.calls()
    );
}

/// `install` with two supervisors (ADR-t632-1): both taking the handoff is
/// an install; one failing keeps the new binary for the other, which takes
/// it after the failure, and fails with `kept: true` and each supervisor's
/// outcome, withdrawing only the failed one's request; both failing puts
/// the replaced binary back.
#[test]
fn install_keeps_the_binary_unless_every_supervisor_failed_the_handoff() {
    use dagq::application::install::{KeptBinary, Source};
    const SECOND: u32 = 464_646;
    let two = |first_version: &'static str, second_version: &'static str| {
        let fixture = fixture();
        let mut queue = handoff_supervisor(&fixture, "first", SupervisorMode::InCmux);
        queue
            .register_supervisor(&LeaseToken::new("second"), SECOND, 4, "0.0.1")
            .unwrap();
        queue.accept_handoff(&LeaseToken::new("second")).unwrap();
        let binaries = FakeBinaries::new(&[], true);
        let processes = FakeProcesses::default();
        let no_down = || -> Result<Value> { panic!("no drain") };
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
                wait_until(&processes, std::process::id(), || {
                    queue
                        .handoff_request(&LeaseToken::new("first"))
                        .unwrap()
                        .is_some()
                });
                queue
                    .resume_registration(
                        &LeaseToken::new("first"),
                        std::process::id(),
                        first_version,
                    )
                    .unwrap();
            });
            scope.spawn(|| {
                let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
                wait_until(&processes, SECOND, || {
                    queue
                        .handoff_request(&LeaseToken::new("second"))
                        .unwrap()
                        .is_some()
                });
                // The first one's outcome is seen before this one takes it.
                thread::sleep(Duration::from_millis(300));
                queue
                    .resume_registration(&LeaseToken::new("second"), SECOND, second_version)
                    .unwrap();
            });
            install_with(
                &fixture,
                &binaries,
                &processes,
                &no_down,
                &install_options(Source::Binary("/built/dagq".into())),
            )
        });
        let requests = [
            queue.handoff_request(&LeaseToken::new("first")).unwrap(),
            queue.handoff_request(&LeaseToken::new("second")).unwrap(),
        ];
        (result, binaries.calls(), requests)
    };

    let (result, calls, _) = two(VERSION, VERSION);
    let report = result.unwrap();
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["supervisors"].as_array().unwrap().len(), 2);
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");

    let (result, calls, requests) = two("0.0.1", VERSION);
    let error = result.unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("1 of the 2 supervisors took the handoff"),
        "{message}"
    );
    assert!(message.contains("supervisor first"), "{message}");
    assert!(message.contains("came back as 0.0.1"), "{message}");
    assert!(message.contains("`down --force` and `up`"), "{message}");
    assert!(message.contains("`install --rollback`"), "{message}");
    let report = &KeptBinary::of(&error).unwrap().report;
    assert_eq!(report["outcome"], "partially_handed_off", "{report}");
    assert_eq!(report["kept"], true, "{report}");
    let supervisors = report["supervisors"].as_array().unwrap();
    let first = supervisors.iter().find(|s| s["token"] == "first").unwrap();
    assert!(first["error"].as_str().is_some(), "{report}");
    let second = supervisors.iter().find(|s| s["token"] == "second").unwrap();
    assert_eq!(second["error"], Value::Null, "{report}");
    assert_eq!(second["previous_token"], "second", "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    assert_eq!(requests, [None, None]);

    let (result, calls, _) = two("0.0.1", "0.0.1");
    let error = result.unwrap_err();
    assert!(KeptBinary::of(&error).is_none());
    let failed = dagq::application::install::HandoffFailed::of(&error).unwrap();
    assert_eq!(failed.restored["restored"], true, "{:?}", failed.restored);
    assert_eq!(failed.supervisors.len(), 2, "{:?}", failed.supervisors);
    for supervisor in &failed.supervisors {
        assert!(supervisor["error"].as_str().is_some(), "{supervisor}");
        assert!(supervisor["pid"].as_u64().is_some(), "{supervisor}");
    }
    let message = format!("{error:#}");
    assert!(message.contains("is back at /opt/bin/dagq"), "{message}");
    assert!(message.contains("supervisor first"), "{message}");
    assert!(message.contains("supervisor second"), "{message}");
    assert_eq!(calls.last().unwrap(), "restore /opt/bin/dagq");
}

/// What a supervisor does once it took its request just as the handoff's
/// wait ran out.
#[derive(Clone, Copy)]
enum Late {
    /// Comes back under the same token and this build (or another).
    Resume(&'static str),
    /// Deregisters, its pid registering again under this build.
    Reregister,
    /// Never comes back.
    Nothing,
}

/// Have the supervisor `token` take its request at the moment `hand_off`
/// withdraws it after the wait ran out: the withdrawal finds no request
/// (`cancel_handoff` is `false`) and is recorded in `withdrawal_seen`, and
/// the request stays until the supervisor registers again. Armed before
/// the handoff starts, and only once.
fn take_at_withdrawal(fixture: &Fixture, token: &str) {
    rusqlite::Connection::open(&fixture.location.db)
        .unwrap()
        .execute_batch(&format!(
            "CREATE TABLE withdrawal_seen(token TEXT);
             CREATE TRIGGER take_at_withdrawal BEFORE UPDATE OF handoff_binary ON supervisors
             WHEN NEW.handoff_binary IS NULL AND OLD.handoff_binary IS NOT NULL
                  AND OLD.token = '{token}'
                  AND NOT EXISTS (SELECT 1 FROM withdrawal_seen)
             BEGIN
               INSERT INTO withdrawal_seen VALUES (OLD.token);
               SELECT RAISE(IGNORE);
             END;"
        ))
        .unwrap();
}

/// After the withdrawal [`take_at_withdrawal`] armed, do `late` as the
/// supervisor `token` of `pid`; when it came back under an update job's
/// `watch`, heartbeat on once the watch looked at it. `install` and
/// `hand_off` watch no heartbeat, so nothing is waited for then.
fn back_after_withdrawal(
    fixture: &Fixture,
    processes: &FakeProcesses,
    token: &str,
    pid: u32,
    late: Late,
    watch: bool,
) {
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let db = rusqlite::Connection::open(&fixture.location.db).unwrap();
    wait_until(processes, pid, || {
        db.query_row("SELECT count(*) FROM withdrawal_seen", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
            > 0
    });
    let serving = match late {
        Late::Resume(version) => {
            queue
                .resume_registration(&LeaseToken::new(token), pid, version)
                .unwrap();
            token.to_owned()
        }
        Late::Reregister => {
            let again = format!("{token}-again");
            queue
                .register_supervisor(&LeaseToken::new(&again), pid, 2, VERSION)
                .unwrap();
            queue
                .deregister_supervisor(&LeaseToken::new(token))
                .unwrap();
            again
        }
        Late::Nothing => return,
    };
    if watch {
        until_watched(processes, pid);
        heartbeat_later(fixture, &serving);
    }
}

/// A supervisor still asked when the handoff's wait runs out fails it when
/// its request is withdrawn (a); one whose request is gone by then took it
/// just as the wait ran out (task 715) and is looked at again: back under
/// this build under its token or its pid's successor, it took the handoff
/// (b); back under its old build, or not back within the grace, it did not,
/// and the error says it took the request (c).
#[test]
fn a_handoff_looks_again_at_a_supervisor_that_took_it_as_the_wait_ran_out() {
    let hand_off = |late: Option<Late>| {
        let fixture = fixture();
        let queue = handoff_supervisor(&fixture, "old", SupervisorMode::InCmux);
        let registration = queue.supervisors().unwrap().remove(0);
        let processes = FakeProcesses::default();
        if late.is_some() {
            take_at_withdrawal(&fixture, "old");
        }
        let handed = thread::scope(|scope| {
            if let Some(late) = late {
                let (fixture, processes, pid) = (&fixture, &processes, registration.pid);
                scope.spawn(move || {
                    back_after_withdrawal(fixture, processes, "old", pid, late, false)
                });
            }
            dagq::lifecycle::hand_off(
                &queue,
                &processes,
                &dagq::infrastructure::clock::SystemClock,
                std::slice::from_ref(&registration),
                Path::new("/opt/bin/dagq"),
                VERSION,
                Duration::from_secs(1),
                Duration::from_millis(20),
            )
            .unwrap()
        });
        let request = queue.handoff_request(&LeaseToken::new("old")).unwrap();
        (handed.into_iter().next().unwrap(), request)
    };

    let (handed, request) = hand_off(None);
    let error = handed.error.as_deref().unwrap();
    assert!(error.contains("did not take the handoff"), "{error}");
    assert_eq!(handed.now, None);
    assert_eq!(request, None);

    let (handed, _) = hand_off(Some(Late::Resume(VERSION)));
    assert_eq!(handed.error, None, "{handed:?}");
    assert_eq!(handed.now.as_ref().map(LeaseToken::as_str), Some("old"));
    assert_eq!(handed.report()["previous_token"], "old");

    let (handed, _) = hand_off(Some(Late::Reregister));
    assert_eq!(handed.error, None, "{handed:?}");
    assert_eq!(
        handed.now.as_ref().map(LeaseToken::as_str),
        Some("old-again")
    );

    let (handed, _) = hand_off(Some(Late::Resume("0.0.1")));
    let error = handed.error.as_deref().unwrap();
    assert!(error.contains("just as the wait ran out"), "{error}");
    assert!(error.contains("came back as 0.0.1"), "{error}");
    assert_eq!(handed.now, None);

    let (handed, request) = hand_off(Some(Late::Nothing));
    let error = handed.error.as_deref().unwrap();
    assert!(
        error.contains("took the handoff to /opt/bin/dagq just as the wait ran out"),
        "{error}"
    );
    assert!(
        error.contains("not back 1s after the wait ran out"),
        "{error}"
    );
    assert_eq!(handed.now, None);
    // The request is the supervisor's now, not withdrawn.
    assert_eq!(request.as_deref(), Some("/opt/bin/dagq"));
}

/// `install` counts a supervisor that took the handoff just as the wait
/// ran out and came back under the new build as handed over (task 715), so
/// the binary stays; one that never came back is a failure, and with every
/// supervisor failed the binary goes back.
#[test]
fn install_counts_a_supervisor_that_took_the_handoff_as_the_wait_ran_out() {
    use dagq::application::install::Source;
    let install = |late: Late| {
        let fixture = fixture();
        let _queue = handoff_supervisor(&fixture, "first", SupervisorMode::InCmux);
        take_at_withdrawal(&fixture, "first");
        let binaries = FakeBinaries::new(&[], true);
        let processes = FakeProcesses::default();
        let no_down = || -> Result<Value> { panic!("no drain") };
        let mut options = install_options(Source::Binary("/built/dagq".into()));
        options.handoff_timeout = Duration::from_secs(1);
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                back_after_withdrawal(
                    &fixture,
                    &processes,
                    "first",
                    std::process::id(),
                    late,
                    false,
                )
            });
            install_with(&fixture, &binaries, &processes, &no_down, &options)
        });
        (result, binaries.calls())
    };

    let (result, calls) = install(Late::Resume(VERSION));
    let report = result.unwrap();
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["supervisors"][0]["error"], Value::Null, "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");

    let (result, calls) = install(Late::Nothing);
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("just as the wait ran out"), "{message}");
    assert!(calls.iter().any(|c| c.starts_with("restore")), "{calls:?}");
}

/// The update job does not bring back a supervisor that took the handoff
/// just as the wait ran out and heartbeats on under the new build (task
/// 715): it is an install.
#[test]
fn the_update_job_installs_past_a_supervisor_that_took_the_handoff_as_the_wait_ran_out() {
    let fixture = fixture();
    let _queue = auto_supervisor(&fixture);
    take_at_withdrawal(&fixture, "auto");
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = thread::scope(|scope| {
        scope.spawn(|| {
            back_after_withdrawal(
                &fixture,
                &processes,
                "auto",
                UPDATED_PID,
                Late::Resume(VERSION),
                true,
            )
        });
        run_update_job(&fixture, &binaries, &processes, &restarted)
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert!(
        binaries.calls().iter().all(|c| !c.starts_with("restore")),
        "{:?}",
        binaries.calls()
    );
    assert!(restarted.lock().unwrap().is_empty());
}

/// A `cargo install` that leaves `binary` (or fails), noting each call.
struct FakeCargo {
    binary: PathBuf,
    fails: bool,
    calls: Mutex<Vec<String>>,
}

impl dagq::application::install::ReleaseInstaller for FakeCargo {
    fn install(&self, version: &str, root: &Path, target_dir: &Path, _: &Path) -> Result<PathBuf> {
        self.calls.lock().unwrap().push(format!(
            "cargo install dagq@{version} --root {} --target-dir {}",
            root.display(),
            target_dir.display()
        ));
        if self.fails {
            bail!("cargo was not found");
        }
        Ok(self.binary.clone())
    }
}

/// The installed plugin: its version, and an update whose second command
/// fails when `fails`; each call noted.
#[derive(Default)]
struct FakePlugin {
    fails: bool,
    calls: Mutex<Vec<String>>,
}

impl FakePlugin {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl dagq::application::InstalledPlugin for FakePlugin {
    fn version(&self) -> Result<Option<String>> {
        self.calls.lock().unwrap().push("version".into());
        Ok(Some("9.9.9".into()))
    }
    fn update(&self) -> Vec<dagq::application::PluginCommandRun> {
        use dagq::application::lifecycle::PLUGIN_UPDATE_ARGUMENTS;
        let mut runs = Vec::new();
        for (n, arguments) in PLUGIN_UPDATE_ARGUMENTS.iter().enumerate() {
            let command = format!("claude {}", arguments.join(" "));
            self.calls.lock().unwrap().push(command.clone());
            let succeeded = !(self.fails && n == 1);
            runs.push(dagq::application::PluginCommandRun {
                command,
                output: if succeeded { "ok" } else { "network down" }.into(),
                succeeded,
            });
        }
        runs
    }
}

fn run_release_job(
    fixture: &Fixture,
    binaries: &UpdateBinaries,
    cargo: &FakeCargo,
    processes: &FakeProcesses,
    version: &str,
) -> Value {
    run_release_job_with(fixture, binaries, cargo, processes, version, None, false)
}

fn run_release_job_with(
    fixture: &Fixture,
    binaries: &UpdateBinaries,
    cargo: &FakeCargo,
    processes: &FakeProcesses,
    version: &str,
    plugin: Option<&FakePlugin>,
    plugin_only: bool,
) -> Value {
    use dagq::application::update;
    let queues = |db: &Path| -> std::sync::Arc<dyn dagq::application::QueueOpener> {
        std::sync::Arc::new(dagq::infrastructure::runtime_store::SqliteOpener {
            db: db.to_owned(),
            generators: dagq::infrastructure::clock::system(),
            actor: None,
        })
    };
    let restart =
        |_: &dagq::domain::SupervisorRegistration| -> Result<Value> { Ok(json!({"by": "test"})) };
    let dir = fixture._dir.path();
    update::run_release(
        &update::JobPorts {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            restart: &restart,
        },
        cargo,
        plugin.map(|plugin| plugin as &dyn dagq::application::InstalledPlugin),
        &fixture.location.db,
        &update::ReleaseJobOptions {
            version: version.to_owned(),
            token: LeaseToken::new("auto"),
            target: dir.join("bin").join("dagq"),
            paths: update::UpdatePaths::under(&dir.join("queue-dir")),
            log: dir.join("cargo.log"),
            restart: vec!["--cmux".into(), "/opt/cmux".into()],
            handoff_timeout: Duration::from_secs(5),
            watch_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(20),
            pid: 7,
            plugin_only,
        },
    )
    .unwrap()
}

/// The release update's job (ADR-t618-1 decisions 5 and 7): the release
/// `cargo install`ed under the queue's `update/release` goes through the
/// same check, swap, handoff and watch as a build and ends in
/// `update_installed` with `source: release`; a failed `cargo install`
/// replaces nothing and asks; a breaking migration is left to
/// `approve_update`, whose command starts no `up --auto-update`.
#[test]
fn the_release_job_installs_a_release_like_a_build_and_asks_on_a_failure() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let cargo = |fails: bool| FakeCargo {
        binary: dir.join("built").join("dagq"),
        fails,
        calls: Mutex::default(),
    };
    let release_steps = |queue: &SqliteQueue| {
        queue
            .update_events(20)
            .unwrap()
            .into_iter()
            .map(|u| {
                assert_eq!(u.payload["source"], "release", "{u:?}");
                assert_eq!(u.payload["release"], "9.9.9", "{u:?}");
                assert!(u.payload.get("commit").is_none(), "{u:?}");
                u.kind
            })
            .collect::<Vec<_>>()
    };

    let binaries = UpdateBinaries::new(dir, false, &[(40, true)]);
    let installer = cargo(false);
    let report = thread::scope(|scope| {
        scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
        run_release_job(&fixture, &binaries, &installer, &processes, "9.9.9")
    });
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["release"], "9.9.9", "{report}");
    assert_eq!(report["previous_version"], "0.0.1", "{report}");
    assert_eq!(
        *installer.calls.lock().unwrap(),
        [format!(
            "cargo install dagq@9.9.9 --root {} --target-dir {}",
            dir.join("queue-dir/update/release").display(),
            dir.join("queue-dir/update/target").display()
        )]
    );
    assert_eq!(
        binaries.calls(),
        [
            "migrate".to_owned(),
            format!("replace {}", target.display())
        ]
    );
    assert_eq!(release_steps(&queue), ["update_installed", "update_built"]);
    let installed = &queue.update_events(1).unwrap()[0];
    assert_eq!(installed.payload["version"], VERSION);

    // cargo fails: nothing replaced, the `update_failed` ask says how to
    // go on by hand.
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let report = run_release_job(&fixture, &binaries, &cargo(true), &processes, "9.9.9");
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "build", "{report}");
    assert!(binaries.calls().is_empty(), "{:?}", binaries.calls());
    let asks = queue.asks(dagq::application::AskQuery::default()).unwrap();
    let failed = asks
        .iter()
        .find(|a| a.kind == dagq::domain::AskKind::UpdateFailed)
        .unwrap();
    assert!(
        failed
            .question
            .contains("release 9.9.9 failed at its build")
            && failed.question.contains("cargo was not found")
            && failed.question.contains("dagq install --release 9.9.9"),
        "{}",
        failed.question
    );
    assert_eq!(release_steps(&queue)[0], "update_failed");

    // A breaking migration, with the host set to install without asking
    // or not: kept for a person, and the command drains without an `up`.
    let binaries = UpdateBinaries::new(dir, false, &[(41, false)]);
    let report = run_release_job(&fixture, &binaries, &cargo(false), &processes, "9.9.9");
    assert_eq!(report["outcome"], "awaiting_approval", "{report}");
    let command = report["command"].as_str().unwrap();
    assert!(
        command.contains("install --from") && command.contains("--allow-breaking"),
        "{command}"
    );
    assert!(
        !command.contains("--auto-update") && !command.contains(" up"),
        "{command}"
    );
    assert!(binaries.calls().iter().all(|c| !c.starts_with("replace")));
    let approve = queue
        .asks(dagq::application::AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|a| a.kind == dagq::domain::AskKind::ApproveUpdate)
        .unwrap();
    assert!(
        approve.question.starts_with("release 9.9.9 (installed as"),
        "{}",
        approve.question
    );
    assert!(!approve.question.contains("--auto-update"));
    assert_eq!(release_steps(&queue)[0], "update_awaiting_approval");
}

/// A binary that is the release already (another queue's answer put it in
/// place) is not installed again nor replaced: only the handoff is left,
/// and `.previous` stays what it was.
#[test]
fn the_release_job_skips_cargo_when_the_binary_is_that_release() {
    let fixture = fixture();
    let processes = FakeProcesses::default();
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "release in place").unwrap();
    let binaries = UpdateBinaries::new(dir, false, &[]);
    let installer = FakeCargo {
        binary: dir.join("built").join("dagq"),
        fails: true,
        calls: Mutex::default(),
    };
    // Every binary under bin/ is 0.0.1 to the fake.
    let report = run_release_job(&fixture, &binaries, &installer, &processes, "0.0.1");
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["version"], "0.0.1", "{report}");
    assert!(installer.calls.lock().unwrap().is_empty());
    assert!(binaries.calls().is_empty(), "{:?}", binaries.calls());
}

/// The release job and the installed plugin (ADR-t618-2 decisions 1 to 3,
/// 5): after the binary is taken, the two update commands run and
/// `update_installed` says to reopen the sessions; a supervisor on
/// `--plugin-dir` leaves the plugin (`skipped: plugin-dir`); a failed
/// update asks at the `plugin` stage and leaves the new binary; a failed
/// handoff never touches the plugin.
#[test]
fn the_release_job_brings_the_plugin_after_the_binary() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let cargo = || FakeCargo {
        binary: dir.join("built").join("dagq"),
        fails: false,
        calls: Mutex::default(),
    };
    let latest = |kind: &str| {
        queue
            .update_events(20)
            .unwrap()
            .into_iter()
            .find(|u| u.kind == kind)
            .unwrap()
    };
    let job = |plugin: Option<&FakePlugin>, alive: bool| {
        let binaries = UpdateBinaries::new(dir, false, &[]);
        let report = thread::scope(|scope| {
            scope.spawn(|| take_and_heartbeat(&fixture, &processes, alive));
            run_release_job_with(
                &fixture,
                &binaries,
                &cargo(),
                &processes,
                "9.9.9",
                plugin,
                false,
            )
        });
        processes.dead.lock().unwrap().remove(&UPDATED_PID);
        (report, binaries.calls())
    };

    // Updated after the binary, and the attention says to reopen.
    let plugin = FakePlugin::default();
    let (report, _) = job(Some(&plugin), true);
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(
        plugin.calls(),
        [
            "claude plugin marketplace update dagq",
            "claude plugin update claude-dagq@dagq",
            "version"
        ]
    );
    let installed = latest("update_installed");
    assert_eq!(
        installed.payload["plugin"]["updated"], true,
        "{installed:?}"
    );
    assert_eq!(installed.payload["plugin"]["version"], "9.9.9");
    assert_eq!(
        installed.payload["plugin"]["commands"][1]["command"],
        "claude plugin update claude-dagq@dagq"
    );
    let message = installed.payload["message"].as_str().unwrap();
    assert!(
        message.contains("reopen them") && message.contains("inbox and planner"),
        "{message}"
    );
    // As `watch` shows it to the inbox.
    let attention = dagq::application::health::compact_event(&installed);
    assert_eq!(attention["plugin"]["updated"], true, "{attention}");
    assert_eq!(attention["next"], "report the update", "{attention}");
    assert!(
        attention["reason"]
            .as_str()
            .unwrap()
            .contains("reopen them"),
        "{attention}"
    );

    // A supervisor on --plugin-dir: nothing is run.
    let (report, _) = job(None, true);
    assert_eq!(report["outcome"], "installed", "{report}");
    let installed = latest("update_installed");
    assert_eq!(installed.payload["plugin"], "skipped: plugin-dir");
    assert!(installed.payload.get("message").is_none(), "{installed:?}");

    // The update fails: the binary stays, the ask is at the plugin stage.
    let plugin = FakePlugin {
        fails: true,
        ..FakePlugin::default()
    };
    let (report, calls) = job(Some(&plugin), true);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "plugin", "{report}");
    assert!(calls.iter().all(|c| !c.starts_with("restore")), "{calls:?}");
    let failed = latest("update_failed");
    assert_eq!(failed.payload["stage"], "plugin");
    assert_eq!(failed.payload["plugin"]["updated"], false);
    assert_eq!(
        failed.payload["plugin"]["commands"][1]["output"],
        "network down"
    );
    let question = queue
        .asks(dagq::application::AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|a| a.kind == dagq::domain::AskKind::UpdateFailed)
        .unwrap()
        .question;
    assert!(
        question.contains("claude-dagq plugin")
            && question.contains("network down")
            && question.contains("The binary stays")
            && question.contains("claude plugin marketplace update dagq"),
        "{question}"
    );

    // The supervisor does not take the binary: it goes back, and the
    // plugin is not touched.
    let plugin = FakePlugin::default();
    let (report, calls) = job(Some(&plugin), false);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "watch", "{report}");
    assert!(calls.iter().any(|c| c.starts_with("restore")), "{calls:?}");
    assert!(plugin.calls().is_empty(), "{:?}", plugin.calls());
}

/// The job that only brings the plugin to the release the binary is
/// already (ADR-t618-2 decision 4): no cargo, no swap; `update_installed`
/// with `plugin_only`, or `update_failed` at the `plugin` stage.
#[test]
fn the_plugin_only_job_updates_the_plugin_and_nothing_else() {
    let fixture = fixture();
    let queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let dir = fixture._dir.path();
    let cargo = FakeCargo {
        binary: dir.join("built").join("dagq"),
        fails: true,
        calls: Mutex::default(),
    };
    let binaries = UpdateBinaries::new(dir, true, &[]);
    let plugin = FakePlugin::default();
    let report = run_release_job_with(
        &fixture,
        &binaries,
        &cargo,
        &processes,
        "9.9.9",
        Some(&plugin),
        true,
    );
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["plugin_only"], true, "{report}");
    assert!(cargo.calls.lock().unwrap().is_empty());
    assert!(binaries.calls().is_empty(), "{:?}", binaries.calls());
    assert_eq!(plugin.calls().len(), 3, "{:?}", plugin.calls());
    let installed = &queue.update_events(1).unwrap()[0];
    assert_eq!(installed.kind, "update_installed");
    assert_eq!(installed.payload["release"], "9.9.9");
    assert_eq!(installed.payload["plugin"]["updated"], true);
    assert!(installed.payload["message"].is_string(), "{installed:?}");

    let failing = FakePlugin {
        fails: true,
        ..FakePlugin::default()
    };
    let report = run_release_job_with(
        &fixture,
        &binaries,
        &cargo,
        &processes,
        "9.9.9",
        Some(&failing),
        true,
    );
    assert_eq!(report["outcome"], "failed", "{report}");
    assert_eq!(report["stage"], "plugin", "{report}");
    let failed = &queue.update_events(1).unwrap()[0];
    assert_eq!(failed.kind, "update_failed");
    assert_eq!(failed.payload["plugin_only"], true);
    assert!(binaries.calls().is_empty());
}

#[test]
fn no_claude_policy_survives_install_drain_and_restart_arguments() {
    use dagq::application::install::Source;
    let fixture = fixture();
    let queue = handoff_supervisor(&fixture, "live", SupervisorMode::InCmux);
    queue
        .set_supervisor_providers(
            &LeaseToken::new("live"),
            &[dagq::domain::worker::ProviderCheck {
                provider: dagq::domain::Provider::Claude,
                executable: "/missing-claude".into(),
                found: false,
                error: Some("provider_disabled".into()),
                modes: vec![],
            }],
        )
        .unwrap();
    let registrations = queue.supervisors().unwrap();
    assert!(
        registrations[0]
            .flag_arguments()
            .contains(&"--no-claude".to_owned())
    );
    let processes = FakeProcesses::default();
    let down = || Ok(json!({"outcome": "stopped"}));
    for explicit in [false, true] {
        let binaries = FakeBinaries::new(&[(28, false)], false);
        let mut options = install_options(Source::Binary("/built/dagq".into()));
        options.allow_breaking = true;
        if explicit {
            options.restart.push("--no-claude".into());
        }
        install_with(&fixture, &binaries, &processes, &down, &options).unwrap();
        assert_eq!(
            binaries.up.lock().unwrap()[0]
                .iter()
                .filter(|a| *a == "--no-claude")
                .count(),
            1
        );
    }
}
