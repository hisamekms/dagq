//! Runtime tests: the `stalled` ask of a headless worker (task 1179). Its
//! session has no screen and takes no keys, so the ask offers no
//! `intervene`; an `intervene` that still comes (to an ask opened before,
//! or typed as text) closes the ask and opens it again, and is never sent
//! as a turn. The interactive ask keeps `intervene` (`runtime_stall`).
use crate::common;
use crate::runtime_support;

use dagq::domain::{AskReason, EventKind};
use runtime_support::headless::*;
use runtime_support::*;

/// Turns that are refused (so the run's recovery job, which does not
/// repair, escalates to a `stalled` ask) until one answers an ask, which
/// finishes the task.
fn refused_until_answered() -> String {
    format!(
        r#"case "$PROMPT" in
"answer to ask "*) {FINISH} ;;
*) denied; say refused ;;
esac"#
    )
}

/// The `stall_resolved` of the ask `id`.
fn resolved_of(detail: &dagq::domain::TaskDetail, id: AskId) -> Vec<&Value> {
    payloads(detail, "stall_resolved")
        .into_iter()
        .filter(|p| p["detection"] == "ask" && p["ask_id"] == json!(id))
        .collect()
}

/// Whether a request of a turn names `text`.
fn requested(run: &TaskRun, text: &str) -> bool {
    let turns = Path::new(run.run_dir().unwrap()).join("turns");
    fs::read_dir(&turns).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            let name = entry.file_name().into_string().unwrap_or_default();
            name.starts_with("request")
                && fs::read_to_string(entry.path()).is_ok_and(|content| content.contains(text))
        })
    })
}

/// Acceptance (1) and (2): the headless run's `stalled` ask offers `wait`,
/// `stop` and `propose`, no `intervene`, and its question has a person read
/// the turns before answering. `intervene` answered anyway closes it with
/// one `answered_intervene` and opens a second ask at once with the same
/// options, sending nothing to the session; `stop` to that one is applied
/// as before.
#[test]
fn an_intervene_answer_closes_the_headless_stalled_ask_and_opens_another() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &refused_until_answered());
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let first = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert_eq!(first.options, ["wait", "stop", "propose"]);
    assert!(!first.question.contains("intervene"), "{}", first.question);
    assert!(
        first
            .question
            .contains("Before answering, read what its turns did"),
        "{}",
        first.question
    );
    SqliteQueue::open(&db)
        .unwrap()
        .answer(first.id, "intervene")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        stalled_asks(queue).len() == 2
    });
    let queue = SqliteQueue::open(&db).unwrap();
    let closed = queue.read_ask(first.id).unwrap();
    assert!(closed.closed_at.is_some(), "{closed:?}");
    assert_eq!(closed.answer.as_deref(), Some("intervene"));
    let second = stalled_asks(&queue).remove(1);
    assert!(second.is_open(), "{second:?}");
    assert_eq!(second.options, ["wait", "stop", "propose"]);
    assert_eq!(second.reason_category, first.reason_category);
    assert!(
        second.question.contains(&format!(
            "stalled ask {} was answered `intervene`",
            first.id
        )),
        "{}",
        second.question
    );
    // Not asked a third time.
    await_passes(&passes, SOME_PASSES);
    assert_eq!(stalled_asks(&queue).len(), 2);
    let asked = detail(&db);
    let ends = resolved_of(&asked, first.id);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "answered_intervene");
    assert_eq!(ends[0]["reopened"], true);
    assert!(!requested(&asked.runs[0], "intervene"));
    assert!(payloads(&asked, "turn_requested").is_empty());

    SqliteQueue::open(&db)
        .unwrap()
        .answer(second.id, "stop")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(stub_calls(run).len(), 1, "{:?}", stub_calls(run));
    let ends = resolved_of(&detail, second.id);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "answered_stop");
    assert_eq!(resolved_of(&detail, first.id).len(), 1);
}

/// Acceptance (2): `intervene` typed as text is no instruction either; the
/// ask opened again takes an instruction, sent as the session's next turn,
/// and the run lands.
#[test]
fn an_intervene_text_is_not_sent_and_the_next_ask_takes_an_instruction() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &refused_until_answered());
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) =
        supervise_thread(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let first = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(first.id, "intervene: I will look at it")
        .unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        stalled_asks(queue).len() == 2
    });
    let second = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(1);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(second.id, "go on and finish")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let last = outcome["runs"].as_array().unwrap().last().unwrap();
    assert_eq!(last["status"], "integrated", "{outcome}");
    let detail = detail(&db);
    let run = &detail.runs[0];
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[1].contains(&format!("answer to ask {}: go on and finish", second.id)),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|call| call.contains("I will look")));
    let outcomes: Vec<&Value> = resolved_of(&detail, first.id)
        .into_iter()
        .chain(resolved_of(&detail, second.id))
        .map(|p| &p["outcome"])
        .collect();
    assert_eq!(
        outcomes,
        [&json!("answered_intervene"), &json!("answered_instruction")]
    );
}

/// What an `idle_process` ask opened before task 1179 said of the session.
const IDLE_PROCESS_SITUATION: &str = "The session of run r (task 2) has processes that have used almost no CPU time for over 300s (alert: idle_process): pid 7: sleep 600. And the recovery job could not repair it.";

/// Such an ask's question, with its answers of then.
const IDLE_PROCESS_QUESTION: &str = "The session of run r (task 2) has processes that have used almost no CPU time for over 300s (alert: idle_process): pid 7: sleep 600. And the recovery job could not repair it.\nAnswer `wait` to leave the session alone, or `intervene` to step in yourself (read the screen, stop the processes, type an instruction; see the dagq-recover skill). This ask closes itself once the session moves on.";

/// Where the supervisor that died with the headless run's `stalled` ask
/// answered `intervene` had got to.
#[derive(Clone, Copy)]
enum Died {
    /// A supervisor before task 1179 applied it as a person stepping in
    /// (`answered_intervene`, the ask left open): or one after it, which
    /// stopped between recording the outcome and closing the ask.
    Held,
    /// It recorded the outcome and closed the ask, and stopped before the
    /// next ask opened.
    Closed,
    /// It died with the ask unanswered; a person answered `intervene` and
    /// closed it while no supervisor watched.
    ClosedByPerson,
}

/// Acceptance (3): the supervisor that adopts the run opens the next ask
/// once, records the answered ask's outcome no second time, and does not
/// hold the run as a person's: the next ask's instruction lands it.
fn adopted_intervene(died: Died) {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
"answer to ask "*) {FINISH} ;;
*) say working ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    // The wrapper writes the idle marker after `turn_finished`: a marker
    // written after the next ask opened would read as a turn the session
    // took since, and close that ask.
    let marker = run.idle_marker_path().unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_finished").is_empty() && marker.exists()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Stalled,
            task_id: None,
            run_id: Some(run.id().clone()),
            // An `idle_process` escalation, as asked before task 1179.
            question: IDLE_PROCESS_QUESTION.into(),
            options: vec!["wait".into(), "intervene".into(), "stop".into()],
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    queue.answer(ask.id, "intervene").unwrap();
    let mut end = json!({
        "phase": "session",
        "detection": "ask",
        "threshold": "idle_without_receipt_secs",
        "threshold_secs": 1,
        "detected_after_secs": 0,
        "outcome": "answered_intervene",
        "resolved_after_secs": 0,
        "ask_id": ask.id,
    });
    if let Died::Closed = died {
        end["reopened"] = json!(true);
    }
    if !matches!(died, Died::ClosedByPerson) {
        queue
            .record_runtime_event(run.id(), EventKind::StallResolved, end)
            .unwrap();
    }
    if !matches!(died, Died::Held) {
        queue.close_ask(ask.id).unwrap();
    }
    age_lease(&db, &run, 31);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        stalled_asks(queue).len() == 2
    });
    let queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let next = stalled_asks(&queue).remove(1);
    assert_eq!(next.options, ["wait", "stop", "propose"]);
    // What the previous ask said of the session stands; its answers are
    // the headless ones.
    assert!(
        next.question.starts_with(IDLE_PROCESS_SITUATION),
        "{}",
        next.question
    );
    assert!(
        next.question
            .contains(&format!("stalled ask {} was answered `intervene`", ask.id)),
        "{}",
        next.question
    );
    assert!(
        !next.question.contains("`intervene` to step in"),
        "{}",
        next.question
    );
    assert!(
        next.question
            .contains("`stop` to have the supervisor end the session")
    );
    // Opened once.
    await_passes(&passes, SOME_PASSES);
    assert_eq!(stalled_asks(&queue).len(), 2);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(next.id, "go on")
        .unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let last = outcome["runs"].as_array().unwrap().last().unwrap();
    assert_eq!(last["status"], "integrated", "{outcome}");
    let detail = detail(&db);
    assert_landed_run(&detail.runs[0], &repo, &base);
    // Opened at once: neither nudged nor sent to a recovery job first.
    assert!(payloads(&detail, "stall_nudged").is_empty());
    assert!(payloads(&detail, "recovery_requested").is_empty());
    let calls = stub_calls(&detail.runs[0]);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        !calls.iter().any(|call| call.contains("intervene")),
        "{calls:?}"
    );
    let ends = resolved_of(&detail, ask.id);
    assert_eq!(ends.len(), 1, "{ends:?}");
    let ends = resolved_of(&detail, next.id);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "answered_instruction");
}

/// Task 1179 (3): an `intervene` applied and left open (by a supervisor
/// before task 1179, or one that stopped before closing it).
#[test]
fn an_adopter_reopens_a_headless_stalled_ask_left_answered_intervene() {
    adopted_intervene(Died::Held);
}

/// Task 1179 (3): the ask closed and the next one not opened yet.
#[test]
fn an_adopter_opens_the_next_ask_of_a_closed_intervene_once() {
    adopted_intervene(Died::Closed);
}

/// Task 1179 (3): an ask a person answered `intervene` and closed while no
/// supervisor watched holds nothing either: its outcome is recorded once
/// and the next ask opens.
#[test]
fn an_adopter_opens_the_next_ask_of_an_intervene_a_person_closed() {
    adopted_intervene(Died::ClosedByPerson);
}
