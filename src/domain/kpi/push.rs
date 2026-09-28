//! The push of the KPIs (ADR-0051 decisions 18, 22 and 23): the breaches
//! the supervisor records as they start and end, the messages a host's
//! push command reads on its stdin (the daily and weekly summaries and a
//! breach as it starts), how often a failed push is tried again, and what
//! of a failure is recorded. Pure: the caller reads the queue, runs the
//! command and records the events.
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};

use super::{ALL, Measure, PeriodKpis, TargetReport, report::Report};

/// A target of a KPI and a stratum went into breach (decision 18): its
/// `period` (`day` / `week`), `kpi`, `stratum`, `stat`, `min`, `max`,
/// `source`, `since`, `streak`, the latest judged `label` and `value`, and
/// whether it was pushed at once (`pushed`, on the host's local `day`).
pub const KPI_BREACH_STARTED: &str =
    crate::domain::event_kind::EventKind::KpiBreachStarted.as_str();
/// A breach ended: its `period`, `kpi` and `stratum`, the `label` of the
/// period that met the target and the `reason` (`met`, or
/// `target_removed` when the target is no longer set).
pub const KPI_BREACH_RESOLVED: &str =
    crate::domain::event_kind::EventKind::KpiBreachResolved.as_str();
/// The breaches' start and end, the latest of a target telling its state.
pub const KPI_BREACH_KINDS: [&str; 2] = [KPI_BREACH_STARTED, KPI_BREACH_RESOLVED];
/// The push command took a message (`push_kind`, `period`, `attempt`).
pub const KPI_PUSH_SENT: &str = crate::domain::event_kind::EventKind::KpiPushSent.as_str();
/// The push command failed on a message: `push_kind`, `period`,
/// `attempt`, `exit_code`, `signal`, `timed_out`, `error`, the tail of its
/// stderr and whether the message was given up (`gave_up`).
pub const KPI_PUSH_FAILED: &str = crate::domain::event_kind::EventKind::KpiPushFailed.as_str();
/// A message was given up after its last attempt, and no push succeeded
/// since the last one given up: the inbox's attention (`fix the push
/// command`), which ends with the next [`KPI_PUSH_SENT`].
pub const KPI_PUSH_ABANDONED: &str =
    crate::domain::event_kind::EventKind::KpiPushAbandoned.as_str();
/// The events whose latest tells whether the push's attention is open.
pub const KPI_PUSH_ATTENTION_KINDS: [&str; 2] = [KPI_PUSH_SENT, KPI_PUSH_ABANDONED];

/// How long the command may take without `timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// The breaches pushed at once on one local day without
/// `max_breach_per_day`.
pub const DEFAULT_MAX_BREACH_PER_DAY: usize = 3;
/// How long after a failed attempt the same message is tried again: twice,
/// after 1 and 5 minutes (decision 23).
pub const RETRY_DELAYS_SECS: [u64; 2] = [60, 300];
/// The bytes of the command's stderr a failure records, from its end.
pub const STDERR_TAIL_BYTES: usize = 2000;
/// The KPIs a summary's text names, when the period has them.
const SUMMARY_KPIS: &[&str] = &[
    "lead_time",
    "phase.work",
    "first_pass_rate",
    "revise_rate",
    "asks_per_landing",
];

/// `[push]` of the host's `host.toml` (decision 22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfig {
    /// The program and its arguments, run without a shell.
    pub command: Vec<String>,
    pub timeout_secs: u64,
    /// Push the daily (and weekly) summary.
    pub daily: bool,
    /// Push a breach as it starts.
    pub breach: bool,
    pub max_breach_per_day: usize,
}

impl PushConfig {
    /// The settings of `command` with the defaults.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            daily: true,
            breach: true,
            max_breach_per_day: DEFAULT_MAX_BREACH_PER_DAY,
        }
    }
}

/// What a message is: `DAGQ_PUSH_KIND` and its `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    Daily,
    Weekly,
    Breach,
}

impl PushKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Breach => "breach",
        }
    }
}

/// One message for the push command: what goes on its stdin and in its
/// environment.
#[derive(Debug, Clone, PartialEq)]
pub struct PushMessage {
    pub kind: PushKind,
    /// The period it is about (`YYYY-MM-DD`, `YYYY-Www`).
    pub period: String,
    /// The JSON object on the command's stdin.
    pub body: Value,
    pub report_html: Option<String>,
    pub report_json: Option<String>,
}

impl PushMessage {
    /// The bytes on the command's stdin: the object and a newline.
    pub fn stdin(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&self.body).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }
}

/// A breach's key: its period, KPI and stratum.
pub fn breach_key(payload: &Value) -> (String, String, String) {
    let field = |name: &str| {
        payload
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    (field("period"), field("kpi"), field("stratum"))
}

/// The latest period `target` could judge, and its value.
fn latest_judged(target: &TargetReport) -> Option<(&str, Option<f64>)> {
    target
        .periods
        .iter()
        .rev()
        .find(|period| period.judged)
        .map(|period| (period.period.as_str(), period.value))
}

/// The payload of `target`'s breach started on the `period` (`day` /
/// `week`) KPIs; `None` when it is not in breach.
pub fn breach_started(period: &str, target: &TargetReport) -> Option<Value> {
    if target.state != "breach" {
        return None;
    }
    let latest = latest_judged(target);
    Some(json!({
        "period": period,
        "kpi": target.kpi,
        "stratum": target.stratum,
        "stat": target.stat,
        "min": target.min,
        "max": target.max,
        "source": target.source,
        "since": target.breach_since,
        "streak": target.streak,
        "label": latest.map(|(label, _)| label),
        "value": latest.and_then(|(_, value)| value),
    }))
}

/// The payload of the end of the breach `open` (a [`KPI_BREACH_STARTED`]
/// payload) of `period` by `targets` judged now; `None` while it goes on
/// or could not be judged (`not_judged` keeps a breach open).
pub fn breach_resolved(open: &Value, targets: &[TargetReport]) -> Option<Value> {
    let (period, kpi, stratum) = breach_key(open);
    let target = targets
        .iter()
        .find(|target| target.kpi == kpi && target.stratum == stratum);
    let (reason, label) = match target {
        None => ("target_removed", None),
        Some(target) if matches!(target.state, "ok" | "missed") => {
            ("met", latest_judged(target).map(|(label, _)| label))
        }
        Some(_) => return None,
    };
    Some(json!({
        "period": period,
        "kpi": kpi,
        "stratum": stratum,
        "label": label,
        "reason": reason,
    }))
}

/// A KPI's value in its unit, as the report shows it.
fn shown(kpi: &str, value: Option<f64>) -> String {
    super::report::format_value(kpi, value)
}

/// `≥ min`, `≤ max` or both, of a target or a breach.
fn bounds(kpi: &str, min: Option<f64>, max: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some(min) = min {
        parts.push(format!("≥ {}", shown(kpi, Some(min))));
    }
    if let Some(max) = max {
        parts.push(format!("≤ {}", shown(kpi, Some(max))));
    }
    parts.join(" and ")
}

/// A breach as `breaches` of a message lists it.
fn breach_line(breach: &Value) -> Value {
    let kpi = breach
        .get("kpi")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "kpi": kpi,
        "stratum": breach.get("stratum"),
        "stat": breach.get("stat"),
        "value": breach.get("value"),
        "min": breach.get("min"),
        "max": breach.get("max"),
        "periods": breach.get("streak"),
        "since": breach.get("since"),
    })
}

/// One line of text of a breach.
fn breach_text(breach: &Value) -> String {
    let text = |name: &str| breach.get(name).and_then(Value::as_str).unwrap_or("—");
    let kpi = text("kpi");
    format!(
        "{kpi} ({}): {} ({}, target {}), missed {} period(s) in a row (since {})",
        text("stratum"),
        shown(kpi, breach.get("value").and_then(Value::as_f64)),
        text("stat"),
        bounds(
            kpi,
            breach.get("min").and_then(Value::as_f64),
            breach.get("max").and_then(Value::as_f64)
        ),
        breach.get("streak").and_then(Value::as_u64).unwrap_or(0),
        text("since"),
    )
}

/// The message of a breach as it starts (`started`, a
/// [`KPI_BREACH_STARTED`] payload) of the queue `name` at `queue`.
pub fn breach_message(name: &str, queue: &str, started: &Value) -> PushMessage {
    let text = |field: &str| started.get(field).and_then(Value::as_str).unwrap_or("—");
    let period = text("label").to_owned();
    let title = format!(
        "{name} target breach: {} ({})",
        text("kpi"),
        text("stratum")
    );
    PushMessage {
        kind: PushKind::Breach,
        period: period.clone(),
        body: json!({
            "kind": PushKind::Breach,
            "queue": queue,
            "period": period,
            "title": title,
            "text": breach_text(started),
            "breaches": [breach_line(started)],
            "resolved": [],
            "report_html": Value::Null,
            "report_json": Value::Null,
        }),
        report_html: None,
        report_json: None,
    }
}

/// The value of `kpi` over every run in `period`.
fn overall<'a>(period: &'a PeriodKpis, kpi: &str) -> Option<&'a Measure> {
    period.window.kpis.get(kpi)?.get(ALL)
}

/// The summary of `report` (a day's or a week's) of the queue `name` at
/// `queue`: its KPIs, the breaches and misses of its targets, the breaches
/// that ended (`resolved`, [`KPI_BREACH_RESOLVED`] payloads), the open
/// asks and the report's files.
pub fn summary_message(
    name: &str,
    queue: &str,
    report: &Report,
    resolved: &[Value],
    open_asks: usize,
    files: (&Path, &Path),
) -> PushMessage {
    let kind = if report.report.period == "week" {
        PushKind::Weekly
    } else {
        PushKind::Daily
    };
    let label = report.report.label.clone();
    let latest = report.kpi.periods.last();
    let landings = latest
        .and_then(|period| overall(period, "landings"))
        .and_then(|measure| measure.value);
    let breaches: Vec<Value> = report
        .kpi
        .targets
        .iter()
        .filter_map(|target| {
            let mut started = breach_started(report.report.period, target)?;
            started["label"] = json!(label);
            Some(started)
        })
        .collect();
    let missed: Vec<&TargetReport> = report
        .kpi
        .targets
        .iter()
        .filter(|target| target.state == "missed")
        .collect();
    let title = format!(
        "{name} {label}: landings {}, breaches {}",
        shown("landings", landings),
        breaches.len()
    );
    let mut lines = Vec::new();
    if let Some(period) = latest {
        let values: Vec<String> = SUMMARY_KPIS
            .iter()
            .filter_map(|kpi| {
                let measure = overall(period, kpi)?;
                let value = measure.primary()?;
                Some(format!("{kpi} {}", shown(kpi, Some(value))))
            })
            .collect();
        lines.push(format!("landings {}", shown("landings", landings)));
        if !values.is_empty() {
            lines.push(values.join(", "));
        }
    }
    if !breaches.is_empty() {
        lines.push("Breaches:".to_owned());
        lines.extend(breaches.iter().map(|b| format!("- {}", breach_text(b))));
    }
    if !missed.is_empty() {
        lines.push("Missed (1 period):".to_owned());
        lines.extend(missed.iter().map(|target| {
            let value = latest_judged(target).and_then(|(_, value)| value);
            format!(
                "- {} ({}): {} (target {})",
                target.kpi,
                target.stratum,
                shown(&target.kpi, value),
                bounds(&target.kpi, target.min, target.max)
            )
        }));
    }
    if !resolved.is_empty() {
        lines.push("Resolved:".to_owned());
        lines.extend(resolved.iter().map(|end| {
            let text = |field: &str| end.get(field).and_then(Value::as_str).unwrap_or("—");
            format!("- {} ({})", text("kpi"), text("stratum"))
        }));
    }
    lines.push(format!("open asks: {open_asks}"));
    let (html, json_path) = (files.0.display().to_string(), files.1.display().to_string());
    lines.push(format!("report: {html}"));
    PushMessage {
        kind,
        period: label.clone(),
        body: json!({
            "kind": kind,
            "queue": queue,
            "period": label,
            "title": title,
            "text": lines.join("\n"),
            "breaches": breaches.iter().map(breach_line).collect::<Vec<_>>(),
            "missed": missed.iter().map(|target| json!({
                "kpi": target.kpi,
                "stratum": target.stratum,
                "value": latest_judged(target).and_then(|(_, value)| value),
                "min": target.min,
                "max": target.max,
            })).collect::<Vec<_>>(),
            "resolved": resolved.iter().map(|end| json!({
                "kpi": end.get("kpi"),
                "stratum": end.get("stratum"),
                "reason": end.get("reason"),
            })).collect::<Vec<_>>(),
            "open_asks": open_asks,
            "report_html": html,
            "report_json": json_path,
        }),
        report_html: Some(html),
        report_json: Some(json_path),
    }
}

/// The delay before the attempt after `attempt` (1-based), or `None` when
/// `attempt` was the last.
pub fn retry_after(attempt: usize) -> Option<u64> {
    attempt
        .checked_sub(1)
        .and_then(|index| RETRY_DELAYS_SECS.get(index).copied())
}

/// The end of `stderr` a failure records: at most [`STDERR_TAIL_BYTES`],
/// with each argument of `command` (but the program) that is long enough
/// to be a secret replaced, so a URL or a token given as an argument does
/// not reach the events.
pub fn stderr_tail(stderr: &str, command: &[String]) -> String {
    let mut text = stderr.to_owned();
    for argument in command.iter().skip(1) {
        if argument.len() >= 8 {
            text = text.replace(argument.as_str(), "[argument]");
        }
    }
    let text = text.trim_end();
    if text.len() <= STDERR_TAIL_BYTES {
        return text.to_owned();
    }
    let mut start = text.len() - STDERR_TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests;
