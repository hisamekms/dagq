//! The telemetry token of a target of no run on the host (ADR-t1662-3
//! decisions 10 to 12, docs/design/measurement.md "送る口"): the runtime's
//! planner, the plan review, the goal review, the eval job, the
//! observation and the throughput review. Each is issued its token as its
//! agent starts; the queue service process records what is sent with it
//! in the SessionStepStore under that target's kind and id, never on a run
//! or a task; and once the queue holds the target's end, or the process
//! that ran it is gone, the service refuses the token, revoked yet or not,
//! and the sweep revokes it, each revocation recorded on the queue only
//! once it succeeded.

use crate::common;
use common::service::{events, queue, start};

use std::{fs, path::Path, process::Command};

use dagq::{
    application::{
        LedgerStore, SessionOwners, SessionStepStore, TelemetrySink,
        queue_service::TelemetryTokens,
        telemetry_tokens::{TargetTelemetry, sweep_targets},
    },
    domain::{
        EventKind, LeaseToken,
        ledger::LedgerKind,
        session_step::{Step, StepKind, StepTarget, TargetKind},
        telemetry::TelemetryReport,
    },
    infrastructure::{
        queue_service::{self as service, ServiceTelemetrySink, SystemServiceAccess},
        sqlite::SqliteQueue,
    },
};
use serde_json::json;

/// The supervisor that runs the jobs, live, and one that fell.
const LIVE: &str = "live";
const FELL: &str = "fell";

/// One target of each kind of no run in the queue at `db`, each still
/// running: an open planner, a goal review and an eval job of the live
/// supervisor, an observation and a throughput review whose start names
/// this process and an hour's timeout, and last a plan review of the
/// supervisor that fell (its registration gone with it).
fn targets(db: &Path) -> Vec<StepTarget> {
    let queue = SqliteQueue::open(db).unwrap();
    let planner = queue
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap();
    let sql = rusqlite::Connection::open(db).unwrap();
    sql.execute_batch(&format!(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO supervisors(token, pid, parallel, heartbeat_at)
             VALUES ('{LIVE}', {}, 1, unixepoch());
         INSERT INTO goal_reviews(goal_id, attempt, supervisor_token, fingerprint, started_at)
             VALUES (1, 1, '{LIVE}', 'f', 1);
         INSERT INTO headless_jobs(kind, label, attempt, pid, supervisor_token, started_at, provider)
             VALUES ('agent_eval', 'a:e1:c1:0', 0, 1, '{LIVE}', 1, 'claude');
         INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at, finished_at, outcome)
             VALUES (2, 1, '{LIVE}', 1, 2, 'succeeded');
         INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at)
             VALUES (1, 1, '{FELL}', 1);",
        std::process::id()
    ))
    .unwrap();
    let start = |kind, dir: &str| {
        queue
            .record_queue_event(
                kind,
                json!({"mode": "hourly", "period": "p", "dir": dir,
                    "pid": std::process::id(), "timeout_secs": 3600}),
            )
            .unwrap()
            .as_i64()
    };
    let observer = start(EventKind::ObserveStarted, "/q/observer/1");
    let review = start(EventKind::ThroughputReviewStarted, "/q/reviews/hourly-p");
    vec![
        StepTarget::of(TargetKind::Planner, planner.id.as_i64()),
        StepTarget::of(TargetKind::GoalReview, 1),
        StepTarget::of(TargetKind::EvalJob, 1),
        StepTarget::of(TargetKind::Observer, observer),
        StepTarget::of(TargetKind::ThroughputReview, review),
        StepTarget::of(TargetKind::PlanReview, 2),
    ]
}

/// The live supervisor beats now, so its jobs hold however long the test
/// took to get here.
fn beat(db: &Path) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            &format!("UPDATE supervisors SET heartbeat_at = unixepoch() WHERE token = '{LIVE}'"),
            [],
        )
        .unwrap();
}

/// The control side's issue and revocation, from a command (no lease).
fn telemetry(db: &Path) -> TargetTelemetry<'_> {
    TargetTelemetry {
        tokens: &SystemServiceAccess,
        db,
        lease: None,
    }
}

/// The session of `target`'s executor: `<kind>-<id>`.
fn session(target: &StepTarget) -> String {
    format!("{}-{}", target.kind.as_str(), target.id)
}

/// Send one step of `target`'s session as the token in `file` (copied to a
/// file of the test's, as an executor that read it earlier holds it); the
/// error when it is refused.
fn send(dir: &Path, token: &str, target: &StepTarget) -> Option<String> {
    let file = dir.join(format!("held-{}", session(target)));
    fs::write(&file, token).unwrap();
    ServiceTelemetrySink::new(service::socket_path(dir), file)
        .report(&TelemetryReport::new(
            target.clone(),
            session(target),
            vec![Step::new(1, StepKind::ModelTurn, 0, 5)],
        ))
        .err()
        .map(|error| format!("{error:#}"))
}

/// The reason of the latest request the service refused.
fn last_refusal(db: &Path) -> String {
    events(db, "queue_service_unauthenticated").last().unwrap()["payload"]["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The queue's `telemetry_token_*` events of `target`, as kind and reason.
fn token_events(db: &Path, target: &StepTarget) -> Vec<(String, Option<String>)> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.kind.is_in(&[
                EventKind::TelemetryTokenIssued,
                EventKind::TelemetryTokenRevoked,
            ]) && event.payload["target"] == json!(target)
                && event.run_id.is_none()
        })
        .map(|event| {
            (
                event.kind.as_str().to_owned(),
                event.payload["reason"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// A pid no process has any more.
fn gone_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Each target of no run (a planner, a plan review and a goal review of the
/// live supervisor, an eval job, an observation and a throughput review) is
/// issued its own token, its file under the queue's `service/`; what is
/// sent with it is kept under its target's kind and id, for its session the
/// control side registered to it, and on no run: the queue's run keeps no
/// step, no event of it and no ledger row.
#[test]
fn each_target_of_no_run_is_issued_a_token_whose_steps_are_kept_by_its_target_alone() {
    let queue = queue();
    start(&queue);
    let (db, dir) = (queue.db.as_path(), queue.dir());
    let store = SqliteQueue::open(db).unwrap();
    let run_events = store.run_events(queue.run.id()).unwrap().len();
    let mut targets = targets(db);
    // The plan review of the supervisor that fell gives way to one of the
    // live supervisor's (one plan review runs at a time).
    rusqlite::Connection::open(db)
        .unwrap()
        .execute_batch(&format!(
            "PRAGMA foreign_keys = OFF;
             UPDATE plan_reviews SET finished_at = 2, outcome = 'interrupted' WHERE id = 2;
             INSERT INTO plan_reviews(proposal_id, attempt, supervisor_token, started_at)
                 VALUES (3, 1, '{LIVE}', 1);"
        ))
        .unwrap();
    targets[5] = StepTarget::of(TargetKind::PlanReview, 3);
    let mut files = Vec::new();
    for target in &targets {
        beat(db);
        let file = telemetry(db).issue(&store, target).unwrap();
        assert!(file.starts_with(service::service_dir(dir)), "{file:?}");
        assert!(!files.contains(&file), "{file:?}");
        store.register_session(target, &session(target), 1).unwrap();
        let token = service::read_token(&file).unwrap();
        assert_eq!(send(dir, &token, target), None, "{target:?}");
        // Another target's name, with this token, is refused.
        let other =
            &targets[(targets.iter().position(|t| t == target).unwrap() + 1) % targets.len()];
        assert!(send(dir, &token, other).is_some(), "{target:?}");
        files.push(file);
    }
    let kinds: Vec<TargetKind> = targets.iter().map(|target| target.kind).collect();
    assert_eq!(
        kinds,
        [
            TargetKind::Planner,
            TargetKind::GoalReview,
            TargetKind::EvalJob,
            TargetKind::Observer,
            TargetKind::ThroughputReview,
            TargetKind::PlanReview,
        ]
    );
    for target in &targets {
        let steps = store.target_steps(target).unwrap();
        assert_eq!(steps.len(), 1, "{target:?}");
        assert_eq!(steps[0].target, *target);
        assert_eq!(steps[0].session_id, session(target));
        assert_eq!(
            token_events(db, target),
            [("telemetry_token_issued".to_owned(), None)]
        );
    }
    assert_eq!(
        store
            .target_steps(&StepTarget::run(queue.run.id()))
            .unwrap(),
        []
    );
    assert_eq!(store.run_events(queue.run.id()).unwrap().len(), run_events);
    for kind in [LedgerKind::Run, LedgerKind::Task] {
        assert_eq!(store.ledger_rows(kind).unwrap(), [], "{kind:?}");
    }
}

/// Once the queue holds a target's end (the planner closed, the goal
/// review's row finished, the eval job's row ended, the throughput review's
/// finish recorded) or the process that ran it is gone (the plan review's
/// supervisor, which fell without a word to the queue), the service
/// process refuses its token at once (`target_ended`), though no one
/// revoked it; the observation whose command was killed is refused only
/// once the sweep, which asks the host, revoked it. The sweep revokes every
/// one of them, recorded on the queue with its reason only once it
/// succeeded, and leaves a target still running; then the service no longer
/// knows those tokens (`unknown_token`).
#[test]
fn a_targets_token_is_refused_once_it_ended_or_its_runner_fell_and_the_sweep_revokes_it() {
    let queue = queue();
    start(&queue);
    let (db, dir) = (queue.db.as_path(), queue.dir());
    let store = SqliteQueue::open(db).unwrap();
    let targets = targets(db);
    let tokens: Vec<String> = targets
        .iter()
        .map(|target| {
            store.register_session(target, &session(target), 1).unwrap();
            service::read_token(&telemetry(db).issue(&store, target).unwrap()).unwrap()
        })
        .collect();
    let [planner, goal_review, eval, observer, review, plan_review] = &targets[..] else {
        unreachable!()
    };
    beat(db);
    // The plan review's supervisor fell: its token is refused already.
    assert!(send(dir, &tokens[5], plan_review).is_some());
    assert_eq!(last_refusal(db), "target_ended");
    for (target, token) in targets.iter().zip(&tokens).take(5) {
        assert_eq!(send(dir, token, target), None, "{target:?}");
    }

    store
        .close_planner(
            dagq::domain::PlannerId::new(planner.id.parse().unwrap()),
            None,
        )
        .unwrap();
    let sql = rusqlite::Connection::open(db).unwrap();
    sql.execute_batch(&format!(
        "UPDATE goal_reviews SET finished_at = 3, outcome = 'succeeded' WHERE id = {};
         UPDATE headless_jobs SET ended_at = 3, outcome = 'ended' WHERE id = {};
         UPDATE run_events SET payload = json_set(payload, '$.pid', {})
             WHERE id = {};",
        goal_review.id,
        eval.id,
        gone_pid(),
        observer.id
    ))
    .unwrap();
    store
        .record_queue_event(
            EventKind::ThroughputReviewFinished,
            json!({"mode": "hourly", "period": "p", "dir": "/q/reviews/hourly-p",
                "started_event_id": review.id.parse::<i64>().unwrap()}),
        )
        .unwrap();
    for (target, token) in [planner, goal_review, eval, review]
        .into_iter()
        .zip([&tokens[0], &tokens[1], &tokens[2], &tokens[4]])
    {
        assert!(send(dir, token, target).is_some(), "{target:?}");
        assert_eq!(last_refusal(db), "target_ended", "{target:?}");
    }
    // The service reads only the queue: the killed command's start says
    // nothing yet.
    assert_eq!(send(dir, &tokens[3], observer), None);

    // A goal review still running is left alone.
    let running = StepTarget::of(TargetKind::GoalReview, 2);
    sql.execute_batch(&format!(
        "INSERT INTO goal_reviews(goal_id, attempt, supervisor_token, fingerprint, started_at)
             VALUES (2, 1, '{LIVE}', 'g', 1);"
    ))
    .unwrap();
    let kept = service::read_token(&telemetry(db).issue(&store, &running).unwrap()).unwrap();
    store
        .register_session(&running, &session(&running), 1)
        .unwrap();
    beat(db);
    let alive = |pid| dagq::infrastructure::adapters::process_alive(pid);
    let mut revoked = sweep_targets(
        &SystemServiceAccess,
        &store,
        db,
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap(),
        &alive,
    );
    revoked.sort();
    let mut expected = targets.clone();
    expected.sort();
    assert_eq!(revoked, expected);
    for (target, token) in targets.iter().zip(&tokens) {
        assert!(send(dir, token, target).is_some(), "{target:?}");
        assert_eq!(last_refusal(db), "unknown_token", "{target:?}");
        assert_eq!(SystemServiceAccess.telemetry_credential(db, target), None);
        let reason = if target == planner {
            "planner_closed"
        } else {
            "job_ended"
        };
        assert_eq!(
            token_events(db, target),
            [
                ("telemetry_token_issued".to_owned(), None),
                (
                    "telemetry_token_revoked".to_owned(),
                    Some(reason.to_owned())
                ),
            ],
            "{target:?}"
        );
    }
    beat(db);
    assert_eq!(send(dir, &kept, &running), None);
    assert_eq!(
        SystemServiceAccess
            .held_telemetry(db)
            .unwrap()
            .into_iter()
            .map(|held| (held.target, held.issued_under))
            .collect::<Vec<_>>(),
        [(running, None::<LeaseToken>)]
    );
}
