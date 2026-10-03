//! Runtime tests: the headless review and recovery jobs of a run start as
//! their own actors, each with its role and actor id in the environment
//! (ADR-t728-1 decisions 2 and 4).
use crate::runtime_support;

use runtime_support::*;

/// A job script that appends its role and actor id to `log`, then runs
/// `script`.
fn logging(log: &Path, script: &str) -> String {
    format!(
        "printf '%s %s\\n' \"$DAGQ_ROLE\" \"$DAGQ_ACTOR_ID\" >> '{}'; {script}",
        log.display()
    )
}

#[test]
fn the_review_and_the_recovery_job_run_as_their_own_actors() {
    headless_workers();
    let (_dir, repo, db) = fixture();
    let log = db.parent().unwrap().join("job-env.txt");
    let mark = db.parent().unwrap().join("failed-once");
    let worker = format!(
        "if [ ! -f '{mark}' ]; then : > '{mark}'; exit 7; fi; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
        mark = mark.display()
    );
    let backend = TestWorkspace::new(&db, false, &worker);
    let reviewer =
        TestReviewer::new(&[logging(&log, &verdict("pass", &[], "meets the acceptance"))])
            .with_triages(&[logging(
                &log,
                &repair(json!({"action": "retry"}), "the session died on its own"),
            )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let (failed, landed) = (&detail.runs[0], &detail.runs[1]);
    assert_eq!(landed.status(), RunStatus::Integrated);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!(
            "recovery-job recovery-job:{}:failed:1\nreview-job review-job:{}:1\n",
            failed.id(),
            landed.id()
        )
    );
    // Each run's worker is its own actor with its run.
    let tags = backend.tags.lock().unwrap();
    for (tags, run) in tags.iter().zip([failed, landed]) {
        let env = |name: &str| {
            tags.env
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(env("DAGQ_ROLE").as_deref(), Some("worker"));
        assert_eq!(env("DAGQ_ACTOR_ID"), Some(format!("worker:{}", run.id())));
        assert_eq!(env("DAGQ_RUN_ID"), Some(run.id().to_string()));
        assert_eq!(env("DAGQ_TASK_ID").as_deref(), Some("1"));
    }
    drop(tags);

    // The events record their actor (task 730): the supervisor's own
    // transitions as the supervisor, the verdicts it
    // applied with the job that returned them as the requester.
    let supervisor = format!("supervisor:{}", std::process::id());
    let actor = |kind: &str, run: &TaskRun| {
        let event = detail
            .events
            .iter()
            .find(|e| e.kind == kind && e.run_id.as_ref() == Some(run.id()))
            .unwrap_or_else(|| panic!("no {kind} of {}", run.id()));
        let actor = event.actor.clone().expect("an actor");
        (actor.role, actor.id, actor.requested_by)
    };
    for (kind, run) in [("run_claimed", failed), ("run_claimed", landed)] {
        assert_eq!(
            actor(kind, run),
            ("supervisor".to_owned(), supervisor.clone(), None),
            "{kind}"
        );
    }
    // The landing is the Integrator's at the supervisor's request
    // (ADR-t728-2).
    assert_eq!(
        actor("run_integrated", landed),
        (
            "integrator".to_owned(),
            format!("integrator:{}", std::process::id()),
            Some(supervisor.clone())
        )
    );
    assert_eq!(
        actor("recovery_finished", failed),
        (
            "supervisor".to_owned(),
            supervisor.clone(),
            Some(format!("recovery-job:{}:failed:1", failed.id()))
        )
    );
    // The session wrapper's events are the wrapper's, not the worker's.
    assert_eq!(
        actor("wrapper_started", landed),
        (
            "wrapper".to_owned(),
            format!("wrapper:{}", landed.id()),
            None
        )
    );
    assert!(
        detail.events.iter().all(|e| e.actor.is_some()),
        "{:?}",
        detail.events
    );
    assert_eq!(
        actor("review_finished", landed),
        (
            "supervisor".to_owned(),
            supervisor.clone(),
            Some(format!("review-job:{}:1", landed.id()))
        )
    );
}
