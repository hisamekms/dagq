//! The host's load, recorded continuously (task 516): every
//! [`HostMetricsPort::interval`], draining or not, a job thread samples
//! the host once and writes the sample to the NodeSampleStore (the
//! permanent record the measurement reads, ADR-t1662-2 decision 5) and to
//! `<queue dir>/host/metrics-YYYYMMDD.csv` (the copy a person reads), then
//! removes from both what is past the retention
//! ([`crate::domain::host_metrics`]). Nothing is recorded as an event.
//! The first job of the process imports the CSV files the retention keeps
//! into the store, and the next ones until an import succeeds. A failed
//! or panicking job, or one of the two writes failing, is logged (once
//! until a job succeeds again) and stops nothing, nor the other write;
//! one job runs at a time, so a slow tool delays the next sample instead
//! of piling them up.

use super::*;
use crate::application::{HostMetricsFiles, NodeSampleStore};
use crate::domain::host_metrics::{self, HOST_NODE, HostSample, SampleSource};

/// What the supervisor records the host's load with.
#[derive(Clone)]
pub struct HostMetricsPort {
    /// How often a sample is taken.
    pub interval: Duration,
    /// How many local days are kept, today's included; zero removes none.
    pub retention_days: u32,
    /// Takes one sample at the unix second given.
    pub sample: Arc<dyn Fn(i64) -> HostSample + Send + Sync>,
    /// The host's offset from UTC (seconds) at a unix second.
    pub utc_offset: fn(i64) -> i64,
    /// The CSV files, the copy a person reads.
    pub files: Arc<dyn HostMetricsFiles>,
    /// Opens the NodeSampleStore on the job's thread.
    pub store: Arc<dyn QueueOpener<dyn NodeSampleStore + Send>>,
}

/// What one job did ([`HostMetricsPort::record`]).
#[derive(Debug, Default)]
pub struct Recorded {
    /// The CSV files the retention removed.
    pub removed: Vec<PathBuf>,
    /// The rows the import of the CSV files added, when it ran and
    /// succeeded.
    pub imported: Option<usize>,
    /// What could not be written, each its own reason.
    pub failures: Vec<String>,
}

impl HostMetricsPort {
    /// Take one sample at the unix second `now` and write it to the store
    /// (its values as the files keep them, [`HostSample::as_written`]) and
    /// to the files; with `import`, import the CSV files into the store
    /// first. The store decides whether the sample is kept (one decision,
    /// atomic across supervisors), and the files follow that decision, so
    /// both keep the same samples however two supervisors' jobs
    /// interleave; when the store cannot decide (it cannot be opened or
    /// written), the files decide by their last row, and a failure of
    /// either stops neither. The sample is on the host's local day at its
    /// offset now, the day of the file it goes to, which the store keeps
    /// with it.
    pub fn record(&self, now: i64, import: bool) -> Recorded {
        let sample = (self.sample)(now);
        let offset = (self.utc_offset)(now);
        let today = host_metrics::local_day(sample.unix, offset);
        let mut recorded = Recorded::default();
        let mut admitted = None;
        match self.store.open() {
            Ok(store) => {
                if import {
                    match import_files(&*store, &*self.files, today, self.retention_days) {
                        Ok(added) => recorded.imported = Some(added),
                        Err(error) => recorded
                            .failures
                            .push(format!("import the CSV files: {error:#}")),
                    }
                }
                // The values as the file keeps them, so both summarize alike.
                match store.record_node_sample(
                    HOST_NODE,
                    today,
                    &sample.as_written(),
                    SampleSource::Sampled,
                    host_metrics::apart_secs(self.interval),
                ) {
                    Ok(written) => admitted = Some(written),
                    Err(error) => recorded
                        .failures
                        .push(format!("record the sample in the store: {error:#}")),
                }
                if let Some(oldest) = host_metrics::oldest_kept_day(today, self.retention_days)
                    && let Err(error) = store.remove_node_samples(oldest)
                {
                    recorded.failures.push(format!(
                        "remove the store's rows past the retention: {error:#}"
                    ));
                }
            }
            Err(error) => recorded.failures.push(format!("open the store: {error:#}")),
        }
        match self.files.record(
            &sample,
            offset,
            self.interval,
            self.retention_days,
            admitted,
        ) {
            Ok(removed) => recorded.removed = removed,
            Err(error) => recorded
                .failures
                .push(format!("write the CSV file: {error:#}")),
        }
        recorded
    }
}

/// Import into `store` every row of the CSV files the retention keeps on
/// the local day `today` (`csv_import`), each on its file's day: the rows
/// a supervisor kept, however near each other, but none at a second the
/// store has (a sample written to both). Each file is marked with the
/// bytes read; one no longer than its mark is not read again, and one
/// that grew since (rows added to it after: a job that ended late after
/// midnight, a clock gone back, a supervisor that could not write the
/// store) is read again whole and adds the rows it lacks. Returns the
/// rows added.
fn import_files(
    store: &dyn NodeSampleStore,
    files: &dyn HostMetricsFiles,
    today: i64,
    retention_days: u32,
) -> Result<usize> {
    let marks = store.imported_files(HOST_NODE)?;
    let lengths: BTreeMap<String, u64> = files.files()?.into_iter().collect();
    let names: Vec<String> = lengths.keys().cloned().collect();
    let mut added = 0;
    for (name, day) in host_metrics::retained_files(&names, today, retention_days) {
        if marks
            .get(name)
            .is_some_and(|read| lengths.get(name).is_some_and(|length| length <= read))
        {
            continue;
        }
        let (samples, bytes) = files.read_file(name)?;
        added += store.import_node_samples(HOST_NODE, name, day, &samples, bytes)?;
    }
    Ok(added)
}

/// The sample job running now and when the last one started.
#[derive(Default)]
pub(super) struct HostMetricsWatch {
    job: Option<thread::JoinHandle<Recorded>>,
    started: Option<Instant>,
    failing: bool,
    /// Whether this process imported the CSV files.
    imported: bool,
}

impl HostMetricsWatch {
    pub(super) const fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl HostMetricsWatch {
    /// Reap the sample job once it ended; start one when the interval
    /// passed since the last one started, unless `handing_off`: a handoff
    /// waits for the job and none starts once it is asked for, so a slow
    /// sample cannot put the exec off. `now` reads the injected clock's unix
    /// second, `at` its monotonic time.
    pub(super) fn pass(
        &mut self,
        port: Option<&HostMetricsPort>,
        handing_off: bool,
        now: impl FnOnce() -> i64,
        at: Instant,
    ) {
        let Some(port) = port.cloned() else {
            return;
        };
        if let Some(job) = self.job.take() {
            if !job.is_finished() {
                self.job = Some(job);
                return;
            }
            self.reap(job);
        }
        if handing_off
            || !sample_due(
                self.started
                    .map(|started| at.saturating_duration_since(started)),
                port.interval,
            )
        {
            return;
        }
        self.started = Some(at);
        let now = now();
        let import = !self.imported;
        self.job = Some(spawn_traced(move || port.record(now, import)));
    }

    /// Wait for the sample job still running, as the loop ends, until
    /// `deadline` on `clock` when one is given ([`super::finished_by`]);
    /// one still running then is left, unrecorded.
    pub(super) fn finish(
        &mut self,
        deadline: Option<Instant>,
        clock: &dyn crate::application::Clock,
    ) {
        if let Some(job) = self.job.take() {
            if super::finished_by(&job, deadline, clock) {
                self.reap(job);
            } else {
                warn!("the host's sample still runs as the supervisor ends; it is left unrecorded");
            }
        }
    }

    fn reap(&mut self, job: thread::JoinHandle<Recorded>) {
        let failure = match job.join() {
            Ok(recorded) => self.apply(recorded),
            Err(_) => Some("the sample job panicked".to_owned()),
        };
        let Some(failure) = failure else {
            if self.failing {
                info!("the host's load is recorded again");
            }
            self.failing = false;
            return;
        };
        if !self.failing {
            warn!(error = %failure, "the host's load could not be recorded: {failure}");
        }
        self.failing = true;
    }

    /// Take in what a job did; its failures in one line, if any.
    fn apply(&mut self, recorded: Recorded) -> Option<String> {
        for path in recorded.removed {
            info!(
                "host metrics past their retention removed: {}",
                path.display()
            );
        }
        if let Some(added) = recorded.imported {
            info!(
                added,
                "the host's CSV files are imported into the node samples"
            );
            self.imported = true;
        }
        (!recorded.failures.is_empty()).then(|| recorded.failures.join("; "))
    }
}

/// Whether a sample is due `since_started` the last one started (`None`:
/// none yet), taken every `interval`.
fn sample_due(since_started: Option<Duration>, interval: Duration) -> bool {
    since_started.is_none_or(|since| since >= interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::host_metrics::{NodeSample, header, parse_file};
    use std::sync::Mutex;

    /// The rows a store holds, the local day of each by its second, and
    /// the days its retention was given.
    #[derive(Default)]
    struct Rows {
        samples: Vec<(SampleSource, HostSample)>,
        days: BTreeMap<i64, i64>,
        imported: BTreeMap<String, u64>,
        removed_before: Vec<i64>,
        /// Opens but refuses every write of a sample.
        refuses: bool,
    }

    impl Rows {
        /// The samples by the second they were taken.
        fn sorted(&self) -> Vec<HostSample> {
            let mut samples: Vec<HostSample> = self
                .samples
                .iter()
                .map(|(_, sample)| sample.clone())
                .collect();
            samples.sort_by_key(|sample| sample.unix);
            samples
        }

        fn put(&mut self, source: SampleSource, day: i64, sample: HostSample) {
            self.days.insert(sample.unix, day);
            self.samples.push((source, sample));
        }
    }

    /// A NodeSampleStore in memory; `None` cannot be opened.
    struct Store(Option<Arc<Mutex<Rows>>>);

    struct Open(Arc<Mutex<Rows>>);

    impl QueueOpener<dyn NodeSampleStore + Send> for Store {
        fn open(&self) -> Result<Box<dyn NodeSampleStore + Send>> {
            match &self.0 {
                Some(rows) => Ok(Box::new(Open(rows.clone()))),
                None => bail!("the database is locked"),
            }
        }
    }

    impl NodeSampleStore for Open {
        fn record_node_sample(
            &self,
            _node: &str,
            day: i64,
            sample: &HostSample,
            source: SampleSource,
            apart_secs: i64,
        ) -> Result<bool> {
            let mut rows = self.0.lock().unwrap();
            ensure!(!rows.refuses, "disk I/O error");
            let near = rows.samples.iter().any(|(_, row)| {
                row.unix == sample.unix
                    || (rows.days[&row.unix] == day && (row.unix - sample.unix).abs() < apart_secs)
            });
            if !near {
                rows.put(source, day, sample.clone());
            }
            Ok(!near)
        }

        fn import_node_samples(
            &self,
            _node: &str,
            file: &str,
            day: i64,
            samples: &[HostSample],
            bytes: u64,
        ) -> Result<usize> {
            let mut rows = self.0.lock().unwrap();
            let mut added = 0;
            for sample in samples {
                if !rows.samples.iter().any(|(_, row)| row.unix == sample.unix) {
                    rows.put(SampleSource::CsvImport, day, sample.clone());
                    added += 1;
                }
            }
            rows.imported.insert(file.to_owned(), bytes);
            Ok(added)
        }

        fn imported_files(&self, _node: &str) -> Result<BTreeMap<String, u64>> {
            Ok(self.0.lock().unwrap().imported.clone())
        }

        fn remove_node_samples(&self, before_day: i64) -> Result<usize> {
            let mut rows = self.0.lock().unwrap();
            rows.removed_before.push(before_day);
            let count = rows.samples.len();
            let days = rows.days.clone();
            rows.samples
                .retain(|(_, row)| days[&row.unix] >= before_day);
            Ok(count - rows.samples.len())
        }

        fn node_samples(&self, _node: &str, _from: i64, _until: i64) -> Result<Vec<NodeSample>> {
            Ok(Vec::new())
        }

        fn node_samples_after(&self, _after: i64) -> Result<Vec<NodeSample>> {
            Ok(Vec::new())
        }
    }

    /// The CSV files in memory, each its text: a sample is appended to its
    /// local day's file unless the file's last row keeps it out and the
    /// files past the retention go, as the files under `host/` do, and a
    /// file reads by `parse_file`; `broken` cannot be written.
    #[derive(Default)]
    struct Files {
        texts: Mutex<BTreeMap<String, String>>,
        broken: bool,
        reads: Mutex<Vec<String>>,
        /// Runs once as the next write begins: another supervisor's job
        /// interleaved between this job's store write and its file write.
        before_write: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Files {
        fn of(texts: impl IntoIterator<Item = (String, String)>) -> Self {
            Self {
                texts: Mutex::new(texts.into_iter().collect()),
                ..Self::default()
            }
        }

        /// Every sample of the files, by the second it was taken.
        fn samples(&self) -> Vec<HostSample> {
            let mut samples: Vec<HostSample> = self
                .texts
                .lock()
                .unwrap()
                .values()
                .flat_map(|text| parse_file(text))
                .collect();
            samples.sort_by_key(|sample| sample.unix);
            samples
        }
    }

    impl HostMetricsFiles for Files {
        fn record(
            &self,
            sample: &HostSample,
            offset: i64,
            interval: Duration,
            retention_days: u32,
            admitted: Option<bool>,
        ) -> Result<Vec<PathBuf>> {
            let interleaved = self.before_write.lock().unwrap().take();
            if let Some(job) = interleaved {
                job();
            }
            ensure!(!self.broken, "host is not a directory");
            let today = host_metrics::local_day(sample.unix, offset);
            let half = host_metrics::apart_secs(interval);
            let mut texts = self.texts.lock().unwrap();
            let text = texts.entry(host_metrics::file_name(today)).or_default();
            let near =
                host_metrics::last_unix(text).is_some_and(|last| (sample.unix - last).abs() < half);
            if admitted.unwrap_or(!near) {
                if text.is_empty() {
                    text.push_str(&header());
                    text.push('\n');
                }
                text.push_str(&sample.row(offset));
                text.push('\n');
            }
            let names: Vec<String> = texts.keys().cloned().collect();
            let mut removed = Vec::new();
            for name in host_metrics::expired(&names, today, retention_days) {
                texts.remove(name);
                removed.push(PathBuf::from(name));
            }
            Ok(removed)
        }

        fn files(&self) -> Result<Vec<(String, u64)>> {
            Ok(self
                .texts
                .lock()
                .unwrap()
                .iter()
                .map(|(name, text)| (name.clone(), text.len() as u64))
                .collect())
        }

        fn read_file(&self, name: &str) -> Result<(Vec<HostSample>, u64)> {
            self.reads.lock().unwrap().push(name.to_owned());
            let texts = self.texts.lock().unwrap();
            let text = texts.get(name).map_or("", String::as_str);
            Ok((parse_file(text), text.len() as u64))
        }
    }

    const DAY: i64 = 86_400;
    /// Noon of a day, in UTC.
    const NOW: i64 = 20_000 * DAY + DAY / 2;

    fn at(unix: i64) -> HostSample {
        let mut sample = HostSample::new(unix);
        #[allow(clippy::cast_precision_loss)]
        sample.set("load1", Some(1.5 + (unix % 100) as f64 / 4.0));
        sample.set("cpu_total", Some(150.5));
        sample
    }

    fn port(store: Option<Arc<Mutex<Rows>>>, files: Arc<Files>) -> HostMetricsPort {
        HostMetricsPort {
            interval: Duration::from_secs(30),
            retention_days: 2,
            sample: Arc::new(at),
            utc_offset: |_| 0,
            files,
            store: Arc::new(Store(store)),
        }
    }

    /// The first pass samples, and the next one once the interval passed
    /// since the last sample started: not 1 ms before.
    #[test]
    fn a_sample_is_due_first_and_then_once_the_interval_passed() {
        let interval = Duration::from_secs(60);
        assert!(sample_due(None, interval));
        assert!(!sample_due(Some(Duration::ZERO), interval));
        assert!(!sample_due(
            Some(interval - Duration::from_millis(1)),
            interval
        ));
        assert!(sample_due(Some(interval), interval));
    }

    /// One sample goes to the store and to the files; a store that cannot
    /// be opened leaves the files written, and files that cannot be
    /// written leave the store written, each failure its own reason.
    #[test]
    fn a_sample_goes_to_the_store_and_the_files_whatever_the_other_did() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let both = port(Some(rows.clone()), files.clone()).record(NOW, false);
        assert!(both.failures.is_empty(), "{both:?}");
        assert_eq!(
            rows.lock().unwrap().samples,
            [(SampleSource::Sampled, at(NOW))]
        );
        assert_eq!(files.samples(), [at(NOW)]);

        let no_store = port(None, files.clone()).record(NOW + 30, false);
        assert_eq!(no_store.failures.len(), 1, "{no_store:?}");
        assert!(no_store.failures[0].contains("the database is locked"));
        assert_eq!(files.samples().len(), 2);

        let broken = Arc::new(Files {
            broken: true,
            ..Files::default()
        });
        let no_files = port(Some(rows.clone()), broken).record(NOW + 60, false);
        assert_eq!(no_files.failures.len(), 1, "{no_files:?}");
        assert!(no_files.failures[0].contains("host is not a directory"));
        assert_eq!(rows.lock().unwrap().samples.len(), 2);

        // Neither stops the watch: it logs and goes on to the next sample.
        let mut watch = HostMetricsWatch::default();
        assert!(watch.apply(no_files).is_some());
        assert!(watch.apply(both).is_none());
    }

    /// A store that opens but cannot take the sample leaves the files
    /// written and the retention run, and the failure is the job's only
    /// one, which the watch logs; with the store taking samples again the
    /// next job records to both.
    #[test]
    fn a_store_that_refuses_the_sample_leaves_the_files_written() {
        let rows = Arc::new(Mutex::new(Rows {
            refuses: true,
            ..Rows::default()
        }));
        let files = Arc::new(Files::default());
        let port = port(Some(rows.clone()), files.clone());
        let refused = port.record(NOW, true);
        assert_eq!(refused.failures.len(), 1, "{refused:?}");
        assert!(
            refused.failures[0].contains("record the sample in the store")
                && refused.failures[0].contains("disk I/O error"),
            "{refused:?}"
        );
        assert_eq!(refused.imported, Some(0));
        assert_eq!(files.samples(), [at(NOW)]);
        assert!(rows.lock().unwrap().samples.is_empty());
        assert_eq!(rows.lock().unwrap().removed_before.len(), 1);

        let mut watch = HostMetricsWatch::default();
        let failure = watch.apply(refused);
        assert!(failure.is_some_and(|failure| failure.contains("disk I/O error")));
        rows.lock().unwrap().refuses = false;
        let recorded = port.record(NOW + 30, false);
        assert!(recorded.failures.is_empty(), "{recorded:?}");
        assert_eq!(rows.lock().unwrap().sorted(), [at(NOW + 30)]);
        assert_eq!(files.samples(), [at(NOW), at(NOW + 30)]);
    }

    /// Two supervisors of a queue sampling 10 s apart every 30 s write one
    /// row to each: a sample less than half the interval from another is
    /// kept out of the store and the files alike, and one 15 s apart is
    /// not.
    #[test]
    fn a_second_supervisor_within_half_the_interval_adds_no_row() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let first = port(Some(rows.clone()), files.clone());
        let second = port(Some(rows.clone()), files.clone());
        first.record(NOW, false);
        second.record(NOW + 10, false);
        assert_eq!(rows.lock().unwrap().sorted(), [at(NOW)]);
        second.record(NOW + 15, false);
        assert_eq!(rows.lock().unwrap().sorted(), [at(NOW), at(NOW + 15)]);
        assert_eq!(files.samples(), rows.lock().unwrap().sorted());
    }

    /// Two supervisors' jobs interleaved: A writes 1000 s to the store, B
    /// samples 10 s later in full (the store keeps it out, so the files
    /// leave it out too), then A writes to the files. Both keep A's sample
    /// only: the files follow the store's decision, not their last row.
    #[test]
    fn interleaved_jobs_of_two_supervisors_keep_the_same_sample_in_both() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let (a, b) = (
            port(Some(rows.clone()), files.clone()),
            port(Some(rows.clone()), files.clone()),
        );
        *files.before_write.lock().unwrap() = Some(Box::new(move || {
            let recorded = b.record(NOW + 10, false);
            assert!(recorded.failures.is_empty(), "{recorded:?}");
        }));
        assert!(a.record(NOW, false).failures.is_empty());
        assert_eq!(rows.lock().unwrap().sorted(), [at(NOW)]);
        assert_eq!(files.samples(), [at(NOW)]);
    }

    /// Samples out of the order they were taken (a job that ended late, a
    /// clock gone back): 1000, 1030 then 1005 s. The store keeps 1005 out,
    /// 5 s from 1000 though not from the file's last row, and the files
    /// follow it. Without the store, the files decide by their last row
    /// alone.
    #[test]
    fn samples_out_of_order_are_kept_alike_in_both() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let port = port(Some(rows.clone()), files.clone());
        for at in [NOW, NOW + 30, NOW + 5] {
            assert!(port.record(at, false).failures.is_empty());
        }
        assert_eq!(rows.lock().unwrap().sorted(), [at(NOW), at(NOW + 30)]);
        assert_eq!(files.samples(), rows.lock().unwrap().sorted());

        let alone = Arc::new(Files::default());
        let without = super::tests::port(None, alone.clone());
        for at in [NOW, NOW + 30, NOW + 5, NOW + 10] {
            without.record(at, false);
        }
        assert_eq!(alone.samples(), [at(NOW), at(NOW + 5), at(NOW + 30)]);
    }

    /// Two supervisors sampling 23:59:50 and 00:00:00 of the host's local
    /// time, each importing as it starts, and a third process starting
    /// after: the new day's first sample is in its file and in the store
    /// alike, and the store's rows summarize as the files do.
    #[test]
    fn two_supervisors_across_the_local_midnight_keep_the_same_samples() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let offset = 9 * 3600;
        let midnight = 20_000 * DAY - offset;
        let local = |files: &Arc<Files>| HostMetricsPort {
            utc_offset: |_| 9 * 3600,
            ..port(Some(rows.clone()), files.clone())
        };
        let first = local(&files).record(midnight - 10, true);
        let second = local(&files).record(midnight, true);
        let third = local(&files).record(midnight + 30, true);
        for recorded in [&first, &second, &third] {
            assert!(recorded.failures.is_empty(), "{recorded:?}");
        }
        // The second imported the first's row, which it had already.
        assert_eq!(
            (first.imported, second.imported, third.imported),
            (Some(0), Some(0), Some(0))
        );
        let stored = rows.lock().unwrap().sorted();
        assert_eq!(stored, [at(midnight - 10), at(midnight), at(midnight + 30)]);
        assert_eq!(files.samples(), stored);
        let (from, until) = (midnight - DAY, midnight + DAY);
        assert_eq!(
            host_metrics::summarize(&stored, from, until),
            host_metrics::summarize(&files.samples(), from, until)
        );
    }

    /// Samples of more than two decimals, as getloadavg and `ps` give
    /// them, go to the store as the files keep them: the store's rows
    /// summarize as the files' rows do (1.234, 1.234 and 1.244 have the
    /// mean 1.24 unrounded, 1.23 in the files).
    #[test]
    fn the_store_keeps_the_values_as_the_files_do() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let fine = HostMetricsPort {
            sample: Arc::new(|unix: i64| {
                let mut sample = HostSample::new(unix);
                let load = [1.234, 1.234, 1.244][usize::try_from(unix / 30 % 3).unwrap()];
                sample.set("load1", Some(load));
                sample.set("cpu_total", Some(load * 100.0 + 0.0049));
                sample.set("mem_used_mb", Some(8191.996));
                sample
            }),
            ..port(Some(rows.clone()), files.clone())
        };
        for at in [NOW, NOW + 30, NOW + 60] {
            assert!(fine.record(at, false).failures.is_empty());
        }
        let stored = rows.lock().unwrap().sorted();
        assert_eq!(stored, files.samples());
        let (from, until) = (NOW, NOW + 60);
        let summary = host_metrics::summarize(&stored, from, until);
        assert_eq!(
            summary,
            host_metrics::summarize(&files.samples(), from, until)
        );
        assert_eq!(summary.metrics["load1"].unwrap().mean, 1.23);
        assert!(summary.cpu_secs.is_some());
    }

    /// Across the end of daylight saving in New York (2026-11-01 06:00
    /// UTC), the store keeps the days the files keep: on 2026-11-02 with
    /// two days kept, 2026-11-01 from its own midnight (04:00 UTC, at -4 h)
    /// though the offset is now -5 h, so its 00:30 row stays, and
    /// 2026-10-31 goes with its file; the store's rows then summarize as
    /// the files do.
    #[test]
    fn the_store_keeps_the_days_the_files_keep_across_a_daylight_saving_change() {
        const NOV_1: i64 = 20_758;
        fn new_york(unix: i64) -> i64 {
            if unix < 1_793_512_800 {
                -4 * 3600
            } else {
                -5 * 3600
            }
        }
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let port = HostMetricsPort {
            utc_offset: new_york,
            ..port(Some(rows.clone()), files.clone())
        };
        let midnight = NOV_1 * DAY + 4 * 3600;
        let now = (NOV_1 + 1) * DAY + 17 * 3600;
        for at in [midnight - 1800, midnight + 1800, midnight + 10 * 3600, now] {
            assert!(port.record(at, false).failures.is_empty());
        }
        assert_eq!(rows.lock().unwrap().removed_before.last(), Some(&NOV_1));
        let stored = rows.lock().unwrap().sorted();
        assert_eq!(
            stored,
            [at(midnight + 1800), at(midnight + 10 * 3600), at(now)]
        );
        assert_eq!(files.samples(), stored);
        let (from, until) = (midnight - DAY, now);
        assert_eq!(
            host_metrics::summarize(&stored, from, until),
            host_metrics::summarize(&files.samples(), from, until)
        );
    }

    /// A clock change that repeats midnight (+3 h until 22:00 UTC, then
    /// +2 h): day 20,001 begins at its first midnight (21:00 UTC), so on
    /// 20,002 with two days kept its rows from that first hour stay in the
    /// store as in their file, the day before goes from both, and the
    /// store's rows summarize as the files do.
    #[test]
    fn the_store_keeps_the_first_hour_of_a_day_whose_midnight_repeats() {
        const D: i64 = 20_001 * DAY;
        fn repeats(unix: i64) -> i64 {
            if unix < D - 7200 { 10_800 } else { 7200 }
        }
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let port = HostMetricsPort {
            utc_offset: repeats,
            ..port(Some(rows.clone()), files.clone())
        };
        let now = D + DAY + 12 * 3600;
        for at in [D - 12_600, D - 9000, D - 3600, now] {
            assert!(port.record(at, false).failures.is_empty());
        }
        assert_eq!(rows.lock().unwrap().removed_before.last(), Some(&20_001));
        let stored = rows.lock().unwrap().sorted();
        assert_eq!(stored, [at(D - 9000), at(D - 3600), at(now)]);
        assert_eq!(files.samples(), stored);
        let (from, until) = (D - DAY, now);
        assert_eq!(
            host_metrics::summarize(&stored, from, until),
            host_metrics::summarize(&files.samples(), from, until)
        );
    }

    /// The clock going back from 00:30 to 23:30 (+1 h until 23:30 UTC,
    /// then +0): day 20,001 begins at 23:00 UTC and day 20,000 goes on
    /// after it, until 00:00 UTC. Two supervisors sampling 10 s apart
    /// across the change are on two days, so each day's file and the
    /// store keep both; on 20,002 with two days kept, the rows of 20,000
    /// after the change go from the store with their file, and the store's
    /// rows summarize as the files do.
    #[test]
    fn the_store_goes_by_the_local_day_when_the_clock_goes_back_into_the_day_before() {
        const D: i64 = 20_001 * DAY;
        fn back(unix: i64) -> i64 {
            if unix < D - 1800 { 3600 } else { 0 }
        }
        assert_eq!(host_metrics::local_day(D - 1805, back(D - 1805)), 20_001);
        assert_eq!(host_metrics::local_day(D - 1795, back(D - 1795)), 20_000);
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let port = || HostMetricsPort {
            utc_offset: back,
            ..port(Some(rows.clone()), files.clone())
        };
        let (first, second) = (port(), port());
        assert!(first.record(D - 1805, false).failures.is_empty());
        assert!(second.record(D - 1795, false).failures.is_empty());
        assert_eq!(rows.lock().unwrap().sorted(), [at(D - 1805), at(D - 1795)]);
        assert_eq!(files.samples(), rows.lock().unwrap().sorted());

        first.record(D - 900, false);
        first.record(D + 3600, false);
        assert_eq!(rows.lock().unwrap().days[&(D - 900)], 20_000);
        let now = D + DAY + 12 * 3600;
        assert!(first.record(now, false).failures.is_empty());
        assert_eq!(rows.lock().unwrap().removed_before.last(), Some(&20_001));
        let stored = rows.lock().unwrap().sorted();
        assert_eq!(stored, [at(D - 1805), at(D + 3600), at(now)]);
        assert_eq!(files.samples(), stored);
        let (from, until) = (D - DAY, now);
        assert_eq!(
            host_metrics::summarize(&stored, from, until),
            host_metrics::summarize(&files.samples(), from, until)
        );
    }

    /// The store removes the rows before the start of the oldest local day
    /// the retention keeps, the files' days; zero days removes none.
    #[test]
    fn the_store_keeps_the_days_of_the_retention() {
        let rows = Arc::new(Mutex::new(Rows::default()));
        rows.lock()
            .unwrap()
            .put(SampleSource::Sampled, 19_998, at(NOW - 2 * DAY));
        let files = Arc::new(Files::default());
        port(Some(rows.clone()), files.clone()).record(NOW, false);
        {
            let rows = rows.lock().unwrap();
            assert_eq!(rows.removed_before, [19_999]);
            assert_eq!(rows.samples, [(SampleSource::Sampled, at(NOW))]);
        }
        let kept = Arc::new(Mutex::new(Rows::default()));
        let forever = HostMetricsPort {
            retention_days: 0,
            ..port(Some(kept.clone()), files)
        };
        forever.record(NOW, false);
        assert!(kept.lock().unwrap().removed_before.is_empty());
    }

    /// A past day's file imported, then a row added to it: supervisor B,
    /// past midnight, imports day 20,000's file; supervisor A's job of
    /// 23:59:20, ending late, cannot write the store and writes that file
    /// alone. The next process reads the file again, since it grew past
    /// its mark, and adds the row; one after it reads nothing again.
    #[test]
    fn a_row_added_to_a_file_after_its_import_comes_in_at_the_next_import() {
        const MIDNIGHT: i64 = 20_001 * DAY;
        let rows = Arc::new(Mutex::new(Rows::default()));
        let files = Arc::new(Files::default());
        let port = port(Some(rows.clone()), files.clone());
        assert!(port.record(MIDNIGHT - 100, false).failures.is_empty());
        let b = port.record(MIDNIGHT + 30, true);
        assert_eq!(b.imported, Some(0), "{b:?}");
        rows.lock().unwrap().refuses = true;
        let late = port.record(MIDNIGHT - 40, false);
        assert_eq!(late.failures.len(), 1, "{late:?}");
        rows.lock().unwrap().refuses = false;
        assert_eq!(
            files.samples(),
            [at(MIDNIGHT - 100), at(MIDNIGHT - 40), at(MIDNIGHT + 30)]
        );
        assert_ne!(files.samples(), rows.lock().unwrap().sorted());

        let next = port.record(MIDNIGHT + 90, true);
        assert_eq!(next.imported, Some(1), "{next:?}");
        assert_eq!(files.samples(), rows.lock().unwrap().sorted());
        let reads = files.reads.lock().unwrap().len();
        let after = port.record(MIDNIGHT + 150, true);
        assert_eq!(after.imported, Some(0), "{after:?}");
        // Only today's file, grown by the sample at MIDNIGHT + 90.
        assert_eq!(
            files.reads.lock().unwrap()[reads..],
            [host_metrics::file_name(20_001)]
        );
    }

    /// The import reads the CSV files the retention keeps and adds every
    /// row of them: a day whose binary changed its columns (two headers),
    /// rows 10 s apart, but not a row without a unix second nor one at a
    /// second the store has (a sample written to both). Each file read is
    /// marked with its length: unchanged, it is not read again, and today's,
    /// grown by a later sample, is read again and adds no row. It comes
    /// before the sample, so a file's row 10 s
    /// before the sample keeps the sample out of the store as out of the
    /// file, and the store's rows then summarize as the files do. The
    /// watch imports until an import succeeded.
    #[test]
    fn the_import_keeps_every_row_once_and_comes_before_the_sample() {
        let today = host_metrics::local_day(NOW, 0);
        let yesterday = NOW - DAY;
        let files = Arc::new(Files::of([
            (
                host_metrics::file_name(today - 2),
                format!("{}\n{}\n", header(), at(NOW - 2 * DAY).row(0)),
            ),
            (
                host_metrics::file_name(today - 1),
                format!(
                    "time,unix,load1,cpu_total\nx,{},1.5,120\nx,,9,9\nx,{},2.5,130\n{}\n{}\n{}\n{}\n",
                    yesterday,
                    yesterday + 30,
                    header(),
                    at(yesterday + 60).row(0),
                    at(yesterday + 70).row(0),
                    at(yesterday + 80).row(0),
                ),
            ),
            (
                host_metrics::file_name(today),
                format!(
                    "{}\n{}\n,,1\n{}\n{}\n",
                    header(),
                    at(NOW - 60).row(0),
                    at(NOW - 30).row(0),
                    at(NOW - 10).row(0),
                ),
            ),
        ]));
        let rows = Arc::new(Mutex::new(Rows::default()));
        rows.lock()
            .unwrap()
            .put(SampleSource::Sampled, 20_000, at(NOW - 30));
        let port = port(Some(rows.clone()), files.clone());
        let first = port.record(NOW, true);
        assert_eq!(first.imported, Some(7), "{first:?}");
        let again = port.record(NOW + 30, true);
        assert_eq!(again.imported, Some(0), "{again:?}");
        assert_eq!(
            *files.reads.lock().unwrap(),
            [
                host_metrics::file_name(today - 1),
                host_metrics::file_name(today),
            ]
        );
        let held = rows.lock().unwrap();
        let mut sources: Vec<(i64, SampleSource)> = held
            .samples
            .iter()
            .map(|(source, sample)| (sample.unix, *source))
            .collect();
        sources.sort_unstable_by_key(|(unix, _)| *unix);
        assert_eq!(
            sources,
            [
                (yesterday, SampleSource::CsvImport),
                (yesterday + 30, SampleSource::CsvImport),
                (yesterday + 60, SampleSource::CsvImport),
                (yesterday + 70, SampleSource::CsvImport),
                (yesterday + 80, SampleSource::CsvImport),
                (NOW - 60, SampleSource::CsvImport),
                (NOW - 30, SampleSource::Sampled),
                (NOW - 10, SampleSource::CsvImport),
                (NOW + 30, SampleSource::Sampled),
            ]
        );
        let stored = held.sorted();
        assert_eq!(stored[0].values.len(), 2);
        assert_eq!(stored[2], at(yesterday + 60));
        assert_eq!(
            held.imported.keys().cloned().collect::<Vec<_>>(),
            [
                host_metrics::file_name(today - 1),
                host_metrics::file_name(today)
            ]
        );
        let (from, until) = (yesterday - 10, NOW + 30);
        let summary = host_metrics::summarize(&stored, from, until);
        assert_eq!(
            summary,
            host_metrics::summarize(&files.samples(), from, until)
        );
        assert_eq!(summary.samples, 9);
        assert!(summary.cpu_secs.is_some());
        drop(held);
        // Today's file grew by the sample at NOW + 30: read again.
        assert_eq!(port.record(NOW + 60, true).imported, Some(0));
        assert_eq!(files.reads.lock().unwrap().len(), 3);

        let mut watch = HostMetricsWatch::default();
        watch.apply(port.record(NOW + 90, false));
        assert!(!watch.imported);
        watch.apply(Recorded {
            imported: Some(0),
            ..Recorded::default()
        });
        assert!(watch.imported);
    }
}
