//! The automatic update's swap closes the `update_failed` asks it settles:
//! once a job puts a build in place, the runtime answers the job's open
//! `update_failed` asks whose failed commit the build contains, and leaves
//! a person's install's and those whose ancestry cannot be told, the swap
//! staying installed. The job runs against the fake binaries and process
//! signals of [`crate::lifecycle_install`].

use crate::common;
use crate::lifecycle_install::{
    UpdateBinaries, auto_supervisor, run_update_job_of, take_and_heartbeat,
};
use common::lifecycle::*;

use anyhow::{Result, bail};
use dagq::domain::{AskId, AskKind, EventKind, INSTALL_SOURCE};
use serde_json::{Value, json};
use std::{fs, sync::Mutex, thread, time::Duration};

/// `fff…` descends from `bbb…`; any other pair but a commit and itself is
/// unrelated.
fn ancestry(ancestor: &str, descendant: &str) -> Result<bool> {
    Ok(ancestor == descendant || (ancestor.starts_with('b') && descendant.starts_with('f')))
}

/// A swap whose ancestry cannot be told is installed and closes nothing;
/// one of a commit that contains the failed build closes the job's ask,
/// answered `installed` by the runtime with the commit in its
/// `ask_answered`, and leaves a person's install's ask open.
#[test]
fn the_update_job_closes_the_failures_its_swap_contains() {
    let fixture = fixture();
    let mut queue = auto_supervisor(&fixture);
    let processes = FakeProcesses::default();
    let restarted = Mutex::new(Vec::new());
    let dir = fixture._dir.path();
    let target = dir.join("bin").join("dagq");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "old build").unwrap();
    let broken = "b".repeat(40);
    let fixed = "f".repeat(40);
    let timeout = Duration::from_secs(5);

    // A person's install failed, and a job's build of `broken`.
    let install_ask = queue
        .open_update_ask(
            AskKind::UpdateFailed,
            "A person's `dagq install` failed",
            &["retry", "skip"],
            "supervisor",
            Some(INSTALL_SOURCE),
            json!({"source": INSTALL_SOURCE}),
        )
        .unwrap()
        .id;
    queue
        .record_queue_event(
            EventKind::UpdateFailed,
            json!({"ask_id": install_ask, "stage": "watch", "source": INSTALL_SOURCE}),
        )
        .unwrap();
    let report = run_update_job_of(
        &fixture,
        &UpdateBinaries::new(dir, true, &[]),
        &processes,
        &restarted,
        timeout,
        &broken,
        &ancestry,
    );
    assert_eq!(report["stage"], "build", "{report}");
    let failed_ask = AskId::new(report["ask_id"].as_i64().unwrap());

    let swap = |is_ancestor: &dyn Fn(&str, &str) -> Result<bool>, pending: &[(i64, bool)]| {
        thread::scope(|scope| {
            scope.spawn(|| take_and_heartbeat(&fixture, &processes, true));
            run_update_job_of(
                &fixture,
                &UpdateBinaries::new(dir, false, pending),
                &processes,
                &restarted,
                timeout,
                &fixed,
                is_ancestor,
            )
        })
    };
    let unknown = |_: &str, _: &str| -> Result<bool> { bail!("git is gone") };
    let report = swap(&unknown, &[(40, true)]);
    assert_eq!(report["outcome"], "installed", "{report}");
    assert!(queue.read_ask(failed_ask).unwrap().is_open());

    let report = swap(&ancestry, &[]);
    assert_eq!(report["outcome"], "installed", "{report}");
    let closed = queue.read_ask(failed_ask).unwrap();
    assert_eq!(closed.answer.as_deref(), Some("installed"));
    assert_eq!(closed.answered_by.as_deref(), Some("runtime"));
    assert!(closed.closed_at.is_some());
    assert!(queue.read_ask(install_ask).unwrap().is_open());
    let answered: Vec<Value> = rusqlite::Connection::open(&fixture.location.db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind='ask_answered'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|payload| serde_json::from_str(&payload.unwrap()).unwrap())
        .collect();
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["ask_id"], json!(failed_ask));
    assert_eq!(answered[0]["runtime_closed"], true);
    assert_eq!(answered[0]["installed_commit"], json!(fixed));
}
