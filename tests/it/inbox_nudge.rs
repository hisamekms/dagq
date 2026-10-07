//! The runtime's backstop for an inbox without a watcher (ADR-t1433-5
//! decision 1 (3)): while no watch watches and an ask waited past the
//! threshold, the supervisor records `inbox_nudged` once for the absence
//! and, with `[push]` in `host.toml`, sends one message through it; it
//! reads no screen and types nothing (decision 2), and these tests run it
//! on the backend of background wrappers alone, with no cmux fake
//! (ADR-t1433-1 decision 3). The watcher's changes are recorded as
//! `inbox_watcher_absent` / `inbox_watcher_returned`, and `kpi` derives
//! from them how long an ask waited to be seen (`ask_seen_wait`, task
//! 1021).

use crate::plan_review::{Fixture, StubReviewer, fixture, options};
use dagq::{
    application::{Clock, Generators, TaskStore, inbox_watcher::WatcherRecord},
    domain::{
        AskKind, AskReason, EventKind, NewAsk, SessionRole, TaskAction, TaskId,
        event_kind::{ASK_OPENED, INBOX_NUDGED, INBOX_WATCHER_ABSENT, INBOX_WATCHER_RETURNED},
    },
    infrastructure::{adapters::BackgroundSessions, sqlite::SqliteQueue},
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
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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

struct Inbox {
    fx: Fixture,
    clock: Arc<Ahead>,
    /// Where the `[push]` command keeps what it read, when there is one.
    pushed: Option<PathBuf>,
}

impl Inbox {
    /// A queue whose inbox workspace is recorded, with one open ask when
    /// `ask` and, with `push`, a `[push]` in `host.toml`. Its one task is
    /// canceled: the supervisor claims nothing.
    fn new(ask: bool, push: bool) -> Self {
        let fx = fixture();
        let mut queue = SqliteQueue::open(&fx.db).unwrap();
        queue
            .transition(TaskId::new(1), TaskAction::Cancel)
            .unwrap();
        queue
            .register_session_workspace(SessionRole::Inbox, "INBOX")
            .unwrap();
        if ask {
            open_ask(&mut queue);
        }
        let queue_dir = fx
            .db
            .canonicalize()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let pushed = push.then(|| {
            let pushed = queue_dir.join("pushed");
            fs::create_dir(&pushed).unwrap();
            let script = queue_dir.join("push.sh");
            crate::common::template::script_env(
                &script,
                "#!/bin/sh\nn=$(ls \"$STUB_INBOX\" | wc -l | tr -d ' ')\ncat > \"$STUB_INBOX/$n.json\"\nprintf '%s\\n' \"$DAGQ_PUSH_KIND\" > \"$STUB_INBOX/$n.env\"\n",
                &[("STUB_INBOX", pushed.to_str().unwrap())],
            );
            fs::write(
                queue_dir.join("host.toml"),
                format!(
                    "[push]\ncommand = [\"{}\"]\ntimeout_secs = 10\ndaily = false\nbreach = false\n",
                    script.display()
                ),
            )
            .unwrap();
            pushed
        });
        Self {
            fx,
            clock: Arc::default(),
            pushed,
        }
    }

    /// One `supervise --once` with the clock `at` seconds ahead, on the
    /// backend of background wrappers alone: no cmux, nor a fake of it.
    fn supervise(&self, at: i64) {
        self.clock.0.store(at, Ordering::SeqCst);
        let queue_dir = self
            .fx
            .db
            .canonicalize()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let options = SuperviseOptions {
            generators: Generators {
                clock: self.clock.clone(),
                ids: dagq::infrastructure::clock::system().ids,
            },
            report_daily: self.pushed.is_some(),
            host_config: Some(queue_dir.join("no host-wide file.toml")),
            push_retry: [Duration::from_millis(10), Duration::from_millis(10)],
            ..options(1, Duration::from_secs(3600))
        };
        runtime::supervise_with_reviewer(
            &self.fx.db,
            &self.fx.repo,
            &BackgroundSessions,
            &self.fx.claude,
            &StubReviewer::new(&[]),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap();
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

    /// What the `[push]` command read, each message with its kind.
    fn pushed(&self) -> Vec<(Value, String)> {
        let dir = self.pushed.as_ref().unwrap();
        let mut messages = Vec::new();
        for n in 0.. {
            let Ok(json) = fs::read(dir.join(format!("{}.json", 2 * n))) else {
                break;
            };
            let kind = fs::read_to_string(dir.join(format!("{}.env", 2 * n))).unwrap();
            messages.push((
                serde_json::from_slice(&json).unwrap(),
                kind.trim().to_owned(),
            ));
        }
        messages
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

/// One open ask for the inbox, on no task.
fn open_ask(queue: &mut SqliteQueue) -> dagq::domain::AskId {
    queue
        .ask(NewAsk {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            kind: AskKind::Blocked,
            task_id: None,
            run_id: None,
            question: "which way?".into(),
            options: vec![],
            asked_by: "planner".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
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

/// Without `[push]`, the absence is recorded once, as the event alone,
/// past the threshold, and never again in the same absence.
#[test]
fn an_inbox_without_a_watch_is_recorded_once_and_nothing_is_typed() {
    let inbox = Inbox::new(true, false);
    // The ask is new: nothing yet.
    inbox.supervise(0);
    assert!(inbox.nudges(INBOX_NUDGED).is_empty());
    inbox.supervise(400);
    let nudges = inbox.nudges(INBOX_NUDGED);
    assert_eq!(nudges.len(), 1, "{nudges:?}");
    assert_eq!(nudges[0]["attempt"], 1);
    assert_eq!(nudges[0]["action"], "recorded");
    assert_eq!(nudges[0]["absent_since"], 0);
    assert_eq!(nudges[0]["open_asks"], 1);
    assert_eq!(nudges[0]["waiting_asks"], 1);
    assert_eq!(nudges[0]["push_error"], Value::Null);
    assert_eq!(
        nudges[0].get("workspace_id"),
        None,
        "no workspace is aimed at"
    );
    inbox.supervise(1_100);
    inbox.supervise(5_000);
    assert_eq!(inbox.nudges(INBOX_NUDGED).len(), 1);
}

/// With `[push]`, the absence is recorded and sent once through it, with
/// the counts and nothing of the asks' content.
#[test]
fn an_inbox_without_a_watch_is_told_once_through_push() {
    let inbox = Inbox::new(true, true);
    inbox.supervise(0);
    assert!(inbox.pushed().is_empty());
    inbox.supervise(400);
    let nudges = inbox.nudges(INBOX_NUDGED);
    assert_eq!(nudges.len(), 1, "{nudges:?}");
    assert_eq!(nudges[0]["action"], "pushed");
    let pushed = inbox.pushed();
    assert_eq!(pushed.len(), 1, "{pushed:?}");
    let (message, kind) = &pushed[0];
    assert_eq!(kind, "inbox_watch");
    assert_eq!(message["kind"], "inbox_watch");
    assert_eq!(message["open_asks"], 1);
    assert_eq!(message["waiting_asks"], 1);
    assert!(!message.to_string().contains("which way?"), "{message}");
    assert_eq!(
        inbox.nudges("kpi_push_sent"),
        [json!({"push_kind": "inbox_watch", "period": "absent since 0", "attempt": 1})]
    );
    inbox.supervise(2_000);
    assert_eq!(inbox.pushed().len(), 1);
    assert_eq!(inbox.nudges(INBOX_NUDGED).len(), 1);
}

/// The supervisor reads a live watch's record under the queue's directory
/// and records nothing for an ask that waited past the threshold. The
/// other cases of the
/// judgment (no ask, an ask not waited long enough) are the unit test
/// `supervise::inbox_nudge::tests::no_nudge_while_a_watcher_is_alive_or_no_ask_waited_long_enough`.
#[test]
fn nothing_is_recorded_while_a_watch_watches() {
    let inbox = Inbox::new(true, false);
    inbox.watching(400);
    inbox.supervise(400);
    assert!(inbox.nudges(INBOX_NUDGED).is_empty());
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
    let inbox = Inbox::new(false, false);
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
