//! A planner of the runtime's nothing was seen of within the planner
//! timeout (task 805): no input, no idle marker and no idle screen since
//! its last one. The inbox is told once per planner by
//! `planner_unresponsive`, and the planner is left open; a person's
//! planner is never timed.

use crate::plan_review::{PlanWorkspace, StubReviewer, fixture, open_goal, options, runtime_draft};
use dagq::{
    application::{Clock, Generators, planner_idle_marker},
    domain::{DraftOrigin, EventId, FindingTarget, NewFinding, PlannerOrigin, stall::StallConfig},
    infrastructure::{location::planners_dir, sqlite::SqliteQueue},
    runtime::{self, SuperviseOptions},
};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime},
};

/// The planner timeout in these tests. Each step leaves about half of it
/// to the real time a pass takes on a loaded host.
const TIMEOUT_SECS: u64 = 60;

/// How long a screen must look idle in these tests, well within the
/// timeout.
const SCREEN_IDLE_SECS: i64 = 5;

/// Claude Code at rest: its empty input box, no spinner, no dialog.
const READY: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// Claude Code at work on a turn.
const WORKING: &str = "\
⏺ Done.

✻ Working… (3s · esc to interrupt)

──────────────────────────────────────────────────────────────────────
❯
──────────────────────────────────────────────────────────────────────
  ? for shortcuts
";

/// The wall clock moved on by a number of seconds the test sets.
#[derive(Default)]
struct Ahead(AtomicI64);

impl Clock for Ahead {
    fn system_time(&self) -> SystemTime {
        SystemTime::now() + Duration::from_secs(self.0.load(Ordering::SeqCst) as u64)
    }

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

fn supervise(
    fx: &crate::plan_review::Fixture,
    backend: &PlanWorkspace,
    clock: &Arc<Ahead>,
    at: i64,
) {
    clock.0.store(at, Ordering::SeqCst);
    let options = SuperviseOptions {
        generators: Generators {
            clock: clock.clone(),
            ids: dagq::infrastructure::clock::system().ids,
        },
        stall: Some(StallConfig {
            screen_idle_secs: SCREEN_IDLE_SECS,
            ..Default::default()
        }),
        ..options(1, Duration::from_secs(TIMEOUT_SECS))
    };
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        backend,
        &fx.claude,
        &StubReviewer::new(&[]),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
}

fn unresponsive(queue: &SqliteQueue) -> Vec<Value> {
    queue
        .latest_events_of("planner_unresponsive", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

/// Move the planners' heartbeats far ahead, so the clock the test moves on
/// never finds their wrappers lost.
fn heartbeat_ahead(db: &Path) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE planners SET heartbeat_at = heartbeat_at + 100000 WHERE heartbeat_at IS NOT NULL",
            [],
        )
        .unwrap();
}

/// Set a file's modification time `secs` ahead of now, as if it was
/// written when the test's clock reads that.
fn touch_ahead(path: &Path, secs: u64) {
    fs::write(path, "{}").unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(secs))
        .unwrap();
}

/// A draft's planner whose screen cmux cannot read and whose idle marker
/// was never written: an input or a marker in time holds the timeout off;
/// past it, the inbox is told once, as the planner's attention on its
/// draft, and the planner is neither asked to exit nor closed.
#[test]
fn a_silent_runtime_planner_is_told_to_the_inbox_once() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    let draft = runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::GoalGap,
        json!({"findings": ["the acceptance names a check nobody runs"]}),
    );
    let backend = PlanWorkspace::listing(&["PW"]);
    let clock = Arc::new(Ahead::default());
    supervise(&fx, &backend, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(planner.origin, PlannerOrigin::Runtime);
    assert_eq!(planner.draft_task_id, Some(draft));
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    // A person's planner, as silent, is never timed.
    let person = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue.planner_workspace_created(person.id, "PW").unwrap();
    queue
        .register_planner_wrapper(person.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(person.id, std::process::id(), std::process::id())
        .unwrap();
    *backend.screen.lock().unwrap() = Some(Err("cmux read-screen failed".into()));
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    fs::create_dir_all(&dir).unwrap();

    heartbeat_ahead(&fx.db);

    // Within the timeout, nothing.
    supervise(&fx, &backend, &clock, 30);
    assert!(unresponsive(&queue).is_empty());

    // An input the planner took holds the timeout off (70 seconds from
    // its opening without it).
    touch_ahead(&dir.join("prompt-submit.json"), 69);
    supervise(&fx, &backend, &clock, 70);
    assert!(unresponsive(&queue).is_empty());
    // So does an idle marker after it, on a screen at work (31 seconds
    // from the input without it).
    *backend.screen.lock().unwrap() = Some(Ok(WORKING.into()));
    touch_ahead(&planner_idle_marker(&dir), 99);
    supervise(&fx, &backend, &clock, 100);
    assert!(unresponsive(&queue).is_empty());

    // Past the timeout from its last input, with its marker older than
    // that and its screen unread, the inbox is told once.
    touch_ahead(&dir.join("prompt-submit.json"), 101);
    *backend.screen.lock().unwrap() = Some(Err("cmux read-screen failed".into()));
    let past = 101 + TIMEOUT_SECS as i64;
    supervise(&fx, &backend, &clock, past + 2);
    supervise(&fx, &backend, &clock, past + 3);
    let events = unresponsive(&queue);
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["subject"], "planner");
    assert_eq!(event["planner_id"], planner.id.as_i64());
    assert_eq!(event["origin"], "runtime");
    assert_eq!(event["draft_task_id"], json!(draft));
    assert_eq!(event["state"], "working");
    assert!(event["waited_secs"].as_i64().unwrap() > TIMEOUT_SECS as i64);
    assert!(event.get("proposal_id").is_none(), "{event}");
    // It is left open: no `/exit`, no close.
    assert!(backend.exits.lock().unwrap().is_empty());
    assert!(queue.planner(planner.id).unwrap().closed_at.is_none());

    // The inbox reads it in `status` and `watch`.
    let status = runtime::status(&fx.db).unwrap();
    let attention: Vec<&Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "planner_unresponsive")
        .collect();
    assert_eq!(attention.len(), 1, "{status}");
    assert_eq!(attention[0]["next"], "check the planner");
    assert_eq!(attention[0]["task_id"], json!(draft));
    let watched = dagq::compose::events(&fx.db, EventId::new(0), 100, false).unwrap();
    assert!(
        watched["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "planner_unresponsive" && e["next"] == "check the planner"),
        "{watched}"
    );

    // Once its row closes, the attention ends.
    queue.close_planner(planner.id, None).unwrap();
    let status = runtime::status(&fx.db).unwrap();
    assert!(
        !status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "planner_unresponsive"),
        "{status}"
    );
}

/// A finding's planner, as silent, is told to the inbox once, naming the
/// finding, and is left open.
#[test]
fn a_silent_finding_planner_is_told_to_the_inbox_once() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let finding = queue
        .record_finding(NewFinding {
            kind: "conflict_hotspot".into(),
            target: FindingTarget::Queue,
            subject: "src/lib.rs".into(),
            summary: "src/lib.rs conflicts in most landings".into(),
            detail: None,
            impact: None,
            evidence: Vec::new(),
            propose: Some("split it".into()),
            by: "observer".into(),
        })
        .unwrap()
        .finding
        .id;
    let backend = PlanWorkspace::default();
    let clock = Arc::new(Ahead::default());
    supervise(&fx, &backend, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(planner.origin, PlannerOrigin::Runtime);
    assert_eq!(planner.finding_id, Some(finding));
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    *backend.screen.lock().unwrap() = Some(Err("cmux read-screen failed".into()));

    supervise(&fx, &backend, &clock, 30);
    assert!(unresponsive(&queue).is_empty());
    let past = TIMEOUT_SECS as i64;
    supervise(&fx, &backend, &clock, past + 2);
    supervise(&fx, &backend, &clock, past + 3);
    let events = unresponsive(&queue);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["subject"], "planner");
    assert_eq!(events[0]["planner_id"], planner.id.as_i64());
    assert_eq!(events[0]["finding_id"], json!(finding));
    assert_eq!(events[0]["draft_task_id"], Value::Null);
    assert!(backend.exits.lock().unwrap().is_empty());
    assert!(queue.planner(planner.id).unwrap().closed_at.is_none());
}

/// A draft's planner without its idle marker whose screen is inferred idle
/// within the timeout is asked to exit, and past the timeout the inbox is
/// told nothing about it.
#[test]
fn a_planner_inferred_idle_by_its_screen_is_not_told_to_the_inbox() {
    let fx = fixture();
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let goal = open_goal(&mut queue);
    runtime_draft(
        &mut queue,
        "gap",
        Some(goal),
        DraftOrigin::GoalGap,
        json!({"findings": ["the acceptance names a check nobody runs"]}),
    );
    let backend = PlanWorkspace::default();
    let clock = Arc::new(Ahead::default());
    supervise(&fx, &backend, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    let workspace = planner.workspace_id.clone().unwrap();
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
        .unwrap();
    heartbeat_ahead(&fx.db);
    let dir = planners_dir(&fx.db).join(planner.id.to_string());
    assert!(!planner_idle_marker(&dir).exists());
    *backend.screen.lock().unwrap() = Some(Ok(READY.into()));

    // Two captures over the screen threshold infer it idle: it is asked
    // to exit.
    supervise(&fx, &backend, &clock, 10);
    supervise(&fx, &backend, &clock, 10 + SCREEN_IDLE_SECS + 1);
    assert_eq!(
        *backend.exits.lock().unwrap(),
        std::slice::from_ref(&workspace)
    );
    assert_eq!(
        queue.latest_events_of("idle_inferred", 10).unwrap().len(),
        1
    );

    // Still idle past the timeout, with no input and no marker since its
    // opening: nothing is told.
    let past = TIMEOUT_SECS as i64;
    supervise(&fx, &backend, &clock, past + 2);
    supervise(&fx, &backend, &clock, past + 3);
    assert!(
        unresponsive(&queue).is_empty(),
        "{:?}",
        unresponsive(&queue)
    );
    // Each pass here is a new supervisor, which asks again: only `/exit`
    // reaches it, and it is not closed for silence.
    let exits = backend.exits.lock().unwrap().clone();
    assert!(exits.iter().all(|w| *w == workspace), "{exits:?}");
    assert!(queue.planner(planner.id).unwrap().closed_at.is_none());
}
