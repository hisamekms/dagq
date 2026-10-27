//! Runtime tests: what the supervisor sends a headless worker's session
//! (an answer, the hold's `continue`, a nudge, a `stalled` ask's
//! instruction, a recovery job's instruction) is its next turn's request,
//! and from the request until that turn ends the idle marker is the turn's
//! before: no nudge, `stalled` ask or recovery job comes of it, and an
//! answer is sent once. A request written while a turn ran is older than
//! that turn's marker. Once the requested turn ends without a receipt, the
//! nudge, the job or the ask follows as before, and a supervisor that takes
//! the run over between the request and the turn's end judges the same
//! from the records. The decision is `turn::tests::a_requested_turn_is_awaited_from_its_request_until_it_ends`'s.
use crate::common;
use crate::runtime_support;

use dagq::domain::{AskReason, EventKind, HOLD_OPTIONS, NewHold};
use runtime_support::headless::*;
use runtime_support::*;

/// Turns the test steers by files next to the stub: turn N asks a
/// question with `ask-N`, waits for `go-N` with `gate-N`, and finishes the
/// task with `finish-N`; otherwise it ends without a receipt.
fn steered(finish: &str) -> String {
    format!(
        r#"CTL="${{0%/*}}"
if [ -f "$CTL/ask-$TURN" ]; then ask "which file"; fi
if [ -f "$CTL/gate-$TURN" ]; then await_file "$CTL/go-$TURN"; fi
if [ -f "$CTL/finish-$TURN" ]; then {finish}; else say thinking; fi"#
    )
}

/// Mark turn `turn` with `what` (`ask`, `gate`, `go` or `finish`).
fn mark(dir: &Path, what: &str, turn: u64) {
    fs::write(dir.join(format!("{what}-{turn}")), "").unwrap();
}

/// Wait until turn `turn` of the task's run started.
fn started(db: &Path, turn: u64) {
    wait_until(db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_started")
            .iter()
            .any(|p| p["turn"] == turn)
    });
}

/// The nudges, `stalled` asks and recovery jobs of the task's run.
fn stall_counts(db: &Path) -> (usize, usize, usize) {
    let detail = detail(db);
    (
        payloads(&detail, "stall_nudged").len(),
        stalled_asks(&SqliteQueue::open(db).unwrap()).len(),
        payloads(&detail, "recovery_requested").len(),
    )
}

/// While turn `turn`, which the supervisor requested, runs past some
/// passes: its marker is still the turn's before, and the counts of
/// [`stall_counts`] stay `counts`.
fn quiet_while(db: &Path, passes: &AtomicU64, turn: u64, counts: (usize, usize, usize)) {
    started(db, turn);
    await_passes(passes, SOME_PASSES);
    let detail = detail(db);
    assert!(
        !payloads(&detail, "turn_finished")
            .iter()
            .any(|p| p["turn"] == turn),
        "turn {turn} ended early"
    );
    assert_eq!(stall_counts(db), counts, "{:?}", event_kinds(&detail));
}

/// The index among the run's events of the first of `kind` whose payload
/// has `key` `value`.
fn index_of(detail: &dagq::domain::TaskDetail, kind: &str, key: &str, value: Value) -> usize {
    detail
        .events
        .iter()
        .position(|e| e.kind.as_str() == kind && e.payload[key] == value)
        .unwrap_or_else(|| panic!("no {kind} with {key} {value}"))
}

/// The supervisor's outcome: no errors, and the run integrated.
fn integrated(supervisor: thread::JoinHandle<Result<Value>>, backend: &TestWorkspace) {
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let last = outcome["runs"].as_array().unwrap().last().unwrap();
    assert_eq!(last["status"], "integrated", "{outcome}");
}

/// Hold the run in an authentication hold ask, as the runtime does when a
/// login stops it, and answer it `done` while turn 1 runs: the session's
/// `continue` is written while the turn runs.
fn continue_during_the_first_turn(db: &Path) {
    started(db, 1);
    let run = detail(db).runs[0].clone();
    let mut queue = SqliteQueue::open(db).unwrap();
    let hold = queue
        .hold(NewHold {
            reason_category: AskReason::Authentication,
            subject: None,
            run_id: Some(run.id().clone()),
            job: None,
            question: "the login ran out".into(),
            options: HOLD_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
            asked_by: "supervisor".into(),
        })
        .unwrap()
        .ask;
    queue.answer(hold.id, "done").unwrap();
    wait_until(db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "hold_continue_sent").is_empty()
    });
}

/// The hold's `continue`, written while turn 1 runs, is turn 2: turn 1's
/// marker, written after it, gets no nudge while turn 2 runs. Turn 2 ends
/// without a receipt and is nudged; the nudge's turn lands the run.
fn a_continue_written_during_a_turn(dir: &Path, db: &Path, passes: &AtomicU64) {
    continue_during_the_first_turn(db);
    mark(dir, "go", 1);
    quiet_while(db, passes, 2, (0, 0, 0));
    mark(dir, "go", 2);
}

/// The nudge's turn: while it runs, the marker of the turn it nudged gets
/// no second nudge, ask or job; once it ends without a receipt, the second
/// nudge follows, and its turn lands the run.
#[test]
fn a_nudges_turn_is_awaited_and_the_next_nudge_follows_its_end() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &steered(FINISH));
    mark(dir.path(), "gate", 2);
    mark(dir.path(), "finish", 3);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    quiet_while(&db, &passes, 2, (1, 0, 0));
    mark(dir.path(), "go", 2);
    integrated(supervisor, &backend);
    let detail = detail(&db);
    let nudged = payloads(&detail, "stall_nudged");
    assert_eq!(nudged.len(), 2, "{nudged:?}");
    let second = detail
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::StallNudged)
        .nth(1)
        .unwrap()
        .0;
    assert!(index_of(&detail, "turn_finished", "turn", json!(2)) < second);
    assert_eq!(stub_calls(&detail.runs[0]).len(), 3);
}

/// The hold's `continue` written while turn 1 runs
/// ([`a_continue_written_during_a_turn`]), on a Claude worker.
#[test]
fn a_continue_written_during_a_turn_is_awaited_until_its_turn_ends() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &steered(FINISH));
    for turn in [1, 2] {
        mark(dir.path(), "gate", turn);
    }
    mark(dir.path(), "finish", 3);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    a_continue_written_during_a_turn(dir.path(), &db, &passes);
    integrated(supervisor, &backend);
    let detail = detail(&db);
    let nudged = payloads(&detail, "stall_nudged");
    assert_eq!(nudged.len(), 1, "{nudged:?}");
    assert!(
        index_of(&detail, "turn_finished", "turn", json!(2))
            < index_of(&detail, "stall_nudged", "phase", json!("session"))
    );
}

/// [`a_continue_written_during_a_turn`] on a Codex worker.
#[test]
fn a_continue_written_during_a_codex_turn_is_awaited_until_its_turn_ends() {
    use runtime_support::codex::{FINISH, codex_fixture};
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_turns(dir.path(), &steered(FINISH));
    for turn in [1, 2] {
        mark(dir.path(), "gate", turn);
    }
    mark(dir.path(), "finish", 3);
    let backend = Arc::new(backend);
    let options = SuperviseOptions {
        codex: codex.clone(),
        stall: Some(Default::default()),
        ..supervise_options(4, true)
    };
    let passes = options.passes.clone();
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || {
            let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &options,
            )
        })
    };
    a_continue_written_during_a_turn(dir.path(), &db, &passes);
    integrated(supervisor, &backend);
    let detail = detail(&db);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    assert!(
        index_of(&detail, "turn_finished", "turn", json!(2))
            < index_of(&detail, "stall_nudged", "phase", json!("session"))
    );
}

/// Turn 1 asks and ends; another turn (2) runs when the answer comes, so
/// the answer is written while it runs, once. Turn 2's marker, written
/// after the answer, gets no nudge while the answer's turn (3) runs; that
/// turn ends without a receipt and is nudged, and the nudge's turn lands
/// the run.
#[test]
fn an_answer_written_during_a_turn_is_sent_once_and_awaited_until_its_turn_ends() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &steered(FINISH));
    mark(dir.path(), "ask", 1);
    for turn in [2, 3] {
        mark(dir.path(), "gate", turn);
    }
    mark(dir.path(), "finish", 4);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        payloads(&queue.show(TASK).unwrap(), "turn_finished")
            .iter()
            .any(|p| p["turn"] == 1)
    });
    let run = detail(&db).runs[0].clone();
    write_turn_request(&run, "look again", "test turn");
    started(&db, 2);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.kind == AskKind::WorkerQuestion)
        .unwrap();
    queue.answer(ask.id, "change.txt").unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !payloads(&queue.show(TASK).unwrap(), "ask_delivered").is_empty()
    });
    mark(dir.path(), "go", 2);
    quiet_while(&db, &passes, 3, (0, 0, 0));
    mark(dir.path(), "go", 3);
    integrated(supervisor, &backend);
    let detail = detail(&db);
    let answers: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .filter(|p| p["what"] == format!("answer of ask {}", ask.id))
        .collect();
    assert_eq!(answers.len(), 1, "{answers:?}");
    let calls = stub_calls(&detail.runs[0]);
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(
        calls[2].contains(&format!("answer to ask {}: change.txt", ask.id)),
        "{calls:?}"
    );
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    assert!(
        index_of(&detail, "turn_finished", "turn", json!(3))
            < index_of(&detail, "stall_nudged", "phase", json!("session"))
    );
}

/// After two nudges the recovery job escalates to a `stalled` ask, whose
/// instruction is turn 4: while it runs, no ask, nudge or job comes again.
/// It ends without a receipt: the next recovery job and its ask follow,
/// whose instruction lands the run.
#[test]
fn a_stalled_asks_instruction_is_awaited_and_the_next_job_follows_its_end() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &steered(FINISH));
    mark(dir.path(), "gate", 4);
    mark(dir.path(), "finish", 5);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) =
        supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !stalled_asks(queue).is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = stalled_asks(&queue).remove(0);
    queue.answer(first.id, "look at it again").unwrap();
    quiet_while(&db, &passes, 4, (2, 1, 1));
    mark(dir.path(), "go", 4);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        stalled_asks(queue).len() == 2
    });
    let second = stalled_asks(&queue).remove(1);
    queue.answer(second.id, "go on and finish").unwrap();
    integrated(supervisor, &backend);
    let detail = detail(&db);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 2);
    assert_eq!(payloads(&detail, "recovery_requested").len(), 2);
    assert!(
        index_of(&detail, "turn_finished", "turn", json!(4))
            < index_of(&detail, "recovery_requested", "attempt", json!(2))
    );
}

/// After two nudges the recovery job sends an instruction, turn 4: while
/// it runs, no ask, nudge or job comes again. It ends without a receipt:
/// the next job follows, whose instruction lands the run.
#[test]
fn a_recovery_instruction_is_awaited_and_the_next_job_follows_its_end() {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &steered(FINISH));
    mark(dir.path(), "gate", 4);
    mark(dir.path(), "finish", 5);
    let backend = Arc::new(backend);
    let (_reviewer, supervisor, passes) = supervise_thread_counted(
        &db,
        &repo,
        backend.clone(),
        Default::default(),
        &[
            repair(
                json!({"action": "send_instruction", "instruction": "write the receipt"}),
                "the worker forgot its receipt",
            ),
            repair(
                json!({"action": "send_instruction", "instruction": "write the receipt now"}),
                "the worker forgot its receipt again",
            ),
        ],
    );
    quiet_while(&db, &passes, 4, (2, 0, 1));
    mark(dir.path(), "go", 4);
    integrated(supervisor, &backend);
    let detail = detail(&db);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 2);
    assert_eq!(payloads(&detail, "recovery_requested").len(), 2);
    assert!(
        index_of(&detail, "turn_finished", "turn", json!(4))
            < index_of(&detail, "recovery_requested", "attempt", json!(2))
    );
    assert!(stalled_asks(&SqliteQueue::open(&db).unwrap()).is_empty());
}

/// When the supervisor that adopts the run starts.
#[derive(Clone, Copy, Debug)]
enum Adopted {
    /// While the turn the request was written during still runs.
    BeforeTheTurnEnds,
    /// Once the requested turn runs.
    WhileTheRequestedTurnRuns,
}

/// A supervisor died after writing a request (`what`, recorded as
/// `turn_requested`, and `delivered` its answer's `ask_delivered`) while
/// turn 1 ran; its adopter, started `adopted`, gets no nudge of turn 1's
/// marker while the requested turn (2) runs, and nudges once that turn
/// ends without a receipt; the nudge's turn lands the run.
fn adopted_request(what: &str, answer: bool, adopted: Adopted) {
    let (dir, repo, db, backend) = headless_fixture(&[]);
    set_turns(dir.path(), &steered(FINISH));
    if answer {
        mark(dir.path(), "ask", 1);
    }
    for turn in [1, 2] {
        mark(dir.path(), "gate", turn);
    }
    mark(dir.path(), "finish", 3);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    started(&db, 1);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let what = if answer {
        wait_until(&db, common::STEP_LIMIT, |queue| {
            queue
                .asks(AskQuery::default())
                .unwrap()
                .iter()
                .any(|ask| ask.kind == AskKind::WorkerQuestion)
        });
        let ask = queue
            .asks(AskQuery::default())
            .unwrap()
            .into_iter()
            .find(|ask| ask.kind == AskKind::WorkerQuestion)
            .unwrap();
        queue.answer(ask.id, "change.txt").unwrap();
        let what = format!("answer of ask {}", ask.id);
        request_turn_left_by_a_dead_supervisor(
            &db,
            &run,
            &format!("answer to ask {}: change.txt", ask.id),
            &what,
        );
        queue
            .ask_delivered(ask.id, run.workspace_id().unwrap())
            .unwrap();
        what
    } else {
        request_turn_left_by_a_dead_supervisor(&db, &run, "go on", what);
        what.to_owned()
    };
    age_lease(&db, &run, 31);
    let start = || supervise_thread_counted(&db, &repo, backend.clone(), Default::default(), &[]);
    let (_reviewer, supervisor, passes) = match adopted {
        Adopted::BeforeTheTurnEnds => {
            let (reviewer, supervisor, passes) = start();
            wait_until(&db, common::STEP_LIMIT, |queue| {
                !adoption_events(&queue.show(TASK).unwrap()).is_empty()
            });
            await_passes(&passes, SOME_PASSES);
            assert_eq!(stall_counts(&db), (0, 0, 0));
            mark(dir.path(), "go", 1);
            (reviewer, supervisor, passes)
        }
        Adopted::WhileTheRequestedTurnRuns => {
            mark(dir.path(), "go", 1);
            started(&db, 2);
            start()
        }
    };
    wait_until(&db, common::STEP_LIMIT, |queue| {
        !adoption_events(&queue.show(TASK).unwrap()).is_empty()
    });
    quiet_while(&db, &passes, 2, (0, 0, 0));
    mark(dir.path(), "go", 2);
    integrated(supervisor, &backend);
    let detail = detail(&db);
    assert_landed_run(&detail.runs[0], &repo, &base);
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1, "{adopted:?}");
    assert!(
        index_of(&detail, "turn_finished", "turn", json!(2))
            < index_of(&detail, "stall_nudged", "phase", json!("session"))
    );
    let requested: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .filter(|p| p["what"] == what)
        .collect();
    assert_eq!(requested.len(), 1, "{requested:?}");
}

/// [`adopted_request`]: a `continue` written during turn 1, adopted before
/// that turn ends.
#[test]
fn an_adopter_awaits_the_turn_of_a_continue_written_during_a_turn() {
    adopted_request("continue", false, Adopted::BeforeTheTurnEnds);
}

/// [`adopted_request`]: an answer delivered during turn 1, adopted while
/// its turn runs.
#[test]
fn an_adopter_awaits_the_turn_of_an_answer_written_during_a_turn() {
    adopted_request("", true, Adopted::WhileTheRequestedTurnRuns);
}
