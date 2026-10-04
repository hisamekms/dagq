//! Runtime tests: a passed run that conflicts with a moving main, and the
//! requests an adopting supervisor sends again.
use crate::{common, runtime_support};
use dagq::domain::EventKind;

use runtime_support::headless::request_turn_left_by_a_dead_supervisor;
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

/// The headless worker's turns: its first commits, and each later one (a
/// conflict request) rebases onto the main the request names, resolves
/// `change.txt` and rewrites the receipt, up to `requests` times.
pub(crate) fn rebasing_agent(requests: usize) -> String {
    format!(
        "if [ \"$TURN\" -eq 1 ]; then commit work; receipt \"$(git rev-parse HEAD)\"\n\
         elif [ \"$TURN\" -le {} ]; then\n{RESUME_TURN_PRELUDE}\n\
         await_message; resolve || exit 1; receipt \"$(git rev-parse HEAD)\"\n\
         fi",
        requests + 1
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
    assert_exit_sent(&backend, &run, 1);
    assert_eq!(backend.closed(), vec![WORKSPACE_ID.to_owned()]);
    // The request is the resume's, for the live session.
    let texts = session_texts(&run);
    assert_eq!(texts.len(), 1);
    let text = &texts[0];
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
        // The headless worker's own lines: its turn is its reply.
        "Before you end the turn, stop every process you started".to_owned(),
        format!(
            "Rewrite the receipt at {} with the new head commit",
            run.receipt_path().unwrap()
        ),
        "Do not merge or push. Follow the repository's instructions for a worker".to_owned(),
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
/// `conflict_precheck`, one exit request, and the landing, as before.
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
    assert!(session_texts(&detail.runs[0]).is_empty());
    assert_exit_sent(&backend, &detail.runs[0], 1);
    assert_eq!(reviewer.prompts().len(), 1);
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
        &[],
    );
    let texts = session_texts(&detail.runs[0]);
    assert!(texts.is_empty(), "{texts:?}");
    assert!(reviewer.prompts().is_empty(), "reviewed again");
    assert_exit_sent(&backend, &detail.runs[0], 1);
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

/// A conflict request recorded as sent (`requested: true`, recorded before
/// it is written) and written as the headless session's next request is
/// not sent again by the supervisor that adopts the run: it waits for the
/// live session's turn of it, which rebases and rewrites the receipt, then
/// validates, reviews, and lands the run. Moved from the deleted
/// interactive
/// `an_adopted_run_with_a_pending_conflict_request_waits_without_sending_it_again`
/// (task 1437).
#[test]
fn an_adopted_headless_run_with_a_pending_conflict_request_waits_without_sending_it_again() {
    let (_dir, repo, db) = fixture();
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
        &[],
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &moved);
    assert_not_requested_again(&detail, &run, "conflict request");
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
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{head}~1")]),
        moved,
        "the session rebased onto the main the request named"
    );
    assert_eq!(reviewer.prompts().len(), 1);
    assert_exit_sent(&backend, &run, 1);
    assert!(!event_kinds(&detail).contains(&"resume_started"));
}

/// A revise request recorded as sent and written as the headless session's
/// next request is not sent again by the supervisor that adopts the run
/// either: it waits for the session's turn of it, and the rewritten receipt
/// is validated, reviewed, and landed. Moved from the deleted interactive
/// `an_adopted_run_with_a_pending_revise_waits_without_sending_it_again`
/// (task 1437).
#[test]
fn an_adopted_headless_run_with_a_pending_revise_waits_without_sending_it_again() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &crate::runtime_review::revising_agent(1));
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
        &[],
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    assert_not_requested_again(&detail, &run, "revise request");
    assert_eq!(payloads(&detail, "revise_requested").len(), 1);
    assert_eq!(payloads(&detail, "revise_finished").len(), 1);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_exit_sent(&backend, &run, 1);
}

/// A headless run whose supervisor stopped after it recorded its second
/// revise request (`revise_requested` of attempt 2) and before it wrote it
/// to the session's `turns/` has the request written once by the
/// supervisor that adopts it (task 1683): the first attempt's request,
/// taken with the same `what`, is no delivery of the second. The session
/// runs it as its next turn, and the rewritten receipt is reviewed again
/// and landed.
#[test]
fn an_adopted_revise_recorded_but_not_written_is_written_once() {
    const FIRST: &str = "dagq: the supervisor's review asks for changes (revise 1 of 2).";
    const SECOND: &str = "dagq: the supervisor's review asks for changes (revise 2 of 2).";
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &crate::runtime_review::revising_agent(1));
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fixed")]);
    let detail = adopt_pending_request(
        &repo,
        &db,
        &backend,
        &reviewer,
        |_, sent_at| {
            let review = |attempt: u64| {
                [
                    ("review_started", json!({"attempt": attempt})),
                    (
                        "review_finished",
                        json!({"verdict": "revise", "reasons": ["add a line"], "summary": "one gap", "attempt": attempt}),
                    ),
                    (
                        "revise_requested",
                        json!({"attempt": attempt, "reasons": ["add a line"], "sent_at": sent_at}),
                    ),
                ]
            };
            let mut events = vec![(
                "validation_finished",
                json!({"status": "awaiting_integration"}),
            )];
            events.extend(review(1));
            events.push((
                "turn_requested",
                json!({"seq": 1, "what": "revise request", "workspace_id": "w"}),
            ));
            events.push(("revise_finished", json!({"attempt": 1})));
            events.extend(review(2));
            events
        },
        || SECOND.to_owned(),
        false,
        &[(FIRST, "revise request")],
    );
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\n", run.id())
    );
    // The adopter wrote the second attempt's request once, after the
    // first, and the session ran it as its second turn.
    let requested: Vec<&Value> = payloads(&detail, "turn_requested")
        .into_iter()
        .filter(|p| p["what"] == "revise request")
        .collect();
    assert_eq!(
        requested
            .iter()
            .map(|p| p["seq"].clone())
            .collect::<Vec<_>>(),
        [json!(1), json!(2)]
    );
    let run_dir = Path::new(run.run_dir().unwrap());
    let written: dagq::domain::turn::TurnRequest = serde_json::from_str(
        &fs::read_to_string(dagq::domain::turn::taken_path(run_dir, 2)).unwrap(),
    )
    .unwrap();
    assert_eq!(written.prompt, SECOND);
    assert!(!dagq::domain::turn::request_path(run_dir, 3).exists());
    assert!(
        !dagq::domain::turn::taken_path(run_dir, 3).exists(),
        "written twice"
    );
    assert_eq!(stub_calls(&run).len(), 2);
    assert_eq!(payloads(&detail, "revise_requested").len(), 2);
    assert_eq!(reviewer.prompts().len(), 1);
    assert_exit_sent(&backend, &run, 1);
}

/// The adopted request `what` was taken as one turn and never written
/// again: the session ran its first turn and the turn of the dead
/// supervisor's request, one request file holds it, and only the dead
/// supervisor's `turn_requested` names it.
fn assert_not_requested_again(detail: &dagq::domain::TaskDetail, run: &TaskRun, what: &str) {
    use dagq::domain::turn::turns_dir;
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(session_texts(run).len(), 1);
    let requested: Vec<&Value> = payloads(detail, "turn_requested")
        .into_iter()
        .filter(|p| p["what"] == what)
        .collect();
    assert_eq!(requested.len(), 1, "{requested:?}");
    let requests = fs::read_dir(turns_dir(Path::new(run.run_dir().unwrap())))
        .unwrap()
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap_or_default())
        .filter(|content| content.contains(&format!("\"what\":\"{what}\"")))
        .count();
    assert_eq!(requests, 1);
}

/// A conflict request that cannot be written as the headless session's next
/// request (another writer holds the requests' lock past its wait) is
/// withdrawn by a `conflict_precheck` with `unsent: true`, and the run goes
/// on to land as without a session to ask: the session is asked to exit,
/// and the rebase conflicts and parks the run for a resume. Moved from the
/// deleted interactive
/// `a_conflict_request_that_cannot_be_sent_is_withdrawn_and_the_run_lands`
/// (task 1437).
#[test]
fn a_conflict_request_that_cannot_be_written_is_withdrawn_and_the_run_lands() {
    let (_dir, repo, db) = fixture();
    let backend = Arc::new(TestWorkspace::new(&db, false, IDLE_AGENT));
    let reviewer = Arc::new(TestReviewer::new(&[moving_main_then_pass()]));
    let supervisor = {
        let (db, repo, backend, reviewer) =
            (db.clone(), repo.clone(), backend.clone(), reviewer.clone());
        thread::spawn(move || supervise_reviewed(&db, &repo, &backend, &reviewer))
    };
    let mut run_dir = None;
    wait_until(&db, common::STEP_LIMIT, |queue| {
        run_dir = queue
            .show(TaskId::new(1))
            .unwrap()
            .runs
            .first()
            .and_then(|run| run.run_dir().map(PathBuf::from))
            .filter(|dir| dir.is_dir());
        run_dir.is_some()
    });
    // The lock the writers of a headless session's requests take
    // (`turns.lock` next to its `turns/`), held from before the review to
    // the end: the request's writer gives up after about two seconds. The
    // exit request takes no lock.
    let lock = fs::File::create(run_dir.unwrap().join("turns.lock")).unwrap();
    lock.lock().unwrap();
    let outcome = joined(supervisor, "the supervisor thread to return");
    drop(lock);
    // The test backend has no resume script: the parked run stays parked.
    assert_eq!(outcome["runs"][0]["status"], "needs_session", "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
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
            .starts_with("the request could not be sent"),
        "{}",
        prechecks[1]
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "conflict_precheck") < position(&kinds, "exit_requested"));
    assert!(position(&kinds, "exit_requested") < position(&kinds, "integration_started"));
    assert!(!kinds.contains(&"conflict_resolved"), "{kinds:?}");
    // Nothing reached the session after its first turn.
    assert_eq!(stub_calls(&detail.runs[0]).len(), 1);
}

/// Runs task 1 under a supervisor that died after it recorded a request to
/// the live session, with `events` as what it recorded after the
/// validation, and lets another supervisor adopt the run with `reviewer`.
/// The request's text `message` is written to its file in the run
/// directory (`revise-1.txt` or `conflict-1.txt`, after the request's
/// event), and with `typed` the dead supervisor also wrote it as the
/// headless session's next request, which the session takes and runs as
/// its turn. Each of `taken` (a prompt and what it is) is left in `turns/`
/// as a request an earlier attempt wrote and the session took, before the
/// events are recorded. Returns the adopted run's detail once the
/// supervisor returns.
#[allow(clippy::too_many_arguments)]
fn adopt_pending_request(
    repo: &Path,
    db: &Path,
    backend: &TestWorkspace,
    reviewer: &TestReviewer,
    events: impl FnOnce(&str, i64) -> Vec<(&'static str, Value)>,
    message: impl FnOnce() -> String,
    typed: bool,
    taken: &[(&str, &str)],
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
    let run_dir = Path::new(run.run_dir().unwrap());
    // Written straight under its taken name: a pending request would be
    // taken by the live session, which polls `turns/`.
    for (prompt, what) in taken {
        use dagq::domain::turn::{TurnRequest, next_seq, taken_path, turns_dir};
        let turns = turns_dir(run_dir);
        fs::create_dir_all(&turns).unwrap();
        let names: Vec<String> = fs::read_dir(&turns)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let seq = next_seq(names.iter().map(String::as_str));
        let request = TurnRequest {
            seq,
            what: (*what).to_owned(),
            prompt: (*prompt).to_owned(),
        };
        let path = taken_path(run_dir, seq);
        let written = run_dir.join("taken-request.tmp");
        fs::write(&written, serde_json::to_string(&request).unwrap()).unwrap();
        fs::rename(&written, &path).unwrap();
    }
    let mut queue = SqliteQueue::open(db).unwrap();
    let events = events(&head, sent_at);
    // The request's file is the one of the last request's attempt.
    let file = events.iter().rev().find_map(|(kind, payload)| {
        let attempt = payload["attempt"].as_u64().unwrap_or(1);
        match *kind {
            "revise_requested" => Some(format!("revise-{attempt}.txt")),
            "conflict_precheck" => Some(format!("conflict-{attempt}.txt")),
            _ => None,
        }
    });
    for (kind, payload) in events {
        queue
            .record_runtime_event(run.id(), EventKind::from_name(kind).unwrap(), payload)
            .unwrap();
    }
    let message = message();
    if let Some(file) = &file {
        fs::write(run_dir.join(file), &message).unwrap();
    }
    if typed {
        let what = if file
            .as_deref()
            .is_some_and(|file| file.starts_with("conflict-"))
        {
            "conflict request"
        } else {
            "revise request"
        };
        request_turn_left_by_a_dead_supervisor(db, &run, &message, what);
    }
    age_lease(db, &run, 31);
    let outcome = supervise_reviewed(db, repo, backend, reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    detail
}
