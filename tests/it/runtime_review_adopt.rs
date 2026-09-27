//! Runtime tests: a revise the supervisor adopts carries over what the
//! previous supervisor recorded for its live session (task 581).
use crate::{common, runtime_support};

use dagq::domain::{AskReason, NewAsk};
use runtime_support::*;
use sha2::Digest;

/// A dialog screen as Claude Code draws it.
const DIALOG_SCREEN: &str = "\
 Auto mode is available

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

/// The `screen_hash` the supervisor records for `screen`: the digest of its
/// excerpt, the last non-empty lines right-trimmed.
fn screen_hash(screen: &str) -> String {
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
    adopt_revise_at_dialog(SuperviseOptions::new(4, true).max_waiting);
}

/// The same with waits turned off: the revise's own watch reads the screen
/// while the dialog stays up and finds the dialog it adopted.
#[test]
fn an_adopted_revise_in_its_slot_does_not_record_its_dialog_again() {
    adopt_revise_at_dialog(0);
}

fn adopt_revise_at_dialog(max_waiting: usize) {
    let (_dir, repo, db) = fixture();
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
    *backend.screen.lock().unwrap() = DIALOG_SCREEN.into();
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
    let mut queue = SqliteQueue::open(&db).unwrap();
    for (kind, payload) in [
        (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        ),
        ("review_started", json!({"attempt": 1})),
        (
            "review_finished",
            json!({"verdict": "revise", "reasons": ["add a line"], "summary": "one gap", "attempt": 1}),
        ),
        (
            "revise_requested",
            json!({"attempt": 1, "reasons": ["add a line"], "sent_at": sent_at}),
        ),
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
    ] {
        queue.record_runtime_event(run.id(), kind, payload).unwrap();
    }
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
    age_lease(&db, &run, 31);
    let reviewer = Arc::new(TestReviewer::new(&[verdict("pass", &[], "fixed")]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
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
                    max_waiting,
                    ..supervise_options(4, true)
                },
            )
            .unwrap();
            backend.join();
            outcome
        })
    };
    wait_until(&db, Duration::from_secs(30), |queue| {
        !adoption_events(&queue.show(TaskId::new(1)).unwrap()).is_empty()
    });
    // Several screen checks later, the dialog still up is the one recorded.
    let captures = backend.captures.load(Ordering::SeqCst);
    wait_until(&db, Duration::from_secs(30), |_| {
        backend.captures.load(Ordering::SeqCst) >= captures + 3
    });
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "prompt_waiting").len(), 1);
    assert_eq!(payloads(&detail, "recovery_requested").len(), 1);
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_none());

    // The session rewrites its receipt, the dialog left on its screen.
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
