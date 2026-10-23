use serde_json::json;

use super::*;
use crate::domain::RunId;
use crate::domain::ledger::testkit::{Log, ms, run_id};
use crate::domain::session_step::StepTarget;

fn interval(phase: &str, start: i64, end: i64) -> Interval {
    serde_json::from_value(json!({"phase": phase, "start": start, "end": end})).unwrap()
}

fn window(from_ms: i64, until_ms: i64, resources: Resources) -> Window {
    Window {
        from_ms,
        until_ms,
        resources,
    }
}

fn spent(cpu_seconds: f64, memory_max_bytes: i64) -> Resources {
    Resources {
        cpu_seconds: Some(cpu_seconds),
        cpu_limit: Some(8.0),
        memory_max_bytes: Some(memory_max_bytes),
        ..Resources::default()
    }
}

/// The time spent goes to each interval in the share of the window it
/// overlaps and adds up to the whole; the memory and the limit are the
/// most of the windows over it; an interval no window reaches has none; a
/// window of no length goes to the interval it falls in.
#[test]
fn the_windows_from_outside_fold_into_the_intervals_in_the_units_they_were_measured() {
    let intervals = [
        interval("provisioning", 0, 10_000),
        interval("worker", 10_000, 40_000),
        interval("validating", 40_000, 50_000),
        interval("review", 50_000, 60_000),
    ];
    let outside = [
        window(0, 20_000, spent(20.0, 100)),
        window(20_000, 50_000, spent(3.0, 300)),
        window(40_000, 40_000, spent(1.0, 50)),
    ];
    let folded = fold(&intervals, &outside, &[]).unwrap();
    assert_eq!(folded.source, Some(Source::Outside));
    assert_eq!(folded.total.cpu_seconds, Some(24.0));
    assert_eq!(folded.total.memory_max_bytes, Some(300));
    let cpu: Vec<Option<f64>> = folded.intervals.iter().map(|r| r.cpu_seconds).collect();
    assert_eq!(cpu, [Some(10.0), Some(12.0), Some(2.0), None]);
    let memory: Vec<Option<i64>> = folded
        .intervals
        .iter()
        .map(|r| r.memory_max_bytes)
        .collect();
    assert_eq!(memory, [Some(100), Some(300), Some(300), None]);
    assert_eq!(folded.intervals[1].cpu_limit, Some(8.0));
    assert!(folded.intervals[3].is_empty());
    // As the row keeps it, and back.
    let detail = json!({ DETAIL_KEY: folded.detail() });
    assert_eq!(Folded::of_detail(&detail), Some(folded));
}

/// Where the supervisor measured no value from outside (an environment it
/// cannot measure from), the samples the executor measured from inside,
/// placed from the session's start, are folded in their place; any value
/// from outside wins over them; with neither there is nothing to fold.
#[test]
fn the_samples_from_inside_are_used_where_nothing_was_measured_from_outside() {
    let intervals = [interval("worker", 0, 60_000)];
    let mut sample = ResourceSample::new(1, 5_000, 10_000);
    sample.cpu_seconds = Some(4.0);
    sample.memory_max_bytes = Some(2048);
    sample.throttled_seconds = Some(1.5);
    sample.cpu_pressure = Some(12.0);
    let inside = SessionResource {
        id: 1,
        target: StepTarget::run(&RunId::new(run_id(1)).unwrap()),
        session_id: "s".to_owned(),
        received_at: 0,
        sample,
    };
    let placed = Window::of_sample(&inside, 10_000);
    assert_eq!((placed.from_ms, placed.until_ms), (15_000, 25_000));
    let unmeasured = [window(0, 30_000, Resources::default())];
    let folded = fold(&intervals, &unmeasured, std::slice::from_ref(&placed)).unwrap();
    assert_eq!(folded.source, Some(Source::Inside));
    assert_eq!(folded.intervals[0].cpu_seconds, Some(4.0));
    assert_eq!(folded.intervals[0].throttled_seconds, Some(1.5));
    assert_eq!(folded.intervals[0].cpu_pressure, Some(12.0));
    assert_eq!(folded.total.memory_max_bytes, Some(2048));

    let outside = [window(0, 30_000, spent(9.0, 10))];
    let folded = fold(&intervals, &outside, &[placed]).unwrap();
    assert_eq!(folded.source, Some(Source::Outside));
    assert_eq!(folded.total.cpu_seconds, Some(9.0));
    assert_eq!(fold(&intervals, &unmeasured, &[]), None);
}

fn attached(log: &mut Log, run: &str, secs: i64) {
    let payload = EnvironmentAttached {
        env_kind: EnvKind::Host,
        node_id: "host".to_owned(),
        image_digest: None,
        cpu_limit: Some(8.0),
        mem_limit: Some(1 << 34),
        arch: "aarch64".to_owned(),
    }
    .payload();
    log.add(Some(1), Some(run), "environment_attached", payload, secs);
}

fn resource(id: i64, run: &str, attachment: i64, last: bool) -> RunResource {
    RunResource {
        id,
        run_id: run.to_owned(),
        attachment,
        node_id: "host".to_owned(),
        from_ms: ms(0),
        until_ms: ms(30),
        resources: Resources::default(),
        last,
    }
}

/// A run attached to no environment (one from before the record) waits on
/// no measure; one attached waits on a last window of its last
/// attachment, whatever windows of the attachments before it were
/// recorded.
#[test]
fn a_run_waits_on_the_last_window_of_its_last_attachment() {
    let run = run_id(1);
    let mut log = Log::default();
    log.add(Some(1), Some(&run), "run_claimed", json!({}), 0);
    assert!(resources_recorded(&log.events, &[]));
    attached(&mut log, &run, 1);
    let first = log.last().as_i64();
    assert!(!resources_recorded(&log.events, &[]));
    assert!(!resources_recorded(
        &log.events,
        &[resource(1, &run, first, false)]
    ));
    assert!(resources_recorded(
        &log.events,
        &[
            resource(1, &run, first, false),
            resource(2, &run, first, true)
        ]
    ));
    // A resume attaches it again: the last window of the first no longer
    // settles it.
    attached(&mut log, &run, 100);
    let second = log.last().as_i64();
    assert_eq!(last_attachment(&log.events), Some(second));
    assert!(!resources_recorded(
        &log.events,
        &[resource(2, &run, first, true)]
    ));
    assert!(resources_recorded(
        &log.events,
        &[
            resource(2, &run, first, true),
            resource(3, &run, second, true)
        ]
    ));
}

/// The payload of `environment_attached` holds every item and reads back.
#[test]
fn an_attachment_reads_back_from_its_payload() {
    let attached = EnvironmentAttached {
        env_kind: EnvKind::Container,
        node_id: "node-a".to_owned(),
        image_digest: Some("sha256:abc".to_owned()),
        cpu_limit: Some(2.0),
        mem_limit: Some(4 << 30),
        arch: "x86_64".to_owned(),
    };
    let payload = attached.payload();
    assert_eq!(
        payload,
        json!({
            "env_kind": "container",
            "node_id": "node-a",
            "image_digest": "sha256:abc",
            "cpu_limit": 2.0,
            "mem_limit": 4_294_967_296_i64,
            "arch": "x86_64",
        })
    );
    assert_eq!(EnvironmentAttached::of_payload(&payload), Some(attached));
    assert_eq!(
        EnvironmentWait::ImagePull.phase_name(),
        "environment_image_pull"
    );
}

/// `ps`'s times read as seconds on macOS and Linux, and a line it cannot
/// read is left out.
#[test]
fn ps_reads_each_process_with_its_cpu_seconds_and_memory() {
    let listed = processes(
        "  1     0  3-00:00:00   0:01.50   100\n\
         20     1     05:00    1:02:03    10\n\
         21    20     00:07  2-00:00:01    1\n\
         22    20     00:07  125:00.25     4\n\
         bad line\n",
    );
    assert_eq!(
        listed,
        [
            process(1, 0, 259_200.0, 1.5, 102_400),
            process(20, 1, 300.0, 3723.0, 10_240),
            process(21, 20, 7.0, 172_801.0, 1024),
            process(22, 20, 7.0, 7500.25, 4096),
        ]
    );
    assert_eq!(cpu_time("1:2:3:4"), None);
    assert_eq!(cpu_time("x"), None);
}

fn process(pid: u32, ppid: u32, elapsed_secs: f64, cpu_seconds: f64, rss_bytes: i64) -> Process {
    Process {
        pid,
        ppid,
        elapsed_secs,
        cpu_seconds,
        rss_bytes,
    }
}

/// A run's tree is its processes and every one under them; each window
/// counts the CPU spent since the last (a process started in the window
/// all it spent, a pid whose CPU went down another process), and the
/// memory held now; a tree gone measures nothing.
#[test]
fn the_meter_counts_what_the_runs_tree_spent_since_its_last_window() {
    let mut meter = ProcessMeter::default();
    let first = [
        process(1, 0, 900.0, 100.0, 1),
        process(10, 1, 20.0, 5.0, 100),
        process(11, 10, 10.0, 2.0, 50),
        process(12, 1, 20.0, 7.0, 25),
    ];
    assert_eq!(tree(&first, &[10]), BTreeSet::from([10, 11]));
    assert_eq!(
        meter.window(&first, &[10, 99], 30.0),
        (Some(7.0), Some(150))
    );
    let second = [
        process(10, 1, 50.0, 6.5, 120),
        process(11, 10, 5.0, 1.0, 10),
        process(13, 11, 5.0, 0.5, 5),
    ];
    assert_eq!(meter.window(&second, &[10], 30.0), (Some(3.0), Some(135)));
    assert_eq!(
        meter.window(&[process(12, 1, 1.0, 9.0, 1)], &[10], 30.0),
        (None, None)
    );
}

/// A process first seen that started before the window (a run adopted
/// from another supervisor) counts only what it spends from then on: what
/// it spent before was another supervisor's window.
#[test]
fn a_process_that_started_before_the_window_counts_from_its_first_measure() {
    let mut meter = ProcessMeter::default();
    let adopted = [
        process(10, 1, 600.0, 600.0, 100),
        process(11, 10, 3.0, 2.0, 50),
    ];
    assert_eq!(meter.window(&adopted, &[10], 30.0), (Some(2.0), Some(150)));
    let later = [
        process(10, 1, 630.0, 610.0, 100),
        process(11, 10, 33.0, 4.0, 50),
    ];
    assert_eq!(meter.window(&later, &[10], 30.0), (Some(12.0), Some(150)));
}

/// The host's node in the common units: its CPU over the interval from
/// the percent of a core, its memory used in bytes, its cores and its
/// load; nothing throttles the host. The PSI's `some avg10` is its
/// pressure.
#[test]
fn a_sample_of_the_hosts_load_is_put_in_the_common_units() {
    let mut sample = HostSample::new(0);
    sample.set("cpu_total", Some(250.0));
    sample.set("mem_used_mb", Some(1024.0));
    sample.set("load1", Some(3.5));
    assert_eq!(
        host_node_resources(&sample, 30.0, Some(8.0), Some(1.25)),
        Resources {
            cpu_seconds: Some(75.0),
            cpu_limit: Some(8.0),
            memory_max_bytes: Some(1 << 30),
            throttled_seconds: None,
            cpu_pressure: Some(1.25),
            load_average: Some(3.5),
        }
    );
    assert!(host_node_resources(&HostSample::new(0), 30.0, None, None).is_empty());
    assert_eq!(
        cpu_pressure(
            "some avg10=2.04 avg60=1.00 avg300=0.50 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
        ),
        Some(2.04)
    );
    assert_eq!(cpu_pressure("full avg10=1.00"), None);
}

/// A process started for a run is the run's until its guard is dropped:
/// each run's apart, a pid registered twice counted once and its run's
/// until both guards are dropped.
#[test]
fn a_process_started_for_a_run_is_its_until_its_guard_is_dropped() {
    let (a, b) = (
        crate::domain::RunId::new("00000001-1111-4111-8111-111111111111").unwrap(),
        crate::domain::RunId::new("00000002-1111-4111-8111-111111111111").unwrap(),
    );
    let children = RunChildren::default();
    let job = children.started(&a, 300);
    let check = children.started(&a, 200);
    let again = children.started(&a, 300);
    let other = children.started(&b, 400);
    assert_eq!(children.of(&a), [200, 300]);
    assert_eq!(children.of(&b), [400]);
    drop(job);
    assert_eq!(children.of(&a), [200, 300]);
    drop(again);
    drop(check);
    assert!(children.of(&a).is_empty());
    assert_eq!(children.clone().of(&b), [400]);
    drop(other);
    assert!(children.of(&b).is_empty());
}

/// The tree of each root is the run's: a job's in another tree than the
/// wrapper's counts with it, and a process under two roots once.
#[test]
fn the_trees_of_several_roots_count_each_process_once() {
    let listed = |pid, ppid, cpu| Process {
        pid,
        ppid,
        elapsed_secs: 1.0,
        cpu_seconds: cpu,
        rss_bytes: 1024,
    };
    let processes = [
        listed(100, 1, 1.0),
        listed(101, 100, 2.0),
        listed(300, 1, 4.0),
        listed(301, 300, 8.0),
        listed(500, 1, 16.0),
    ];
    assert_eq!(
        tree(&processes, &[100, 300, 301]),
        [100, 101, 300, 301].into()
    );
    let mut meter = ProcessMeter::default();
    assert_eq!(
        meter.window(&processes, &[100, 300, 301], 30.0),
        (Some(15.0), Some(4096))
    );
}
