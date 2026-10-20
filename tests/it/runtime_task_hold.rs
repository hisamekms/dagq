//! Runtime tests: the hold of a task at the store's starts and results
//! (ADR-t1879-1). The supervisor's line of claims, resumes and recovery
//! jobs is made before it starts each, so a hold that comes in between is
//! read again by the start's own transaction: the test reads the
//! candidates the fill pass reads, holds the task, and then calls the
//! start the pass would. A recovery job's verdict and a person's answer
//! that reach a task held after the job started are kept, not applied,
//! and once the hold is released they are applied if nothing moved, and
//! discarded otherwise. Through the supervisor: a held task is not
//! claimed, and a verdict whose job saw its task held meanwhile is kept.
use crate::common;
use crate::common::cli::invoke_with;
use crate::runtime_support;
use dagq::application::{KeptOutcome, TRIAGE_ASKER};
use dagq::domain::{
    AskReason, ClaimOutcome, EventKind, HEARTBEAT_TIMEOUT_SECS, LeaseToken,
    actor_model::{ActorLaunch, ModelRole},
    task_hold::DiscardReason,
};
use dagq::infrastructure::runtime_store::TriageAction;

use runtime_support::*;

/// `dagq` as a person at a plain terminal; its JSON, or a panic.
fn as_user(db: &Path, args: &[&str]) -> Value {
    let output = invoke_with(&[], db, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn hold(db: &Path, target: &str) -> Value {
    as_user(db, &["hold", target, "--reason", "the login expired"])
}

fn release(db: &Path, target: &str) -> Value {
    as_user(db, &["release", target])
}

/// The kinds of the events of `run`, in order.
fn kinds(queue: &SqliteQueue, run: &TaskRun) -> Vec<String> {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .map(|e| e.kind.to_string())
        .collect()
}

/// The last event of `kind` of `run`.
fn last(queue: &SqliteQueue, run: &TaskRun, kind: &str) -> dagq::domain::RunEvent {
    queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind.as_str() == kind)
        .unwrap_or_else(|| panic!("no {kind}"))
}

/// An interrupted run of task 1, made through the store with no session,
/// on a queue whose clock is ahead of the claim's real time (so the
/// claim's lease is stale and `recover_run` takes the run).
fn interrupted(repo: &Path, db: &Path) -> (SqliteQueue, TaskRun) {
    let run = provision_under(repo, db, "first");
    let clock = ManualClock::at(unix_second_now() + 10 * HEARTBEAT_TIMEOUT_SECS);
    let mut queue = SqliteQueue::open(db).unwrap().with_generators(Generators {
        clock: Arc::new(clock),
        ids: Arc::new(clock::UuidGenerator),
    });
    let run = queue.recover_run(run.id(), 0, json!({})).unwrap();
    assert_eq!(run.status(), RunStatus::Interrupted);
    (queue, run)
}

const JOB: &str = "job";

/// A recovery job's round on `run`, as the fill pass starts it.
fn begin_round(queue: &mut SqliteQueue, run: &TaskRun) -> Option<(TaskRun, usize)> {
    queue
        .begin_triage(
            run.id(),
            &LeaseToken::new(JOB),
            Some(json!({"alert": "interrupted", "by": "runtime"})),
            &ActorLaunch::default_of(ModelRole::Recovery),
        )
        .unwrap()
}

/// The round's end with `action`, as `act_on_recovery` ends it, with the
/// repair and the job's record it would write; then the lease goes back.
fn finish_round(queue: &mut SqliteQueue, run: &TaskRun, action: &TriageAction) -> TaskRun {
    let finished = queue
        .finish_triage(
            run.id(),
            &LeaseToken::new(JOB),
            action,
            json!({"by": "runtime"}),
            vec![
                (
                    EventKind::AutoRepaired,
                    json!({"layer": "recovery", "repair": action.as_str()}),
                ),
                (
                    EventKind::RecoveryFinished,
                    json!({"applied": [action.as_str()], "escalated": false}),
                ),
            ],
        )
        .unwrap();
    queue
        .release_lease(run.id(), &LeaseToken::new(JOB))
        .unwrap();
    finished
}

/// The recovery job's `decide` ask on `run`, answered `answer`.
fn answered_decide(queue: &mut SqliteQueue, run: &TaskRun, answer: &str) -> AskId {
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Decide,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Retry it?".into(),
            options: vec!["retry".into(), "resume".into(), "cancel".into()],
            asked_by: TRIAGE_ASKER.into(),
            reason_category: AskReason::RecoveryFailed,
            topics: Vec::new(),
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue.answer(ask, answer).unwrap();
    ask
}

/// (1) A hold that comes after the fill pass read its candidates keeps
/// each start the pass then calls: the claim of a ready task, the
/// recovery job of a stopped run and the resume of a `needs_session` run,
/// whether the task or its run was named;
/// and the release lets each go again.
#[test]
fn a_hold_after_the_line_was_read_keeps_the_claim_the_recovery_job_and_the_resume() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ready = add_ready_task(&mut queue, "ready task", &[]);
    let base = sha(&git_out(&repo, &["rev-parse", "main"]));

    // The claim: a candidate when read, held, not claimed.
    assert!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .any(|task| task.id() == ready)
    );
    hold(&db, &ready.to_string());
    assert!(matches!(
        queue.claim(&base).unwrap(),
        ClaimOutcome::NoReadyTask
    ));
    assert!(
        queue
            .candidates()
            .unwrap()
            .iter()
            .all(|task| task.id() != ready)
    );
    assert_eq!(queue.show(ready).unwrap().task.status(), TaskStatus::Ready);

    // The recovery job: due when read, held by its run's ID, not started.
    assert!(
        queue
            .runs_to_triage()
            .unwrap()
            .iter()
            .any(|due| due.id() == run.id())
    );
    let held = hold(&db, run.id().as_str());
    assert_eq!(held["hold"]["task_id"], 1);
    assert_eq!(held["hold"]["run_id"], json!(run.id()));
    assert!(begin_round(&mut queue, &run).is_none());
    assert!(!kinds(&queue, &run).contains(&"triage_started".to_owned()));

    // The release lets it start; its round parks the run for a resume.
    release(&db, "1");
    assert!(begin_round(&mut queue, &run).is_some());
    let parked = finish_round(
        &mut queue,
        &run,
        &TriageAction::Resume {
            instruction: "go on".into(),
        },
    );
    assert_eq!(parked.status(), RunStatus::NeedsSession);

    // The resume: a candidate when read, held, neither resumed nor given
    // up.
    assert!(
        queue
            .runs_needing_session()
            .unwrap()
            .iter()
            .any(|candidate| candidate.run.id() == run.id())
    );
    hold(&db, "1");
    let resume = |queue: &mut SqliteQueue| {
        queue
            .begin_resume(
                run.id(),
                &LeaseToken::new("s"),
                &base,
                None,
                Default::default(),
            )
            .unwrap()
    };
    assert!(resume(&mut queue).is_none());
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::NeedsSession
    );
    release(&db, "1");
    assert!(resume(&mut queue).is_some());

    // The ready task is claimed once released.
    release(&db, &ready.to_string());
    let ClaimOutcome::Claimed { run: claimed } = queue.claim(&base).unwrap() else {
        panic!("the released task is claimed")
    };
    assert_eq!(claimed.task_id(), ready);
}

/// (2) A recovery job's verdict that would move a task held after the job
/// started is kept: the round applies nothing (`triage_finished` with
/// action `held`, `recovery_finished` without the repair) and
/// `hold_result_kept` records the action and the version. Nothing settles
/// it while the hold is open; after the release it is applied by the
/// round's path, recorded as `hold_result_applied`.
#[test]
fn a_verdict_reaching_a_task_held_meanwhile_is_kept_and_applied_after_the_release() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    assert!(begin_round(&mut queue, &run).is_some());
    let held = hold(&db, "1");
    // The round runs: the hold settles at its end, not before.
    assert_eq!(held["state"], "settling");
    let after = finish_round(&mut queue, &run, &TriageAction::Retry);
    assert_eq!(after.status(), RunStatus::Interrupted);
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    let finished = last(&queue, &run, "triage_finished").payload;
    assert_eq!(finished["action"], "held");
    assert_eq!(finished["kept_action"], "retry");
    let recovered = last(&queue, &run, "recovery_finished").payload;
    assert_eq!(recovered["applied"], json!([]));
    assert_eq!(recovered["held"], true);
    assert!(!kinds(&queue, &run).contains(&"auto_repaired".to_owned()));
    let kept = last(&queue, &run, "hold_result_kept");
    assert_eq!(kept.payload["source"], "recovery_verdict");
    assert_eq!(kept.payload["action"]["action"], "retry");
    assert_eq!(kept.payload["version"]["run_status"], "interrupted");

    // Held: neither listed nor settled, and no round is due.
    assert!(queue.released_kept_results().unwrap().is_empty());
    assert!(matches!(
        queue.settle_kept_result(kept.id).unwrap(),
        KeptOutcome::Untouched
    ));
    assert!(begin_round(&mut queue, &run).is_none());

    // The round's end gave the lease back, and the hold settled with it.
    assert!(
        queue
            .task_hold(TaskId::new(1))
            .unwrap()
            .unwrap()
            .settled_at
            .is_some()
    );
    release(&db, "1");
    assert_eq!(queue.released_kept_results().unwrap(), vec![kept.id]);
    assert!(matches!(
        queue.settle_kept_result(kept.id).unwrap(),
        KeptOutcome::Applied(_)
    ));
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    let applied = last(&queue, &run, "hold_result_applied").payload;
    assert_eq!(applied["kept_id"], json!(kept.id));
    assert_eq!(
        last(&queue, &run, "triage_finished").payload["action"],
        "retry"
    );
    assert!(queue.released_kept_results().unwrap().is_empty());
    assert!(matches!(
        queue.settle_kept_result(kept.id).unwrap(),
        KeptOutcome::Untouched
    ));
}

/// (2) A person's answer to the recovery job's ask of a held task's run is
/// kept with its ask left open, and the decision itself is refused while
/// the hold is open. After the release an answer kept while nothing moved
/// is applied (the ask closes); one kept before the task was edited is
/// discarded as stale, its ask closed, and the run's next recovery round
/// is due. A live session's verdict is kept and discarded at the release.
#[test]
fn an_answer_reaching_a_held_task_is_applied_or_discarded_after_the_release() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "retry");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "retry", "a person chose retry")
            .unwrap()
    );
    // Once per ask.
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "retry", "a person chose retry")
            .unwrap()
    );
    let kept: Vec<_> = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::HoldResultKept)
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].payload["source"], "triage_answer");
    let refused = queue
        .decide_triage(run.id(), ask, "retry", "a person chose retry")
        .unwrap_err();
    assert!(refused.to_string().contains("is held"), "{refused}");
    // A live session's verdict is kept too.
    assert!(
        queue
            .keep_live_verdict(run.id(), json!({"verdict": "repair"}))
            .unwrap()
    );
    release(&db, "1");
    let released = queue.released_kept_results().unwrap();
    assert_eq!(released.len(), 2);
    assert!(matches!(
        queue.settle_kept_result(released[0]).unwrap(),
        KeptOutcome::Applied(_)
    ));
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::Ready
    );
    assert!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .iter()
            .any(|a| a.id == ask && a.closed_at.is_some())
    );
    assert!(matches!(
        queue.settle_kept_result(released[1]).unwrap(),
        KeptOutcome::Discarded(DiscardReason::LiveSession)
    ));
    // Not held: nothing is kept.
    assert!(
        !queue
            .keep_live_verdict(run.id(), json!({"verdict": "repair"}))
            .unwrap()
    );
}

#[test]
fn an_answer_kept_before_the_task_moved_is_discarded_and_the_next_round_is_due() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let ask = answered_decide(&mut queue, &run, "retry");
    hold(&db, "1");
    assert!(
        queue
            .keep_triage_answer(run.id(), ask, "retry", "a person chose retry")
            .unwrap()
    );
    // The run moves while held: a person's recover of it records a
    // progress of its own.
    queue
        .record_runtime_event(run.id(), EventKind::RunRecovered, json!({"by": "test"}))
        .unwrap();
    release(&db, "1");
    let released = queue.released_kept_results().unwrap();
    assert!(matches!(
        queue.settle_kept_result(released[0]).unwrap(),
        KeptOutcome::Discarded(DiscardReason::Changed)
    ));
    assert_eq!(
        queue.show(TaskId::new(1)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    let discarded = last(&queue, &run, "hold_result_discarded").payload;
    assert_eq!(discarded["reason"], "changed");
    assert!(
        queue
            .asks(AskQuery {
                all: true,
                ..Default::default()
            })
            .unwrap()
            .iter()
            .any(|a| a.id == ask && a.closed_at.is_some())
    );
    assert!(begin_round(&mut queue, &run).is_some());
}

/// Through the supervisor: a held ready task is not claimed, the pass
/// records why once (`hold_deferred`), and the task keeps its status.
#[test]
fn the_supervisor_claims_no_held_task() {
    let (_dir, repo, db) = fixture();
    hold(&db, "1");
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(detail.runs.is_empty());
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    let deferred: Vec<_> = detail
        .events
        .iter()
        .filter(|e| e.kind == EventKind::HoldDeferred)
        .collect();
    assert_eq!(deferred.len(), 1, "{deferred:?}");
    assert_eq!(deferred[0].payload["step"], "claim");
}

/// Through the supervisor: a task held while its run's recovery job runs
/// (the job's script holds it) gets the job's `retry` kept, not applied:
/// no second run is claimed and the run stays as it ended.
#[test]
fn the_supervisor_keeps_a_verdict_whose_task_was_held_while_the_job_ran() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let hold_now = format!(
        "sqlite3 -cmd '.timeout 5000' {} {} || exit 97",
        shell_path(&db),
        dagq::infrastructure::adapters::shell_quote(
            "INSERT INTO task_holds(task_id, reason, by_role, by_id, created_at, settled_at)
             VALUES (1, 'held while the job ran', 'user', 'user', 0, 0);"
        )
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[format!(
        "{hold_now} && {}",
        repair(json!({"action": "retry"}), "the session died on its own")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1, "no second run while held");
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(
        last(&queue, run, "triage_finished").payload["action"],
        "held"
    );
    assert_eq!(
        last(&queue, run, "hold_result_kept").payload["source"],
        "recovery_verdict"
    );
    assert!(common::cli::invoke(&db, &["show", "1"]).status.success());
}

/// The takeover of a resting run for its hold's park: refused while the
/// recovery round's lease is fresh, taken once it is stale (as a killed
/// supervisor's), and the lease given back settles the hold in the same
/// transaction; a settled hold is taken over no more.
#[test]
fn a_held_runs_stale_lease_is_taken_over_for_the_park_only_while_the_hold_settles() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    assert!(begin_round(&mut queue, &run).is_some());
    let held = hold(&db, "1");
    assert_eq!(held["state"], "settling");
    let hold_id = dagq::domain::HoldId::new(held["hold"]["id"].as_i64().unwrap());
    let other = LeaseToken::new("other");
    assert!(
        queue
            .lease_for_hold_park(run.id(), &other, hold_id)
            .unwrap()
            .is_none(),
        "a fresh lease is the round's"
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_at=0, pid=?2 WHERE run_id=?1",
            rusqlite::params![run.id(), dead_pid()],
        )
        .unwrap();
    let taken = queue
        .lease_for_hold_park(run.id(), &other, hold_id)
        .unwrap()
        .expect("a stale lease is taken over");
    assert_eq!(taken.status(), RunStatus::Interrupted);
    let acquired = last(&queue, &run, "lease_acquired").payload;
    assert_eq!(acquired["reason"], "hold_park");
    assert_eq!(acquired["previous_token"], JOB);
    assert!(
        queue
            .task_hold(TaskId::new(1))
            .unwrap()
            .unwrap()
            .settled_at
            .is_none()
    );
    queue.release_lease(run.id(), &other).unwrap();
    assert!(
        queue
            .task_hold(TaskId::new(1))
            .unwrap()
            .unwrap()
            .settled_at
            .is_some()
    );
    assert!(
        queue
            .lease_for_hold_park(run.id(), &other, hold_id)
            .unwrap()
            .is_none(),
        "a settled hold parks nothing more"
    );
}

/// A lease given back is no writer's end (a resume given up keeps a
/// session whose wrapper may still run its turn), and neither is a
/// heartbeat gone old while the process lives: a run resting with nobody
/// holding it while a session process it registered has no exit recorded
/// is no safe wait. A hold on it is `settling`, a release refused, and a
/// lease given back without a look at the writer settles nothing, whether
/// the wrapper's heartbeat is old or fresh; once the wrapper's exit is
/// recorded, the next lease given back settles it. A park that found the
/// writer ended settles it as it gives the lease back. The decision is
/// `domain::task_hold::settles_at_rest`'s (its unit test goes over the
/// cases); this checks that the store's transactions read the records
/// into it.
#[test]
fn a_rest_whose_registered_wrapper_still_runs_is_no_safe_wait_for_a_hold() {
    let (_dir, repo, db) = fixture();
    let (mut queue, run) = interrupted(&repo, &db);
    let connection = Connection::open(&db).unwrap();
    connection
        .execute(
            "UPDATE task_runs SET status='needs_session' WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_processes(run_id, role, pid, heartbeat_at)
             VALUES (?1, 'wrapper', ?2, 0)",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    let held = hold(&db, "1");
    assert_eq!(held["state"], "settling", "{held}");
    let output = invoke_with(&[], &db, &["release", "1"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hold_not_settled"));
    let hold_id = dagq::domain::HoldId::new(held["hold"]["id"].as_i64().unwrap());
    let token = LeaseToken::new("park");
    let settled = || {
        SqliteQueue::open(&db)
            .unwrap()
            .task_hold(TaskId::new(1))
            .unwrap()
            .unwrap()
            .settled_at
            .is_some()
    };
    // A lease given back as a step's end: the wrapper's heartbeat is old
    // but its process (this test's) lives, and so may its turn.
    assert!(
        queue
            .lease_for_hold_park(run.id(), &token, hold_id)
            .unwrap()
            .is_some()
    );
    queue.release_lease(run.id(), &token).unwrap();
    assert!(!settled());
    // Its heartbeat fresh again: still a writer.
    connection
        .execute(
            "UPDATE run_processes SET heartbeat_at=unixepoch() WHERE run_id=?1",
            [run.id()],
        )
        .unwrap();
    assert!(
        queue
            .lease_for_hold_park(run.id(), &token, hold_id)
            .unwrap()
            .is_some()
    );
    queue.release_lease(run.id(), &token).unwrap();
    assert!(!settled());
    // The wrapper's exit is recorded: the next lease given back settles it.
    connection
        .execute(
            "UPDATE run_processes SET exited_at=unixepoch(), exit_code=0 WHERE run_id=?1",
            [run.id()],
        )
        .unwrap();
    assert!(
        queue
            .lease_for_hold_park(run.id(), &token, hold_id)
            .unwrap()
            .is_some()
    );
    queue.release_lease(run.id(), &token).unwrap();
    assert!(settled());
    release(&db, "1");

    // A park that found the writer ended (a wrapper that died without
    // recording its exit, heartbeating still by its row) settles the hold
    // as it gives the lease back.
    connection
        .execute(
            "UPDATE run_processes SET exited_at=NULL, heartbeat_at=unixepoch() WHERE run_id=?1",
            [run.id()],
        )
        .unwrap();
    let held = hold(&db, "1");
    assert_eq!(held["state"], "settling");
    let hold_id = dagq::domain::HoldId::new(held["hold"]["id"].as_i64().unwrap());
    assert!(
        queue
            .lease_for_hold_park(run.id(), &token, hold_id)
            .unwrap()
            .is_some()
    );
    assert!(queue.release_for_hold(hold_id, run.id(), &token).unwrap());
    assert!(settled());
    assert!(queue.run_lease(run.id()).unwrap().is_none());
}
