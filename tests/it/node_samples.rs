//! The NodeSampleStore in queue.db (ADR-t1662-2 decision 5) and its
//! wiring with the host's CSV files: one row per node and second, the
//! retention, the record of one sample by two supervisors, and the import
//! of the CSV files the retention keeps. The decisions (the half interval,
//! the retention's days, the order of the import) are the unit tests of
//! `application::supervise::host_metrics` and `domain::host_metrics`.
use dagq::domain::environment::Resources;
use dagq::{
    application::{NodeSampleStore, supervise::HostMetricsPort},
    domain::host_metrics::{
        self, DiskSpace, HOST_NODE, HostSample, NodeSample, SampleSource, header, value_columns,
    },
    infrastructure::{
        clock,
        environment::HostEnvironment,
        host_metrics::{HostMetricsDir, read_file, summary},
        runtime_store::{SqliteOpener, SqlitePorts},
        sqlite::SqliteQueue,
    },
};
use std::{fs, path::Path, sync::Arc, time::Duration};

const DAY: i64 = 86_400;
/// Noon of a day, in UTC.
const NOW: i64 = 20_500 * DAY + DAY / 2;

/// A sample with every column of the files, each its own value.
fn full(unix: i64) -> HostSample {
    let mut sample = HostSample::new(unix);
    for (index, column) in value_columns().iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        sample.set(column, Some(index as f64 + 0.25));
    }
    sample
}

/// A sample with the CPU, the memory and the disk, in values a file keeps
/// as they are (two decimals).
fn busy(unix: i64) -> HostSample {
    #[allow(clippy::cast_precision_loss)]
    let step = (unix % 600) as f64 / 4.0;
    let mut sample = HostSample::new(unix)
        .with_loads([Some(1.25 + step), Some(2.0), Some(3.5)])
        .with_disk(Some(DiskSpace {
            free_bytes: 3 << 30,
            total_bytes: 12 << 30,
        }));
    sample.set("cpu_total", Some(150.5 + step));
    sample.set("cpu_rustc", Some(100.0));
    sample.set("mem_used_mb", Some(8192.0));
    sample.set("pageouts", Some(1000.0 + step));
    sample
}

fn rows(queue: &SqliteQueue) -> Vec<NodeSample> {
    queue.node_samples(HOST_NODE, 0, i64::MAX).unwrap()
}

/// The supervisor's port over the queue at `db` and its `host/`, sampling
/// `take` every 30 s at the offset 0.
fn port(db: &Path, take: fn(i64) -> HostSample, retention_days: u32) -> HostMetricsPort {
    HostMetricsPort {
        interval: Duration::from_secs(30),
        retention_days,
        sample: Arc::new(take),
        utc_offset: |_| 0,
        held_targets: None,
        files: Arc::new(HostMetricsDir(db.parent().unwrap().join("host"))),
        store: Arc::new(SqlitePorts {
            opener: SqliteOpener {
                db: db.to_path_buf(),
                generators: clock::system(),
                actor: None,
            },
            keep: |queue| -> Box<dyn NodeSampleStore + Send> { Box::new(queue) },
        }),
        environment: Arc::new(HostEnvironment::with(
            Some(8.0),
            Some(1 << 34),
            || None,
            || None,
        )),
    }
}

/// Every column of the files reads back as written and a value missing as
/// missing, with its local day; rows are one per node and second: a sample
/// at a second recorded adds nothing, nor does one of the same local day
/// less than `apart` from it, but one of another day as near does, and
/// one of another node; an import adds a file's rows however near each
/// other but none at a second recorded. The retention removes the rows and
/// the marks of the days before its day, whatever their seconds, and the
/// ids go on past the rows removed.
#[test]
fn the_store_keeps_one_row_per_node_and_second_and_its_retention() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let (at, day) = (NOW, NOW / DAY);
    let record = |node: &str, day: i64, sample: &HostSample, apart: i64| {
        queue
            .record_node_sample(
                node,
                day,
                sample,
                &|_| Resources::default(),
                SampleSource::Sampled,
                apart,
            )
            .unwrap()
    };
    assert!(record(HOST_NODE, day, &full(at), 15));
    assert!(!record(HOST_NODE, day, &full(at), 0));
    let mut sparse = HostSample::new(at + 14);
    sparse.set("load1", Some(2.0));
    assert!(!record(HOST_NODE, day, &sparse, 15));
    // The same second on the day before (a clock gone back): still kept
    // out by the second.
    assert!(!record(HOST_NODE, day - 1, &full(at), 15));
    // 14 s later but on the day before: a row of its own day only.
    assert!(record(HOST_NODE, day - 1, &sparse, 15));
    sparse.unix = at + 15;
    assert!(record(HOST_NODE, day, &sparse, 15));
    assert!(record("other", day, &full(at), 15));
    // A file's rows 5 s apart all come in; its row at a second recorded
    // does not.
    assert_eq!(
        queue
            .import_node_samples(
                HOST_NODE,
                "metrics-x.csv",
                day - 2,
                &[full(at), full(at + 5), full(at + 10)],
                300
            )
            .unwrap(),
        2
    );
    let read = rows(&queue);
    assert_eq!(
        read.iter()
            .map(|row| (row.sample.unix, row.local_day, row.source))
            .collect::<Vec<_>>(),
        [
            (at, day, SampleSource::Sampled),
            (at + 5, day - 2, SampleSource::CsvImport),
            (at + 10, day - 2, SampleSource::CsvImport),
            (at + 14, day - 1, SampleSource::Sampled),
            (at + 15, day, SampleSource::Sampled),
        ]
    );
    assert_eq!(read[0].sample, full(at));
    assert_eq!(read[4].sample, sparse);
    assert!(read.windows(2).all(|pair| pair[0].id != pair[1].id));

    queue
        .import_node_samples(HOST_NODE, "metrics-y.csv", day, &[], 10)
        .unwrap();
    // Every node's rows of the days before go, whatever their seconds.
    assert_eq!(queue.remove_node_samples(day).unwrap(), 3);
    assert_eq!(
        rows(&queue)
            .iter()
            .map(|row| row.sample.unix)
            .collect::<Vec<_>>(),
        [at, at + 15]
    );
    assert_eq!(
        queue.imported_files(HOST_NODE).unwrap(),
        [("metrics-y.csv".to_owned(), 10)].into()
    );
    let last = read.iter().map(|row| row.id).max().unwrap();
    assert!(record(HOST_NODE, day, &full(at + 100), 0));
    assert!(rows(&queue).last().unwrap().id > last);
}

/// Two supervisors of a queue sampling 10 s apart write one row to the
/// store and the file, and one 30 s later a row to each; the store keeps
/// the retention's local days, and every day with zero days.
#[test]
fn two_supervisors_write_one_row_to_the_store_and_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    queue
        .record_node_sample(
            HOST_NODE,
            NOW / DAY - 3,
            &busy(NOW - 3 * DAY),
            &|_| Resources::default(),
            SampleSource::Sampled,
            0,
        )
        .unwrap();
    let (first, second) = (port(&db, busy, 2), port(&db, busy, 2));
    assert!(first.record(NOW, false).failures.is_empty());
    assert!(second.record(NOW + 10, false).failures.is_empty());
    assert!(second.record(NOW + 30, false).failures.is_empty());
    let read = rows(&queue);
    assert_eq!(
        read.iter().map(|row| row.sample.unix).collect::<Vec<_>>(),
        [NOW, NOW + 30]
    );
    assert!(read.iter().all(|row| row.source == SampleSource::Sampled));
    let host = dir.path().join("host");
    let file = read_file(&host, &host_metrics::file_name(NOW / DAY)).unwrap();
    assert_eq!(file, [busy(NOW), busy(NOW + 30)]);

    queue
        .record_node_sample(
            HOST_NODE,
            NOW / DAY - 3,
            &busy(NOW - 3 * DAY),
            &|_| Resources::default(),
            SampleSource::Sampled,
            0,
        )
        .unwrap();
    assert!(
        port(&db, busy, 0)
            .record(NOW + 60, false)
            .failures
            .is_empty()
    );
    assert_eq!(rows(&queue).len(), 4);
}

/// The files the retention keeps are imported once as `csv_import`: a day
/// whose binary changed its columns (two headers), rows 10 s apart, a row
/// without a unix second, a second the store has (a sample written to
/// both); a file past the retention is not. The import comes before the
/// sample, which a file's row 10 s before keeps out of the store as out
/// of the file. A second import adds no row, and the store's rows then
/// summarize as the files do, the CPU seconds and the disk too.
#[test]
fn the_kept_files_are_imported_once_and_summarize_as_the_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let host = dir.path().join("host");
    fs::create_dir_all(&host).unwrap();
    let today = NOW / DAY;
    let yesterday = NOW - DAY;
    fs::write(
        host.join(host_metrics::file_name(today - 1)),
        format!(
            "time,unix,load1,cpu_total\nx,{},1.5,120\nx,,9,9\nx,{},2.5,130\n{}\n{}\n{}\n{}\n",
            yesterday,
            yesterday + 30,
            header(),
            busy(yesterday + 60).row(0),
            busy(yesterday + 70).row(0),
            busy(yesterday + 80).row(0),
        ),
    )
    .unwrap();
    // A line two supervisors wrote over each other, and a row 10 s before
    // the sample.
    fs::write(
        host.join(host_metrics::file_name(today)),
        format!(
            "{}\n{}\n,,1\n{}\n{}\n",
            header(),
            busy(NOW - 60).row(0),
            busy(NOW - 30).row(0),
            busy(NOW - 10).row(0),
        ),
    )
    .unwrap();
    fs::write(
        host.join(host_metrics::file_name(today - 2)),
        format!("{}\n{}\n", header(), busy(NOW - 2 * DAY).row(0)),
    )
    .unwrap();
    queue
        .record_node_sample(
            HOST_NODE,
            NOW / DAY,
            &busy(NOW - 30),
            &|_| Resources::default(),
            SampleSource::Sampled,
            15,
        )
        .unwrap();

    let port = port(&db, busy, 2);
    let first = port.record(NOW, true);
    assert!(first.failures.is_empty(), "{first:?}");
    assert_eq!(first.imported, Some(7));
    let read = rows(&queue);
    assert_eq!(
        read.iter()
            .map(|row| (row.sample.unix, row.source))
            .collect::<Vec<_>>(),
        [
            (yesterday, SampleSource::CsvImport),
            (yesterday + 30, SampleSource::CsvImport),
            (yesterday + 60, SampleSource::CsvImport),
            (yesterday + 70, SampleSource::CsvImport),
            (yesterday + 80, SampleSource::CsvImport),
            (NOW - 60, SampleSource::CsvImport),
            (NOW - 30, SampleSource::Sampled),
            (NOW - 10, SampleSource::CsvImport),
        ]
    );
    assert_eq!(read[0].sample.values.len(), 2);
    assert_eq!(read[2].sample, busy(yesterday + 60));
    assert_eq!(port.record(NOW + 30, true).imported, Some(0));
    assert_eq!(rows(&queue).len(), read.len() + 1);
    // Each file read is marked with its length; yesterday's has not grown.
    let marks = queue.imported_files(HOST_NODE).unwrap();
    let yesterdays = host_metrics::file_name(today - 1);
    assert_eq!(
        marks.keys().cloned().collect::<Vec<_>>(),
        [yesterdays.clone(), host_metrics::file_name(today)]
    );
    assert_eq!(
        marks[&yesterdays],
        fs::metadata(host.join(&yesterdays)).unwrap().len()
    );
    assert!(!host.join(host_metrics::file_name(today - 2)).exists());

    let (from, until) = (yesterday - 10, NOW + 30);
    let stored: Vec<HostSample> = rows(&queue).into_iter().map(|row| row.sample).collect();
    let files = summary(&host, from, until);
    assert_eq!(host_metrics::summarize(&stored, from, until), files);
    assert_eq!(files.samples, 9);
    assert!(files.cpu_secs.is_some());
    assert!(host_metrics::DiskFree::of(&files).is_some());
}

/// Two supervisors sampling 23:59:50 and 00:00:00 of the host's local
/// time, each importing as it starts, and a third process starting after:
/// the new day's first sample is in its new file and in the store alike,
/// and the store's rows summarize as the files do.
#[test]
fn two_supervisors_across_the_local_midnight_keep_the_same_samples() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let midnight = 20_500 * DAY - 9 * 3600;
    let local = || HostMetricsPort {
        utc_offset: |_| 9 * 3600,
        ..port(&db, busy, 2)
    };
    for at in [midnight - 10, midnight, midnight + 30] {
        let recorded = local().record(at, true);
        assert!(recorded.failures.is_empty(), "{recorded:?}");
        assert_eq!(recorded.imported, Some(0), "{recorded:?}");
    }
    let stored: Vec<HostSample> = rows(&queue).into_iter().map(|row| row.sample).collect();
    assert_eq!(
        stored,
        [busy(midnight - 10), busy(midnight), busy(midnight + 30)]
    );
    let host = dir.path().join("host");
    let (from, until) = (midnight - DAY, midnight + DAY);
    let files = summary(&host, from, until);
    assert_eq!(files.samples, 3);
    assert_eq!(host_metrics::summarize(&stored, from, until), files);
}

/// A sample of the host's load average and `ps`'s CPU as they come, of
/// more than two decimals.
fn fine(unix: i64) -> HostSample {
    let load = [1.234, 1.234, 1.244][usize::try_from(unix / 30 % 3).unwrap()];
    let mut sample = HostSample::new(unix)
        .with_loads([Some(load), Some(2.0049), None])
        .with_disk(Some(DiskSpace {
            free_bytes: 3_333_333_333,
            total_bytes: 12 << 30,
        }));
    sample.set("cpu_total", Some(load * 100.0 + 0.0049));
    sample
}

/// Samples of more than two decimals go to the store as the file keeps
/// them: the store's rows read as the file's and summarize alike, the
/// mean of 1.234, 1.234 and 1.244 being 1.23 in both, the CPU seconds and
/// the disk too.
#[test]
fn the_store_keeps_the_values_as_the_file_does() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let port = port(&db, fine, 2);
    for at in [NOW, NOW + 30, NOW + 60] {
        let recorded = port.record(at, false);
        assert!(recorded.failures.is_empty(), "{recorded:?}");
    }
    let stored: Vec<HostSample> = rows(&queue).into_iter().map(|row| row.sample).collect();
    let host = dir.path().join("host");
    assert_eq!(
        stored,
        read_file(&host, &host_metrics::file_name(NOW / DAY)).unwrap()
    );
    let files = summary(&host, NOW, NOW + 60);
    assert_eq!(host_metrics::summarize(&stored, NOW, NOW + 60), files);
    assert_eq!(files.metrics["load1"].unwrap().mean, 1.23);
    assert!(files.cpu_secs.is_some());
    assert!(host_metrics::DiskFree::of(&files).is_some());
}

/// Samples out of the order they were taken (a job that ended late, a
/// clock gone back), 1000, 1030 then 1005 s: the store keeps 1005 out and
/// the file follows it, so the store's rows read as the file's.
#[test]
fn samples_out_of_order_are_kept_alike_in_the_store_and_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let port = port(&db, busy, 2);
    for at in [NOW, NOW + 30, NOW + 5] {
        let recorded = port.record(at, false);
        assert!(recorded.failures.is_empty(), "{recorded:?}");
    }
    let stored: Vec<HostSample> = rows(&queue).into_iter().map(|row| row.sample).collect();
    assert_eq!(stored, [busy(NOW), busy(NOW + 30)]);
    let host = dir.path().join("host");
    assert_eq!(
        read_file(&host, &host_metrics::file_name(NOW / DAY)).unwrap(),
        stored
    );
}

/// A file of more rows than one transaction holds: its import commits them
/// a bounded number at a time and marks the file with the last, so an
/// import stopped part way (a row it cannot add) keeps the batches before
/// it and leaves the file's mark as it was (none, or that of a shorter
/// read before it grew); the next import of the file adds the rest, a row
/// sampled meanwhile at one of their seconds kept as it is, and every row
/// is there once with the values the file kept.
#[test]
fn a_files_import_commits_by_batches_and_the_next_adds_the_rest() {
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    let queue = SqliteQueue::init(&db).unwrap();
    let day = NOW / DAY;
    let file: Vec<HostSample> = (0..i64::try_from(BATCH * 2 + 50).unwrap())
        .map(|n| full(NOW + n * 5))
        .collect();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER stop BEFORE INSERT ON node_samples WHEN new.unix = {}
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
        file[BATCH + 5].unix
    ))
    .unwrap();
    let import = || queue.import_node_samples(HOST_NODE, "metrics-x.csv", day, &file, 900);
    let error = import().unwrap_err();
    assert!(format!("{error:#}").contains("stopped"), "{error:#}");
    assert_eq!(rows(&queue).len(), BATCH);
    assert!(queue.imported_files(HOST_NODE).unwrap().is_empty());
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    let sampled = &file[BATCH + 20];
    assert!(
        queue
            .record_node_sample(
                HOST_NODE,
                day,
                sampled,
                &|_| Resources::default(),
                SampleSource::Sampled,
                0
            )
            .unwrap()
    );
    assert_eq!(import().unwrap(), file.len() - BATCH - 1);
    let read = rows(&queue);
    assert_eq!(
        read.iter()
            .map(|row| row.sample.clone())
            .collect::<Vec<_>>(),
        file
    );
    assert_eq!(
        read.iter()
            .filter(|row| row.source == SampleSource::Sampled)
            .map(|row| row.sample.unix)
            .collect::<Vec<_>>(),
        [sampled.unix]
    );
    assert_eq!(
        queue.imported_files(HOST_NODE).unwrap(),
        [("metrics-x.csv".to_owned(), 900)].into()
    );
    assert_eq!(import().unwrap(), 0);
    assert_eq!(rows(&queue).len(), file.len());
    // The file grown since its import, and the import of it stopped part
    // way again: the file keeps its mark of before, shorter than the file,
    // and the next import adds the rest.
    let grown: Vec<HostSample> = (0..i64::try_from(file.len() + BATCH + 10).unwrap())
        .map(|n| full(NOW + n * 5))
        .collect();
    conn.execute_batch(&format!(
        "CREATE TRIGGER stop BEFORE INSERT ON node_samples WHEN new.unix = {}
         BEGIN SELECT RAISE(ABORT, 'stopped'); END",
        grown[file.len() + 5].unix
    ))
    .unwrap();
    let import = || queue.import_node_samples(HOST_NODE, "metrics-x.csv", day, &grown, 1800);
    assert!(import().is_err());
    assert_eq!(
        queue.imported_files(HOST_NODE).unwrap(),
        [("metrics-x.csv".to_owned(), 900)].into()
    );
    conn.execute_batch("DROP TRIGGER stop").unwrap();
    assert_eq!(import().unwrap(), BATCH + 10);
    assert_eq!(
        rows(&queue)
            .iter()
            .map(|row| row.sample.clone())
            .collect::<Vec<_>>(),
        grown
    );
    assert_eq!(
        queue.imported_files(HOST_NODE).unwrap(),
        [("metrics-x.csv".to_owned(), 1800)].into()
    );
}

/// While a file of many rows is imported, another connection's heartbeat
/// gets the write lock between two of its transactions, within its busy
/// wait: it sees some rows added and some not yet.
#[test]
fn a_heartbeat_writes_while_a_file_is_imported() {
    use crate::common::{self, queue::*};
    use dagq::infrastructure::runtime_store::ROWS_PER_TRANSACTION as BATCH;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.db");
    drop(SqliteQueue::init(&db).unwrap());
    beats(&db);
    let file: Vec<HostSample> = (0..i64::try_from(BATCH * 8 + 10).unwrap())
        .map(|n| full(NOW + n * 5))
        .collect();
    let total = i64::try_from(file.len()).unwrap();
    let heart = heart(&db);
    let job = {
        let (db, file) = (db.clone(), file.clone());
        std::thread::spawn(move || {
            SqliteQueue::open(&db).unwrap().import_node_samples(
                HOST_NODE,
                "metrics-x.csv",
                NOW / DAY,
                &file,
                900,
            )
        })
    };
    let samples = "SELECT count(*) FROM node_samples";
    until_committed(&db, samples, None);
    let (written, took, seen) = beat(&heart, &format!("SELECT ({samples})"));
    let _waiting = common::within(common::STEP_LIMIT, "the import of a file");
    assert_eq!(job.join().unwrap().unwrap(), file.len());
    assert!(written, "the heartbeat was not written in {took:?}");
    assert!(took < dagq::application::QUEUE_BUSY_TIMEOUT, "{took:?}");
    assert!(0 < seen && seen < total, "{seen} of {total}");
}

/// The supervisor's record of the host's load writes, in the same row as
/// the columns, the same sample in the common units as the host's
/// environment measures the node (the CPU seconds over the time since the
/// row before, the cores, the memory used), and reads it back; a row imported from a CSV
/// file has none. The columns, the CSV files, the node rows of the ledger
/// and every summary of the host made of them (`stats`' and `kpi`'s
/// `host`, `cpu_per_landing`'s CPU seconds, `load_per_core`'s load and
/// `health.disk`) are those of the same samples recorded without the
/// common units.
#[test]
fn the_common_units_go_in_the_same_row_and_leave_the_hosts_values_as_they_were() {
    use dagq::domain::environment::host_node_resources;
    use dagq::domain::host_metrics::DiskFree;
    use dagq::domain::ledger::node::{host_summary, node_rows};
    let with = tempfile::tempdir().unwrap();
    let without = tempfile::tempdir().unwrap();
    let (db, plain_db) = (with.path().join("q.db"), without.path().join("q.db"));
    let (queue, plain) = (
        SqliteQueue::init(&db).unwrap(),
        SqliteQueue::init(&plain_db).unwrap(),
    );
    let port = port(&db, busy, 2);
    let times = [NOW, NOW + 30, NOW + 60, NOW + 3600];
    for at in times {
        assert!(port.record(at, false).failures.is_empty());
        assert!(
            plain
                .record_node_sample(
                    HOST_NODE,
                    NOW / DAY,
                    &busy(at).as_written(),
                    &|_| Resources::default(),
                    SampleSource::Sampled,
                    15,
                )
                .unwrap()
        );
    }
    let (rows, plain_rows) = (rows(&queue), rows(&plain));
    assert_eq!(rows.len(), times.len());
    // Each stands for the time since the row before it, up to twice the
    // interval: the first for the interval, the one after the hour 60 s.
    let stands_for = [30.0, 30.0, 30.0, 60.0];
    for ((row, at), secs) in rows.iter().zip(times).zip(stands_for) {
        let mut expected = host_node_resources(&busy(at), secs, Some(8.0), None);
        // The load average is read back from the row's `load1`, as kept.
        expected.load_average = row.sample.values.get("load1").copied();
        assert_eq!(row.resources, expected, "{at}");
        assert!(row.resources.cpu_seconds.is_some_and(|cpu| cpu > 0.0));
        assert_eq!(row.resources.cpu_limit, Some(8.0));
        assert_eq!(row.resources.memory_max_bytes, Some(8192 << 20));
    }
    assert!(plain_rows.iter().all(|row| row.resources.is_empty()));
    let samples = |rows: &[NodeSample]| -> Vec<HostSample> {
        rows.iter().map(|row| row.sample.clone()).collect()
    };
    assert_eq!(samples(&rows), samples(&plain_rows));

    let built = "2030-01-01T00:00:00.000Z";
    let (ledger, plain_ledger) = (node_rows(&rows, built), node_rows(&plain_rows, built));
    assert_eq!(ledger, plain_ledger);
    let (from, until) = (NOW - 10, NOW + 3700);
    let (summary, plain_summary) = (
        host_summary(&ledger, from, until),
        host_summary(&plain_ledger, from, until),
    );
    assert_eq!(summary, plain_summary);
    assert!(summary.cpu_secs.as_ref().is_some_and(|cpu| cpu.total > 0.0));
    assert!(summary.metrics["load1"].is_some());
    assert_eq!(DiskFree::of(&summary), DiskFree::of(&plain_summary));
    assert!(DiskFree::of(&summary).is_some());
    // The CSV copy a person reads is the files' as before.
    let file = read_file(
        &with.path().join("host"),
        &host_metrics::file_name(NOW / DAY),
    )
    .unwrap();
    assert_eq!(file, times.map(busy));
    assert_eq!(
        summary,
        summary_of_files(&with.path().join("host"), from, until)
    );
}

/// The store makes a sample's common units from the node's row before it,
/// read in the write that admits it, whichever supervisor wrote that
/// row: none for the node's first, the row before for the next. A sample
/// written after a later row of the node (it waited on the lock) gets its
/// own second, so it stands for no time the later row counted.
#[test]
fn a_sample_stands_for_the_time_since_the_nodes_row_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
    let day = NOW / DAY;
    let before = |at: i64| {
        let seen = std::cell::Cell::new(None);
        let written = queue
            .record_node_sample(
                HOST_NODE,
                day,
                &busy(at),
                &|previous| {
                    seen.set(Some(previous));
                    Resources::default()
                },
                SampleSource::Sampled,
                15,
            )
            .unwrap();
        assert!(written, "{at}");
        seen.get().unwrap()
    };
    assert_eq!(before(NOW), None);
    assert_eq!(before(NOW + 40), Some(NOW));
    assert_eq!(before(NOW + 20), Some(NOW + 20));
    assert_eq!(before(NOW + 70), Some(NOW + 40));
}

/// The summary of the CSV files over `[from, until]`, as `stats` reads it.
fn summary_of_files(dir: &Path, from: i64, until: i64) -> host_metrics::HostSummary {
    summary(dir, from, until)
}
