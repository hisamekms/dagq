//! The process's `tracing` subscriber (ADR-0033 decisions 1, 2 and 7): every
//! progress and diagnostic event goes to one JSON Lines file per process in
//! the queue's `logs/` and, as its message alone, to stderr as before.
//!
//! A record is one line: `timestamp` (ISO 8601, UTC, milliseconds), `level`,
//! `target`, `message`, `fields` (the event's own fields over those of the
//! spans it is in) and `spans` (their names and fields, outermost first).
//! JSON escapes a newline inside a message, so a message with one (a cmux
//! stderr tail) never splits a record. Events of [`FILE_ONLY_TARGET`] (the
//! start record and panics, which the default panic hook already prints) go
//! to the file only. A file that cannot be opened or written leaves the
//! process running on stderr alone.
//!
//! Opening a file also prunes the directory: log files of this runtime
//! last written more than [`LOG_RETENTION`] ago are removed, except those
//! of a process that is still alive.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    panic,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};
use tracing::{
    Dispatch, Event, Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{
    Layer, Registry,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

/// The target prefix of events written to the file only.
pub const FILE_ONLY_TARGET: &str = "dagq::telemetry";

/// How long a log file is kept after it was last written: 14 days.
pub const LOG_RETENTION: Duration = Duration::from_secs(14 * 86_400);

/// Where the records of one process go.
pub struct Telemetry {
    pub dispatch: Dispatch,
    /// The JSON Lines file, when it could be opened.
    pub path: Option<PathBuf>,
}

impl Telemetry {
    /// Records to `<log_dir>/<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`
    /// (the directory created if missing) and messages to stderr. The
    /// file starts with a record of the process, its pid and version.
    /// Log files older than [`LOG_RETENTION`] are pruned first (see
    /// [`prune_logs`]). A file that cannot be opened is reported on stderr
    /// once, and the process goes on with stderr alone.
    pub fn open(log_dir: &Path, process: &str) -> Self {
        let now = SystemTime::now();
        let pruned = prune_logs(log_dir, now, LOG_RETENTION);
        let pid = std::process::id();
        let path = log_dir.join(format!("{process}-{}-{pid}.jsonl", file_stamp(now)));
        let opened = fs::create_dir_all(log_dir)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&path));
        match opened {
            Ok(file) => {
                let telemetry = Self::with_writer(Box::new(file), true, Some(path));
                telemetry.in_scope(|| {
                    tracing::info!(
                        target: FILE_ONLY_TARGET,
                        process,
                        pid,
                        version = crate::VERSION,
                        "dagq {process} started"
                    );
                    if !pruned.is_empty() {
                        tracing::info!(
                            target: FILE_ONLY_TARGET,
                            removed = pruned.len(),
                            retention_days = LOG_RETENTION.as_secs() / 86_400,
                            "removed {} log files last written more than {} days ago",
                            pruned.len(),
                            LOG_RETENTION.as_secs() / 86_400
                        );
                    }
                });
                telemetry
            }
            Err(error) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "log file {} could not be opened: {error}; messages go to stderr only",
                    path.display()
                );
                Self::stderr()
            }
        }
    }

    /// Messages to stderr only, as a process that keeps no log file has them.
    pub fn stderr() -> Self {
        Self {
            dispatch: Dispatch::new(Registry::default().with(StderrLine { enabled: true })),
            path: None,
        }
    }

    /// Records to `writer`, and messages to stderr when `stderr` is set.
    pub fn with_writer(writer: Box<dyn Write + Send>, stderr: bool, path: Option<PathBuf>) -> Self {
        let subscriber = Registry::default()
            .with(JsonLines {
                writer: Mutex::new(writer),
            })
            .with(StderrLine { enabled: stderr });
        Self {
            dispatch: Dispatch::new(subscriber),
            path,
        }
    }

    /// Records into a shared buffer instead of a file, for tests.
    pub fn capture() -> (Self, Captured) {
        let buffer = Captured::default();
        (
            Self::with_writer(Box::new(buffer.clone()), false, None),
            buffer,
        )
    }

    /// Run `work` with this subscriber as the thread's default; threads it
    /// starts through `spawn_traced` report here too.
    pub fn in_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.dispatch, work)
    }

    /// Make this the subscriber of the whole process and log panics
    /// through it. A second install (a subscriber is already set) is
    /// ignored.
    pub fn install(self) -> Option<PathBuf> {
        let installed = tracing::dispatcher::set_global_default(self.dispatch).is_ok();
        if installed {
            install_panic_hook();
        }
        self.path
    }
}

/// Remove from `log_dir` the log files this runtime wrote (a
/// names [`log_file_owner`] knows) whose last write is more than
/// `retention` before `now`, unless the process of the pid in the name is
/// still alive (a quiet supervisor keeps writing to its file). Other files
/// (`launchd.log`, `rebind.jsonl`, a person's notes) are left alone. Failures are
/// ignored: pruning never stops a process. Returns the removed paths.
pub fn prune_logs(log_dir: &Path, now: SystemTime, retention: Duration) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(owner) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(log_file_owner)
        else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let stale = metadata.is_file()
            && metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > retention);
        if stale
            && !owner.is_some_and(crate::infrastructure::adapters::process_alive)
            && fs::remove_file(&path).is_ok()
        {
            removed.push(path);
        }
    }
    removed.sort();
    removed
}

/// Whether a file name is one this runtime writes in `logs/`, and then
/// the pid of the process writing it, if the name has one:
/// `<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`, an older binary's
/// `supervisor-<unix time>-<pid>.log`, or an update job's
/// `update-<unix time>-<commit>.{log,build.log,json}` (no pid).
fn log_file_owner(name: &str) -> Option<Option<u32>> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if let Some(stem) = name.strip_suffix(".jsonl") {
        let (rest, pid) = stem.rsplit_once('-')?;
        let (process, stamp) = rest.rsplit_once('-')?;
        let stamp_ok = stamp.len() == 16
            && digits(&stamp[..8])
            && stamp.as_bytes()[8] == b'T'
            && digits(&stamp[9..15])
            && stamp.ends_with('Z');
        return (!process.is_empty() && stamp_ok && digits(pid)).then(|| pid.parse().ok());
    }
    if let Some(stem) = name
        .strip_prefix("supervisor-")
        .and_then(|rest| rest.strip_suffix(".log"))
    {
        let (started_at, pid) = stem.rsplit_once('-')?;
        return (digits(started_at) && digits(pid)).then(|| pid.parse().ok());
    }
    let stem = name.strip_prefix("update-")?;
    let stem = [".build.log", ".log", ".json"]
        .iter()
        .find_map(|suffix| stem.strip_suffix(suffix))?;
    let (at, commit) = stem.split_once('-')?;
    (digits(at) && !commit.is_empty() && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(None)
}

/// Also record a panic, with its thread and location, before the default
/// hook prints it on stderr as always.
pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            target: "dagq::telemetry::panic",
            thread = thread.name().unwrap_or("unnamed"),
            location = info.location().map(ToString::to_string),
            "panic: {}",
            panic_message(info)
        );
        previous(info);
    }));
}

fn panic_message(info: &panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string payload".to_owned())
}

/// The bytes a [`Telemetry::capture`] wrote.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    /// Each line as JSON; a line that is not JSON is `Value::Null`.
    pub fn records(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or(Value::Null))
            .collect()
    }
}

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One JSON object per line.
struct JsonLines {
    writer: Mutex<Box<dyn Write + Send>>,
}

/// The fields of a span, kept in its extensions.
struct SpanFields(Map<String, Value>);

impl<S> Layer<S> for JsonLines
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(fields.map));
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(SpanFields(map)) = extensions.get_mut::<SpanFields>() {
                let mut fields = Fields {
                    map: std::mem::take(map),
                    message: None,
                };
                values.record(&mut fields);
                *map = fields.map;
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut own = Fields::default();
        event.record(&mut own);
        let mut fields = Map::new();
        let mut spans = Vec::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                let mut record = Map::new();
                record.insert("name".to_owned(), json!(span.name()));
                if let Some(SpanFields(map)) = span.extensions().get::<SpanFields>() {
                    for (key, value) in map {
                        fields.insert(key.clone(), value.clone());
                        record.insert(key.clone(), value.clone());
                    }
                }
                spans.push(Value::Object(record));
            }
        }
        fields.extend(own.map);
        let metadata = event.metadata();
        let record = json!({
            "timestamp": timestamp(SystemTime::now()),
            "level": metadata.level().as_str(),
            "target": metadata.target(),
            "message": own.message.unwrap_or_default(),
            "fields": fields,
            "spans": spans,
        });
        let mut line = record.to_string();
        line.push('\n');
        // One write per record, so appends of other processes do not
        // interleave; a panic that poisoned the lock does not end the log.
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writer.write_all(line.as_bytes());
    }
}

/// The event's message on one stderr line, as the runtime always printed it.
struct StderrLine {
    enabled: bool,
}

impl<S: Subscriber> Layer<S> for StderrLine {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !self.enabled || event.metadata().target().starts_with(FILE_ONLY_TARGET) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let _ = writeln!(
            io::stderr().lock(),
            "{}",
            fields.message.unwrap_or_default()
        );
    }
}

/// Collects an event's or span's fields as JSON, the message apart.
#[derive(Default)]
struct Fields {
    map: Map<String, Value>,
    message: Option<String>,
}

impl Fields {
    fn insert(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            self.message = Some(match value {
                Value::String(text) => text,
                other => other.to_string(),
            });
        } else {
            self.map.insert(field.name().to_owned(), value);
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, json!(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, json!(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, json!(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, json!(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, json!(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, json!(value.to_string()));
    }
}

/// `2026-09-24T01:02:03.456Z`.
pub fn timestamp(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (date, time) = civil(since.as_secs());
    format!("{date}T{time}.{:03}Z", since.subsec_millis())
}

/// `20260924T010203Z`, for file names.
fn file_stamp(at: SystemTime) -> String {
    let (date, time) = civil(at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs());
    format!("{}T{}Z", date.replace('-', ""), time.replace(':', ""))
}

/// The UTC date `YYYY-MM-DD` and time `HH:MM:SS` of a Unix time
/// (Howard Hinnant's days-to-civil).
fn civil(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}-{month:02}-{day:02}"),
        format!(
            "{:02}:{:02}:{:02}",
            rest / 3_600,
            rest % 3_600 / 60,
            rest % 60
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_record_is_one_json_line_even_when_its_message_has_newlines() {
        let (telemetry, captured) = Telemetry::capture();
        telemetry.in_scope(|| {
            let _span = tracing::info_span!("integrate", run_id = "r1", task_id = 7).entered();
            tracing::warn!(
                op = "push",
                error = "stderr tail\nsecond line\r\n",
                "run r1: push of main failed: stderr tail\nsecond line\r\n"
            );
            tracing::info!(run_id = "r2", "overrides the span's run_id");
        });
        let text = captured.text();
        assert_eq!(text.lines().count(), 2, "{text}");
        let records = captured.records();
        let first = &records[0];
        assert_eq!(first["level"], "WARN");
        assert_eq!(first["target"], module_path!());
        assert_eq!(
            first["message"],
            "run r1: push of main failed: stderr tail\nsecond line\r\n"
        );
        assert_eq!(first["fields"]["op"], "push");
        assert_eq!(first["fields"]["error"], "stderr tail\nsecond line\r\n");
        assert_eq!(first["fields"]["run_id"], "r1");
        assert_eq!(first["fields"]["task_id"], 7);
        assert_eq!(
            first["spans"],
            json!([{"name": "integrate", "run_id": "r1", "task_id": 7}])
        );
        let stamp = first["timestamp"].as_str().unwrap();
        assert!(stamp.ends_with('Z') && stamp.len() == 24, "{stamp}");
        assert_eq!(records[1]["fields"]["run_id"], "r2");
        assert_eq!(records[1]["level"], "INFO");
    }

    #[test]
    fn span_fields_recorded_later_and_typed_values_are_kept() {
        let (telemetry, captured) = Telemetry::capture();
        telemetry.in_scope(|| {
            let span = tracing::info_span!("run", run_id = tracing::field::Empty);
            span.record("run_id", "late");
            let _entered = span.enter();
            tracing::error!(
                ok = true,
                ratio = 0.5,
                count = 3_u64,
                delta = -2_i64,
                "typed"
            );
        });
        let record = &captured.records()[0];
        assert_eq!(
            record["fields"],
            json!({"run_id": "late", "ok": true, "ratio": 0.5, "count": 3, "delta": -2})
        );
        assert_eq!(record["level"], "ERROR");
    }

    #[test]
    fn the_file_is_named_by_process_time_and_pid_and_starts_with_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        let telemetry = Telemetry::open(&logs, "integrate");
        let path = telemetry.path.clone().unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let pid = std::process::id();
        assert!(
            name.starts_with("integrate-20") && name.ends_with(&format!("Z-{pid}.jsonl")),
            "{name}"
        );
        telemetry.in_scope(|| tracing::info!(target: FILE_ONLY_TARGET, "file only"));
        let lines: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["message"], "dagq integrate started");
        assert_eq!(lines[0]["fields"]["process"], "integrate");
        assert_eq!(lines[0]["fields"]["pid"], pid);
        assert_eq!(lines[0]["fields"]["version"], crate::VERSION);
        assert_eq!(lines[1]["message"], "file only");
    }

    #[test]
    fn log_files_past_the_retention_are_pruned_unless_their_process_lives() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path();
        let dead = u32::MAX; // no process has this pid
        let alive = std::process::id();
        let old = SystemTime::now() - LOG_RETENTION - Duration::from_secs(60);
        let names = [
            format!("integrate-20260901T000000Z-{dead}.jsonl"),
            format!("supervisor-1756684800-{dead}.log"),
            format!("supervise-20260901T000000Z-{alive}.jsonl"),
            format!("session-20260926T000000Z-{dead}.jsonl"),
            "launchd.log".to_owned(),
            "notes.jsonl".to_owned(),
            "update-1756684800-0123456789ab.build.log".to_owned(),
        ];
        for (index, name) in names.iter().enumerate() {
            let file = fs::File::create(logs.join(name)).unwrap();
            if index != 3 {
                file.set_modified(old).unwrap();
            }
        }
        fs::create_dir(logs.join(format!("dir-20260901T000000Z-{dead}.jsonl"))).unwrap();

        let telemetry = Telemetry::open(logs, "observe");

        let mut left: Vec<String> = fs::read_dir(logs)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| !name.starts_with("observe-"))
            .collect();
        left.sort();
        let mut expected = vec![
            names[2].clone(),
            names[3].clone(),
            names[4].clone(),
            names[5].clone(),
            format!("dir-20260901T000000Z-{dead}.jsonl"),
        ];
        expected.sort();
        assert_eq!(left, expected);
        let records: Vec<Value> = fs::read_to_string(telemetry.path.unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[1]["fields"]["removed"], 3);
        assert_eq!(records[1]["fields"]["retention_days"], 14);
        assert_eq!(
            records[1]["message"],
            "removed 3 log files last written more than 14 days ago"
        );
        assert!(prune_logs(&logs.join("missing"), SystemTime::now(), LOG_RETENTION).is_empty());
    }

    #[test]
    fn only_names_this_runtime_writes_are_pruned() {
        let owner = log_file_owner;
        assert_eq!(owner("integrate-20260901T000000Z-42.jsonl"), Some(Some(42)));
        assert_eq!(
            owner("planner-session-20260901T000000Z-7.jsonl"),
            Some(Some(7))
        );
        assert_eq!(owner("supervisor-1756684800-9.log"), Some(Some(9)));
        assert_eq!(owner("update-1756684800-0123456789ab.log"), Some(None));
        assert_eq!(
            owner("update-1756684800-0123456789ab.build.log"),
            Some(None)
        );
        assert_eq!(owner("update-1756684800-0123456789ab.json"), Some(None));
        for name in [
            "launchd.log",
            "rebind.jsonl",
            "other-1-2.log",
            "x-1.jsonl",
            "-20260901T000000Z-1.jsonl",
            "x-20260901T000000Z-.jsonl",
            "notes-2026-1.jsonl",
            "x-20260901X000000Z-1.jsonl",
            "x-20260901T000000Z-pid.jsonl",
            "supervisor-abc-1.log",
            "update-1756684800-notes.log",
            "update-x-0123.json",
            "update-1756684800.log",
        ] {
            assert_eq!(owner(name), None, "{name}");
        }
    }

    #[test]
    fn a_log_dir_that_cannot_be_made_leaves_stderr_only() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("file");
        fs::write(&blocked, "not a directory").unwrap();
        let telemetry = Telemetry::open(&blocked, "supervise");
        assert!(telemetry.path.is_none());
        telemetry.in_scope(|| tracing::info!("still printed"));
        Telemetry::stderr().in_scope(|| tracing::warn!(target: FILE_ONLY_TARGET, "skipped"));
    }

    #[test]
    fn a_writer_that_fails_does_not_stop_the_process() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        Telemetry::with_writer(Box::new(Broken), false, None)
            .in_scope(|| tracing::info!("lost, and nothing else happens"));
    }

    #[test]
    fn a_panic_is_recorded_through_the_hook() {
        let (telemetry, captured) = Telemetry::capture();
        telemetry.in_scope(|| {
            install_panic_hook();
            let result = std::panic::catch_unwind(|| panic!("boom\nnext"));
            let _ = panic::take_hook();
            assert!(result.is_err());
        });
        let records = captured.records();
        let panic = records
            .iter()
            .find(|r| r["target"] == "dagq::telemetry::panic")
            .unwrap();
        assert_eq!(panic["message"], "panic: boom\nnext");
        assert_eq!(panic["level"], "ERROR");
        assert!(
            panic["fields"]["location"]
                .as_str()
                .unwrap()
                .contains("telemetry.rs")
        );
    }

    #[test]
    fn timestamps_are_utc_iso_8601() {
        let at = UNIX_EPOCH + Duration::from_millis(1_790_211_723_456);
        assert_eq!(timestamp(at), "2026-09-24T01:02:03.456Z");
        assert_eq!(file_stamp(at), "20260924T010203Z");
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
    }
}
