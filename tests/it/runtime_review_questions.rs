//! Runtime tests: a revise follows only the `worker_question`s asked since
//! its request; one from before it is the inbox's (task 582). One open
//! since it holds the judgment only until a rewritten receipt (task 583).
use crate::runtime_support;

use dagq::domain::{AskId, AskReason, NewAsk};
use runtime_support::*;

/// A worker whose first turn commits and writes its receipt, and whose
/// turn of the revise request runs `revise` (goal 92).
fn on_revise(revise: &str) -> String {
    format!(
        "case \"$TURN\" in\n\
         1) commit work; receipt \"$(git rev-parse HEAD)\" ;;\n\
         2) {revise}\n;;\n\
         esac\n"
    )
}

/// A `revise` verdict that waits for [`ask_during_review`] (its file next to
/// the queue `db`), so that a question can be asked during the review, in
/// an earlier second than the revise request.
fn slow_revise(db: &std::path::Path) -> String {
    format!(
        "{}; {}",
        crate::common::await_path(review_go(db)),
        verdict("revise", &["add a line"], "one gap")
    )
}

/// The file that lets [`slow_revise`] print its verdict.
fn review_go(db: &std::path::Path) -> std::path::PathBuf {
    db.with_file_name("review.go")
}

/// Opens, during the run's review, the `worker_question` a worker asked
/// then (as a stray `dagq ask` after its receipt does).
fn ask_during_review(db: &std::path::Path) -> AskId {
    wait_until(db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"review_started")
    });
    let mut queue = SqliteQueue::open(db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: vec!["task_overlap".into()],
            kind: AskKind::WorkerQuestion,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question: "Which line?".into(),
            options: vec![],
            asked_by: "worker".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"revise_requested"), "{kinds:?}");
    // The review ends in a later second than the ask's.
    while unix_now() <= ask.created_at {
        thread::sleep(Duration::from_millis(20));
    }
    fs::write(review_go(db), "").unwrap();
    ask.id
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Waits for the revise request and checks that `ask` was created in an
/// earlier second than it was sent.
fn await_revise_after(db: &std::path::Path, ask: AskId) {
    wait_until(db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"revise_requested")
    });
    let mut queue = SqliteQueue::open(db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    // Recorded to the millisecond (task 1197), compared to the second of
    // the ask.
    let sent_at = payloads(&detail, "revise_requested")[0]["sent_at"]
        .as_f64()
        .unwrap()
        .floor() as i64;
    let created_at = queue.read_ask(ask).unwrap().created_at;
    assert!(created_at < sent_at, "{created_at} {sent_at}");
}

/// The supervisor under test, and the count of its passes.
fn run_supervisor(
    db: &std::path::Path,
    repo: &std::path::Path,
    backend: &Arc<TestWorkspace>,
    reviewer: &Arc<TestReviewer>,
) -> (thread::JoinHandle<Value>, Arc<AtomicU64>) {
    let (supervisor, passes, _) = run_supervisor_ahead(db, repo, backend, reviewer);
    (supervisor, passes)
}

/// [`run_supervisor`] with the [`MonotonicAhead`] of its clock: the test
/// runs the revise's resume timeout out by moving the clock on, not by
/// sleeping (task 1557). When the timeout ends a revise at which time is
/// the unit tests' (`revise::tests`); these check that the revise reads
/// the supervisor's clock and what the end does.
fn run_supervisor_ahead(
    db: &std::path::Path,
    repo: &std::path::Path,
    backend: &Arc<TestWorkspace>,
    reviewer: &Arc<TestReviewer>,
) -> (thread::JoinHandle<Value>, Arc<AtomicU64>, MonotonicAhead) {
    let (options, ahead) = supervise_options_ahead(4, true);
    let passes = options.passes.clone();
    let (db, repo, backend, reviewer) = (
        db.to_owned(),
        repo.to_owned(),
        backend.clone(),
        reviewer.clone(),
    );
    let supervisor =
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options));
    (supervisor, passes, ahead)
}

/// The revise's `approve_landing` ask, with the worker's question from
/// before it still open for the inbox.
fn assert_revise_ended(queue: &mut SqliteQueue, why: &str) {
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "revise_requested") < position(&kinds, "exit_requested"),
        "{kinds:?}"
    );
    let mut asks = queue.asks(AskQuery::default()).unwrap();
    asks.sort_by_key(|a| a.id);
    assert_eq!(asks.len(), 2, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::WorkerQuestion);
    assert_eq!(asks[1].kind, AskKind::ApproveLanding);
    assert!(asks[1].question.contains(why), "{}", asks[1].question);
}

/// An answer to a question the worker asked during its review, answered
/// while the revise waits, is the inbox's to deliver by hand
/// (`runtime_delivers: false`): the revise does not type it (no
/// `ask_delivered`), nor waits for its close, and judges the rewritten
/// receipt; the run lands.
#[test]
fn an_answer_to_a_question_from_before_the_revise_is_left_to_the_inbox() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &on_revise(
            "await_file \"$EXIT.go\"\n\
             printf 'fix\\n' >> change.txt; git commit -q -am fix\n\
             receipt \"$(git rev-parse HEAD)\"",
        ),
    ));
    let reviewer = Arc::new(TestReviewer::new(&[
        slow_revise(&db),
        verdict("pass", &[], "fixed"),
    ]));
    let (supervisor, passes) = run_supervisor(&db, &repo, &backend, &reviewer);
    let ask = ask_during_review(&db);
    await_revise_after(&db, ask);
    // Answered while the revise waits: the answer is the inbox's.
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask, "the second").unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let answered = payloads(&detail, "ask_answered");
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["runtime_delivers"], false, "{}", answered[0]);
    // The revise watch polls a while with the answer there.
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "ask_delivered").is_empty());
    let run = detail.runs[0].clone();
    let texts = session_texts(&run);
    assert_eq!(texts.len(), 1, "{texts:?}");

    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert!(payloads(&detail, "ask_delivered").is_empty());
    let texts = session_texts(&detail.runs[0]);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(!texts[0].contains("answer to ask"), "{texts:?}");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());
}

/// A question left open from the review does not hold a revise: the
/// session that goes idle without rewriting its receipt is judged so, and
/// the run goes on to its `approve_landing` ask.
#[test]
fn a_question_open_from_before_the_revise_does_not_hold_an_idle_session() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, &on_revise(":")));
    let reviewer = Arc::new(TestReviewer::new(&[slow_revise(&db)]));
    let (supervisor, _) = run_supervisor(&db, &repo, &backend, &reviewer);
    let ask = ask_during_review(&db);
    await_revise_after(&db, ask);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_revise_ended(&mut queue, "went idle without rewriting the receipt");
    assert!(payloads(&queue.show(TaskId::new(1)).unwrap(), "ask_delivered").is_empty());
}

/// A question left open from the review does not stop the revise's
/// resume timeout either: a session that neither rewrites its receipt nor
/// goes idle runs out of it. The timeout is run out on the supervisor's
/// clock (task 1557).
#[test]
fn a_question_open_from_before_the_revise_does_not_stop_its_resume_timeout() {
    let (_dir, repo, db) = fixture();
    // The turn of the revise runs on until it is stopped.
    let backend = TestWorkspace::new(&db, false, &on_revise("while :; do sleep 0.05; done"));
    let timeout = backend.resume_timeout;
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[slow_revise(&db)]));
    let (supervisor, passes, ahead) = run_supervisor_ahead(&db, &repo, &backend, &reviewer);
    let ask = ask_during_review(&db);
    await_revise_after(&db, ask);
    // The pass that sent the request made its watch; the next one is after.
    await_passes(&passes, 1);
    ahead.by(timeout);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_revise_ended(
        &mut queue,
        &format!(
            "did not rewrite the receipt within {} seconds",
            timeout.as_secs()
        ),
    );
}

/// What a worker does first in its turn of the revise request: it asks a
/// `worker_question` and does not stop at it.
const ASKS: &str = r#""$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which line?' --cmux /usr/bin/true > /dev/null || exit 70"#;

/// A worker that asks in its turn of the revise request ([`ASKS`]) and then
/// runs `revise`, and whose next turn (the answer, or the request to fix
/// its receipt) runs `next` (goal 92).
fn asks_after_revise(revise: &str, next: &str) -> String {
    format!(
        "case \"$TURN\" in\n\
         1) commit work; receipt \"$(git rev-parse HEAD)\" ;;\n\
         2) {ASKS}\n{revise}\n;;\n\
         *) {next}\n;;\n\
         esac\n"
    )
}

/// The `worker_question` a worker asked during its revise, still open.
fn open_question(queue: &mut SqliteQueue) -> AskId {
    let asks = queue.asks(AskQuery::default()).unwrap();
    let question = asks
        .iter()
        .find(|a| a.kind == AskKind::WorkerQuestion)
        .unwrap_or_else(|| panic!("{asks:?}"));
    assert!(question.closed_at.is_none(), "{question:?}");
    question.id
}

/// A session that asks a question during its revise and goes on to rewrite
/// its receipt and go idle is judged by the rewritten receipt without the
/// question's close (task 583): the run lands.
#[test]
fn a_receipt_rewritten_after_an_open_question_is_judged() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &asks_after_revise(
            "printf 'fix\\n' >> change.txt; git commit -q -am fix\n\
             receipt \"$(git rev-parse HEAD)\"",
            ":",
        ),
    ));
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let (supervisor, _) = run_supervisor(&db, &repo, &backend, &reviewer);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    let kinds = event_kinds(&detail);
    assert!(
        position(&kinds, "revise_requested") < position(&kinds, "ask_opened"),
        "{kinds:?}"
    );
    open_question(&mut queue);
}

/// A receipt rewritten after an open question that names another commit
/// is judged too, as a mismatch, without the question's close (task 583):
/// the session is asked to fix it, and the run lands on the fixed receipt.
#[test]
fn a_mismatched_receipt_rewritten_after_an_open_question_is_judged() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &asks_after_revise(
            "printf 'fix\\n' >> change.txt; git commit -q -am fix\n\
             receipt 0123456789012345678901234567890123456789",
            "receipt \"$(git rev-parse HEAD)\"",
        ),
    ));
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let (supervisor, _) = run_supervisor(&db, &repo, &backend, &reviewer);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let rejected = payloads(&detail, "revise_receipt_rejected");
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0]["code"], "commit_mismatch", "{}", rejected[0]);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    open_question(&mut queue);
}

/// A question the session went on past (it rewrote its receipt after it)
/// holds the revise no longer: once the mismatched receipt is sent back to
/// be fixed, the session that goes idle without fixing it is judged so
/// while the question is still open (task 583).
#[test]
fn a_question_passed_by_a_rewritten_receipt_does_not_hold_its_fix() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &asks_after_revise("receipt 0123456789012345678901234567890123456789", ":"),
    ));
    let reviewer = Arc::new(TestReviewer::new(&[verdict(
        "revise",
        &["add a line"],
        "one gap",
    )]));
    let (supervisor, _) = run_supervisor(&db, &repo, &backend, &reviewer);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "revise_receipt_rejected").len(), 1);
    open_question(&mut queue);
    let asks = queue.asks(AskQuery::default()).unwrap();
    let approve = asks
        .iter()
        .find(|a| a.kind == AskKind::ApproveLanding)
        .unwrap_or_else(|| panic!("{asks:?}"));
    assert!(
        approve
            .question
            .contains("went idle without rewriting the receipt"),
        "{}",
        approve.question
    );
}

/// A session idle at a question it asked during its revise, its receipt
/// not rewritten, still waits for the answer past the resume timeout (task
/// 238): answered, it rewrites the receipt and the run lands. The timeout
/// is run out on the supervisor's clock once the session is idle at its
/// question (task 1557): with the default timeout, neither the question
/// nor the turn of the answer races a short one under load (task 1360).
#[test]
fn an_open_question_without_a_rewritten_receipt_holds_past_the_resume_timeout() {
    let (_dir, repo, db) = fixture();
    let _dump = EventsOnPanic(db.clone());
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &asks_after_revise(
            ":",
            "printf 'fix\\n' >> change.txt; git commit -q -am fix\n\
             receipt \"$(git rev-parse HEAD)\"",
        ),
    );
    let timeout = backend.resume_timeout;
    let backend = Arc::new(backend);
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let (supervisor, passes, ahead) = run_supervisor_ahead(&db, &repo, &backend, &reviewer);
    wait_until(&db, Duration::from_secs(30), |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|a| a.kind == AskKind::WorkerQuestion)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = open_question(&mut queue);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let opened = first_event_millis(&detail, "ask_opened");
    // Idle at its question past the resume timeout, and passes after it.
    await_written_after(&detail.runs[0].idle_marker_path().unwrap(), opened);
    await_passes(&passes, 1);
    ahead.by(timeout + Duration::from_secs(1));
    await_passes(&passes, SOME_PASSES);
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
    assert_eq!(queue.asks(AskQuery::default()).unwrap().len(), 1);

    queue.answer(ask, "the second").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
}
