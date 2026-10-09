//! Runtime tests: a session idle with a receipt for an older commit is asked
//! once to rewrite it (task 357).
use crate::runtime_support;

use runtime_support::*;

/// Commit a second change after the receipt, leaving it for the older commit.
const COMMIT_AGAIN: &str =
    "printf 'more\\n' > more.txt && git add more.txt && git commit -q -m more";

/// The `case` pattern of the prompt of a turn that asks for the receipt's
/// rewrite.
const NUDGE: &str = "*'cannot accept a receipt for another commit'*";

/// The requests to rewrite its receipt that `run`'s worker got.
fn nudges(run: &TaskRun) -> Vec<String> {
    session_texts(run)
        .into_iter()
        .filter(|text| text.contains("cannot accept a receipt for another commit"))
        .collect()
}

/// The script of a worker whose turn that asks for the receipt's rewrite
/// runs `nudged`, and whose other turns run `otherwise` (goal 92).
fn on_nudge(nudged: &str, otherwise: &str) -> String {
    format!("case \"$PROMPT\" in\n{NUDGE}) {nudged}\n;;\n*) {otherwise}\n;;\nesac")
}

/// Task 205's worker: it wrote its receipt, committed again and went idle.
/// The supervisor asks it once to rewrite the receipt for its clean HEAD;
/// it does, and the run goes on to validation and review like any other.
#[test]
fn a_session_idle_with_a_stale_receipt_is_asked_once_and_the_rewrite_goes_on() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &on_nudge(
            "receipt \"$(git rev-parse HEAD)\"",
            &format!(
                "commit work; old=\"$(git rev-parse HEAD)\"; receipt \"$old\"; {COMMIT_AGAIN}"
            ),
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = &detail.runs[0];
    let head = run.result_commit().unwrap().to_string();
    let old = git_out(&repo, &["rev-parse", &format!("{head}~1")]);
    let texts = nudges(run);
    assert_eq!(texts.len(), 1, "{texts:?}");
    for expected in [
        run.id().to_string(),
        format!("names commit {old} while the clean worktree HEAD is {head}"),
        format!("rewrite the receipt at {}", run.receipt_path().unwrap()),
        "Do all of this in this turn.".to_owned(),
    ] {
        assert!(
            texts[0].contains(&expected),
            "{expected:?} not in {}",
            texts[0]
        );
    }
    let nudged = payloads(&detail, "stale_receipt_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "session");
    assert_eq!(nudged[0]["receipt_commit"], old);
    assert_eq!(nudged[0]["head"], head);
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "session", "outcome": "rewritten"})]
    );
    let repaired = payloads(&detail, "auto_repaired");
    assert_eq!(repaired.len(), 1, "{repaired:?}");
    assert_eq!(repaired[0]["repair"], "receipt_rewrite_requested");
    assert_eq!(repaired[0]["conditions"]["receipt_commit"], old);
    assert_eq!(repaired[0]["conditions"]["head"], head);
    assert_eq!(repaired[0]["detail"]["phase"], "session");
    // The stall watch took no part in it.
    assert!(payloads(&detail, "stall_nudged").is_empty());
}

/// A session that answers the request without rewriting its receipt is not
/// asked again: the run goes on as before, and validation rejects the
/// receipt for the older commit.
#[test]
fn a_stale_receipt_left_as_it_is_goes_on_as_before() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &on_nudge(
            ":",
            &format!("commit work; receipt \"$(git rev-parse HEAD)\"; {COMMIT_AGAIN}"),
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(nudges(&detail.runs[0]).len(), 1);
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "session", "outcome": "unchanged"})]
    );
    let validated = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(validated[0]["accepted"], false, "{}", validated[0]);
    assert!(
        validated[0].to_string().contains("commit_mismatch"),
        "{}",
        validated[0]
    );
}

/// Task 205 itself: the resumed session rebased onto main (a clean head on
/// top of it) and went idle with the receipt still naming the old commit.
/// Asked once, it rewrites the receipt and the attempt resolves the run,
/// which lands.
#[test]
fn a_resumed_session_idle_after_its_rebase_with_the_old_receipt_is_asked_once() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    backend.resume_script_for(
        2,
        &on_nudge(
            "receipt \"$(git rev-parse HEAD)\"",
            "await_message; resolve",
        ),
    );
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &first_landed);
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    assert_eq!(nudges(&landed).len(), 1);
    let nudged = payloads(&detail, "stale_receipt_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert_eq!(nudged[0]["phase"], "resume");
    assert_eq!(nudged[0]["attempt"], 1);
    assert_eq!(nudged[0]["receipt_commit"], json!(run.result_commit()));
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "resume", "attempt": 1, "outcome": "rewritten"})]
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "resolved");
}

/// A resumed session's turn of its request (`await_message`): the first
/// attempt rebases and leaves the receipt for the old commit, a later one
/// writes the receipt for HEAD.
const RESOLVES_ON_THE_SECOND_ATTEMPT: &str = "await_message; mark=\"$(dirname \"$RECEIPT\")/attempted\"\n\
    if [ -f \"$mark\" ]; then receipt \"$(git rev-parse HEAD)\"; else : > \"$mark\"; resolve; fi";

/// A resumed session asked to rewrite its stale receipt that asks a
/// `worker_question` right after the request and waits for the answer
/// outside its slot. `rewrite` says whether it rewrites the receipt during
/// that wait; left as it was, the attempt ends unresolved and the next one
/// resolves the run. Returns the run's events once it landed.
fn stale_request_across_a_wait(rewrite: bool) -> dagq::domain::TaskDetail {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    let during_wait = if rewrite {
        "receipt \"$(git rev-parse HEAD)\""
    } else {
        ":"
    };
    // The turn of the request asks; the turn of the answer does nothing.
    backend.resume_script_for(
        2,
        &format!(
            r#"case "$PROMPT" in
{NUDGE})
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Rewrite it?' --cmux /usr/bin/true > /dev/null || exit 70
{during_wait}; : > "$(dirname "$RECEIPT")/waiting" ;;
"answer to ask "*) ;;
*) {RESOLVES_ON_THE_SECOND_ATTEMPT} ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let _waiting = crate::common::within(crate::common::STEP_LIMIT, "supervise to return");
            supervise_with(&db, &repo, &backend, &supervise_options(1, true))
        })
    };
    wait_until(&db, Duration::from_secs(60), |_| {
        !events_of(&db, run.id(), "run_waiting_started").is_empty()
    });
    // Past the second of the request and of the rewrite: the return from
    // the wait restarts the request's clock later than both. The session
    // marks `waiting` after the rewrite (or not) and its idle.
    let waiting = Path::new(run.receipt_path().unwrap()).with_file_name("waiting");
    wait_until(&db, Duration::from_secs(60), |_| waiting.is_file());
    await_second_after(modified_second(&waiting));
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| {
            ask.run_id.as_ref() == Some(run.id())
                && ask.kind == AskKind::WorkerQuestion
                && ask.is_open()
        })
        .map(|ask| ask.id)
        .unwrap();
    queue.answer(ask, "yes").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    assert_eq!(nudges(&detail.runs[0]).len(), 1);
    let kinds = event_kinds(&detail);
    let regained = kinds.iter().position(|k| *k == "run_slot_regained");
    let resolved = kinds.iter().position(|k| *k == "stale_receipt_resolved");
    assert!(regained.is_some() && regained < resolved, "{kinds:?}");
    // The session ended its turn right after the answer, so the request is
    // settled by that idle, not by its 120-second timeout: a restart of
    // the request's clock after the answer's send left that idle unseen
    // (task 931).
    let at = |kind: &str| {
        let event = detail.events.iter().find(|e| e.kind == kind).unwrap();
        dagq::domain::stats::timestamp_millis(&event.created_at).unwrap()
    };
    let settled_ms = at("stale_receipt_resolved") - at("ask_delivered");
    assert!(
        settled_ms < 60_000,
        "settled {settled_ms} ms after the answer"
    );
    detail
}

/// A receipt rewritten while the run waited for the answer to its
/// question counts as `rewritten` once it is back in its slot: the return
/// restarts the request's clock (ADR-0071 decision 15) but not when it was
/// typed, from which the outcome is judged (`stale::tests`). This keeps the
/// wiring of that restart in the resume.
#[test]
fn a_stale_receipt_rewritten_during_a_wait_is_rewritten() {
    let detail = stale_request_across_a_wait(true);
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "resume", "attempt": 1, "outcome": "rewritten"})]
    );
}

/// A receipt left as it was across the wait stays `unchanged`: the attempt
/// ends unresolved without being asked again, and the next attempt
/// resolves the run.
#[test]
fn a_stale_receipt_left_during_a_wait_is_unchanged() {
    let detail = stale_request_across_a_wait(false);
    assert_eq!(
        payloads(&detail, "stale_receipt_resolved"),
        [&json!({"phase": "resume", "attempt": 1, "outcome": "unchanged"})]
    );
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 2, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["outcome"], "unresolved");
    assert_eq!(finished[1]["outcome"], "resolved");
}
