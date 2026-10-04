//! Runtime tests (task 1372): a headless run's session lost while it waits
//! for an answer is opened again and the wait goes on, within a bound of
//! attempts; and a `worker_question` closed without its answer is told to
//! the worker in place of the nudge, once.
use crate::common;
use crate::runtime_support;

use dagq::domain::LeaseToken;
use dagq::domain::turn::exit_path;
use runtime_support::headless::*;
use runtime_support::*;

/// Turn 1 asks; a later turn finishes on the answer.
fn asking_turns() -> String {
    format!(
        r#"case "$TURN" in
1) ask "which file"; say asked ;;
*) case "$PROMPT" in "answer to ask "*) {FINISH} ;; *) say lost ;; esac ;;
esac"#
    )
}

/// Wait until the run waits for its question; the run and the ask.
fn waiting_run(db: &Path) -> (TaskRun, dagq::domain::Ask) {
    wait_until(db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "run_waiting_started")
    });
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TASK).unwrap().runs[0].clone();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    (run, ask)
}

/// End the session's wrapper as a wrapper that stopped while the run
/// waits: it takes the exit request and records its exit.
fn end_wrapper(db: &Path, run: &TaskRun) {
    fs::write(exit_path(Path::new(run.run_dir().unwrap())), "").unwrap();
    wait_until(db, common::STEP_LIMIT, |queue| {
        queue
            .show(TASK)
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "session_exited")
    });
}

fn reopens(detail: &dagq::domain::TaskDetail) -> Vec<Value> {
    payloads(detail, "auto_repaired")
        .into_iter()
        .filter(|p| p["repair"] == "headless_session_reopened")
        .cloned()
        .collect()
}

/// Acceptance (1): the wrapper of a headless run that waits for the answer
/// of its `worker_question` ends; the supervisor closes its workspace,
/// opens a new one whose wrapper resumes the same session, records
/// `auto_repaired`, and the wait goes on: the run neither fails nor goes
/// to a recovery job. The answer is the reopened session's next turn, and
/// the run lands.
#[test]
fn a_session_lost_during_its_wait_is_opened_again_and_takes_the_answer() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &asking_turns());
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let (run, ask) = waiting_run(&db);
    end_wrapper(&db, &run);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !reopens(&queue.show(TASK).unwrap()).is_empty()
    });
    let reopened = reopens(&detail(&db));
    assert_eq!(reopened.len(), 1, "{reopened:?}");
    assert_eq!(reopened[0]["layer"], "runtime");
    assert_eq!(reopened[0]["conditions"]["attempt"], 1);
    assert_eq!(reopened[0]["conditions"]["open_asks"], json!([ask.id]));
    assert_eq!(reopened[0]["detail"]["cause"], "exited");
    assert_eq!(reopened[0]["detail"]["exit_code"], 0);
    assert_eq!(
        reopened[0]["detail"]["previous_workspace"],
        json!(workspace_id(0))
    );
    // The wait goes on in the reopened workspace, with the run running.
    let now = detail(&db);
    assert_eq!(now.runs[0].status(), RunStatus::Running);
    assert_eq!(now.runs[0].workspace_id(), Some(workspace_id(1).as_str()));
    assert!(payloads(&now, "run_waiting_ended").is_empty());
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1],
        format!("resume {} answer to ask {}: change.txt", run.id(), ask.id)
    );
    let created = payloads(&detail, "workspace_created");
    assert_eq!(created[1]["reopened"], 1, "{created:?}");
    assert_eq!(payloads(&detail, "wrapper_started").len(), 2);
    assert!(backend.closed().contains(&workspace_id(0)));
    let ended = payloads(&detail, "run_waiting_ended");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["cause"], "answered");
    assert!(payloads(&detail, "recovery_requested").is_empty());
    assert!(payloads(&detail, "session_reopen_failed").is_empty());
}

/// Acceptance (1): a reopened workspace whose wrapper never registers is a
/// failed attempt (`session_reopen_failed`); past three attempts in a row
/// the run goes the way of a session that exited without a receipt: it
/// fails and goes to its recovery job (alert `failed`).
#[test]
fn a_session_that_cannot_be_opened_again_goes_to_its_recovery_job() {
    let (dir, repo, db, mut backend) = headless_fixture(&[]);
    set_turns(dir.path(), &asking_turns());
    backend.resume_no_session = true;
    backend.registration_timeout = Duration::from_millis(500);
    backend.reopen_interval = Duration::from_millis(100);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let (run, _ask) = waiting_run(&db);
    end_wrapper(&db, &run);
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    let reopened = reopens(&detail);
    assert_eq!(reopened.len(), 3, "{reopened:?}");
    let attempts: Vec<_> = reopened
        .iter()
        .map(|p| p["conditions"]["attempt"].clone())
        .collect();
    assert_eq!(attempts, [json!(1), json!(2), json!(3)]);
    let failed = payloads(&detail, "session_reopen_failed");
    assert_eq!(failed.len(), 3, "{failed:?}");
    assert!(failed.iter().all(|p| p["cause"] == "registration_timeout"));
    let ended = payloads(&detail, "run_waiting_ended");
    assert_eq!(ended.last().unwrap()["cause"], "session_exited");
    let requested = payloads(&detail, "recovery_requested");
    assert!(
        requested.iter().any(|p| p["alert"] == "failed"),
        "{requested:?}"
    );
    assert!(payloads(&detail, "runtime_error").is_empty());
}

/// Close the answered-or-not `worker_question` `ask` as the inbox does,
/// with `answer` recorded, in one transaction (so the supervisor cannot
/// deliver it between the answer and the close).
fn close_as_inbox(db: &Path, run: &TaskRun, ask: AskId, answer: &str) {
    let mut raw = Connection::open(db).unwrap();
    let tx = raw.transaction().unwrap();
    tx.execute(
        "UPDATE asks SET answer=?2, answered_at=strftime('%s','now'), answered_by='inbox',
         closed_at=strftime('%s','now') WHERE id=?1",
        rusqlite::params![ask, answer],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload,actor_role,actor_id)
         VALUES (?1,?2,'ask_closed',?3,'inbox','inbox')",
        rusqlite::params![
            run.task_id(),
            run.id(),
            json!({"ask_id": ask, "kind": "worker_question"}).to_string()
        ],
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Acceptance (2), headless: a `worker_question` the inbox closed without
/// its answer delivered ends the wait; the session's next turn is the
/// notice that names the ask, who closed it and what was recorded, in
/// place of the nudge, recorded as `stall_nudged` with `closed_ask`, once.
#[test]
fn a_question_closed_without_its_answer_is_told_as_the_next_turn() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) ask "which file"; say asked ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    let (run, ask) = waiting_run(&db);
    close_as_inbox(&db, &run, ask.id, "decide it yourself");
    let detail = {
        let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        detail(&db)
    };
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    let nudged = payloads(&detail, "stall_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["closed_ask"], json!(ask.id));
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    let requested = payloads(&detail, "turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    let turns = Path::new(run.run_dir().unwrap()).join("turns");
    let text = fs::read_dir(&turns)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("request")
        })
        .map(|path| fs::read_to_string(path).unwrap())
        .find(|text| text.contains("worker_question"))
        .expect("the notice was requested");
    assert!(
        text.contains(&format!(
            "ask {} (your worker_question on run {}) was closed by inbox without an answer delivered to you.",
            ask.id,
            run.id()
        )),
        "{text}"
    );
    assert!(
        text.contains("What was recorded with it when it was closed: decide it yourself"),
        "{text}"
    );
    assert!(
        text.contains("Do not ask the same question again."),
        "{text}"
    );
    assert!(!text.contains("ended without a receipt or an open question"));
}

/// The queue's side of a reopen: only a running run's lost wrapper (or no
/// wrapper) is forgotten, one that registered under another pid refuses
/// it; a resume wrapper then registers for the running run once, the new
/// workspace becomes the run's with the repair recorded, and a session no
/// attempt could open again ends with the lost wrapper's code.
#[test]
fn the_queue_forgets_only_the_lost_wrapper_and_takes_the_reopened_one() {
    use dagq::{domain::ClaimOutcome, infrastructure::runtime_store::RunPlan};
    let (_dir, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let owner = LeaseToken::new("owner");
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&sha("0123456789abcdef0123456789abcdef01234567"), &owner)
        .unwrap()
    else {
        panic!()
    };
    let plan = RunPlan {
        repo_path: "/test".into(),
        run_dir: "/run".into(),
        branch: "dagq/test".into(),
        worktree_path: "/run/worktree".into(),
        receipt_path: "/run/receipt.json".into(),
        log_path: "/run/log".into(),
    };
    queue.plan_run(run.id(), &owner, &plan).unwrap();
    queue.workspace_created(run.id(), &owner, "ws-1").unwrap();
    // Not running yet.
    assert!(queue.clear_lost_session(run.id(), &owner, None).is_err());
    queue.register_wrapper(run.id(), &owner, 10).unwrap();
    queue.register_agent(run.id(), 10, 12).unwrap();
    // Wrapper 10 has not recorded its exit: only its own loss forgets it.
    assert!(
        queue
            .clear_lost_session(run.id(), &owner, Some(11))
            .is_err()
    );
    assert!(queue.clear_lost_session(run.id(), &owner, None).is_err());
    let other = LeaseToken::new("other");
    assert!(
        queue
            .clear_lost_session(run.id(), &other, Some(10))
            .is_err()
    );
    queue
        .clear_lost_session(run.id(), &owner, Some(10))
        .unwrap();
    assert!(queue.processes(run.id()).unwrap().is_empty());
    queue.register_resume_wrapper(run.id(), &owner, 20).unwrap();
    assert!(queue.register_resume_wrapper(run.id(), &owner, 21).is_err());
    queue
        .session_reopened(
            run.id(),
            &owner,
            "ws-2",
            1,
            json!({"layer": "runtime", "repair": "headless_session_reopened"}),
        )
        .unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let reopened = &detail.runs[0];
    assert_eq!(reopened.status(), RunStatus::Running);
    assert_eq!(reopened.workspace_id(), Some("ws-2"));
    let created = payloads(&detail, "workspace_created");
    assert_eq!(created.last().unwrap()["reopened"], 1, "{created:?}");
    assert_eq!(
        payloads(&detail, "auto_repaired")[0]["repair"],
        "headless_session_reopened"
    );
    // The resume wrapper exited too and no attempt is left.
    queue.wrapper_exited(run.id(), 20, 0).unwrap();
    queue
        .clear_lost_session(run.id(), &owner, Some(20))
        .unwrap();
    let ended = queue
        .finish_lost_session(run.id(), &owner, 1, false)
        .unwrap();
    assert_eq!(ended.status(), RunStatus::Failed);
    assert!(queue.clear_lost_session(run.id(), &owner, None).is_err());
}

/// Make the run's last turn look lost in its middle, still running: its
/// agent row names `agent` (a live process standing in for the turn) and a
/// turn 2 started that no `turn_finished` follows. The wrapper then ends
/// as one lost while the run waits.
fn lose_wrapper_mid_turn(db: &Path, run: &TaskRun, agent: u32) {
    let raw = Connection::open(db).unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2 WHERE run_id=?1 AND role='agent'",
        rusqlite::params![run.id(), agent],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,?2,'turn_started',?3)",
        rusqlite::params![
            run.task_id(),
            run.id(),
            json!({"turn": 2, "resume": true}).to_string()
        ],
    )
    .unwrap();
    end_wrapper(db, run);
}

/// Task 1372's revise: a wrapper lost while its turn still runs is not
/// given up. The wait goes on with nothing opened beside the turn; once
/// the turn ended, the session is opened again, the lost turn's idle
/// marker is written (`lost_turn`), and the answer is the reopened
/// session's next turn.
#[test]
fn a_session_lost_in_the_middle_of_a_turn_is_opened_again_once_the_turn_ended() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &asking_turns());
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    let (run, ask) = waiting_run(&db);
    let mut turn = sleeper();
    lose_wrapper_mid_turn(&db, &run, turn.id());
    await_passes(&passes, 5);
    let now = detail(&db);
    assert!(reopens(&now).is_empty(), "{:?}", reopens(&now));
    assert!(payloads(&now, "run_waiting_ended").is_empty());
    assert_eq!(now.runs[0].status(), RunStatus::Running);
    turn.kill().unwrap();
    turn.wait().unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !reopens(&queue.show(TASK).unwrap()).is_empty()
    });
    let reopened = reopens(&detail(&db));
    assert_eq!(reopened[0]["conditions"]["lost_turn"], 2, "{reopened:?}");
    let marker = fs::read_to_string(run.idle_marker_path().unwrap()).unwrap();
    assert!(marker.contains("\"turn\":2"), "{marker}");
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "change.txt")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_landed_run(run, &repo, &base);
    let calls = stub_calls(run);
    assert_eq!(
        calls.last().unwrap(),
        &format!("resume {} answer to ask {}: change.txt", run.id(), ask.id)
    );
    assert!(payloads(&detail, "recovery_requested").is_empty());
}

/// A turn that outlives its lost wrapper past the turn's limit is not
/// waited for longer: nothing is opened beside it, and the run goes the
/// way of a session that exited, to its recovery job.
#[test]
fn a_turn_outliving_its_lost_wrapper_past_its_limit_goes_to_recovery() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &asking_turns());
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        dagq::domain::stall::StallConfig::default().with_millis("turn_limit_secs", 3000),
        &[],
    );
    let (run, _ask) = waiting_run(&db);
    let mut turn = sleeper();
    lose_wrapper_mid_turn(&db, &run, turn.id());
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    turn.kill().unwrap();
    turn.wait().unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    assert!(reopens(&detail).is_empty());
    let ended = payloads(&detail, "run_waiting_ended");
    assert_eq!(ended.last().unwrap()["cause"], "session_exited");
    let requested = payloads(&detail, "recovery_requested");
    assert!(
        requested.iter().any(|p| p["alert"] == "failed"),
        "{requested:?}"
    );
}
