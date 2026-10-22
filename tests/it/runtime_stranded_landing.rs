//! Runtime tests: a run left awaiting integration with no lease and no
//! open ask after its review passed is taken up again by one supervisor
//! and lands without a second review (ADR-t3049-1). Which runs are taken
//! up, and the phase and progress they show meanwhile, are decided in
//! `dagq::domain::stranded_landing`, `run_phase` and `run_progress` (their
//! unit tests); this test keeps SQLite's transaction, Git and the
//! supervisor's wiring.
use crate::runtime_support;
use dagq::domain::{EventKind, LeaseToken};
use std::sync::{Arc, Barrier};

use runtime_support::*;

/// The agent of task `n`: a change of its own, so that one task's landing
/// does not leave the other's conflicting, and its receipt.
fn own_change(n: i64) -> String {
    format!(
        "printf '{n}\\n' > f{n}.txt && git add f{n}.txt && git commit -q -m t{n}; receipt \"$(git rev-parse HEAD)\""
    )
}

/// A later review of `run` passed, its session left, and it was queued to
/// land, as before a supervisor that stopped gave its lease back.
fn passed_and_queued(queue: &SqliteQueue, run: &TaskRun) {
    for (kind, payload) in [
        (
            EventKind::ReviewFinished,
            json!({"attempt": 2, "verdict": "pass", "reasons": [], "summary": "reviewed again"}),
        ),
        (EventKind::ExitRequested, json!({})),
        (EventKind::LandingQueued, json!({"via": "exit"})),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
}

/// The `lease_acquired` of the take-ups of `task`'s run.
fn taken_up(queue: &mut SqliteQueue, task: i64) -> usize {
    payloads(&queue.show(TaskId::new(task)).unwrap(), "lease_acquired")
        .into_iter()
        .filter(|p| p["reason"] == "stranded_landing")
        .count()
}

/// Boundary (ADR-t3049-1): the lease of a run left awaiting integration is
/// taken in one transaction that judges the run again as it stands, so an
/// open ask refuses it, of two supervisors that judged the same run at
/// once exactly one takes it, and one that judged a run before another's
/// landing held it for a person (`integration_held`) and gave it back
/// takes nothing. Given back again after its review passed, the run is
/// taken up by the next supervisor's pass and lands without a second
/// review, and the held one stays a person's.
#[test]
fn a_run_left_awaiting_integration_is_taken_up_by_one_supervisor_and_lands() {
    let (_fixture, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    backend.script_for(1, &own_change(1));
    backend.script_for(2, &own_change(2));
    // Their first reviews raised a concern: each run rests with its ask open.
    let reviewer = TestReviewer::new(&[verdict("concern", &[], "concern")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let held = queue.show(TaskId::new(2)).unwrap().runs[0].clone();
    for run in [&run, &held] {
        assert_eq!(run.status(), RunStatus::AwaitingIntegration);
        assert!(queue.run_lease(run.id()).unwrap().is_none());
        passed_and_queued(&queue, run);
    }
    let asks = queue.asks(Default::default()).unwrap();
    assert!(!asks.is_empty());
    let mine = LeaseToken::new("supervisor-a");
    let theirs = LeaseToken::new("supervisor-b");
    assert!(
        queue
            .lease_stranded_landing(run.id(), &mine)
            .unwrap()
            .is_none()
    );
    for ask in asks {
        queue.answer(ask.id, "withdrawn").unwrap();
        queue.close_ask(ask.id).unwrap();
    }
    // Two supervisors race for it: one takes it, the other finds its lease.
    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = ["supervisor-a", "supervisor-b"]
        .into_iter()
        .map(|token| {
            let (db, run, barrier) = (db.clone(), run.id().clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut queue = SqliteQueue::open(&db).unwrap();
                {
                    let _waiting = crate::common::within(
                        crate::common::STEP_LIMIT,
                        format!("{token} to meet the other racer"),
                    );
                    barrier.wait();
                }
                queue
                    .lease_stranded_landing(&run, &LeaseToken::new(token))
                    .unwrap()
                    .map(|_| token)
            })
        })
        .collect();
    let winners: Vec<_> = racers
        .into_iter()
        .filter_map(|racer| joined(racer, "a racer for the run's lease to return"))
        .collect();
    assert_eq!(winners.len(), 1, "{winners:?}");
    let winner = LeaseToken::new(winners[0]);
    assert_eq!(queue.run_lease(run.id()).unwrap().unwrap().token, winner);
    assert_eq!(taken_up(&mut queue, 1), 1);
    queue.release_lease(run.id(), &winner).unwrap();
    // The other run: one supervisor takes it and its landing is held for a
    // person; a supervisor that judged it before then takes nothing.
    assert!(
        queue
            .lease_stranded_landing(held.id(), &theirs)
            .unwrap()
            .is_some()
    );
    queue
        .record_runtime_event(
            held.id(),
            EventKind::IntegrationHeld,
            json!({"status": "awaiting_integration"}),
        )
        .unwrap();
    queue.release_lease(held.id(), &theirs).unwrap();
    assert!(
        queue
            .lease_stranded_landing(held.id(), &mine)
            .unwrap()
            .is_none()
    );
    let status = runtime::status(&db).unwrap();
    let entry = run_attention_of(&status, run.id()).unwrap_or_else(|| panic!("{status}"));
    assert_eq!(entry["next"], "queued to land (runtime)", "{status}");
    let entry = run_attention_of(&status, held.id()).unwrap_or_else(|| panic!("{status}"));
    assert_ne!(entry["next"], "queued to land (runtime)", "{status}");
    let reviews = payloads(&queue.show(TaskId::new(1)).unwrap(), "review_started").len();
    let outcome = supervise_reviewed(&db, &repo, &backend, &TestReviewer::new(&[]));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(payloads(&detail, "review_started").len(), reviews);
    assert_eq!(taken_up(&mut queue, 1), 2);
    // The held run stays awaiting integration for a person, nobody's.
    assert_eq!(
        queue.run(held.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert!(queue.run_lease(held.id()).unwrap().is_none());
    assert_eq!(taken_up(&mut queue, 2), 1);
}
