//! `down` against fakes for launchd and process signals: every outcome in
//! either mode (interrupted, drained, force-killed, not running), and the
//! workspace of an in-cmux supervisor an earlier binary registered (`up`
//! starts none now, ADR-t1433-4), which `down` leaves to a person.

use crate::common;
use dagq::domain::LeaseToken;

use common::lifecycle::*;

use anyhow::Result;
use dagq::{
    VERSION,
    application::ProcessControl,
    domain::{SessionRole, SupervisorMode, recovery::ProcessInfo},
    infrastructure::sqlite::SqliteQueue,
    lifecycle::{self, DownOptions},
};
use rusqlite::Connection;
use serde_json::json;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::Duration,
};

/// Register a supervisor of the retired in-cmux mode, as an earlier binary
/// did, in `workspace`, with the workspace's record in `session_workspaces`.
fn in_cmux_supervisor(queue: &mut SqliteQueue, token: &str, pid: u32, workspace: &str) {
    queue
        .register_supervisor(&LeaseToken::new(token), pid, 2, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode(
            &LeaseToken::new(token),
            SupervisorMode::InCmux,
            Some(workspace),
        )
        .unwrap();
    queue
        .register_session_workspace(SessionRole::Supervisor, workspace)
        .unwrap();
}

/// What `down` reports of an in-cmux supervisor's workspace: left open for
/// a person to close (ADR-t2159-1 decision 3).
fn left_open(workspace: &str) -> serde_json::Value {
    json!([{
        "workspace_id": workspace,
        "outcome": "left_open",
        "reason": "dagq closes no cmux workspace: a person closes it",
    }])
}

/// An in-cmux supervisor (registered by an earlier binary) has no service
/// manager to signal it, so `down` sends the SIGINT itself, as before; it
/// closes no cmux workspace (ADR-t2159-1 decisions 3 and 6): the one the
/// supervisor ran in is reported left open whether `down` returns while it
/// drains or waits for the drain, and its record is forgotten. No cmux is
/// on the fixture's path, so none could be called.
#[test]
fn down_interrupts_an_in_cmux_supervisor_and_leaves_its_workspace_to_a_person() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    let workspace = "01234567-89ab-4def-8123-0000000000aa";
    in_cmux_supervisor(&mut queue, "in-cmux", pid, workspace);
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();

    // Default: SIGINT and return while it drains.
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining", "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(report["launch_agent_unloaded"], false);
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert_eq!(report["supervisor_workspaces"], left_open(workspace));
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        None
    );

    // `--wait` waits for the drain; the workspace is still the person's.
    queue
        .register_session_workspace(SessionRole::Supervisor, workspace)
        .unwrap();
    let processes = FakeProcesses::default();
    let db = fixture.location.db.clone();
    let report = thread::scope(|scope| {
        scope.spawn(move || {
            thread::sleep(Duration::from_millis(200));
            SqliteQueue::open(&db)
                .unwrap()
                .deregister_supervisor(&LeaseToken::new("in-cmux"))
                .unwrap();
        });
        down(&fixture, &launchd, &processes, true, false)
    });
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["pid"], pid);
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(report["supervisor_workspaces"], left_open(workspace));
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        None
    );
    assert!(backend_failures(&fixture).is_empty());
}

/// `--force` kills the in-cmux supervisor after its SIGINT and drops its
/// registration; its workspace is left to a person all the same.
#[test]
fn down_force_kills_an_in_cmux_supervisor_and_leaves_its_workspace_to_a_person() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    let workspace = "01234567-89ab-4def-8123-0000000000ab";
    in_cmux_supervisor(&mut queue, "in-cmux", pid, workspace);
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, true);
    assert_eq!(report["outcome"], "killed", "{report}");
    assert_eq!(processes.interrupted.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(processes.killed.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(report["supervisor_workspaces"], left_open(workspace));
    assert!(queue.supervisors().unwrap().is_empty());
    assert_eq!(
        queue.session_workspace(SessionRole::Supervisor).unwrap(),
        None
    );
    assert!(backend_failures(&fixture).is_empty());
}

#[test]
fn down_reports_not_running_without_a_live_registration_and_still_unloads_the_agent() {
    let fixture = fixture();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": false,
               "pruned_supervisors": [], "supervisor_workspaces": []})
    );
    // A dead registration is not a running supervisor either; a loaded
    // agent with no registered process (a crash loop) is unloaded.
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let dead = dead_pid();
    queue
        .register_supervisor(&LeaseToken::new("dead"), dead, 1, VERSION)
        .unwrap();
    processes.dead.lock().unwrap().insert(dead);
    launchd.load(None);
    let report = down(&fixture, &launchd, &processes, true, false);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": true,
               "pruned_supervisors": [], "supervisor_workspaces": []})
    );
    assert_eq!(
        launchd.uninstalls.lock().unwrap().as_slice(),
        &[
            (
                fixture.location.label.clone(),
                fixture.location.launch_agent.clone()
            ),
            (
                fixture.location.label.clone(),
                fixture.location.launch_agent.clone()
            ),
        ]
    );
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
    // The dead row is `up`'s to prune; only `down --force` removes it too.
    assert_eq!(queue.supervisors().unwrap().len(), 1);
    let report = down(&fixture, &launchd, &processes, false, true);
    assert_eq!(
        report,
        json!({"outcome": "not_running", "launch_agent_unloaded": false,
               "pruned_supervisors": [{"token": "dead", "pid": dead}],
               "supervisor_workspaces": []})
    );
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
    // The dead one could not record its stop, so the prune does, with the
    // row's last heartbeat `kpi` ends its life at (ADR-0051 decision 10).
    let stops: Vec<_> = queue
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "supervisor_stopped")
        .collect();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].payload["supervisor"], "dead");
    assert_eq!(stops[0].payload["outcome"], "pruned");
    assert!(stops[0].payload["last_heartbeat_at"].is_i64(), "{stops:?}");
}

/// A stale row with a reused pid is removed without signalling that pid,
/// including under --force. A silent real supervisor and a recently
/// heartbeating one are still stopped.
#[test]
fn down_prunes_reused_pids_without_signalling_them() {
    for force in [false, true] {
        let fixture = fixture();
        let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
        let (foreign, younger, silent, fresh) = (4_100_001, 4_100_002, 4_100_003, 4_100_004);
        for (token, pid) in [
            ("foreign", foreign),
            ("younger", younger),
            ("silent", silent),
            ("fresh", fresh),
        ] {
            queue
                .register_supervisor(&LeaseToken::new(token), pid, 1, VERSION)
                .unwrap();
        }
        let workspace = "01234567-89ab-4def-8123-0000000000ac";
        queue
            .set_supervisor_mode(
                &LeaseToken::new("foreign"),
                SupervisorMode::InCmux,
                Some(workspace),
            )
            .unwrap();
        let registered_at = 1_700_000_000;
        Connection::open(&fixture.location.db)
            .unwrap()
            .execute(
                "UPDATE supervisors SET started_at=?1, heartbeat_at=?1 WHERE token!='fresh'",
                [registered_at],
            )
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let listed = |pid, elapsed_secs| ProcessInfo {
            pid,
            ppid: 1,
            elapsed_secs,
            command: "dagq supervise".into(),
            cwd: None,
            cpu_ms: None,
        };
        let processes = FakeProcesses::default();
        *processes.listed.lock().unwrap() = Some(vec![
            listed(younger, 60),
            listed(silent, now - registered_at as u64 + 60),
        ]);
        let launchd = FakeLaunchd::new(&fixture.location.db);
        let report = down(&fixture, &launchd, &processes, false, force);
        assert_eq!(
            report["outcome"],
            if force { "killed" } else { "draining" },
            "{report}"
        );
        assert_eq!(report["pids"], json!([silent, fresh]), "{report}");
        assert_eq!(
            report["pruned_supervisors"],
            json!([
                {"token": "foreign", "pid": foreign, "reason": "pid_reused"},
                {"token": "younger", "pid": younger, "reason": "pid_reused"},
            ]),
            "{report}"
        );
        assert_eq!(
            processes.terminated.lock().unwrap().as_slice(),
            &[silent, fresh]
        );
        assert!(processes.interrupted.lock().unwrap().is_empty());
        let expected_kills = if force { vec![silent, fresh] } else { vec![] };
        assert_eq!(*processes.killed.lock().unwrap(), expected_kills);
        assert_eq!(report["supervisor_workspaces"], left_open(workspace));
        let tokens: Vec<_> = queue
            .supervisors()
            .unwrap()
            .into_iter()
            .map(|r| r.token.into_string())
            .collect();
        assert_eq!(
            tokens,
            if force {
                vec![]
            } else {
                vec!["silent", "fresh"]
            }
        );
    }
}

#[test]
fn down_signals_a_silent_supervisor_when_process_listing_fails() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = 4_100_005;
    queue
        .register_supervisor(&LeaseToken::new("silent"), pid, 1, VERSION)
        .unwrap();
    Connection::open(&fixture.location.db)
        .unwrap()
        .execute("UPDATE supervisors SET heartbeat_at=1700000000", [])
        .unwrap();
    let processes = FakeProcesses::default(); // list() fails
    let report = down(
        &fixture,
        &FakeLaunchd::new(&fixture.location.db),
        &processes,
        false,
        false,
    );
    assert_eq!(report["outcome"], "draining", "{report}");
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[pid]);
    assert_eq!(queue.supervisors().unwrap().len(), 1);
}

/// A queue can hold supervisors of both modes at once: the launchd one is
/// left to the bootout's SIGTERM, the in-cmux one is interrupted here, and
/// only the in-cmux one has a workspace, which is left to a person.
#[test]
fn down_stops_a_launchd_and_an_in_cmux_supervisor_in_one_call() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let agent_pid = std::process::id();
    let in_cmux_pid = dead_pid(); // any pid the fake treats as alive
    let workspace = "01234567-89ab-4def-8123-0000000000ad";
    queue
        .register_supervisor(&LeaseToken::new("agent"), agent_pid, 4, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode(&LeaseToken::new("agent"), SupervisorMode::Launchd, None)
        .unwrap();
    queue
        .register_supervisor(&LeaseToken::new("in-cmux"), in_cmux_pid, 2, VERSION)
        .unwrap();
    queue
        .set_supervisor_mode(
            &LeaseToken::new("in-cmux"),
            SupervisorMode::InCmux,
            Some(workspace),
        )
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(agent_pid));
    let processes = FakeProcesses::default();

    let report = down(&fixture, &launchd, &processes, false, true);
    assert_eq!(report["outcome"], "killed", "{report}");
    assert_eq!(report["pids"], json!([agent_pid, in_cmux_pid]));
    assert_eq!(report["launch_agent_unloaded"], true);
    // launchd's bootout carries the agent's SIGTERM; only the in-cmux
    // supervisor is signalled from here, and with SIGINT.
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert_eq!(
        processes.interrupted.lock().unwrap().as_slice(),
        &[in_cmux_pid]
    );
    assert_eq!(report["supervisor_workspaces"], left_open(workspace));
    assert!(queue.supervisors().unwrap().is_empty());
}

#[test]
fn down_unloads_the_agent_and_returns_while_the_supervisor_drains() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor(&LeaseToken::new("resident"), pid, 4, VERSION)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(
        report,
        json!({"outcome": "draining", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "supervisor_workspaces": []})
    );
    assert_eq!(
        launchd.uninstalls.lock().unwrap().as_slice(),
        &[(
            fixture.location.label.clone(),
            fixture.location.launch_agent.clone()
        )]
    );
    // launchd delivered the SIGTERM; nothing was signalled or removed here.
    assert!(processes.terminated.lock().unwrap().is_empty());
    assert!(processes.killed.lock().unwrap().is_empty());
    assert_eq!(queue.supervisors().unwrap().len(), 1);

    // A supervisor started by hand has no agent: it gets the SIGTERM directly.
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining");
    assert_eq!(report["launch_agent_unloaded"], false);
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[pid]);

    // Both at once: the agent's own process is left to launchd's SIGTERM
    // (a second one would end its drain), the hand-started one is signalled.
    let by_hand = dead_pid(); // any pid the fake treats as alive
    queue
        .register_supervisor(&LeaseToken::new("by-hand"), by_hand, 1, VERSION)
        .unwrap();
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, false);
    assert_eq!(report["outcome"], "draining");
    assert_eq!(report["pids"], json!([pid, by_hand]));
    assert_eq!(processes.terminated.lock().unwrap().as_slice(), &[by_hand]);
    // When launchd cannot say which process is the agent's, nobody is signalled.
    launchd.load(None);
    let processes = FakeProcesses::default();
    down(&fixture, &launchd, &processes, false, false);
    assert!(processes.terminated.lock().unwrap().is_empty());
}

#[test]
fn down_wait_returns_stopped_once_the_registration_is_gone_or_the_process_died() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor(&LeaseToken::new("resident"), pid, 4, VERSION)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    // The supervisor deregisters itself at the end of its drain.
    let db = fixture.location.db.clone();
    let drain = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        SqliteQueue::open(&db)
            .unwrap()
            .deregister_supervisor(&LeaseToken::new("resident"))
            .unwrap();
    });
    let report = down(&fixture, &launchd, &processes, true, false);
    {
        let _waiting = common::within(common::STEP_LIMIT, "the drain thread to return");
        drain.join().unwrap();
    }
    assert_eq!(
        report,
        json!({"outcome": "stopped", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "supervisor_workspaces": []})
    );
    assert!(processes.killed.lock().unwrap().is_empty());

    // A supervisor killed by launchd's ExitTimeOut leaves its row; the dead
    // PID ends the wait just the same (and `up` prunes the row later).
    queue
        .register_supervisor(&LeaseToken::new("second"), pid, 4, VERSION)
        .unwrap();
    launchd.load(Some(pid));
    struct DiesLater {
        polls: AtomicUsize,
    }
    impl ProcessControl for DiesLater {
        fn alive(&self, _: u32) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) < 3
        }
        fn terminate(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn kill(&self, _: u32) -> Result<()> {
            unreachable!()
        }
    }
    let report = lifecycle::down(
        &fixture.location,
        &launchd,
        &DiesLater {
            polls: AtomicUsize::new(0),
        },
        &DownOptions {
            wait: true,
            force: false,
            poll: Duration::from_millis(20),
        },
    )
    .unwrap();
    assert_eq!(
        report,
        json!({"outcome": "stopped", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "supervisor_workspaces": []})
    );
    // The row stays: the process, not `down`, removes a registration.
    assert_eq!(queue.supervisors().unwrap().len(), 1);
}

#[test]
fn down_force_kills_after_the_unload_and_drops_the_registration() {
    let fixture = fixture();
    let mut queue = SqliteQueue::open(&fixture.location.db).unwrap();
    let pid = std::process::id();
    queue
        .register_supervisor(&LeaseToken::new("resident"), pid, 4, VERSION)
        .unwrap();
    let launchd = FakeLaunchd::new(&fixture.location.db);
    launchd.load(Some(pid));
    let processes = FakeProcesses::default();
    let report = down(&fixture, &launchd, &processes, false, true);
    assert_eq!(
        report,
        json!({"outcome": "killed", "pid": pid, "pids": [pid],
               "launch_agent_unloaded": true, "pruned_supervisors": [],
               "supervisor_workspaces": []})
    );
    assert_eq!(launchd.uninstalls.lock().unwrap().len(), 1);
    assert_eq!(processes.killed.lock().unwrap().as_slice(), &[pid]);
    assert!(queue.supervisors().unwrap().is_empty());
    assert!(!*launchd.loaded.lock().unwrap());
}
