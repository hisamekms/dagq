//! Runtime tests: a person's retry that carries a failed or interrupted
//! run's branch over (`ready --inherit`, ADR-t1962-1), its refusals, its
//! relation to the automatic `retry_inherit`, its race with a recovery
//! job's round, ordered by the test on a clock it moves, and the refusal of
//! a bare `retry_inherit` answer to the recovery job's `decide` ask that
//! points to it.
use crate::common;
use crate::runtime_support;
use dagq::application::TRIAGE_ASKER;
use dagq::application::commands::planning::Planning;
use dagq::application::inherit::InheritedByHand;
use dagq::domain::{
    ActorContext, ActorRole, AskReason, HEARTBEAT_TIMEOUT_SECS, LeaseToken, StaticPolicy,
    actor_model::{ActorLaunch, ModelRole},
};
use dagq::infrastructure::adapters::RunRepositories;
use dagq::infrastructure::runtime_store::TriageAction;

use runtime_support::*;

const REASON: &str = "the person chose retry_inherit in ask 511";
const INBOX: [(&str, &str); 2] = [("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")];

/// A recovery job's script that escalates: the run waits for a person
/// with a `decide` ask.
fn escalate() -> String {
    recovery(json!({
        "verdict": "escalate",
        "confidence": "high",
        "diagnosis": "a person decides",
        "question": "Carry the work over?",
    }))
}

/// `dagq ready 1 --inherit --reason REASON` with the actor of `env`.
fn ready_inherit(env: &[(&str, &str)], db: &Path) -> std::process::Output {
    common::cli::invoke_with(env, db, &["ready", "1", "--inherit", "--reason", REASON])
}

/// What the CLI printed as its error.
fn refusal(output: &std::process::Output) -> String {
    assert!(!output.status.success(), "it succeeded");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The commit `refs/dagq/runs/<run>` holds, if any.
fn run_ref(repo: &Path, run: &TaskRun) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/dagq/runs/{}", run.id()),
        ])
        .bounded_output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The `triage_finished` events of `run` with action `retry_inherit`.
fn inherit_retries(detail: &dagq::domain::TaskDetail, run: &TaskRun) -> Vec<Value> {
    detail
        .events
        .iter()
        .filter(|e| {
            e.kind == "triage_finished"
                && e.run_id.as_ref() == Some(run.id())
                && e.payload["action"] == "retry_inherit"
        })
        .map(|e| e.payload.clone())
        .collect()
}

/// How many times the task went from `in_progress` back to `ready`.
fn readied(detail: &dagq::domain::TaskDetail) -> usize {
    detail
        .events
        .iter()
        .filter(|e| {
            e.kind == "task_status_changed"
                && e.payload["from"] == "in_progress"
                && e.payload["to"] == "ready"
        })
        .count()
}

/// The unclosed `decide` ask the recovery job's escalation left on `run`.
fn decide_ask(queue: &SqliteQueue, run: &TaskRun) -> dagq::domain::Ask {
    let asks = queue.unclosed_run_asks(run.id()).unwrap();
    let [ask] = asks.as_slice() else {
        panic!("{asks:?}")
    };
    assert_eq!(ask.kind, AskKind::Decide);
    ask.clone()
}

/// ADR-t1962-1: the inbox carries the failed (or interrupted) run with
/// commits over by hand: the task is ready without plan review, the
/// recovery job's `decide` ask is closed, `triage_finished` records who,
/// why and the head kept under `refs/dagq/runs/<run-id>`, and the next run
/// records `run_inherited` and starts from it.
fn carried_over_by_the_inbox(interrupted: bool) {
    let (dir, repo, db) = fixture();
    let mark = dir.path().join("committed-once");
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "if [ ! -f {mark} ]; then : > {mark}; commit work; fi; exit 7",
            mark = shell_path(&mark)
        ),
    );
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let first = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(first.status(), RunStatus::Failed);
    if interrupted {
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE task_runs SET status='interrupted' WHERE id=?1",
                [first.id()],
            )
            .unwrap();
    }
    let ask = decide_ask(&queue, &first);
    let bypassed = |detail: &dagq::domain::TaskDetail| {
        event_kinds(detail)
            .into_iter()
            .filter(|kind| *kind == "review_bypassed")
            .count()
    };
    let bypassed_before = bypassed(&queue.show(TaskId::new(1)).unwrap());

    let output = ready_inherit(&INBOX, &db);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed["task"]["status"], "ready");
    assert_eq!(printed["closed_asks"], json!([ask.id]));
    assert_eq!(printed["removed_lease"], Value::Null);
    let head = printed["head"].as_str().unwrap().to_owned();
    assert_eq!(run_ref(&repo, &first).as_deref(), Some(head.as_str()));
    assert_ne!(head, first.base_commit().as_str());
    assert!(queue.read_ask(ask.id).unwrap().closed_at.is_some());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(bypassed(&detail), bypassed_before);
    let retries = inherit_retries(&detail, &first);
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["by"], "inbox");
    assert_eq!(retries[0]["reason"], REASON);
    assert_eq!(retries[0]["inherit"]["head"], json!(head));
    assert_eq!(
        retries[0]["inherit"]["branch"],
        json!(format!("dagq/{}", first.id()))
    );
    assert_eq!(retries[0]["closed_asks"], json!([ask.id]));
    let recorded = detail
        .events
        .iter()
        .rfind(|e| e.kind == "triage_finished")
        .unwrap();
    assert_eq!(
        serde_json::to_value(&recorded.actor).unwrap()["role"],
        "inbox"
    );

    // The next run is claimed from it (and fails at once: what it inherits
    // is recorded at its claim).
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let second = detail.runs[1].clone();
    let inherited: Vec<&Value> = detail
        .events
        .iter()
        .filter(|e| e.kind == "run_inherited" && e.run_id.as_ref() == Some(second.id()))
        .map(|e| &e.payload)
        .collect();
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0]["inherit_from_run"], json!(first.id()));
    assert_eq!(inherited[0]["head"], json!(head));
    let prompt = read_prompt(&second);
    assert!(
        prompt.contains(&format!("Carried over from run {}", first.id())),
        "{prompt}"
    );
    assert!(
        prompt.contains("inbox carried its work over by hand"),
        "{prompt}"
    );
}

#[test]
fn the_inbox_carries_a_failed_run_over_by_hand_and_the_next_run_inherits_it() {
    carried_over_by_the_inbox(false);
}

#[test]
fn the_inbox_carries_an_interrupted_run_over_by_hand_and_the_next_run_inherits_it() {
    carried_over_by_the_inbox(true);
}

/// A run without commits of its own, a live process or lease, a latest run
/// that did not fail, a task with no run, and another actor than user or
/// inbox are refused with the reason, and the task, the ask and the ref
/// stay as they were.
#[test]
fn ready_inherit_is_refused_with_its_reason_and_changes_nothing() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    let ask = decide_ask(&queue, &run);
    let before = queue.show(TaskId::new(1)).unwrap().events.len();
    let unchanged = |queue: &mut SqliteQueue| {
        let detail = queue.show(TaskId::new(1)).unwrap();
        assert_eq!(detail.task.status(), TaskStatus::InProgress);
        assert!(inherit_retries(&detail, &run).is_empty());
        let now = queue.read_ask(ask.id).unwrap();
        assert!(now.closed_at.is_none() && now.answered_at.is_none());
        assert_eq!(run_ref(&repo, &run), None);
        detail.events.len()
    };

    // Nothing of its own on the branch: a plain ready retries.
    let error = refusal(&ready_inherit(&INBOX, &db));
    assert!(error.contains("holds no commit of its own"), "{error}");
    assert!(error.contains("plain `dagq ready`"), "{error}");
    assert_eq!(unchanged(&mut queue), before);
    // Another actor: refused before the store is read.
    for env in [
        vec![
            ("DAGQ_ROLE", "worker"),
            ("DAGQ_RUN_ID", run.id().as_str()),
            ("DAGQ_TASK_ID", "1"),
        ],
        vec![("DAGQ_ROLE", "planner"), ("DAGQ_ACTOR_ID", "planner:1")],
    ] {
        let error = refusal(&ready_inherit(&env, &db));
        assert!(error.contains("not granted"), "{error}");
        assert!(error.contains("task.ready_bypass_review"), "{error}");
        unchanged(&mut queue);
    }
    // A live process or a lease that is not stale.
    let db_conn = Connection::open(&db).unwrap();
    // Its wrapper heartbeats again for a while, unexited.
    let exited: (i64, i64) = db_conn
        .query_row(
            "SELECT exited_at, heartbeat_at FROM run_processes WHERE run_id=?1 AND role='wrapper'",
            [run.id()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    db_conn
        .execute(
            "UPDATE run_processes SET exited_at=NULL, heartbeat_at=unixepoch()
             WHERE run_id=?1 AND role='wrapper'",
            [run.id()],
        )
        .unwrap();
    let error = refusal(&ready_inherit(&INBOX, &db));
    assert!(error.contains("still alive"), "{error}");
    db_conn
        .execute(
            "UPDATE run_processes SET exited_at=?2, heartbeat_at=?3 WHERE run_id=?1 AND role='wrapper'",
            rusqlite::params![run.id(), exited.0, exited.1],
        )
        .unwrap();
    db_conn
        .execute(
            "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,'job',?2,unixepoch())",
            rusqlite::params![run.id(), std::process::id()],
        )
        .unwrap();
    let error = refusal(&ready_inherit(&INBOX, &db));
    assert!(error.contains("is leased"), "{error}");
    db_conn
        .execute("DELETE FROM run_leases WHERE run_id=?1", [run.id()])
        .unwrap();
    unchanged(&mut queue);

    // A task with no run, and a task whose latest run has just started.
    add_ready_task(&mut queue, "second", &[]);
    let error = refusal(&common::cli::invoke_with(
        &INBOX,
        &db,
        &["ready", "2", "--inherit", "--reason", REASON],
    ));
    assert!(error.contains("the task is ready"), "{error}");
    let running = provision_under(&repo, &db, "live");
    assert_eq!(running.task_id(), TaskId::new(2));
    let error = refusal(&common::cli::invoke_with(
        &INBOX,
        &db,
        &["ready", "2", "--inherit", "--reason", REASON],
    ));
    assert!(
        error.contains("is starting, not failed or interrupted"),
        "{error}"
    );
    assert_eq!(
        queue.show(TaskId::new(2)).unwrap().task.status(),
        TaskStatus::InProgress
    );
    assert_eq!(run_ref(&repo, &running), None);
    unchanged(&mut queue);
}

/// The option a recovery job adds to its `decide` ask for carrying the
/// work over.
const INHERIT_OPTION: &str = "retry_inherit once process inspection is allowed";

/// `dagq answer ID --text TEXT` by the inbox.
fn answer(db: &Path, ask: AskId, text: &str) -> std::process::Output {
    common::cli::invoke_with(&INBOX, db, &["answer", &ask.to_string(), "--text", text])
}

/// The `ask_answered` payloads of `ask`.
fn answers_of(db: &Path, ask: AskId) -> Vec<Value> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM run_events
             WHERE kind='ask_answered' AND json_extract(payload,'$.ask_id')=?1",
        )
        .unwrap();
    stmt.query_map([ask], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect()
}

/// An answer to the recovery job's `decide` ask that is only a runtime
/// operation's name and none of its options is refused before anything is
/// written: the ask stays open, and the message names the option with the
/// name and the retry by hand. An option as it is and a person's own words
/// are taken as before.
#[test]
fn a_bare_operation_name_to_a_decide_ask_is_refused_and_the_ask_stays_open() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[recovery(json!({
            "verdict": "escalate",
            "confidence": "high",
            "diagnosis": "a person decides",
            "question": "Carry the work over?",
            "options": [INHERIT_OPTION],
        }))]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::Failed);
    let ask = decide_ask(&queue, &run);
    assert_eq!(ask.asked_by, TRIAGE_ASKER);
    assert!(ask.options.iter().any(|option| option == INHERIT_OPTION));

    for text in ["retry_inherit", " RETRY_INHERIT ", "\"retry_inherit\""] {
        let printed: Value = serde_json::from_str(&refusal(&answer(&db, ask.id, text))).unwrap();
        let error = printed["error"].as_str().unwrap();
        assert!(error.contains("the ask stays open"), "{error}");
        assert!(error.contains(&format!("\"{INHERIT_OPTION}\"")), "{error}");
        assert!(error.contains("dagq ready 1 --inherit --reason"), "{error}");
        let now = queue.read_ask(ask.id).unwrap();
        assert!(now.is_open(), "{now:?}");
        assert!(now.answer.is_none() && now.answered_at.is_none());
        assert!(answers_of(&db, ask.id).is_empty());
    }

    // The option as it is goes to the runtime (here, to the recovery job).
    let output = answer(&db, ask.id, INHERIT_OPTION);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answered = answers_of(&db, ask.id);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["runtime_delivers"], true);

    // A person's own words, to the same kind of ask: a person reads them.
    queue.close_ask(ask.id).unwrap();
    let second = queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Decide,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Carry the work over?".into(),
            options: ask.options.clone(),
            asked_by: TRIAGE_ASKER.into(),
            reason_category: AskReason::RecoveryFailed,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask;
    assert_ne!(second.id, ask.id);
    let words = "carry it over with retry_inherit once you can";
    let output = answer(&db, second.id, words);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        queue.read_ask(second.id).unwrap().answer.as_deref(),
        Some(words)
    );
    let answered = answers_of(&db, second.id);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["runtime_delivers"], false);
}

/// ADR-t1962-1: the retry by hand does not use the automatic one. After a
/// person carried run 1 over, the recovery job's `retry_inherit` of run 2
/// still applies.
#[test]
fn a_retry_by_hand_leaves_the_automatic_one() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[escalate()]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let output = ready_inherit(&INBOX, &db);
    assert!(output.status.success(), "{}", refusal(&output));

    // The next run fails too; its job's `retry_inherit` is applied. The
    // run after that fails with no verdict left (its job cannot start).
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[repair(
        json!({"action": "retry_inherit"}),
        "the work is done",
    )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert!(detail.runs.len() >= 3, "{:?}", event_kinds(&detail));
    assert_eq!(inherit_retries(&detail, &detail.runs[0])[0]["by"], "inbox");
    let automatic = inherit_retries(&detail, &detail.runs[1]);
    assert_eq!(automatic.len(), 1, "{:?}", event_kinds(&detail));
    assert_ne!(automatic[0]["by"], "inbox");
}

/// ADR-t1962-1: the retry by hand has no once-per-task limit. After the
/// recovery job's automatic `retry_inherit` of run 1, a person carries run
/// 2 over.
#[test]
fn a_retry_by_hand_follows_the_automatic_one() {
    let (_dir, repo, db) = fixture();
    let backend = TestWorkspace::new(&db, false, "commit work; exit 7");
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "fine")]).with_triages(&[
        repair(json!({"action": "retry_inherit"}), "the work is done"),
        escalate(),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.runs.len(), 2, "{:?}", event_kinds(&detail));
    let runs = detail.runs.clone();
    assert_eq!(inherit_retries(&detail, &runs[0]).len(), 1);
    assert_eq!(runs[1].status(), RunStatus::Failed);
    decide_ask(&queue, &runs[1]);

    // A person's own shell: no DAGQ_ROLE.
    let output = ready_inherit(&[], &db);
    assert!(output.status.success(), "{}", refusal(&output));
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    assert_eq!(inherit_retries(&detail, &runs[1])[0]["by"], "user");
    assert!(queue.unclosed_run_asks(runs[1].id()).unwrap().is_empty());
}

/// Where the race tests start their injected clock: some heartbeat
/// timeouts after the real time of the claim the fixture makes, so that
/// the claim's lease is stale on it, wherever the test runs; the test then
/// moves the clock itself.
fn clock_start() -> i64 {
    unix_second_now() + 10 * HEARTBEAT_TIMEOUT_SECS
}

/// An interrupted run of task 1 whose branch holds a commit of its own,
/// made through the store with no session and no recovery round yet, on a
/// queue whose clock the test moves (ahead of the claim's real time, so
/// the claim's lease is stale and `recover_run` takes the run); and its
/// head.
fn interrupted_with_a_commit(
    repo: &Path,
    db: &Path,
    clock: &ManualClock,
) -> (SqliteQueue, TaskRun, String) {
    let run = provision_under(repo, db, "first");
    let worktree = Path::new(run.worktree_path().unwrap());
    fs::write(worktree.join("work.txt"), "work\n").unwrap();
    git(worktree, &["add", "work.txt"]);
    git(worktree, &["commit", "-q", "-m", "work"]);
    let head = git_out(worktree, &["rev-parse", "HEAD"]);
    let mut queue = SqliteQueue::open(db).unwrap().with_generators(Generators {
        clock: Arc::new(clock.clone()),
        ids: Arc::new(clock::UuidGenerator),
    });
    let run = queue.recover_run(run.id(), 0, json!({})).unwrap();
    assert_eq!(run.status(), RunStatus::Interrupted);
    assert!(queue.run_leases().unwrap().is_empty());
    (queue, run, head)
}

/// A recovery job's round takes the run's lease.
fn begin_round(queue: &mut SqliteQueue, run: &TaskRun) -> Option<(TaskRun, usize)> {
    queue
        .begin_triage(
            run.id(),
            &LeaseToken::new("job"),
            Some(json!({"alert": "failed", "by": "runtime"})),
            &ActorLaunch::default_of(ModelRole::Recovery),
        )
        .unwrap()
}

/// The `decide` ask a round escalated with, answered `retry`.
fn answered_decide(queue: &mut SqliteQueue, run: &TaskRun) -> AskId {
    let ask = queue
        .ask(NewAsk {
            kind: AskKind::Decide,
            task_id: None,
            run_id: Some(run.id().clone()),
            question: "Carry the work over?".into(),
            options: vec!["retry".into(), "resume".into(), "cancel".into()],
            asked_by: TRIAGE_ASKER.into(),
            reason_category: AskReason::RecoveryFailed,
            topics: Vec::new(),
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        })
        .unwrap()
        .ask
        .id;
    queue.answer(ask, "retry").unwrap();
    ask
}

/// The inbox's `ready --inherit` through the use case the CLI calls.
fn inherit(queue: &mut SqliteQueue) -> anyhow::Result<InheritedByHand> {
    let inbox = ActorContext::instance(ActorRole::Inbox, 1);
    Planning::new(queue, &inbox, &StaticPolicy).ready_inheriting(
        TaskId::new(1),
        REASON,
        &RunRepositories,
    )
}

/// The job's round ends with its own `retry_inherit` of `head`.
fn finish_round(queue: &mut SqliteQueue, run: &TaskRun, head: &str) -> anyhow::Result<TaskRun> {
    queue.finish_triage(
        run.id(),
        &LeaseToken::new("job"),
        &TriageAction::RetryInherit {
            branch: run.branch().map(str::to_owned),
            head: sha(head),
        },
        json!({"by": "runtime"}),
        Vec::new(),
    )
}

/// (e1) A round whose lease is not stale holds the run: the retry by hand
/// is refused and changes neither the task, the ask nor the ref; the round
/// goes on and its own `retry_inherit` is the run's one.
#[test]
fn a_round_with_a_live_lease_refuses_the_retry_by_hand() {
    let (_dir, repo, db) = fixture();
    let t = clock_start();
    let clock = ManualClock::at(t);
    let (mut queue, run, head) = interrupted_with_a_commit(&repo, &db, &clock);
    assert!(begin_round(&mut queue, &run).is_some());
    let ask = answered_decide(&mut queue, &run);
    clock.set(t + HEARTBEAT_TIMEOUT_SECS);

    let error = inherit(&mut queue).unwrap_err();
    assert!(format!("{error:#}").contains("is leased"), "{error:#}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::InProgress);
    assert!(inherit_retries(&detail, &run).is_empty());
    assert!(queue.read_ask(ask).unwrap().closed_at.is_none());
    assert_eq!(run_ref(&repo, &run), None);
    assert_eq!(queue.run_leases().unwrap()[0].token.as_str(), "job");

    finish_round(&mut queue, &run, &head).unwrap();
    let error = inherit(&mut queue).unwrap_err();
    assert!(
        format!("{error:#}").contains("the task is ready"),
        "{error:#}"
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(inherit_retries(&detail, &run).len(), 1);
    assert_eq!(readied(&detail), 1);
}

/// (e2) The retry by hand first: no round takes the run afterwards.
#[test]
fn no_round_takes_a_run_carried_over_by_hand() {
    let (_dir, repo, db) = fixture();
    let clock = ManualClock::at(clock_start());
    let (mut queue, run, head) = interrupted_with_a_commit(&repo, &db, &clock);
    let inherited = inherit(&mut queue).unwrap();
    assert_eq!(inherited.head.as_str(), head);
    assert_eq!(run_ref(&repo, &run).as_deref(), Some(head.as_str()));
    assert!(begin_round(&mut queue, &run).is_none());
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(inherit_retries(&detail, &run).len(), 1);
    assert_eq!(readied(&detail), 1);
}

/// (e1)/(e2) A round whose lease went stale before it finished: the retry
/// by hand goes on and removes that lease in its transaction, so the
/// round's late `retry_inherit` fails and the answer of its closed
/// `decide` ask is applied by nobody; the run is carried over once.
#[test]
fn a_retry_by_hand_over_a_stale_round_takes_its_right_to_apply_away() {
    let (_dir, repo, db) = fixture();
    let t = clock_start();
    let clock = ManualClock::at(t);
    let (mut queue, run, head) = interrupted_with_a_commit(&repo, &db, &clock);
    assert!(begin_round(&mut queue, &run).is_some());
    let ask = answered_decide(&mut queue, &run);
    clock.set(t + HEARTBEAT_TIMEOUT_SECS + 1);

    let inherited = inherit(&mut queue).unwrap();
    assert_eq!(
        inherited.removed_lease.as_ref().map(LeaseToken::as_str),
        Some("job")
    );
    assert_eq!(inherited.closed_asks, [ask]);
    assert!(queue.run_leases().unwrap().is_empty());
    assert!(queue.read_ask(ask).unwrap().closed_at.is_some());
    assert_eq!(run_ref(&repo, &run).as_deref(), Some(head.as_str()));

    let late = finish_round(&mut queue, &run, &head).unwrap_err();
    assert!(format!("{late:#}").contains("lease is missing"), "{late:#}");
    assert!(queue.decide_triage(run.id(), ask, "retry", "late").is_err());
    assert!(
        queue
            .triage_answers()
            .unwrap()
            .iter()
            .all(|answer| answer.id != ask)
    );
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Ready);
    let retries = inherit_retries(&detail, &run);
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["removed_lease"], "job");
    assert_eq!(readied(&detail), 1);
    assert!(
        !detail
            .events
            .iter()
            .any(|e| e.kind == "triage_decided" && e.run_id.as_ref() == Some(run.id()))
    );
}
