//! The fake binaries and the calls of `dagq install` the install tests
//! (`tests/it/lifecycle_install*.rs`) share: [`FakeBinaries`] that build,
//! run and move nothing, and [`install_with`] against the fixture's queue.

use super::lifecycle::{FakeProcesses, Fixture};

use anyhow::Result;
use dagq::VERSION;
use dagq::application::install::{E2eGate, E2eOutcome};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// [`Binaries`] that build, run and move nothing: what each call was, the
/// schema it reports, and whether it answers a probe.
pub struct FakeBinaries {
    pub schema: dagq::application::install::SchemaCheck,
    pub calls: Mutex<Vec<String>>,
    /// The arguments of each `run`: the `up` that starts the supervisor
    /// again after a drain.
    pub up: Mutex<Vec<Vec<String>>>,
    /// How the e2e of a checkout goes: passes unless set.
    pub e2e: Mutex<Option<E2eOutcome>>,
    /// Why the service a replacement starts refuses to migrate: it
    /// migrates unless set.
    pub serve_refuses: Mutex<Option<String>>,
    /// The binary whose service fails to start, after the one there was
    /// stopped or not ([`Self::service_runs`]).
    pub serve_fails_for: Mutex<Option<PathBuf>>,
    /// Whether a service of the queue answers after a failed start.
    pub service_runs: std::sync::atomic::AtomicBool,
    /// What each `serve` does besides noting it: a test's own service.
    pub on_serve: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Whether the binaries' `install` sends its steps to the service (a
    /// build of before it does not).
    pub sends_steps: std::sync::atomic::AtomicBool,
}

impl FakeBinaries {
    pub fn new(pending: &[(i64, bool)], opens: bool) -> Self {
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
            serve_refuses: Mutex::default(),
            serve_fails_for: Mutex::default(),
            service_runs: std::sync::atomic::AtomicBool::new(false),
            on_serve: Mutex::default(),
            sends_steps: std::sync::atomic::AtomicBool::new(true),
        }
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    pub fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

/// An e2e that passed in `secs`.
pub fn e2e_passed(secs: u64) -> E2eOutcome {
    E2eOutcome {
        passed: true,
        secs,
        cleanup: json!({"removed": true}),
        ..Default::default()
    }
}

/// An e2e whose `failed` tests failed, rerun by name, with `still` failing
/// again, and the marks of `marks` (`(name, until)`) (ADR-t1165-1).
pub fn e2e_rerun(failed: &[&str], still: &[&str], marks: &[(&str, &str)]) -> E2eOutcome {
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
pub fn e2e_settings(dir: &Path) -> dagq::application::install::E2eSettings {
    dagq::application::install::E2eSettings {
        command: None,
        timeout: Duration::from_secs(1800),
        run_env_root: None,
        queue_dir: None,
        scratch: dir.join("e2e"),
        log: dir.join("e2e.log"),
        utc_offset_secs: 0,
        lock: None,
        git_env: Vec::new(),
        ended: None,
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
    fn serve(&self, binary: &Path, _: &Path, breaking: bool) -> Result<Value> {
        let breaking = if breaking { " breaking" } else { "" };
        self.note(format!("serve {}{breaking}", binary.display()));
        if let Some(error) = self.serve_refuses.lock().unwrap().clone() {
            anyhow::bail!(error);
        }
        if self.serve_fails_for.lock().unwrap().as_deref() == Some(binary) {
            anyhow::bail!("the queue service did not answer");
        }
        if let Some(serve) = &*self.on_serve.lock().unwrap() {
            serve();
        }
        Ok(json!({"applied": self.schema.pending.len()}))
    }
    fn sends_install_steps(&self, _: &Path) -> bool {
        self.sends_steps.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn service_runs(&self, _: &Path) -> bool {
        self.service_runs.load(std::sync::atomic::Ordering::SeqCst)
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
        assert_eq!(
            target,
            Some(checkout.join("target").as_path()),
            "install's e2e uses the build's own target"
        );
        self.note(format!("e2e {}", checkout.display()));
        Ok(self.e2e.lock().unwrap().clone().unwrap_or(e2e_passed(3)))
    }
}

/// `install` of the queue of `fixture` with `binaries`, `processes` and
/// `down` for the drain, against the real queue.
pub fn install_with(
    fixture: &Fixture,
    binaries: &FakeBinaries,
    processes: &FakeProcesses,
    down: &dyn Fn() -> Result<Value>,
    options: &dagq::application::install::InstallOptions,
) -> Result<Value> {
    install_at(
        Some(&fixture.location.db),
        binaries,
        processes,
        down,
        options,
    )
}

/// [`install_with`] of the queue at `db`, or of none.
pub fn install_at(
    db: Option<&Path>,
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
        db,
        options,
    )
}

/// `install` of `source` into `/opt/bin/dagq`, restarting with `--claude
/// /opt/claude`, without `--allow-breaking`.
pub fn install_options(
    source: dagq::application::install::Source,
) -> dagq::application::install::InstallOptions {
    dagq::application::install::InstallOptions {
        source,
        target: "/opt/bin/dagq".into(),
        allow_breaking: false,
        restart: vec!["--claude".into(), "/opt/claude".into()],
        handoff_timeout: Duration::from_secs(5),
        poll: Duration::from_millis(20),
        e2e: E2eGate::Run(e2e_settings(Path::new("/queue"))),
    }
}
