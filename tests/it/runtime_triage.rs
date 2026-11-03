//! Runtime tests: The recovery job of failed runs, dead runs and silent
//! wrappers.
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::LeaseToken;

use runtime_support::*;

/// The events of `kind` of one run, in order.
fn run_payloads(detail: &dagq::domain::TaskDetail, run: &TaskRun, kind: &str) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|e| e.kind.as_str() == kind && e.run_id.as_ref() == Some(run.id()))
        .map(|e| e.payload.clone())
        .collect()
}

/// `escalate`: a `decide` ask for the inbox, the run stays `failed`; a
/// failure stopping a wrapper still running at the round's end is
/// recorded and the round goes on, and the wrapper that ended with its
/// agent is not stopped. The supervisor applies the answer (`cancel` here)
/// and closes the ask.
#[test]
fn an_escalation_waits_for_a_person_and_the_supervisor_applies_the_answer() {
    let (_dir, repo, db) = fixture();
    let mut backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    backend.close_fail = true;
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")])
        .with_triages(&[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "the acceptance cannot be met",
            "question": "Is task 1 still wanted?",
        }))])
        .leaving_sessions_running(&db, &backend);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    let finished = payloads(&detail, "triage_finished");
    assert_eq!(finished[0]["action"], "ask");
    assert_eq!(finished[0]["status"], "failed");
    let ask = queue
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    assert_eq!(ask.options, ["retry", "resume", "cancel"]);
    assert_eq!(ask.reason_category, dagq::domain::AskReason::RecoveryFailed);
    for part in [
        "the recovery job could not repair it",
        "Why a person: recovery_failed",
        "Diagnosis: the acceptance cannot be met",
        "Question: Is task 1 still wanted?",
        "Last error: session exited with code 1",
        "recovery-failed-1.prompt.txt",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
    // The supervisor notifies nobody: the inbox's watch tells of the ask.
    // The stop of the wrapper left running failed: recorded, the session
    // kept, the ask still made. The wrapper that ended with its agent was
    // not stopped (its process is gone, as production tells it).
    let cleanup = payloads(&detail, "cleanup_failed");
    assert_eq!(cleanup.len(), 1, "{cleanup:?}");
    let left = background_session(&run);
    assert_eq!(cleanup[0]["workspace_id"], left);
    assert!(
        cleanup[0]["message"]
            .as_str()
            .unwrap()
            .contains("injected session stop failure")
    );
    assert!(backend.exists(&left).unwrap());
    let ended = payloads(&detail, "workspace_created")[0]["workspace_id"].clone();
    assert_ne!(ended, json!(left));
    assert!(!backend.exists(ended.as_str().unwrap()).unwrap());
    assert!(backend.closed().is_empty());
    assert!(run.workspace_closed_at().is_none());
    assert!(!event_kinds(&detail).contains(&"workspace_closed"));
    assert_eq!(run.last_error(), Some("session exited with code 1"));
    let status = runtime::status(&db).unwrap();
    assert!(run_attention_of(&status, run.id()).is_none(), "{status}");

    // An answer the supervisor applies is its own, not a person's.
    queue.answer(ask.id, "cancel").unwrap();
    let answered = queue
        .run_events(run.id())
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::AskAnswered)
        .unwrap();
    assert_eq!(answered.payload["runtime_delivers"], true);
    assert_eq!(
        ask_attention(&runtime::status(&db).unwrap(), ask.id)[0]["next"],
        format!("applying the answer of ask {} (runtime)", ask.id)
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(detail.runs[0].status(), RunStatus::Failed);
    let decided = payloads(&detail, "triage_decided");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["answer"], "cancel");
    assert_eq!(decided[0]["ask_id"], ask.id.as_i64());
    // The answer it applied is the cancel's reason (ADR-t947-4 decision 3).
    let canceled = payloads(&detail, "task_status_changed").pop().unwrap();
    assert_eq!(
        (&canceled["reason"], &canceled["by"], &canceled["ask_id"]),
        (&json!("answered_cancel"), &json!("decide"), &json!(ask.id))
    );
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    assert!(ask_attention(&runtime::status(&db).unwrap(), ask.id).is_empty());
}

/// `retry_inherit` of a run whose branch has commits (task 358's retry):
/// the task is ready again and its next run carries the branch over. That
/// run fails too, and its recovery job cannot start: nothing moves, and the
/// run waits to be recovered by hand (`triage_failed`, ADR-0047 decision 40).
/// A low-confidence repair and a second `retry_inherit` of the task are not
/// applied.
#[test]
fn retry_inherit_carries_the_branch_over_once_and_a_failed_job_is_recovered_by_hand() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[repair(
        json!({"action": "retry_inherit"}),
        "a flaky test killed the session after the work was done",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let (first, second) = (detail.runs[0].clone(), detail.runs[1].clone());
    let finished = run_payloads(&detail, &first, "triage_finished");
    assert_eq!(finished[0]["action"], "retry_inherit");
    let head = finished[0]["inherit"]["head"].as_str().unwrap().to_owned();
    assert_eq!(
        finished[0]["inherit"]["branch"],
        json!(format!("dagq/{}", first.id()))
    );
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", first.id())]
        ),
        head
    );
    let repaired = run_payloads(&detail, &first, "auto_repaired");
    assert_eq!(repaired[0]["repair"], "retry_inherit");
    assert_eq!(repaired[0]["conditions"]["own_commits"], true);
    let inherited = run_payloads(&detail, &second, "run_inherited");
    assert_eq!(inherited[0]["inherit_from_run"], json!(first.id()));
    assert_eq!(inherited[0]["head"], json!(head));

    // The second run's job cannot start: recover it by hand.
    assert_eq!(second.status(), RunStatus::Failed);
    let failed = run_payloads(&detail, &second, "triage_failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["alert"], "failed");
    assert_eq!(failed[0]["code"], "job_failed");
    assert!(
        failed[0]["error"]
            .as_str()
            .unwrap()
            .contains("the recovery job could not start")
    );
    let recovered = run_payloads(&detail, &second, "recovery_finished");
    assert_eq!(recovered[0]["outcome"], "job_failed");
    assert_eq!(recovered[0]["reason_category"], "recovery_failed");
    let status = runtime::status(&db).unwrap();
    assert_eq!(
        run_attention_of(&status, second.id()).unwrap()["next"],
        "triage by hand",
        "{status}"
    );
    assert!(other_asks(&mut queue, true).is_empty());

    // Handed back (as a person would, by hand): a repair of low confidence
    // and a second retry_inherit of the task are asked, not applied.
    Connection::open(&db)
        .unwrap()
        .execute(
            "DELETE FROM run_events WHERE run_id=?1 AND kind='triage_failed'",
            [second.id()],
        )
        .unwrap();
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(json!({
            "verdict": "repair",
            "confidence": "high",
            "diagnosis": "again",
            "actions": [{"action": "retry_inherit"}],
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    let finished = run_payloads(&detail, &second, "triage_finished");
    assert_eq!(finished.last().unwrap()["action"], "ask");
    let ask = queue
        .read_ask(AskId::new(
            finished.last().unwrap()["ask_id"].as_i64().unwrap(),
        ))
        .unwrap();
    assert!(
        ask.question
            .contains("was retried with a branch carried over already"),
        "{}",
        ask.question
    );
    let requested = run_payloads(&detail, &second, "recovery_requested");
    assert_eq!(requested.len(), 2);
    assert_eq!(requested[1]["attempt"], 2);
}

/// A triage round's `resume` reads again, in its own transaction, whether
/// recovery may still bring the run back
/// (`domain::telemetry::run_revivable`, whose cases its unit test holds):
/// a task canceled, or given a later run, while the round ran refuses it
/// and leaves the run as it was, and once the run is the latest of a task
/// that goes on the same round resumes it.
#[test]
fn a_resume_is_refused_once_the_runs_task_ended_or_has_a_later_run_while_its_round_ran() {
    use dagq::application::TriageAction;
    use dagq::domain::actor_model::{ActorLaunch, ModelRole};
    let (_dir, repo, db) = fixture();
    let run = provision_under(&repo, &db, "first");
    // The queue's clock is ahead, so the claim's lease is stale.
    let clock = ManualClock::at(unix_second_now() + 10 * dagq::domain::HEARTBEAT_TIMEOUT_SECS);
    let mut queue = SqliteQueue::open(&db).unwrap().with_generators(Generators {
        clock: Arc::new(clock),
        ids: Arc::new(clock::UuidGenerator),
    });
    let run = queue.recover_run(run.id(), 0, json!({})).unwrap();
    assert_eq!(run.status(), RunStatus::Interrupted);
    let job = LeaseToken::new("job");
    let set_task = |status: &str| {
        Connection::open(&db)
            .unwrap()
            .execute("UPDATE tasks SET status=?1 WHERE id=1", [status])
            .unwrap();
    };
    let resume = |queue: &mut SqliteQueue| {
        queue.finish_triage(
            run.id(),
            &job,
            &TriageAction::Resume {
                instruction: "go on".into(),
            },
            json!({}),
            Vec::new(),
        )
    };
    assert!(
        queue
            .begin_triage(
                run.id(),
                &job,
                None,
                &ActorLaunch::default_of(ModelRole::Recovery),
            )
            .unwrap()
            .is_some()
    );
    // Canceled while the round ran: refused, the run stays interrupted.
    set_task("canceled");
    let refused = resume(&mut queue).unwrap_err().to_string();
    assert!(refused.contains("no longer the latest run"), "{refused}");
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::Interrupted
    );
    // The task goes on but has a later run: refused as well.
    set_task("in_progress");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(&format!(
        "CREATE TEMP TABLE later AS SELECT * FROM task_runs WHERE id='{}';
         UPDATE later SET id='later', status='claimed', branch=NULL,
           worktree_path=NULL, workspace_id=NULL;
         INSERT INTO task_runs SELECT * FROM later;",
        run.id()
    ))
    .unwrap();
    assert!(resume(&mut queue).is_err());
    // The run its task's latest again: the round's resume parks it.
    conn.execute("DELETE FROM task_runs WHERE id='later'", [])
        .unwrap();
    assert_eq!(
        resume(&mut queue).unwrap().status(),
        RunStatus::NeedsSession
    );
}

/// A run whose wrapper died and that nobody leases is recovered by the
/// supervisor (ADR-0024 decision 3, amending ADR-0012) and triaged as
/// `interrupted`; `retry` runs the task again, and it lands.
#[test]
fn a_dead_run_nobody_leases_is_recovered_triaged_and_retried() {
    let (_dir, repo, db) = fixture();
    let orphan = orphan_run(&repo, &db, "owner", dead_pid(), dead_pid());
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM run_leases", [])
        .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[repair(json!({"action": "retry"}), "the machine restarted")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].id(), orphan.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let events = queue.run_events(orphan.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let recovered = &events[position(&kinds, "run_recovered")].payload;
    assert_eq!(recovered["by"], "supervisor");
    assert_eq!(recovered["previous_status"], "running");
    assert_eq!(recovered["lease_deleted"], false);
    assert_eq!(
        events[position(&kinds, "triage_started")].payload["status"],
        "interrupted"
    );
    assert_eq!(
        events[position(&kinds, "triage_finished")].payload["action"],
        "retry"
    );
    // Its workspace is one cmux does not list: nothing to close.
    assert!(!kinds.contains(&"workspace_closed"));
    assert!(!kinds.contains(&"cleanup_failed"));
    let (prompt, _) = &reviewer.triage_prompts()[0];
    assert!(prompt.contains("which ended interrupted"), "{prompt}");
    assert!(prompt.contains("raised the alert interrupted"), "{prompt}");
    assert_eq!(
        events[position(&kinds, "recovery_requested")].payload["alert"],
        "interrupted"
    );
    assert_eq!(
        events[position(&kinds, "auto_repaired")].payload["repair"],
        "retry"
    );
}

/// A `failed` run given up on with its session left open (the `/exit`
/// could not be sent) is not a person's to end: the next supervisor's
/// recovery job takes it while that session lives, and the round's end
/// stops the session, so its attention stays `triaging (runtime)` (task
/// 751).
#[test]
fn a_failed_run_given_up_with_its_session_open_is_triaged_and_its_session_stopped() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "gone-supervisor", dead_pid(), dead_pid());
    // Its session lives on: a process standing for the wrapper.
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let session = backend.stand_in();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET workspace_id=?2 WHERE id=?1",
            [run.id().as_str(), &session],
        )
        .unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .abandon_run(
            run.id(),
            &LeaseToken::new("gone-supervisor"),
            "the workspace backend failed",
            &ReasonCode::Other.into(),
            Some(&json!({"workspace_id": "ws", "exit": "failed", "error": "x"})),
        )
        .unwrap();
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='failed' WHERE id=?1",
            [run.id()],
        )
        .unwrap();
    let attention = |db: &Path| run_attention_of(&runtime::status(db).unwrap(), run.id()).cloned();
    assert_eq!(attention(&db).unwrap()["next"], "triaging (runtime)");
    assert!(backend.exists(&session).unwrap());
    let reviewer = TestReviewer::new(&[]).with_triages(&[recovery(json!({
        "verdict": "escalate",
        "confidence": "high",
        "diagnosis": "a person decides",
        "question": "Is task 1 still wanted?",
    }))]);
    let outcome = supervise_reviewed_owned(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let events = queue.run_events(run.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(position(&kinds, "runtime_error") < position(&kinds, "triage_started"));
    let closed = &events[position(&kinds, "workspace_closed")].payload;
    assert_eq!(closed["by"], "triage");
    assert_eq!(closed["workspace_id"], session);
    assert!(position(&kinds, "triage_finished") < position(&kinds, "workspace_closed"));
    // The triage stops the session once and records it once.
    assert_eq!(
        kinds.iter().filter(|k| **k == "workspace_closed").count(),
        1,
        "{kinds:?}"
    );
    assert!(!backend.exists(&session).unwrap());
    // The round asked a person, whose ask is the attention, not the run.
    assert!(kinds.contains(&"ask_opened"), "{kinds:?}");
    assert_eq!(attention(&db), None);
}

/// A run whose supervisor and wrapper both died keeps the dead supervisor's
/// stale lease. Nobody adopts it (its wrapper is dead), so the next
/// supervisor recovers it itself as it does a run nobody leases (task 236),
/// triages it as `interrupted`, and `retry` runs the task again to landing.
#[test]
fn a_dead_run_whose_dead_supervisor_still_leases_it_is_recovered_and_retried() {
    let (_dir, repo, db) = fixture();
    let orphan = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET pid=?1, heartbeat_at=unixepoch()-31",
            [dead_pid()],
        )
        .unwrap();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_triages(&[repair(json!({"action": "retry"}), "the machine restarted")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2);
    assert_eq!(detail.runs[0].id(), orphan.id());
    assert_eq!(detail.runs[0].status(), RunStatus::Interrupted);
    assert_landed_run(&detail.runs[1], &repo, &base);
    let events = queue.run_events(orphan.id()).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(!kinds.contains(&"run_adopted"), "{kinds:?}");
    let recovered = &events[position(&kinds, "run_recovered")].payload;
    assert_eq!(recovered["by"], "supervisor");
    assert_eq!(recovered["previous_status"], "running");
    assert_eq!(recovered["lease_deleted"], true);
    assert_eq!(recovered["run"]["lease"]["alive"], false);
    assert_eq!(
        events[position(&kinds, "triage_finished")].payload["action"],
        "retry"
    );
}

/// Commit a change in the worktree of `run` (a `running` orphan), write its
/// receipt and move it to `awaiting_integration` the way validation does,
/// leaving its lease and wrapper registration as they are: a run whose
/// supervisor died during its review.
pub(crate) fn validated_orphan(db: &Path, run: &TaskRun) -> String {
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(
        worktree.join("change.txt"),
        format!("change by {}\n", run.id()),
    )
    .unwrap();
    git(worktree, &["add", "change.txt"]);
    git(worktree, &["commit", "-q", "-m", "work"]);
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    write_receipt(run, &head, "succeeded", "done");
    Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE task_runs SET status='awaiting_integration', result_commit=?2 WHERE id=?1",
            rusqlite::params![run.id(), head],
        )
        .unwrap();
    SqliteQueue::open(db)
        .unwrap()
        .record_runtime_event(
            run.id(),
            EventKind::ValidationFinished,
            json!({"status": "awaiting_integration"}),
        )
        .unwrap();
    head
}

/// Make the wrapper of `run` one that died without recording its exit (a
/// dead pid and a heartbeat past the TTL) and the lease one a dead
/// supervisor left behind.
pub(crate) fn kill_supervisor_and_wrapper(db: &Path, run: &TaskRun) {
    let raw = Connection::open(db).unwrap();
    raw.execute(
        "UPDATE run_processes SET pid=?2, heartbeat_at=unixepoch()-31 WHERE run_id=?1",
        rusqlite::params![run.id(), dead_pid()],
    )
    .unwrap();
    raw.execute(
        "UPDATE run_leases SET pid=?2, heartbeat_at=unixepoch()-31 WHERE run_id=?1",
        rusqlite::params![run.id(), dead_pid()],
    )
    .unwrap();
}

/// The supervisor and the session's wrapper both died while an
/// `awaiting_integration` run was under review (task 236): the next
/// supervisor adopts it all the same, since its review needs no session,
/// reviews it with the session taken for ended, sends no `/exit` and lands
/// it, with nobody touching the queue.
#[test]
fn an_awaiting_run_whose_supervisor_and_wrapper_died_is_adopted_reviewed_and_landed() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 1);
    assert_landed_run(&detail.runs[0], &repo, &base);
    let adopted = adoption_events(&detail);
    assert_eq!(adopted.len(), 1, "{adopted:?}");
    assert_eq!(adopted[0]["previous_token"], "dead-supervisor");
    assert_eq!(adopted[0]["wrapper"]["alive"], false);
    let kinds = event_kinds(&detail);
    assert!(!kinds.contains(&"run_recovered"), "{kinds:?}");
    assert!(!kinds.contains(&"runtime_error"), "{kinds:?}");
    assert!(!kinds.contains(&"exit_requested"), "{kinds:?}");
    // The dead wrapper's silence is recorded once, by the exit before the
    // landing (task 1384).
    let expired = payloads(&detail, "wrapper_heartbeat_expired");
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0]["process_alive"], false);
    assert_eq!(expired[0]["phase"], "exit");
    assert_eq!(expired[0]["waiting"], false);
    assert_eq!(
        payloads(&detail, "review_started")[0]["session_live"],
        false
    );
    assert_exit_sent(&backend, &detail.runs[0], 0);
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A `revise` verdict for such a run has no session to revise it: the run
/// is asked about (`approve_landing`) instead of being given up.
#[test]
fn a_revise_for_an_adopted_run_whose_wrapper_died_asks_a_person() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("revise", &["add a test"], "one gap")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind, AskKind::ApproveLanding);
    assert!(
        asks[0]
            .question
            .contains("the session had ended, so nobody could revise the run"),
        "{}",
        asks[0].question
    );
    assert!(session_texts(&run).is_empty());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// Leave `run` the way a supervisor that died right after opening the
/// `approve_landing` ask of its failed review leaves it (task 424): review
/// 1 started, its session closed, the ask open and no `review_failed`.
/// Returns the ask's ID.
fn failed_review_asked_before_death(db: &Path, run: &TaskRun) -> AskId {
    failed_review_asked_before_death_at(db, run, 1, false)
}

/// As `failed_review_asked_before_death`, with the failed review being
/// review `attempt` (each earlier one retried for an unreadable verdict,
/// `review_retried`) and, when `recorded`, its `review_failed` with the ask
/// already recorded, as by a supervisor that died before giving the lease
/// back.
fn failed_review_asked_before_death_at(
    db: &Path,
    run: &TaskRun,
    attempt: u64,
    recorded: bool,
) -> AskId {
    let mut queue = SqliteQueue::open(db).unwrap();
    for n in 1..=attempt {
        if n > 1 {
            queue
                .record_runtime_event(
                    run.id(),
                    EventKind::ReviewRetried,
                    json!({"attempt": n - 1, "error": "expected `,` or `}`"}),
                )
                .unwrap();
        }
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ReviewStarted,
                json!({"attempt": n, "workspace_id": null, "session_live": false, "session_id": "s"}),
            )
            .unwrap();
    }
    let ask = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::ApproveLanding,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: format!("The supervisor's headless review of run r (task 1) failed and gave no verdict (review {attempt}): exit status 3\nReview material: r/review.md"),
            options: vec!["land".into(), "send_back".into(), "cancel".into()],
            asked_by: "supervisor".into(),
            reason_category: dagq::domain::AskReason::Scope,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    if recorded {
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ReviewFailed,
                json!({"code": "job_failed", "attempt": attempt, "error": "exit status 3", "status": "awaiting_integration", "ask_id": ask}),
            )
            .unwrap();
    }
    ask
}

/// A supervisor that died between the `approve_landing` ask of a failed
/// review and its `review_failed` (task 424): the next one adopts the run
/// without reviewing it again, records the `review_failed` with the ask and
/// gives the lease back, and the ask stays open for the person.
#[test]
fn an_adopted_run_whose_failed_review_was_asked_waits_for_the_ask() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let ask = failed_review_asked_before_death(&db, &run);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.prompts().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(adoption_events(&detail).len(), 1);
    assert_eq!(payloads(&detail, "review_started").len(), 1);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["ask_id"], json!(ask));
    assert_eq!(failed[0]["attempt"], 1);
    assert_eq!(failed[0]["error"], "exit status 3");
    let asks = queue.asks(AskQuery::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].id, ask);
    assert!(asks[0].answer.is_none());
    assert!(queue.run_leases().unwrap().is_empty());
    assert_exit_sent(&backend, &detail.runs[0], 0);
}

/// Such an ask answered before the adoption has its answer applied, as for
/// a run nobody leases, and is still not reviewed again.
#[test]
fn an_adopted_run_whose_failed_review_ask_was_answered_has_the_answer_applied() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let ask = failed_review_asked_before_death(&db, &run);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask, "cancel")
        .unwrap();
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.prompts().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Failed);
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Canceled);
    assert_eq!(payloads(&detail, "review_failed")[0]["ask_id"], json!(ask));
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// The failed review asked about is the second (task 592): review 1
/// printed an unreadable verdict and was retried, and review 2 failed and
/// was asked about. The adoption takes the ask after the last
/// `review_started`, records its `review_failed` as review 2 and does not
/// review the run again.
#[test]
fn an_adopted_run_whose_retried_review_was_asked_records_the_second_attempt() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let ask = failed_review_asked_before_death_at(&db, &run, 2, false);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.prompts().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.run(run.id()).unwrap().status(),
        RunStatus::AwaitingIntegration
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "review_started").len(), 2);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["ask_id"], json!(ask));
    assert_eq!(failed[0]["attempt"], 2);
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// An ask closed before the adoption (its answer applied) is not waited
/// for (task 592): the run
/// is adopted as any whose review started and gave no verdict, and is
/// reviewed again.
#[test]
fn an_adopted_run_whose_failed_review_ask_was_closed_is_reviewed_again() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let ask = failed_review_asked_before_death(&db, &run);
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue.answer(ask, "land").unwrap();
    queue.close_ask(ask).unwrap();
    drop(queue);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(!reviewer.prompts().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(payloads(&detail, "review_started").len(), 2);
    assert!(
        payloads(&detail, "review_failed")
            .iter()
            .all(|failed| failed["ask_id"] != json!(ask)),
        "{:?}",
        payloads(&detail, "review_failed")
    );
}

/// A supervisor that died after recording the failed review's
/// `review_failed` (with its ask) and before giving the lease back (task
/// 592): the adoption records no second `review_failed`, does not review
/// the run again, and gives the lease back.
#[test]
fn an_adopted_run_whose_failed_review_was_recorded_is_not_recorded_again() {
    let (_dir, repo, db) = fixture();
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let ask = failed_review_asked_before_death_at(&db, &run, 1, true);
    kill_supervisor_and_wrapper(&db, &run);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert!(reviewer.prompts().is_empty());
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["ask_id"], json!(ask));
    assert!(failed[0].get("adopted").is_none(), "{failed:?}");
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());
    assert!(queue.run_leases().unwrap().is_empty());
}

/// A supervisor that died after opening the `approve_landing` ask of a
/// `concern` and before giving the lease back (task 425): the next one
/// adopts the run and waits for that ask, open or answered, rather than
/// closing it as stale and asking again.
#[test]
fn an_adopted_run_whose_concern_was_asked_waits_for_the_ask() {
    for answer in [None, Some("cancel")] {
        let (_dir, repo, db) = fixture();
        let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
        validated_orphan(&db, &run);
        let mut queue = SqliteQueue::open(&db).unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ReviewStarted,
                json!({"attempt": 1, "workspace_id": null, "session_live": false, "session_id": "s"}),
            )
            .unwrap();
        queue
            .record_runtime_event(
                run.id(),
                EventKind::ReviewFinished,
                json!({"attempt": 1, "verdict": "concern", "reasons": ["out of scope"], "summary": "scope"}),
            )
            .unwrap();
        let ask = queue
            .ask(NewAsk {
                recommendation: None,
                confidence: None,
                topics: Vec::new(),
                kind: AskKind::ApproveLanding,
                task_id: None,
                run_id: Some(run.id().clone()),
                question: "The supervisor's review of run r (task 1) returned concern: scope"
                    .into(),
                options: vec!["land".into(), "send_back".into(), "cancel".into()],
                asked_by: "supervisor".into(),
                reason_category: dagq::domain::AskReason::Scope,
                finding_id: None,
                request_id: None,
            })
            .unwrap()
            .ask
            .id;
        if let Some(answer) = answer {
            queue.answer(ask, answer).unwrap();
        }
        kill_supervisor_and_wrapper(&db, &run);
        let backend = TestWorkspace::new(&db, false, VALID_AGENT);
        let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
        let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        assert!(reviewer.prompts().is_empty());
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert_eq!(adoption_events(&detail).len(), 1);
        assert_eq!(payloads(&detail, "ask_opened").len(), 1);
        assert!(
            payloads(&detail, "ask_answered")
                .iter()
                .all(|p| p["runtime_closed"] != true)
        );
        assert!(queue.run_leases().unwrap().is_empty());
        if answer.is_none() {
            assert_eq!(
                queue.run(run.id()).unwrap().status(),
                RunStatus::AwaitingIntegration
            );
            let asks = queue.asks(AskQuery::default()).unwrap();
            assert_eq!(asks.len(), 1, "{asks:?}");
            assert_eq!(asks[0].id, ask);
            assert!(asks[0].answer.is_none());
        } else {
            // The answer given while no supervisor ran is applied.
            assert_eq!(queue.run(run.id()).unwrap().status(), RunStatus::Failed);
            assert_eq!(detail.task.status(), TaskStatus::Canceled);
            let closed = queue.read_ask(ask).unwrap();
            assert!(closed.closed_at.is_some());
            assert_eq!(closed.answer.as_deref(), Some("cancel"));
        }
    }
}

/// Without a supervisor, `recover` takes the stale lease of an
/// `awaiting_integration` run whose supervisor and wrapper died (task 236):
/// the run stays `awaiting_integration` and `integrate` lands it. A run
/// awaiting integration that nobody leases has nothing to recover, and a
/// live supervisor's lease is refused as before.
#[test]
fn recover_releases_the_stale_lease_of_an_awaiting_run_for_integrate() {
    let (_dir, repo, db) = fixture();
    let base = git_out(&repo, &["rev-parse", "main"]);
    let run = orphan_run(&repo, &db, "dead-supervisor", dead_pid(), dead_pid());
    validated_orphan(&db, &run);
    let refused = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(refused.contains("lease heartbeat is"), "{refused}");
    kill_supervisor_and_wrapper(&db, &run);
    let error = integrate(&db, 1, &repo).unwrap_err().to_string();
    assert!(error.contains("run is still leased"), "{error}");
    let recovered = runtime::recover(&db, run.id()).unwrap();
    assert_eq!(recovered["run"]["status"], "awaiting_integration");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(queue.run_lease(run.id()).unwrap().is_none());
    let recovered = payloads(&queue.show(TaskId::new(1)).unwrap(), "run_recovered")[0].clone();
    assert_eq!(recovered["previous_status"], "awaiting_integration");
    assert_eq!(recovered["status"], "awaiting_integration");
    assert_eq!(recovered["lease_deleted"], true);
    let again = runtime::recover(&db, run.id()).unwrap_err().to_string();
    assert!(
        again.contains("only unfinished runs, or a run awaiting integration that is still leased"),
        "{again}"
    );
    assert_eq!(integrate(&db, 1, &repo).unwrap()["outcome"], "integrated");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_landed_run(&detail.runs[0], &repo, &base);
}

/// The worker script that commits its work, writes a `failed` receipt
/// naming it ([`RECEIPT`]) and fails, leaving in its run directory
/// `break.sh`, which runs `breaks` in its worktree: what breaks the
/// worktree's registration once the run ended, before its recovery job
/// judges it.
fn breaks_its_worktree(breaks: &str) -> String {
    format!(
        "commit work; {RECEIPT}; {{ printf 'cd \"%s\" || exit 1\\n' \"$PWD\"; cat <<'BREAK'\n{breaks}\nBREAK\n}} > \"$RUN_DIR/break.sh\"; exit 7"
    )
}

/// Writes a `failed` receipt naming the run's head.
const RECEIPT: &str = "printf '{\"run_id\":\"%s\",\"result\":\"failed\",\"commit\":\"%s\",\"tests\":{\"status\":\"failed\",\"evidence_or_reason\":\"a test failed\"},\"e2e\":{\"status\":\"not_applicable\",\"evidence_or_reason\":\"none\"},\"subagent_review\":{\"status\":\"not_applicable\",\"evidence_or_reason\":\"none\"},\"summary\":\"failed\"}' \"$RUN_ID\" \"$(git rev-parse HEAD)\" > \"$RECEIPT\"";

/// Removes the worktree's admin dir, as `git worktree prune` does.
const ADMIN_GONE: &str = "rm -rf \"$(git rev-parse --absolute-git-dir)\"";

/// Points the worktree's `.git` at the admin dir of another worktree, on
/// another branch at the run's base.
const TAKES_ANOTHER: &str = "git worktree add -q -b other \"$PWD-other\" HEAD~1 && \
printf 'gitdir: %s\\n' \"$(git rev-parse --path-format=absolute --git-common-dir)/worktrees/${PWD##*/}-other\" > .git && \
test \"$(git symbolic-ref HEAD)\" = refs/heads/other";

/// What the recovery job of the only run of task 1 whose worker ran
/// `script` did, choosing `retry_inherit` once the run's `break.sh` broke
/// its worktree: the run and its `triage_finished`.
fn retry_inherit_of(script: &str) -> (Fixture, PathBuf, PathBuf, TaskRun, Value) {
    let (dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, script);
    let job = repair(
        json!({"action": "retry_inherit"}),
        "the session died after the work was committed",
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")])
        .with_triages(&[format!("sh break.sh >&2 && {job}")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let first = detail.runs[0].clone();
    let finished = run_payloads(&detail, &first, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    (dir, repo, db, first, finished[0].clone())
}

/// A host run whose worktree lost its admin dir (`git worktree prune`)
/// cannot be read there, so its own commits and the head `retry_inherit`
/// carries over are read from its branch in the repository itself, which
/// names its receipt's commit: the retry is applied with that head.
#[test]
fn a_run_whose_worktree_lost_its_admin_dir_carries_its_branchs_receipt_commit_over() {
    let (_dir, repo, _db, first, finished) = retry_inherit_of(&breaks_its_worktree(ADMIN_GONE));
    assert_eq!(finished["action"], "retry_inherit", "{finished}");
    let branch = git_out(&repo, &["rev-parse", &format!("dagq/{}", first.id())]);
    assert_ne!(branch, first.base_commit().as_str());
    assert_eq!(finished["inherit"]["head"], json!(branch));
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", first.id())]
        ),
        branch
    );
}

/// A host run whose worktree's `.git` names another worktree's admin dir
/// (on another branch, at the run's base) reads its own branch in the
/// repository, not the other's `HEAD`: its commits are its own and are
/// carried over.
#[test]
fn a_run_whose_worktree_names_another_runs_admin_dir_carries_its_own_branch_over() {
    let (_dir, repo, _db, first, finished) = retry_inherit_of(&breaks_its_worktree(TAKES_ANOTHER));
    assert_eq!(finished["action"], "retry_inherit", "{finished}");
    let branch = git_out(&repo, &["rev-parse", &format!("dagq/{}", first.id())]);
    assert_ne!(branch, first.base_commit().as_str());
    assert_eq!(finished["inherit"]["head"], json!(branch));
    assert_eq!(
        git_out(
            &repo,
            &["rev-parse", &format!("refs/dagq/runs/{}", first.id())]
        ),
        branch
    );
}

/// A host run whose worker wrote a succeeded receipt and whose worktree
/// then lost its admin dir (`git worktree prune`) cannot be read by its
/// validation: it fails with `git_failed` and goes to its recovery job
/// (here escalating to a person), rather than an error of every pass that
/// leaves the run where it is.
#[test]
fn a_run_whose_worktree_lost_its_admin_dir_after_its_receipt_fails_to_its_recovery_job() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!("commit work; receipt \"$(git rev-parse HEAD)\"; {ADMIN_GONE}; idle; await_exit"),
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")]).with_triages(&[recovery(
        json!({"verdict": "escalate", "confidence": "high", "diagnosis": "its worktree is broken"}),
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs.len(), 1);
    let run = &detail.runs[0];
    assert_eq!(run.status(), RunStatus::Failed);
    let validated = run_payloads(&detail, run, "validation_finished");
    assert_eq!(validated.len(), 1, "{validated:?}");
    assert_eq!(validated[0]["code"], "git_failed");
    let reason = validated[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("not registered on its branch"),
        "{validated:?}"
    );
    assert_eq!(
        run_payloads(&detail, run, "recovery_requested")[0]["alert"],
        "failed"
    );
    let finished = run_payloads(&detail, run, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["action"], "ask");
}

/// Points the worktree's `.git` at the admin dir of another worktree, on
/// another branch at the run's base.
const TAKES_ANOTHER_AT_BASE: &str = "git worktree add -q -b other \"$PWD-other\" HEAD && \
printf 'gitdir: %s\\n' \"$(git rev-parse --path-format=absolute --git-common-dir)/worktrees/${PWD##*/}-other\" > .git && \
test \"$(git symbolic-ref HEAD)\" = refs/heads/other";

/// ADR-t1818-2 decision 3's preconditions read from a host run whose
/// worktree's `.git` names another worktree's admin dir (clean, at the
/// run's base): its head is its own branch's, and what is uncommitted in
/// its worktree cannot be read, so the hand-off is refused and asked, with
/// no error of the supervisor's pass.
#[test]
fn a_hand_off_of_a_run_whose_worktree_names_another_admin_dir_is_refused_and_asked() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "receipt \"$(git rev-parse HEAD)\"; {{ printf 'cd \"%s\" || exit 1\\n' \"$PWD\"; cat <<'BREAK'\n{TAKES_ANOTHER_AT_BASE}\nBREAK\n}} > \"$RUN_DIR/break.sh\"; idle; await_exit"
        ),
    );
    let job = repair(json!({"action": "hand_off"}), "the base meets it");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "unused")])
        .with_triages(&[format!("sh break.sh >&2 && {job}")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    assert_eq!(
        run_payloads(&detail, &run, "validation_finished")[0]["code"],
        "commit_mismatch"
    );
    assert!(payloads(&detail, "auto_repaired").is_empty());
    assert!(payloads(&detail, "run_satisfied_snapshot").is_empty());
    let finished = run_payloads(&detail, &run, "triage_finished");
    assert_eq!(finished.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(finished[0]["action"], "ask");
    let ask = queue
        .read_ask(AskId::new(finished[0]["ask_id"].as_i64().unwrap()))
        .unwrap();
    for part in [
        "the hand-off does not apply",
        "does not read as on its branch",
    ] {
        assert!(ask.question.contains(part), "{part}: {}", ask.question);
    }
}
