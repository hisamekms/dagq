//! Runtime tests: the stalled detections of a run taken out of its session
//! by `recover` or the supervisor's abandon end with it (ADR-0047
//! decisions 30 and 32): its `stalled` ask closes and each detection not
//! ended gets one `stall_resolved`, of the outcome its events tell (task
//! 799), else `run_ended`.
use crate::runtime_support;
use dagq::domain::EventKind;

use dagq::domain::LeaseToken;
use dagq::infrastructure::runtime_store::{STALL_ABANDONED_CLOSED, STALL_RECOVERED_CLOSED};
use runtime_support::*;

/// A `running` run whose supervisor and session are gone (dead pids, stale
/// lease), with no workspace cmux lists.
fn dead_run(repo: &Path, db: &Path) -> TaskRun {
    let run = orphan_run(repo, db, "dead-supervisor", dead_pid(), dead_pid());
    Connection::open(db)
        .unwrap()
        .execute("UPDATE run_leases SET heartbeat_at=0, pid=?1", [dead_pid()])
        .unwrap();
    run
}

/// Record the receipt-less idle the way the supervisor's watch does: a
/// nudge that went on to a recovery job (its end recorded), the job that
/// could not start (its end not recorded), and the `stalled` ask it
/// escalated to.
fn stall(queue: &mut SqliteQueue, run: &TaskRun) -> AskId {
    let idle = json!({"phase": "session", "idle_secs": 1250, "threshold_secs": 1200});
    queue
        .record_runtime_event(run.id(), EventKind::StallNudged, idle)
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::RecoveryRequested,
            json!({"alert": "stalled", "reason": "idle_without_receipt", "attempt": 1,
                   "idle_secs": 1300, "threshold_secs": 1200}),
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::StallResolved,
            json!({"phase": "session", "detection": "nudge", "outcome": "escalated"}),
        )
        .unwrap();
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "the session is idle without a receipt".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue
        .record_runtime_event(
            run.id(),
            EventKind::RecoveryFinished,
            json!({"alert": "stalled", "reason": "idle_without_receipt", "attempt": 1,
                   "ask_id": ask.id, "escalated": true, "outcome": "job_failed"}),
        )
        .unwrap();
    ask.id
}

/// The `(detection, outcome)` of each `stall_resolved` of the run.
fn resolved(db: &Path, run: &TaskRun) -> Vec<(String, String)> {
    events_of(db, run.id(), "stall_resolved")
        .into_iter()
        .map(|p| {
            (
                p["detection"].as_str().unwrap().to_owned(),
                p["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// `dagq recover` of a run idle without a receipt whose `stalled` ask is
/// open, with no workspace left in cmux: the runtime answers and closes the
/// ask, the job that could not start gets one `escalated` (its escalation
/// is recorded) and the ask one `run_ended` (the nudge ended already). A
/// sweep that follows adds none.
#[test]
fn recover_closes_the_stalled_ask_and_ends_its_detections_once() {
    let (_dir, repo, db) = fixture();
    let run = dead_run(&repo, &db);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = stall(&mut queue, &run);

    let outcome = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(outcome["run"]["status"], "interrupted");
    let closed = queue.read_ask(ask).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some(STALL_RECOVERED_CLOSED));
    let answered = events_of(&db, run.id(), "ask_answered");
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["runtime_closed"], true);
    let ends = events_of(&db, run.id(), "stall_resolved");
    assert_eq!(
        resolved(&db, &run),
        [
            ("nudge".to_owned(), "escalated".to_owned()),
            ("recovery".to_owned(), "escalated".to_owned()),
            ("ask".to_owned(), "run_ended".to_owned()),
        ]
    );
    assert_eq!(ends[1]["attempt"], 1);
    assert_eq!(ends[1]["threshold"], "idle_without_receipt_secs");
    assert_eq!(ends[1]["threshold_secs"], 1200);
    assert_eq!(ends[1]["detected_after_secs"], 1300);
    assert_eq!(ends[2]["ask_id"], json!(ask));
    assert_eq!(ends[2]["phase"], "session");
    assert_eq!(ends[2]["threshold_secs"], 1200);

    // The sweep (or a watch's close) after it ends nothing twice.
    queue
        .end_stalled_detections(run.id(), "the run ended; closed by the runtime")
        .unwrap();
    assert_eq!(resolved(&db, &run).len(), 3);
    // `stats` reads the ends as the run's.
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let idle = &stats["stall_thresholds"]["idle_without_receipt_secs"];
    assert_eq!(idle["detections"], 3, "{stats}");
    assert_eq!(idle["outcomes"]["escalated"], 2, "{stats}");
    assert_eq!(idle["outcomes"]["run_ended"], 1, "{stats}");
}

/// The supervisor abandons a run (a runtime error) whose nudge is not
/// ended (its supervisor died before recording it) and whose `stalled` ask
/// a person answered before the supervisor applied it: the ask closes with
/// the person's answer, the nudge gets one `escalated` (the ask followed
/// it) and the ask one `answered_intervene`, counted so by `stats`. The
/// watch's end at the session's exit that follows adds none.
#[test]
fn an_abandoned_run_closes_its_answered_stalled_ask_and_ends_its_nudge() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "live-supervisor", dead_pid(), dead_pid());
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::StallNudged,
            json!({"phase": "session", "idle_secs": 61, "threshold_secs": 60}),
        )
        .unwrap();
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "the session is idle without a receipt".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(ask.id, "intervene").unwrap();

    queue
        .abandon_run(
            run.id(),
            &LeaseToken::new("live-supervisor"),
            "the workspace backend failed",
            &ReasonCode::Other.into(),
            None,
        )
        .unwrap();
    let closed = queue.read_ask(ask.id).unwrap();
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.answer.as_deref(), Some("intervene"));
    assert_eq!(events_of(&db, run.id(), "ask_closed").len(), 1);
    assert_eq!(
        resolved(&db, &run),
        [
            ("nudge".to_owned(), "escalated".to_owned()),
            ("ask".to_owned(), "answered_intervene".to_owned()),
        ]
    );
    let ends = events_of(&db, run.id(), "stall_resolved");
    assert_eq!(ends[0]["threshold_secs"], 60);
    assert_eq!(ends[0]["detected_after_secs"], 61);
    assert_eq!(ends[1]["ask_id"], json!(ask.id));
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    let idle = &stats["stall_thresholds"]["idle_without_receipt_secs"];
    assert_eq!(idle["detections"], 2, "{stats}");
    assert_eq!(idle["outcomes"]["answered_intervene"], 1, "{stats}");
    assert_eq!(idle["outcomes"]["escalated"], 1, "{stats}");
    assert_eq!(
        idle["by_detection"]["ask"]["outcomes"]["answered_intervene"], 1,
        "{stats}"
    );

    // An ask left open (its answer never came) is answered by the runtime.
    let other = queue
        .ask(NewAsk {
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "still idle".into(),
            options: vec!["wait".into(), "intervene".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    queue
        .abandon_run(
            run.id(),
            &LeaseToken::new("live-supervisor"),
            "the workspace backend failed again",
            &ReasonCode::Other.into(),
            None,
        )
        .unwrap();
    let other = queue.read_ask(other.id).unwrap();
    assert_eq!(other.answer.as_deref(), Some(STALL_ABANDONED_CLOSED));
    // The runtime's close is no person's answer: it ended with the run.
    assert_eq!(
        resolved(&db, &run)[2],
        ("ask".to_owned(), "run_ended".to_owned())
    );
    let answered = events_of(&db, run.id(), "stall_resolved")
        .iter()
        .filter(|end| end["outcome"] == "answered_intervene")
        .count();
    assert_eq!(answered, 1);
    // Once more, nothing is left to end.
    queue
        .end_stalled_detections(run.id(), "the run ended; closed by the runtime")
        .unwrap();
    assert_eq!(resolved(&db, &run).len(), 3);
}
