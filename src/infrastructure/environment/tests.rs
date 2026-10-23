use super::conformance::{self, Fixture};
use super::*;
use std::sync::Arc;

/// A process of the fixture's `ps`.
#[derive(Debug, Clone, Copy)]
struct Listed {
    ppid: u32,
    elapsed_secs: f64,
    cpu_seconds: f64,
    rss_bytes: i64,
}

type Table = Arc<Mutex<BTreeMap<u32, Listed>>>;

/// `ps -A -o pid=,ppid=,etime=,time=,rss=`'s text of `table`.
fn listing(table: &Table) -> String {
    table
        .lock()
        .unwrap()
        .iter()
        .map(|(pid, process)| {
            let clock = |secs: f64| format!("{}:{:05.2}", (secs / 60.0).floor(), secs % 60.0);
            format!(
                "{pid:>5} {:>5} {:>8} {:>10} {:>8}\n",
                process.ppid,
                clock(process.elapsed_secs.floor()),
                clock(process.cpu_seconds),
                process.rss_bytes / 1024
            )
        })
        .collect()
}

/// A host of 8 cores, 16 GiB and a CPU pressure of 0.5, whose `ps` lists
/// the fixture's processes.
struct HostFixture {
    table: Table,
    host: HostEnvironment,
}

impl HostFixture {
    fn new() -> Self {
        let table = Table::default();
        let listed = table.clone();
        Self {
            host: HostEnvironment::with(
                Some(8.0),
                Some(1 << 34),
                move || Some(listing(&listed)),
                || Some(0.5),
            ),
            table,
        }
    }
}

impl Fixture for HostFixture {
    fn environment(&self) -> &dyn ExecutionEnvironment {
        &self.host
    }
    fn start(&self, pid: u32, parent: Option<u32>) {
        self.table.lock().unwrap().insert(
            pid,
            Listed {
                ppid: parent.unwrap_or(1),
                elapsed_secs: 0.0,
                cpu_seconds: 0.0,
                rss_bytes: 0,
            },
        );
    }
    fn spend(&self, pid: u32, cpu_seconds: f64, rss_bytes: i64) {
        let mut table = self.table.lock().unwrap();
        let process = table.get_mut(&pid).unwrap();
        process.cpu_seconds += cpu_seconds;
        process.rss_bytes = rss_bytes;
    }
    fn end(&self, pid: u32) {
        self.table.lock().unwrap().remove(&pid);
    }
    fn advance(&self, secs: f64) {
        for process in self.table.lock().unwrap().values_mut() {
            process.elapsed_secs += secs;
        }
    }
    fn node_sample(&self, cores: f64) -> HostSample {
        let mut sample = HostSample::new(0);
        sample.set("cpu_total", Some(cores * 100.0));
        sample.set("mem_used_mb", Some(1024.0));
        sample
    }
}

/// The host passes the suite every environment's adapter passes
/// (ADR-t1662-3 decision 6).
#[test]
fn the_host_passes_the_conformance_suite_of_every_environment() {
    conformance::conforms(&HostFixture::new());
}

/// The host names itself as the host: the node of the NodeSampleStore's
/// rows of the host, no image, the cores and memory it was given and its
/// architecture; measured from outside, with no wait added, and its node
/// with the CPU pressure it reads, its memory used and nothing throttled.
#[test]
fn the_host_names_itself_with_every_item_of_its_attachment() {
    let fixture = HostFixture::new();
    let environment = fixture.environment();
    assert_eq!(
        environment.attached(),
        EnvironmentAttached {
            env_kind: EnvKind::Host,
            node_id: HOST_NODE.to_owned(),
            image_digest: None,
            cpu_limit: Some(8.0),
            mem_limit: Some(1 << 34),
            arch: std::env::consts::ARCH.to_owned(),
        }
    );
    assert!(environment.measures_from_outside());
    assert!(environment.waits().is_empty());
    let node = environment.node_resources(&fixture.node_sample(1.0), Duration::from_secs(30));
    assert_eq!(
        node,
        Resources {
            cpu_seconds: Some(30.0),
            cpu_limit: Some(8.0),
            memory_max_bytes: Some(1 << 30),
            throttled_seconds: None,
            cpu_pressure: Some(0.5),
            load_average: None,
        }
    );
}

/// A run of processes that started before its window (one adopted from
/// another supervisor) counts them from its first measure, and `ps` that
/// fails fails the measure.
#[test]
fn a_run_of_processes_older_than_its_window_counts_them_from_its_first_measure() {
    let fixture = HostFixture::new();
    fixture.start(100, None);
    fixture.spend(100, 50.0, 2048 * 1024);
    fixture.advance(600.0);
    let a = RunId::new("00000001-1111-4111-8111-111111111111").unwrap();
    let asked = RunProcesses {
        run: a.clone(),
        pids: vec![100],
        window_secs: 30.0,
    };
    let first = fixture
        .host
        .measure_runs(std::slice::from_ref(&asked))
        .unwrap();
    assert_eq!(first[0].1.cpu_seconds, Some(0.0));
    fixture.spend(100, 4.5, 2048 * 1024);
    let next = fixture
        .host
        .measure_runs(std::slice::from_ref(&asked))
        .unwrap();
    assert_eq!(next[0].1.cpu_seconds, Some(4.5));
    let failing = HostEnvironment::with(Some(8.0), None, || None, || None);
    assert!(failing.measure_runs(&[asked]).is_err());
}

/// An environment of the shape a container's or a cluster's adapter has:
/// an image, limits of its own, waits it adds, and no measure of a run
/// from outside (its executor measures from inside). The suite holds it
/// to what it holds the host to, so an adapter of that shape goes in
/// once it passes.
struct InsideOnly;

impl ExecutionEnvironment for InsideOnly {
    fn attached(&self) -> EnvironmentAttached {
        EnvironmentAttached {
            env_kind: EnvKind::Container,
            node_id: "node-a".to_owned(),
            image_digest: Some("sha256:0123".to_owned()),
            cpu_limit: Some(2.0),
            mem_limit: Some(1 << 32),
            arch: "aarch64".to_owned(),
        }
    }
    fn measures_from_outside(&self) -> bool {
        false
    }
    fn waits(&self) -> &'static [EnvironmentWait] {
        &[
            EnvironmentWait::Placement,
            EnvironmentWait::ImagePull,
            EnvironmentWait::Preparation,
            EnvironmentWait::ResultReturn,
        ]
    }
    fn measure_runs(&self, _: &[RunProcesses]) -> Result<Vec<(RunId, Resources)>> {
        Ok(Vec::new())
    }
    fn forget_run(&self, _: &RunId) {}
    fn node_resources(&self, sample: &HostSample, interval: Duration) -> Resources {
        Resources {
            cpu_seconds: sample
                .values
                .get("cpu_total")
                .map(|percent| percent / 100.0 * interval.as_secs_f64()),
            cpu_limit: Some(2.0),
            ..Resources::default()
        }
    }
}

impl Fixture for InsideOnly {
    fn environment(&self) -> &dyn ExecutionEnvironment {
        self
    }
    fn start(&self, _: u32, _: Option<u32>) {}
    fn spend(&self, _: u32, _: f64, _: i64) {}
    fn end(&self, _: u32) {}
    fn advance(&self, _: f64) {}
    fn node_sample(&self, cores: f64) -> HostSample {
        let mut sample = HostSample::new(0);
        sample.set("cpu_total", Some(cores * 100.0));
        sample
    }
}

/// An adapter that measures runs only from inside and adds every wait
/// passes the same suite: its runs are not measured from outside, and
/// each wait is recorded as a phase of `blocker: infra`.
#[test]
fn an_environment_measured_from_inside_with_waits_passes_the_same_suite() {
    conformance::conforms(&InsideOnly);
}
