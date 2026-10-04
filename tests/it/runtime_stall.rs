//! Runtime tests: Idle sessions without a receipt: the nudge and the stalled ask.
use crate::runtime_support;

use runtime_support::*;

/// The receipt-less idle threshold of these tests, in milliseconds.
const IDLE_MS: u64 = 200;

/// Supervisor options whose receipt-less idle threshold is [`IDLE_MS`].
fn stall_options() -> SuperviseOptions {
    SuperviseOptions {
        stall: Some(
            dagq::domain::stall::StallConfig::default()
                .with_millis("idle_without_receipt_secs", IDLE_MS),
        ),
        ..supervise_options(4, true)
    }
}

/// The threshold: a check that nothing happens past it waits it out and
/// then some passes of the supervisor (task 1075).
const IDLE: Duration = Duration::from_millis(IDLE_MS);

/// A worker whose turn ended at its own `worker_question` waits for a
/// person, not stalled: nothing is sent to it until the answer, and no
/// nudge follows it.
#[test]
fn a_worker_idle_at_its_question_is_not_nudged() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        r#"
case "$TURN" in
1) "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
*) commit work; receipt "$(git rev-parse HEAD)" ;;
esac
"#,
    ));
    let options = stall_options();
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise_with(&db, &repo, &backend, &options))
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    wait_until(&db, Duration::from_secs(30), |queue| {
        !queue.asks(Default::default()).unwrap().is_empty()
    });
    let ask = queue.asks(Default::default()).unwrap().remove(0);
    // Past the threshold from the session's idle after its question.
    let detail = queue.show(TaskId::new(1)).unwrap();
    await_written_after(
        &detail.runs[0].idle_marker_path().unwrap(),
        first_event_millis(&detail, "ask_opened"),
    );
    thread::sleep(IDLE);
    await_passes(&passes, SOME_PASSES);
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    let texts = session_texts(&run);
    assert!(texts.is_empty(), "{texts:?}");
    queue.answer(ask.id, "blue").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(session_texts(&run).len(), 1);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(payloads(&detail, "stall_nudged").is_empty());
    assert!(payloads(&detail, "stall_resolved").is_empty());
    assert!(stalled_asks(&queue).is_empty());
}
