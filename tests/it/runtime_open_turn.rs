//! Runtime tests: a resumed or revised session that went idle after it
//! rewrote its receipt, and then took an input (its input marker newer than
//! the idle marker, here a notice that its background work ended), is at
//! work on the turn that input started: its stage does not end, and no
//! `/exit` is sent, until the idle marker that ends that turn (task 672).
use crate::runtime_support;
use dagq::domain::EventKind;

use runtime_support::*;

/// Shell for the session: the `Stop` hook's idle marker, then the input
/// marker of the agent's notice that its background work ended. Both are
/// written aside and renamed in, the input first: the idle marker never
/// shows without the newer input.
const IDLE_THEN_NOTICE: &str = r#"
sleep 0.05
printf '{"session_id":"%s","hook_event_name":"Stop","stop_hook_active":false}' "$RUN_ID" > "$IDLE.tmp"
sleep 0.05
INPUT="$(dirname "$IDLE")/prompt-submit.json"
printf '{"hook_event_name":"UserPromptSubmit","prompt":"<task-notification>\\n<status>completed</status>\\n</task-notification>"}' > "$INPUT.tmp"
mv "$INPUT.tmp" "$INPUT"; mv "$IDLE.tmp" "$IDLE"
while [ ! -f "$EXIT.go" ]; do sleep 0.05; done
sleep 0.05; idle
"#;

/// Wait for the notice's input marker of `run`, and hold a while past it:
/// `during` sees the queue while the notice's turn runs.
fn while_the_notice_runs(db: &Path, run: &TaskRun, during: impl FnOnce(&mut SqliteQueue)) {
    let input = Path::new(&run.idle_marker_path().unwrap()).with_file_name("prompt-submit.json");
    let started = Instant::now();
    while !input.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the session took no notice"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // Several ticks of the supervisor.
    thread::sleep(Duration::from_millis(1500));
    during(&mut SqliteQueue::open(db).unwrap());
    fs::write(
        exit_request_path(run.run_dir().unwrap()).with_extension("go"),
        "",
    )
    .unwrap();
}

/// Acceptance (a): a resumed session that rewrote its receipt and went
/// idle, then took a notice, is not taken for done: no `resume_finished`
/// and no `/exit` while the notice's turn runs; the idle that ends it ends
/// the resume, which goes on to validation with the session open.
#[test]
fn a_resumed_session_at_work_on_a_notice_after_its_idle_is_not_ended() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let (run, _) = parked_conflict(&repo, &db, &backend);
    let mut queue = SqliteQueue::open(&db).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='integration_approved'",
            [&run.id()],
        )
        .unwrap();
    queue
        .record_runtime_event(
            run.id(),
            EventKind::EvidenceMissing,
            json!({"status": "needs_session", "reason": "e2e has no evidence"}),
        )
        .unwrap();
    backend.resume_script_for(
        2,
        &format!(
            "await_message; receipt \"$(git rev-parse HEAD)\"\n{IDLE_THEN_NOTICE}\nawait_exit"
        ),
    );
    // The turn the notice started shows at work.
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let backend = Arc::new(backend);
    let supervisor = {
        let (db, repo, backend) = (db.clone(), repo.clone(), backend.clone());
        thread::spawn(move || supervise(&db, &repo, &backend))
    };
    while_the_notice_runs(&db, &run, |queue| {
        let detail = queue.show(TaskId::new(2)).unwrap();
        assert!(payloads(&detail, "resume_started").len() == 1);
        let kinds = event_kinds(&detail);
        assert!(!kinds.contains(&"resume_finished"), "{kinds:?}");
        assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    });
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(backend.resume_timeout > Duration::from_secs(60));
    let detail = queue.show(TaskId::new(2)).unwrap();
    let finished = payloads(&detail, "resume_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "resolved");
    assert_eq!(finished[0]["session_live"], true);
}

/// Acceptance (b): the same for a revised session: the revise does not end
/// at the idle the notice followed, and ends at the idle after it; the run
/// is reviewed again and lands.
#[test]
fn a_revised_session_at_work_on_a_notice_after_its_idle_is_not_ended() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = Arc::new(TestWorkspace::new(
        &db,
        false,
        &format!(
            "commit work; receipt \"$(git rev-parse HEAD)\"; idle
while [ ! -f \"$MESSAGE\" ]; do sleep 0.05; done
printf 'fix\\n' >> change.txt; git commit -q -am fix
receipt \"$(git rev-parse HEAD)\"
{IDLE_THEN_NOTICE}
await_exit"
        ),
    ));
    *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
    let reviewer = Arc::new(TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || {
            runtime::supervise_with_reviewer(
                &db,
                &repo,
                &*backend,
                &claude_stub(&db),
                &*reviewer,
                Path::new(env!("CARGO_BIN_EXE_dagq")),
                &supervise_options(4, true),
            )
        })
    };
    let run = {
        let started = Instant::now();
        loop {
            let detail = SqliteQueue::open(&db)
                .unwrap()
                .show(TaskId::new(1))
                .unwrap();
            // A run is claimed before it is planned: its run directory
            // comes with the plan.
            if let Some(run) = detail.runs.first().filter(|run| run.run_dir().is_some()) {
                break run.clone();
            }
            assert!(started.elapsed() < Duration::from_secs(60));
            thread::sleep(Duration::from_millis(20));
        }
    };
    while_the_notice_runs(&db, &run, |queue| {
        let detail = queue.show(TaskId::new(1)).unwrap();
        let kinds = event_kinds(&detail);
        assert!(kinds.contains(&"revise_requested"), "{kinds:?}");
        assert!(!kinds.contains(&"revise_finished"), "{kinds:?}");
        assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    });
    let outcome = joined(supervisor, "the supervisor thread to return").unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
}
