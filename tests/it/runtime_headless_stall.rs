//! Runtime tests: the `stalled` ask of a headless worker (task 1179). Its
//! session has no screen and takes no keys, so the ask offers no
//! `intervene`; an `intervene` that still comes (to an ask opened before,
//! or typed as text) closes the ask and opens it again, and is never sent
//! as a turn. Since task 1437 every worker run is headless, so no
//! `stalled` ask offers `intervene` any more; the stall's `wait` answer,
//! the cap on its recovery jobs, an adopted job's `wait` and a job's
//! `resume` are checked here on a headless run, as they were on an
//! interactive one before.
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

/// A headless stall's recovery job whose verdict the runtime does not act
/// on asks a person, as an interactive stall's does
/// (`runtime_stall_recovery::a_failed_stalled_job_raises_the_stalled_ask`
/// and `a_recovery_job_of_low_confidence_raises_one_stalled_ask_that_closes_when_the_session_moves`,
/// interactive, goal 92): a job that failed escalates with
/// `recovery_failed` and nothing applied, and a `repair` of low confidence
/// is not applied but asked, with its actions as the recommendation and
/// its options added. Nothing is sent to the session; `stop` ends the run.
#[test]
fn a_failed_or_unsure_recovery_job_of_a_headless_stall_asks_a_person() {
    let unsure = recovery(json!({
        "verdict": "repair",
        "confidence": "low",
        "diagnosis": "maybe a missing permission",
        "actions": [{"action": "send_instruction", "instruction": "go on"}],
        "options": ["go on"],
    }));
    for (job, failed) in [
        ("echo broken >&2; exit 3".to_owned(), true),
        (unsure, false),
    ] {
        let (dir, repo, db, backend) = headless_fixture(&[]);
        set_turns(dir.path(), &refused_until_answered());
        let backend = Arc::new(backend);
        let (_reviewer, supervisor) =
            supervise_thread(&db, &repo, backend.clone(), Default::default(), &[job]);
        // The job's end is recorded after the ask it opened.
        wait_until(&db, common::STEP_LIMIT, |queue| {
            !stalled_asks(queue).is_empty()
                && !payloads(&queue.show(TASK).unwrap(), "recovery_finished").is_empty()
        });
        let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
        assert_eq!(ask.reason_category, AskReason::RecoveryFailed, "{ask:?}");
        let asked = detail(&db);
        let finished = payloads(&asked, "recovery_finished");
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert_eq!(finished[0]["escalated"], true);
        assert_eq!(finished[0]["ask_id"], json!(ask.id));
        assert_eq!(
            finished[0]["outcome"] == "job_failed",
            failed,
            "{finished:?}"
        );
        assert!(payloads(&asked, "auto_repaired").is_empty());
        assert!(payloads(&asked, "turn_requested").is_empty());
        if !failed {
            assert!(ask.options.contains(&"go on".to_owned()), "{ask:?}");
            for part in [
                "confidence low",
                "Recommended: [{\"action\":\"send_instruction\"",
            ] {
                assert!(ask.question.contains(part), "{part}: {}", ask.question);
            }
        }

        SqliteQueue::open(&db)
            .unwrap()
            .answer(ask.id, "stop")
            .unwrap();
        let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
        backend.join();
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        let detail = detail(&db);
        assert_eq!(detail.runs[0].status(), RunStatus::Failed);
        assert_eq!(stub_calls(&detail.runs[0]).len(), 1);
    }
}

/// Turns that end with neither a receipt nor a question, so the run is
/// nudged twice and then goes to its recovery job (`stalled`, reason
/// `turn_without_receipt`), until one whose prompt matches the shell
/// pattern `finish`, which finishes the task.
fn thinking_until(finish: &str) -> String {
    format!(
        r#"case "$PROMPT" in
{finish}) {FINISH} ;;
*) say thinking ;;
esac"#
    )
}

/// The run of the fixture's task lands from `supervisor`: its outcome has
/// no errors and ends with the run integrated on top of `base`.
fn landed(
    db: &Path,
    repo: &Path,
    base: &str,
    backend: &TestWorkspace,
    supervisor: thread::JoinHandle<Result<Value>>,
) -> dagq::domain::TaskDetail {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let last = outcome["runs"].as_array().unwrap().last().unwrap();
    assert_eq!(last["status"], "integrated", "{outcome}");
    let detail = detail(db);
    assert_landed_run(&detail.runs[0], repo, base);
    detail
}

/// Who closed the `stalled` ask answered `wait`.
#[derive(Clone, Copy)]
enum WaitClosed {
    /// The supervisor, as it applied the answer.
    BySupervisor,
    /// The one who answered, in the same step (as the inbox may close the
    /// ask it answers): the supervisor finds it closed.
    ByAnswerer,
}

/// A headless run nudged twice whose recovery job escalated gets a
/// `stalled` ask, answered `wait`: the ask is closed and its outcome
/// recorded once as `answered_wait`, and no ask, nudge or job follows
/// while the session takes no turn. Once it ends another turn without a
/// receipt, a second ask opens at once, with no third nudge and no second
/// job; its instruction is the next turn, and the run lands. Moved from the
/// interactive `runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied`
/// and `a_stalled_ask_closed_after_wait_is_asked_again`, which task 1437
/// deleted.
fn answered_wait(closed: WaitClosed) {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &thinking_until(r#""answer to ask "*"#));
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = stalled_asks(&queue).remove(0);
    let run = detail(&db).runs[0].clone();
    assert_eq!(first.run_id.as_ref(), Some(run.id()));
    match closed {
        WaitClosed::BySupervisor => {
            queue.answer(first.id, "wait").unwrap();
        }
        WaitClosed::ByAnswerer => {
            let answered = queue.close_stalled_asks(run.id(), "wait").unwrap();
            assert_eq!(answered.len(), 1, "{answered:?}");
        }
    }
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !resolved_of(&queue.show(TASK).unwrap(), first.id).is_empty()
    });
    let answered = queue.read_ask(first.id).unwrap();
    assert!(answered.closed_at.is_some(), "{answered:?}");
    assert_eq!(answered.answer.as_deref(), Some("wait"));
    // Not asked again while the session takes no turn.
    await_passes(&passes, SOME_PASSES);
    assert_eq!(stalled_asks(&queue).len(), 1);
    assert_eq!(stub_calls(&run).len(), 3, "{:?}", stub_calls(&run));

    write_turn_request(&run, "look at it again", "test turn");
    wait_until(&db, common::STEP_LIMIT, |queue| {
        stalled_asks(queue).len() == 2
    });
    let second = stalled_asks(&queue).remove(1);
    assert!(second.is_open(), "{second:?}");
    assert_eq!(second.options, ["wait", "stop", "propose"]);
    let asked = detail(&db);
    // Asked at once: neither nudged nor sent to a recovery job again.
    assert_eq!(payloads(&asked, "stall_nudged").len(), 2);
    let requested = payloads(&asked, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["reason"], "turn_without_receipt");
    assert_eq!(stub_calls(&run).len(), 4, "{:?}", stub_calls(&run));

    queue.answer(second.id, "go on and finish").unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let ends = resolved_of(&detail, first.id);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "answered_wait");
    let ends = resolved_of(&detail, second.id);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["outcome"], "answered_instruction");
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
}

/// [`answered_wait`] with the ask the supervisor closes.
#[test]
fn a_headless_stalled_ask_answered_wait_is_closed_and_asked_again_after_the_next_turn() {
    answered_wait(WaitClosed::BySupervisor);
}

/// [`answered_wait`] with the ask closed by the one who answered it.
#[test]
fn a_headless_stalled_ask_closed_after_wait_is_asked_again_after_the_next_turn() {
    answered_wait(WaitClosed::ByAnswerer);
}

/// A headless stall's recovery job that answers `wait` holds the alert for
/// its `recheck_after_secs`; the alert gets three jobs, and the next look
/// asks a person, saying so, without a fourth. Moved from the interactive
/// `runtime_stall_recovery::a_stall_past_its_three_recovery_jobs_is_asked_without_a_fourth`,
/// which task 1437 deleted.
#[test]
fn a_headless_stall_past_its_three_recovery_jobs_is_asked_without_a_fourth() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &thinking_until(r#""answer to ask "*"#));
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let wait = repair(json!({"action": "wait", "recheck_after_secs": 1}), "slow");
    let (reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        Default::default(),
        &[wait.clone(), wait.clone(), wait],
    );
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let ask = stalled_asks(&SqliteQueue::open(&db).unwrap()).remove(0);
    assert!(
        ask.question
            .contains("the recovery job ran 3 times for this alert already"),
        "{}",
        ask.question
    );
    assert_eq!(ask.reason_category, AskReason::RecoveryFailed);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "go on and finish")
        .unwrap();
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    assert_eq!(reviewer.triage_prompts().len(), 3);
    let requested = payloads(&detail, "recovery_requested");
    let attempts: Vec<&Value> = requested.iter().map(|r| &r["attempt"]).collect();
    assert_eq!(attempts, [&json!(1), &json!(2), &json!(3)], "{requested:?}");
    let finished = payloads(&detail, "recovery_finished");
    assert_eq!(finished.len(), 4, "{finished:?}");
    assert!(
        finished[..3]
            .iter()
            .all(|f| f["applied"] == json!(["wait"])),
        "{finished:?}"
    );
    assert_eq!(finished[3]["ask_id"], json!(ask.id));
    assert_eq!(finished[3]["escalated"], true);
    // The three jobs and the ask follow the two nudges, with no other.
    assert_eq!(payloads(&detail, "stall_nudged").len(), 2);
}

/// The supervisor that nudged a headless run twice and whose recovery job
/// answered `wait` (`recheck_at_ms` an hour on) died: its adopter, which
/// sees the session's turn end after the nudges, neither nudges nor starts
/// a job nor asks while the wait holds. A turn that writes the receipt then
/// lands the run. Moved from the interactive
/// `runtime_stall_recovery::an_adopted_stall_whose_job_waits_gets_no_second_nudge_job_or_ask`,
/// which task 1437 deleted.
#[test]
fn an_adopted_headless_stall_whose_job_waits_gets_no_nudge_job_or_ask() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &format!(
            r#"case "$PROMPT" in
"write the receipt") {FINISH} ;;
*) await_file "$RUN_DIR/go"; say thinking ;;
esac"#
        ),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let queue = SqliteQueue::open(&db).unwrap();
    // Its two nudges; then the turn after them ends without a receipt, and
    // its recovery job answers `wait`.
    for _ in 0..2 {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::StallNudged,
                json!({"phase": "session", "idle_secs": 0, "threshold_secs": 1}),
            )
            .unwrap();
    }
    fs::write(Path::new(run.run_dir().unwrap()).join("go"), "").unwrap();
    let marker = run.idle_marker_path().unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "turn_finished").is_empty() && marker.exists()
    });
    let recheck_at_ms = (SystemTime::now() + Duration::from_secs(3600))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for (kind, payload) in [
        (
            EventKind::RecoveryRequested,
            json!({"alert": "stalled", "reason": "turn_without_receipt", "attempt": 1, "idle_secs": 0, "nudges": 2}),
        ),
        (
            EventKind::RecoveryFinished,
            json!({"alert": "stalled", "reason": "turn_without_receipt", "attempt": 1, "verdict": "repair", "confidence": "high", "applied": ["wait"], "escalated": false, "recheck_at_ms": recheck_at_ms}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(&db, &run, 31);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !adoption_events(&queue.show(TASK).unwrap()).is_empty()
    });
    // The turn ended before the adopter started: its passes since are
    // what would nudge, start a job or ask.
    await_passes(&passes, SOME_PASSES);
    let held = detail(&db);
    assert!(stalled_asks(&queue).is_empty());
    assert_eq!(payloads(&held, "stall_nudged").len(), 2);
    assert_eq!(payloads(&held, "recovery_requested").len(), 1);
    assert!(payloads(&held, "turn_requested").is_empty());
    assert_eq!(stub_calls(&run).len(), 1, "{:?}", stub_calls(&run));

    write_turn_request(&run, "write the receipt", "test turn");
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 2);
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
    assert!(stalled_asks(&queue).is_empty());
}

/// A headless stall's recovery job that answers `resume` parks the run as
/// `needs_session` with its instruction (`recovery_parked`), the session is
/// asked to exit, and the supervisor resumes the run with the instruction
/// in the resume's request; the resumed session writes its receipt and the
/// run lands, with no ask. Moved from the interactive
/// `runtime_stall_recovery::a_stalled_session_the_job_resumes_is_parked_resumed_and_lands`,
/// which task 1437 deleted.
#[test]
fn a_headless_stall_the_job_resumes_is_parked_resumed_and_lands() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(
        dir.path(),
        &thinking_until(r#"*"restart the hung tests in a fresh session"*"#),
    );
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor) = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        Default::default(),
        &[repair(
            json!({"action": "resume", "instruction": "restart the hung tests in a fresh session"}),
            "the session hangs on its tests",
        )],
    );
    // A resume not taken would leave the run to a second job and its ask.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.show(TASK).unwrap().task.status() == TaskStatus::Completed
            || !stalled_asks(queue).is_empty()
    });
    assert!(stalled_asks(&SqliteQueue::open(&db).unwrap()).is_empty());
    let detail = landed(&db, &repo, &base, &backend, supervisor);
    let parked = payloads(&detail, "recovery_parked");
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0]["status"], "needs_session");
    assert_eq!(
        parked[0]["instruction"],
        "restart the hung tests in a fresh session"
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["repair"], "resume");
    assert_eq!(payloads(&detail, "resume_started").len(), 1);
    let requested = payloads(&detail, "recovery_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["reason"], "turn_without_receipt");
    // The session exited before the resume, which carried the instruction.
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("recovery_parked", "exit_requested"),
        ("exit_requested", "resume_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    let calls = stub_calls(&detail.runs[0]);
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[3].starts_with("resume "), "{calls:?}");
    assert!(stalled_asks(&SqliteQueue::open(&db).unwrap()).is_empty());
}
