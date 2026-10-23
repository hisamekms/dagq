//! Runtime tests: the supervisor registers the sessions of the runs it
//! starts to their run (ADR-t1662-3 decision 10, docs/design/measurement.md
//! "送る口"), so that the queue service process records what their
//! executor sends with the run's telemetry token in the SessionStepStore,
//! with no registration of the test's. The claim's session is registered
//! before its wrapper starts; a thread its agent named itself (a Codex
//! turn's) and a new session of the run (a renewal's) once its events name
//! them, at the next pass: what the executor sent of them before is
//! refused, and sent again, recorded.
use crate::runtime_support;

use runtime_support::*;

use dagq::{
    application::{SessionStepStore, TelemetrySink, queue_service::TelemetryTokens},
    domain::{
        EventKind,
        session_step::{Step, StepKind, StepTarget},
        telemetry::TelemetryReport,
        turn::session_name,
    },
    infrastructure::queue_service::{self as service, ServiceTelemetrySink, SystemServiceAccess},
};

/// One step of `session` of the run's `target`.
fn report(target: &StepTarget, session: &str) -> TelemetryReport {
    TelemetryReport::new(
        target.clone(),
        session,
        vec![Step::new(1, StepKind::ModelTurn, 0, 5)],
    )
}

/// A supervisor (not `--once`) that claimed task 1 of the queue at `db`,
/// whose worker waits, and the run's executor's sink, with the run's
/// telemetry token as the executor holds it. `before` runs on the queue
/// before the supervisor starts.
struct Claimed {
    supervisor: thread::JoinHandle<Result<Value>>,
    run: TaskRun,
    target: StepTarget,
    sink: ServiceTelemetrySink,
    _served: crate::common::service::Served,
}

fn claimed(db: &Path, repo: &Path) -> Claimed {
    let queue_dir = db.parent().unwrap().to_owned();
    let started = queue_dir.join("worker-started");
    let worker = format!(
        ": > {started}; await_file \"$EXIT.go\"; {VALID_AGENT}",
        started = shell_path(&started)
    );
    let backend = TestWorkspace::new(db, false, &worker);
    let served = crate::common::service::Served::start(db);
    let options = SuperviseOptions {
        heartbeat_interval: Duration::from_millis(100),
        ..supervise_options(1, false)
    };
    let supervisor = {
        let (db, repo) = (db.to_path_buf(), repo.to_path_buf());
        thread::spawn(move || {
            runtime::supervise(
                &db,
                &repo,
                &backend,
                &claude_stub(&db),
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    headless::wait_for_file(&started);
    let run = SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap()
        .runs[0]
        .clone();
    let target = StepTarget::run(run.id());
    let credential = SystemServiceAccess
        .telemetry_credential(db, &target)
        .expect("the claim issued the run's telemetry token");
    let sent = queue_dir.join("held-token");
    fs::write(&sent, service::read_token(&credential).unwrap()).unwrap();
    let sink = ServiceTelemetrySink::new(service::socket_path(&queue_dir), sent);
    Claimed {
        supervisor,
        run,
        target,
        sink,
        _served: served,
    }
}

impl Claimed {
    /// The run's telemetry target as [`crate::common::service::session_owner`] names it.
    fn owner(&self) -> Option<String> {
        Some(format!("run:{}", self.run.id()))
    }

    /// Wait until `session` is registered.
    fn registered(&self, db: &Path, session: &str) {
        wait_until(db, crate::common::STEP_LIMIT, |_| {
            crate::common::service::session_owner(db, session).is_some()
        });
        assert_eq!(
            crate::common::service::session_owner(db, session),
            self.owner()
        );
    }

    /// The service refuses `session`'s report.
    fn refused(&self, session: &str) {
        let refused = format!(
            "{:#}",
            self.sink
                .report(&report(&self.target, session))
                .unwrap_err()
        );
        assert!(refused.contains("authorization_denied"), "{refused}");
    }

    /// End the loop by a heartbeat that fails, as the worker waits on.
    fn end(self, db: &Path) {
        rusqlite::Connection::open(db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_heartbeat BEFORE UPDATE ON supervisors
                 BEGIN SELECT RAISE(ABORT, 'heartbeat refused by the test'); END;",
            )
            .unwrap();
        let _waiting =
            crate::common::within(crate::common::STEP_LIMIT, "the supervisor thread to return");
        let ended = format!("{:#}", self.supervisor.join().unwrap().unwrap_err());
        assert!(ended.contains("heartbeat failed"), "{ended}");
    }
}

/// The claim's session, named by the run, is the run's before its wrapper
/// started, and its report goes in. A thread a Codex turn names and the
/// run's next session (a renewal's) are refused until a pass reads them
/// in the run's events; sent again then, they are recorded, each step in
/// the SessionStepStore under the run.
#[test]
fn a_runs_sessions_are_registered_to_it_and_their_reports_go_into_the_step_store() {
    let (_dir, repo, db) = fixture();
    let claimed = claimed(&db, &repo);
    let (run, target) = (claimed.run.clone(), claimed.target.clone());
    let first = run.id().to_string();
    assert_eq!(
        crate::common::service::session_owner(&db, &first),
        claimed.owner()
    );
    assert_eq!(
        claimed
            .sink
            .report(&report(&target, &first))
            .unwrap()
            .steps
            .added,
        1
    );

    let renewed = session_name(run.id().as_str(), 1);
    for session in ["thread-1", renewed.as_str()] {
        claimed.refused(session);
    }
    let queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::TurnSessionIdentified,
            json!({"turn": 1, "session_id": "thread-1", "provider": "codex"}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::SessionRenewed,
            json!({"session": renewed}),
        )
        .unwrap();
    for session in ["thread-1", renewed.as_str()] {
        claimed.registered(&db, session);
        assert_eq!(
            claimed
                .sink
                .report(&report(&target, session))
                .unwrap()
                .steps
                .added,
            1
        );
    }
    let mut kept: Vec<String> = queue
        .target_steps(&target)
        .unwrap()
        .into_iter()
        .map(|step| step.session_id)
        .collect();
    kept.sort();
    let mut expected = vec![first, "thread-1".to_owned(), renewed];
    expected.sort();
    assert_eq!(kept, expected);
    claimed.end(&db);
}

/// A registration that fails changes nothing of the run's: it is claimed
/// and its worker starts with its token, and what its executor sends is
/// refused; once the queue takes registrations again, a later pass
/// registers the session, and what is sent again goes in. A session of
/// another target is not taken by the run.
#[test]
fn a_registration_that_failed_leaves_the_run_as_it_was_and_a_later_pass_makes_it() {
    let (_dir, repo, db) = fixture();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_owner BEFORE INSERT ON session_owners
             BEGIN SELECT RAISE(ABORT, 'registration refused by the test'); END;",
        )
        .unwrap();
    let claimed = claimed(&db, &repo);
    let first = claimed.run.id().to_string();
    assert_eq!(crate::common::service::session_owner(&db, &first), None);
    assert_eq!(claimed.run.status(), RunStatus::Running);
    claimed.refused(&first);
    // A planner's session, which a Codex turn of the run then names.
    let queue = SqliteQueue::open(&db).unwrap();
    let planner = queue
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_owner")
        .unwrap();
    let planners = StepTarget::of(
        dagq::domain::session_step::TargetKind::Planner,
        planner.id.as_i64(),
    );
    dagq::application::SessionOwners::register_session(&queue, &planners, "planners", 1).unwrap();
    queue
        .record_runtime_event(
            claimed.run.id(),
            EventKind::TurnSessionIdentified,
            json!({"turn": 1, "session_id": "planners", "provider": "codex"}),
        )
        .unwrap();
    claimed.registered(&db, &first);
    assert_eq!(
        claimed
            .sink
            .report(&report(&claimed.target, &first))
            .unwrap()
            .steps
            .added,
        1
    );
    assert_eq!(
        crate::common::service::session_owner(&db, "planners"),
        Some(format!("planner:{}", planner.id))
    );
    claimed.refused("planners");
    claimed.end(&db);
}

/// Acceptance of the executor's steps (ADR-t1662-3 decision 1): a headless
/// worker's run whose turn calls a tool lands as any other, and what its
/// session wrapper collected of the turn's stream (the model's turn and the
/// tool's call) is in the SessionStepStore under the run, sent with the
/// run's telemetry token, as is the landing's verification command, a
/// tool's call of a session of its own. No step keeps the command's text,
/// and the token's file is nowhere on the agent's command line.
#[test]
fn a_headless_runs_steps_go_into_the_step_store_under_the_run() {
    use runtime_support::headless::{FINISH, detail, headless_fixture};
    let (dir, repo, db, backend) = headless_fixture(&[]);
    let _served = crate::common::service::Served::start(&db);
    let tool_use = json!({"type": "assistant", "parent_tool_use_id": null,
        "message": {"id": "m1", "model": "stub", "content": [{"type": "tool_use", "id": "t1",
            "name": "Bash", "input": {"command": "cargo test --locked --test it runtime_claim::"}}]}});
    let tool_result = json!({"type": "user", "parent_tool_use_id": null,
        "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false}]}});
    set_turns(
        dir.path(),
        &format!("printf '%s\\n' '{tool_use}'; printf '%s\\n' '{tool_result}'; {FINISH}"),
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let run = detail(&db).runs[0].clone();
    let target = StepTarget::run(run.id());
    let steps = SqliteQueue::open(&db)
        .unwrap()
        .target_steps(&target)
        .unwrap();
    let of = |session: &str| -> Vec<Step> {
        steps
            .iter()
            .filter(|kept| kept.session_id == session)
            .map(|kept| kept.step.clone())
            .collect()
    };
    let run_dir = Path::new(run.run_dir().unwrap());
    // What the wrapper kept of its sessions, when its steps are not there.
    let kept_file = service::steps_path(
        db.parent().unwrap(),
        &dagq::domain::telemetry::actor_id(&target),
    );
    let kept = || {
        format!(
            "{steps:?}\nkept {}",
            fs::read_to_string(&kept_file).unwrap_or_default()
        )
    };
    let worker = of(run.id().as_str());
    let kinds: Vec<_> = worker
        .iter()
        .map(|step| (step.kind, step.tool.clone()))
        .collect();
    assert!(
        kinds.contains(&(StepKind::ToolCall, Some("Bash".to_owned()))),
        "{}",
        kept()
    );
    assert!(
        kinds.iter().any(|(kind, _)| *kind == StepKind::ModelTurn),
        "{steps:?}"
    );
    let seqs: Vec<i64> = worker.iter().map(|step| step.seq).collect();
    assert_eq!(
        seqs,
        (1..=worker.len() as i64).collect::<Vec<_>>(),
        "{steps:?}"
    );
    let bash = worker
        .iter()
        .find(|step| step.tool.as_deref() == Some("Bash"))
        .unwrap();
    // A repository that declares no toolchain's profile keeps cargo by its
    // digest alone (ADR-t1673-1).
    assert_eq!(
        bash.command_shape.as_ref().unwrap()["parts"][0]["program"],
        queue_key(&db).digest("cargo")
    );
    assert_eq!(bash.exit_code, Some(0));
    // The landing's verification is a session of the run's own.
    let verification: Vec<&Step> = steps
        .iter()
        .filter(|kept| kept.session_id != run.id().as_str())
        .map(|kept| &kept.step)
        .collect();
    assert_eq!(verification.len(), 1, "{steps:?}");
    assert_eq!(verification[0].tool.as_deref(), Some("verification"));
    assert_eq!(verification[0].kind, StepKind::ToolCall);
    assert_eq!(verification[0].exit_code, Some(0));
    assert_eq!(
        verification[0].command_shape.as_ref().unwrap()["parts"][0]["program"],
        queue_key(&db).digest("test")
    );
    let text =
        serde_json::to_string(&steps.iter().map(|kept| &kept.step).collect::<Vec<_>>()).unwrap();
    for kept_out in ["runtime_claim", "seed.txt", "token"] {
        assert!(!text.contains(kept_out), "{kept_out} in {text}");
    }
    // The agent was given no telemetry token.
    let args = fs::read_to_string(run_dir.join("stub-args.log")).unwrap();
    assert!(
        !args.contains(dagq::domain::telemetry::ACTOR_PREFIX),
        "{args}"
    );
    // Nor where its wrapper kept its steps, which are not in its run's
    // directory.
    assert!(!args.contains(&*kept_file.to_string_lossy()), "{args}");
    assert!(!run_dir.join("turns").join("steps.json").exists());
    // The supervisor's sweep removed what the wrapper kept once the run
    // ended.
    assert!(!kept_file.exists(), "{}", kept());
}
