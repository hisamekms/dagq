//! `dagq hold` and `dagq release` (ADR-t1879-1) through the CLI and the
//! queue: a person and the inbox hold a task by its ID or by a run's, with
//! a reason; the row and its `task_held` event are one write; `show` and
//! `list` carry the open hold; a draft or an ended task takes none, nor
//! does a task held already; a worker and a planner are refused either
//! command; and the release records `task_released` and lets the task be
//! claimed again.

use crate::common::cli::{invoke_with, queue};
use crate::common::queue::{base, new_task};
use dagq::{
    application::TaskStore,
    domain::{ClaimOutcome, RunId, TaskAction, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

/// `dagq` with `env`; whether it succeeded and its JSON (stdout, or the
/// error's stderr as text).
fn dagq(db: &Path, env: &[(&str, &str)], args: &[&str]) -> (bool, Value) {
    let output = invoke_with(env, db, args);
    if output.status.success() {
        (true, serde_json::from_slice(&output.stdout).unwrap())
    } else {
        (
            false,
            Value::String(String::from_utf8_lossy(&output.stderr).into_owned()),
        )
    }
}

fn user(db: &Path, args: &[&str]) -> (bool, Value) {
    dagq(db, &[], args)
}

fn ready(queue: &mut SqliteQueue, title: &str) -> TaskId {
    let id = queue.add(new_task(title)).unwrap().id();
    queue.transition(id, TaskAction::BypassReview).unwrap();
    id
}

fn count(db: &Path, sql: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_person_holds_a_task_by_its_id_or_its_runs_and_releases_it() {
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let a = ready(&mut queue, "A");
    let b = ready(&mut queue, "B");

    let (ok, held) = user(
        &db,
        &[
            "hold",
            "1",
            "--reason",
            "the login expired",
            "--until",
            "@2000000000",
        ],
    );
    assert!(ok, "{held}");
    assert_eq!(held["state"], "held");
    assert_eq!(held["hold"]["task_id"], 1);
    assert_eq!(held["hold"]["reason"], "the login expired");
    assert_eq!(held["hold"]["until"], 2_000_000_000_i64);
    assert_eq!(held["hold"]["by_role"], "user");
    // The row and its event, one each, naming each other.
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM task_holds WHERE released_at IS NULL"
        ),
        1
    );
    let event: Value = serde_json::from_str(
        &Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT payload FROM run_events WHERE kind='task_held' AND task_id=1",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(event["hold_id"], held["hold"]["id"]);
    assert_eq!(event["requested"], "task");
    assert_eq!(event["state"], "held");

    // `show` and `list` carry it; the task keeps its status.
    let (_, shown) = user(&db, &["show", "1"]);
    assert_eq!(shown["task"]["status"], "ready");
    assert_eq!(shown["hold"]["reason"], "the login expired");
    assert_eq!(shown["hold"]["by_role"], "user");
    let (_, listed) = user(&db, &["list"]);
    let item = |id: i64| {
        listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(item(1)["hold"]["reason"], "the login expired");
    assert_eq!(item(1)["hold"]["state"], "held");
    assert!(item(2).get("hold").is_none());

    // Not claimed while held; a hold on it again is refused.
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("B is claimed")
    };
    assert_eq!(run.task_id(), b);
    let (ok, again) = user(&db, &["hold", "1", "--reason", "again"]);
    assert!(!ok);
    assert!(again.as_str().unwrap().contains("held already"), "{again}");

    // A run's ID holds its task.
    let (ok, by_run) = user(&db, &["hold", run.id().as_str(), "--reason", "outage"]);
    assert!(ok, "{by_run}");
    assert_eq!(by_run["hold"]["task_id"], b.as_i64());
    assert_eq!(by_run["hold"]["run_id"], run.id().as_str());
    // Its run is on its way, so the hold settles once the run waits.
    assert_eq!(by_run["state"], "settling");

    // Released: the event, no hold shown, claimed again.
    let (ok, released) = user(&db, &["release", &a.to_string()]);
    assert!(ok, "{released}");
    assert_eq!(released["continuation"], "claim");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM run_events WHERE kind='task_released'"
        ),
        1
    );
    let (_, shown) = user(&db, &["show", "1"]);
    assert!(shown.get("hold").is_none());
    let (ok, twice) = user(&db, &["release", "1"]);
    assert!(!ok);
    assert!(twice.as_str().unwrap().contains("no open hold"), "{twice}");
    let ClaimOutcome::Claimed { run: claimed } = queue.claim(&base()).unwrap() else {
        panic!("A is claimed once released")
    };
    assert_eq!(claimed.task_id(), a);
    // Not released while its run settles: the hold stays as it was.
    let (ok, settling) = user(&db, &["release", run.id().as_str()]);
    assert!(!ok);
    assert!(
        settling.as_str().unwrap().contains("hold_not_settled"),
        "{settling}"
    );
    let (_, shown) = user(&db, &["show", &b.to_string()]);
    assert_eq!(shown["hold"]["reason"], "outage");
    // A task that ended while its hold settled has nothing left to settle:
    // the release closes the hold.
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status='completed' WHERE id=?1",
            [b.as_i64()],
        )
        .unwrap();
    let (ok, released) = user(&db, &["release", run.id().as_str()]);
    assert!(ok, "{released}");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM task_holds WHERE released_at IS NULL"
        ),
        0
    );
}

#[test]
fn a_draft_an_ended_task_and_a_blank_reason_take_no_hold() {
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let draft = queue.add(new_task("draft")).unwrap().id();
    let canceled = ready(&mut queue, "canceled");
    queue.transition(canceled, TaskAction::Cancel).unwrap();
    let submitted = queue.add(new_task("submitted")).unwrap().id();
    queue.transition(submitted, TaskAction::Submit).unwrap();
    for (task, status) in [(draft, "draft"), (canceled, "canceled")] {
        let (ok, refused) = user(&db, &["hold", &task.to_string(), "--reason", "why"]);
        assert!(!ok);
        assert!(refused.as_str().unwrap().contains(status), "{refused}");
    }
    let (ok, blank) = user(&db, &["hold", &submitted.to_string(), "--reason", "  "]);
    assert!(!ok);
    assert!(blank.as_str().unwrap().contains("reason"), "{blank}");
    let (ok, held) = user(&db, &["hold", &submitted.to_string(), "--reason", "why"]);
    assert!(ok, "{held}");
    let (ok, unknown) = user(&db, &["hold", "no-such-run", "--reason", "why"]);
    assert!(!ok);
    assert!(
        unknown.as_str().unwrap().contains("does not exist"),
        "{unknown}"
    );
    assert_eq!(count(&db, "SELECT count(*) FROM task_holds"), 1);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM run_events WHERE kind='task_held'"
        ),
        1
    );
}

#[test]
fn only_a_person_and_the_inbox_hold_or_release() {
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    ready(&mut queue, "A");
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    let run_id: &RunId = run.id();
    let worker = [
        ("DAGQ_ROLE", "worker"),
        ("DAGQ_RUN_ID", run_id.as_str()),
        ("DAGQ_TASK_ID", "1"),
        ("DAGQ_ACTOR_ID", run_id.as_str()),
    ];
    // A planner of the runtime's, with its token.
    let planner = [("DAGQ_ROLE", "planner")];
    for env in [&worker[..], &planner[..]] {
        for args in [
            &["hold", "1", "--reason", "why"][..],
            &["hold", run_id.as_str(), "--reason", "why"][..],
            &["release", "1"][..],
        ] {
            let (ok, refused) = dagq(&db, env, args);
            assert!(!ok, "{env:?} {args:?}");
            assert!(refused.as_str().unwrap().contains("task.hold"), "{refused}");
        }
    }
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM run_events WHERE kind='authorization_denied'
             AND json_extract(payload,'$.capability')='task.hold'"
        ),
        6
    );
    assert_eq!(count(&db, "SELECT count(*) FROM task_holds"), 0);
    let inbox = [("DAGQ_ROLE", "inbox"), ("DAGQ_ACTOR_ID", "inbox")];
    let (ok, held) = dagq(&db, &inbox, &["hold", "1", "--reason", "outage"]);
    assert!(ok, "{held}");
    assert_eq!(held["hold"]["by_role"], "inbox");
    let b = ready(&mut queue, "B");
    let (ok, held) = dagq(&db, &inbox, &["hold", &b.to_string(), "--reason", "outage"]);
    assert!(ok, "{held}");
    let (ok, released) = dagq(&db, &inbox, &["release", &b.to_string()]);
    assert!(ok, "{released}");
}

/// `status` lists each open hold in `held`, with its reason, `until`,
/// when it was held and when its task stopped moving under it, and its
/// task's run now; a hold past its `until` is attention there for the
/// inbox. The queue tells it once as `task_hold_reminded`, which `events`
/// reads as attention, and not again the same day; a hold not due is
/// told nothing, and a released one leaves `held`.
#[test]
fn a_held_task_is_in_status_and_one_past_its_until_is_told_once() {
    let (_dir, db) = queue();
    let mut queue = SqliteQueue::open(&db).unwrap();
    ready(&mut queue, "A");
    ready(&mut queue, "B");
    let ClaimOutcome::Claimed { run } = queue.claim(&base()).unwrap() else {
        panic!("claim")
    };
    assert_eq!(run.task_id(), TaskId::new(1));
    let (ok, held) = user(
        &db,
        &[
            "hold",
            run.id().as_str(),
            "--reason",
            "the login expired",
            "--until",
            "@1000000000",
        ],
    );
    assert!(ok, "{held}");
    let (ok, held) = user(&db, &["hold", "2", "--reason", "the vendor is down"]);
    assert!(ok, "{held}");

    let (ok, status) = user(&db, &["status", "--role", "inbox"]);
    assert!(ok, "{status}");
    let listed = status["held"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "{status}");
    let (running, ready_one) = (&listed[0], &listed[1]);
    assert_eq!(running["task_id"], 1);
    assert_eq!(running["reason"], "the login expired");
    assert_eq!(running["until"], 1_000_000_000_i64);
    assert_eq!(running["by_role"], "user");
    assert!(running["held_at"].is_i64(), "{running}");
    // The run's turn has not ended: it has not stopped moving yet.
    assert_eq!(running["state"], "settling");
    assert_eq!(running["settled_at"], Value::Null);
    assert_eq!(running["run_id"], run.id().as_str());
    assert_eq!(running["requested_run_id"], run.id().as_str());
    assert_eq!(running["run_status"], "claimed");
    assert_eq!(running["overdue"], "until_passed");
    assert_eq!(ready_one["task_id"], 2);
    assert_eq!(ready_one["state"], "held");
    assert_eq!(ready_one["settled_at"], ready_one["held_at"]);
    assert_eq!(ready_one["run_id"], Value::Null);
    assert_eq!(ready_one["overdue"], Value::Null);
    let told: Vec<&Value> = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "task_hold_reminded")
        .collect();
    assert_eq!(told.len(), 1, "{status}");
    assert_eq!(told[0]["task_id"], 1);
    assert_eq!(told[0]["last_error"], "the login expired");
    assert_eq!(told[0]["next"], "release or keep the hold of task 1");
    // A planner sees the holds, not the attention.
    let (ok, planner) = user(&db, &["status", "--role", "planner"]);
    assert!(ok, "{planner}");
    assert_eq!(planner["held"].as_array().unwrap().len(), 2);
    assert!(planner["attention"].as_array().unwrap().is_empty());

    let reminded = queue.remind_task_holds().unwrap();
    assert_eq!(reminded.len(), 1);
    assert_eq!(reminded[0].0.task_id, TaskId::new(1));
    assert!(queue.remind_task_holds().unwrap().is_empty());
    let (ok, events) = user(&db, &["events", "--after", "0"]);
    assert!(ok, "{events}");
    let told: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "task_hold_reminded")
        .collect();
    assert_eq!(told.len(), 1, "{events}");
    assert_eq!(told[0]["task_id"], 1);
    assert_eq!(told[0]["reason"], "the login expired");
    assert_eq!(told[0]["next"], "release or keep the hold of task 1");

    let (ok, released) = user(&db, &["release", "2"]);
    assert!(ok, "{released}");
    let (_, status) = user(&db, &["status"]);
    let listed = status["held"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["task_id"], 1);
}
