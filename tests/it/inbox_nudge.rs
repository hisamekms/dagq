//! The supervisor's nudge of an inbox without a watcher (ADR-t906-1
//! decision 1 (3)): while no watch watches and an ask waited past the
//! threshold, one line is typed into the inbox's workspace when its screen
//! looks idle with nothing typed, once more later, then a `cmux notify`,
//! each recorded as `inbox_nudged`. The watcher's changes are recorded as
//! `inbox_watcher_absent` / `inbox_watcher_returned`, and `kpi` derives
//! from them how long an ask waited to be seen (`ask_seen_wait`, task
//! 1021).

use crate::plan_review::{Fixture, PlanWorkspace, StubReviewer, fixture, options};
use dagq::{
    application::{Clock, Generators, inbox_watcher::WatcherRecord},
    domain::{
        AskKind, AskReason, EventKind, NewAsk, SessionRole, TaskId,
        event_kind::{
            ASK_OPENED, INBOX_NUDGE_FAILED, INBOX_NUDGED, INBOX_WATCHER_ABSENT,
            INBOX_WATCHER_RETURNED,
        },
        stall::StallConfig,
    },
    infrastructure::sqlite::SqliteQueue,
    runtime::{self, SuperviseOptions},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long a screen must look idle in these tests.
const SCREEN_IDLE_SECS: i64 = 5;

const INBOX: &str = "INBOX";

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

/// Claude Code at rest with a person's half-typed line in its box.
const TYPING: &str = "\
⏺ Done.

──────────────────────────────────────────────────────────────────────
❯ answer ask 3 with
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
}

struct Inbox {
    fx: Fixture,
    backend: PlanWorkspace,
    clock: Arc<Ahead>,
}

impl Inbox {
    /// A queue whose inbox workspace is recorded and listed, showing
    /// `screen`, with one open ask when `ask`.
    fn new(screen: &str, ask: bool) -> Self {
        let fx = fixture();
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        queue
            .register_session_workspace(SessionRole::Inbox, INBOX)
            .unwrap();
        if ask {
            open_ask(&mut queue);
        }
        let backend = PlanWorkspace::listing(&[INBOX]);
        *backend.screen.lock().unwrap() = Some(Ok(screen.to_owned()));
        Self {
            fx,
            backend,
            clock: Arc::default(),
        }
    }

    fn show(&self, screen: &str) {
        *self.backend.screen.lock().unwrap() = Some(Ok(screen.to_owned()));
    }

    /// One `supervise --once` with the clock `at` seconds ahead.
    fn supervise(&self, at: i64) {
        self.clock.0.store(at, Ordering::SeqCst);
        let options = SuperviseOptions {
            generators: Generators {
                clock: self.clock.clone(),
                ids: dagq::infrastructure::clock::system().ids,
            },
            stall: Some(StallConfig {
                screen_idle_secs: SCREEN_IDLE_SECS,
                ..Default::default()
            }),
            ..options(1, Duration::from_secs(3600))
        };
        runtime::supervise_with_reviewer(
            &self.fx.db,
            &self.fx.repo,
            &self.backend,
            &self.fx.claude,
            &StubReviewer::new(&[]),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap();
    }

    /// Supervise twice, [`SCREEN_IDLE_SECS`] apart from `at`: a screen that
    /// looks idle over them is inferred idle.
    fn supervise_twice(&self, at: i64) {
        self.supervise(at);
        self.supervise(at + SCREEN_IDLE_SECS + 1);
    }

    fn typed(&self) -> Vec<String> {
        self.backend
            .texts()
            .into_iter()
            .filter(|(workspace, _)| workspace == INBOX)
            .map(|(_, text)| text)
            .collect()
    }

    fn nudges(&self, kind: &str) -> Vec<Value> {
        let queue = SqliteQueue::open(&self.fx.db).unwrap();
        let mut events: Vec<Value> = queue
            .latest_events_of(kind, 20)
            .unwrap()
            .into_iter()
            .map(|event| event.payload)
            .collect();
        events.reverse();
        events
    }

    /// A watch's record under the queue's directory, its heartbeat `at`
    /// seconds ahead of now.
    fn watching(&self, at: i64) {
        let dir = self.fx.db.parent().unwrap().join("inbox-watchers");
        fs::create_dir_all(&dir).unwrap();
        let now = now() + at;
        // This test's own process, with its real start: a record whose pid
        // runs nothing or another process is not watching (task 927).
        let pid = std::process::id();
        let record = WatcherRecord {
            pid,
            started_at: dagq::infrastructure::adapters::process_started_at(pid).unwrap(),
            heartbeat_at: now,
            ended_at: None,
            timeout_secs: None,
            interval_secs: 2,
        };
        fs::write(dir.join("1-1.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    }
}

/// One open ask for the inbox on task 1.
fn open_ask(queue: &mut SqliteQueue) -> dagq::domain::AskId {
    queue
        .ask(NewAsk {
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: Some(TaskId::new(1)),
            run_id: None,
            question: "which way?".into(),
            options: vec![],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
        })
        .unwrap()
        .ask
        .id
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn an_idle_inbox_is_nudged_twice_then_the_person_is_notified_then_nothing() {
    let inbox = Inbox::new(READY, true);
    // The ask is new: nothing yet.
    inbox.supervise_twice(0);
    assert!(inbox.typed().is_empty());
    // Past the threshold, once the screen looked idle over two captures.
    inbox.supervise(400);
    assert!(inbox.typed().is_empty(), "one capture infers no idle");
    inbox.supervise(400 + SCREEN_IDLE_SECS + 1);
    let typed = inbox.typed();
    assert_eq!(typed.len(), 1, "{typed:?}");
    assert!(typed[0].contains("1 open ask(s)"), "{}", typed[0]);
    assert!(
        typed[0].contains("dagq status --role inbox"),
        "{}",
        typed[0]
    );
    assert!(!typed[0].contains('\n'), "one line: {}", typed[0]);
    let nudges = inbox.nudges(INBOX_NUDGED);
    assert_eq!(nudges.len(), 1);
    assert_eq!(nudges[0]["attempt"], 1);
    assert_eq!(nudges[0]["action"], "typed");
    assert_eq!(nudges[0]["absent_since"], 0);
    assert_eq!(nudges[0]["workspace_id"], INBOX);
    assert_eq!(nudges[0]["open_asks"], 1);
    // Not again in the same absence before the interval.
    inbox.supervise_twice(500);
    assert_eq!(inbox.typed().len(), 1);
    // Once more past it.
    inbox.supervise_twice(1_020);
    assert_eq!(inbox.typed().len(), 2);
    // Then the notify, idle or not, and nothing after it.
    inbox.show(WORKING);
    inbox.supervise(1_640);
    assert_eq!(inbox.typed().len(), 2);
    let notifications = inbox.backend.notifications();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert!(notifications[0].0.contains("inbox has no watch"));
    inbox.show(READY);
    inbox.supervise_twice(3_000);
    inbox.supervise_twice(5_000);
    assert_eq!(inbox.typed().len(), 2);
    assert_eq!(inbox.backend.notifications().len(), 1);
    let nudges = inbox.nudges(INBOX_NUDGED);
    let attempts: Vec<(i64, &str)> = nudges
        .iter()
        .map(|n| {
            (
                n["attempt"].as_i64().unwrap(),
                n["action"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(attempts, [(1, "typed"), (2, "typed"), (3, "notified")]);
    assert!(inbox.nudges(INBOX_NUDGE_FAILED).is_empty());
}

#[test]
fn failed_submissions_are_recorded_once_and_advance_to_the_next_nudge() {
    let mut inbox = Inbox::new(READY, true);
    let error = "fake inbox text submission failed";
    inbox.backend.send_text_error = Some(error.into());

    // send_text fails and the line is not left in the box, so submit_input
    // returns that error.
    // Each supervise call must still return successfully.
    inbox.supervise_twice(400);
    let first = inbox.nudges(INBOX_NUDGED);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["absent_since"], 0);
    assert_eq!(first[0]["attempt"], 1);
    assert_eq!(first[0]["action"], "typed");
    assert_eq!(first[0]["workspace_id"], INBOX);
    let failed = |attempt| {
        json!({
            "absent_since": 0,
            "attempt": attempt,
            "action": "typed",
            "workspace_id": INBOX,
            "error": error,
        })
    };
    assert_eq!(inbox.nudges(INBOX_NUDGE_FAILED), [failed(1)]);
    assert_eq!(inbox.typed().len(), 1);

    // A fresh supervisor reads the failed attempt from the persisted claim.
    // The screen remains eligible, so only the history prevents a retry.
    inbox.supervise_twice(500);
    assert_eq!(inbox.typed().len(), 1);
    assert_eq!(inbox.nudges(INBOX_NUDGED), first);
    assert_eq!(inbox.nudges(INBOX_NUDGE_FAILED), [failed(1)]);
    assert!(inbox.backend.notifications().is_empty());

    inbox.supervise_twice(1_020);
    assert_eq!(inbox.typed().len(), 2);
    assert_eq!(inbox.nudges(INBOX_NUDGE_FAILED), [failed(1), failed(2)]);
    assert!(inbox.backend.notifications().is_empty());
    inbox.supervise_twice(1_100);
    assert_eq!(inbox.typed().len(), 2);
    assert_eq!(inbox.nudges(INBOX_NUDGED).len(), 2);

    inbox.supervise(1_640);
    inbox.supervise_twice(3_000);
    assert_eq!(inbox.typed().len(), 2);
    let notifications = inbox.backend.notifications();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].0.contains("inbox has no watch"));
    let nudges = inbox.nudges(INBOX_NUDGED);
    let attempts: Vec<_> = nudges
        .iter()
        .map(|n| {
            (
                n["attempt"].as_i64().unwrap(),
                n["action"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(attempts, [(1, "typed"), (2, "typed"), (3, "notified")]);
    assert_eq!(inbox.nudges(INBOX_NUDGE_FAILED), [failed(1), failed(2)]);
}

#[test]
fn an_inbox_at_work_or_with_a_line_typed_is_not_nudged() {
    let inbox = Inbox::new(WORKING, true);
    inbox.supervise_twice(400);
    inbox.supervise_twice(420);
    assert!(inbox.typed().is_empty());
    inbox.show(TYPING);
    inbox.supervise_twice(440);
    inbox.supervise_twice(460);
    assert!(inbox.typed().is_empty());
    assert!(inbox.nudges(INBOX_NUDGED).is_empty());
    // Once the box is empty and the screen idle, it is.
    inbox.show(READY);
    inbox.supervise_twice(480);
    assert_eq!(inbox.typed().len(), 1);
}

#[test]
fn nothing_is_typed_with_a_watcher_without_an_ask_or_without_the_workspace() {
    // A watcher alive.
    let inbox = Inbox::new(READY, true);
    inbox.watching(400);
    inbox.supervise_twice(400);
    assert!(inbox.typed().is_empty());
    assert!(inbox.nudges(INBOX_NUDGED).is_empty());
    // No ask.
    let inbox = Inbox::new(READY, false);
    inbox.supervise_twice(400);
    assert!(inbox.typed().is_empty());
    // The workspace closed.
    let inbox = Inbox::new(READY, true);
    let _ = dagq::application::WorkspaceBackend::close(&inbox.backend, INBOX);
    inbox.supervise_twice(400);
    assert!(inbox.typed().is_empty());
    assert!(inbox.nudges(INBOX_NUDGED).is_empty());
    // Not recorded.
    let inbox = Inbox::new(READY, true);
    SqliteQueue::open(&inbox.fx.db)
        .unwrap()
        .remove_session_workspace(SessionRole::Inbox)
        .unwrap();
    inbox.supervise_twice(400);
    assert!(inbox.typed().is_empty());
}

#[test]
fn a_nudge_of_an_absence_is_claimed_once_across_supervisors() {
    let fx = fixture();
    let first = SqliteQueue::open(&fx.db).unwrap();
    let second = SqliteQueue::open(&fx.db).unwrap();
    let nudge = json!({"absent_since": 100, "attempt": 1, "action": "typed", "at": 500});
    assert!(first.claim_inbox_nudge(nudge.clone()).unwrap());
    assert!(!second.claim_inbox_nudge(nudge).unwrap());
    // Another attempt, or another absence, is a claim of its own.
    assert!(
        second
            .claim_inbox_nudge(json!({"absent_since": 100, "attempt": 2, "at": 1_100}))
            .unwrap()
    );
    assert!(
        first
            .claim_inbox_nudge(json!({"absent_since": 900, "attempt": 1, "at": 1_200}))
            .unwrap()
    );
    assert_eq!(first.latest_events_of(INBOX_NUDGED, 10).unwrap().len(), 3);
}

/// The watcher's state is recorded at each change only, whatever the
/// passes in between; `kpi` counts an ask opened while it was alive as
/// seen at once, one opened while it was absent as seen at its return, and
/// neither one opened before the first record nor one not seen yet.
#[test]
fn the_watchers_changes_are_recorded_once_and_give_how_long_an_ask_waited_to_be_seen() {
    let inbox = Inbox::new(READY, false);
    let mut queue = SqliteQueue::open(&inbox.fx.db).unwrap();
    // One ask open at a time on the task: each closed before the next.
    let mut open = open_ask(&mut queue);
    let mut reopen = |queue: &mut SqliteQueue| {
        queue.answer(open, "withdrawn").unwrap();
        queue.close_ask(open).unwrap();
        open = open_ask(queue);
    };
    // No watch ever: absent, recorded once over the passes.
    inbox.supervise(0);
    inbox.supervise(10);
    reopen(&mut queue);
    // A watch: back, once.
    inbox.watching(100);
    inbox.supervise(100);
    inbox.supervise(105);
    reopen(&mut queue);
    // Its heartbeat stale: absent again.
    inbox.supervise(400);
    inbox.supervise(410);
    reopen(&mut queue);
    let kinds = [ASK_OPENED, INBOX_WATCHER_ABSENT, INBOX_WATCHER_RETURNED];
    let conn = rusqlite::Connection::open(&inbox.fx.db).unwrap();
    let events: Vec<(i64, String)> = conn
        .prepare("SELECT id, kind FROM run_events WHERE kind IN (?1, ?2, ?3) ORDER BY id")
        .unwrap()
        .query_map(rusqlite::params![kinds[0], kinds[1], kinds[2]], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let names: Vec<&str> = events.iter().map(|(_, kind)| kind.as_str()).collect();
    assert_eq!(
        names,
        [
            ASK_OPENED,
            INBOX_WATCHER_ABSENT,
            ASK_OPENED,
            INBOX_WATCHER_RETURNED,
            ASK_OPENED,
            INBOX_WATCHER_ABSENT,
            ASK_OPENED,
        ]
    );
    let changes = inbox.nudges(INBOX_WATCHER_RETURNED);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["watching"], 1);
    assert_eq!(
        inbox.nudges(INBOX_WATCHER_ABSENT)[0]["last_seen_at"],
        Value::Null
    );

    // The times `kpi` reads, seconds before now: the ask opened while the
    // watch was away waits 60 s, the one opened while it watched none.
    for ((id, _), ago) in events.iter().zip([600, 590, 500, 440, 430, 100, 50]) {
        conn.execute(
            "UPDATE run_events SET created_at=strftime('%Y-%m-%dT%H:%M:%fZ','now',?1) WHERE id=?2",
            rusqlite::params![format!("-{ago} seconds"), id],
        )
        .unwrap();
    }
    let report = crate::common::cli::ok(&inbox.fx.db, &["kpi", "--last", "2"]);
    let waits: Vec<&Value> = report["periods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|period| &period["kpis"]["ask_seen_wait"]["all"])
        .filter(|wait| wait["n"].as_u64().unwrap_or(0) > 0)
        .collect();
    let n: u64 = waits.iter().map(|wait| wait["n"].as_u64().unwrap()).sum();
    assert_eq!(n, 2, "{report}");
    let min = waits
        .iter()
        .map(|w| w["min"].as_f64().unwrap())
        .reduce(f64::min);
    let max = waits
        .iter()
        .map(|w| w["max"].as_f64().unwrap())
        .reduce(f64::max);
    assert_eq!((min, max), (Some(0.0), Some(60.0)), "{report}");
}

#[test]
fn a_change_of_the_watcher_is_recorded_once_across_supervisors() {
    let fx = fixture();
    let first = SqliteQueue::open(&fx.db).unwrap();
    let second = SqliteQueue::open(&fx.db).unwrap();
    let absent = |at: i64| json!({"at": at, "watching": 0});
    assert!(
        first
            .record_inbox_watcher_change(EventKind::InboxWatcherAbsent, absent(100))
            .unwrap()
    );
    assert!(
        !second
            .record_inbox_watcher_change(EventKind::InboxWatcherAbsent, absent(100))
            .unwrap()
    );
    assert!(
        second
            .record_inbox_watcher_change(EventKind::InboxWatcherReturned, json!({"at": 200}))
            .unwrap()
    );
    assert!(
        !first
            .record_inbox_watcher_change(EventKind::InboxWatcherReturned, json!({"at": 210}))
            .unwrap()
    );
    assert!(
        first
            .record_inbox_watcher_change(EventKind::InboxWatcherAbsent, absent(300))
            .unwrap()
    );
    // A slow supervisor's judgment older than the latest record is not
    // written, though its state differs: returned at 250 after absent at
    // 300, nor absent at 150 after a return at 400.
    assert!(
        !second
            .record_inbox_watcher_change(EventKind::InboxWatcherReturned, json!({"at": 250}))
            .unwrap()
    );
    assert!(
        second
            .record_inbox_watcher_change(EventKind::InboxWatcherReturned, json!({"at": 400}))
            .unwrap()
    );
    assert!(
        !first
            .record_inbox_watcher_change(EventKind::InboxWatcherAbsent, absent(150))
            .unwrap()
    );
    let latest = first
        .latest_queue_event(&[INBOX_WATCHER_ABSENT, INBOX_WATCHER_RETURNED])
        .unwrap()
        .unwrap();
    assert_eq!(
        (latest.kind.as_str(), latest.payload["at"].as_i64()),
        (INBOX_WATCHER_RETURNED, Some(400))
    );
    assert_eq!(
        first
            .latest_events_of(INBOX_WATCHER_ABSENT, 10)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        first
            .latest_events_of(INBOX_WATCHER_RETURNED, 10)
            .unwrap()
            .len(),
        2
    );
    // Only the watcher's two kinds.
    assert!(
        first
            .record_inbox_watcher_change(EventKind::InboxNudged, json!({}))
            .is_err()
    );
}
