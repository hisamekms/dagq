//! The route of the runtime's planners (ADR-t1394-2 decisions 1 and 2):
//! under `[roles.runtime_planner] route = "headless"` a planner the
//! supervisor opens runs one call of the agent (the stub `claude` of
//! [`headless_claude`]) per turn, its revise comes as the next request in
//! its directory's `turns/`, its turns are events of the queue that name
//! it, and its wrapper, started in the background under `[headless]
//! wrapper = "background"` (ADR-t1404-1 decision 8), ends on the exit
//! request. Without the key, and with `route = "interactive"`, the planner
//! opens in a workspace and is typed into as before.

use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, events, fixture, git, open_goal, options, runtime_draft,
    submit, supervise_with,
};
use crate::runtime_support::{headless_claude, set_turns};
use dagq::application::TaskStore;
use dagq::{
    domain::{
        DraftOrigin, PlannerRoute, Priority, ProposalStatus, TaskId,
        background_wrapper::is_background,
    },
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Commit `text` as the main checkout's `dagq.toml`.
fn configure(repo: &Path, text: &str) {
    fs::write(repo.join("dagq.toml"), text).unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-qm", "route of the runtime's planners"]);
}

/// The queue's events of `kind`, oldest first.
fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    let mut events: Vec<Value> = SqliteQueue::open(db)
        .unwrap()
        .latest_events_of(kind, 50)
        .unwrap()
        .into_iter()
        .filter(|event| event.run_id.is_none())
        .map(|event| event.payload)
        .collect();
    events.reverse();
    events
}

/// What the planner of a headless test that did not end left.
fn diagnose(db: &Path, proposal: dagq::domain::ProposalId) -> String {
    let queue = SqliteQueue::open(db).unwrap();
    let dir = planners_dir(db).join("1");
    let read = |path: &Path| fs::read_to_string(path).unwrap_or_default();
    let turns: Vec<String> = fs::read_dir(dir.join("turns"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    format!(
        "the headless planner did not end: {:?}\nproposal {:?}\nturns {turns:?}\nsession.log {}\nsubmit.log {}\ncalls {}\nevents {:?}",
        queue.planners(true).unwrap(),
        queue.show_proposal(proposal).unwrap(),
        read(&dir.join("session.log")),
        read(&dir.join("submit.log")),
        read(&dir.join("stub-calls.log")),
        queue
            .latest_events_of("turn_finished", 10)
            .unwrap()
            .into_iter()
            .map(|e| e.payload)
            .collect::<Vec<_>>(),
    )
}

/// What planner 1 of a headless test that did not end left.
fn diagnose_planner(db: &Path) -> String {
    let dir = planners_dir(db).join("1");
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    format!(
        "{:?}\nask.log {}\ncancel.log {}\ncalls {}\nsession.log {}\nturns {:?}",
        SqliteQueue::open(db).unwrap().planners(true).unwrap(),
        read("ask.log"),
        read("cancel.log"),
        read("stub-calls.log"),
        read("session.log"),
        queue_events(db, "turn_finished"),
    )
}

/// A plan review that sends the proposal back with `reasons`.
fn revise(reasons: &[&str]) -> Value {
    json!({"verdict": "revise", "reasons": reasons, "summary": "not yet"})
}

/// A fixture whose runtime's planners run headless in the background, and
/// whose planner's agent is the stub of [`headless_claude`] running `turns`
/// (with `$DB` naming the queue, which is not the checkout's).
fn headless_fixture(turns: &str) -> crate::plan_review::Fixture {
    let mut fx = fixture();
    configure(
        &fx.repo,
        "[roles.runtime_planner]\nroute = \"headless\"\n\n[headless]\nwrapper = \"background\"\n",
    );
    let stub = headless_claude(fx.db.parent().unwrap(), &fx.db);
    set_turns(fx.db.parent().unwrap(), turns);
    let claude = fx.db.parent().unwrap().join("claude-planner");
    crate::common::template::script(
        &claude,
        format!(
            "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.0 (Claude Code)'; exit 0 ;; esac\nexec {} \"$@\"\n",
            stub.display()
        ),
    );
    fx.claude = claude;
    fx
}

/// Supervise, one pass at a time, until `done` holds; the diagnosis of
/// planner 1 when it does not in time.
fn supervise_until(
    fx: &crate::plan_review::Fixture,
    backend: &PlanWorkspace,
    reviewer: &StubReviewer,
    mut done: impl FnMut() -> bool,
    diagnosis: impl Fn() -> String,
) {
    let settings = options(1, Duration::from_secs(3600));
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        supervise_with(fx, backend, reviewer, &settings);
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "{}", diagnosis());
        thread::sleep(Duration::from_millis(50));
    }
}

/// Acceptance: with the route set to headless, the planner the supervisor
/// opens for a revise runs in the background without a workspace, its
/// prompt as its first turn; its turn's output is kept in `turns/`, the
/// turn is recorded on the queue with its ID, and once it submitted the
/// proposal again the exit request ends its wrapper and the runtime closes
/// it. Nothing is typed and no workspace is opened; `doctor` reads the
/// route.
#[test]
fn a_headless_planner_opened_for_a_revise_runs_its_prompt_as_a_turn_and_ends_on_the_exit_request() {
    let fx = headless_fixture(
        "\"$DAGQ\" --db \"$DB\" submit --proposal 1 >> \"$RUN_DIR/submit.log\" 2>&1; say \"turn $TURN submitted\"",
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    assert_eq!(proposal.as_i64(), 1);
    let reviewer = StubReviewer::new(&[
        revise(&["split it"]),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::default();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || {
            !queue_events(&fx.db, "planner_closed").is_empty()
                && queue.show_proposal(proposal).unwrap().status() == ProposalStatus::Accepted
        },
        || diagnose(&fx.db, proposal),
    );

    let planners = queue.planners(true).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    let planner = &planners[0];
    assert_eq!(planner.route, PlannerRoute::Headless);
    assert_eq!(planner.exit_code, Some(0));
    let handle = planner.workspace_id.clone().unwrap();
    assert!(is_background(&handle), "{handle}");
    // Started in the background, as a headless planner of the runtime's.
    assert!(backend.opened().is_empty(), "no workspace");
    assert!(backend.texts().is_empty(), "nothing typed");
    let launched = backend.background.launched();
    assert_eq!(launched.len(), 1, "{launched:?}");
    let (launched_handle, command, env, log) = &launched[0];
    assert_eq!(*launched_handle, handle);
    assert!(command.contains("'--headless'"), "{command}");
    assert!(command.contains("'--background'"), "{command}");
    for (name, value) in [
        ("DAGQ_ROLE", "planner".to_owned()),
        ("DAGQ_PLANNER_ORIGIN", "runtime".to_owned()),
        ("DAGQ_PLANNER_ID", planner.id.to_string()),
        ("DAGQ_SESSION_KIND", "runtime_planner".to_owned()),
    ] {
        assert!(
            env.iter().any(|(k, v)| k == name && *v == value),
            "{name}={value} in {env:?}"
        );
    }
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    assert_eq!(
        log.canonicalize().unwrap(),
        dir.join("session.log").canonicalize().unwrap()
    );
    // Its prompt was its one turn, whose output is in `turns/`; the exit
    // request ended it.
    let turns = dir.join("turns");
    assert!(turns.join("turn-000001.jsonl").is_file());
    assert!(!turns.join("turn-000002.jsonl").exists());
    assert!(turns.join("exit").is_file());
    assert!(turns.join("limits.json").is_file());
    let calls = fs::read_to_string(dir.join("stub-calls.log")).unwrap();
    assert!(calls.starts_with("start "), "{calls}");
    assert!(calls.contains("opened for proposal 1"), "{calls}");
    let sent = events(&mut queue, task, "plan_revise_sent");
    assert_eq!(sent[0]["opened"], true);
    assert_eq!(sent[0]["workspace_id"], handle);
    // Its turn is the queue's, with its ID.
    let started = queue_events(&fx.db, "turn_started");
    let finished = queue_events(&fx.db, "turn_finished");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(started[0]["planner_id"], planner.id.as_i64());
    assert_eq!(started[0]["turn"], 1);
    assert_eq!(started[0]["resume"], false);
    assert_eq!(finished[0]["planner_id"], planner.id.as_i64());
    assert_eq!(finished[0]["outcome"], "succeeded", "{finished:?}");
    assert_eq!(finished[0]["provider"], "claude");
    // Its wrapper ended on the exit request (exit code 0) and the runtime
    // closed it, the handle no longer running: as exited, whether the
    // supervisor's pass or its sweep closed it.
    let closed = queue_events(&fx.db, "planner_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["code"], "runtime_exited", "{closed:?}");
    assert_eq!(closed[0]["planner_id"], planner.id.as_i64());
    assert!(!dagq::application::WorkspaceBackend::exists(&backend, &handle).unwrap());
    let doctor = crate::common::cli::ok(&fx.db, &["doctor"]);
    assert_eq!(doctor["roles"]["runtime_planner"]["route"], "headless");
    assert_eq!(
        doctor["roles"]["runtime_planner"]["route_source"],
        "dagq.toml"
    );
}

/// Acceptance: a headless draft planner that asks a `planner_question`
/// waits for the answer, which comes as its next request in `turns/`
/// (recorded as `turn_requested` with its ID) and runs as a turn that
/// resumes its session; then the exit request ends it. Nothing is typed.
#[test]
fn a_headless_draft_planner_takes_the_answer_of_its_question_as_its_next_turn() {
    let fx = headless_fixture(
        r#"case "$TURN" in
1) "$DAGQ" --db "$DB" ask --kind planner_question --because scope --task 2 --question "in the goal?" --cmux true >> "$RUN_DIR/ask.log" 2>&1 ;;
*) "$DAGQ" --db "$DB" cancel 2 >> "$RUN_DIR/cancel.log" 2>&1 ;;
esac
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::FollowUp,
        json!({"source_task_id": 1, "source_run_id": null, "index": 0}),
    );
    assert_eq!(draft, TaskId::new(2));
    let reviewer = StubReviewer::new(&[json!({"verdict": "pass", "reasons": [], "summary": "ok"})]);
    let backend = PlanWorkspace::default();
    // Its first turn asks, and it waits for the answer without an exit.
    let open_question = || {
        SqliteQueue::open(&fx.db)
            .unwrap()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .find(|ask| ask.task_id == Some(draft))
    };
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || open_question().is_some() && !queue_events(&fx.db, "turn_finished").is_empty(),
        || diagnose_planner(&fx.db),
    );
    for _ in 0..3 {
        supervise_with(
            &fx,
            &backend,
            &reviewer,
            &options(1, Duration::from_secs(3600)),
        );
    }
    let planner = queue.planners(false).unwrap()[0].clone();
    assert_eq!(planner.route, PlannerRoute::Headless);
    assert_eq!(planner.draft_task_id, Some(draft));
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    assert!(
        !dir.join("turns").join("exit").exists(),
        "waits for its answer"
    );
    let asked = open_question().unwrap();
    queue.answer(asked.id, "cancel").unwrap();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || !queue_events(&fx.db, "planner_closed").is_empty(),
        || diagnose_planner(&fx.db),
    );

    assert!(backend.texts().is_empty(), "nothing typed");
    assert!(backend.opened().is_empty(), "no workspace");
    let turns = dir.join("turns");
    let request: Value =
        serde_json::from_str(&fs::read_to_string(turns.join("request-000001.taken.json")).unwrap())
            .unwrap();
    assert_eq!(request["what"], format!("answer of ask {}", asked.id));
    assert_eq!(
        request["prompt"],
        format!("answer to ask {}: cancel", asked.id)
    );
    assert!(turns.join("turn-000002.jsonl").is_file());
    assert!(turns.join("exit").is_file());
    let requested = queue_events(&fx.db, "turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["planner_id"], planner.id.as_i64());
    assert_eq!(requested[0]["what"], format!("answer of ask {}", asked.id));
    let started = queue_events(&fx.db, "turn_started");
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[1]["planner_id"], planner.id.as_i64());
    assert_eq!(started[1]["resume"], true);
    assert_eq!(started[1]["request"], 1);
    let calls = fs::read_to_string(dir.join("stub-calls.log")).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1].starts_with("resume "), "{calls:?}");
    assert_eq!(events(&mut queue, draft, "ask_delivered").len(), 1);
    // Its second turn canceled the draft; its wrapper ended on the exit
    // request.
    assert_eq!(
        queue.show(draft).unwrap().task.status(),
        dagq::domain::TaskStatus::Canceled
    );
    assert_eq!(queue.planner(planner.id).unwrap().exit_code, Some(0));
}

/// Acceptance: without the key, and with `route = "interactive"`, the
/// planner the supervisor opens for a revise is interactive as before: a
/// workspace whose wrapper is not headless, no `turns/`, and the next
/// revise typed into its terminal once it is idle.
#[test]
fn without_the_route_or_with_interactive_the_runtimes_planner_opens_a_workspace() {
    for config in [
        None,
        Some("[roles.runtime_planner]\nroute = \"interactive\"\n"),
    ] {
        let fx = fixture();
        if let Some(text) = config {
            configure(&fx.repo, text);
        }
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
        let proposal = submit(&mut queue, &[task], None);
        let reviewer = StubReviewer::new(&[revise(&["split it"]), revise(&["name the test"])]);
        let backend = PlanWorkspace::default();
        let settings = options(1, Duration::from_secs(3600));
        supervise_with(&fx, &backend, &reviewer, &settings);
        let planners = queue.planners(false).unwrap();
        assert_eq!(planners.len(), 1, "{config:?}: {planners:?}");
        let planner = &planners[0];
        assert_eq!(planner.route, PlannerRoute::Interactive, "{config:?}");
        assert_eq!(planner.workspace_id.as_deref(), Some("RT1"));
        assert!(backend.background.launched().is_empty());
        let commands = backend.commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(commands[0].contains("'planner-session'"), "{commands:?}");
        assert!(!commands[0].contains("--headless"), "{commands:?}");
        let dir = planners_dir(&fx.db).join(planner.id.to_string());
        assert!(!dir.join("turns").exists());

        // The planner resubmits as its own and is idle: the next revise
        // is typed into its workspace, not written as a request.
        crate::plan_review::idle(&queue, &fx.db, planner.id);
        queue
            .submit(dagq::domain::Submission {
                tasks: Vec::new(),
                goals: Vec::new(),
                proposal: Some(proposal),
                owner: dagq::domain::PlannerOwner {
                    origin: dagq::domain::PlannerOrigin::Runtime,
                    workspace_id: Some("RT1".into()),
                },
            })
            .unwrap();
        supervise_with(&fx, &backend, &reviewer, &settings);
        let texts = backend.texts();
        assert_eq!(texts.len(), 1, "{config:?}: {texts:?}");
        assert_eq!(texts[0].0, "RT1");
        assert!(texts[0].1.contains("name the test"), "{texts:?}");
        assert!(!dir.join("turns").exists());
        assert!(queue_events(&fx.db, "turn_requested").is_empty());
        let doctor = crate::common::cli::ok(&fx.db, &["doctor"]);
        assert_eq!(doctor["roles"]["runtime_planner"]["route"], "interactive");
        assert_eq!(
            doctor["roles"]["runtime_planner"]["route_source"],
            if config.is_some() {
                "dagq.toml"
            } else {
                "default"
            }
        );
    }
}

/// Stops, when the test ends, the turn a test left running: its group and
/// what it started, each only while its pid still shows the start it was
/// seen with.
struct TurnGuard(Vec<(u32, Option<String>)>);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        use dagq::application::ProcessControl;
        let processes = dagq::infrastructure::adapters::SystemProcesses;
        for (pid, start) in &self.0 {
            if start.is_some() && processes.start_identity(*pid) == *start {
                let _ = processes.kill_group(*pid);
                let _ = processes.kill(*pid);
            }
        }
    }
}

/// Acceptance (ADR-t1404-1 decisions 3 and 8): a background headless
/// planner whose wrapper is killed mid-turn leaves its turn running in a
/// group of its own; the runtime finds that turn by the planner's
/// `turn_started` (its pid and start), keeps the planner's handle open
/// while it runs, and stops it as it closes the planner.
#[test]
fn the_turn_a_killed_background_planner_wrapper_left_is_stopped_as_the_planner_closes() {
    use dagq::application::ProcessControl;
    let processes = dagq::infrastructure::adapters::SystemProcesses;
    // Only the first planner's turn hangs; one opened after it ends.
    let fx = headless_fixture(
        r#"if [ ! -e "$DB.hung" ]; then : > "$DB.hung"; : > "$RUN_DIR/turn-running"; sleep 600; fi
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[revise(&["split it"])]);
    let backend = PlanWorkspace::default();
    let dir = planners_dir(&fx.db).join("1");
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || dir.join("turn-running").exists() && !queue_events(&fx.db, "turn_started").is_empty(),
        || diagnose_planner(&fx.db),
    );
    let started = queue_events(&fx.db, "turn_started")[0].clone();
    let turn = u32::try_from(started["pid"].as_u64().unwrap()).unwrap();
    let mut seen = vec![(turn, processes.start_identity(turn))];
    seen.extend(
        processes
            .descendants(turn)
            .into_iter()
            .map(|pid| (pid, processes.start_identity(pid))),
    );
    assert!(seen.len() > 1, "the turn runs its sleep: {seen:?}");
    let _guard = TurnGuard(seen.clone());
    let planner = queue.planner(dagq::domain::PlannerId::new(1)).unwrap();
    let handle = planner.workspace_id.clone().unwrap();
    processes.kill(planner.wrapper_pid.unwrap()).unwrap();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || {
            queue_events(&fx.db, "planner_closed")
                .iter()
                .any(|closed| closed["planner_id"] == 1)
        },
        || diagnose_planner(&fx.db),
    );
    let closed = queue_events(&fx.db, "planner_closed");
    let closed = closed.iter().find(|c| c["planner_id"] == 1).unwrap();
    assert_eq!(closed["code"], "runtime_lost", "{closed}");
    assert_eq!(closed["workspace_id"], handle);
    assert_eq!(closed["workspace_closed"], true, "{closed}");
    // The turn and what it started are gone.
    for (pid, start) in &seen {
        assert_ne!(processes.start_identity(*pid), *start, "{pid} still runs");
    }
}

/// Acceptance: the sweep judges a background planner's handle by its
/// process, never by cmux's listing, which has none: a live one whose
/// heartbeat is late keeps its row, and one whose wrapper ended on its
/// agent's exit is closed as `runtime_exited`.
#[test]
fn the_sweep_keeps_a_live_background_planner_and_closes_an_ended_one_as_exited() {
    use dagq::application::{ProcessControl, WorkspaceBackend, planner::close_abandoned_planners};
    let fx = fixture();
    let queue = SqliteQueue::open(&fx.db).unwrap();
    let backend = PlanWorkspace::default();
    let log = fx.db.parent().unwrap().join("wrapper.log");
    let handle = backend
        .launch_background(fx.db.parent().unwrap(), "sleep 600", &[], &log)
        .unwrap();
    let wrapper = dagq::domain::background_wrapper::BackgroundHandle::parse(&handle)
        .unwrap()
        .pid;
    let planner = queue
        .open_planner(dagq::domain::PlannerOrigin::Runtime, None)
        .unwrap();
    queue
        .set_planner_route(planner.id, PlannerRoute::Headless)
        .unwrap();
    queue
        .planner_workspace_created(planner.id, &handle)
        .unwrap();
    queue.register_planner_wrapper(planner.id, wrapper).unwrap();
    // Its heartbeat is far older than the timeout.
    rusqlite::Connection::open(&fx.db)
        .unwrap()
        .execute(
            "UPDATE planners SET heartbeat_at = heartbeat_at - 100000",
            [],
        )
        .unwrap();
    let processes = dagq::infrastructure::adapters::SystemProcesses;
    let clock = dagq::infrastructure::clock::system().clock;
    assert!(processes.alive(wrapper));
    let closed = close_abandoned_planners(&queue, &backend, &processes, &*clock).unwrap();
    assert!(closed.is_empty(), "{closed:?}");
    assert!(queue.planner(planner.id).unwrap().closed_at.is_none());
    // Its wrapper ended on its agent's exit: the row closes, as exited.
    queue.planner_exited(planner.id, wrapper, 0).unwrap();
    backend.close(&handle).unwrap();
    assert!(!backend.exists(&handle).unwrap());
    let closed = close_abandoned_planners(&queue, &backend, &processes, &*clock).unwrap();
    assert_eq!(closed, [planner.id]);
    let events = queue_events(&fx.db, "planner_closed");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["code"], "runtime_exited");
}
