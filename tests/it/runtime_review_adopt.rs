//! Runtime tests: a revise the supervisor adopts carries over what the
//! previous supervisor recorded for its live session (task 581).
use crate::{common, runtime_support};

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
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
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
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("review_started", json!({"attempt": 1})),
        (
            "review_finished",
            json!({"verdict": "pass", "reasons": [], "summary": "meets the acceptance", "attempt": 1}),
        ),
        (
            "exit_requested",
            json!({"workspace_id": WORKSPACE_ID, "timeout_secs": 120}),
        ),
        (
            "exit_unsent",
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
