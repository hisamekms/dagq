//! `up`, `down` and `service start|stop` reach the queue only through its
//! service (docs/design/queue-service.md, "起動と停止"): each operation
//! they make is a step of their command the service answers as the
//! caller's principal, authorized as `service.lifecycle`. `up` and
//! `service start` start the service before they send it anything, `down`
//! and `service stop` send their record before it stops, and a service that
//! does not answer is the command's failure, never a reason to open the
//! queue instead. Each test's own process refuses to open the queue
//! ([`refuse_opening`]); the service runs in another.

use crate::common;
use common::lifecycle::{FakeLaunchd, FakeProcesses, fixture, on_socket, serve_on_up};

use dagq::{
    application::supervisor_service::unreachable,
    domain::{
        ActorContext, ActorRole, LeaseToken, TaskId,
        queue_service::{ControlCall, ControlCommand, ControlParams, Principal, UseCase},
        session_step::StepTarget,
    },
    infrastructure::sqlite::{SqliteQueue, refuse_opening},
    lifecycle::DownOptions,
};
use serde_json::{Value, json};
use std::time::Duration;

/// What the queue recorded of `kind`, oldest first: each record's `by` and
/// its actor's id.
fn recorded(queue: &SqliteQueue, kind: &str) -> Vec<(Value, Option<String>)> {
    let mut events = queue.latest_events_of(kind, 10).unwrap();
    events.sort_by_key(|event| event.id);
    events
        .into_iter()
        .map(|event| {
            (
                event.payload["by"].clone(),
                event.actor.map(|actor| actor.id),
            )
        })
        .collect()
}

/// `up` starts the service and then registers, reads and records through
/// it; a second `up` reuses what runs; `down --force` records the service's
/// stop before it stops it. With the service gone, `down` fails as out of
/// reach and opens nothing; `service start` records through the service it
/// started, and `service stop` records before it stops it. Every record is
/// the user's, and the test's process never opened the queue.
#[test]
fn up_down_and_the_service_s_start_and_stop_reach_the_queue_only_through_it() {
    let mut fixture = fixture();
    let _stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    let mut launchd = FakeLaunchd::new(&db);
    launchd.registers_through_service = true;
    let processes = FakeProcesses::default();
    let shot = on_socket();
    let down = |force| {
        shot.down(
            &fixture.location,
            &launchd,
            &processes,
            &DownOptions {
                wait: false,
                force,
                poll: Duration::from_millis(20),
            },
        )
    };
    let up = || {
        shot.up(
            &fixture.location,
            &fixture.repo,
            &launchd,
            &processes,
            &fixture.environment,
            &fixture.options,
        )
    };

    let refused = refuse_opening(&db);
    let first = up().unwrap();
    assert_eq!(first["queue_service"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["outcome"], "started", "{first}");
    assert_eq!(first["supervisor"]["mode"], "launchd", "{first}");
    let second = up().unwrap();
    assert_eq!(second["queue_service"]["outcome"], "reused", "{second}");
    assert_eq!(second["supervisor"]["outcome"], "reused", "{second}");
    let forced = down(true).unwrap();
    assert_eq!(forced["outcome"], "killed", "{forced}");
    assert_eq!(forced["queue_service"]["outcome"], "stopped", "{forced}");
    let stopped_by_down = forced["queue_service"]["pid"].clone();
    // No service: `down` fails as out of reach, and does not open the queue.
    let error = down(false).unwrap_err();
    assert!(unreachable(&error), "{error:#}");
    let exe = fixture.environment.current_exe.clone();
    let started = dagq::compose::queue_service_start(&db, &exe, None, |_| None).unwrap();
    assert_eq!(started["outcome"], "started", "{started}");
    let stopped = dagq::compose::queue_service_stop(&db, None, |_| None).unwrap();
    assert_eq!(stopped["outcome"], "stopped", "{stopped}");
    assert!(stopped.get("unrecorded").is_none(), "{stopped}");
    let again = dagq::compose::queue_service_stop(&db, None, |_| None).unwrap();
    assert_eq!(again["outcome"], "not_running", "{again}");
    drop(refused);

    let queue = SqliteQueue::open(&db).unwrap();
    let user = Some("user".to_owned());
    assert_eq!(
        recorded(&queue, "queue_service_started"),
        [
            (json!("up"), user.clone()),
            (json!("service start"), user.clone())
        ]
    );
    assert_eq!(
        recorded(&queue, "queue_service_stopped"),
        [(json!("down"), user.clone()), (json!("service stop"), user)]
    );
    // The service `down` stopped is the one its record names.
    let stopped = queue
        .latest_events_of("queue_service_stopped", 10)
        .unwrap()
        .into_iter()
        .find(|event| event.payload["by"] == "down")
        .unwrap();
    assert_eq!(stopped.payload["pid"], stopped_by_down);
    // The supervisor `up` started registered, and `down --force` pruned it.
    assert!(queue.supervisors().unwrap().is_empty());
}

/// The steps of `up`, `down`, `dagq inbox` and `service start|stop` are
/// `service.lifecycle`'s at the service's own socket: the user, the inbox,
/// a planner and a supervisor are answered, and a worker, every job, the observer, a
/// wrapper, an integrator and an executor's telemetry are refused as
/// `authorization_denied`, each refusal recorded, before anything is read.
#[test]
fn only_the_service_lifecycle_s_principals_send_the_lifecycle_s_steps() {
    use common::service::{call, queue, start, token};
    let queue = queue();
    let run = queue.run.id().clone();
    let task = queue.run.task_id();
    let (planner, registration) = {
        let mut direct = SqliteQueue::open(&queue.db).unwrap();
        let planner = direct
            .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
            .unwrap()
            .id
            .as_i64();
        let registration = LeaseToken::new("updating-supervisor");
        direct
            .register_supervisor(&registration, std::process::id(), 1, dagq::VERSION)
            .unwrap();
        (planner, registration)
    };
    start(&queue);
    let pid = std::process::id();
    let session = dagq::domain::queue_service::SessionProcess {
        pid,
        start: dagq::infrastructure::adapters::session_start(pid).unwrap(),
    };
    let step = |command: ControlCommand, op: &str| {
        serde_json::to_value(ControlParams {
            control: ControlCall {
                command,
                task: None,
                yes: false,
                pid,
            },
            call: serde_json::from_value(json!({"op": op})).unwrap(),
        })
        .unwrap()
    };
    let commands = [
        ControlCommand::Up,
        ControlCommand::Down,
        ControlCommand::Inbox,
        ControlCommand::ServiceStart,
        ControlCommand::ServiceStop,
    ];
    let allowed = [
        None,
        Some(token(&queue, &Principal::inbox(session))),
        Some(token(&queue, &Principal::planner(planner))),
        Some(token(&queue, &Principal::supervisor(&registration, pid))),
    ];
    for who in &allowed {
        for command in commands {
            for op in ["control_queue", "supervisors"] {
                let answer = call(&queue, who.as_deref(), UseCase::Control, step(command, op));
                assert_eq!(answer["ok"], true, "{command:?} {op}: {answer}");
            }
        }
    }
    let target = StepTarget::run(&run);
    let refused = [
        Principal::worker(&run, task),
        Principal::of(&ActorContext::review_job(&run, 1)),
        Principal::of(&ActorContext::recovery_job(&run, "alert", 1)),
        Principal::of(&ActorContext::plan_review_job(1, 1)),
        Principal::of(&ActorContext::goal_review_job(1, 1)),
        Principal::of(&ActorContext::throughput_review_job("daily", "p")),
        Principal::of(&ActorContext::setup_review_job("s1")),
        Principal::of(&ActorContext::instance(ActorRole::Observer, "o1")),
        Principal::wrapper(&target, Some(task)),
        Principal::integrator(&run, &LeaseToken::new("a-landing"), pid),
        Principal::telemetry(&target, Some(TaskId::new(task.as_i64()))),
    ];
    let before = common::service::events(&queue.db, "authorization_denied").len();
    for principal in &refused {
        let token = token(&queue, principal);
        for command in commands {
            let answer = call(
                &queue,
                Some(&token),
                UseCase::Control,
                step(command, "supervisors"),
            );
            assert_eq!(
                answer["error"]["code"], "authorization_denied",
                "{} {command:?}: {answer}",
                principal.actor_id
            );
        }
    }
    let denials = common::service::events(&queue.db, "authorization_denied");
    assert_eq!(denials.len() - before, refused.len() * commands.len());
}

/// An environment as `service stop` reads it.
type Env = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// `service stop` sends its record before the stop, and a service that
/// refuses the record (an environment naming a role without its token, a
/// token the service does not know, a repository the queue is not bound
/// to) is not stopped: the command fails and the service keeps its pid. The
/// user's stop then stops it.
#[test]
fn a_stop_the_service_refuses_to_record_stops_nothing() {
    use common::service::{queue, start};
    use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, ServiceState};
    let queue = queue();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .bind_repository("/the/repo/.git")
        .unwrap();
    start(&queue);
    let running = dagq::infrastructure::queue_service::probe(queue.db.parent().unwrap());
    assert_eq!(running.state, ServiceState::Running, "{running:?}");
    let unknown = queue.db.parent().unwrap().join("unknown-token");
    std::fs::write(&unknown, "not-a-token-the-service-issued").unwrap();
    let unknown = unknown.to_str().unwrap().to_owned();
    let refusals: [(Option<&str>, Env); 3] = [
        (
            None,
            Box::new(|name: &str| (name == dagq::domain::actor::ROLE_ENV).then(|| "inbox".into())),
        ),
        (
            None,
            Box::new(move |name: &str| (name == CREDENTIAL_FILE_ENV).then(|| unknown.clone())),
        ),
        (Some("/another/repo/.git"), Box::new(|_: &str| None)),
    ];
    for (binding, env) in refusals {
        let error = dagq::compose::queue_service_stop(&queue.db, binding, env).unwrap_err();
        assert!(!unreachable(&error), "{error:#}");
        assert!(format!("{error:#}").contains("not stopped"), "{error:#}");
        let found = dagq::infrastructure::queue_service::probe(queue.db.parent().unwrap());
        assert_eq!(found.state, ServiceState::Running, "{found:?}");
        assert_eq!(found.pid, running.pid, "{found:?}");
    }
    let stopped =
        dagq::compose::queue_service_stop(&queue.db, Some("/the/repo/.git"), |_| None).unwrap();
    assert_eq!(stopped["outcome"], "stopped", "{stopped}");
    assert!(stopped.get("unrecorded").is_none(), "{stopped}");
}

/// `service start` that finds a service of its build answering sends its
/// first step all the same, so the service authenticates and authorizes
/// the caller and checks the repository's binding on a reused service too:
/// an environment naming a role without its token, a token of a principal
/// `service.lifecycle` refuses (a worker's) and another repository are
/// refused, nothing is recorded, and the service keeps its pid. The user's
/// start reuses it.
#[test]
fn a_start_that_reuses_the_service_still_takes_only_its_callers() {
    use common::service::{queue, start};
    use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, ServiceState};
    let queue = queue();
    SqliteQueue::open(&queue.db)
        .unwrap()
        .bind_repository("/the/repo/.git")
        .unwrap();
    start(&queue);
    let running = dagq::infrastructure::queue_service::probe(queue.dir());
    assert_eq!(running.state, ServiceState::Running, "{running:?}");
    let started = || common::service::events(&queue.db, "queue_service_started").len();
    let before = started();
    let worker = dagq::infrastructure::queue_service::issue(
        queue.dir(),
        &Principal::worker(queue.run.id(), queue.run.task_id()),
        1,
    )
    .unwrap()
    .file
    .to_str()
    .unwrap()
    .to_owned();
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_dagq"));
    let refusals: [(Option<&str>, Env, &str); 3] = [
        (
            None,
            Box::new(|name: &str| (name == dagq::domain::actor::ROLE_ENV).then(|| "inbox".into())),
            "unauthenticated",
        ),
        (
            None,
            Box::new(move |name: &str| (name == CREDENTIAL_FILE_ENV).then(|| worker.clone())),
            "authorization_denied",
        ),
        (
            Some("/another/repo/.git"),
            Box::new(|_: &str| None),
            "bound to another Git repository",
        ),
    ];
    for (binding, env, why) in refusals {
        let error = dagq::compose::queue_service_start(&queue.db, exe, binding, env).unwrap_err();
        assert!(format!("{error:#}").contains(why), "{why}: {error:#}");
        let found = dagq::infrastructure::queue_service::probe(queue.dir());
        assert_eq!(found.state, ServiceState::Running, "{found:?}");
        assert_eq!(found.pid, running.pid, "{found:?}");
    }
    assert_eq!(started(), before);
    let reused =
        dagq::compose::queue_service_start(&queue.db, exe, Some("/the/repo/.git"), |_| None)
            .unwrap();
    assert_eq!(reused["outcome"], "reused", "{reused}");
    assert_eq!(started(), before);
}

/// `down --wait` through the service: the last supervisor ends its drain
/// by stopping the service, so `down`'s next look finds it out of reach;
/// once `end` makes the supervisor's process (this test's pid) look ended,
/// the drain is over, and `down` reports the stop without opening the
/// queue.
fn down_wait_after_the_drain_stopped_the_service(end: fn(&FakeProcesses, u32)) {
    let mut fixture = fixture();
    let stop = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    let mut launchd = FakeLaunchd::new(&db);
    launchd.registers_through_service = true;
    let processes = FakeProcesses::default();
    let shot = on_socket();
    let refused = refuse_opening(&db);
    let up = shot
        .up(
            &fixture.location,
            &fixture.repo,
            &launchd,
            &processes,
            &fixture.environment,
            &fixture.options,
        )
        .unwrap();
    assert_eq!(up["supervisor"]["outcome"], "started", "{up}");
    let pid = std::process::id();
    let report = std::thread::scope(|scope| {
        // The drain's end: the service stops first, and only then is the
        // supervisor's process seen ended.
        let drain = scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            stop.stop();
            std::thread::sleep(Duration::from_millis(300));
            end(&processes, pid);
        });
        let report = shot.down(
            &fixture.location,
            &launchd,
            &processes,
            &DownOptions {
                wait: true,
                force: false,
                poll: Duration::from_millis(20),
            },
        );
        let _waiting = common::within(common::STEP_LIMIT, "the drain thread to return");
        drain.join().unwrap();
        report
    })
    .unwrap();
    drop(refused);
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(
        report["queue_service"]["outcome"], "not_running",
        "{report}"
    );
}

/// The supervisor's process is gone.
#[test]
fn down_wait_takes_the_service_the_drain_stopped_for_the_drain_s_end() {
    down_wait_after_the_drain_stopped_the_service(|processes, pid| {
        processes.dead.lock().unwrap().insert(pid);
    });
}

/// The supervisor's pid lives on as another user's process (absent from
/// this user's listing): the supervisor that drained is not waited for.
#[test]
fn down_wait_ends_when_the_drained_supervisor_s_pid_runs_another_process() {
    down_wait_after_the_drain_stopped_the_service(|processes, _| {
        *processes.listed.lock().unwrap() = Some(Vec::new());
    });
}

/// `up` from a repository the queue is not bound to is refused by the
/// service it starts before that service takes over or migrates: the
/// running service keeps its pid, the queue its schema, and nothing is
/// installed.
#[test]
fn up_from_another_repository_replaces_and_migrates_nothing() {
    use dagq::application::queue_service::QueueServiceControl;
    use dagq::domain::queue_service::ServiceState;
    let mut fixture = fixture();
    let _service = serve_on_up(&mut fixture);
    let db = fixture.location.db.clone();
    // A queue one migration behind, bound to another repository, whose
    // service found the migration pending: `up` would replace it.
    common::queue::queue_at_schema(&db, dagq::infrastructure::schema::MIGRATIONS.len() - 1);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO queue_repository(singleton, git_common_dir) \
             VALUES (1, '/another/repo/.git')",
            [],
        )
        .unwrap();
    let running = dagq::infrastructure::queue_service::SystemQueueService::new(
        &db,
        &fixture.environment.current_exe,
    )
    .start(Duration::from_secs(10))
    .unwrap();
    let version = common::queue::user_version(&db);
    fixture.location.git_common_dir = Some(fixture.repo.join(".git"));
    let launchd = FakeLaunchd::new(&db);
    let processes = FakeProcesses::default();
    let error = format!(
        "{:#}",
        on_socket()
            .up(
                &fixture.location,
                &fixture.repo,
                &launchd,
                &processes,
                &fixture.environment,
                &fixture.options,
            )
            .unwrap_err()
    );
    assert!(error.contains("bound to another Git repository"), "{error}");
    let found = dagq::infrastructure::queue_service::probe(db.parent().unwrap());
    assert_eq!(found.state, ServiceState::Running, "{found:?}");
    assert_eq!(found.pid, running.pid, "{found:?}");
    assert_eq!(common::queue::user_version(&db), version);
    assert!(launchd.installs.lock().unwrap().is_empty());
}

/// `up`'s handoff hands a live supervisor only the executable of the
/// process that asks for it, as the command line hands over its own: the
/// service refuses another path as `authorization_denied`, recorded, and
/// leaves the registration without a handoff, and asks with its own.
#[test]
fn up_s_handoff_names_only_its_own_executable() {
    use common::service::{call, queue, start};
    let queue = queue();
    let registration = LeaseToken::new("live-supervisor");
    SqliteQueue::open(&queue.db)
        .unwrap()
        .register_supervisor(&registration, std::process::id(), 1, dagq::VERSION)
        .unwrap();
    start(&queue);
    let handoff = |binary: &str| {
        serde_json::to_value(ControlParams {
            control: ControlCall {
                command: ControlCommand::Up,
                task: None,
                yes: false,
                pid: std::process::id(),
            },
            call: serde_json::from_value(
                json!({"op": "request_handoff", "args": [registration, binary]}),
            )
            .unwrap(),
        })
        .unwrap()
    };
    let before = common::service::events(&queue.db, "authorization_denied").len();
    let other = call(&queue, None, UseCase::Control, handoff("/bin/sh"));
    assert_eq!(other["error"]["code"], "authorization_denied", "{other}");
    assert_eq!(
        common::service::events(&queue.db, "authorization_denied").len(),
        before + 1
    );
    let unhanded = SqliteQueue::open(&queue.db).unwrap().supervisors().unwrap();
    assert_eq!(unhanded[0].handoff_binary, None);
    let own = std::env::current_exe().unwrap();
    let answer = call(
        &queue,
        None,
        UseCase::Control,
        handoff(own.to_str().unwrap()),
    );
    // Asked: a registration of no running supervisor takes none.
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(
        common::service::events(&queue.db, "authorization_denied").len(),
        before + 1
    );
}
