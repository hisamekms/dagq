//! `install`, `install --rollback` and the automatic and release updates'
//! jobs reach the queue only through its service
//! (docs/design/supervisor-lifecycle/install.md): each read and record is a
//! step of their command the service answers as the caller's principal,
//! authorized as `service.install`. The in-process tests' own process
//! refuses to open the queue ([`refuse_opening`]); the service runs in
//! another. The jobs run as the binary does, with the token of the
//! supervisor that started them.

use crate::common;
use common::install::{FakeBinaries, install_options};
use common::lifecycle::{
    FakeLaunchd, FakeProcesses, fixture, handoff_supervisor, on_socket, serve_on_up,
};
use common::{Bounded, actor::WithoutActor, service::OwnedByTest};

use dagq::{
    application::{
        install::{E2eGate, InstallOptions, Source},
        supervisor_service::unreachable,
    },
    domain::{
        ActorContext, ActorRole, LeaseToken, SupervisorMode,
        queue_service::{
            CREDENTIAL_FILE_ENV, ControlCall, ControlCommand, ControlParams, Principal, UseCase,
        },
    },
    infrastructure::{
        queue_service as service,
        sqlite::{SqliteQueue, refuse_opening},
    },
};
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};

/// The events of `kind` on the queue at `db`, oldest first: each one's
/// payload and its actor's id.
fn recorded(db: &Path, kind: &str) -> Vec<(Value, Option<String>)> {
    let mut events = SqliteQueue::open(db)
        .unwrap()
        .latest_events_of(kind, 10)
        .unwrap();
    events.sort_by_key(|event| event.id);
    events
        .into_iter()
        .map(|event| (event.payload, event.actor.map(|actor| actor.id)))
        .collect()
}

/// The install of `source` into `target` without an e2e, whose handoff
/// waits `handoff` at most.
fn options(source: Source, target: &Path, handoff: Duration) -> InstallOptions {
    InstallOptions {
        target: target.to_path_buf(),
        handoff_timeout: handoff,
        e2e: E2eGate::NotApplicable,
        ..install_options(source)
    }
}

/// `install` and `install --rollback` read the registrations through the
/// service and replace the binary. With no service, `install` starts its
/// build's to read through, and with that one not answering either it
/// fails as out of reach and replaces nothing. The test's process never
/// opens the queue.
#[test]
fn install_and_its_rollback_reach_the_queue_only_through_its_service() {
    let mut fixture = fixture();
    let stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    let target = db.parent().unwrap().join("bin").join("dagq");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(dagq::application::install::previous_path(&target), "").unwrap();
    let launchd = FakeLaunchd::new(&db);
    let processes = FakeProcesses::default();
    let install = |binaries: &FakeBinaries, source: Source| {
        on_socket().install_on(
            &fixture.location,
            &launchd,
            &processes,
            binaries,
            &options(source, &target, Duration::from_secs(5)),
            Duration::from_secs(5),
        )
    };
    let refused = refuse_opening(&db);
    let binaries = FakeBinaries::new(&[], true);
    let installed = install(&binaries, Source::Binary("/built/dagq".into())).unwrap();
    assert_eq!(installed["outcome"], "installed", "{installed}");
    assert_eq!(installed["supervisors"], json!([]), "{installed}");
    let rolled = install(&binaries, Source::Rollback).unwrap();
    assert_eq!(rolled["outcome"], "installed", "{rolled}");
    let previous = dagq::application::install::previous_path(&target);
    let replaced = |source: &Path| format!("replace {} {}", source.display(), target.display());
    assert_eq!(
        binaries.calls(),
        [
            "probe /built/dagq".to_owned(),
            replaced(Path::new("/built/dagq")),
            format!("probe {}", previous.display()),
            replaced(&previous),
        ]
    );

    stop.stop();
    let binaries = FakeBinaries::new(&[], true);
    let error = install(&binaries, Source::Binary("/built/dagq".into())).unwrap_err();
    drop(refused);
    assert!(unreachable(&error), "{error:#}");
    assert_eq!(binaries.calls(), ["probe /built/dagq", "serve /built/dagq"]);
}

/// A person's install whose handoff no supervisor takes sends the handoff,
/// its withdrawal, the `update_failed` record and the inbox's ask to the
/// service as the user, and the failure stays the install's.
#[test]
fn an_install_s_handoff_and_failure_are_the_user_s_steps() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    drop(handoff_supervisor(&fixture, "live", SupervisorMode::InCmux));
    let binaries = FakeBinaries::new(&[], true);
    let processes = FakeProcesses::default();
    let refused = refuse_opening(&db);
    let error = on_socket()
        .install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &processes,
            &binaries,
            &options(
                Source::Binary("/built/dagq".into()),
                Path::new("/opt/bin/dagq"),
                Duration::from_millis(300),
            ),
            Duration::from_secs(5),
        )
        .unwrap_err();
    drop(refused);
    let error = format!("{error:#}");
    assert!(error.contains("the install failed at its"), "{error}");
    // The service took the request, which no supervisor took in time.
    assert!(
        error.contains("did not take the handoff to /opt/bin/dagq"),
        "{error}"
    );
    let failed = recorded(&db, "update_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0["source"], "install", "{failed:?}");
    assert_eq!(failed[0].1.as_deref(), Some("user"));
    assert!(failed[0].0["ask_id"].is_number(), "{failed:?}");
    // The handoff was asked for and withdrawn on the service.
    let live = SqliteQueue::open(&db).unwrap().supervisors().unwrap();
    assert_eq!(live[0].handoff_binary, None);
}

/// The service stops and starts again while an install waits for its
/// handoff: the steps no service took meanwhile go to the service that
/// answers next, also those of a queue the install opens after the stop,
/// and the failure is recorded on it.
#[test]
fn an_install_s_steps_go_on_across_a_restart_of_the_service() {
    let mut fixture = fixture();
    let stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    let exe = fixture.environment.current_exe.clone();
    dagq::compose::queue_service_start(&db, &exe, None, |_| None).unwrap();
    drop(handoff_supervisor(&fixture, "live", SupervisorMode::InCmux));
    let binaries = FakeBinaries::new(&[], true);
    let processes = FakeProcesses::default();
    let pid = std::process::id();
    let refused = refuse_opening(&db);
    let error = std::thread::scope(|scope| {
        // Once the install looks at the handoff it asked for (its second
        // look at the pid: the first is its read of the live supervisors,
        // before the request, a write the service may have done when its
        // answer is cut off, which is not sent again), the service goes
        // away for a while and comes back.
        let restart = scope.spawn(|| {
            let deadline = common::deadline(common::STEP_LIMIT);
            while processes.looks_at(pid) < 2 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.stop();
            std::thread::sleep(Duration::from_millis(500));
            dagq::compose::queue_service_start(&db, &exe, None, |_| None).unwrap();
        });
        let error = on_socket()
            .install_on(
                &fixture.location,
                &FakeLaunchd::new(&db),
                &processes,
                &binaries,
                &options(
                    Source::Binary("/built/dagq".into()),
                    Path::new("/opt/bin/dagq"),
                    Duration::from_secs(3),
                ),
                Duration::from_secs(5),
            )
            .unwrap_err();
        let _waiting = common::within(common::STEP_LIMIT, "the restart thread to return");
        restart.join().unwrap();
        error
    });
    drop(refused);
    let error = format!("{error:#}");
    assert!(
        error.contains("did not take the handoff to /opt/bin/dagq"),
        "{error}"
    );
    let failed = recorded(&db, "update_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].1.as_deref(), Some("user"));
    assert_eq!(recorded(&db, "queue_service_started").len(), 2);
}

/// `install --allow-breaking`'s drain runs `down --wait` through the
/// service, which reads the live supervisor and waits for its end, and the
/// install goes on with the new build's service: neither opens the queue.
#[test]
fn install_s_drain_reaches_the_queue_only_through_its_service() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    drop(handoff_supervisor(&fixture, "live", SupervisorMode::InCmux));
    let binaries = FakeBinaries::new(&[(9_999, false)], false);
    let processes = FakeProcesses::default();
    let mut options = options(
        Source::Binary("/built/dagq".into()),
        Path::new("/opt/bin/dagq"),
        Duration::from_secs(5),
    );
    options.allow_breaking = true;
    let pid = std::process::id();
    let refused = refuse_opening(&db);
    let report = std::thread::scope(|scope| {
        // The drained supervisor's end, once `down` asked for it.
        let drain = scope.spawn(|| {
            let deadline = common::deadline(common::STEP_LIMIT);
            while !processes.interrupted.lock().unwrap().contains(&pid)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            processes.dead.lock().unwrap().insert(pid);
        });
        let report = on_socket().install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &processes,
            &binaries,
            &options,
            Duration::from_secs(5),
        );
        let _waiting = common::within(common::STEP_LIMIT, "the drain thread to return");
        drain.join().unwrap();
        report
    })
    .unwrap();
    drop(refused);
    assert_eq!(report["outcome"], "installed", "{report}");
    assert_eq!(report["drained"]["outcome"], "stopped", "{report}");
    assert!(
        binaries
            .calls()
            .contains(&"serve /built/dagq breaking".to_owned()),
        "{:?}",
        binaries.calls()
    );
    assert_eq!(binaries.up.lock().unwrap().len(), 1);
    assert_eq!(*processes.interrupted.lock().unwrap(), [pid]);
}

/// The wiring of the steps of `install` and the update jobs at the
/// service's socket, `service.install`'s (every role's judgement is
/// `control`'s unit test): the user's install and a supervisor's update job
/// are answered, a worker's is refused as `authorization_denied` and
/// recorded, a handoff naming no absolute path is refused, and an update
/// ask whose details are no object is the request's error, the service
/// answering on.
#[test]
fn only_the_install_s_principals_send_its_steps() {
    use common::service::{call, queue, start, token};
    let queue = queue();
    let run = queue.run.id().clone();
    let task = queue.run.task_id();
    let registration = LeaseToken::new("installing-supervisor");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&registration, std::process::id(), 1, dagq::VERSION)
        .unwrap();
    start(&queue);
    let pid = std::process::id();
    let step = |command: ControlCommand, call: Value| {
        serde_json::to_value(ControlParams {
            control: ControlCall {
                command,
                task: None,
                yes: false,
                pid,
            },
            call: serde_json::from_value(call).unwrap(),
        })
        .unwrap()
    };
    let supervisor = token(&queue, &Principal::supervisor(&registration, pid));
    for (who, command) in [
        (None, ControlCommand::Install),
        (Some(supervisor.as_str()), ControlCommand::AutoUpdate),
    ] {
        for op in ["control_queue", "supervisors"] {
            let answer = call(
                &queue,
                who,
                UseCase::Control,
                step(command, json!({"op": op})),
            );
            assert_eq!(answer["ok"], true, "{command:?} {op}: {answer}");
        }
    }
    // An install's handoff names the binary it put in place by its
    // absolute path; another is refused, recorded.
    let relative = step(
        ControlCommand::Install,
        json!({"op": "request_handoff", "args": [registration.as_str(), "dagq"]}),
    );
    let answer = call(&queue, None, UseCase::Control, relative);
    assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
    let before = common::service::events(&queue.db, "authorization_denied").len();
    let worker = token(&queue, &Principal::worker(&run, task));
    let answer = call(
        &queue,
        Some(&worker),
        UseCase::Control,
        step(ControlCommand::Install, json!({"op": "supervisors"})),
    );
    assert_eq!(answer["error"]["code"], "authorization_denied", "{answer}");
    let denials = common::service::events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, 1);
    let ask = step(
        ControlCommand::Install,
        json!({
            "op": "open_update_ask",
            "record": "an-identity",
            "args": ["update_failed", "failed", ["retry", "skip"], "supervisor", null, [1]],
        }),
    );
    let answer = call(&queue, None, UseCase::Control, ask);
    assert_eq!(answer["error"]["code"], "bad_request", "{answer}");
    let answer = call(
        &queue,
        None,
        UseCase::Control,
        step(ControlCommand::Install, json!({"op": "supervisors"})),
    );
    assert_eq!(answer["ok"], true, "{answer}");
}

/// Runs the binary's `args` on the queue at `db` as the job a supervisor
/// starts: the supervisor's role and actor, and the file of its token when
/// given (none for a supervisor whose service runs in its own process).
fn job(db: &Path, credential: Option<&Path>, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .without_actor_env()
        .owned_by_test()
        .envs(ActorContext::instance(ActorRole::Supervisor, std::process::id()).env())
        .arg("--db")
        .arg(db)
        .args(args);
    command.env_remove(dagq::domain::queue_service::SOCKET_ENV);
    match credential {
        Some(file) => command.env(CREDENTIAL_FILE_ENV, file),
        None => command.env_remove(CREDENTIAL_FILE_ENV),
    };
    command.bounded_output().unwrap()
}

/// The automatic update's and the release update's jobs send their steps
/// with the token of the supervisor that started them: a build that fails
/// is recorded as `update_failed`, with the inbox's ask, as that
/// supervisor. With the service gone, a job fails as out of reach and
/// records nothing.
#[test]
fn the_update_jobs_record_as_the_supervisor_that_started_them() {
    use common::service::{queue, start};
    let queue = queue();
    let pid = std::process::id();
    let registration = LeaseToken::new("updating-supervisor");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&registration, pid, 1, dagq::VERSION)
        .unwrap();
    start(&queue);
    let issued =
        service::issue(queue.dir(), &Principal::supervisor(&registration, pid), 1).unwrap();
    let dir = queue.dir();
    let not_a_repository = dir.join("no-repository");
    std::fs::create_dir_all(&not_a_repository).unwrap();
    let to = dir.join("bin").join("dagq");
    let path = |path: &Path| path.to_str().unwrap().to_owned();
    let auto_update = |credential| {
        job(
            &queue.db,
            credential,
            &[
                "auto-update",
                "--commit",
                "c0ffee",
                "--token",
                registration.as_str(),
                "--to",
                &path(&to),
                "--repo",
                &path(&not_a_repository),
                "--log",
                &path(&dir.join("update.build.log")),
                "--claude",
                "/opt/bin/claude",
                "--codex",
                "/opt/bin/codex",
            ],
        )
    };
    let output = auto_update(Some(&issued.file));
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "failed", "{report}");
    let output = job(
        &queue.db,
        Some(&issued.file),
        &[
            "release-update",
            "--release",
            "0.0.0-none",
            "--token",
            registration.as_str(),
            "--to",
            &path(&to),
            "--log",
            &path(&dir.join("release.build.log")),
            "--cargo",
            "/usr/bin/false",
            "--claude",
            "/opt/bin/claude",
            "--codex",
            "/opt/bin/codex",
            "--plugin-dir",
            &path(dir),
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let failed = recorded(&queue.db, "update_failed");
    let supervisor = Some(format!("supervisor:{pid}"));
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(failed[0].0["commit"], "c0ffee", "{failed:?}");
    assert_eq!(failed[1].0["release"], "0.0.0-none", "{failed:?}");
    assert!(
        failed.iter().all(|(_, actor)| *actor == supervisor),
        "{failed:?}"
    );

    // A job of a supervisor that names its role without its token is
    // refused, and one whose service is gone fails as out of reach; the
    // queue is opened for neither.
    let refused = auto_update(None);
    assert!(!refused.status.success(), "{refused:?}");
    dagq::compose::queue_service_stop(&queue.db, None, |_| None).unwrap();
    let unreached = auto_update(Some(&issued.file));
    assert!(!unreached.status.success(), "{unreached:?}");
    let stderr = String::from_utf8_lossy(&unreached.stderr);
    assert!(stderr.contains("dagq service status"), "{stderr}");
    assert_eq!(recorded(&queue.db, "update_failed").len(), 2);
}

/// An update job whose watch fails stops its own supervisor to bring it
/// back, and that stop ends the registration whose token the job's steps
/// go under (the supervisor revokes it as it exits): the job records
/// `update_failed` and opens its ask before it stops it, and the stop
/// still happens. The release's binary is in place already, so only the
/// watch of the supervisor, whose heartbeat is stale, fails.
#[test]
fn an_update_job_records_its_failure_before_it_stops_its_own_supervisor() {
    use common::service::{queue, start};
    let _test = common::test();
    let queue = queue();
    let dir = queue.dir().to_owned();
    let (terminated, revoked) = (dir.join("terminated"), dir.join("revoked"));
    // A supervisor that, stopped, waits until its token is revoked, as one
    // that deregisters and gives its token back on its way out.
    let script = format!(
        "on_term() {{ touch {terminated}; {revoked}; exit 0; }}; trap on_term TERM; \
while :; do sleep 0.05; done",
        terminated = common::shell_path(&terminated),
        revoked = common::await_path(&revoked),
    );
    let mut supervisor = common::KillOnDrop::new(
        Command::new("/bin/sh")
            .args(["-c", &script])
            .spawn()
            .unwrap(),
        "the stand-in supervisor",
    );
    let pid = supervisor.child().id();
    let registration = LeaseToken::new("updating-supervisor");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&registration, pid, 1, dagq::VERSION)
        .unwrap();
    rusqlite::Connection::open(&queue.db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET heartbeat_at = unixepoch() - 3600 WHERE token = ?1",
            rusqlite::params![registration.as_str()],
        )
        .unwrap();
    start(&queue);
    let principal = Principal::supervisor(&registration, pid);
    let issued = service::issue(&dir, &principal, 1).unwrap();
    let to = dir.join("bin").join("dagq");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_dagq"), &to).unwrap();
    let path = |path: &Path| path.to_str().unwrap().to_owned();
    let (output, gave_back) = std::thread::scope(|scope| {
        let gave_back = scope.spawn(|| {
            let deadline = common::deadline(common::STEP_LIMIT);
            while !terminated.exists() {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            SqliteQueue::open(&queue.db)
                .unwrap()
                .deregister_supervisor(&registration)
                .unwrap();
            service::revoke(&dir, &principal.credential_key()).unwrap();
            std::fs::write(&revoked, "").unwrap();
            true
        });
        let output = job(
            &queue.db,
            Some(&issued.file),
            &[
                "release-update",
                "--release",
                dagq::VERSION,
                "--token",
                registration.as_str(),
                "--to",
                &path(&to),
                "--log",
                &path(&dir.join("release.build.log")),
                "--cargo",
                "/usr/bin/false",
                "--claude",
                "/opt/bin/claude",
                "--codex",
                "/opt/bin/codex",
                "--plugin-dir",
                &path(&dir),
                "--watch-timeout",
                "1",
            ],
        );
        if !terminated.exists() {
            std::fs::write(&terminated, "").unwrap();
        }
        let _waiting = common::within(common::STEP_LIMIT, "the revocation to return");
        (output, gave_back.join().unwrap())
    });
    assert!(output.status.success(), "{output:?}");
    assert!(gave_back, "the job did not stop its supervisor: {output:?}");
    let failed = recorded(&queue.db, "update_failed");
    assert_eq!(failed.len(), 1, "{failed:?} {output:?}");
    let (payload, actor) = &failed[0];
    assert_eq!(payload["stage"], "watch", "{payload}");
    assert_eq!(actor.as_deref(), Some(format!("supervisor:{pid}").as_str()));
    assert_eq!(
        payload["supervisors"][0]["supervisor"]["state"], "brought_back_after_this_record",
        "{payload}"
    );
    assert!(payload["ask_id"].as_i64().is_some(), "{payload}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["brought_back"][0]["stopped"], true, "{report}");
}

/// An update's job whose service does not answer fails as out of reach at
/// its first step, before it builds anything, and does not open the queue
/// instead.
#[test]
fn an_update_job_with_no_service_opens_no_queue() {
    let fixture = fixture();
    let db = fixture.location.db.clone();
    let dir = db.parent().unwrap();
    let refused = refuse_opening(&db);
    let error = on_socket()
        .auto_update(
            &fixture.location,
            &dagq::compose::AutoUpdateJob {
                commit: "c0ffee".into(),
                token: LeaseToken::new("updating-supervisor"),
                target: dir.join("bin").join("dagq"),
                repository: fixture.repo.clone(),
                log: dir.join("update.build.log"),
                build_command: None,
                e2e_command: None,
                e2e_timeout: Duration::from_secs(1),
                claude: "/opt/bin/claude".into(),
                codex: "/opt/bin/codex".into(),
                plugin_dir: None,
                handoff_timeout: Duration::from_secs(1),
                watch_timeout: Duration::from_secs(1),
                poll: Duration::from_millis(20),
            },
        )
        .unwrap_err();
    drop(refused);
    assert!(unreachable(&error), "{error:#}");
    assert!(!dir.join("update").join("checkout").exists());
}

/// A service of a build before `install`'s steps on the queue at `db`: it
/// takes every request and refuses it as one whose params it does not read,
/// as such a service refuses them, until `stop` is set; it then takes no
/// more and returns, leaving its socket's file to the one that replaces it.
fn service_of_a_build_before(
    db: &Path,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    let socket = service::socket_path(db.parent().unwrap());
    let _ = std::fs::remove_file(&socket);
    // A short queue path keeps the socket in the queue's own service
    // directory, which no service made yet.
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let deadline = common::deadline(common::STEP_LIMIT);
        while !stop.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = Vec::new();
                    let _ = stream.read_to_end(&mut request);
                    let answer = dagq::domain::queue_service::ServiceResponse::failure(
                        dagq::domain::queue_service::ServiceErrorCode::BadRequest,
                        "the params of control do not read: unknown variant `install`",
                    );
                    let _ = stream.write_all(&serde_json::to_vec(&answer).unwrap());
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    })
}

/// `install` meeting a service of a build before it, which reads none of
/// its steps, starts its build's service in that one's place, as with no
/// service, and goes on through it; the test's process opens no queue.
#[test]
fn install_replaces_a_service_that_reads_none_of_its_steps() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    let exe = fixture.environment.current_exe.clone();
    let before = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let older = std::sync::Mutex::new(Some(service_of_a_build_before(&db, before.clone())));
    let binaries = FakeBinaries::new(&[], true);
    // The new build's service: the older one ends, and this build's starts
    // in its place.
    let (stopped, served) = (before.clone(), db.clone());
    let older = std::sync::Arc::new(older);
    let ending = older.clone();
    *binaries.on_serve.lock().unwrap() = Some(Box::new(move || {
        stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(older) = ending.lock().unwrap().take() {
            let _waiting = common::within(common::STEP_LIMIT, "the older service to stop");
            older.join().unwrap();
            let _ = std::fs::remove_file(service::socket_path(served.parent().unwrap()));
        }
        dagq::compose::queue_service_start(&served, &exe, None, |_| None).unwrap();
    }));
    let target = db.parent().unwrap().join("bin").join("dagq");
    let refused = refuse_opening(&db);
    let installed = on_socket()
        .install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &FakeProcesses::default(),
            &binaries,
            &options(
                Source::Binary("/built/dagq".into()),
                &target,
                Duration::from_secs(5),
            ),
            Duration::from_secs(5),
        )
        .unwrap();
    drop(refused);
    before.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(older) = older.lock().unwrap().take() {
        let _waiting = common::within(common::STEP_LIMIT, "the older service to stop");
        older.join().unwrap();
    }
    assert_eq!(installed["outcome"], "installed", "{installed}");
    assert_eq!(
        installed["queue_service"]["outcome"], "replaced",
        "{installed}"
    );
    assert_eq!(
        binaries.calls()[..2],
        ["probe /built/dagq", "serve /built/dagq"]
    );
}

/// A rollback to a build before this one, whose service would read none of
/// this install's steps once its supervisor brought it back, is that
/// binary's own `install --rollback` with this one's options; this process
/// opens no queue.
#[test]
fn a_rollback_to_a_build_before_is_handed_to_that_binary() {
    let fixture = fixture();
    let db = fixture.location.db.clone();
    let target = db.parent().unwrap().join("bin").join("dagq");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let previous = dagq::application::install::previous_path(&target);
    std::fs::write(&previous, "").unwrap();
    let binaries = FakeBinaries::new(&[], true);
    binaries
        .sends_steps
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let refused = refuse_opening(&db);
    let report = on_socket()
        .install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &FakeProcesses::default(),
            &binaries,
            &options(Source::Rollback, &target, Duration::from_secs(7)),
            Duration::from_secs(9),
        )
        .unwrap();
    drop(refused);
    assert_eq!(report["installed_by"], json!(previous), "{report}");
    assert_eq!(binaries.calls(), [format!("run {}", previous.display())]);
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    let mut expected = vec![
        "--db".to_owned(),
        text(&db),
        "install".into(),
        "--rollback".into(),
        "--to".into(),
        text(&target),
        "--handoff-timeout".into(),
        "7".into(),
        "--poll-ms".into(),
        "20".into(),
        "--watch-timeout".into(),
        "9".into(),
    ];
    expected.extend(["--claude".to_owned(), "/opt/claude".to_owned()]);
    assert_eq!(*binaries.up.lock().unwrap(), [expected]);
}

/// A checkout whose build is of a build before this one is built and passes
/// its e2e gate here, recorded through the running service as the user;
/// that build's own `install --from` with this one's options then does the
/// rest, as for a rollback. This process opens no queue.
#[test]
fn a_checkout_of_a_build_before_is_handed_to_its_build() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    let dir = db.parent().unwrap().to_path_buf();
    let target = dir.join("bin").join("dagq");
    let checkout = dir.join("checkout");
    let built = checkout.join("target/release/dagq");
    let binaries = FakeBinaries::new(&[], true);
    binaries
        .sends_steps
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let refused = refuse_opening(&db);
    let report = on_socket()
        .install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &FakeProcesses::default(),
            &binaries,
            &InstallOptions {
                e2e: E2eGate::Run(common::install::e2e_settings(&dir)),
                ..options(
                    Source::Checkout(checkout.clone()),
                    &target,
                    Duration::from_secs(7),
                )
            },
            Duration::from_secs(9),
        )
        .unwrap();
    drop(refused);
    assert_eq!(report["installed_by"], json!(built), "{report}");
    assert_eq!(report["e2e"]["status"], "passed", "{report}");
    assert!(report["e2e"]["event_id"].is_number(), "{report}");
    assert_eq!(
        binaries.calls(),
        [
            format!("build {}", checkout.display()),
            format!("e2e {}", checkout.display()),
            format!("run {}", built.display()),
        ]
    );
    let text = |path: &Path| path.to_str().unwrap().to_owned();
    let expected = [
        "--db".to_owned(),
        text(&db),
        "install".into(),
        "--from".into(),
        text(&built),
        "--to".into(),
        text(&target),
        "--handoff-timeout".into(),
        "7".into(),
        "--poll-ms".into(),
        "20".into(),
        "--watch-timeout".into(),
        "9".into(),
        "--claude".into(),
        "/opt/claude".into(),
    ];
    assert_eq!(*binaries.up.lock().unwrap(), [expected]);
    let gates = recorded(&db, "install_e2e_finished");
    assert_eq!(gates.len(), 1, "{gates:?}");
    assert_eq!(gates[0].0["binary"], json!(built), "{gates:?}");
    assert_eq!(gates[0].1.as_deref(), Some("user"), "{gates:?}");
}

/// A proxy in front of the queue service of `db`: its socket is moved
/// aside and each request is passed to it, but for the first `times` ones
/// naming `op`, whose answer is dropped once the service took it (`after`)
/// or which are not passed (before), each connection closed with no
/// answer, as a replacement of the service cuts a request off. It runs
/// until `stop` is set, puts the service's socket back and returns whether
/// it cut one.
fn cutting_off(
    db: &Path,
    op: &'static str,
    after: bool,
    times: usize,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<bool> {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    let socket = service::socket_path(db.parent().unwrap());
    let aside = socket.with_extension("aside");
    std::fs::rename(&socket, &aside).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let mut cut = 0;
        let deadline = common::deadline(common::STEP_LIMIT);
        while !stop.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            let Ok((mut client, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            client.set_nonblocking(false).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let _ = client.read_to_end(&mut request);
            let names = cut < times && request.windows(op.len()).any(|w| w == op.as_bytes());
            if names && !after {
                cut += 1;
                continue;
            }
            let mut service = UnixStream::connect(&aside).unwrap();
            service
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            service.write_all(&request).unwrap();
            service.shutdown(std::net::Shutdown::Write).unwrap();
            let mut answer = Vec::new();
            service.read_to_end(&mut answer).unwrap();
            if names {
                cut += 1;
                continue;
            }
            let _ = client.write_all(&answer);
        }
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        std::fs::rename(&aside, &socket).unwrap();
        cut > 0
    })
}

/// The service took an install's request for the handoff and its answer
/// was cut off: the install finds the request in the registrations rather
/// than failing, and the supervisor that took it runs the new build while
/// the binary stays in place.
#[test]
fn a_handoff_request_whose_answer_was_cut_off_is_found_in_the_registrations() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    drop(handoff_supervisor(&fixture, "live", SupervisorMode::InCmux));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = cutting_off(&db, "request_handoff", true, 1, stop.clone());
    let binaries = FakeBinaries::new(&[], true);
    let processes = FakeProcesses::default();
    let installed = std::thread::scope(|scope| {
        // The supervisor takes the request, comes back under the new build
        // and heartbeats on.
        let supervisor = scope.spawn(|| {
            let mut queue = SqliteQueue::open(&db).unwrap();
            let token = LeaseToken::new("live");
            let pid = std::process::id();
            let deadline = common::deadline(common::STEP_LIMIT);
            while queue.handoff_request(&token).unwrap().is_none()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(queue.take_handoff(&token, "/opt/bin/dagq").unwrap());
            queue
                .resume_registration(&token, pid, dagq::VERSION)
                .unwrap();
            while !stop.load(std::sync::atomic::Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                queue.heartbeat(&token).unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let installed = on_socket().install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &processes,
            &binaries,
            &options(
                Source::Binary("/built/dagq".into()),
                Path::new("/opt/bin/dagq"),
                Duration::from_secs(5),
            ),
            Duration::from_secs(5),
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _waiting = common::within(common::STEP_LIMIT, "the supervisor to return");
        supervisor.join().unwrap();
        installed
    });
    let waiting = common::within(common::STEP_LIMIT, "the proxy to return");
    assert!(proxy.join().unwrap(), "no answer was cut off");
    drop(waiting);
    let installed = installed.unwrap();
    assert_eq!(installed["outcome"], "installed", "{installed}");
    assert_eq!(installed["supervisors"][0]["token"], "live", "{installed}");
    assert_eq!(installed["watch"][0]["heartbeat"], true, "{installed}");
    assert!(
        !binaries
            .calls()
            .iter()
            .any(|call| call.starts_with("restore")),
        "{:?}",
        binaries.calls()
    );
}

/// An install's failure whose update ask the service took but whose answer
/// was cut off is sent again under its identity: one ask opens, and the
/// failure's record names it. A step no service took is sent again as
/// `supervisor_service::across`'s unit test judges.
#[test]
fn an_update_ask_cut_off_after_the_service_took_it_opens_once() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    drop(handoff_supervisor(&fixture, "live", SupervisorMode::InCmux));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = cutting_off(&db, "open_update_ask", true, 1, stop.clone());
    let binaries = FakeBinaries::new(&[], true);
    let refused = refuse_opening(&db);
    let error = on_socket()
        .install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &FakeProcesses::default(),
            &binaries,
            &options(
                Source::Binary("/built/dagq".into()),
                Path::new("/opt/bin/dagq"),
                Duration::from_millis(300),
            ),
            Duration::from_secs(5),
        )
        .unwrap_err();
    drop(refused);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let waiting = common::within(common::STEP_LIMIT, "the proxy to return");
    assert!(proxy.join().unwrap(), "no answer was cut off");
    drop(waiting);
    let error = format!("{error:#}");
    assert!(error.contains("the install failed at its"), "{error}");
    let asks = SqliteQueue::open(&db)
        .unwrap()
        .asks(dagq::application::AskQuery {
            all: true,
            open: false,
            role: None,
        })
        .unwrap();
    let opened: Vec<_> = asks
        .iter()
        .filter(|ask| ask.kind == dagq::domain::AskKind::UpdateFailed)
        .collect();
    assert_eq!(opened.len(), 1, "{opened:?}");
    let failed = recorded(&db, "update_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].0["ask_id"], json!(opened[0].id));
}

/// An install whose handoff no supervisor takes in time, and each of whose
/// withdrawals of the request is cut off before the service took it, ends
/// with the request maybe still taken: the binary stays in place, nothing
/// is put back, and the request is left to its supervisor.
#[test]
fn a_handoff_whose_withdrawal_is_cut_off_leaves_the_binary_in_place() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    dagq::compose::queue_service_start(&db, &fixture.environment.current_exe, None, |_| None)
        .unwrap();
    drop(handoff_supervisor(&fixture, "live", SupervisorMode::InCmux));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = cutting_off(&db, "cancel_handoff", false, usize::MAX, stop.clone());
    let binaries = FakeBinaries::new(&[], true);
    let refused = refuse_opening(&db);
    let error = on_socket()
        .install_on(
            &fixture.location,
            &FakeLaunchd::new(&db),
            &FakeProcesses::default(),
            &binaries,
            &options(
                Source::Binary("/built/dagq".into()),
                Path::new("/opt/bin/dagq"),
                Duration::from_millis(300),
            ),
            Duration::from_secs(5),
        )
        .unwrap_err();
    drop(refused);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let waiting = common::within(common::STEP_LIMIT, "the proxy to return");
    assert!(proxy.join().unwrap(), "no withdrawal was cut off");
    drop(waiting);
    let error = format!("{error:#}");
    assert!(error.contains("stays at /opt/bin/dagq"), "{error}");
    assert!(
        !binaries
            .calls()
            .iter()
            .any(|call| call.starts_with("restore")),
        "{:?}",
        binaries.calls()
    );
    let live = SqliteQueue::open(&db).unwrap().supervisors().unwrap();
    assert_eq!(live[0].handoff_binary.as_deref(), Some("/opt/bin/dagq"));
}
