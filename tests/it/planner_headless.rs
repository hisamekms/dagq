//! The runtime's planners run headless only (ADR-t1394-2 decision 2,
//! ADR-t1433-2 decision 3): a planner the supervisor opens runs one call
//! of the agent (the stub `claude` of [`headless_claude`]) per turn, its
//! revise comes as the next request in its directory's `turns/`, its turns
//! are events of the queue that name it, and its wrapper, started in the
//! background without a workspace (ADR-t1404-1 decision 8), ends on the
//! exit request. `[roles.runtime_planner] route` and `[headless] wrapper`
//! of `dagq.toml`, whatever they say, are accepted and ignored.

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
pub(crate) fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
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
pub(crate) fn diagnose_planner(db: &Path) -> String {
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

/// A fixture whose planner's agent is the stub of [`headless_claude`]
/// running `turns` (with `$DB` naming the queue, which is not the
/// checkout's). Its `dagq.toml` still names the interactive route and a
/// workspace for the wrapper, which the runtime's planners ignore: they
/// run headless in the background (ADR-t1433-2 decision 3).
pub(crate) fn headless_fixture(turns: &str) -> crate::plan_review::Fixture {
    let mut fx = fixture();
    configure(
        &fx.repo,
        "[roles.runtime_planner]\nroute = \"interactive\"\n\n[headless]\nwrapper = \"workspace\"\n",
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
pub(crate) fn supervise_until(
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

/// Acceptance: the planner the supervisor opens for a revise runs in the
/// background without a workspace, its prompt as its first turn, though
/// `dagq.toml` names the interactive route; its turn's output is kept in `turns/`, the
/// turn is recorded on the queue with its ID, and once it submitted the
/// proposal again the exit request ends its wrapper and the runtime closes
/// it. Nothing is typed and no workspace is opened; `doctor` shows the
/// one route.
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
    let backend = PlanWorkspace::running();
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
    assert_eq!(doctor["roles"]["runtime_planner"].get("route_source"), None);
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
    let backend = PlanWorkspace::running();
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

/// Acceptance (ADR-t1433-2 decision 3): with the old `route =
/// "interactive"` of `[roles.runtime_planner]`, the planner the supervisor
/// opens for a revise runs headless: its wrapper starts in the background
/// (`--headless --background`) and no workspace opens, its directory has
/// `turns/`, and once it is idle the next revise is written as its next
/// request (`turn_requested`), never typed. The supervisor warns, once,
/// that the key is ignored (handled as ADR-t1433-3 decision 2 handles a
/// switch key). `doctor` shows the one route. That the key loads whatever
/// its value is the unit test
/// `run_env::tests::the_old_route_of_the_runtimes_planners_is_accepted_and_ignored`.
#[test]
fn the_old_route_key_is_ignored_and_the_runtimes_planner_runs_headless() {
    let fx = fixture();
    configure(
        &fx.repo,
        "[roles.runtime_planner]\nroute = \"interactive\"\n",
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[revise(&["split it"]), revise(&["name the test"])]);
    // Parked: the test plays the planner.
    let backend = PlanWorkspace::default();
    let settings = options(1, Duration::from_secs(3600));
    let (telemetry, captured) = dagq::infrastructure::telemetry::Telemetry::capture();
    telemetry.in_scope(|| supervise_with(&fx, &backend, &reviewer, &settings));
    let log = captured.text();
    let warning = "[roles.runtime_planner] route = \\\"interactive\\\" of dagq.toml is ignored";
    assert_eq!(log.matches(warning).count(), 1, "{log}");
    let planners = queue.planners(false).unwrap();
    assert_eq!(planners.len(), 1, "{planners:?}");
    let planner = &planners[0];
    assert_eq!(planner.route, PlannerRoute::Headless);
    let handle = planner.workspace_id.clone().unwrap();
    assert!(is_background(&handle), "{handle}");
    let launched = backend.background.launched();
    assert_eq!(launched.len(), 1, "{launched:?}");
    assert_eq!(launched[0].0, handle);
    assert!(launched[0].1.contains("'planner-session'"), "{launched:?}");
    assert!(launched[0].1.contains("'--headless'"), "{launched:?}");
    assert!(launched[0].1.contains("'--background'"), "{launched:?}");
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    assert!(dir.join("turns").join("limits.json").is_file());

    // The planner resubmits as its own and is idle: the next revise
    // is written as its next request, not typed.
    crate::runtime_support::planner_turns::idle(&queue, &fx.db, planner.id);
    queue
        .submit(dagq::domain::Submission {
            tasks: Vec::new(),
            goals: Vec::new(),
            proposal: Some(proposal),
            owner: dagq::domain::PlannerOwner {
                origin: dagq::domain::PlannerOrigin::Runtime,
                workspace_id: Some(handle.clone()),
            },
        })
        .unwrap();
    supervise_with(&fx, &backend, &reviewer, &settings);
    assert!(backend.texts().is_empty(), "nothing typed");
    let requests = crate::runtime_support::planner_turns::turn_requests(&fx.db, planner.id);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["what"], "revise");
    assert!(
        requests[0]["prompt"]
            .as_str()
            .unwrap()
            .contains("name the test"),
        "{requests:?}"
    );
    let requested = queue_events(&fx.db, "turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["planner_id"], planner.id.as_i64());
    let doctor = crate::common::cli::ok(&fx.db, &["doctor"]);
    assert_eq!(doctor["roles"]["runtime_planner"]["route"], "headless");
    assert_eq!(doctor["roles"]["runtime_planner"].get("route_source"), None);
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
    let backend = PlanWorkspace::running();
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
    // A descendant that ended between the listing and the read of its start
    // (a `sleep` of the stub's watchdog, task 1580) is none to stop.
    seen.retain(|(_, start)| start.is_some());
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
    // A parked wrapper: a real process that ends once the test's process
    // is gone, however the test ends.
    let backend = PlanWorkspace::default();
    let log = fx.db.parent().unwrap().join("wrapper.log");
    let handle = backend
        .launch_background(fx.db.parent().unwrap(), "planner-session", &[], &log)
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

/// ADR-t1404-1 decisions 2 and 10, ADR-t1433-1: whether a background
/// planner lives is judged by its handle's pid and recorded start alone,
/// without cmux (`planners --cmux` names none that runs). One whose
/// heartbeat is far past the timeout but whose wrapper runs is alive (not
/// `lost`, so the supervisor neither ends its row nor closes its span); one
/// whose wrapper is gone reads `closed`.
#[test]
fn a_background_planner_with_a_late_heartbeat_is_alive_until_its_wrapper_is_gone() {
    use crate::common::cli;
    use dagq::application::WorkspaceBackend;
    let fx = fixture();
    let queue = SqliteQueue::open(&fx.db).unwrap();
    // A parked wrapper: a real process that ends once the test's process
    // is gone, however the test ends.
    let backend = PlanWorkspace::default();
    let log = fx.db.parent().unwrap().join("wrapper.log");
    let handle = backend
        .launch_background(fx.db.parent().unwrap(), "planner-session", &[], &log)
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
    rusqlite::Connection::open(&fx.db)
        .unwrap()
        .execute(
            "UPDATE planners SET heartbeat_at = heartbeat_at - 100000",
            [],
        )
        .unwrap();
    let view = || {
        let planners = cli::ok(&fx.db, &["planners", "--cmux", "/nonexistent/cmux"]);
        planners["planners"]
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["id"] == planner.id.as_i64())
            .cloned()
            .unwrap_or_else(|| panic!("{planners}"))
    };
    let alive = view();
    assert_eq!(alive["alive"], true, "{alive}");
    assert_ne!(alive["state"], "lost", "{alive}");
    // Its wrapper is gone: the row reads closed, by the handle alone.
    backend.close(&handle).unwrap();
    let gone = view();
    assert_eq!(gone["state"], "closed", "{gone}");
    assert_eq!(gone["alive"], false, "{gone}");
}

/// A planning request's planner (ADR-t1394-1) on the headless route: its
/// `planner_question` about the request waits for the answer, which comes
/// as its next turn, and its decline from that turn ends the request.
#[test]
fn a_headless_request_planner_takes_the_answer_as_its_next_turn_and_declines() {
    let fx = headless_fixture(
        r#"case "$TURN" in
1) "$DAGQ" --db "$DB" ask --kind planner_question --because scope --request 1 --question "plan it?" --option plan --option decline --cmux true >> "$RUN_DIR/ask.log" 2>&1 ;;
*) "$DAGQ" --db "$DB" request decline 1 --reason "done already" >> "$RUN_DIR/decline.log" 2>&1 ;;
esac
say "turn $TURN""#,
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let request = queue
        .record_plan_request(
            &dagq::domain::plan_request::NewPlanRequest {
                text: "plan the landing rate back".into(),
                note: None,
                refs: Vec::new(),
                priority: None,
            },
            "inbox",
            "inbox",
        )
        .unwrap()
        .id;
    let reviewer = StubReviewer::new(&[]);
    let backend = PlanWorkspace::running();
    let open_question = || {
        SqliteQueue::open(&fx.db)
            .unwrap()
            .asks(Default::default())
            .unwrap()
            .into_iter()
            .find(|ask| ask.request_id == Some(request))
    };
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || open_question().is_some() && !queue_events(&fx.db, "turn_finished").is_empty(),
        || diagnose_planner(&fx.db),
    );
    let planner = queue.planners(false).unwrap()[0].clone();
    assert_eq!(planner.route, PlannerRoute::Headless);
    assert_eq!(planner.request_id, Some(request));
    let asked = open_question().unwrap();
    queue.answer(asked.id, "decline").unwrap();
    supervise_until(
        &fx,
        &backend,
        &reviewer,
        || !queue_events(&fx.db, "planner_closed").is_empty(),
        || diagnose_planner(&fx.db),
    );
    assert!(backend.texts().is_empty(), "nothing typed");
    let turns = planners_dir(&fx.db)
        .join(planner.id.to_string())
        .join("turns");
    let taken: Value =
        serde_json::from_str(&fs::read_to_string(turns.join("request-000001.taken.json")).unwrap())
            .unwrap();
    assert_eq!(taken["what"], format!("answer of ask {}", asked.id));
    assert_eq!(
        taken["prompt"],
        format!("answer to ask {}: decline", asked.id)
    );
    use dagq::application::PlanRequestStore;
    let declined = queue.plan_request(request).unwrap();
    assert_eq!(
        declined.status,
        dagq::domain::plan_request::RequestStatus::Declined,
        "{}",
        diagnose_planner(&fx.db)
    );
    assert_eq!(declined.status_reason.as_deref(), Some("done already"));
}
