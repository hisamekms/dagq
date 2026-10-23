//! The conformance suite every adapter of [`ExecutionEnvironment`] passes
//! (ADR-t1662-3 decision 6): what the supervisor and the ledger rely on of
//! an environment, whatever it is. An adapter's tests give [`conforms`] a
//! [`Fixture`] that drives the processes the adapter measures by values,
//! so the suite starts no process and reads no OS.

use crate::application::ExecutionEnvironment;
use crate::domain::RunId;
use crate::domain::environment::{EnvironmentAttached, Resources, RunProcesses};
use crate::domain::host_metrics::HostSample;
use crate::domain::run_phase::{Blocker, Phase};
use std::collections::BTreeSet;
use std::time::Duration;

/// What an adapter's tests give the suite: the adapter, measuring the
/// processes the fixture starts, makes spend and ends, on the fixture's
/// clock.
pub trait Fixture {
    /// The adapter under test.
    fn environment(&self) -> &dyn ExecutionEnvironment;
    /// Start the process `pid`, under `parent` or as a root.
    fn start(&self, pid: u32, parent: Option<u32>);
    /// Make `pid` spend `cpu_seconds` more and hold `rss_bytes`.
    fn spend(&self, pid: u32, cpu_seconds: f64, rss_bytes: i64);
    /// End `pid`.
    fn end(&self, pid: u32);
    /// Let `secs` pass.
    fn advance(&self, secs: f64);
    /// A sample of the node's load with `cores` busy, as the adapter's
    /// measure of the node reads one.
    fn node_sample(&self, cores: f64) -> HostSample;
}

const WINDOW_SECS: f64 = 30.0;
const MIB: i64 = 1 << 20;

fn run(n: u8) -> RunId {
    RunId::new(format!("{n:08x}-2222-4222-8222-222222222222")).unwrap()
}

fn asked(run: &RunId, pids: &[u32]) -> RunProcesses {
    RunProcesses {
        run: run.clone(),
        pids: pids.to_vec(),
        window_secs: WINDOW_SECS,
    }
}

fn close(value: Option<f64>, expected: f64) -> bool {
    value.is_some_and(|value| (value - expected).abs() < 1e-6)
}

/// Run the whole suite on `fixture`'s adapter.
pub fn conforms(fixture: &dyn Fixture) {
    let attached = names_itself(fixture.environment());
    adds_waits_as_phases_of_infra(fixture.environment());
    measures_the_node_in_the_common_units(fixture, &attached);
    measures_runs(fixture, &attached);
}

/// It names itself the same each time, with a node, an architecture,
/// limits of a positive value where it has them and an image digest of
/// one where it has an image, and its payload reads back as it.
fn names_itself(environment: &dyn ExecutionEnvironment) -> EnvironmentAttached {
    let attached = environment.attached();
    assert_eq!(environment.attached(), attached);
    assert!(!attached.node_id.is_empty(), "{attached:?}");
    assert!(!attached.arch.is_empty(), "{attached:?}");
    assert!(
        attached
            .cpu_limit
            .is_none_or(|cores| cores.is_finite() && cores > 0.0),
        "{attached:?}"
    );
    assert!(
        attached.mem_limit.is_none_or(|bytes| bytes > 0),
        "{attached:?}"
    );
    assert!(
        attached
            .image_digest
            .as_ref()
            .is_none_or(|digest| !digest.is_empty()),
        "{attached:?}"
    );
    assert_eq!(
        EnvironmentAttached::of_payload(&attached.payload()),
        Some(attached.clone())
    );
    attached
}

/// Each wait it adds is a phase of its own of `blocker: infra`.
fn adds_waits_as_phases_of_infra(environment: &dyn ExecutionEnvironment) {
    let waits = environment.waits();
    let names: BTreeSet<&str> = waits.iter().map(|wait| wait.phase_name()).collect();
    assert_eq!(names.len(), waits.len(), "{waits:?}");
    for wait in waits {
        let phase = Phase::Environment(*wait);
        assert!(matches!(phase.tags().blocker, Blocker::Infra), "{wait:?}");
    }
}

/// The node's CPU seconds are its busy cores over the time the sample
/// stands for, under the limit the environment named.
fn measures_the_node_in_the_common_units(fixture: &dyn Fixture, attached: &EnvironmentAttached) {
    let environment = fixture.environment();
    let sample = fixture.node_sample(2.0);
    let over = |secs| environment.node_resources(&sample, Duration::from_secs(secs));
    let (half, whole) = (over(30), over(60));
    assert!(close(half.cpu_seconds, 60.0), "{half:?}");
    assert!(close(whole.cpu_seconds, 120.0), "{whole:?}");
    assert_eq!(half.cpu_limit, attached.cpu_limit);
    in_the_common_units(&half);
}

/// Values of the common units are never negative.
fn in_the_common_units(resources: &Resources) {
    for value in [
        resources.cpu_seconds,
        resources.cpu_limit,
        resources.throttled_seconds,
        resources.cpu_pressure,
        resources.load_average,
    ]
    .into_iter()
    .flatten()
    {
        assert!(value.is_finite() && value >= 0.0, "{resources:?}");
    }
    assert!(
        resources.memory_max_bytes.is_none_or(|bytes| bytes >= 0),
        "{resources:?}"
    );
}

/// A run is measured by its tree of processes in each window: the CPU
/// spent since its last window and the memory it holds, apart from
/// another run and from processes of no run; a run with no process is
/// not measured, nor is any by one that measures only from inside; once
/// forgotten, what a run's processes spent before is not counted again.
fn measures_runs(fixture: &dyn Fixture, attached: &EnvironmentAttached) {
    let environment = fixture.environment();
    assert!(environment.measure_runs(&[]).unwrap().is_empty());
    let (a, b, gone) = (run(1), run(2), run(3));
    fixture.start(100, None);
    fixture.start(101, Some(100));
    fixture.start(200, None);
    fixture.start(300, None);
    fixture.spend(100, 2.0, MIB);
    fixture.spend(101, 3.0, 2 * MIB);
    fixture.spend(200, 5.0, 4 * MIB);
    fixture.spend(300, 7.0, 8 * MIB);
    fixture.advance(10.0);
    let runs = [asked(&a, &[100]), asked(&b, &[200]), asked(&gone, &[999])];
    let first = environment.measure_runs(&runs).unwrap();
    if !environment.measures_from_outside() {
        assert!(first.is_empty(), "{first:?}");
        return;
    }
    let of = |measured: &[(RunId, Resources)], run: &RunId| {
        measured
            .iter()
            .find(|(id, _)| id == run)
            .map(|(_, resources)| resources.clone())
    };
    assert!(of(&first, &gone).is_none(), "{first:?}");
    let (first_a, first_b) = (of(&first, &a).unwrap(), of(&first, &b).unwrap());
    assert!(close(first_a.cpu_seconds, 5.0), "{first_a:?}");
    assert_eq!(first_a.memory_max_bytes, Some(3 * MIB));
    assert_eq!(first_a.cpu_limit, attached.cpu_limit);
    assert!(close(first_b.cpu_seconds, 5.0), "{first_b:?}");
    assert_eq!(first_b.memory_max_bytes, Some(4 * MIB));
    in_the_common_units(&first_a);

    // The next window: what the tree spent since, a process started under
    // it included.
    fixture.advance(WINDOW_SECS);
    fixture.spend(101, 1.5, 2 * MIB);
    fixture.start(102, Some(101));
    fixture.spend(102, 0.5, MIB);
    let next = environment.measure_runs(&runs[..2]).unwrap();
    let (next_a, next_b) = (of(&next, &a).unwrap(), of(&next, &b).unwrap());
    assert!(close(next_a.cpu_seconds, 2.0), "{next_a:?}");
    assert_eq!(next_a.memory_max_bytes, Some(4 * MIB));
    assert!(close(next_b.cpu_seconds, 0.0), "{next_b:?}");

    // b's processes ended: not measured.
    fixture.end(200);
    fixture.advance(WINDOW_SECS);
    let ended = environment.measure_runs(&runs[..2]).unwrap();
    assert!(of(&ended, &b).is_none(), "{ended:?}");

    // Forgotten: what a's processes spent before is not counted again.
    environment.forget_run(&a);
    fixture.advance(WINDOW_SECS);
    fixture.spend(101, 1.0, 2 * MIB);
    let again = environment.measure_runs(&runs[..1]).unwrap();
    let again_a = of(&again, &a).unwrap();
    assert!(
        again_a
            .cpu_seconds
            .is_some_and(|cpu| (0.0..=1.0 + 1e-6).contains(&cpu)),
        "{again_a:?}"
    );
}
