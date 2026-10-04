//! `status` and `doctor` list the runs a supervisor holds and the latest
//! unfinished run of each task, with their phase and slot (goal 98), from
//! a fixture queue whose runs, leases and events are written directly.
use crate::common;
use dagq::domain::{ClaimOutcome, CommitSha, LeaseToken};
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

use common::cli::*;

/// Claim the next ready task of the queue as `t`; its run's ID.
fn claim(db: &Path, name: &str) -> String {
    let added = ok(db, &["add", name, "--goal", "1"]);
    let id = added["id"].to_string();
    ok(db, &["ready", &id, "--bypass-review"]);
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = SqliteQueue::open(db)
        .unwrap()
        .claim_for_supervisor(&base, &LeaseToken::new("t"))
        .unwrap()
    else {
        panic!("nothing to claim");
    };
    run.id().as_str().to_owned()
}

/// Set `run`'s status and its task's; a landed run gets its commit.
fn set(conn: &Connection, run: &str, status: &str, task: &str) {
    conn.execute(
        "UPDATE task_runs SET status=?2,
         result_commit=CASE WHEN ?2='integrated' THEN base_commit ELSE result_commit END
         WHERE id=?1",
        params![run, status],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status=?2 WHERE id=(SELECT task_id FROM task_runs WHERE id=?1)",
        params![run, task],
    )
    .unwrap();
}

/// Record `kind` on `run` at 2026-10-04 `HH:MM:SS` UTC.
fn event(conn: &Connection, run: &str, kind: &str, payload: Value, at: &str) {
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload,created_at)
         SELECT task_id,id,?2,?3,?4 FROM task_runs WHERE id=?1",
        params![
            run,
            kind,
            payload.to_string(),
            format!("2026-10-04T{at}.000Z")
        ],
    )
    .unwrap();
}

fn lease(conn: &Connection, run: &str, token: &str, pid: u32, age_secs: i64) {
    conn.execute(
        "INSERT INTO run_leases(run_id,token,pid,heartbeat_at) VALUES (?1,?2,?3,unixepoch()-?4)",
        params![run, token, pid, age_secs],
    )
    .unwrap();
}

/// The unix second of 2026-10-04 `HH:MM:SS` UTC.
fn at(hms: &str) -> i64 {
    let mut parts = hms.split(':').map(|p| p.parse::<i64>().unwrap());
    let (h, m, s) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    1_791_072_000 + h * 3600 + m * 60 + s
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// The runs of the fixture by the role each plays in it.
struct Runs(HashMap<&'static str, String>);

impl Runs {
    fn id(&self, name: &str) -> &str {
        &self.0[name]
    }
}

/// A live supervisor `live` (parallel 3) holding a running run, a run
/// whose revise waits for a person's answer, a resume whose wait ended
/// (returning), a run whose e2e waits to be tried again, a run asked to
/// `/exit` and a failed run under recovery; a dead supervisor `dead`
/// that died during a review; an `integrate` process `landing` holding
/// a landing and a landed run it cleans up; and runs nobody leases: a
/// run queued to land by a `land` answer, a `needs_session` run, the
/// latest attempt of a retried task (its earlier attempt failed), a
/// finished run and a canceled task's interrupted run.
fn fixture(db: &Path) -> Runs {
    common::template::queue(db);
    ok(db, &["goal", "add", "listed"]);
    let mut runs = HashMap::new();
    for name in [
        "running",
        "revise",
        "resume",
        "e2e_retry",
        "exit",
        "recovery",
        "dead_review",
        "integrating",
        "cleanup",
        "queued",
        "parked",
        "retried_old",
    ] {
        runs.insert(name, claim(db, name));
    }
    let conn = Connection::open(db).unwrap();
    // The task retried: its first attempt failed, the second awaits
    // integration with nobody holding it.
    set(&conn, &runs["retried_old"], "failed", "ready");
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    let ClaimOutcome::Claimed { run } = SqliteQueue::open(db)
        .unwrap()
        .claim_for_supervisor(&base, &LeaseToken::new("t"))
        .unwrap()
    else {
        panic!("the retry was not claimed");
    };
    runs.insert("retried", run.id().as_str().to_owned());
    for name in ["finished", "canceled"] {
        runs.insert(name, claim(db, name));
    }
    let id = |name: &str| runs[name].clone();
    conn.execute("DELETE FROM run_leases", []).unwrap();
    conn.execute("DELETE FROM run_events WHERE run_id IS NOT NULL", [])
        .unwrap();
    let me = std::process::id();
    let dead = dead_pid();
    SqliteQueue::open(db)
        .unwrap()
        .register_supervisor(&LeaseToken::new("live"), me, 3, "0.0.1")
        .unwrap();
    conn.execute(
        "INSERT INTO supervisors(token,pid,parallel,binary_version,started_at,heartbeat_at)
         VALUES ('dead',?1,2,'0.0.1',unixepoch()-7200,unixepoch()-3600)",
        [dead],
    )
    .unwrap();

    set(&conn, &id("running"), "running", "in_progress");
    event(&conn, &id("running"), "run_claimed", json!({}), "00:00:00");
    event(
        &conn,
        &id("running"),
        "agent_started",
        json!({}),
        "00:01:00",
    );
    lease(&conn, &id("running"), "live", me, 0);

    // Reviewed once, sent back, and its worker asked a question while it
    // revises.
    let revise = id("revise");
    set(&conn, &revise, "awaiting_integration", "in_progress");
    for (kind, payload, time) in [
        ("validation_finished", json!({"accepted": true}), "00:02:00"),
        ("review_started", json!({}), "00:02:10"),
        ("review_finished", json!({"verdict": "revise"}), "00:03:00"),
        ("revise_requested", json!({"attempt": 1}), "00:03:05"),
        (
            "run_waiting_started",
            json!({"phase": "revise", "status": "awaiting_integration", "ask_id": 7, "ask_kind": "worker_question"}),
            "00:04:00",
        ),
    ] {
        event(&conn, &revise, kind, payload, time);
    }
    lease(&conn, &revise, "live", me, 0);

    let resume = id("resume");
    set(&conn, &resume, "needs_session", "in_progress");
    for (kind, payload, time) in [
        ("resume_started", json!({"attempt": 1}), "00:05:00"),
        (
            "run_waiting_started",
            json!({"phase": "resume", "status": "needs_session", "ask_id": 8, "ask_kind": "worker_question"}),
            "00:05:30",
        ),
        (
            "run_waiting_ended",
            json!({"cause": "answered"}),
            "00:06:00",
        ),
    ] {
        event(&conn, &resume, kind, payload, time);
    }
    lease(&conn, &resume, "live", me, 0);

    let e2e = id("e2e_retry");
    set(&conn, &e2e, "awaiting_integration", "in_progress");
    for (kind, payload, time) in [
        ("review_started", json!({}), "00:07:00"),
        ("review_finished", json!({"verdict": "pass"}), "00:07:30"),
        ("landing_queued", json!({"via": "exit"}), "00:07:40"),
        ("run_e2e_started", json!({"attempt": 1}), "00:08:00"),
        (
            "run_e2e_finished",
            json!({"outcome": "unavailable"}),
            "00:08:20",
        ),
    ] {
        event(&conn, &e2e, kind, payload, time);
    }
    lease(&conn, &e2e, "live", me, 0);

    let exit = id("exit");
    set(&conn, &exit, "awaiting_integration", "in_progress");
    event(
        &conn,
        &exit,
        "review_finished",
        json!({"verdict": "pass"}),
        "00:09:00",
    );
    event(&conn, &exit, "exit_requested", json!({}), "00:09:10");
    lease(&conn, &exit, "live", me, 0);

    let recovery = id("recovery");
    set(&conn, &recovery, "failed", "in_progress");
    event(&conn, &recovery, "triage_started", json!({}), "00:10:00");
    lease(&conn, &recovery, "live", me, 0);

    // Its supervisor died while it was reviewed.
    let dead_review = id("dead_review");
    set(&conn, &dead_review, "awaiting_integration", "in_progress");
    event(&conn, &dead_review, "review_started", json!({}), "00:11:00");
    lease(&conn, &dead_review, "dead", dead, 3600);

    let integrating = id("integrating");
    set(&conn, &integrating, "integrating", "in_progress");
    event(
        &conn,
        &integrating,
        "integration_started",
        json!({}),
        "00:12:00",
    );
    lease(&conn, &integrating, "landing", me, 0);
    // Landed; the `integrate` process still holds it while it cleans up.
    set(&conn, &id("cleanup"), "integrated", "completed");
    lease(&conn, &id("cleanup"), "landing", me, 0);

    // A `land` answer queued it; it waits for the supervisor to land it.
    let queued = id("queued");
    set(&conn, &queued, "awaiting_integration", "in_progress");
    event(
        &conn,
        &queued,
        "integration_approved",
        json!({"ask_id": 3}),
        "00:13:00",
    );
    event(
        &conn,
        &queued,
        "landing_queued",
        json!({"via": "approve"}),
        "00:13:01",
    );

    set(&conn, &id("parked"), "needs_session", "in_progress");
    event(
        &conn,
        &id("parked"),
        "run_e2e_failed",
        json!({}),
        "00:14:00",
    );

    set(&conn, &id("retried"), "awaiting_integration", "in_progress");
    set(&conn, &id("finished"), "integrated", "completed");
    set(&conn, &id("canceled"), "interrupted", "canceled");
    Runs(runs)
}

const LISTED: [&str; 12] = [
    "running",
    "revise",
    "resume",
    "e2e_retry",
    "exit",
    "recovery",
    "dead_review",
    "integrating",
    "cleanup",
    "queued",
    "parked",
    "retried",
];

/// `runs` keyed by run ID; each listed once.
fn by_id(runs: &Value) -> HashMap<String, Value> {
    let runs = runs.as_array().unwrap();
    let keyed: HashMap<String, Value> = runs
        .iter()
        .map(|run| (run["run_id"].as_str().unwrap().to_owned(), run.clone()))
        .collect();
    assert_eq!(keyed.len(), runs.len(), "a run is listed twice: {runs:?}");
    keyed
}

/// The lease holders' runs, the leased runs and the unfinished latest
/// runs are listed once each with their phase, since when and their slot;
/// history is not (acceptance 1 and 2), and each supervisor's `slots` and
/// `waiting` are the listed runs' slots (acceptance 3).
#[test]
fn status_lists_the_held_and_the_latest_runs_with_their_phase_and_slot() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let runs = fixture(&db);
    let status = ok(&db, &["status"]);
    let listed = by_id(&status["runs"]);
    let mut expected: Vec<&str> = LISTED.iter().map(|name| runs.id(name)).collect();
    expected.sort_unstable();
    let mut got: Vec<&str> = listed.keys().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(got, expected, "{status}");
    for name in ["retried_old", "finished", "canceled"] {
        assert!(!listed.contains_key(runs.id(name)), "{name}: {status}");
    }

    let now = status["checked_at"].as_i64().unwrap();
    let progress = |name: &str| listed[runs.id(name)]["progress"].clone();
    let since = |phase: &str, slot: Value, time: &str| json!({"phase": phase, "since": at(time), "elapsed_secs": now - at(time), "slot": slot});
    let nothing =
        |slot: Value| json!({"phase": null, "since": null, "elapsed_secs": null, "slot": slot});
    for (name, expected) in [
        ("running", since("session", json!("used"), "00:01:00")),
        ("revise", since("revise", json!("waiting"), "00:03:05")),
        ("resume", since("resume", json!("returning"), "00:05:00")),
        (
            "e2e_retry",
            since("e2e_retry_wait", json!("used"), "00:08:20"),
        ),
        ("exit", since("exit", json!("used"), "00:09:10")),
        ("recovery", since("recovery", json!("used"), "00:10:00")),
        // Its supervisor died: it holds the slot, but no review goes on.
        ("dead_review", nothing(json!("used"))),
        (
            "integrating",
            since("integrating", json!("used"), "00:12:00"),
        ),
        ("cleanup", nothing(json!("used"))),
        ("queued", since("landing_queue", Value::Null, "00:13:01")),
        ("parked", nothing(Value::Null)),
        ("retried", nothing(Value::Null)),
    ] {
        assert_eq!(progress(name), expected, "{name}");
    }
    for name in LISTED {
        let run = &listed[runs.id(name)];
        let leased = !run["lease"].is_null();
        assert_eq!(leased, !run["progress"]["slot"].is_null(), "{name}");
    }

    // Each lease holder's runs are listed, and a registered one's slots
    // and waits are its runs' slots.
    let supervisors = status["supervisors"].as_array().unwrap();
    let mut held = 0;
    for supervisor in supervisors {
        let mut slots: HashMap<String, i64> = HashMap::new();
        for run_id in supervisor["run_ids"].as_array().unwrap() {
            let run = &listed[run_id.as_str().unwrap()];
            assert_eq!(run["lease"]["pid"], supervisor["pid"], "{run}");
            *slots
                .entry(run["progress"]["slot"].as_str().unwrap().to_owned())
                .or_default() += 1;
            held += 1;
        }
        if supervisor["registered"] == true {
            let count = |slot: &str| slots.get(slot).copied().unwrap_or(0);
            assert_eq!(supervisor["slots"]["used"], count("used"), "{supervisor}");
            assert_eq!(
                supervisor["slots"]["landing_queue"],
                count("landing_queue"),
                "{supervisor}"
            );
            assert_eq!(
                supervisor["waiting"]["count"],
                count("waiting") + count("returning"),
                "{supervisor}"
            );
            assert_eq!(supervisor["waiting"]["returning"], count("returning"));
        }
    }
    assert_eq!(held, 9, "every leased run is held by one listed holder");
    let live = supervisors
        .iter()
        .find(|supervisor| supervisor["registered"] == true && supervisor["alive"] == true)
        .unwrap();
    assert_eq!(live["run_ids"].as_array().unwrap().len(), 6);
    // Runs out of their slots: the six runs are not the slots in use.
    assert_eq!(live["slots"]["used"], 4);
    assert_eq!(live["waiting"]["count"], 2);
    let waiting: Vec<&str> = status["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .map(|wait| wait["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(waiting.len(), 2);
    assert!(waiting.contains(&runs.id("revise")) && waiting.contains(&runs.id("resume")));

    // The attention and the role's narrowing are as before: the inbox's,
    // none of it the planner's.
    assert!(
        ok_as("planner", &db, &["status", "--role", "planner"])["attention"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// `doctor` lists the same runs in its default and full output, and only
/// the run `recover` takes is recoverable: the review whose supervisor
/// died. A `needs_session` run, an unleased run awaiting integration, a
/// failed or landed run and a run whose holder lives are not (acceptance
/// 4); `recover` agrees with each.
#[test]
fn doctor_lists_the_same_runs_and_recoverable_follows_recover() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let runs = fixture(&db);
    let status = by_id(&ok(&db, &["status"])["runs"]);
    let summary = ok(&db, &["doctor"]);
    let full = ok(&db, &["doctor", "--full"]);
    let summary = by_id(&summary["runs"]);
    let full = by_id(&full["runs"]);
    let mut ids: Vec<&String> = status.keys().collect();
    ids.sort_unstable();
    let mut summary_ids: Vec<&String> = summary.keys().collect();
    summary_ids.sort_unstable();
    let mut full_ids: Vec<&String> = full.keys().collect();
    full_ids.sort_unstable();
    assert_eq!(summary_ids, ids);
    assert_eq!(full_ids, ids);
    for (id, run) in &status {
        for doctor in [&summary[id], &full[id]] {
            assert_eq!(doctor["task_id"], run["task_id"], "{id}");
            assert_eq!(doctor["status"], run["status"], "{id}");
            assert_eq!(
                doctor["progress"]["phase"], run["progress"]["phase"],
                "{id}"
            );
            assert_eq!(doctor["progress"]["slot"], run["progress"]["slot"], "{id}");
        }
        assert_eq!(summary[id]["lease_pid"], run["lease"]["pid"], "{id}");
        assert_eq!(full[id]["lease"]["pid"], run["lease"]["pid"], "{id}");
        assert_eq!(
            summary[id]["blocker_count"],
            full[id]["blockers"].as_array().unwrap().len(),
            "{id}"
        );
    }
    let recoverable = |name: &str| summary[runs.id(name)]["recoverable"].clone();
    assert_eq!(recoverable("dead_review"), true);
    for name in LISTED.iter().filter(|name| **name != "dead_review") {
        assert_eq!(recoverable(name), false, "{name}");
        let refused = invoke(&db, &["recover", runs.id(name)]);
        assert!(!refused.status.success(), "{name} was recovered");
    }
    for name in ["parked", "queued", "retried", "recovery", "cleanup"] {
        let blockers = full[runs.id(name)]["blockers"].to_string();
        assert!(
            blockers.contains("recover takes only"),
            "{name}: {blockers}"
        );
    }
    let recovered = ok(&db, &["recover", runs.id("dead_review")]);
    assert_eq!(recovered["outcome"], "recovered", "{recovered}");
}
