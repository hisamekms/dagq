//! A planner whose idle marker is missing, or older than its last input,
//! is judged idle by its screen (ADR-t803-1): cmux's capture showing the
//! input box ready, no work and no dialog over `[stall].screen_idle_secs`.
//! The supervisor records `idle_inferred` once per span and asks a planner
//! of the runtime's to exit; a person's planner only shows `idle`.

use crate::common::WithoutActor;
use crate::plan_review::{
    Fixture, PlanWorkspace, StubReviewer, fixture, open_goal, options, runtime_draft,
};
use dagq::{
    application::{
        Clock, Generators, TaskStore,
        planner::{PlannerProbes, PlannerView, planner_views},
        planner_idle_marker,
        screen_idle::{ScreenIdle, Spans},
    },
    domain::{DraftOrigin, PlannerId, PlannerOrigin, PlannerState, TaskAction, stall::StallConfig},
    infrastructure::{
        adapters::{ClaudeCode, SystemProcesses},
        location::planners_dir,
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
    },
    runtime::{self, SuperviseOptions},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime},
};

/// How long a screen must look idle in these tests.
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

/// Claude Code at rest with background shells still running, as its
/// status line under the input box counts them.
const BACKGROUND: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯
──────────────────────────────────────────────────────────────────────
  ⏵⏵ auto mode on · 1 shell · ← for agents · ↓ to manage
";

/// Claude Code holding a dialog over its input.
const DIALOG: &str = "\
 Do you want to proceed?
 ❯ 1. Yes
   2. No, and tell Claude what to do differently (esc)
";

/// The wall clock moved on by a number of seconds the test sets, so a span
/// covers seconds without the test sleeping through them.
#[derive(Default)]
struct Ahead(AtomicI64);

impl Ahead {
    fn by(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }
}

impl Clock for Ahead {
    fn system_time(&self) -> SystemTime {
        SystemTime::now() + Duration::from_secs(self.0.load(Ordering::SeqCst) as u64)
    }
}

fn supervise(fx: &Fixture, backend: &PlanWorkspace, clock: &Arc<Ahead>, at: i64) {
    clock.by(at);
    let options = SuperviseOptions {
        generators: Generators {
            clock: clock.clone(),
            ids: dagq::infrastructure::clock::system().ids,
        },
        stall: Some(StallConfig {
            screen_idle_secs: SCREEN_IDLE_SECS,
            ..Default::default()
        }),
        ..options(1, Duration::from_secs(3600))
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

/// The planners judged as `dagq planners` would, the captures not kept.
fn views(
    fx: &Fixture,
    backend: &PlanWorkspace,
    clock: &Ahead,
    mode: ScreenIdle,
) -> Vec<PlannerView> {
    let queue = SqliteQueue::open(&fx.db).unwrap();
    let planners = planners_dir(&fx.db);
    planner_views(
        &queue,
        &PlannerProbes {
            cmux: backend,
            processes: &SystemProcesses,
            files: &LocalRunFiles,
            signals: &ClaudeCode {
                executable: "claude".into(),
            },
            clock,
            planners_dir: &planners,
            screen_idle_threshold: Duration::from_secs(SCREEN_IDLE_SECS as u64),
            screen_idle: mode,
        },
        false,
    )
    .unwrap()
}

fn inferred(queue: &SqliteQueue) -> Vec<Value> {
    queue
        .latest_events_of("idle_inferred", 10)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

fn dir(fx: &Fixture, id: PlannerId) -> PathBuf {
    planners_dir(&fx.db).join(id.to_string())
}

/// A planner of the runtime's whose `Stop` hook could not write its idle
/// marker (the disk was full) is asked to exit once its screen looks idle
/// long enough, and its row is closed once it exits; `idle_inferred`
/// names the failed hook from its debug log, once for the span.
#[test]
fn a_runtime_planner_without_its_marker_is_ended_by_its_screen() {
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
    let backend = PlanWorkspace::default();
    let clock = Arc::new(Ahead::default());
    supervise(&fx, &backend, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    assert_eq!(planner.origin, PlannerOrigin::Runtime);
    let workspace = planner.workspace_id.clone().unwrap();
    // It decided its draft and stopped, but no marker was written.
    queue.transition(draft, TaskAction::Cancel).unwrap();
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    fs::write(
        dir(&fx, planner.id).join("claude.log"),
        "2026-09-27T01:00:00Z [DEBUG] turn\n\
         2026-09-27T01:02:00Z [DEBUG] Hook Stop (Stop) error: No space left on device\n",
    )
    .unwrap();
    assert!(!planner_idle_marker(&dir(&fx, planner.id)).exists());
    *backend.screen.lock().unwrap() = Some(Ok(READY.into()));

    // One capture is not a span yet.
    supervise(&fx, &backend, &clock, 2);
    assert!(backend.exits.lock().unwrap().is_empty());
    assert!(inferred(&queue).is_empty());
    let before = &views(&fx, &backend, &clock, ScreenIdle::Peek)[0];
    assert_eq!(before.state, PlannerState::Working);
    assert_eq!(before.idle_since, None);

    // A second capture past the threshold infers it idle.
    supervise(&fx, &backend, &clock, 2 + SCREEN_IDLE_SECS);
    assert_eq!(*backend.exits.lock().unwrap(), [workspace]);
    let events = inferred(&queue);
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["planner_id"], planner.id.as_i64());
    assert_eq!(event["origin"], "runtime");
    assert_eq!(event["source"], "screen");
    assert_eq!(event["marker"], "missing");
    assert_eq!(event["captures"], 2);
    assert_eq!(event["background_running"], false);
    // The wall clock may tick between the passes.
    assert!(event["observed_secs"].as_i64().unwrap() >= SCREEN_IDLE_SECS);
    assert_eq!(
        event["hook_error"],
        "2026-09-27T01:02:00Z [DEBUG] Hook Stop (Stop) error: No space left on device"
    );
    let view = &views(&fx, &backend, &clock, ScreenIdle::Peek)[0];
    assert_eq!(view.state, PlannerState::Idle);
    assert_eq!(view.idle_since, event["since"].as_i64());
    assert_eq!(
        serde_json::to_value(view).unwrap()["idle_inferred"]["marker"],
        "missing"
    );

    // Still idle, the span is recorded once.
    supervise(&fx, &backend, &clock, 3 + SCREEN_IDLE_SECS);
    assert_eq!(inferred(&queue).len(), 1, "{:?}", inferred(&queue));

    // It exits, and its row is closed.
    queue
        .planner_exited(planner.id, std::process::id(), 0)
        .unwrap();
    supervise(&fx, &backend, &clock, 4 + SCREEN_IDLE_SECS);
    assert!(queue.planner(planner.id).unwrap().closed_at.is_some());

    // `dagq events` reads it.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .arg("--db")
        .arg(&fx.db)
        .args(["events", "--kind", "idle_inferred", "--full"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("idle_inferred") && text.contains("No space left"),
        "{text}"
    );
}

/// A person's planner without a fresh marker shows `idle` once its screen
/// looks idle long enough, and is never asked to exit. A screen at work,
/// one with a dialog, or one cmux cannot read infers nothing; a marker
/// older than the planner's last input is stale and the screen decides.
#[test]
fn a_persons_planner_shows_idle_by_its_screen_and_is_not_asked_to_exit() {
    let fx = fixture();
    let queue = SqliteQueue::open(&fx.db).unwrap();
    let planner = queue.open_planner(PlannerOrigin::Person, None).unwrap();
    queue.planner_workspace_created(planner.id, "PW1").unwrap();
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    fs::create_dir_all(dir(&fx, planner.id)).unwrap();
    let backend = PlanWorkspace::listing(&["PW1"]);
    let clock = Arc::new(Ahead::default());
    let spans = Spans::default();
    let state = |at: i64| {
        clock.by(at);
        let view = views(&fx, &backend, &clock, ScreenIdle::Record(&spans)).remove(0);
        (view.state, view.idle_since)
    };

    // Busy screens, and a screen that cannot be read, infer nothing
    // however long they last.
    for screen in [
        Ok(WORKING),
        Ok(DIALOG),
        Ok(""),
        Err("cmux read-screen failed"),
    ] {
        *backend.screen.lock().unwrap() = Some(screen.map(str::to_owned).map_err(str::to_owned));
        for at in [2, 2 + SCREEN_IDLE_SECS, 3 + SCREEN_IDLE_SECS] {
            assert_eq!(state(at), (PlannerState::Working, None), "{screen:?}");
        }
    }

    // A marker older than the last input is stale: the screen decides.
    let marker = planner_idle_marker(&dir(&fx, planner.id));
    fs::write(&marker, r#"{"hook_event_name":"Stop"}"#).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    fs::write(dir(&fx, planner.id).join("prompt-submit.json"), "{}").unwrap();
    *backend.screen.lock().unwrap() = Some(Ok(READY.into()));
    assert_eq!(state(10), (PlannerState::Working, None));
    // The screen at work breaks the span; it starts over.
    *backend.screen.lock().unwrap() = Some(Ok(WORKING.into()));
    assert_eq!(state(12), (PlannerState::Working, None));
    *backend.screen.lock().unwrap() = Some(Ok(READY.into()));
    assert_eq!(state(13), (PlannerState::Working, None));
    // A copy the disk lost (a full disk truncates it) loses no capture:
    // the spans are kept in memory.
    fs::write(dir(&fx, planner.id).join("screen-idle.json"), "").unwrap();
    let (idle, since) = state(13 + SCREEN_IDLE_SECS);
    assert_eq!(idle, PlannerState::Idle);
    let since = since.unwrap();
    // A capture cmux fails keeps the span but infers nothing now.
    *backend.screen.lock().unwrap() = Some(Err("cmux read-screen failed".into()));
    assert_eq!(state(14 + SCREEN_IDLE_SECS), (PlannerState::Working, None));
    *backend.screen.lock().unwrap() = Some(Ok(READY.into()));
    assert_eq!(
        state(15 + SCREEN_IDLE_SECS),
        (PlannerState::Idle, Some(since))
    );

    // The supervisor records the stale marker's span, and sends a person's
    // planner no `/exit`.
    supervise(&fx, &backend, &clock, 16 + SCREEN_IDLE_SECS);
    assert!(backend.exits.lock().unwrap().is_empty());
    let events = inferred(&queue);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["marker"], "stale");
    assert_eq!(events[0]["origin"], "person");
    assert_eq!(events[0]["since"], since);
    assert!(events[0].get("hook_error").is_none(), "{events:?}");

    // A fresh marker decides again, over the screen.
    fs::write(&marker, r#"{"hook_event_name":"Stop"}"#).unwrap();
    *backend.screen.lock().unwrap() = Some(Ok(WORKING.into()));
    assert_eq!(state(17 + SCREEN_IDLE_SECS).0, PlannerState::Working);
    *backend.screen.lock().unwrap() = Some(Ok(DIALOG.into()));
    assert_eq!(state(18 + SCREEN_IDLE_SECS).0, PlannerState::Idle);
}

/// Task 823: a planner of the runtime's without its marker whose screen
/// shows background shells running is not sent a `/exit` (it would stop at
/// Claude Code's "Background work is running" dialog), however long the
/// screen stays so: it is `working`, and `idle_inferred` says what the
/// screen showed. Once the shells are done it is idle and asked to exit.
#[test]
fn a_runtime_planner_whose_screen_shows_background_work_is_not_asked_to_exit() {
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
    let backend = PlanWorkspace::default();
    let clock = Arc::new(Ahead::default());
    supervise(&fx, &backend, &clock, 0);
    let planner = queue.planners(false).unwrap().remove(0);
    let workspace = planner.workspace_id.clone().unwrap();
    queue.transition(draft, TaskAction::Cancel).unwrap();
    queue
        .register_planner_wrapper(planner.id, std::process::id())
        .unwrap();
    *backend.screen.lock().unwrap() = Some(Ok(BACKGROUND.into()));

    for at in [2, 2 + SCREEN_IDLE_SECS, 3 + SCREEN_IDLE_SECS * 3] {
        supervise(&fx, &backend, &clock, at);
        assert!(backend.exits.lock().unwrap().is_empty(), "at {at}");
    }
    let view = &views(&fx, &backend, &clock, ScreenIdle::Peek)[0];
    assert_eq!(view.state, PlannerState::Working);
    assert_eq!(view.idle_since, None);
    let events = inferred(&queue);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["background_running"], true);
    assert_eq!(events[0]["marker"], "missing");

    // The shells are done: the screen at rest starts a new span, and the
    // planner is asked to exit once it is long enough.
    *backend.screen.lock().unwrap() = Some(Ok(READY.into()));
    supervise(&fx, &backend, &clock, 4 + SCREEN_IDLE_SECS * 3);
    assert!(backend.exits.lock().unwrap().is_empty());
    supervise(&fx, &backend, &clock, 4 + SCREEN_IDLE_SECS * 4);
    assert_eq!(*backend.exits.lock().unwrap(), [workspace]);
    let events = inferred(&queue);
    assert_eq!(events.len(), 2, "{events:?}");
    // The latest first.
    assert_eq!(events[0]["background_running"], false, "{events:?}");
}
