//! Runtime tests: a revise the supervisor adopts carries over what the
//! previous supervisor recorded for its live session (task 581).
use crate::{common, runtime_support};
use dagq::domain::EventKind;

use dagq::domain::{AskReason, NewAsk};
use runtime_support::*;
use sha2::Digest;

/// A dialog screen as Claude Code draws it.
pub(crate) const DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

/// The `screen_hash` the supervisor records for `screen`: the digest of its
/// excerpt, the last non-empty lines right-trimmed.
pub(crate) fn screen_hash(screen: &str) -> String {
    let excerpt = screen
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    format!("{:x}", sha2::Sha256::digest(excerpt.as_bytes()))
}

/// A revise whose session stopped at a dialog before its supervisor died
/// (`prompt_waiting`, its recovery job escalated to an `answer_prompt` ask)
/// is adopted with that dialog: the adopter records neither the same screen
/// again nor another recovery job for it, and once the session rewrites its
/// receipt the revise's end records `prompt_cleared`, closes the ask, and
/// the run lands. Here the run waits outside its slot for the ask
/// (ADR-0071), and goes back to its revise once the session moves.
#[test]
fn an_adopted_revise_keeps_the_dialog_recorded_before_and_clears_it_at_its_end() {
    adopt_revise_at_dialog(dagq::domain::waiting::DEFAULT_MAX_WAITING);
}

/// The same with waits turned off: the revise's own watch reads the screen
/// while the dialog stays up and finds the dialog it adopted.
#[test]
fn an_adopted_revise_in_its_slot_does_not_record_its_dialog_again() {
    adopt_revise_at_dialog(0);
}

/// A revise under a dead supervisor whose session committed its work and
/// waits for the revise's request: the run is `awaiting_integration` with
/// `before` recorded after its validation, then the review's revise verdict
/// and its `revise_requested`, then `after`. The session commits the fix
/// and rewrites its receipt once `$EXIT.go` exists.
fn revise_under_dead_supervisor(
    screen: Option<&str>,
    before: Vec<(&str, Value)>,
    after: Vec<(&str, Value)>,
) -> (
    Fixture,
    PathBuf,
    PathBuf,
    String,
    Arc<TestWorkspace>,
    TaskRun,
) {
    let (dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         while [ ! -f \"$EXIT.go\" ]; do sleep 0.05; done; \
         printf 'fix\\n' >> change.txt; git commit -q -am fix; \
         receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.prompt_wait = Duration::from_millis(300);
    if let Some(screen) = screen {
        *backend.screen.lock().unwrap() = screen.into();
    }
    let backend = Arc::new(backend);
    let run = start_run_under_dead_supervisor(&repo, &db, &backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(&db, Duration::from_secs(20), |_| idle.is_file());
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
    // The request is sent a second after the session's idle marker, which
    // then predates it. No `revise-1.txt` is written, so the adopter does
    // not check whether the session took it.
    thread::sleep(Duration::from_millis(1100));
    let sent_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let queue = SqliteQueue::open(&db).unwrap();
    let events = [(
        "validation_finished",
        json!({"status": "awaiting_integration"}),
    )]
    .into_iter()
    .chain(before)
    .chain([
        ("review_started", json!({"attempt": 1})),
        (
            "review_finished",
            json!({"verdict": "revise", "reasons": ["add a line"], "summary": "one gap", "attempt": 1}),
        ),
        (
            "revise_requested",
            json!({"attempt": 1, "reasons": ["add a line"], "sent_at": sent_at}),
        ),
    ])
    .chain(after);
    for (kind, payload) in events {
        queue
            .record_runtime_event(run.id(), EventKind::from_name(kind).unwrap(), payload)
            .unwrap();
    }
    age_lease(&db, &run, 31);
    (dir, repo, db, base, backend, run)
}

/// Supervise the queue in a thread with a reviewer that passes the revise.
fn adopter(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
    max_waiting: usize,
) -> thread::JoinHandle<Value> {
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fixed")]));
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    thread::spawn(move || {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        let outcome = runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &*reviewer,
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &SuperviseOptions {
                max_waiting: Some(max_waiting),
                ..supervise_options(4, true)
            },
        )
        .unwrap();
        backend.join();
        outcome
    })
}

/// Wait for the adoption and for several screen checks after it.
fn adopted_and_checked(db: &Path, backend: &TestWorkspace) {
    wait_until(db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    let captures = backend.captures.load(Ordering::SeqCst);
    wait_until(db, Duration::from_secs(30), |_| {
        backend.captures.load(Ordering::SeqCst) >= captures + 3
    });
}

/// Let the session rewrite its receipt.
fn let_the_session_fix(run: &TaskRun) {
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
}

fn adopt_revise_at_dialog(max_waiting: usize) {
    let (_dir, repo, db, base, backend, run) = revise_under_dead_supervisor(
        Some(DIALOG_SCREEN),
        vec![],
        vec![
            (
                "prompt_waiting",
                json!({
                    "workspace_id": WORKSPACE_ID,
                    "excerpt": "Auto mode is available",
                    "screen_hash": screen_hash(DIALOG_SCREEN),
                    "prompt": "choice",
                }),
            ),
            (
                "recovery_requested",
                json!({"alert": "prompt_waiting", "attempt": 1}),
            ),
            (
                "recovery_finished",
                json!({"alert": "prompt_waiting", "attempt": 1, "outcome": "escalated"}),
            ),
        ],
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let ask = queue
        .ask(NewAsk {
            topics: Vec::new(),
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question: "run waits at a choice dialog".into(),
            options: vec![],
            asked_by: "supervisor".into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
        })
        .unwrap()
        .ask;
    let supervisor = adopter(&db, &repo, &backend, max_waiting);
    // Several screen checks later, the dialog still up is the one recorded.
    adopted_and_checked(&db, &backend);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1);
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());

    // The session rewrites its receipt, the dialog left on its screen.
    let_the_session_fix(&run);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let kinds = event_kinds(&detail);
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1, "{kinds:?}");
    assert_eq!(
        payloads(&detail, "recovery_requested").len(),
        1,
        "{kinds:?}"
    );
    assert_eq!(payloads(&detail, "prompt_cleared").len(), 1, "{kinds:?}");
    assert!(position(&kinds, "run_adopted") < position(&kinds, "prompt_cleared"));
    assert!(position(&kinds, "prompt_cleared") < position(&kinds, "revise_finished"));
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
}

/// A dialog recorded before the revise's `revise_requested` and never
/// cleared (a path such as the end of a resume's wrapper records no
/// `prompt_cleared`) is not carried over into the adopted revise: its
/// first screen check, with no dialog on the screen, records no
/// `prompt_cleared`, and the run lands (task 742).
#[test]
fn an_adopted_revise_leaves_a_dialog_recorded_before_its_request() {
    let (_dir, repo, db, base, backend, run) = revise_under_dead_supervisor(
        None,
        vec![(
            "prompt_waiting",
            json!({
                "workspace_id": WORKSPACE_ID,
                "excerpt": "Auto mode is available",
                "screen_hash": screen_hash(DIALOG_SCREEN),
                "prompt": "choice",
            }),
        )],
        vec![],
    );
    let supervisor = adopter(&db, &repo, &backend, 0);
    adopted_and_checked(&db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(payloads(&detail, "prompt_cleared").is_empty(), "{kinds:?}");

    let_the_session_fix(&run);
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    let kinds = event_kinds(&detail);
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1, "{kinds:?}");
    assert!(payloads(&detail, "prompt_cleared").is_empty(), "{kinds:?}");
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
}

/// A passed run whose `/exit` never got there, adopted after its supervisor
/// recorded `exit_unsent` (`action: close_and_land`) and died before it
/// recorded its workspace closed: the passed run with its review passed,
/// `/exit` requested and never reached, and the `extra` shell run in its
/// worktree, with a stale lease. Returns the run.
fn adoptable_unsent_exit(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    extra: Option<&str>,
) -> TaskRun {
    let run = start_run_under_dead_supervisor(repo, db, backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(db, Duration::from_secs(20), |_| idle.is_file());
    let worktree = Path::new(run.worktree_path().unwrap());
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    if let Some(extra) = extra {
        let status = std::process::Command::new("sh")
            .args(["-c", extra])
            .current_dir(worktree)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let queue = SqliteQueue::open(db).unwrap();
    for (kind, payload) in [
        (
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        ),
        (EventKind::ReviewStarted, json!({"attempt": 1})),
        (
            EventKind::ReviewFinished,
            json!({"verdict": "pass", "reasons": [], "summary": "meets the acceptance", "attempt": 1}),
        ),
        (
            EventKind::ExitRequested,
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
        (
            EventKind::ExitUnsent,
            json!({"code": "backend_timeout", "workspace_id": WORKSPACE_ID, "attempts": 3, "action": "close_and_land"}),
        ),
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
    age_lease(db, &run, 31);
    run
}

/// Supervise until the queue is idle, with a reviewer that has no verdict
/// to give: an adopted run past its review is not reviewed again.
fn supervise_adopter(
    db: &Path,
    repo: &Path,
    backend: &Arc<TestWorkspace>,
) -> thread::JoinHandle<Value> {
    let (db, repo, backend) = (db.to_owned(), repo.to_owned(), backend.clone());
    thread::spawn(move || {
        let _waiting = common::within(common::STEP_LIMIT, "supervise to return");
        let outcome = runtime::supervise_with_reviewer(
            &db,
            &repo,
            &*backend,
            &claude_stub(&db),
            &TestReviewer::new(&[]),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &supervise_options(4, true),
        )
        .unwrap();
        backend.join();
        outcome
    })
}

/// A run whose supervisor decided to close its workspace and land after a
/// `/exit` that never got there, and died before recording the close, is
/// landed by its adopter at once (task 464): the adopter judges again that
/// its receipt holds against its clean worktree at the reviewed commit,
/// closes the workspace, and records the close and the repair as adopted,
/// without a second `/exit` or waiting out the exit timeout.
#[test]
fn an_adopter_closes_and_lands_a_run_whose_unsent_exit_was_to_land() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.close_ends_session = true;
    // Waiting out the exit timeout would outlast the test.
    backend.exit_timeout = Duration::from_secs(3600);
    let backend = Arc::new(backend);
    adoptable_unsent_exit(&repo, &db, &backend, None);
    let outcome = joined(
        supervise_adopter(&db, &repo, &backend),
        "the supervisor thread to return",
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed(&repo, &detail.runs[0], "test task", &base);
    assert_eq!(
        payloads(&detail, "auto_repaired"),
        [&json!({
            "layer": "runtime",
            "repair": "exit_forced_close",
            "conditions": {
                "cause": "backend_timeout",
                "attempts": 3,
                "exit_reached": false,
                "then": "land",
                "review": "pass",
                "receipt_holds": true,
                "adopted": true,
            },
            "detail": {"workspace_id": WORKSPACE_ID},
        })]
    );
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("exit_unsent", "run_adopted"),
        ("run_adopted", "workspace_closed"),
        ("workspace_closed", "run_integrated"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    assert!(queue.asks(AskQuery::default()).unwrap().is_empty());
}

/// The same run whose worktree changed since the close was decided does not
/// land without its session's exit: its adopter records the timeout of the
/// `/exit` that never got there with why, and the `stuck_exit` recovery job
/// escalates to the ask, once (task 464). Once the person cleans up and has
/// the session exit, the run lands.
#[test]
fn an_adopter_asks_when_an_unsent_exit_to_land_no_longer_holds() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(3600);
    backend.registration_timeout = common::STEP_LIMIT;
    let backend = Arc::new(backend);
    let run = adoptable_unsent_exit(&repo, &db, &backend, Some("printf 'x\\n' > stray.txt"));
    let supervisor = supervise_adopter(&db, &repo, &backend);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .any(|ask| ask.kind == AskKind::StuckExit)
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let timed_out = payloads(&detail, "exit_request_timed_out");
    assert_eq!(timed_out.len(), 1, "{timed_out:?}");
    assert_eq!(timed_out[0]["unsent"], true);
    assert_eq!(timed_out[0]["adopted"], true);
    let held = timed_out[0]["held"].as_str().unwrap();
    assert!(
        held.starts_with("its receipt no longer holds: worktree is not clean"),
        "{held}"
    );
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert!(backend.closed().is_empty());
    assert!(queue.run(run.id()).unwrap().workspace_closed_at().is_none());
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(
        asks[0].question.contains("(exit_unsent)"),
        "{}",
        asks[0].question
    );
    assert!(asks[0].question.contains(held), "{}", asks[0].question);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);

    // The person cleans up and has the session exit: the run lands.
    fs::remove_file(Path::new(run.worktree_path().unwrap()).join("stray.txt")).unwrap();
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    assert!(queue.read_ask(asks[0].id).unwrap().closed_at.is_some());
    assert_eq!(
        payloads(
            &queue.show(TaskId::new(1)).unwrap(),
            "exit_request_timed_out"
        )
        .len(),
        1
    );
}

/// The same run whose workspace is gone by the time its adopter finds the
/// close no longer holding, as the previous supervisor records
/// `close_and_land` only after the close succeeded (task 757): its session
/// is taken as ended even while its wrapper's heartbeat is fresh, so no
/// `/exit`, `stuck_exit` recovery job or ask is aimed at the workspace, and
/// the run goes on to land, where integrate checks it again and parks it.
#[test]
fn an_adopter_takes_the_session_as_ended_when_the_workspace_of_an_unsent_exit_is_gone() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(3600);
    let backend = Arc::new(backend);
    let run = adoptable_unsent_exit(&repo, &db, &backend, Some("printf 'x\\n' > stray.txt"));
    backend.hidden.lock().unwrap().push(WORKSPACE_ID.to_owned());
    let supervisor = supervise_adopter(&db, &repo, &backend);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue.run(run.id()).unwrap().workspace_closed_at().is_some()
            && queue
                .show(TaskId::new(1))
                .unwrap()
                .events
                .iter()
                .any(|e| e.kind == "landing_queued" || e.kind == "integration_started")
    });
    // The wrapper lived on; the test ends its session.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    // Integrate finds the stray file and parks the run for a session, which
    // this backend has no script to resume.
    assert_eq!(outcome["runs"][0]["status"], "needs_session", "{outcome}");
    let last_error = outcome["runs"][0]["last_error"].as_str().unwrap();
    assert!(last_error.contains("stray.txt"), "{last_error}");
    let errors = outcome["errors"].as_array().unwrap();
    assert!(
        errors.iter().all(|e| e["message"]
            .as_str()
            .unwrap()
            .ends_with("could not be resumed: no resume script for task 1")),
        "{outcome}"
    );
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
    assert!(!kinds.contains(&"recovery_requested"), "{kinds:?}");
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent.len(), 2, "{unsent:?}");
    assert_eq!(unsent[1]["action"], "session_gone");
    assert_eq!(unsent[1]["adopted"], true);
    assert_eq!(unsent[1]["workspace_gone"], true);
    assert_eq!(unsent[1]["then"], "land");
    let held = unsent[1]["held"].as_str().unwrap();
    assert!(
        held.starts_with("its receipt no longer holds: worktree is not clean"),
        "{held}"
    );
    for (earlier, later) in [
        ("run_adopted", "workspace_closed"),
        ("workspace_closed", "landing_queued"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert!(backend.closed().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert!(
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .all(|ask| ask.kind != AskKind::StuckExit),
    );
}

/// What integrate does not check again (here the reviewed commit that is no
/// longer the head) keeps the run from landing even when the workspace of
/// its unsent exit is gone, but no `/exit`, `stuck_exit` recovery job or ask
/// is aimed at the workspace either, however fresh its wrapper's heartbeat:
/// its session is taken as ended and the run is parked for a resume
/// (`session_gone_parked`), which starts once that session is over (task
/// 960).
#[test]
fn an_adopter_parks_an_unreviewed_head_for_a_resume_when_the_workspace_is_gone() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(3600);
    let backend = Arc::new(backend);
    let run = adoptable_unsent_exit(&repo, &db, &backend, None);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), base],
        )
        .unwrap();
    backend.hidden.lock().unwrap().push(WORKSPACE_ID.to_owned());
    let supervisor = supervise_adopter(&db, &repo, &backend);
    // The park comes before the workspace is recorded closed, and the lease
    // goes back last: the adopter is done with the run once it has.
    wait_until(&db, common::STEP_LIMIT, |queue| {
        let events = queue.show(TaskId::new(1)).unwrap().events;
        events
            .iter()
            .position(|e| e.kind == "session_gone_parked")
            .is_some_and(|parked| events[parked..].iter().any(|e| e.kind == "lease_released"))
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    for kind in [
        "exit_request_timed_out",
        "recovery_requested",
        "landing_queued",
        "integration_started",
    ] {
        assert!(!kinds.contains(&kind), "{kind}: {kinds:?}");
    }
    let unsent = payloads(&detail, "exit_unsent");
    assert_eq!(unsent.len(), 2, "{unsent:?}");
    assert_eq!(unsent[1]["action"], "session_gone");
    assert_eq!(unsent[1]["adopted"], true);
    assert_eq!(unsent[1]["workspace_gone"], true);
    assert_eq!(unsent[1]["then"], "land");
    assert_eq!(unsent[1]["resume"], true);
    let held = unsent[1]["held"].as_str().unwrap();
    assert!(held.contains("is not the reviewed commit"), "{held}");
    let parked = payloads(&detail, "session_gone_parked");
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert_eq!(parked[0]["code"], "session_gone");
    assert_eq!(parked[0]["status"], "needs_session");
    assert_eq!(parked[0]["workspace_id"], WORKSPACE_ID);
    assert_eq!(parked[0]["held"], held);
    let reason = parked[0]["reason"].as_str().unwrap();
    assert!(reason.ends_with(held), "{reason}");
    for (earlier, later) in [
        ("run_adopted", "session_gone_parked"),
        ("session_gone_parked", "workspace_closed"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    let parked_run = queue.run(run.id()).unwrap();
    assert!(parked_run.workspace_closed_at().is_some());
    assert_eq!(parked_run.last_error(), Some(reason));
    assert!(backend.closed().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert!(
        queue
            .asks(AskQuery::default())
            .unwrap()
            .iter()
            .all(|ask| ask.kind != AskKind::StuckExit),
    );

    // The wrapper lived on, which holds the resume back; once the test ends
    // its session, a supervisor resumes the run, which this backend has no
    // script for. The adopter may have gone idle before that, so another
    // supervisor pass follows the wrapper's exit.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .processes(run.id())
            .unwrap()
            .iter()
            .any(|p| p.role == "wrapper" && p.exited_at.is_some())
    });
    let first = joined(supervisor, "the supervisor thread to return");
    let second = joined(
        supervise_adopter(&db, &repo, &backend),
        "the second supervisor thread to return",
    );
    let errors: Vec<&Value> = [&first, &second]
        .iter()
        .flat_map(|outcome| outcome["errors"].as_array().unwrap())
        .collect();
    assert!(!errors.is_empty(), "{first} {second}");
    assert!(
        errors.iter().all(|e| e["message"]
            .as_str()
            .unwrap()
            .ends_with("could not be resumed: no resume script for task 1")),
        "{first} {second}"
    );
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::NeedsSession
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(kinds.contains(&"resume_started"), "{kinds:?}");
    assert!(!kinds.contains(&"integration_started"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_request_timed_out"), "{kinds:?}");
}

/// The same unreviewed head whose workspace cmux still lists is not parked:
/// its adopter records the timeout of the `/exit` that never got there with
/// why, and the run takes the `stuck_exit` path as before (task 960).
#[test]
fn an_adopter_keeps_an_unreviewed_head_on_the_stuck_exit_path_while_the_workspace_is_listed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.exit_timeout = Duration::from_secs(3600);
    let backend = Arc::new(backend);
    let run = adoptable_unsent_exit(&repo, &db, &backend, None);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), base],
        )
        .unwrap();
    let supervisor = supervise_adopter(&db, &repo, &backend);
    wait_until(&db, common::STEP_LIMIT, |queue| {
        queue
            .show(TaskId::new(1))
            .unwrap()
            .events
            .iter()
            .any(|e| e.kind == "exit_request_timed_out")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let timed_out = payloads(&detail, "exit_request_timed_out");
    assert_eq!(timed_out[0]["unsent"], true);
    assert_eq!(timed_out[0]["adopted"], true);
    let held = timed_out[0]["held"].as_str().unwrap();
    assert!(held.contains("is not the reviewed commit"), "{held}");
    assert_eq!(payloads(&detail, "exit_unsent").len(), 1);
    assert!(payloads(&detail, "session_gone_parked").is_empty());
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    assert!(queue.run(run.id()).unwrap().workspace_closed_at().is_none());
    // The test ends the session; what follows is the old path's.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    joined(supervisor, "the supervisor thread to return");
}

/// A reviewed run whose `/exit` its supervisor requested 120 seconds ago
/// before it died, with its session still up and a stale lease: past a
/// passed review, or, when `failed_ask`, past a review that failed and the
/// `approve_landing` ask the supervisor opened for it. Returns the run.
fn reviewed_run_asked_to_exit(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    failed_ask: bool,
) -> TaskRun {
    let run = start_run_under_dead_supervisor(repo, db, backend, "dead-supervisor");
    let idle = run.idle_marker_path().unwrap();
    wait_until(db, Duration::from_secs(20), |_| idle.is_file());
    let head = git_out(
        Path::new(run.worktree_path().unwrap()),
        &["rev-parse", "HEAD"],
    );
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        )
        .unwrap();
    queue
        .record_runtime_event(run.id(), EventKind::ReviewStarted, json!({"attempt": 1}))
        .unwrap();
    if !failed_ask {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ReviewFinished,
                json!({"verdict": "pass", "reasons": [], "summary": "meets the acceptance", "attempt": 1}),
            )
            .unwrap();
    }
    queue
        .record_runtime_event(
            run.id(),
            EventKind::ExitRequested,
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 60}),
        )
        .unwrap();
    if failed_ask {
        queue
            .ask(NewAsk {
                topics: Vec::new(),
                kind: AskKind::ApproveLanding,
                task_id: None,
                run_id: Some(run.id().clone()),
                question: "The supervisor's headless review of run r (task 1) failed and gave no verdict (review 1): exit status 3".into(),
                options: vec!["land".into(), "send_back".into(), "cancel".into()],
                asked_by: "supervisor".into(),
                reason_category: AskReason::Scope,
                finding_id: None,
            })
            .unwrap();
    }
    crate::runtime_adopt::backdate_event(db, &run, "exit_requested", 120);
    age_lease(db, &run, 31);
    run
}

/// The `/exit` a passed run's supervisor requested after its review, before
/// it died, keeps the time already waited (task 959): requested twice the
/// exit timeout ago, its adopter records `exit_request_timed_out` without
/// waiting the timeout again and types no second `/exit`.
#[test]
fn an_adopted_exit_after_a_passed_review_times_out_from_its_recorded_request() {
    adopted_exit_after_review_times_out(false);
}

/// The same for the `/exit` of a review that failed and whose
/// `approve_landing` ask was opened before the supervisor died (task 959).
#[test]
fn an_adopted_exit_after_a_failed_review_ask_times_out_from_its_recorded_request() {
    adopted_exit_after_review_times_out(true);
}

fn adopted_exit_after_review_times_out(failed_ask: bool) {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let exit_timeout = Duration::from_secs(60);
    backend.exit_timeout = exit_timeout;
    let backend = Arc::new(backend);
    let run = reviewed_run_asked_to_exit(&repo, &db, &backend, failed_ask);
    let supervisor = supervise_adopter(&db, &repo, &backend);
    wait_until(&db, Duration::from_secs(30), |queue| {
        event_kinds(&queue.show(TaskId::new(1)).unwrap()).contains(&"exit_request_timed_out")
    });
    let mut queue = SqliteQueue::open(&db).unwrap();
    let waited = crate::runtime_adopt::between(&mut queue, "run_adopted", "exit_request_timed_out");
    assert!(
        waited < exit_timeout,
        "timed out {waited:?} after the adoption"
    );
    // Let the fake session out, the way a person answering it would.
    fs::write(exit_request_path(run.run_dir().unwrap()), "").unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert_eq!(
        outcome["runs"][0]["status"],
        if failed_ask {
            "awaiting_integration"
        } else {
            "integrated"
        },
        "{outcome} {kinds:?}"
    );
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 0);
    assert_eq!(kinds.iter().filter(|k| **k == "exit_requested").count(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
}
