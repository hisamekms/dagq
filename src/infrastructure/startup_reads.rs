//! The queue service's lifetime record of answered requests ([`StartupReads`])
//! and its own memory ([`ResidentMemory`]). Periodic aggregates identify
//! the work behind read volume, latency and memory peaks during both the
//! supervisor's startup catch-up and steady operation.
//!
//! Requests are aggregated throughout the process lifetime into monotonic
//! periods, minute bins during [`STARTUP_WINDOW`], then ten-minute bins.
//! A completed request belongs to its answer's period, including both of
//! its memory samples; long requests expose their full duration maximum.
//! Periods without requests write nothing. Ticks and answers flush elapsed
//! periods, and shutdown flushes the partial period. Local records stay in
//! one file; deployments use the tracing envelope below. The startup summary
//! remains once, after its in-flight answers.
//!
//! Per-request startup lines are disabled by default. At service start,
//! `[service_log] startup_requests = true` in the queue's `host.toml`
//! replaces the host-wide table (`$XDG_CONFIG_HOME/dagq/host.toml`, or
//! `~/.config/dagq/host.toml`). This temporary investigation switch needs
//! no repository landing. Unreadable or malformed settings disable it,
//! with one stderr warning; they never prevent startup or change answers.
//! Even when enabled, only requests begun inside the startup window get
//! request lines. Remove the setting after investigation: each restart can
//! otherwise add over 100 MB. Slow-request logging is independent.
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
//! - `by_caller`: one line for each bounded (use_case, op, role, caller)
//!   group: calls, bytes and duration_ms sums, max_bytes, max_duration_ms,
//!   rows sum, heavy_read_wait_ms maximum and heavy_read_refused count.
//!   `at` and `period_secs` match its `period` line. See [`periods`] for
//!   label validation, cardinality bounds and interval rationale.
//! - `period`: at, period_secs, requests, max_resident and peak (maxima of
//!   samples associated with the completed requests of this period).
//! - `request` (investigation setting only): `at` (when its answer was built, before it is sent),
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
//!   `bytes`, `rows`, `caller` and `duration_ms`. `top_by_calls` names the
//!   grouped calls, bytes and duration_ms totals across the whole startup
//!   window, descending by calls, under a key limit of its own that a
//!   caller first seen in a later period does not hit before a period's.
//!   Both request and summary entries include the normalized caller.
//!
//! The memory is the process's own in bytes ([`MemoryReading`]): what it
//! holds now (`resident`) and the most it held since it started (`peak`),
//! each `null` where it cannot be read. The record never changes an
//! answer: a request is answered the same whether the file could not be
//! opened, a line could not be written or the memory could not be read.

mod periods;

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

/// Read the queue-local `[service_log]` table before the host-wide one,
/// as release_update's `load_host_update` does. An invalid file fails
/// closed, emits one warning and never changes request handling.
pub fn load_request_lines(queue_dir: &Path, host_wide: Option<&Path>) -> bool {
    load_request_lines_with(queue_dir, host_wide, |path| fs::read_to_string(path))
}

pub(super) fn load_request_lines_with(
    queue_dir: &Path,
    host_wide: Option<&Path>,
    read_file: impl Fn(&Path) -> std::io::Result<String>,
) -> bool {
    let read = || -> Result<bool, String> {
        for path in [Some(queue_dir.join("host.toml").as_path()), host_wide]
            .into_iter()
            .flatten()
        {
            let text = match read_file(path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            if let Some(value) =
                parse_request_lines(&text).map_err(|e| format!("{}: {e}", path.display()))?
            {
                return Ok(value);
            }
        }
        Ok(false)
    };
    read().unwrap_or_else(|error| {
        warn!("service log host.toml: {error}; startup request lines disabled");
        false
    })
}

/// Other host tables are opaque, as in load_host_update; only this
/// investigation table's entries are interpreted here.
fn parse_request_lines(text: &str) -> Result<Option<bool>, &'static str> {
    let mut active = false;
    let mut found = None;
    let mut value_seen = false;
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = super::run_env::strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = header.strip_suffix(']').map(str::trim);
            // Recognize the target even with missing/extra brackets. A
            // broken local table must disable the switch, not inherit a
            // host-wide true or retain a preceding valid table's true.
            let target = header
                .trim_start_matches('[')
                .trim_start()
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .next();
            if target == Some("service_log") && name != Some("service_log") {
                return Err("malformed service_log table");
            }
            active = name == Some("service_log");
            if active {
                if found.is_some() {
                    return Err("duplicate service_log table");
                }
                found = Some(false);
            }
        } else if active {
            let (key, value) = line.split_once('=').ok_or("expected KEY = value")?;
            if key.trim() != "startup_requests" || value_seen {
                return Err("unknown or duplicate service_log key");
            }
            found = Some(match value.trim() {
                "true" => true,
                "false" => false,
                _ => return Err("startup_requests must be a boolean"),
            });
            value_seen = true;
        }
    }
    Ok(found)
}

/// The startup summary and opt-in request-line window: 15
/// minutes. A supervisor that starts with the service catches up in its
/// first minutes (its first observation pass and the ledger's builder
/// read the queue's history then), and the service's memory was seen to
/// peak within 3 minutes and settle by 12 (2026-11-03); the window holds
/// that with room, and ends before the record grows with the requests of
/// an ordinary day. Periodic aggregates continue after it.
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
    pub caller: Option<String>,
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
    startup: bool,
}

/// The lifetime record of the service's answers (see the
/// module doc): one per service, shared by the threads that answer.
pub struct StartupReads {
    clock: Arc<dyn Clock>,
    memory: Box<dyn ResidentMemory>,
    started: Instant,
    window: Duration,
    /// Sample monotonic time while holding this lock: startup admission,
    /// period advancement and answer registration share the same order.
    state: Mutex<State>,
    request_lines: bool,
}

struct State {
    out: Option<Box<dyn Write + Send>>,
    periods: periods::Periods,
    calls: periods::StartupCalls,
    stopped: bool,
    /// Whether the summary is still to be written.
    open: bool,
    /// Why the startup window ended: the summary stops admitting new
    /// requests and waits for its startup requests still in flight.
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
        Self::open_with_mode(
            queue_dir,
            clock,
            memory,
            super::telemetry::deployment_streams(|name| std::env::var_os(name)),
        )
    }

    /// Both sinks share the startup switch. Supplying the selected mode
    /// also lets adapter tests exercise opening without changing env.
    fn open_with_mode(
        queue_dir: &Path,
        clock: Arc<dyn Clock>,
        memory: Box<dyn ResidentMemory>,
        deployment: bool,
    ) -> Self {
        let request_lines =
            load_request_lines(queue_dir, super::kpi_config::host_wide_file().as_deref());
        if deployment {
            return Self::configured(
                Some(Box::new(TracingWriter)),
                clock,
                memory,
                STARTUP_WINDOW,
                request_lines,
            );
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
                    "the queue service's record of its requests {} could not be opened: \
                     {error}; nothing is recorded",
                    path.display()
                );
            })
            .ok()
            .map(|file| Box::new(file) as Box<dyn Write + Send>);
        Self::configured(out, clock, memory, STARTUP_WINDOW, request_lines)
    }

    /// A lifetime record into `out` (none: nothing written), with a
    /// startup summary window of `window`; begins with its `start` line.
    pub fn new(
        out: Option<Box<dyn Write + Send>>,
        clock: Arc<dyn Clock>,
        memory: Box<dyn ResidentMemory>,
        window: Duration,
    ) -> Self {
        Self::configured(out, clock, memory, window, false)
    }

    /// Like `new`, with the investigation switch already read at startup.
    pub fn configured(
        out: Option<Box<dyn Write + Send>>,
        clock: Arc<dyn Clock>,
        memory: Box<dyn ResidentMemory>,
        window: Duration,
        request_lines: bool,
    ) -> Self {
        let started = clock.system_time();
        let reading = memory.read();
        let reads = Self {
            started: clock.monotonic(),
            clock,
            memory,
            window,
            request_lines,
            state: Mutex::new(State {
                out,
                periods: periods::Periods::new(window),
                calls: periods::StartupCalls::new(window),
                stopped: false,
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

    /// Begin a lifetime observation; startup membership is fixed now.
    pub fn begin(&self) -> Option<Began> {
        self.tick();
        let mut state = self.lock();
        if state.stopped {
            return None;
        }
        let since = self.clock.monotonic();
        let startup = state.open && state.ending.is_none() && !self.past_window(since);
        if startup {
            state.in_flight += 1;
        }
        drop(state);
        Some(Began {
            at: self.clock.system_time(),
            since,
            before: self.memory.read(),
            startup,
        })
    }

    /// Record an answer without changing it. Aggregates continue after
    /// the startup summary; request lines are explicitly opted in.
    pub fn answered(&self, began: Began, answered: &Answered) {
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        let after = self.memory.read();
        let at = self.clock.system_time();
        let now = self.clock.monotonic();
        let duration_ms = u64::try_from(now.saturating_duration_since(began.since).as_millis())
            .unwrap_or(u64::MAX);
        let elapsed = now.saturating_duration_since(self.started);
        for line in state.periods.advance(elapsed, &timestamp(at)) {
            state.write(&line);
        }
        state
            .periods
            .add(answered, duration_ms, [began.before, after]);
        if began.startup && state.open {
            state.in_flight = state.in_flight.saturating_sub(1);
            state.requests += 1;
            state.noted(began.before, began.at);
            state.noted(after, at);
            state.calls.add(answered, duration_ms);
            let entry = json!({
                "at": timestamp(at), "use_case": answered.use_case,
                "op": periods::label(answered.op.as_deref()), "role": answered.role,
                "caller": periods::label(answered.caller.as_deref()),
                "bytes": answered.bytes, "rows": answered.rows, "duration_ms": duration_ms,
            });
            if self.request_lines {
                let mut line = entry.clone();
                line["record"] = json!("request");
                line["heavy_read_wait_ms"] = json!(answered.heavy_read.map(|note| note.waited_ms));
                line["heavy_read_refused"] =
                    json!(answered.heavy_read.is_some_and(|note| note.refused));
                line["resident_before"] = json!(began.before.resident);
                line["resident_after"] = json!(after.resident);
                line["peak"] = json!(after.peak);
                state.write(&line);
            }
            state.ranked(answered.bytes, entry);
        }
        if self.past_window(now) {
            self.end(&mut state, "window_ended");
        }
    }

    /// Flush elapsed periods even without a newly answered request.
    pub fn tick(&self) {
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        let now = self.clock.monotonic();
        for line in state.periods.advance(
            now.saturating_duration_since(self.started),
            &timestamp(self.clock.system_time()),
        ) {
            state.write(&line);
        }
        if self.past_window(now) {
            self.end(&mut state, "window_ended");
        }
    }

    /// Flush the partial period and the startup summary on shutdown.
    pub fn stopped(&self, reason: &str) {
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        let now = self
            .clock
            .monotonic()
            .saturating_duration_since(self.started);
        let at = timestamp(self.clock.system_time());
        for line in state.periods.advance(now, &at) {
            state.write(&line);
        }
        for line in state.periods.flush(now, &at) {
            state.write(&line);
        }
        if state.ending.is_none() {
            state.ending = Some(reason.to_owned());
        }
        self.summarize(&mut state);
        state.stopped = true;
    }

    /// End the startup window (`reason` kept from its first end) and
    /// write its summary once no startup request remains in flight.
    fn end(&self, state: &mut State, reason: &str) {
        if state.ending.is_none() {
            state.ending = Some(reason.to_owned());
        }
        if state.in_flight == 0 {
            self.summarize(state);
        }
    }

    /// Write the startup summary once; lifetime aggregation continues.
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
            "top_by_calls": state.calls.top(),
            "top_by_bytes": state.top.iter().map(|(_, entry)| entry.clone()).collect::<Vec<_>>(),
        });
        state.write(&line);
        state.top = Vec::new();
        state.calls.clear();
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
                "the queue service's record of its requests could not be written: \
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
    use std::sync::{TryLockError, mpsc};
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

    const WAIT_LIMIT: Duration = Duration::from_secs(10);

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

    struct ClockPause {
        remaining: usize,
        sampled: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }

    /// Pause after capturing a monotonic reading, before returning it to
    /// the recorder, so a tick can contend at exactly that boundary.
    struct PausedClock {
        now: Arc<Stepped>,
        pause: Mutex<Option<ClockPause>>,
    }

    impl PausedClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: Stepped::at(0),
                pause: Mutex::new(None),
            })
        }
    }

    impl Clock for PausedClock {
        fn system_time(&self) -> SystemTime {
            self.now.system_time()
        }

        fn monotonic(&self) -> Instant {
            let now = self.now.monotonic();
            let pause = {
                let mut pause = self.pause.lock().unwrap();
                if let Some(armed) = pause.as_mut() {
                    armed.remaining -= 1;
                    if armed.remaining == 0 {
                        pause.take()
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(pause) = pause {
                pause.sampled.send(()).unwrap();
                pause
                    .resume
                    .recv_timeout(WAIT_LIMIT)
                    .expect("paused recorder clock: waiting to resume its captured time");
            }
            now
        }
    }

    /// Start a boundary tick after the operation has sampled its time,
    /// while it is paused before registering that observation. The state
    /// lock must keep the tick from flushing ahead of that registration.
    fn tick_during_sample<T: Send + 'static>(
        reads: Arc<StartupReads>,
        clock: &PausedClock,
        sample: usize,
        advance: Duration,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (sampled, sampling) = mpsc::channel();
        let (resume, resuming) = mpsc::channel();
        *clock.pause.lock().unwrap() = Some(ClockPause {
            remaining: sample,
            sampled,
            resume: resuming,
        });
        let (returned, returning) = mpsc::channel();
        let recording = std::thread::spawn(move || returned.send(operation()).unwrap());
        sampling
            .recv_timeout(WAIT_LIMIT)
            .expect("recorder boundary test: waiting for the captured time");
        clock.now.advance(advance);
        let held = matches!(reads.state.try_lock(), Err(TryLockError::WouldBlock));
        let (tick, ticking) = mpsc::channel();
        let flush = std::thread::spawn(move || {
            tick.send(false).unwrap();
            reads.tick();
            tick.send(true).unwrap();
        });
        assert!(!ticking.recv_timeout(WAIT_LIMIT).expect("tick must start"));
        resume.send(()).unwrap();
        let result = returning
            .recv_timeout(WAIT_LIMIT)
            .expect("recorder boundary test: waiting for observation registration");
        recording.join().unwrap();
        assert!(
            ticking
                .recv_timeout(WAIT_LIMIT)
                .expect("recorder boundary test: waiting for the boundary tick")
        );
        flush.join().unwrap();
        assert!(held, "time sampling must hold the recorder's state lock");
        result
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

    fn answered(caller: &str, bytes: usize) -> Answered {
        Answered {
            use_case: Some("supervisor".into()),
            op: Some("ledger_row".into()),
            caller: Some(caller.into()),
            bytes,
            rows: Some(2),
            ..Answered::default()
        }
    }

    fn record(reads: &StartupReads, clock: &Stepped, caller: &str, bytes: usize) {
        let began = reads.begin().unwrap();
        clock.advance(Duration::from_millis(25));
        reads.answered(began, &answered(caller, bytes));
    }

    #[test]
    fn deployment_periods_keep_callers_and_honor_the_host_request_switch() {
        for enabled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join("host.toml"),
                format!("[service_log]\nstartup_requests = {enabled}\n"),
            )
            .unwrap();
            let clock = Stepped::at(0);
            let (telemetry, captured) = super::super::telemetry::Telemetry::capture();
            telemetry.in_scope(|| {
                let reads =
                    StartupReads::open_with_mode(dir.path(), clock.clone(), Box::new(Unread), true);
                record(&reads, &clock, "deployment.startup", 12);
                clock.advance(STARTUP_WINDOW);
                reads.tick();
                record(&reads, &clock, "deployment.steady", 9);
                clock.advance(Duration::from_secs(600));
                reads.tick();
                reads.stopped("stopped");
            });
            assert!(!dir.path().join("logs").exists());
            let lines: Vec<Value> = captured
                .records()
                .iter()
                .map(|line| {
                    assert_eq!(line["message"], "service startup reads");
                    serde_json::from_str(line["fields"]["startup_reads"].as_str().unwrap()).unwrap()
                })
                .collect();
            let requests: Vec<_> = lines.iter().filter(|v| v["record"] == "request").collect();
            assert_eq!(requests.len(), usize::from(enabled));
            if enabled {
                assert_eq!(requests[0]["caller"], "deployment.startup");
            }
            let groups: Vec<_> = lines
                .iter()
                .filter(|v| v["record"] == "by_caller")
                .collect();
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0]["caller"], "deployment.startup");
            assert_eq!(groups[0]["period_secs"], 60.0);
            assert_eq!(groups[1]["caller"], "deployment.steady");
            assert_eq!(groups[1]["period_secs"], 600.0);
            assert!(groups.iter().all(|v| v["calls"] == 1));
            assert_eq!(lines.iter().filter(|v| v["record"] == "period").count(), 2);
            let summaries: Vec<_> = lines.iter().filter(|v| v["record"] == "summary").collect();
            assert_eq!(summaries.len(), 1);
            assert_eq!(
                summaries[0]["top_by_calls"][0]["caller"],
                "deployment.startup"
            );
        }
    }

    #[test]
    fn default_periods_continue_and_summary_ranks_calls_and_bytes() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[100, 200, 7000, 500]),
            STARTUP_WINDOW,
        );
        record(&reads, &clock, "sweep", 4000);
        record(&reads, &clock, "restore", 10);
        record(&reads, &clock, "restore", 10);
        assert_eq!(buffer.lines().len(), 1);
        clock.advance(Duration::from_secs(60));
        reads.tick();
        let lines = buffer.lines();
        let groups: Vec<_> = lines
            .iter()
            .filter(|v| v["record"] == "by_caller")
            .collect();
        assert_eq!(groups.len(), 2);
        let restore = groups.iter().find(|v| v["caller"] == "restore").unwrap();
        assert_eq!(restore["calls"], 2);
        assert_eq!(restore["rows"], 4);
        assert_eq!(restore["duration_ms"], 50);
        assert_eq!(restore["max_duration_ms"], 25);
        assert_eq!(restore["max_bytes"], 10);
        let period = lines.iter().find(|v| v["record"] == "period").unwrap();
        assert_eq!(period["period_secs"], 60.0);
        assert_eq!(period["requests"], 3);
        assert_eq!(period["max_resident"], 7000);
        assert_eq!(period["peak"], 14000);
        let count = lines.len();
        clock.advance(STARTUP_WINDOW);
        reads.tick();
        reads.tick();
        let lines = buffer.lines();
        assert_eq!(lines.len(), count + 1);
        let summary = lines.last().unwrap();
        assert_eq!(summary["record"], "summary");
        assert_eq!(summary["requests"], 3);
        assert_eq!(summary["unfinished"], 0);
        assert_eq!(summary["max_resident"], 7000);
        assert!(!summary["max_resident_at"].is_null());
        assert_eq!(summary["top_by_bytes"][0]["caller"], "sweep");
        assert_eq!(summary["top_by_calls"][0]["caller"], "restore");
        assert_eq!(summary["top_by_calls"][0]["bytes"], 20);
        record(&reads, &clock, "steady", 50);
        clock.advance(Duration::from_secs(600));
        reads.tick();
        let lines = buffer.lines();
        assert_eq!(lines.iter().filter(|v| v["record"] == "summary").count(), 1);
        assert_eq!(lines.last().unwrap()["period_secs"], 600.0);
        assert!(lines.iter().all(|v| v["record"] != "request"));
        record(&reads, &clock, "partial", 1);
        reads.stopped("stopped");
        let lines = buffer.lines();
        assert_eq!(lines[lines.len() - 2]["caller"], "partial");
        assert!(lines.last().unwrap()["period_secs"].as_f64().unwrap() < 600.0);
        let count = lines.len();
        reads.stopped("stopped");
        reads.tick();
        assert!(reads.begin().is_none());
        assert_eq!(buffer.lines().len(), count);
    }

    #[test]
    fn startup_call_ranking_keeps_new_callers_across_periods() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Box::new(Unread),
            STARTUP_WINDOW,
        );
        for n in 0..periods::MAX_KEYS - 1 {
            record(&reads, &clock, &format!("first{n}"), 1);
        }
        clock.advance(periods::STARTUP_PERIOD);
        reads.tick();
        for _ in 0..100 {
            record(&reads, &clock, "laterA", 2);
        }
        for _ in 0..200 {
            record(&reads, &clock, "laterB", 3);
        }
        clock.advance(periods::STARTUP_PERIOD);
        reads.tick();
        clock.advance(STARTUP_WINDOW);
        reads.tick();
        let lines = buffer.lines();
        let summary = lines.last().unwrap();
        assert_eq!(summary["record"], "summary");
        assert_eq!(summary["requests"], periods::MAX_KEYS - 1 + 300);
        let top = summary["top_by_calls"].as_array().unwrap();
        assert_eq!(top.len(), TOP_REQUESTS);
        assert_eq!(top[0]["caller"], "laterB");
        assert_eq!(top[0]["calls"], 200);
        assert_eq!(top[0]["bytes"], 600);
        assert_eq!(top[0]["duration_ms"], 5000);
        assert_eq!(top[1]["caller"], "laterA");
        assert_eq!(top[1]["calls"], 100);
        assert_eq!(top[1]["bytes"], 200);
        assert_eq!(top[1]["duration_ms"], 2500);
        assert!(top.iter().all(|row| row["caller"] != "other"));
        assert_eq!(
            lines.iter().filter(|row| row["record"] == "period").count(),
            2
        );
        assert!(
            lines
                .iter()
                .filter(|row| row["record"] == "by_caller")
                .all(|row| row["caller"] != "other")
        );
    }

    #[test]
    fn startup_summary_keeps_only_the_largest_requests() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Box::new(Unread),
            STARTUP_WINDOW,
        );
        for bytes in 0..TOP_REQUESTS + 5 {
            record(&reads, &clock, "same", bytes);
        }
        reads.stopped("stopped");
        let lines = buffer.lines();
        let summary = lines.last().unwrap();
        let bytes: Vec<_> = summary["top_by_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["bytes"].as_u64().unwrap())
            .collect();
        assert_eq!(
            bytes,
            (5..TOP_REQUESTS as u64 + 5).rev().collect::<Vec<_>>()
        );
        assert_eq!(summary["top_by_calls"][0]["calls"], TOP_REQUESTS + 5);
    }

    #[test]
    fn investigation_lines_only_for_requests_started_inside_window() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let enabled = load_request_lines_with(Path::new("queue"), None, |_| {
            Ok("[service_log]\nstartup_requests = true\n".into())
        });
        let reads = StartupReads::configured(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Box::new(Unread),
            Duration::from_secs(10),
            enabled,
        );
        let first = reads.begin().unwrap();
        clock.step_wall_back(Duration::from_secs(3600));
        clock.advance(Duration::from_secs(11));
        reads.tick();
        assert_eq!(
            buffer.lines().len(),
            1,
            "summary waits for in-flight requests"
        );
        reads.answered(first, &answered("restore", 10));
        record(&reads, &clock, "late", 1);
        reads.stopped("stopped");
        let lines = buffer.lines();
        let requests: Vec<_> = lines.iter().filter(|v| v["record"] == "request").collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["caller"], "restore");
        assert_eq!(requests[0]["duration_ms"], 11000);
        let summaries: Vec<_> = lines.iter().filter(|v| v["record"] == "summary").collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["requests"], 1);
        assert_eq!(summaries[0]["reason"], "window_ended");
    }

    #[test]
    fn startup_registration_cannot_follow_its_summary() {
        let clock = PausedClock::new();
        let buffer = Buffer::default();
        let reads = Arc::new(StartupReads::configured(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Box::new(Unread),
            Duration::from_secs(10),
            true,
        ));
        clock.now.advance(Duration::from_millis(9999));
        let beginning = reads.clone();
        // begin first ticks, then samples the time of startup admission.
        let began = tick_during_sample(
            reads.clone(),
            &clock,
            2,
            Duration::from_millis(1),
            move || beginning.begin().unwrap(),
        );
        assert!(began.startup);
        assert!(buffer.lines().iter().all(|v| v["record"] != "summary"));
        reads.answered(began, &answered("boundary", 12));
        let lines = buffer.lines();
        let requests: Vec<_> = lines.iter().filter(|v| v["record"] == "request").collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["caller"], "boundary");
        let summaries: Vec<_> = lines.iter().filter(|v| v["record"] == "summary").collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["requests"], 1);
        assert_eq!(summaries[0]["unfinished"], 0);
        assert_eq!(summaries[0]["top_by_calls"][0]["caller"], "boundary");
    }

    #[test]
    fn answer_time_and_memory_stay_in_the_period_that_accepts_them() {
        let clock = PausedClock::new();
        let buffer = Buffer::default();
        let reads = Arc::new(StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock.clone(),
            Readings::of(&[100, 200, 900, 300, 400]),
            STARTUP_WINDOW,
        ));
        let began = reads.begin().unwrap();
        clock.now.advance(Duration::from_millis(59999));
        let answering = reads.clone();
        tick_during_sample(
            reads.clone(),
            &clock,
            1,
            Duration::from_millis(1),
            move || {
                answering.answered(began, &answered("boundary", 12));
            },
        );
        record(&reads, &clock.now, "next", 3);
        clock.now.advance(Duration::from_secs(60));
        reads.tick();
        let lines = buffer.lines();
        let groups: Vec<_> = lines
            .iter()
            .filter(|v| v["record"] == "by_caller")
            .collect();
        let periods: Vec<_> = lines.iter().filter(|v| v["record"] == "period").collect();
        assert_eq!(groups.len(), 2);
        assert_eq!(periods.len(), 2);
        assert_eq!(groups[0]["caller"], "boundary");
        assert_eq!(groups[0]["duration_ms"], 59999);
        assert_eq!(groups[1]["caller"], "next");
        for (group, period) in groups.iter().zip(&periods) {
            assert_eq!(group["calls"], 1);
            assert_eq!(group["at"], period["at"]);
            assert_eq!(group["period_secs"], 60.0);
            assert_eq!(period["requests"], 1);
        }
        assert_eq!(periods[0]["max_resident"], 900);
        assert_eq!(periods[0]["peak"], 1800);
        assert_eq!(periods[1]["max_resident"], 400);
        assert_eq!(periods[1]["peak"], 800);
    }

    #[test]
    fn bounded_groups_normalize_labels_and_keep_heavy_read_maxima() {
        use crate::application::heavy_reads::HeavyReadNote;
        assert_eq!(
            periods::label(Some(&"a".repeat(65))).as_deref(),
            Some("other")
        );
        for label in ["", "with space", "日本語", "line\nbreak"] {
            assert_eq!(periods::label(Some(label)).as_deref(), Some("other"));
        }
        assert_eq!(periods::label(None), None);
        let mut periods = periods::Periods::new(STARTUP_WINDOW);
        for n in 0..periods::MAX_KEYS + 10 {
            let mut answer = answered(&format!("caller{n}"), n);
            answer.op = Some("bad\nop".into());
            answer.heavy_read = Some(HeavyReadNote {
                waited_ms: n as u64,
                refused: true,
            });
            periods.add(&answer, n as u64, [MemoryReading::default(); 2]);
        }
        let lines = periods.advance(Duration::from_secs(60), "now");
        assert_eq!(lines.len(), periods::MAX_KEYS + 1);
        let overflow = lines.iter().find(|v| v["caller"] == "other").unwrap();
        assert_eq!(overflow["calls"], 11);
        assert_eq!(overflow["heavy_read_refused"], 11);
        assert_eq!(
            overflow["heavy_read_wait_ms"],
            (periods::MAX_KEYS + 9) as u64
        );
        assert_eq!(overflow["max_duration_ms"], (periods::MAX_KEYS + 9) as u64);
        assert!(
            lines
                .iter()
                .filter(|v| v["caller"] != "other" && v["record"] == "by_caller")
                .all(|v| v["op"] == "other")
        );
        assert!(periods.advance(Duration::from_secs(120), "now").is_empty());
    }

    #[test]
    fn startup_call_ranking_merges_keys_only_past_what_its_periods_can_name() {
        // A minute window spans one startup period and the steady one its
        // late answers land in: two periods' named keys, then `other`.
        let mut calls = periods::StartupCalls::new(Duration::from_secs(60));
        let named = 2 * (periods::MAX_KEYS - 1);
        for n in 0..named {
            calls.add(&answered(&format!("caller{n}"), 1), 1);
        }
        for n in 0..5 {
            calls.add(&answered(&format!("late{n}"), 1), 1);
        }
        for _ in 0..3 {
            calls.add(&answered("caller0", 1), 1);
        }
        let top = calls.top();
        assert_eq!(top[0]["caller"], "other");
        assert_eq!(top[0]["calls"], 5);
        assert_eq!(top[1]["caller"], "caller0");
        assert_eq!(top[1]["calls"], 4);
        calls.clear();
        assert!(calls.top().is_empty());
    }

    #[test]
    fn stopping_keeps_unfinished_and_peak_samples_never_decrease() {
        let clock = Stepped::at(0);
        let buffer = Buffer::default();
        let reads = StartupReads::new(
            Some(Box::new(buffer.clone())),
            clock,
            Readings::peaks(&[5, 10, 50, 60, 20, 1]),
            STARTUP_WINDOW,
        );
        let first = reads.begin().unwrap();
        let second = reads.begin().unwrap();
        reads.answered(second, &answered("second", 1));
        reads.answered(first, &answered("first", 1));
        let unfinished = reads.begin().unwrap();
        reads.stopped("queue_gone");
        reads.answered(unfinished, &answered("unfinished", 1));
        let lines = buffer.lines();
        let summary = lines.last().unwrap();
        assert_eq!(summary["unfinished"], 1);
        assert_eq!(summary["peak"], 60);
        assert_eq!(summary["reason"], "queue_gone");
    }

    #[test]
    fn unavailable_writer_and_memory_do_not_stop_lifetime_recording() {
        let clock = Stepped::at(0);
        for out in [None, Some(Box::new(Broken) as Box<dyn Write + Send>)] {
            let reads = StartupReads::new(out, clock.clone(), Box::new(Unread), STARTUP_WINDOW);
            record(&reads, &clock, "before", 1);
            clock.advance(STARTUP_WINDOW + Duration::from_secs(600));
            reads.tick();
            record(&reads, &clock, "after", 1);
            reads.stopped("stopped");
        }
        assert_eq!(
            proc_status_memory("VmRSS: 2 kB\nVmHWM: 5 kB"),
            MemoryReading {
                resident: Some(2048),
                peak: Some(5120)
            }
        );
    }

    #[test]
    fn host_setting_overrides_and_errors_disable_request_lines() {
        let local = Path::new("queue/host.toml");
        let wide = Path::new("wide.toml");
        let load = |local_text: Option<Result<&str, std::io::ErrorKind>>| {
            load_request_lines_with(Path::new("queue"), Some(wide), |path| {
                if path == local {
                    match &local_text {
                        Some(Ok(text)) => Ok(text.to_string()),
                        Some(Err(kind)) => Err(std::io::Error::from(*kind)),
                        None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                    }
                } else {
                    Ok("[service_log]\nstartup_requests = true\n".into())
                }
            })
        };
        assert!(load(None));
        assert!(load(Some(Ok("[update]\nrelease = \"off\"\n"))));
        assert!(!load(Some(Ok(
            "[service_log]\nstartup_requests = false # done\n"
        ))));
        for text in [
            "[service_log]\nstartup_requests = nope",
            "[service_log",
            "[service_log]\ngarbage",
            "[service_log]\nstartup_requests = true\nstartup_requests = false",
        ] {
            assert!(!load(Some(Ok(text))));
        }
        assert!(!load(Some(Err(std::io::ErrorKind::PermissionDenied))));
        assert!(!load_request_lines_with(
            Path::new("absent"),
            None,
            |_| Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        ));
    }
    /// Unrelated tables use their own grammar, including multiline and
    /// nested arrays. As load_host_update does, judge only our table.
    #[test]
    fn other_host_tables_do_not_disable_startup_request_lines() {
        let unrelated = "[push]\ncommand = [\n \"x\",\n [\n  \"nested\",\n ],\n]\n";
        let setting = "[service_log]\nstartup_requests = true\n";
        for text in [
            format!("{unrelated}{setting}"),
            format!("{setting}{unrelated}"),
        ] {
            assert_eq!(parse_request_lines(&text), Ok(Some(true)));
            assert!(load_request_lines_with(Path::new("queue"), None, |_| Ok(
                text.clone()
            )));
        }
        assert_eq!(parse_request_lines(unrelated), Ok(None));
    }

    #[test]
    fn malformed_service_log_headers_disable_request_lines_without_fallback() {
        let enabled = "[service_log]\nstartup_requests = true\n";
        for text in [
            format!("{enabled}[service_log]]\nstartup_requests = false"),
            "[service_log]]\nstartup_requests = false".into(),
            "[service_log".into(),
            "[[service_log]]\nstartup_requests = false".into(),
            "[ service_log ]]\nstartup_requests = false".into(),
            "[service_log] trailing\nstartup_requests = false".into(),
        ] {
            assert_eq!(
                parse_request_lines(&text),
                Err("malformed service_log table")
            );
            let warnings = Buffer::default();
            let telemetry = super::super::telemetry::Telemetry::with_writer(
                Box::new(warnings.clone()),
                false,
                None,
            );
            let enabled = telemetry.in_scope(|| {
                load_request_lines_with(Path::new("queue"), Some(Path::new("wide.toml")), |path| {
                    if path == Path::new("queue/host.toml") {
                        Ok(text.clone())
                    } else {
                        Ok(enabled.into())
                    }
                })
            });
            assert!(!enabled, "malformed local table must not inherit host true");
            let warnings = warnings.lines();
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0]["level"], "WARN");
            assert!(
                warnings[0]["message"]
                    .as_str()
                    .unwrap()
                    .contains("startup request lines disabled")
            );
        }
        assert_eq!(
            parse_request_lines(&format!("{enabled}[service_logger]\nother = true")),
            Ok(Some(true))
        );
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
        let reads =
            StartupReads::open_with_mode(dir.path(), Stepped::at(0), Box::new(Unread), false);
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
        let none = StartupReads::open_with_mode(&file, Stepped::at(0), Box::new(Unread), false);
        let began = none.begin().unwrap();
        none.answered(began, &answered("show", 1));
    }
}
