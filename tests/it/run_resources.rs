//! The runs' resources the supervisor measures from outside on queue.db
//! (`RunResourceStore`, docs/design/measurement.md "実行環境と資源"): the
//! windows in the order they were recorded and the attachments still
//! open, the pass that measures the runs of the slots by the processes
//! they registered on this host and closes those that left the slots, and
//! those whose last attachment no live lease holds, and the ledger's pass
//! that folds them and settles the run only after its last window. The
//! decisions of the fold are the unit tests of `domain::environment` and
//! `application::supervise::ledger`.
use crate::runtime_support;

use dagq::application::supervise::{Retain, Track, build_pass, measure_pass};
use dagq::application::{ExecutionEnvironment, LedgerStore, RunResourceStore};
use dagq::domain::environment::{EnvKind, Resources, RunChildren, RunResource};
use dagq::domain::host_metrics::HOST_NODE;
use dagq::domain::ledger::{LedgerKind, builder::Builder};
use dagq::domain::{ClaimOutcome, EventKind, LeaseToken, RunId};
use dagq::infrastructure::environment::HostEnvironment;
use dagq::infrastructure::runtime_store::RunPlan;
use runtime_support::*;
use std::collections::BTreeMap;

const NOW_MS: i64 = 1_900_000_000_000;

/// Now on the clock the queue's leases beat on, unix ms.
fn now_ms() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    i64::try_from(since.as_millis()).unwrap()
}

/// A run of a ready task claimed under `token`, its wrapper this test's
/// process.
fn running(queue: &mut SqliteQueue, token: &str) -> RunId {
    let base = sha("0123456789abcdef0123456789abcdef01234567");
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor(&base, &LeaseToken::new(token))
        .unwrap()
    else {
        panic!("nothing to claim")
    };
    let id = run.id().clone();
    queue
        .plan_run(
            &id,
            &LeaseToken::new(token),
            &RunPlan {
                repo_path: "/test".into(),
                run_dir: format!("/run/{id}"),
                branch: format!("dagq/{id}"),
                worktree_path: format!("/run/{id}/worktree"),
                receipt_path: format!("/run/{id}/receipt.json"),
                log_path: format!("/run/{id}/log"),
            },
        )
        .unwrap();
    queue
        .workspace_created(&id, &LeaseToken::new(token), "ws")
        .unwrap();
    queue
        .register_wrapper(&id, &LeaseToken::new(token), std::process::id())
        .unwrap();
    id
}

/// Record `run`'s attachment to `environment`; the event's id.
fn attach(queue: &SqliteQueue, run: &RunId, environment: &dyn ExecutionEnvironment) -> i64 {
    queue
        .record_runtime_event(
            run,
            EventKind::EnvironmentAttached,
            environment.attached().payload(),
        )
        .unwrap();
    queue
        .run_events(run)
        .unwrap()
        .iter()
        .rfind(|event| event.kind == EventKind::EnvironmentAttached)
        .unwrap()
        .id
        .as_i64()
}

/// The windows are kept in the order they were recorded with every value
/// as written; a run's last attachment is open until a last window of it
/// is recorded, and a new attachment opens the run again.
#[test]
fn the_windows_are_kept_in_order_and_an_attachment_is_open_until_its_last() {
    let (_fixture, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = running(&mut queue, "sv");
    let host = HostEnvironment::new();
    assert!(queue.open_attachments().unwrap().is_empty());
    let first = attach(&queue, &run, &host);
    assert_eq!(queue.open_attachments().unwrap(), [(run.clone(), first)]);
    let window = |last: bool, cpu: f64| RunResource {
        id: 0,
        run_id: run.to_string(),
        attachment: first,
        node_id: HOST_NODE.to_owned(),
        from_ms: NOW_MS,
        until_ms: NOW_MS + 30_000,
        resources: Resources {
            cpu_seconds: Some(cpu),
            cpu_limit: Some(8.0),
            memory_max_bytes: Some(1 << 30),
            throttled_seconds: Some(0.25),
            cpu_pressure: Some(1.5),
            load_average: Some(2.0),
        },
        last,
    };
    assert_eq!(queue.record_run_resource(&window(false, 1.5)).unwrap(), 1);
    assert_eq!(queue.record_run_resource(&window(true, 2.5)).unwrap(), 2);
    let kept = queue.run_resources(&run).unwrap();
    assert_eq!(
        kept,
        [
            RunResource {
                id: 1,
                ..window(false, 1.5)
            },
            RunResource {
                id: 2,
                ..window(true, 2.5)
            }
        ]
    );
    assert_eq!(queue.run_resources_after(1, 10).unwrap(), kept[1..]);
    assert_eq!(queue.run_resources_after(0, 1).unwrap(), kept[..1]);
    assert!(queue.open_attachments().unwrap().is_empty());
    let second = attach(&queue, &run, &host);
    assert_eq!(queue.open_attachments().unwrap(), [(run, second)]);
}

/// The pass measures a run of the slots by its wrapper (this test's
/// process) on this host, in the common units, under its attachment; a
/// run not attached is not measured. Once the run left the slots the next
/// pass closes it with a last window, and one back in the slots under the
/// attachment it closed is not measured again. A run whose attachment is open and
/// whose lease lives is left to its supervisor; once that lease is stale
/// any supervisor's pass closes it with a last window of no value. The
/// ledger then folds the windows into the run's row.
#[test]
fn a_pass_measures_the_runs_of_the_slots_and_closes_those_no_one_measures() {
    let (_fixture, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    add_ready_task(&mut queue, "second", &[]);
    let measured_run = running(&mut queue, "sv");
    let orphan = running(&mut queue, "gone");
    let host = HostEnvironment::new();
    // This host names itself by its own cores, memory and architecture.
    let attached = host.attached();
    assert_eq!(attached.env_kind, EnvKind::Host);
    assert!(
        attached.cpu_limit.is_some_and(|cores| cores >= 1.0),
        "{attached:?}"
    );
    assert!(
        attached.mem_limit.is_some_and(|bytes| bytes > 0),
        "{attached:?}"
    );
    assert_eq!(attached.arch, std::env::consts::ARCH);
    let mut tracked: BTreeMap<String, Track> = BTreeMap::new();
    let alive = |_: u32| true;
    let now = now_ms();

    // Neither attached yet: nothing measured.
    let held = [measured_run.clone()];
    let pass = measure_pass(
        &queue,
        &host,
        &RunChildren::default(),
        &mut tracked,
        &held,
        now,
        &alive,
    );
    assert_eq!(pass.failure, None);
    assert_eq!(pass.windows, 0);
    assert!(tracked.is_empty());

    let attachment = attach(&queue, &measured_run, &host);
    let pass = measure_pass(
        &queue,
        &host,
        &RunChildren::default(),
        &mut tracked,
        &held,
        now + 1_000,
        &alive,
    );
    assert_eq!((pass.windows, pass.closed), (1, 0), "{pass:?}");
    let windows = queue.run_resources(&measured_run).unwrap();
    assert_eq!(windows.len(), 1);
    let window = &windows[0];
    assert_eq!(window.attachment, attachment);
    assert_eq!(window.node_id, HOST_NODE);
    assert!(!window.last);
    assert_eq!(window.until_ms, now + 1_000);
    assert!(window.from_ms <= window.until_ms);
    assert!(
        window.resources.cpu_seconds.is_some_and(|cpu| cpu >= 0.0),
        "{window:?}"
    );
    assert!(
        window
            .resources
            .memory_max_bytes
            .is_some_and(|bytes| bytes > 0),
        "{window:?}"
    );

    // It left the slots: closed with a last window, and no longer tracked.
    let pass = measure_pass(
        &queue,
        &host,
        &RunChildren::default(),
        &mut tracked,
        &[],
        now + 2_000,
        &alive,
    );
    assert_eq!(pass.closed, 1, "{pass:?}");
    assert!(tracked.is_empty());
    let last = queue.run_resources(&measured_run).unwrap();
    assert!(last[1].last && last[1].attachment == attachment);
    assert!(queue.open_attachments().unwrap().is_empty());
    // Back in the slots under the attachment it closed: not measured again
    // until a new attachment.
    let pass = measure_pass(
        &queue,
        &host,
        &RunChildren::default(),
        &mut tracked,
        &held,
        now + 2_500,
        &alive,
    );
    assert_eq!((pass.windows, pass.closed), (0, 0), "{pass:?}");
    assert!(tracked.is_empty());
    assert_eq!(queue.run_resources(&measured_run).unwrap().len(), 2);

    // Another supervisor's run, attached and left open.
    let orphaned = attach(&queue, &orphan, &host);
    let pass = measure_pass(
        &queue,
        &host,
        &RunChildren::default(),
        &mut tracked,
        &[],
        now + 3_000,
        &alive,
    );
    assert_eq!(pass.closed, 0, "its lease lives: {pass:?}");
    assert_eq!(
        queue.open_attachments().unwrap(),
        [(orphan.clone(), orphaned)]
    );
    let dead = |_: u32| false;
    let pass = measure_pass(
        &queue,
        &host,
        &RunChildren::default(),
        &mut tracked,
        &[],
        now + 4_000,
        &dead,
    );
    assert_eq!(pass.closed, 1, "{pass:?}");
    let closed = queue.run_resources(&orphan).unwrap();
    assert_eq!(closed.len(), 1);
    assert!(closed[0].last && closed[0].resources.is_empty());
    assert!(queue.open_attachments().unwrap().is_empty());

    // The ledger folds the windows it reads into the run's row once the run
    // has one to write; here it reads them past its cursor.
    let mut builder = Builder::default();
    let built = build_pass(
        &queue,
        &mut builder,
        (now + 5_000, "2030-01-01T00:00:00.000Z"),
        Retain::default(),
        None,
    );
    assert_eq!(built.failure, None);
    assert_eq!(builder.resources, 3);
    // The runs are on their way: no row is written for them yet.
    assert!(
        queue
            .ledger_row(LedgerKind::Run, measured_run.as_str())
            .unwrap()
            .is_none()
    );
}

/// What `ps` reads of `pid` in the column `column`.
fn ps_of(pid: u32, column: &str) -> String {
    let ps = std::process::Command::new("ps")
        .args(["-o", &format!("{column}="), "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout).trim().to_owned()
}

/// A process of another tree than this test's: a shell's background
/// subshell left by the shell (its parent) as it exits, which spends CPU
/// until `ps` reads some (Linux's `time` has whole seconds, macOS's
/// hundredths), then idles; its pid. It spins and idles in `dir`, and ends
/// once `dir` or this test's process is gone, whether the test passed,
/// failed or timed out.
fn orphan_that_spent_cpu(dir: &std::path::Path) -> u32 {
    let stop = dir.join("stop");
    let idle = dir.join("idle");
    let script = format!(
        "( while [ ! -e {stop} ]; do [ -d {dir} ] && kill -0 {test} 2>/dev/null || exit 1; \
         done; : > {idle}; while [ -d {dir} ] && kill -0 {test} 2>/dev/null; do sleep 1; \
         done ) </dev/null >/dev/null 2>&1 & echo $!",
        stop = crate::common::shell_path(&stop),
        idle = crate::common::shell_path(&idle),
        dir = crate::common::shell_path(dir),
        test = std::process::id(),
    );
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &script])
        .output()
        .unwrap();
    let pid: u32 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Until `ps` reads CPU it spent, then until it idles.
    let _waiting = crate::common::within(crate::common::STEP_LIMIT, "the orphan to spend its CPU");
    loop {
        let time = ps_of(pid, "time");
        assert!(!time.is_empty(), "the orphan ended before it spent CPU");
        if time.bytes().any(|c| matches!(c, b'1'..=b'9')) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::fs::write(&stop, "").unwrap();
    while !idle.exists() {
        assert!(
            !ps_of(pid, "time").is_empty(),
            "the orphan ended before it idled"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let parent: u32 = ps_of(pid, "ppid").parse().unwrap();
    assert_ne!(parent, std::process::id(), "it is not under this test");
    pid
}

/// A run's process in another tree than its worker's (a headless job or a
/// landing's verification command, which the supervisor starts and
/// registers in `RunChildren`) is measured as the run's on this host: a
/// run whose worker exited and that has no such process is not measured;
/// with a process of another tree registered (here one orphaned from
/// this test's tree), its window counts what that process spent and the
/// memory it holds, and the fold puts it in the run's interval it ran in.
/// Once its guard is dropped it is no longer the run's.
#[test]
fn a_runs_process_in_another_tree_is_measured_and_folded_as_the_runs() {
    use dagq::domain::environment::{Window, fold};
    let (_fixture, _repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = running(&mut queue, "sv");
    // Its worker's wrapper exited: none of the processes it registered lives.
    queue.wrapper_exited(&run, std::process::id(), 0).unwrap();
    let host = HostEnvironment::new();
    let children = RunChildren::default();
    let mut tracked: BTreeMap<String, Track> = BTreeMap::new();
    let alive = |_: u32| true;
    let held = [run.clone()];
    // Attached before the orphan starts, so the window holds its whole
    // life: the host counts a process first seen that started before the
    // window only from that measure on.
    attach(&queue, &run, &host);
    let spin = tempfile::tempdir().unwrap();
    let orphan = orphan_that_spent_cpu(spin.path());

    let pass = measure_pass(
        &queue,
        &host,
        &children,
        &mut tracked,
        &held,
        now_ms(),
        &alive,
    );
    assert_eq!(
        (pass.failure, pass.windows),
        (None, 0),
        "nothing of it lives"
    );

    let job = children.started(&run, orphan);
    let pass = measure_pass(
        &queue,
        &host,
        &children,
        &mut tracked,
        &held,
        now_ms(),
        &alive,
    );
    assert_eq!((pass.failure, pass.windows), (None, 1));
    let windows = queue.run_resources(&run).unwrap();
    let window = windows.last().unwrap();
    assert!(
        window.resources.cpu_seconds.is_some_and(|cpu| cpu > 0.0),
        "{window:?}"
    );
    assert!(
        window
            .resources
            .memory_max_bytes
            .is_some_and(|bytes| bytes > 0),
        "{window:?}"
    );
    let interval: dagq::domain::ledger::fold::Interval =
        serde_json::from_value(serde_json::json!({
            "phase": "review",
            "start": window.from_ms - 1,
            "end": window.until_ms + 1,
        }))
        .unwrap();
    let folded = fold(&[interval], &[Window::of_run(window)], &[]).unwrap();
    assert_eq!(folded.total.cpu_seconds, window.resources.cpu_seconds);
    assert_eq!(
        folded.intervals[0].cpu_seconds,
        window.resources.cpu_seconds
    );

    drop(job);
    assert!(children.of(&run).is_empty());
    let pass = measure_pass(
        &queue,
        &host,
        &children,
        &mut tracked,
        &held,
        now_ms(),
        &alive,
    );
    assert_eq!(
        (pass.failure.as_deref(), pass.windows),
        (None, 0),
        "{pass:?}"
    );
    let _ = std::process::Command::new("kill")
        .arg(orphan.to_string())
        .status();
}

/// A verification command the shell verifier runs for a run is the run's
/// process while it runs, and no longer once it ended.
#[test]
fn a_verification_command_run_for_a_run_is_its_process_while_it_runs() {
    use dagq::application::Verifier;
    use dagq::infrastructure::run_env::ShellVerifier;
    let _test = crate::common::test();
    let dir = tempfile::tempdir().unwrap();
    let children = RunChildren::default();
    let verifier = ShellVerifier {
        checkout: dir.path().to_path_buf(),
        db: dir.path().join("queue.db"),
        user_config: None,
        verification_timeout: std::time::Duration::from_secs(60),
        profiles: &dagq::domain::toolchain::BUILTIN,
        children: children.clone(),
    };
    let run = RunId::new("00000001-1111-4111-8111-111111111111").unwrap();
    let flag = dir.path().join("go");
    let log = dir.path().join("command.log");
    let command = crate::common::await_path(&flag);
    let (pid, ran) = {
        let _waiting = crate::common::within(
            crate::common::STEP_LIMIT,
            "the command to start, be the run's and end",
        );
        std::thread::scope(|scope| {
            let ran =
                scope.spawn(|| verifier.run_to_log_for(&run, &command, dir.path(), &[], &log));
            let pid = loop {
                if let Some(pid) = children.of(&run).first().copied() {
                    break pid;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            // The command ends before anything is asserted, so a failed
            // assertion leaves no thread for the scope to wait on.
            std::fs::write(&flag, "").unwrap();
            (pid, ran.join().unwrap())
        })
    };
    assert_ne!(pid, std::process::id());
    assert!(ran.unwrap().success);
    assert!(children.of(&run).is_empty());
}
