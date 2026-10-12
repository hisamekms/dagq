//! The NodeSampleStore in queue.db (ADR-t1662-2 decision 5) and its
//! wiring with the host's CSV files: one row per node and second, the
//! retention, the record of one sample by two supervisors, and the import
//! of the CSV files the retention keeps. The decisions (the half interval,
//! the retention's days, the order of the import) are the unit tests of
//! `application::supervise::host_metrics` and `domain::host_metrics`.
use dagq::{
    application::{NodeSampleStore, supervise::HostMetricsPort},
    domain::host_metrics::{
        self, DiskSpace, HOST_NODE, HostSample, NodeSample, SampleSource, header, value_columns,
    },
    infrastructure::{
        clock,
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
        files: Arc::new(HostMetricsDir(db.parent().unwrap().join("host"))),
        store: Arc::new(SqlitePorts {
            opener: SqliteOpener {
                db: db.to_path_buf(),
                generators: clock::system(),
                actor: None,
            },
            keep: |queue| -> Box<dyn NodeSampleStore + Send> { Box::new(queue) },
        }),
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
            .record_node_sample(node, day, sample, SampleSource::Sampled, apart)
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
