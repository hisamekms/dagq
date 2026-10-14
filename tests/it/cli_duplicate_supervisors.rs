//! `doctor` and `status` warn of two supervisors serving one queue at once
//! (`duplicate_supervisors`), live registrations that are not a handoff's
//! pair or two of the queue's LaunchAgents loaded, and stay quiet about a
//! handoff's pair.
use crate::common;
use crate::common::lifecycle::{FakeLaunchd, fixture};
use dagq::application::LaunchAgent;
use dagq::domain::{LeaseToken, SupervisorMode};
use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::{Value, json};
use std::path::Path;

use common::cli::*;

/// Register `token` under `pid`, started `ago` seconds back, in `mode`.
fn register(db: &Path, token: &str, pid: u32, ago: i64, mode: Option<SupervisorMode>) {
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .register_supervisor(&LeaseToken::new(token), pid, 2, "0.0.1")
        .unwrap();
    if let Some(mode) = mode {
        queue
            .set_supervisor_mode(&LeaseToken::new(token), mode, None)
            .unwrap();
    }
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE supervisors SET started_at=unixepoch()-?2 WHERE token=?1",
            rusqlite::params![token, ago],
        )
        .unwrap();
}

fn duplicate_attention(status: &Value) -> Vec<&Value> {
    status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["kind"] == "supervisor_duplicate")
        .collect()
}

#[test]
fn doctor_and_status_warn_of_two_live_supervisors_with_their_pids_tokens_and_modes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    // Two live processes: this test and the process that runs it.
    let (me, parent) = (std::process::id(), std::os::unix::process::parent_id());
    register(&db, "launchd", me, 60, Some(SupervisorMode::Launchd));
    register(&db, "by-hand", parent, 30, None);

    let doctor = ok(&db, &["doctor"]);
    let found = &doctor["duplicate_supervisors"];
    assert_eq!(found["severity"], "warning", "{doctor:#}");
    let seen: Vec<Value> = found["supervisors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| json!([s["token"], s["pid"], s["mode"]]))
        .collect();
    assert_eq!(
        seen,
        [
            json!(["launchd", me, "launchd"]),
            json!(["by-hand", parent, null])
        ]
    );
    let fix = found["fix"].to_string();
    assert!(fix.contains("dagq down --wait"), "{fix}");

    let status = ok(&db, &["status"]);
    assert_eq!(status["duplicate_supervisors"], *found);
    let attention = duplicate_attention(&status);
    assert_eq!(attention.len(), 1, "{status:#}");
    assert_eq!(attention[0]["next"], "restart supervisor");
    let text = attention[0]["last_error"].as_str().unwrap();
    assert!(text.contains(&format!("pids {me}, {parent}")), "{text}");
}

#[test]
fn a_handoff_s_pair_of_registrations_is_not_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    ok(&db, &["init"]);
    // The supervisor that took the handoff, and the row its exec made
    // again under the same pid.
    let me = std::process::id();
    register(&db, "before", me, 60, Some(SupervisorMode::Launchd));
    register(&db, "after", me, 5, Some(SupervisorMode::Launchd));

    assert_eq!(ok(&db, &["doctor"])["duplicate_supervisors"], Value::Null);
    let status = ok(&db, &["status"]);
    assert_eq!(status["duplicate_supervisors"], Value::Null);
    assert!(duplicate_attention(&status).is_empty(), "{status:#}");
}

#[test]
fn doctor_names_the_labels_of_two_loaded_agents_of_the_queue() {
    let fixture = fixture();
    let db = fixture.location.db.clone();
    let launchd = FakeLaunchd::new(&db);
    let me = std::process::id();
    register(&db, "own", me, 60, Some(SupervisorMode::Launchd));
    launchd.define_other(&fixture, &fixture.location.label, &db, true, Some(me));
    launchd.define_other(&fixture, "com.dagq.6a8d142711718fbe", &db, true, None);
    // Another queue's agent is not the queue's.
    let other = fixture.location.queue_dir.join("other.db");
    launchd.define_other(&fixture, "com.dagq.other", &other, true, Some(7));

    let agents = dagq::compose::queue_agents(&fixture.location, &launchd).unwrap();
    let doctor = dagq::compose::OneShot::system()
        .doctor_with_agents(&db, false, None, || agents.clone())
        .unwrap();
    let found = &doctor["duplicate_supervisors"];
    assert_eq!(found["severity"], "warning", "{doctor:#}");
    let mut labels: Vec<&str> = found["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["label"].as_str().unwrap())
        .collect();
    labels.sort();
    let mut expected = vec!["com.dagq.6a8d142711718fbe", fixture.location.label.as_str()];
    expected.sort();
    assert_eq!(labels, expected);
    let fix = found["fix"].to_string();
    assert!(
        fix.contains("launchctl bootout \\\"gui/$(id -u)/\\\"'com.dagq.6a8d142711718fbe'"),
        "{fix}"
    );

    // One of them unloaded: nothing to warn of.
    launchd
        .uninstall(
            "com.dagq.6a8d142711718fbe",
            &fixture
                .location
                .launch_agent
                .with_file_name("com.dagq.6a8d142711718fbe.plist"),
        )
        .unwrap();
    let agents = dagq::compose::queue_agents(&fixture.location, &launchd).unwrap();
    let doctor = dagq::compose::OneShot::system()
        .doctor_with_agents(&db, false, None, || agents.clone())
        .unwrap();
    assert_eq!(doctor["duplicate_supervisors"], Value::Null, "{doctor:#}");
}

#[test]
fn status_warns_the_inbox_of_two_loaded_agents_of_the_queue() {
    let fixture = fixture();
    let db = fixture.location.db.clone();
    let launchd = FakeLaunchd::new(&db);
    let me = std::process::id();
    register(&db, "own", me, 60, Some(SupervisorMode::Launchd));
    launchd.define_other(&fixture, &fixture.location.label, &db, true, Some(me));
    launchd.define_other(&fixture, "com.dagq.6a8d142711718fbe", &db, true, None);

    let agents = dagq::compose::queue_agents(&fixture.location, &launchd).unwrap();
    let queue = SqliteQueue::open_read_only(&db).unwrap();
    let status_as = |role| {
        dagq::compose::OneShot::system()
            .status_with_agents(&db, &queue, role, || agents.clone())
            .unwrap()
    };
    let status = status_as(None);
    let found = &status["duplicate_supervisors"];
    assert_eq!(found["severity"], "warning", "{status:#}");
    let mut labels: Vec<&str> = found["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["label"].as_str().unwrap())
        .collect();
    labels.sort();
    let mut expected = vec!["com.dagq.6a8d142711718fbe", fixture.location.label.as_str()];
    expected.sort();
    assert_eq!(labels, expected);
    let attention = duplicate_attention(&status);
    assert_eq!(attention.len(), 1, "{status:#}");
    let text = attention[0]["last_error"].as_str().unwrap();
    assert!(text.contains("2 agents loaded"), "{text}");
    assert!(text.contains("com.dagq.6a8d142711718fbe"), "{text}");

    // A planner's status has no attention, the inbox's has it.
    let planner = status_as(Some(dagq::domain::SessionRole::Planner));
    assert_eq!(planner["duplicate_supervisors"], *found);
    assert!(duplicate_attention(&planner).is_empty(), "{planner:#}");
    let inbox = status_as(Some(dagq::domain::SessionRole::Inbox));
    assert_eq!(duplicate_attention(&inbox).len(), 1, "{inbox:#}");
}
