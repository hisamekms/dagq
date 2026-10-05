//! A person's `dagq install` watches the supervisors it handed over as the
//! automatic update's job does (ADR-0073 decisions 13 and 14, ADR-t632-1),
//! against the job's fake binaries and process signals: it succeeds once
//! each heartbeats on under the new build; when none does it puts a
//! `.previous` of the replaced build back, brings each back and tells the
//! inbox with `update_failed`; when some do it keeps the binary and brings
//! back only the failed ones; and its failure is no step of the automatic
//! update.

use crate::common;
use crate::lifecycle_install::{
    Afterwards, OTHER_PID, UPDATED_PID, UpdateBinaries, auto_supervisor, take_as,
};
use crate::runtime_support;
use common::lifecycle::*;

use anyhow::Result;
use dagq::{
    application::{
        AskQuery,
        install::{E2eGate, InstallOptions, Source},
        update::{self, InstallFailed},
    },
    domain::{AskKind, LeaseToken, SupervisorMode},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// How a person's install went with the supervisors doing what they were
/// given: its value or the [`InstallFailed`] report, the binaries' calls,
/// the tokens started again, the pids terminated and the fixture.
struct Installed {
    outcome: Result<Value, Value>,
    calls: Vec<String>,
    restarted: Vec<String>,
    /// The mode each one started again had, in the same order.
    restarted_modes: Vec<Option<SupervisorMode>>,
    terminated: Vec<u32>,
    target: PathBuf,
    fixture: Fixture,
}

/// Run `install --from <a build>` on a queue with the supervisors
/// `supervisors` (`auto`, then `other`), each doing what it is given after
/// the handoff; `previous` is what `.previous` reports when not the old
/// build.
fn install(supervisors: &[Afterwards], previous: Option<&str>) -> Installed {
    install_with(supervisors, previous, None)
}

/// The supervisor that registers while the install builds and checks its
/// binary, after the install took its record of the registrations.
const LATE_PID: u32 = 454_545;

/// [`install`], with `late`, when given, what a supervisor `late` (in-cmux,
/// workspace `ws-late`) that registers while the install checks the binary
/// does after the handoff.
fn install_with(
    supervisors: &[Afterwards],
    previous: Option<&str>,
    late: Option<Afterwards>,
) -> Installed {
    let fixture = fixture();
    let dir = fixture._dir.path().to_owned();
    if !supervisors.is_empty() {
        let mut queue = auto_supervisor(&fixture);
        if supervisors.len() > 1 {
            queue
                .register_supervisor(&LeaseToken::new("other"), OTHER_PID, 2, "0.0.1")
                .unwrap();
            queue.accept_handoff(&LeaseToken::new("other")).unwrap();
        }
    }
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let mut binaries = UpdateBinaries::new(&dir, false, &[]);
    binaries.previous_version = previous.map(str::to_owned);
    if late.is_some() {
        let db = fixture.location.db.clone();
        binaries.on_probe = Some(Box::new(move || {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let token = LeaseToken::new("late");
            queue
                .register_supervisor(&token, LATE_PID, 2, "0.0.1")
                .unwrap();
            queue.accept_handoff(&token).unwrap();
            queue
                .set_supervisor_mode(&token, SupervisorMode::InCmux, Some("ws-late"))
                .unwrap();
        }));
    }
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let done = AtomicBool::new(false);
    /// Ends the helper below however the install ends, a panic included.
    struct Done<'a>(&'a AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let outcome = thread::scope(|scope| {
        for (then, (token, pid)) in supervisors
            .iter()
            .zip([("auto", UPDATED_PID), ("other", OTHER_PID)])
        {
            let (fixture, processes) = (&fixture, &processes);
            scope.spawn(move || take_as(fixture, processes, token, pid, *then));
        }
        if let Some(then) = late {
            let (fixture, processes) = (&fixture, &processes);
            scope.spawn(move || take_as(fixture, processes, "late", LATE_PID, then));
        }
        // A supervisor the install terminates dies at once.
        scope.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                let terminated = processes.terminated.lock().unwrap().clone();
                processes.dead.lock().unwrap().extend(terminated);
                thread::sleep(Duration::from_millis(10));
            }
        });
        let _done = Done(&done);
        run(&fixture, &binaries, &processes, &restarted, &target)
    });
    let outcome = outcome.map_err(|error| {
        InstallFailed::of(&error)
            .unwrap_or_else(|| panic!("not an InstallFailed: {error:#}"))
            .report
            .clone()
    });
    let (restarted, restarted_modes) = restarted.into_inner().unwrap().into_iter().unzip();
    Installed {
        outcome,
        calls: binaries.calls(),
        restarted,
        restarted_modes,
        terminated: processes.terminated.lock().unwrap().clone(),
        target,
        fixture,
    }
}

fn run(
    fixture: &Fixture,
    binaries: &UpdateBinaries,
    processes: &FakeProcesses,
    restarted: &Mutex<Vec<(String, Option<SupervisorMode>)>>,
    target: &Path,
) -> Result<Value> {
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
            .push((registration.token.to_string(), registration.mode));
        Ok(json!({"by": "test"}))
    };
    let down = || -> Result<Value> { anyhow::bail!("no drain") };
    update::install_watched(
        &update::JobPorts {
            binaries,
            files: &dagq::infrastructure::run_files::LocalRunFiles,
            processes,
            clock: &dagq::infrastructure::clock::SystemClock,
            queues: &queues,
            restart: &restart,
        },
        &down,
        Some(&fixture.location.db),
        &InstallOptions {
            source: Source::Binary(binaries.built.clone()),
            target: target.to_owned(),
            allow_breaking: false,
            restart: vec!["--cmux".into(), "/opt/cmux".into()],
            handoff_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(20),
            e2e: E2eGate::NotApplicable,
        },
        Duration::from_secs(3),
    )
}

fn queue(installed: &Installed) -> SqliteQueue {
    SqliteQueue::open(&installed.fixture.location.db).unwrap()
}

/// The `update_failed` the install recorded, if any.
fn recorded_failure(installed: &Installed) -> Option<Value> {
    queue(installed)
        .update_events(20)
        .unwrap()
        .into_iter()
        .find(|u| u.kind == "update_failed")
        .map(|u| u.payload)
}

/// The open `update_failed` ask's question.
fn asked(installed: &Installed) -> String {
    let asks = queue(installed).asks(AskQuery::default()).unwrap();
    let ask = asks
        .iter()
        .find(|a| a.kind == AskKind::UpdateFailed && a.is_open())
        .unwrap_or_else(|| panic!("no update_failed ask: {asks:?}"));
    ask.question.clone()
}

/// The install succeeds only once the supervisor it handed over heartbeats
/// on under the new build, and says so per supervisor; with no supervisor
/// it watches nothing.
#[test]
fn a_persons_install_succeeds_once_each_supervisor_heartbeats_on() {
    let installed = install(&[Afterwards::Heartbeat], None);
    let report = installed.outcome.as_ref().unwrap();
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["watch"][0]["token"], "auto", "{report}");
    assert_eq!(report["watch"][0]["heartbeat"], true, "{report}");
    assert!(recorded_failure(&installed).is_none());
    assert!(installed.restarted.is_empty());

    let installed = install(&[], None);
    let report = installed.outcome.as_ref().unwrap();
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["watch"], json!([]), "{report}");
}

/// A build whose supervisor stops right after it registered under it: the
/// old binary goes back, the supervisor is started again, and the inbox is
/// told with what a person types next; no step of a job is written.
#[test]
fn a_build_that_stops_after_it_registered_goes_back_and_tells_the_inbox() {
    let installed = install(&[Afterwards::Die], None);
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["source"], "install", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(
        report["supervisors"][0]["supervisor"]["state"], "restarted",
        "{report}"
    );
    assert_eq!(installed.restarted, ["auto"]);
    assert!(
        installed
            .calls
            .contains(&format!("restore {}", installed.target.display())),
        "{:?}",
        installed.calls
    );
    let failure = recorded_failure(&installed).unwrap();
    assert_eq!(failure["source"], "install", "{failure}");
    assert_eq!(failure["ask_id"], report["ask_id"], "{failure}");
    let kinds: Vec<String> = queue(&installed)
        .update_events(20)
        .unwrap()
        .into_iter()
        .map(|u| u.kind)
        .collect();
    assert_eq!(kinds, ["update_failed"]);
    let question = asked(&installed);
    for said in [
        "A person's `dagq install`",
        "failed at its watch",
        "dagq install --rollback",
        "dagq down --force",
        "Nothing applies the answer",
    ] {
        assert!(question.contains(said), "{said}: {question}");
    }
}

/// A build that hangs once it registered (no heartbeat, the process alive)
/// is stopped and its supervisor started again.
#[test]
fn a_build_that_stops_heartbeating_is_stopped_and_started_again() {
    let installed = install(&[Afterwards::Freeze], None);
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["stage"], "watch", "{report}");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("did not heartbeat within 3s"),
        "{report}"
    );
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(installed.terminated, [UPDATED_PID]);
    assert_eq!(
        report["supervisors"][0]["supervisor"]["state"], "restarted",
        "{report}"
    );
    assert_eq!(installed.restarted, ["auto"]);
}

/// When one of two supervisors runs the new build, the binary stays and
/// only the one that failed is brought back (ADR-t632-1); the inbox is told
/// with `kept`.
#[test]
fn only_the_failed_supervisor_is_brought_back_when_the_other_runs_the_build() {
    let installed = install(&[Afterwards::Heartbeat, Afterwards::Die], None);
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["kept"], true, "{report}");
    assert_eq!(report["restored"]["restored"], false, "{report}");
    assert!(
        installed.calls.iter().all(|c| !c.starts_with("restore")),
        "{:?}",
        installed.calls
    );
    assert_eq!(installed.restarted, ["other"]);
    assert_eq!(recorded_failure(&installed).unwrap()["kept"], true);
    assert!(asked(&installed).contains("The new binary stays"));
}

/// The handoff's own failures tell the inbox too: every supervisor failing
/// it (`install`, the binary put back) and only some (`handoff`, `kept`).
#[test]
fn the_failures_of_the_handoff_tell_the_inbox_too() {
    let installed = install(
        &[Afterwards::DieBeforeTaking, Afterwards::DieBeforeTaking],
        None,
    );
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["stage"], "install", "{report}");
    assert_eq!(report["kept"], false, "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(report["supervisors"].as_array().unwrap().len(), 2);
    assert!(asked(&installed).contains("failed at its install"));

    let installed = install(&[Afterwards::Heartbeat, Afterwards::DieBeforeTaking], None);
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["stage"], "handoff", "{report}");
    assert_eq!(report["kept"], true, "{report}");
    assert_eq!(installed.restarted, ["other"]);
    assert!(asked(&installed).contains("failed at its handoff"));
}

/// A supervisor that registers while the install builds and checks its
/// binary, after the install took its record of the registrations, is
/// handed over all the same, and brought back in its mode when the new
/// build fails it: stopped and started again when it hangs, started again
/// when it dies after it registered again under the new build (its new row
/// has no mode of its own).
#[test]
fn a_supervisor_that_registers_during_the_install_is_brought_back_too() {
    let installed = install_with(&[], None, Some(Afterwards::Freeze));
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["stage"], "watch", "{report}");
    assert_eq!(report["restored"]["restored"], true, "{report}");
    assert_eq!(report["supervisors"][0]["token"], "late", "{report}");
    assert_eq!(
        report["supervisors"][0]["supervisor"]["state"], "restarted",
        "{report}"
    );
    assert_eq!(installed.terminated, [LATE_PID]);
    assert_eq!(installed.restarted, ["late"]);
    assert_eq!(installed.restarted_modes, [Some(SupervisorMode::InCmux)]);

    let installed = install_with(
        &[Afterwards::Heartbeat],
        None,
        Some(Afterwards::ReregisterAndDie),
    );
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["kept"], true, "{report}");
    let late = report["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["pid"] == LATE_PID)
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(late["supervisor"]["state"], "restarted", "{report}");
    assert_eq!(installed.restarted, ["late-again"]);
    assert_eq!(installed.restarted_modes, [Some(SupervisorMode::InCmux)]);
}

/// A `.previous` of another build than the one replaced is not put back,
/// and the inbox is told why.
#[test]
fn a_previous_of_another_build_is_not_put_back() {
    let installed = install(&[Afterwards::Die], Some("0.0.0"));
    let report = installed.outcome.as_ref().unwrap_err();
    assert_eq!(report["restored"]["restored"], false, "{report}");
    assert!(
        report["restored"]["reason"]
            .as_str()
            .unwrap()
            .contains("rather than the replaced 0.0.1"),
        "{report}"
    );
    assert!(installed.calls.iter().all(|c| !c.starts_with("restore")));
    assert!(asked(&installed).contains("rather than the replaced"));
}

/// A person's install's failure is no step of the automatic update: its
/// state stays idle, no `retry` is read from it, `stats` counts it apart,
/// its ask opens beside a job's, and with a live supervisor that has the
/// automatic update its answer stays the inbox's: `answer` records no
/// runtime delivery, `status` asks a person to read it, and a pass of a
/// supervisor with the automatic update, which applies a job's answer
/// beside it, neither applies it (no `update_retry`, no build) nor closes
/// its ask.
#[test]
fn a_persons_install_failure_leaves_the_automatic_update_alone() {
    let installed = install(&[Afterwards::Die], None);
    let report = installed.outcome.as_ref().unwrap_err();
    let db = &installed.fixture.location.db;
    let status = dagq::compose::status(db).unwrap();
    assert_eq!(status["auto_update"]["state"], "idle", "{status}");
    let mut queue = queue(&installed);
    let updates = queue.update_events(20).unwrap();
    assert!(!update::retry_requested(&updates));
    assert!(update::latest_job_step(&updates).is_none());
    let stats = dagq::domain::stats::updates::updates(
        &updates,
        dagq::domain::EventId::new(0),
        dagq::domain::EventId::new(i64::MAX),
        |_| true,
    );
    assert_eq!(stats.by_kind.get("update_failed"), None, "{stats:?}");
    assert_eq!(stats.by_kind["update_failed_install"], 1, "{stats:?}");
    assert_eq!(stats.install_failed_by_stage["watch"], 1, "{stats:?}");

    // A job's failure opens its ask beside it: neither supersedes the
    // other.
    let ask = dagq::domain::AskId::new(report["ask_id"].as_i64().unwrap());
    let job = queue
        .open_update_ask(
            AskKind::UpdateFailed,
            "the job failed",
            dagq::domain::UPDATE_FAILED_OPTIONS,
            "supervisor",
            None,
            Value::Null,
        )
        .unwrap();
    let open: Vec<_> = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .filter(|a| a.is_open())
        .map(|a| (a.id, a.subject))
        .collect();
    assert_eq!(
        open,
        [(ask, Some("install".to_owned())), (job.id, None)],
        "{open:?}"
    );

    // A live supervisor with the automatic update leaves the answer to
    // the inbox.
    let token = LeaseToken::new("live");
    queue
        .register_supervisor(&token, std::process::id(), 2, dagq::VERSION)
        .unwrap();
    queue.set_auto_update(&token, true).unwrap();
    queue.answer(ask, "retry").unwrap();
    let answered: String = rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT payload FROM run_events WHERE kind='ask_answered'
             AND json_extract(payload,'$.ask_id')=?1",
            [ask.as_i64()],
            |row| row.get(0),
        )
        .unwrap();
    let answered: Value = serde_json::from_str(&answered).unwrap();
    assert_eq!(answered["runtime_delivers"], false, "{answered}");
    let updates = queue.update_events(20).unwrap();
    assert!(!update::retry_requested(&updates));
    // A job's answer beside it is the supervisor's to apply.
    queue.answer(job.id, "skip").unwrap();
    let next = |status: &Value, ask: dagq::domain::AskId| {
        status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["ask_id"] == ask.as_i64())
            .map(|a| a["next"].clone())
            .unwrap_or_else(|| panic!("no attention for ask {ask}: {status}"))
    };
    let status = dagq::compose::status(db).unwrap();
    assert_eq!(
        next(&status, ask),
        format!("read the answer of ask {ask} and close it"),
        "{status}"
    );
    assert_eq!(
        next(&status, job.id),
        format!("applying the answer of ask {} (runtime)", job.id),
        "{status}"
    );

    // A pass of a supervisor with the automatic update in dagq's source,
    // main's head already tried by an earlier job (so only an answer could
    // make it build): it applies the job's `skip`, and leaves the person's
    // install's `retry` alone.
    let repo = &installed.fixture.repo;
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"dagq\"\n").unwrap();
    git(repo, &["add", "Cargo.toml"]);
    git(repo, &["commit", "-q", "-m", "dagq's manifest"]);
    let head = {
        use common::Bounded;
        let output = std::process::Command::new(
            dagq::infrastructure::git_binary::git_executable().expect("git executable"),
        )
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .bounded_output()
        .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    for (kind, payload) in [
        (
            dagq::domain::EventKind::UpdateStarted,
            json!({"pid": 999_999_999u32, "commit": head}),
        ),
        (
            dagq::domain::EventKind::UpdateFailed,
            json!({"pid": 999_999_999u32, "commit": head, "stage": "build"}),
        ),
    ] {
        queue.record_queue_event(kind, payload).unwrap();
    }
    let options = dagq::compose::SuperviseOptions {
        update: dagq::application::supervise::UpdateSettings {
            register: true,
            interval: Duration::ZERO,
            build_command: Some("echo no build here >&2; exit 1".into()),
            e2e_command: None,
            e2e_timeout: None,
            poll: None,
            cmux: Some(PathBuf::from("/usr/bin/true")),
            cargo: None,
        },
        ..runtime_support::supervise_options(1, true)
    };
    let backend = runtime_support::TestWorkspace::new(db, false, runtime_support::VALID_AGENT);
    runtime_support::supervise_with(db, repo, &backend, &options).unwrap();
    let updates = queue.update_events(50).unwrap();
    let kinds: Vec<&str> = updates.iter().map(|u| u.kind.as_str()).collect();
    assert!(
        updates
            .iter()
            .any(|u| u.kind == "update_answered" && u.payload["ask_id"] == job.id.as_i64()),
        "{kinds:?}"
    );
    assert!(!kinds.contains(&"update_retry"), "{kinds:?}");
    assert_eq!(
        kinds.iter().filter(|k| **k == "update_started").count(),
        1,
        "{kinds:?}"
    );
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());
    assert!(queue.read_ask(job.id).unwrap().closed_at.is_some());
    let status = dagq::compose::status(db).unwrap();
    assert_eq!(
        next(&status, ask),
        format!("read the answer of ask {ask} and close it"),
        "{status}"
    );
}
