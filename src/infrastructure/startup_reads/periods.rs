//! Pure aggregation: elapsed monotonic time and samples enter, JSON leaves.
use super::{Answered, MemoryReading};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

/// Minute bins expose a 25-second pause as a call-count trough and a
/// duration maximum. Ten-minute bins after catch-up bound steady log growth.
pub const STARTUP_PERIOD: Duration = Duration::from_secs(60);
pub const STEADY_PERIOD: Duration = Duration::from_secs(600);
/// 154 distinct keys per minute were observed without caller labels;
/// 512 leaves room for caller splits, including one reserved overflow key.
pub const MAX_KEYS: usize = 512;

/// Untrusted op and caller labels: at most 64 ASCII alphanumerics or
/// `_`, `-`, `.`, `:`; every other string (including empty) becomes `other`.
pub fn label(value: Option<&str>) -> Option<String> {
    value.map(|value| {
        if !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        {
            value.to_owned()
        } else {
            "other".into()
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key(
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);
impl Key {
    fn of(answer: &Answered) -> Self {
        Self(
            answer.use_case.clone(),
            label(answer.op.as_deref()),
            answer.role.clone(),
            label(answer.caller.as_deref()),
        )
    }
    fn other() -> Self {
        Self(
            Some("other".into()),
            Some("other".into()),
            Some("other".into()),
            Some("other".into()),
        )
    }
}
#[derive(Default)]
pub(super) struct Totals {
    calls: u64,
    bytes: u64,
    max_bytes: u64,
    rows: u64,
    duration_ms: u64,
    max_duration_ms: u64,
    heavy_read_wait_ms: Option<u64>,
    heavy_read_refused: u64,
}
impl Totals {
    fn add(&mut self, answer: &Answered, duration: u64) {
        self.calls += 1;
        self.bytes = self.bytes.saturating_add(answer.bytes as u64);
        self.max_bytes = self.max_bytes.max(answer.bytes as u64);
        self.rows = self.rows.saturating_add(answer.rows.unwrap_or(0) as u64);
        self.duration_ms = self.duration_ms.saturating_add(duration);
        self.max_duration_ms = self.max_duration_ms.max(duration);
        if let Some(note) = answer.heavy_read {
            self.heavy_read_wait_ms =
                Some(self.heavy_read_wait_ms.unwrap_or(0).max(note.waited_ms));
            self.heavy_read_refused += u64::from(note.refused);
        }
    }
    fn line(&self, key: &Key) -> Value {
        json!({"use_case": key.0, "op": key.1, "role": key.2, "caller": key.3,
            "calls": self.calls, "bytes": self.bytes, "max_bytes": self.max_bytes,
            "rows": self.rows, "duration_ms": self.duration_ms, "max_duration_ms": self.max_duration_ms,
            "heavy_read_wait_ms": self.heavy_read_wait_ms, "heavy_read_refused": self.heavy_read_refused})
    }
}

/// Totals by key, at most `limit` keys: the last one is reserved for
/// `other`, which takes every key first seen after the others filled it.
pub(super) struct Groups {
    totals: BTreeMap<Key, Totals>,
    limit: usize,
}
impl Default for Groups {
    fn default() -> Self {
        Self {
            totals: BTreeMap::new(),
            limit: MAX_KEYS,
        }
    }
}
impl Groups {
    pub fn add(&mut self, answer: &Answered, duration: u64) {
        let mut key = Key::of(answer);
        if !self.totals.contains_key(&key) && self.totals.len() >= self.limit - 1 {
            key = Key::other();
        }
        self.totals.entry(key).or_default().add(answer, duration);
    }
}

/// Totals over the whole startup window, for the summary's ranking. A
/// period's key limit must not merge callers first seen in later periods,
/// which would change their startup-wide ranking when no period overflows.
/// Its own limit admits every key that the window's minute periods and the
/// first steady period, where its late answers land, can each name: a key
/// goes to `other` here only once some period's `by_caller` rows merged
/// keys too, or a startup answer outlasted that steady period as well,
/// while the memory it holds stays bounded.
pub(super) struct StartupCalls(Groups);
impl StartupCalls {
    pub fn new(window: Duration) -> Self {
        let periods = window.as_millis().div_ceil(STARTUP_PERIOD.as_millis()) + 1;
        let periods = usize::try_from(periods).unwrap_or(usize::MAX);
        Self(Groups {
            totals: BTreeMap::new(),
            limit: periods.saturating_mul(MAX_KEYS - 1).saturating_add(1),
        })
    }
    pub fn add(&mut self, answer: &Answered, duration: u64) {
        self.0.add(answer, duration);
    }
    /// Let go of the totals once the summary has used them.
    pub fn clear(&mut self) {
        self.0.totals.clear();
    }
    pub fn top(&self) -> Vec<Value> {
        let mut rows: Vec<_> = self.0.totals.iter().collect();
        rows.sort_by_key(|(_, totals)| std::cmp::Reverse(totals.calls));
        rows.into_iter()
            .take(super::TOP_REQUESTS)
            .map(|(k, t)| t.line(k))
            .collect()
    }
}

pub(super) struct Periods {
    window: Duration,
    start: Duration,
    end: Duration,
    groups: Groups,
    requests: u64,
    resident: Option<u64>,
    peak: Option<u64>,
}
impl Periods {
    pub fn new(window: Duration) -> Self {
        let mut this = Self {
            window,
            start: Duration::ZERO,
            end: Duration::ZERO,
            groups: Groups::default(),
            requests: 0,
            resident: None,
            peak: None,
        };
        this.align(Duration::ZERO);
        this
    }
    fn align(&mut self, now: Duration) {
        let (origin, period) = if now < self.window {
            (Duration::ZERO, STARTUP_PERIOD)
        } else {
            (self.window, STEADY_PERIOD)
        };
        self.start = origin
            + Duration::from_secs((now - origin).as_secs() / period.as_secs() * period.as_secs());
        self.end = self.start + period;
        if now < self.window {
            self.end = self.end.min(self.window);
        }
    }
    pub fn advance(&mut self, now: Duration, at: &str) -> Vec<Value> {
        if now < self.end {
            return Vec::new();
        }
        let lines = self.flush(self.end, at);
        self.align(now);
        lines
    }
    pub fn add(&mut self, answer: &Answered, duration: u64, readings: [MemoryReading; 2]) {
        self.groups.add(answer, duration);
        self.requests += 1;
        for reading in readings {
            self.resident = self.resident.max(reading.resident);
            self.peak = self.peak.max(reading.peak);
        }
    }
    pub fn flush(&mut self, end: Duration, at: &str) -> Vec<Value> {
        let mut lines = Vec::new();
        if self.requests > 0 {
            let secs = end.saturating_sub(self.start).as_secs_f64();
            for (key, totals) in &self.groups.totals {
                let mut line = totals.line(key);
                line["record"] = json!("by_caller");
                line["at"] = json!(at);
                line["period_secs"] = json!(secs);
                lines.push(line);
            }
            lines.push(json!({"record": "period", "at": at, "period_secs": secs,
                "requests": self.requests, "max_resident": self.resident, "peak": self.peak}));
        }
        self.groups = Groups::default();
        self.requests = 0;
        self.resident = None;
        self.peak = None;
        lines
    }
}
