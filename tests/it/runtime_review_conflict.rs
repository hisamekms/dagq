//! Runtime tests: a passed run that conflicts with a moving main, and the
//! requests an adopting supervisor sends again.
use crate::runtime_support;
use dagq::domain::EventKind;

use crate::runtime_review::revising_agent;
use runtime_support::*;

/// A reviewer script that moves main in the main checkout with a change to
/// `change.txt` that conflicts with the run's, then passes the run.
pub(crate) fn moving_main_then_pass() -> String {
    format!(
        "cd \"$(git rev-parse --path-format=absolute --git-common-dir)/..\" && \
         printf 'main moved by %s\\n' $$ > change.txt && git add change.txt && \
         git commit -q -m 'main moves' && {}",
        verdict("pass", &[], "meets the acceptance")
    )
}

/// The worker goes idle after its receipt and never exits by itself; each
/// time a conflict request arrives in its terminal it rebases onto the main
/// the request names, resolves `change.txt`, rewrites the receipt and goes
/// idle again, `requests` times.
pub(crate) fn rebasing_agent(requests: usize) -> String {
    format!(
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; {RESUME_PRELUDE}\n\
         for n in $(seq 1 {requests}); do \
           await_message; rm \"$MESSAGE\"; resolve || exit 1; \
           receipt \"$(git rev-parse HEAD)\"; idle; \
         done; await_exit"
    )
}

/// A passed run whose head conflicts with the main that moved during its
/// review is not asked to exit (ADR-0027 decision 4): `git merge-tree` finds
/// the conflict without touching the worktree, `conflict_precheck` is
/// recorded, and the live session gets the resume's resolution request. It
/// rebases and rewrites its receipt; the run is validated and reviewed
/// again, the second precheck finds no conflict, and the run lands without
/// a `needs_session` or a resume.
#[test]
fn a_passed_run_that_conflicts_with_main_is_rebased_by_its_live_session_and_lands() {
    let (_dir, repo, db) = fixture();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(1));
    let reviewer = TestReviewer::new(&[
        moving_main_then_pass(),
        verdict("pass", &[], "still meets it"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let run = detail.runs[0].clone();
    let moved = git_out(&repo, &["rev-parse", "main~1"]);
    assert_eq!(git_out(&repo, &["rev-parse", "main~2"]), seed);
    assert_landed(&repo, &run, "test task", &moved);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        "resolved by the resumed session\n"
    );
    let validated: Vec<&Value> = payloads(&detail, "validation_finished");
    assert_eq!(validated.len(), 2);
    let source = validated[0]["receipt"]["commit"].as_str().unwrap();
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1);
    let precheck = prechecks[0];
    assert_eq!(precheck["main"], json!(moved));
    // The head that passed, untouched by the precheck.
    assert_eq!(precheck["head"], json!(source));
    assert_eq!(precheck["merge_base"], json!(seed));
    assert_eq!(precheck["conflicts"], json!(["change.txt"]));
    assert_eq!(precheck["attempt"], 1);
    assert_eq!(precheck["requested"], true);
    // Recorded to the millisecond (task 1197).
    assert!(precheck["sent_at"].is_f64());
    let resolved = payloads(&detail, "conflict_resolved");
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(resolved, [&json!({"attempt": 1, "head": head})]);
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{head}~1")]),
        moved,
        "the session rebased onto the main the request named"
    );
    let verdicts: Vec<&Value> = payloads(&detail, "review_finished")
        .iter()
        .map(|p| &p["verdict"])
        .collect();
    assert_eq!(verdicts, [&json!("pass"), &json!("pass")]);
    let kinds = event_kinds(&detail);
    for (earlier, later) in [
        ("review_finished", "conflict_precheck"),
        ("conflict_precheck", "conflict_resolved"),
        ("conflict_resolved", "exit_requested"),
        ("exit_requested", "workspace_closed"),
        ("workspace_closed", "integration_started"),
    ] {
        assert!(
            position(&kinds, earlier) < position(&kinds, later),
            "{earlier} before {later}: {kinds:?}"
        );
    }
    for absent in ["resume_started", "integration_deferred", "revise_requested"] {
        assert!(!kinds.contains(&absent), "{absent} in {kinds:?}");
    }
    assert_eq!(payloads(&detail, "integration_started").len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    // The request is the resume's, for the live session.
    let texts = backend.texts();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].0, WORKSPACE_ID);
    let text = &texts[0].1;
    for expected in [
        format!(
            "dagq: the supervisor's review of run {} (task 1) passed, but integrate would conflict with main, so the run was not landed.",
            run.id()
        ),
        format!(
            "Reason: git merge-tree finds that main {moved} conflicts with the run in change.txt"
        ),
        format!("main is now {moved} (your base commit was {seed})."),
        "Tasks landed on main since your base: none.".to_owned(),
        format!("1. In this worktree run git rebase {moved} and resolve the conflicts."),
        "[\"test -f seed.txt\"]".to_owned(),
        "3. Keep the worktree clean.".to_owned(),
        runtime::STOP_BACKGROUND.to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "Do not merge or push. When done, report briefly and stop; do not run /exit.".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected:?} not in {text}");
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    assert_eq!(
        &fs::read_to_string(run_dir.join("conflict-1.txt")).unwrap(),
        text
    );
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(run_attention_of(&runtime::status(&db).unwrap(), run.id()).is_none());
}

/// A passed run that merges cleanly with main is not sent anything: no
/// `conflict_precheck`, one `/exit`, and the landing, as before.
#[test]
fn a_passed_run_that_merges_cleanly_with_main_lands_without_a_request() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Main moves during the review, but in another file.
    let reviewer = TestReviewer::new(&[format!(
        "cd \"$(git rev-parse --path-format=absolute --git-common-dir)/..\" && \
         printf 'other\\n' > other.txt && git add other.txt && \
         git commit -q -m 'main moves elsewhere' && {}",
        verdict("pass", &[], "meets the acceptance")
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "integrated", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let moved = git_out(&repo, &["rev-parse", "main~1"]);
    assert_landed(&repo, &detail.runs[0], "test task", &moved);
    assert!(repo.join("other.txt").is_file());
    assert!(payloads(&detail, "conflict_precheck").is_empty());
    assert!(backend.texts().is_empty());
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(reviewer.prompts().len(), 1);
}

/// [`fixture`] whose supervisors look for a sign of work one second after
/// a request (`[stall].send_confirm_secs` in the main checkout's
/// `dagq.toml`, task 546).
fn confirming_fixture() -> (Fixture, PathBuf, PathBuf) {
    let (dir, repo, db) = fixture();
    fs::write(repo.join("dagq.toml"), "[stall]\nsend_confirm_secs = 1\n").unwrap();
    git(&repo, &["add", "dagq.toml"]);
    git(&repo, &["commit", "-q", "-m", "stall"]);
    (dir, repo, db)
}

/// Runs task 1 under a supervisor that died after it recorded a request to
/// the live session, with `events` as what it recorded after the
/// validation, and lets another supervisor adopt the run with `reviewer`.
/// The request's text `message` is written to its file in the run
/// directory (`revise-1.txt` or `conflict-1.txt`, after the request's event),
/// and with `typed` the dead supervisor typed it: the session reads it and
/// is at work. Returns the adopted run's detail once the supervisor returns.
fn adopt_pending_request(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    events: impl FnOnce(&str, i64) -> Vec<(&'static str, Value)>,
    message: impl FnOnce() -> String,
    typed: bool,
) -> dagq::domain::TaskDetail {
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
    // The request is sent a second after the session's idle marker, which
    // then predates it.
    await_second_after(modified_second(&idle));
    let sent_at = unix_second_now();
    let mut queue = SqliteQueue::open(db).unwrap();
    let events = events(&head, sent_at);
    let file = events.iter().rev().find_map(|(kind, _)| match *kind {
        "revise_requested" => Some("revise-1.txt"),
        "conflict_precheck" => Some("conflict-1.txt"),
        _ => None,
    });
    for (kind, payload) in events {
        queue
            .record_runtime_event(run.id(), EventKind::from_name(kind).unwrap(), payload)
            .unwrap();
    }
    let run_dir = Path::new(run.run_dir().unwrap());
    let message = message();
    if let Some(file) = file {
        fs::write(run_dir.join(file), &message).unwrap();
    }
    if typed {
        *backend.screen.lock().unwrap() = WORKING_SCREEN.into();
        fs::write(resume_message_path(run.run_dir().unwrap()), message).unwrap();
    }
    age_lease(db, &run, 31);
    let outcome = supervise_reviewed(db, repo, backend, reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    detail
}

/// A conflict request recorded as sent (`requested: true`, recorded before
/// the text is typed) is not sent again by the supervisor that adopts the
/// run: it waits for the live session to resolve it, then validates,
/// reviews, and lands the run.
#[test]
fn an_adopted_run_with_a_pending_conflict_request_waits_without_sending_it_again() {
    let (_dir, repo, db) = confirming_fixture();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(1));
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let moved = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "still meets it")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |head, sent_at| {
            vec![
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                ),
                ("review_started", json!({"attempt": 1})),
                (
                    "review_finished",
                    json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
                ),
                (
                    "conflict_precheck",
                    json!({
                        "code": "rebase_conflict",
                        "main": moved,
                        "head": head,
                        "conflicts": ["change.txt"],
                        "attempt": 1,
                        "requested": true,
                        "sent_at": sent_at,
                    }),
                ),
            ]
        },
        || format!("main is now {moved} (your base commit was {seed})."),
        true,
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &moved);
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 1, "{prechecks:?}");
    let head = git_out(
        &repo,
        &["rev-parse", &format!("refs/dagq/runs/{}", run.id())],
    );
    assert_eq!(
        payloads(&detail, "conflict_resolved"),
        [&json!({"attempt": 1, "head": head})]
    );
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    assert!(!event_kinds(&detail).contains(&"resume_started"));
}

/// A revise request recorded as sent is not sent again by the supervisor
/// that adopts the run either: the live session's rewritten receipt is
/// validated, reviewed, and landed.
#[test]
fn an_adopted_run_with_a_pending_revise_waits_without_sending_it_again() {
    let (_dir, repo, db) = confirming_fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fixed")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| {
            vec![
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
            ]
        },
        || "dagq: the supervisor's review asks for changes (revise 1 of 2).".to_owned(),
        true,
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert_eq!(payloads(&detail, "revise_requested").len(), 1);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// The events of a supervisor that recorded revise request 1 at `sent_at`.
fn pending_revise(sent_at: i64) -> Vec<(&'static str, Value)> {
    vec![
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
    ]
}

const REVISE_TEXT: &str = "dagq: the supervisor's review asks for changes (revise 1 of 2).";

/// A revise request recorded but never typed (the supervisor died in
/// between) is not waited for up to the resume timeout by the supervisor
/// that adopts the run (task 546): its session shows no sign of it within
/// `[stall].send_confirm_secs` and its input box is empty, so the request
/// written to `revise-1.txt` is sent once (`submit_resent`), and the
/// session's rewritten receipt is reviewed and landed.
#[test]
fn an_adopted_revise_the_session_never_got_is_sent_again_and_lands() {
    let (_dir, repo, db) = confirming_fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_agent(1));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fixed")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| pending_revise(sent_at),
        || REVISE_TEXT.to_owned(),
        false,
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    assert_eq!(
        backend.texts(),
        [(WORKSPACE_ID.to_owned(), REVISE_TEXT.to_owned())]
    );
    let resent = payloads(&detail, "submit_resent");
    assert_eq!(resent.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(resent[0]["what"], "revise request");
    assert_eq!(resent[0]["waited_secs"], 1);
    assert!(payloads(&detail, "submit_not_started").is_empty());
    assert_eq!(payloads(&detail, "revise_requested").len(), 1);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// A conflict request recorded but never typed is sent once by the
/// supervisor that adopts the run, as a revise is (task 546): the session
/// rebases onto the main it names, and the run is reviewed and lands.
#[test]
fn an_adopted_conflict_request_the_session_never_got_is_sent_again_and_lands() {
    let (_dir, repo, db) = confirming_fixture();
    let seed = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &rebasing_agent(1));
    fs::write(repo.join("change.txt"), "main moved\n").unwrap();
    git(&repo, &["add", "change.txt"]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let moved = git_out(&repo, &["rev-parse", "main"]);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "still meets it")]);
    let text = format!("main is now {moved} (your base commit was {seed}).");
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |head, sent_at| {
            vec![
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                ),
                ("review_started", json!({"attempt": 1})),
                (
                    "review_finished",
                    json!({"verdict": "pass", "reasons": [], "summary": "ok", "attempt": 1}),
                ),
                (
                    "conflict_precheck",
                    json!({
                        "code": "rebase_conflict",
                        "main": moved,
                        "head": head,
                        "conflicts": ["change.txt"],
                        "attempt": 1,
                        "requested": true,
                        "sent_at": sent_at,
                    }),
                ),
            ]
        },
        || text.clone(),
        false,
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &moved);
    assert_eq!(backend.texts(), [(WORKSPACE_ID.to_owned(), text)]);
    let resent = payloads(&detail, "submit_resent");
    assert_eq!(resent.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(resent[0]["what"], "conflict request");
    assert!(payloads(&detail, "submit_not_started").is_empty());
    assert_eq!(payloads(&detail, "conflict_precheck").len(), 1);
    assert_eq!(payloads(&detail, "conflict_resolved").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
}

/// Adopts a revise request `events` recorded that its session never gets,
/// the session exiting once the `stalled` ask is open.
fn adopt_lost_revise(
    events: impl FnOnce(i64) -> Vec<(&'static str, Value)>,
) -> (
    Fixture,
    PathBuf,
    TestWorkspace,
    TestReviewer,
    dagq::domain::TaskDetail,
) {
    let (dir, repo, db) = confirming_fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        "commit work; receipt \"$(git rev-parse HEAD)\"; idle; \
         until \"$DAGQ\" asks --open | grep -q stalled; do sleep 0.05; done",
    );
    backend.dropped_texts.store(usize::MAX, Ordering::SeqCst);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| events(sent_at),
        || REVISE_TEXT.to_owned(),
        false,
    );
    (dir, db, backend, reviewer, detail)
}

/// A supervisor that adopts a request an earlier supervisor's check sent
/// once more already (`submit_resent`, as after an exec handoff) does not
/// send it again: it records `submit_not_started` (task 546).
#[test]
fn an_adopted_request_already_sent_again_is_not_sent_a_third_time() {
    let (_dir, _db, backend, _reviewer, detail) = adopt_lost_revise(|sent_at| {
        let mut events = pending_revise(sent_at);
        events.push((
            "submit_resent",
            json!({"workspace_id": WORKSPACE_ID, "what": "revise request", "waited_secs": 1}),
        ));
        events
    });
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    let not_started = payloads(&detail, "submit_not_started");
    assert_eq!(not_started.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(not_started[0]["resent"], true);
}

/// An adopted request that is lost again after it is sent once more is not
/// sent a third time: the run records `submit_not_started`, and its
/// recovery job escalates it to the `stalled` ask in the inbox (task 546).
/// The session here exits once that ask is open.
#[test]
fn an_adopted_request_lost_again_is_asked_to_the_inbox() {
    let (_dir, db, backend, reviewer, detail) = adopt_lost_revise(pending_revise);
    assert_eq!(backend.texts().len(), 1, "{:?}", backend.texts());
    assert_eq!(payloads(&detail, "submit_resent").len(), 1);
    let not_started = payloads(&detail, "submit_not_started");
    assert_eq!(not_started.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(not_started[0]["what"], "revise request");
    assert_eq!(not_started[0]["resent"], true);
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    let queue = SqliteQueue::open(&db).unwrap();
    let asks = queue
        .asks(AskQuery {
            all: true,
            ..Default::default()
        })
        .unwrap();
    let stalled: Vec<_> = asks.iter().filter(|a| a.kind == AskKind::Stalled).collect();
    assert_eq!(stalled.len(), 1, "{asks:?}");
    assert_eq!(stalled[0].run_id.as_ref(), Some(detail.runs[0].id()));
    assert!(
        asks.iter().any(|a| a.kind == AskKind::ApproveLanding),
        "{asks:?}"
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
}

/// A revise request that cannot be typed is withdrawn: the
/// `revise_requested` recorded before the send is followed by
/// `revise_unsent`, and a person is asked after the session's `/exit`.
#[test]
fn a_revise_that_cannot_be_sent_is_withdrawn_and_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.text_fails = true;
    let reviewer = TestReviewer::new(&[verdict("revise", &["add a line"], "one gap")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(outcome["runs"][0]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "revise_requested") < position(&kinds, "revise_unsent"));
    assert!(position(&kinds, "revise_unsent") < position(&kinds, "exit_requested"));
    let unsent = payloads(&detail, "revise_unsent");
    assert_eq!(unsent[0]["attempt"], 1);
    assert!(
        unsent[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("the revise request could not be sent")
    );
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
}

/// A conflict request that cannot be typed is withdrawn by a
/// `conflict_precheck` with `unsent: true`, and the run lands as without a
/// session to ask: the rebase conflicts and parks it for a resume.
#[test]
fn a_conflict_request_that_cannot_be_sent_is_withdrawn_and_the_run_lands() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    backend.text_fails = true;
    let reviewer = TestReviewer::new(&[moving_main_then_pass()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    // The test backend has no resume script: the parked run stays parked.
    assert_eq!(outcome["runs"][0]["status"], "needs_session", "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let prechecks = payloads(&detail, "conflict_precheck");
    assert_eq!(prechecks.len(), 2, "{prechecks:?}");
    assert_eq!(prechecks[0]["requested"], true);
    assert_eq!(prechecks[0]["conflicts"], json!(["change.txt"]));
    assert_eq!(prechecks[1]["requested"], false);
    assert_eq!(prechecks[1]["unsent"], true);
    assert_eq!(prechecks[1]["attempt"], 1);
    assert!(prechecks[1].get("conflicts").is_none());
    assert!(
        prechecks[1]["error"]
            .as_str()
            .unwrap()
            .starts_with("the request could not be sent")
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "conflict_precheck") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "exit_requested") < position(&kinds, "integration_started"));
    assert!(!kinds.contains(&"conflict_resolved"), "{kinds:?}");
}

/// A supervisor that adopts a run after a withdrawn revise request
/// (`revise_unsent`) does not send it: it exits the session and asks a
/// person, as the supervisor that could not send it was doing.
#[test]
fn an_adopted_run_with_a_withdrawn_revise_asks_a_person_without_sending_it() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("concern", &["x"], "never")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| {
            vec![
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
                    "revise_unsent",
                    json!({"attempt": 1, "error": "the revise request could not be sent: injected"}),
                ),
            ]
        },
        String::new,
        false,
    );
    assert!(backend.texts().is_empty(), "{:?}", backend.texts());
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    assert_eq!(backend.exits_sent.load(Ordering::SeqCst), 1);
    let queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind, dagq::domain::AskKind::ApproveLanding);
    assert!(
        asks[0]
            .question
            .contains("the revise request could not be sent: injected"),
        "{}",
        asks[0].question
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
}
