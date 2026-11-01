//! The queue's ledger built as the supervisor's pass builds it, for a test
//! that reads `stats`, `kpi` or `report` (which read the ledger alone)
//! with no supervisor pass after what it recorded: first what the pass
//! records for the ledger besides the events (the attributes' and the
//! findings' baseline, main's history of the repository the queue is bound
//! to, one record of `stats`' judgments of now, the host's CSV files
//! imported into the node samples), then the rows.

use std::path::Path;

use dagq::application::supervise::{Retain, build_pass};
use dagq::domain::ledger::builder::Builder;
use dagq::domain::stats::StatsQuery;
use dagq::infrastructure::sqlite::SqliteQueue;

/// Build the ledger of the queue at `db` now, pass after pass until no
/// mark is left.
pub fn build(db: &Path) {
    let queue = SqliteQueue::open(db).unwrap();
    import_host_files(&queue, db);
    build_queue(&queue);
}

/// [`build`] on `queue`, already open.
pub fn build_queue(queue: &SqliteQueue) {
    queue.record_attributes_baseline().unwrap();
    queue.record_findings_baseline().unwrap();
    record_main_history(queue);
    record_judgments(queue, false);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut builder = Builder::default();
    for _ in 0..100 {
        let built = build_pass(
            queue,
            &mut builder,
            (now_ms, "built"),
            Retain::default(),
            None,
        );
        assert_eq!(built.failure, None);
        if built.backlog == 0 {
            return;
        }
    }
    panic!("the ledger was not built in 100 passes");
}

/// Whether the command `args` reads the ledger: `stats`, `kpi` and
/// `report` do, `stats --ledger` (the parallel run) and `--help` do not.
pub fn reads_ledger(args: &[&str]) -> bool {
    matches!(args.first(), Some(&("stats" | "kpi" | "report")))
        && !args
            .iter()
            .any(|arg| matches!(*arg, "--ledger" | "--help" | "-h"))
}

/// [`build`] before the command `args` when it [`reads_ledger`], on a
/// queue the binary opens as it is: one that does not exist or that this
/// binary cannot open (an older schema a test migrates) is left alone.
pub fn build_before(db: &Path, args: &[&str]) {
    if !reads_ledger(args) || !db.exists() {
        return;
    }
    if let Ok(queue) = SqliteQueue::open(db) {
        import_host_files(&queue, db);
        build_queue(&queue);
    }
}

/// Record `stats`' judgments of now again at the queue at `db` (what a
/// process did, which no event tells, moved them), then [`build`].
pub fn observe_again(db: &Path) {
    record_judgments(&SqliteQueue::open(db).unwrap(), true);
    build(db);
}

/// `dagq::runtime::stats` of the queue at `db` after [`build`].
pub fn stats(db: &Path, query: &StatsQuery) -> anyhow::Result<serde_json::Value> {
    build(db);
    dagq::runtime::stats(db, query)
}

/// Import the host's CSV files beside the queue (`<queue dir>/host`) into
/// its node samples, as the supervisor's sample job imports them: each
/// file whole, a second the store has not added twice.
pub fn import_host_files(queue: &SqliteQueue, db: &Path) {
    use dagq::application::NodeSampleStore;
    use dagq::domain::host_metrics::{HOST_DIR, HOST_NODE, file_day};
    let dir = db.parent().unwrap().join(HOST_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = file_day(&name).filter(|_| entry.path().is_file()) else {
            continue;
        };
        let bytes = entry.metadata().unwrap().len();
        let samples = dagq::infrastructure::host_metrics::read_file(&dir, &name).unwrap();
        queue
            .import_node_samples(HOST_NODE, &name, day, &samples, bytes)
            .unwrap();
    }
}

/// Record what moved of main's history of the repository the queue is
/// bound to, as the supervisor's pass does; nothing for a queue bound to
/// none or whose repository is gone.
fn record_main_history(queue: &SqliteQueue) {
    let Some(common_dir) = queue.repository_binding().unwrap() else {
        return;
    };
    let Some(checkout) =
        dagq::infrastructure::adapters::main_checkout_from_files(Path::new(&common_dir))
    else {
        return;
    };
    let Ok(repository) = dagq::infrastructure::adapters::GitRepository::inspect(&checkout) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    dagq::application::supervise::look_at_main(queue, now, &repository).unwrap();
}

/// Record one observation of `stats`' judgments of now, as a supervisor's
/// pass records its stream, in a stream of its own each call (a fresh
/// recorder writes its baseline), so a stream of a supervisor that runs
/// is left as it is and the latest observation is this one. Only where a
/// supervisor would record one: one is registered or a run is active; a
/// queue with neither records nothing, nor does one whose latest event is
/// this record (nothing moved since), so a read again reads the same.
const STREAM: &str = "ledger-built-by-the-test";

fn record_judgments(queue: &SqliteQueue, again: bool) {
    if queue.supervisors().unwrap().is_empty() && queue.active_runs().unwrap().is_empty() {
        return;
    }
    let kinds = dagq::domain::live_alerts::RECORD_KINDS.map(|kind| kind.as_str());
    let latest = queue.latest_event_id().unwrap();
    if !again
        && queue
            .latest_queue_event(&kinds)
            .unwrap()
            .is_some_and(|event| {
                event.id == latest
                    && event.payload["supervisor"]
                        .as_str()
                        .is_some_and(|stream| stream.starts_with(STREAM))
            })
    {
        return;
    }
    use dagq::application::live_inputs::{LiveSources, carried_live_inputs};
    use dagq::domain::live_alerts::{LiveHistory, LiveRecorder, Observation, judge};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let no_file = || Ok(None);
    let signals = dagq::infrastructure::adapters::ClaudeCode {
        executable: std::path::PathBuf::from("claude"),
    };
    let sources = LiveSources {
        files: &dagq::infrastructure::run_files::LocalRunFiles,
        signals: &signals,
        config_file: &no_file,
    };
    let (inputs, events) = carried_live_inputs(
        queue,
        &dagq::infrastructure::adapters::SystemProcesses,
        now,
        &mut LiveHistory::default(),
        &sources,
    )
    .unwrap();
    let alerts = judge(&events, now, &inputs).keys();
    let stream = format!(
        "{STREAM}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let records = LiveRecorder::default().observe(
        &stream,
        now * 1000,
        Observation::Seen {
            inputs: Box::new(inputs),
            alerts,
        },
    );
    for (kind, payload) in records {
        queue.record_queue_event(kind, payload).unwrap();
    }
}
