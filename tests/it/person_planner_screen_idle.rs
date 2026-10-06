//! A person's planner opened before `dagq plan` was abolished (ADR-t1394-1)
//! whose idle marker is missing, or older than its last input, is judged
//! idle by its screen (ADR-t803-1): cmux's capture showing the input box
//! ready, no work and no dialog over `[stall].screen_idle_secs`. The
//! supervisor records `idle_inferred` once per span and never asks it to
//! exit. A planner of the runtime's has no screen (ADR-t1433-2): its tests
//! here were removed with the interactive route (task 1441), and the
//! person's planner's rows are task 1577's to retire.

use crate::plan_review::{Fixture, PlanWorkspace, StubReviewer, fixture, options};
use dagq::{
    application::{
        Clock, Generators,
        planner::{PlannerProbes, PlannerView, planner_views},
        planner_idle_marker,
        screen_idle::{ScreenIdle, Spans},
    },
    domain::{PlannerId, PlannerOrigin, PlannerState, stall::StallConfig},
    infrastructure::{
        adapters::{ClaudeCode, SystemProcesses},
        location::planners_dir,
        run_files::LocalRunFiles,
        sqlite::SqliteQueue,
    },
    runtime::{self, SuperviseOptions},
};
use serde_json::Value;
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

    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
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
    queue
        .register_planner_agent(planner.id, std::process::id(), std::process::id())
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
