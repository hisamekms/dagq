//! Runtime tests: runs that wait for a person while their live session
//! fixes a `revise` verdict or resolves what parked them (ADR-0071
//! decisions 1 and 15 to 18).
use crate::{common, runtime_support};
use dagq::domain::EventKind;
use dagq::domain::stats::timestamp_millis;

use runtime_support::headless::request_turn_left_by_a_dead_supervisor;
use runtime_support::*;

/// The resume timeout of these tests: long enough for a session to ask
/// once its request arrived on a loaded host.
const STAGE_TIMEOUT: Duration = Duration::from_secs(6);

/// Past the stage timeout, how much longer a question is held: covers the
/// whole second the event's time is not precise to and the moment between
/// the stage's request being typed and its clock starting.
const HELD_MARGIN: Duration = Duration::from_millis(1500);

/// Hold the question that stopped `run`'s stage until its clock, had it run
/// on, would have run out: [`STAGE_TIMEOUT`] and [`HELD_MARGIN`] past the
/// `kind` event, which follows the question and so the request that started
/// the clock; then wait for passes of the supervisor that would have ended
/// the stage. The time the test already spent since then counts, instead of
/// a fixed sleep past the timeout from wherever the test got to (task 1047).
/// Returns how long after that event the hold ended.
fn hold_past_the_stage_timeout(
    db: &Path,
    run: &TaskRun,
    kind: &str,
    passes: &AtomicU64,
) -> Duration {
    let event = SqliteQueue::open(db)
        .unwrap()
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .find(|e| e.kind.as_str() == kind)
        .unwrap_or_else(|| panic!("no {kind} of {}", run.id()));
    let at = UNIX_EPOCH
        + Duration::from_millis(
            u64::try_from(timestamp_millis(&event.created_at).unwrap()).unwrap(),
        );
    let until = at + STAGE_TIMEOUT + HELD_MARGIN;
    if let Ok(left) = until.duration_since(SystemTime::now()) {
        thread::sleep(left);
    }
    await_passes(passes, SOME_PASSES);
    let held = SystemTime::now().duration_since(at).unwrap();
    assert!(held > STAGE_TIMEOUT, "held {held:?} after {kind}");
    held
}

/// A worker that asks a `worker_question` while it revises: its turn of the
/// revise request asks, and the turn of the answer commits the answer.
const REVISE_ASKING_AGENT: &str = concat!(
    r#"answer_to() { printf '%s\n' "$PROMPT" | head -n 1 | tr -d '\n' > "$1"; }"#,
    r#"
case "$PROMPT" in
"answer to ask "*) answer_to answer.txt; git add answer.txt; git commit -q -m answer
  receipt "$(git rev-parse HEAD)" ;;
*) if [ "$TURN" -eq 1 ]; then commit work; receipt "$(git rev-parse HEAD)"
   else "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which line?' --cmux /usr/bin/true > /dev/null || exit 70
   fi ;;
esac
"#
);

/// A worker whose change lands beside the others' (`two.txt`).
const BESIDE_AGENT: &str = r#"
printf 'two\n' > two.txt; git add two.txt; git commit -q -m two
receipt "$(git rev-parse HEAD)"; idle; await_exit
"#;

/// A worker that asks at once and commits the answer it got (`first.txt`)
/// in the turn of the answer.
const ASKING_AGENT: &str = concat!(
    r#"answer_to() { printf '%s\n' "$PROMPT" | head -n 1 | tr -d '\n' > "$1"; }"#,
    r#"
case "$PROMPT" in
"answer to ask "*) answer_to first.txt; git add first.txt; git commit -q -m answer
  receipt "$(git rev-parse HEAD)" ;;
*) "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Which word?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
esac
"#
);

/// A resumed session that asks a `worker_question` once the resolution
/// request arrived (after `$EXIT.gate` when `gated`) and ends its turn,
/// and resolves the conflict in the turn of the answer.
fn resume_asking(gated: bool) -> String {
    let gate = if gated {
        "await_file \"$EXIT.gate\""
    } else {
        ":"
    };
    format!(
        r#"await_message
case "$PROMPT" in
"answer to ask "*) resolve; receipt "$(git rev-parse HEAD)" ;;
*) {gate}
"$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question 'Keep which side?' --cmux /usr/bin/true > /dev/null || exit 70 ;;
esac
"#
    )
}

fn run_of(queue: &mut SqliteQueue, task: i64) -> Option<TaskRun> {
    queue.show(TaskId::new(task)).unwrap().runs.last().cloned()
}

fn open_ask_of(queue: &mut SqliteQueue, run: &TaskRun, kind: AskKind) -> Option<AskId> {
    queue
        .asks(AskQuery::default())
        .unwrap()
        .into_iter()
        .find(|ask| ask.run_id.as_ref() == Some(run.id()) && ask.kind == kind && ask.is_open())
        .map(|ask| ask.id)
}

fn kinds_of(db: &Path, task: i64) -> Vec<String> {
    SqliteQueue::open(db)
        .unwrap()
        .show(TaskId::new(task))
        .unwrap()
        .events
        .into_iter()
        .map(|e| e.kind.to_string())
        .collect()
}

fn at(kinds: &[String], kind: &str) -> usize {
    kinds
        .iter()
        .position(|k| k == kind)
        .unwrap_or_else(|| panic!("no {kind} in {kinds:?}"))
}

fn supervise_in_thread(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    reviewer: Option<&Arc<TestReviewer>>,
    options: SuperviseOptions,
) -> thread::JoinHandle<Result<Value>> {
    // The service the supervisor starts takes the test as its owner.
    let dagq = common::service::owned_executable(db.parent().unwrap());
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    let reviewer = reviewer.cloned();
    thread::spawn(move || {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        match reviewer {
            Some(reviewer) => runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                &dagq,
                &options,
            ),
            None => supervise_with(&db, &repo, &backend, &options),
        }
    })
}

/// A session that asks a `worker_question` while it revises waits for the
/// answer outside the one slot (decision 1): another task is claimed and
/// lands meanwhile, and the revise's clock stops, so holding the question
/// past the resume timeout neither ends the revise nor asks a person
/// (decisions 15 and 16). The answer ends the wait; back in its slot the
/// run gets the answer typed, rewrites its receipt and lands.
#[test]
fn a_question_while_revising_waits_outside_the_slot_with_its_clock_stopped() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let mut backend = TestWorkspace::new(&db, false, REVISE_ASKING_AGENT);
    backend.resume_timeout = STAGE_TIMEOUT;
    backend.script_for(2, BESIDE_AGENT);
    let backend = Arc::new(backend);
    let options = supervise_options(1, true);
    let passes = options.passes.clone();
    // The first revise, the second task's pass while the first waits, and
    // the pass of the first once answered.
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["say which line"], "one gap"),
        verdict("pass", &[], "fine"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = supervise_in_thread(&db, &repo, &backend, Some(&reviewer), options);
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|run| run.status() == RunStatus::Integrated)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::AwaitingIntegration);
    let ask = open_ask_of(&mut queue, &first, AskKind::WorkerQuestion).unwrap();
    let started = events_of(&db, first.id(), "run_waiting_started");
    assert_eq!(
        started,
        vec![
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "revise",
                    "status": "awaiting_integration", "waiting": 1, "limit": 4})
        ]
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        status["waiting"][0]["run_id"],
        json!(first.id()),
        "{status}"
    );
    assert_eq!(status["waiting"][0]["phase"], "revise");
    assert_eq!(status["waiting"][0]["state"], "waiting");
    // Held past the resume timeout: the revise goes on waiting.
    hold_past_the_stage_timeout(&db, &first, "run_waiting_started", &passes);
    let kinds = kinds_of(&db, 1);
    assert!(!kinds.iter().any(|k| k == "exit_requested"), "{kinds:?}");
    assert!(!kinds.iter().any(|k| k == "run_waiting_ended"), "{kinds:?}");
    assert!(other_asks(&mut queue, false).iter().all(|a| a.id == ask));

    queue.answer(ask, "the second").unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(
        payloads(&detail, "ask_answered")[0]["runtime_delivers"],
        true
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let first = run_of(&mut queue, 1).unwrap();
    assert_eq!(first.status(), RunStatus::Integrated);
    let ended = events_of(&db, first.id(), "run_waiting_ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["cause"], "answered");
    // The wait, with the revise's clock stopped, outlasted its timeout.
    assert!(
        ended[0]["waited_secs"].as_u64().unwrap() >= STAGE_TIMEOUT.as_secs(),
        "{ended:?}"
    );
    let kinds = kinds_of(&db, 1);
    assert!(at(&kinds, "run_slot_regained") < at(&kinds, "ask_delivered"));
    assert!(at(&kinds, "ask_delivered") < at(&kinds, "revise_finished"));
    assert_eq!(
        fs::read_to_string(repo.join("answer.txt")).unwrap(),
        format!("answer to ask {ask}: the second")
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["started"]["worker_question"], 1, "{stats}");
}

/// A worker that asks twice while it revises: the turn of the revise
/// request asks, the turn of the first answer asks again, and the turn of
/// the second answer commits it.
const REVISE_ASKING_TWICE_AGENT: &str = concat!(
    r#"answer_to() { printf '%s\n' "$PROMPT" | head -n 1 | tr -d '\n' > "$1"; }"#,
    r#"
ask() { "$DAGQ" ask --run "$RUN_ID" --kind worker_question --because scope --topic acceptance_conflict --question "$1" --cmux /usr/bin/true > /dev/null || exit 70; }
case "$PROMPT" in
"answer to ask "*": first"*) ask 'And which word?' ;;
"answer to ask "*) answer_to answer.txt; git add answer.txt; git commit -q -m answer
  receipt "$(git rev-parse HEAD)" ;;
*) if [ "$TURN" -eq 1 ]; then commit work; receipt "$(git rev-parse HEAD)"
   else ask 'Which line?'
   fi ;;
esac
"#
);

/// A revise whose questions the waits leave in its slot (a limit of no
/// waits) is recorded waiting for a person, in its slot, from the pass
/// that finds each question, and back in its revise once the answer
/// came, however often it asks: each `waiting` follows its `ask_opened`
/// with no other phase between them but the move back from the earlier
/// question's answer, when one pass finds both.
#[test]
fn a_revise_asking_in_its_slot_is_recorded_waiting_from_each_question() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, REVISE_ASKING_TWICE_AGENT));
    let options = SuperviseOptions {
        max_waiting: Some(0),
        ..supervise_options(1, true)
    };
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["say which line"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = supervise_in_thread(&db, &repo, &backend, Some(&reviewer), options);
    let phases = |run: &TaskRun| events_of(&db, run.id(), "run_phase_changed");
    let waits = |run: &TaskRun| {
        phases(run)
            .iter()
            .filter(|phase| phase["phase"] == "waiting")
            .count()
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (n, answer) in [(1, "first"), (2, "second")] {
        wait_until(&db, Duration::from_secs(60), |queue| {
            run_of(queue, 1).is_some_and(|run| {
                open_ask_of(queue, &run, AskKind::WorkerQuestion).is_some() && waits(&run) == n
            })
        });
        let run = run_of(&mut queue, 1).unwrap();
        let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
        queue.answer(ask, answer).unwrap();
    }
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let run = run_of(&mut queue, 1).unwrap();
    assert_eq!(run.status(), RunStatus::Integrated);
    assert!(events_of(&db, run.id(), "run_waiting_started").is_empty());

    let events = SqliteQueue::open(&db)
        .unwrap()
        .run_events(run.id())
        .unwrap();
    let revise = json!({"kind": "revise", "n": 1});
    let mut asked = 0;
    for (index, event) in events.iter().enumerate() {
        if event.kind != EventKind::AskOpened {
            continue;
        }
        asked += 1;
        let mut after = events[index..]
            .iter()
            .filter(|e| e.kind == EventKind::RunPhaseChanged);
        let mut next = after.next().expect("a phase after the question");
        // A pass that finds the earlier question answered and this one
        // asked records both moves after this question: back to the
        // revise, then into this wait.
        if asked > 1 && next.payload["cause"] == "ask_answered" {
            next = after.next().expect("a wait after the move back");
        }
        let (at, payload) = (timestamp_millis(&next.created_at).unwrap(), &next.payload);
        assert_eq!(payload["phase"], "waiting", "{payload}");
        assert_eq!(payload["blocker"], "human");
        assert_eq!(payload["holds"], "worker_slot");
        assert_eq!(payload["cause"], "ask_opened");
        assert_eq!(payload["attempt"], revise);
        // In the pass that finds the ask, not a later one.
        let since = at - timestamp_millis(&event.created_at).unwrap();
        assert!(since < 5_000, "waiting {since} ms after the question");
    }
    assert_eq!(asked, 2);
    let named: Vec<(String, String)> = phases(&run)
        .iter()
        .map(|p| {
            (
                p["phase"].as_str().unwrap().to_owned(),
                p["cause"].as_str().unwrap().to_owned(),
            )
        })
        .skip_while(|(phase, _)| phase != "revise")
        .take(5)
        .collect();
    let pair = |phase: &str, cause: &str| (phase.to_owned(), cause.to_owned());
    assert_eq!(
        named,
        vec![
            pair("revise", "review_finished"),
            pair("waiting", "ask_opened"),
            pair("revise", "ask_answered"),
            pair("waiting", "ask_opened"),
            pair("revise", "ask_answered"),
        ]
    );
}

/// A supervisor that takes over a revising headless run whose supervisor
/// died adopts its wait from the run's events without a free slot
/// (decision 11): the one slot goes to another task, which lands, while
/// the question the session asked in its turn of the revise stays open;
/// the answer then reaches the live session as its next turn, and the run
/// lands. Moved from the deleted interactive
/// `an_adopted_revise_keeps_waiting_outside_the_slot` (task 1437).
#[test]
fn an_adopted_headless_revise_keeps_waiting_outside_the_slot() {
    let (_dir, repo, db) = fixture();
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second", &[]);
    let backend = TestWorkspace::new(&db, false, REVISE_ASKING_AGENT);
    backend.script_for(2, BESIDE_AGENT);
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    assert_eq!(run.task_id(), TaskId::new(1));
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, common::STEP_LIMIT, |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    // The request is sent a second after the session's idle marker.
    await_second_after(modified_second(&idle));
    let sent_at = unix_second_now();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (EventKind::ReviewStarted, json!({"attempt": 1})),
        (
            EventKind::ReviewFinished,
            json!({"verdict": "revise", "reasons": ["say which line"], "summary": "one gap", "attempt": 1}),
        ),
        (
            EventKind::ReviseRequested,
            json!({"attempt": 1, "reasons": ["say which line"], "sent_at": sent_at}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    let text = "dagq: the supervisor's review asks for changes (revise 1 of 2).";
    fs::write(Path::new(run.run_dir().unwrap()).join("revise-1.txt"), text).unwrap();
    request_turn_left_by_a_dead_supervisor(&db, &run, text, "revise request");
    wait_until(&db, common::STEP_LIMIT, |queue| {
        open_ask_of(queue, &run, AskKind::WorkerQuestion).is_some()
    });
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    // What the dead supervisor recorded when the run began to wait.
    queue
        .record_runtime_event(
            run.id(),
            EventKind::RunWaitingStarted,
            json!({"ask_id": ask, "ask_kind": "worker_question", "phase": "revise",
                   "status": "awaiting_integration", "waiting": 1, "limit": 4}),
        )
        .unwrap();
    age_lease(&db, &run, 31);
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("pass", &[], "fine"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = supervise_in_thread(
        &db,
        &repo,
        &backend,
        Some(&reviewer),
        supervise_options(1, true),
    );
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 2).is_some_and(|two| two.status() == RunStatus::Integrated)
    });
    assert_eq!(
        adoption_events(&queue.show(TaskId::new(1)).unwrap()).len(),
        1
    );
    let status = runtime::status(&db).unwrap();
    assert_eq!(status["waiting"][0]["run_id"], json!(run.id()), "{status}");
    assert_eq!(status["waiting"][0]["phase"], "revise");
    assert_eq!(status["waiting"][0]["state"], "waiting");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());

    queue.answer(ask, "the first").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Integrated);
    assert_eq!(events_of(&db, run.id(), "run_waiting_started").len(), 1);
    assert_eq!(
        events_of(&db, run.id(), "run_waiting_ended")[0]["cause"],
        "answered"
    );
    assert_eq!(
        fs::read_to_string(repo.join("answer.txt")).unwrap(),
        format!("answer to ask {ask}: the first")
    );
    // The revise was not written again: the first turn, the revise's and
    // the answer's.
    assert_eq!(stub_calls(&run).len(), 3, "{:?}", stub_calls(&run));
}

/// Move `ahead`, the supervisor's monotonic clock, past the stage timeout
/// and wait for passes of the supervisor that would have ended the stage,
/// had its clock run on.
fn run_the_stage_clock_out(ahead: &MonotonicAhead, passes: &AtomicU64) {
    ahead.by(STAGE_TIMEOUT + HELD_MARGIN);
    await_passes(passes, SOME_PASSES);
}

/// A resumed session's question waits where the waits leave room for it,
/// with the resume's clock stopped throughout (decisions 1, 7 and 15 to
/// 17): while another run's question takes the one wait, it stays in its
/// slot (`run_waiting_deferred`), and once that wait ended it leaves its
/// slot to wait. Moving the supervisor's clock past the resume timeout in
/// either place gets it no `/exit`. Its answer is the runtime's to type
/// once the run is back in a slot; the idle after that answer, with the
/// conflict resolved, ends the resume with its `exit_requested`, and the
/// run lands. Its phases record the wait in its slot from the pass that
/// defers it, then the wait outside the slots. When the return restarts
/// the resume's clocks, and from which time, is the unit tests'
/// (`waiting::tests`); where the waits' limit keeps a question in its slot
/// is `domain::waiting`'s.
#[test]
fn a_resume_question_waits_in_its_slot_then_outside_it_with_its_clock_stopped() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, first_landed) = parked_conflict(&repo, &db, &backend);
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "third", &[]);
    backend.resume_timeout = STAGE_TIMEOUT;
    backend.resume_script_for(2, &resume_asking(true));
    backend.script_for(3, ASKING_AGENT);
    let backend = Arc::new(backend);
    let (options, ahead) = supervise_options_ahead(2, true);
    let options = SuperviseOptions {
        max_waiting: Some(1),
        ..options
    };
    let passes = options.passes.clone();
    let supervisor = supervise_in_thread(&db, &repo, &backend, None, options);
    // The third task's first session takes the one wait.
    wait_until(&db, Duration::from_secs(60), |queue| {
        run_of(queue, 3)
            .is_some_and(|three| !events_of(&db, three.id(), "run_waiting_started").is_empty())
    });
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("gate"),
        "",
    )
    .unwrap();
    wait_until(&db, Duration::from_secs(30), |_| {
        !events_of(&db, run.id(), "run_waiting_deferred").is_empty()
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = open_ask_of(&mut queue, &run, AskKind::WorkerQuestion).unwrap();
    let deferrals = events_of(&db, run.id(), "run_waiting_deferred");
    assert_eq!(
        deferrals,
        vec![json!({"ask_id": ask, "ask_kind": "worker_question", "waiting": 1, "limit": 1})]
    );
    // In its slot past the resume timeout: no `/exit`, and no wait yet.
    run_the_stage_clock_out(&ahead, &passes);
    let kinds = kinds_of(&db, 2);
    let resumed = at(&kinds, "resume_started");
    assert!(
        !kinds[resumed..].iter().any(|k| k == "exit_requested"),
        "{kinds:?}"
    );
    assert!(events_of(&db, run.id(), "run_waiting_started").is_empty());

    // The other wait ends: the resume's question takes the wait.
    let three = run_of(&mut queue, 3).unwrap();
    let other = open_ask_of(&mut queue, &three, AskKind::WorkerQuestion).unwrap();
    queue.answer(other, "blue").unwrap();
    wait_until(&db, Duration::from_secs(60), |_| {
        !events_of(&db, run.id(), "run_waiting_started").is_empty()
    });
    let started = events_of(&db, run.id(), "run_waiting_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["phase"], "resume");
    assert_eq!(started[0]["status"], "needs_session");
    assert_eq!(started[0]["ask_id"], json!(ask));
    let status = runtime::status(&db).unwrap();
    let waiting = &status["waiting"][0];
    assert_eq!(waiting["run_id"], json!(run.id()), "{status}");
    assert_eq!(waiting["phase"], "resume");
    assert_eq!(waiting["status"], "needs_session");
    // Outside the slots past the resume timeout: no `/exit` either.
    run_the_stage_clock_out(&ahead, &passes);
    let kinds = kinds_of(&db, 2);
    assert!(!kinds.iter().any(|k| k == "resume_finished"), "{kinds:?}");
    assert!(
        !kinds[resumed..].iter().any(|k| k == "exit_requested"),
        "{kinds:?}"
    );

    queue.answer(ask, "theirs").unwrap();
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_eq!(
        payloads(&detail, "ask_answered")[0]["runtime_delivers"],
        true
    );
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(2)).unwrap();
    assert_landed(&repo, &detail.runs[0], "second", &first_landed);
    let texts = session_texts(&detail.runs[0]);
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with(&format!("answer to ask {ask}: theirs"))),
        "{texts:?}"
    );
    assert_eq!(payloads(&detail, "ask_delivered").len(), 1);
    // The wait records the time it took on the queue's clock, which the
    // moved monotonic clock does not stretch.
    let event = |kind: EventKind| detail.events.iter().find(|e| e.kind == kind).unwrap();
    let (began, ended) = (
        event(EventKind::RunWaitingStarted),
        event(EventKind::RunWaitingEnded),
    );
    assert_eq!(ended.payload["cause"], "answered");
    let millis = |e: &dagq::domain::RunEvent| timestamp_millis(&e.created_at).unwrap();
    let span = (millis(ended) - millis(began)) / 1000;
    let waited = ended.payload["waited_secs"].as_i64().unwrap();
    assert!((span - 1..=span + 1).contains(&waited), "{waited} {span}");
    let kinds = kinds_of(&db, 2);
    let exit = resumed
        + kinds[resumed..]
            .iter()
            .position(|k| k == "exit_requested")
            .unwrap();
    assert!(at(&kinds, "run_waiting_deferred") < at(&kinds, "run_waiting_started"));
    assert!(at(&kinds, "run_waiting_ended") < at(&kinds, "run_slot_regained"));
    assert!(at(&kinds, "run_slot_regained") < at(&kinds, "ask_delivered"));
    assert!(at(&kinds, "ask_delivered") < exit, "{kinds:?}");
    let requested = payloads(&detail, "exit_requested");
    assert_eq!(requested.last().unwrap()["resume_attempt"], 1);
    assert_eq!(
        payloads(&detail, "resume_finished")[0]["outcome"],
        "resolved"
    );
    let stats = runtime::stats(&db, &Default::default()).unwrap();
    assert_eq!(stats["waiting"]["deferred"], 1, "{stats}");
    // Kept in its slot, the question is a wait there from the pass that
    // defers it; the wait outside the slots is a record of its own, and
    // back in its slot the run resumes.
    let events = SqliteQueue::open(&db)
        .unwrap()
        .run_events(run.id())
        .unwrap();
    let asked = events
        .iter()
        .position(|e| e.kind == EventKind::AskOpened && e.payload["ask_id"] == json!(ask))
        .unwrap();
    let in_slot = events[asked..]
        .iter()
        .find(|e| e.kind == EventKind::RunPhaseChanged)
        .unwrap();
    let millis = |e: &dagq::domain::RunEvent| timestamp_millis(&e.created_at).unwrap();
    let since = millis(in_slot) - millis(&events[asked]);
    assert!(since < 5_000, "waiting {since} ms after the question");
    assert_eq!(in_slot.payload["blocker"], "human");
    let phases: Vec<[String; 3]> = events_of(&db, run.id(), "run_phase_changed")
        .iter()
        .map(|p| ["phase", "holds", "cause"].map(|key| p[key].as_str().unwrap().to_owned()))
        .skip_while(|[phase, _, _]| phase != "resume")
        .take(5)
        .collect();
    let row = |phase: &str, holds: &str, cause: &str| [phase, holds, cause].map(str::to_owned);
    assert_eq!(phases[0][0], "resume", "{phases:?}");
    assert_eq!(
        phases[1..],
        [
            row("waiting", "worker_slot", "ask_opened"),
            row("waiting", "none", "run_waiting_started"),
            row("returning", "none", "run_waiting_ended"),
            row("resume", "worker_slot", "run_slot_regained"),
        ],
        "{phases:?}"
    );
    assert_eq!(in_slot.payload["phase"], "waiting");
    assert_eq!(
        in_slot.payload["attempt"],
        json!({"kind": "resume", "n": 1})
    );
}
