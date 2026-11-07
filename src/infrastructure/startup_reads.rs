//! The queue service's record of what it answered right after it started
//! ([`StartupReads`]) and of its own memory then ([`ResidentMemory`]): a
//! service that starts with a supervisor (an auto-update's exec) answers
//! the supervisor's catch-up reads at once, and its memory peaks then, so
//! the record says which requests read how much while it did.
//!
//! What is recorded: one line per request that began within
//! [`STARTUP_WINDOW`] of the service's start (one that ends past it too),
//! and one summary line once the window has ended and those requests are
//! answered, or when the service stops first; nothing after. The
//! window and the durations go by the monotonic clock, so a step of the
//! wall clock neither stretches nor cuts them. It is
//! not a measurement kept up all the time.
//!
//! Where: the queue's `logs/` (next to the supervisor's
//! `supervise-<UTC time>-<pid>.jsonl`), in
//! `service-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`, the time the service's start
//! and the pid its process. The name is one the log pruning of
//! [`super::telemetry::prune_logs`] knows, so the file goes with the
//! other logs once it is older than [`super::telemetry::LOG_RETENTION`]
//! and its process is gone; opening it prunes the directory first, as
//! opening any of the runtime's log files does.
//! With an explicit deployment store, these diagnostics go through tracing
//! instead, inside the ordinary JSON Lines envelope, and open no file.
//!
//! Each line is one JSON object whose `record` says which:
//!
//! - `start`: `at` (UTC, milliseconds), `pid`, `version`, `window_secs`
//!   (the window's length) and the memory at the start, `resident` and
//!   `peak` (below).
//! - `request`: `at` (when its answer was built, before it is sent),
//!   `use_case` (the
//!   request's, `null` for one that did not read), `op` (the name of the
//!   queue's operation a `supervisor` request, or a `control` command's
//!   step, forwards to the queue: `ledger_heads`, `events_between`...;
//!   `null` for any other use case), `role` (the role of the principal the
//!   request was authenticated as, `null` when it was refused before or
//!   needs none), `bytes` (the length of the answer the service built,
//!   before an answer past its limit is swapped for `response_too_large`),
//!   `rows` (the length of a result that is a JSON array, `null` for any
//!   other), `duration_ms` (from reading the request to building its
//!   answer, on the monotonic clock; the socket's write is not in it, the
//!   wait for a heavy read's place is), `heavy_read_wait_ms` (how long it
//!   waited for that place, `null` for one that took none),
//!   `heavy_read_refused` (it got none, and read nothing), and
//!   `resident_before`, `resident_after` and `peak`.
//! - `summary`: `at`, `reason` (`window_ended`, or why the service stopped
//!   first: `stopped`, `queue_gone`, `owner_gone`),
//!   `window_secs`, `requests` (how many were recorded), `max_resident`
//!   and `max_resident_at` (the largest `resident` read, at the start or
//!   around a request, and when), `peak` (the largest one read),
//!   `unfinished` (the requests still in flight when the service stopped;
//!   0 at the window's end, whose summary waits for them), and
//!   `top_by_bytes`: up to [`TOP_REQUESTS`] of the requests with the most
//!   `bytes`, the most first, each with `at`, `use_case`, `op`, `role`,
//!   `bytes`, `rows` and `duration_ms`.
//!
//! The memory is the process's own in bytes ([`MemoryReading`]): what it
//! holds now (`resident`) and the most it held since it started (`peak`),
//! each `null` where it cannot be read. The record never changes an
//! answer: a request is answered the same whether the file could not be
//! opened, a line could not be written or the memory could not be read.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};
use tracing::warn;

use crate::application::{Clock, timestamp};

/// How long after its start the service records its requests: 15
/// minutes. A supervisor that starts with the service catches up in its
/// first minutes (its first observation pass and the ledger's builder
/// read the queue's history then), and the service's memory was seen to
/// peak within 3 minutes and settle by 12 (2026-11-03); the window holds
/// that with room, and ends before the record grows with the requests of
/// an ordinary day.
pub const STARTUP_WINDOW: Duration = Duration::from_secs(15 * 60);

/// How many of the requests with the most bytes the summary names.
pub const TOP_REQUESTS: usize = 10;

/// The process's own memory at one moment, in bytes; `None` where it
/// cannot be read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryReading {
    /// What the process holds now: macOS's physical footprint
    /// (`ri_phys_footprint`, what `footprint` and Activity Monitor show),
    /// Linux's resident set (`VmRSS`).
    pub resident: Option<u64>,
    /// The most it held since it started: macOS's
    /// `ri_lifetime_max_phys_footprint`, Linux's `VmHWM`. It catches a
    /// peak between two requests, which `resident` read around them may
    /// miss.
    pub peak: Option<u64>,
}

/// Where the record reads the process's memory: the system's
/// ([`OwnMemory`]), or a test's.
pub trait ResidentMemory: Send + Sync {
    /// The memory now; [`MemoryReading::default`] (all `None`) on a
    /// platform where it cannot be read, never an error.
    fn read(&self) -> MemoryReading;
}

/// This process's memory as its system tells it: `proc_pid_rusage` on
/// macOS, `/proc/self/status` on Linux, nothing elsewhere.
pub struct OwnMemory;

impl ResidentMemory for OwnMemory {
    #[cfg(target_vendor = "apple")]
    fn read(&self) -> MemoryReading {
        // SAFETY: an all-zero rusage_info_v4 is a valid value of the
        // plain struct of integers.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(2) writes one rusage_info_v4 of the
        // flavor asked for into the buffer, which lives through the call.
        let read = unsafe {
            libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast())
        };
        if read == 0 {
            MemoryReading {
                resident: Some(info.ri_phys_footprint),
                peak: Some(info.ri_lifetime_max_phys_footprint),
            }
        } else {
            MemoryReading::default()
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn read(&self) -> MemoryReading {
        fs::read_to_string("/proc/self/status")
            .map(|status| proc_status_memory(&status))
            .unwrap_or_default()
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    fn read(&self) -> MemoryReading {
        MemoryReading::default()
    }
}

/// `VmRSS` and `VmHWM` of a Linux `/proc/<pid>/status`, in bytes (the
/// file gives them in kB); a line missing or not read is `None`.
pub fn proc_status_memory(status: &str) -> MemoryReading {
    let field = |name: &str| {
        status.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix(':')?.trim();
            let kb = value.strip_suffix("kB").unwrap_or(value).trim();
            kb.parse::<u64>().ok().map(|kb| kb * 1024)
        })
    };
    MemoryReading {
        resident: field("VmRSS"),
        peak: field("VmHWM"),
    }
}

/// What a request was and what it was answered, as its line names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answered {
    pub use_case: Option<String>,
    pub op: Option<String>,
    pub role: Option<String>,
    pub bytes: usize,
    pub rows: Option<usize>,
    /// What it met at the limit of the heavy reads, when it asked for a
    /// place among them.
    pub heavy_read: Option<crate::application::heavy_reads::HeavyReadNote>,
}

impl Answered {
    /// The operation a request of `use_case` with `params` forwards to the
    /// queue: a `supervisor` request's `op`, a `control` command's step's
    /// (`call.op`); `None` for any other use case.
    pub fn op_of(use_case: &str, params: &Value) -> Option<String> {
        let op = match use_case {
            "supervisor" => params.get("op"),
            "control" => params.get("call").and_then(|call| call.get("op")),
            _ => None,
        };
        op.and_then(Value::as_str).map(str::to_owned)
    }

    /// How many rows a `result` holds: its length when it is an array.
    pub fn rows_of(result: Option<&Value>) -> Option<usize> {
        result.and_then(Value::as_array).map(Vec::len)
    }
}

/// When a request began to be answered, and the memory then.
#[derive(Debug, Clone, Copy)]
pub struct Began {
    at: SystemTime,
    since: Instant,
    before: MemoryReading,
}

/// The record of the service's requests right after its start (see the
/// module doc): one per service, shared by the threads that answer.
pub struct StartupReads {
    clock: Arc<dyn Clock>,
    memory: Box<dyn ResidentMemory>,
    started: Instant,
    window: Duration,
    state: Mutex<State>,
}

struct State {
    out: Option<Box<dyn Write + Send>>,
    /// Whether the summary is still to be written.
    open: bool,
    /// Why the window ended, once it did: no request begins to be
    /// recorded after it, and the summary waits for the ones in flight.
    ending: Option<String>,
    /// The requests that began in the window and are not answered yet.
    in_flight: usize,
    requests: u64,
    max: Option<(u64, SystemTime)>,
    peak: Option<u64>,
    top: Vec<(usize, Value)>,
}

impl StartupReads {
    /// The record of the service of the queue at `queue_dir`, started now
    /// by `clock`, in its own file in the queue's `logs/` (the old log
    /// files pruned first). A file that cannot be opened is said once on
    /// the service's stderr, and nothing is recorded.
    /// An explicit deployment store uses tracing without opening a file.
    pub fn open(queue_dir: &Path, clock: Arc<dyn Clock>, memory: Box<dyn ResidentMemory>) -> Self {
        if super::telemetry::deployment_streams(|name| std::env::var_os(name)) {
            return Self::new(Some(Box::new(TracingWriter)), clock, memory, STARTUP_WINDOW);
        }
        let logs = queue_dir.join(super::location::LOGS_DIR_NAME);
        let started = clock.system_time();
        super::telemetry::prune_logs(&logs, started, super::telemetry::LOG_RETENTION);
        let path = logs.join(format!(
            "service-{}-{}.jsonl",
            super::telemetry::file_stamp(started),
            std::process::id()
        ));
        let out = fs::create_dir_all(&logs)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&path))
            .map_err(|error| {
                warn!(
                    "the queue service's record of its first requests {} could not be opened: \
                     {error}; nothing is recorded",
                    path.display()
                );
            })
            .ok()
            .map(|file| Box::new(file) as Box<dyn Write + Send>);
        Self::new(out, clock, memory, STARTUP_WINDOW)
    }

    /// The record into `out` (none: nothing written) for `window` from
    /// now; it begins with its `start` line.
    pub fn new(
        out: Option<Box<dyn Write + Send>>,
        clock: Arc<dyn Clock>,
        memory: Box<dyn ResidentMemory>,
        window: Duration,
    ) -> Self {
        let started = clock.system_time();
        let reading = memory.read();
        let reads = Self {
            started: clock.monotonic(),
            clock,
            memory,
            window,
            state: Mutex::new(State {
                out,
                open: true,
                ending: None,
                in_flight: 0,
                requests: 0,
                max: None,
                peak: None,
                top: Vec::new(),
            }),
        };
        let mut state = reads.lock();
        state.noted(reading, started);
        let line = json!({
            "record": "start",
            "at": timestamp(started),
            "pid": std::process::id(),
            "version": crate::VERSION,
            "window_secs": window.as_secs(),
            "resident": reading.resident,
            "peak": reading.peak,
        });
        state.write(&line);
        drop(state);
        reads
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn past_window(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.window
    }

    /// A request begins to be answered: its time and memory, while the
    /// window is open, counted in flight until [`Self::answered`]; `None`
    /// past it (the window ended, its summary written once no request is
    /// in flight), and the request is not recorded.
    pub fn begin(&self) -> Option<Began> {
        let since = self.clock.monotonic();
        let mut state = self.lock();
        if self.past_window(since) {
            self.end(&mut state, "window_ended");
        }
        if !state.open || state.ending.is_some() {
            return None;
        }
        state.in_flight += 1;
        drop(state);
        Some(Began {
            at: self.clock.system_time(),
            since,
            before: self.memory.read(),
        })
    }

    /// The request that `began` was answered as `answered`: its line, even
    /// when the window ended while it was in flight, and the summary when
    /// the window has ended and no other request is in flight.
    pub fn answered(&self, began: Began, answered: &Answered) {
        let after = self.memory.read();
        let at = self.clock.system_time();
        let now = self.clock.monotonic();
        let duration_ms = u64::try_from(now.saturating_duration_since(began.since).as_millis())
            .unwrap_or(u64::MAX);
        let mut state = self.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.open {
            state.requests += 1;
            state.noted(began.before, began.at);
            state.noted(after, at);
            let entry = json!({
                "at": timestamp(at),
                "use_case": answered.use_case,
                "op": answered.op,
                "role": answered.role,
                "bytes": answered.bytes,
                "rows": answered.rows,
                "duration_ms": duration_ms,
            });
            let mut line = entry.clone();
            if let Value::Object(fields) = &mut line {
                fields.insert("record".into(), json!("request"));
                fields.insert(
                    "heavy_read_wait_ms".into(),
                    json!(answered.heavy_read.map(|note| note.waited_ms)),
                );
                fields.insert(
                    "heavy_read_refused".into(),
                    json!(answered.heavy_read.is_some_and(|note| note.refused)),
                );
                fields.insert("resident_before".into(), json!(began.before.resident));
                fields.insert("resident_after".into(), json!(after.resident));
                fields.insert("peak".into(), json!(after.peak));
            }
            state.write(&line);
            state.ranked(answered.bytes, entry);
        }
        if self.past_window(now) {
            self.end(&mut state, "window_ended");
        } else if let Some(reason) = state.ending.clone() {
            self.end(&mut state, &reason);
        }
    }

    /// End the window if it has by now: the service's loop asks between
    /// connections, so the summary comes at the window's end even when no
    /// request does (once those in flight are answered).
    pub fn tick(&self) {
        if self.past_window(self.clock.monotonic()) {
            let mut state = self.lock();
            self.end(&mut state, "window_ended");
        }
    }

    /// The service stops (`reason`, why): the summary now, whatever is in
    /// flight (its `unfinished`), as the process ends with them; nothing
    /// is recorded after it.
    pub fn stopped(&self, reason: &str) {
        let mut state = self.lock();
        if state.ending.is_none() {
            state.ending = Some(reason.to_owned());
        }
        self.summarize(&mut state);
    }

    /// The window ends (`reason`, why, kept from the first end): no request
    /// begins to be recorded, and the summary is written once none is in
    /// flight.
    fn end(&self, state: &mut State, reason: &str) {
        if state.ending.is_none() {
            state.ending = Some(reason.to_owned());
        }
        if state.in_flight == 0 {
            self.summarize(state);
        }
    }

    /// Write the summary and record nothing more; once.
    fn summarize(&self, state: &mut State) {
        if !state.open {
            return;
        }
        state.open = false;
        let line = json!({
            "record": "summary",
            "at": timestamp(self.clock.system_time()),
            "reason": state.ending,
            "window_secs": self.window.as_secs(),
            "requests": state.requests,
            "unfinished": state.in_flight,
            "max_resident": state.max.map(|(bytes, _)| bytes),
            "max_resident_at": state.max.map(|(_, at)| timestamp(at)),
            "peak": state.peak,
            "top_by_bytes": state.top.iter().map(|(_, entry)| entry.clone()).collect::<Vec<_>>(),
        });
        state.write(&line);
        state.top = Vec::new();
    }
}

/// StartupReads writes one complete JSON record at a time. The tracing
/// envelope keeps the same deployment sink, credential filtering and line
/// escaping as other diagnostics; the inner record remains available as JSON.
struct TracingWriter;

impl Write for TracingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        tracing::info!(startup_reads = %String::from_utf8_lossy(bytes).trim_end(), "service startup reads");
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl State {
    /// Keep `reading`, read at `at`, toward the summary's largest.
    fn noted(&mut self, reading: MemoryReading, at: SystemTime) {
        if let Some(resident) = reading.resident
            && self.max.is_none_or(|(max, _)| resident > max)
        {
            self.max = Some((resident, at));
        }
        // The largest read, whatever the order the readings are kept in:
        // requests answered on other threads keep theirs out of order.
        if let Some(peak) = reading.peak {
            self.peak = Some(self.peak.map_or(peak, |kept| kept.max(peak)));
        }
    }

    /// Keep `entry` of `bytes` among the [`TOP_REQUESTS`] with the most.
    fn ranked(&mut self, bytes: usize, entry: Value) {
        let at = self.top.partition_point(|(kept, _)| *kept >= bytes);
        if at < TOP_REQUESTS {
            self.top.insert(at, (bytes, entry));
            self.top.truncate(TOP_REQUESTS);
        }
    }

    /// Write `line`; a write that fails is said once on the service's
    /// stderr, and nothing more is written.
    fn write(&mut self, line: &Value) {
        let Some(out) = self.out.as_mut() else {
            return;
        };
        let mut text = line.to_string();
        text.push('\n');
        if let Err(error) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
            warn!(
                "the queue service's record of its first requests could not be written: \
                 {error}; nothing more is recorded"
            );
            self.out = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::UNIX_EPOCH;

    #[test]
    fn deployment_startup_records_use_the_tracing_envelope() {
        let (telemetry, captured) = super::super::telemetry::Telemetry::capture();
        let inner = json!({"record": "request", "op": "events_between", "bytes": 12});
        telemetry.in_scope(|| {
            let mut writer = TracingWriter;
            writer.write_all(format!("{inner}\n").as_bytes()).unwrap();
            writer.flush().unwrap();
        });
        let records = captured.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["message"], "service startup reads");
        assert_eq!(
            serde_json::from_str::<Value>(records[0]["fields"]["startup_reads"].as_str().unwrap())
                .unwrap(),
            inner
        );
    }

    /// A clock a test moves: the wall clock and the monotonic one alike,
    /// unless only the wall clock is stepped.
    struct Stepped(Mutex<(SystemTime, Instant)>);

    impl Stepped {
        fn at(secs: u64) -> Arc<Self> {
            Arc::new(Self(Mutex::new((
                UNIX_EPOCH + Duration::from_secs(secs),
                Instant::now(),
            ))))
        }
        fn advance(&self, by: Duration) {
            let mut now = self.0.lock().unwrap();
            now.0 += by;
            now.1 += by;
        }
        fn step_wall_back(&self, by: Duration) {
            self.0.lock().unwrap().0 -= by;
        }
    }

    impl Clock for Stepped {
        fn system_time(&self) -> SystemTime {
            self.0.lock().unwrap().0
        }
        fn monotonic(&self) -> Instant {
            self.0.lock().unwrap().1
        }
    }

    /// Memory a test sets, read in turn; the last one read again.
    struct Readings(Mutex<Vec<MemoryReading>>);

    impl Readings {
        fn of(residents: &[u64]) -> Box<Self> {
            Box::new(Self(Mutex::new(
                residents
                    .iter()
                    .rev()
                    .map(|&resident| MemoryReading {
                        resident: Some(resident),
                        peak: Some(resident * 2),
                    })
                    .collect(),
            )))
        }

        /// Readings of these peaks in turn, the resident set alike.
        fn peaks(peaks: &[u64]) -> Box<Self> {
            Box::new(Self(Mutex::new(
                peaks
                    .iter()
                    .rev()
                    .map(|&peak| MemoryReading {
                        resident: Some(1),
                        peak: Some(peak),
                    })
                    .collect(),
            )))
        }
    }

    impl ResidentMemory for Readings {
        fn read(&self) -> MemoryReading {
            let mut readings = self.0.lock().unwrap();
            if readings.len() > 1 {
                readings.pop().unwrap()
            } else {
                readings.last().copied().unwrap_or_default()
            }
        }
    }

    /// A platform where the memory cannot be read.
    struct Unread;

    impl ResidentMemory for Unread {
        fn read(&self) -> MemoryReading {
            MemoryReading::default()
        }
    }

    /// Lines written into a buffer the test reads.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn lines(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    /// A writer that always fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    fn answered(use_case: &str, op: Option<&str>, bytes: usize, rows: Option<usize>) -> Answered {
        Answered {
            use_case: Some(use_case.into()),
            op: op.map(Into::into),
            role: Some("supervisor".into()),
            bytes,
            rows,
            heavy_read: None,
        }
    }

    /// Within the window each request is one line with its use case, its
    /// operation, its caller's role, its bytes and rows, how long it took
    /// and the memory before and after; at the window's end one summary
    /// names the largest memory and when, and the requests with the most
    /// bytes, the most first.
    #[test]
    fn each_request_in_the_window_is_a_line_and_its_end_a_summary() {
        let clock = Stepped::at(1_000);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[100, 200, 7_000, 7_500, 300]),
            Duration::from_secs(60),
        );
        let began = reads.begin().unwrap();
        clock.advance(Duration::from_millis(250));
        reads.answered(
            began,
            &answered("supervisor", Some("ledger_heads"), 4_096, Some(12)),
        );
        clock.advance(Duration::from_secs(1));
        let began = reads.begin().unwrap();
        clock.advance(Duration::from_millis(5));
        reads.answered(began, &answered("show", None, 64, None));
        reads.tick();
        assert_eq!(buffer.lines().len(), 3, "no summary before the end");
        clock.advance(Duration::from_secs(60));
        reads.tick();
        let lines = buffer.lines();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["record"], "start");
        assert_eq!(lines[0]["window_secs"], 60);
        assert_eq!(lines[0]["resident"], 100);
        assert_eq!(
            lines[1],
            json!({
                "record": "request",
                "at": "1970-01-01T00:16:40.250Z",
                "use_case": "supervisor",
                "op": "ledger_heads",
                "role": "supervisor",
                "bytes": 4_096,
                "rows": 12,
                "duration_ms": 250,
                "heavy_read_wait_ms": null,
                "heavy_read_refused": false,
                "resident_before": 200,
                "resident_after": 7_000,
                "peak": 14_000,
            })
        );
        assert_eq!(lines[2]["op"], Value::Null);
        assert_eq!(lines[2]["duration_ms"], 5);
        let summary = &lines[3];
        assert_eq!(summary["record"], "summary");
        assert_eq!(summary["reason"], "window_ended");
        assert_eq!(summary["requests"], 2);
        assert_eq!(summary["max_resident"], 7_500);
        assert_eq!(summary["max_resident_at"], "1970-01-01T00:16:41.250Z");
        assert_eq!(summary["peak"], 15_000);
        let top = summary["top_by_bytes"].as_array().unwrap();
        assert_eq!(
            top.iter().map(|entry| &entry["bytes"]).collect::<Vec<_>>(),
            [&json!(4_096), &json!(64)]
        );
        assert_eq!(top[0]["op"], "ledger_heads");
    }

    /// Past the window no request is recorded, and the summary is written
    /// once, by the first look past it; a stop within the window writes it
    /// then. The top keeps the requests with the most bytes only.
    #[test]
    fn past_the_window_nothing_is_recorded_and_the_summary_is_once() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[1]),
            Duration::from_secs(10),
        );
        for bytes in 0..(TOP_REQUESTS + 5) {
            let began = reads.begin().unwrap();
            reads.answered(began, &answered("show", None, bytes, None));
        }
        clock.advance(Duration::from_secs(10));
        assert!(reads.begin().is_none());
        assert!(reads.begin().is_none());
        reads.tick();
        reads.stopped("stopped");
        let lines = buffer.lines();
        assert_eq!(lines.len(), 1 + TOP_REQUESTS + 5 + 1);
        let summary = lines.last().unwrap();
        assert_eq!(summary["reason"], "window_ended");
        let top: Vec<_> = summary["top_by_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["bytes"].as_u64().unwrap())
            .collect();
        assert_eq!(
            top,
            (5..(TOP_REQUESTS as u64 + 5)).rev().collect::<Vec<_>>()
        );

        let stopped = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(stopped.clone())),
            Stepped::at(0),
            Readings::of(&[1]),
            Duration::from_secs(10),
        );
        reads.stopped("stopped");
        assert!(reads.begin().is_none());
        assert_eq!(stopped.lines().last().unwrap()["reason"], "stopped");
    }

    /// A step of the wall clock back neither keeps the window open past its
    /// length nor makes a duration of it.
    #[test]
    fn a_step_of_the_wall_clock_moves_neither_the_window_nor_a_duration() {
        let clock = Stepped::at(10_000);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[1]),
            Duration::from_secs(10),
        );
        let began = reads.begin().unwrap();
        clock.step_wall_back(Duration::from_secs(3_600));
        clock.advance(Duration::from_millis(40));
        reads.answered(began, &answered("show", None, 1, None));
        clock.advance(Duration::from_secs(10));
        assert!(reads.begin().is_none());
        let lines = buffer.lines();
        assert_eq!(lines[1]["duration_ms"], 40);
        assert_eq!(lines.last().unwrap()["reason"], "window_ended");
    }

    /// A window that ends while requests are in flight writes its summary
    /// only once the last of them is answered, each of them a line and
    /// counted, even when the service's loop looked past the window before
    /// they were answered; no request begins to be recorded after its end.
    #[test]
    fn the_window_s_summary_waits_for_the_requests_in_flight() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[1]),
            Duration::from_secs(10),
        );
        let first = reads.begin().unwrap();
        let second = reads.begin().unwrap();
        clock.advance(Duration::from_secs(11));
        reads.tick();
        assert!(reads.begin().is_none());
        assert_eq!(buffer.lines().len(), 1, "no summary with two in flight");
        reads.answered(
            first,
            &answered("supervisor", Some("ledger_rows"), 900, Some(9)),
        );
        reads.tick();
        let lines = buffer.lines();
        assert_eq!(lines.len(), 2, "no summary with one in flight: {lines:?}");
        assert_eq!(lines[1]["op"], "ledger_rows");
        reads.answered(
            second,
            &answered("supervisor", Some("events_between"), 50, Some(2)),
        );
        let lines = buffer.lines();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[2]["op"], "events_between");
        let summary = &lines[3];
        assert_eq!(summary["record"], "summary");
        assert_eq!(summary["reason"], "window_ended");
        assert_eq!(summary["requests"], 2);
        assert_eq!(summary["unfinished"], 0);
        let ops: Vec<_> = summary["top_by_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["op"].clone())
            .collect();
        assert_eq!(ops, [json!("ledger_rows"), json!("events_between")]);
        reads.tick();
        assert_eq!(buffer.lines().len(), 4, "the summary is once");
    }

    /// A stop does not wait for the requests in flight: its summary counts
    /// them as unfinished, and their answers after it write nothing.
    #[test]
    fn a_stop_summarizes_at_once_with_the_requests_unfinished() {
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            Stepped::at(0),
            Readings::of(&[1]),
            Duration::from_secs(10),
        );
        let began = reads.begin().unwrap();
        reads.stopped("queue_gone");
        reads.answered(began, &answered("show", None, 1, None));
        let lines = buffer.lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1]["reason"], "queue_gone");
        assert_eq!(lines[1]["unfinished"], 1);
    }

    /// The summary's peak is the largest read, though the requests keep
    /// their readings in another order than they were read: a reading kept
    /// last but smaller does not lower it.
    #[test]
    fn the_peak_does_not_go_down_when_readings_are_kept_out_of_order() {
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            Stepped::at(0),
            // The start, the first's before, the second's before and
            // after, then the first's after.
            Readings::peaks(&[5, 10, 50, 60, 20]),
            Duration::from_secs(10),
        );
        let first = reads.begin().unwrap();
        let second = reads.begin().unwrap();
        reads.answered(second, &answered("show", None, 1, None));
        reads.answered(first, &answered("show", None, 1, None));
        reads.stopped("stopped");
        let lines = buffer.lines();
        assert_eq!(
            lines[2]["peak"], 20,
            "the first's own line keeps its reading"
        );
        assert_eq!(lines.last().unwrap()["peak"], 60);
    }

    /// A request that began in the window and ends past it is recorded,
    /// and closes the window with the summary.
    #[test]
    fn a_request_that_ends_past_the_window_closes_it() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[1]),
            Duration::from_secs(10),
        );
        let began = reads.begin().unwrap();
        clock.advance(Duration::from_secs(11));
        reads.answered(
            began,
            &answered("supervisor", Some("events_between"), 9, Some(3)),
        );
        let lines = buffer.lines();
        assert_eq!(lines[1]["record"], "request");
        assert_eq!(lines[2]["record"], "summary");
        assert_eq!(lines[2]["requests"], 1);
    }

    /// Memory that cannot be read is `null` in every line, and a writer
    /// that fails stops the record without a panic: the caller's work goes
    /// on the same.
    #[test]
    fn unread_memory_is_null_and_a_failed_write_stops_only_the_record() {
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            Stepped::at(0),
            Box::new(Unread),
            Duration::from_secs(10),
        );
        let began = reads.begin().unwrap();
        reads.answered(began, &answered("show", None, 1, None));
        reads.stopped("stopped");
        let lines = buffer.lines();
        assert_eq!(lines[0]["resident"], Value::Null);
        assert_eq!(lines[1]["resident_before"], Value::Null);
        assert_eq!(lines[1]["resident_after"], Value::Null);
        assert_eq!(lines[2]["max_resident"], Value::Null);
        assert_eq!(lines[2]["max_resident_at"], Value::Null);

        let broken = StartupReads::new(
            Some(Box::new(Broken)),
            Stepped::at(0),
            Box::new(Unread),
            Duration::from_secs(10),
        );
        let began = broken.begin().unwrap();
        broken.answered(began, &answered("show", None, 1, None));
        broken.stopped("stopped");
        assert!(broken.lock().out.is_none());
    }

    /// The operation a request forwards is a `supervisor` request's `op`
    /// and a `control` step's; the rows are an array result's length.
    #[test]
    fn the_op_and_the_rows_are_read_from_the_request_and_the_result() {
        assert_eq!(
            Answered::op_of("supervisor", &json!({"op": "ledger_heads", "args": []})),
            Some("ledger_heads".into())
        );
        assert_eq!(
            Answered::op_of(
                "control",
                &json!({"control": {}, "call": {"op": "events_between"}})
            ),
            Some("events_between".into())
        );
        assert_eq!(Answered::op_of("show", &json!({"op": "x"})), None);
        assert_eq!(Answered::rows_of(Some(&json!([1, 2, 3]))), Some(3));
        assert_eq!(Answered::rows_of(Some(&json!({"a": 1}))), None);
        assert_eq!(Answered::rows_of(None), None);
    }

    /// Linux's status gives the resident set and its high-water mark in
    /// kB, read as bytes; a missing line is `None`.
    #[test]
    fn a_proc_status_reads_as_bytes() {
        let status = "Name:\tdagq\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\n";
        assert_eq!(
            proc_status_memory(status),
            MemoryReading {
                resident: Some(1024 * 1024),
                peak: Some(2048 * 1024),
            }
        );
        assert_eq!(
            proc_status_memory("Name:\tdagq\n"),
            MemoryReading::default()
        );
    }

    /// The system's reading of this process: on the platforms it reads,
    /// what it holds now is some and no more than its peak.
    #[test]
    fn this_process_reads_its_own_memory() {
        let reading = OwnMemory.read();
        if cfg!(any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android"
        )) {
            let (resident, peak) = (reading.resident.unwrap(), reading.peak.unwrap());
            assert!(resident > 0 && resident <= peak, "{reading:?}");
        }
    }

    /// The record opens its own file in the queue's `logs/`, named as the
    /// runtime's other log files so that their pruning takes it too.
    #[test]
    fn the_record_is_a_file_of_the_queue_s_logs() {
        let dir = tempfile::tempdir().unwrap();
        let reads = StartupReads::open(dir.path(), Stepped::at(0), Box::new(Unread));
        reads.stopped("stopped");
        let files: Vec<_> = fs::read_dir(dir.path().join("logs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            files,
            [format!(
                "service-19700101T000000Z-{}.jsonl",
                std::process::id()
            )]
        );
        assert!(super::super::telemetry::log_file_owner(&files[0]).is_some());
        let text = fs::read_to_string(dir.path().join("logs").join(&files[0])).unwrap();
        assert_eq!(text.lines().count(), 2);
        // A queue dir it cannot write in records nothing and stops nothing.
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "").unwrap();
        let none = StartupReads::open(&file, Stepped::at(0), Box::new(Unread));
        let began = none.begin().unwrap();
        none.answered(began, &answered("show", None, 1, None));
    }
}
