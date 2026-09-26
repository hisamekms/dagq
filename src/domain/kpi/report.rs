//! The KPI report (ADR-0051 decisions 20 and 21): what `dagq kpi` derives
//! for one day or ISO week, with the build and time it was made and the
//! top open findings, as JSON and as one self-contained HTML page; where
//! each report's files go under `<queue dir>/reports/`, which periods the
//! supervisor still owes, and which files the retention removes. Pure: the
//! caller reads the queue and writes the files.
use std::collections::HashSet;

use serde::Serialize;

use super::{DAY_MS, Kpi, Period, date};
use crate::domain::{Finding, marks, stats::timestamp_millis};

mod html;

pub use html::{index_html, render_html};

/// A KPI's value in its unit as the report shows it; `—` for none.
pub fn format_value(kpi: &str, value: Option<f64>) -> String {
    html::value(kpi, value)
}

/// The open findings a report lists, larger impact first.
pub const FINDINGS_LISTED: usize = 10;
/// The days before today the supervisor writes the reports it missed.
pub const BACKFILL_DAYS: i64 = 7;
/// How long the reports are kept without `[report]` in `host.toml`.
pub const DEFAULT_KEEP_DAILY_DAYS: usize = 90;
pub const DEFAULT_KEEP_WEEKLY_WEEKS: usize = 104;
/// The event of a report the supervisor wrote: `period`, `label`, the
/// files, the build and the supervisor.
pub const REPORT_WRITTEN: &str = "report_written";
/// The file the reports are listed in, newest first.
pub const INDEX_FILE: &str = "index.html";
/// The marker of a report of a period not over yet in its file names.
const PARTIAL: &str = ".partial";

/// How many reports are kept (`[report]` of `host.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keep {
    pub daily_days: usize,
    pub weekly_weeks: usize,
}

impl Default for Keep {
    fn default() -> Self {
        Self {
            daily_days: DEFAULT_KEEP_DAILY_DAYS,
            weekly_weeks: DEFAULT_KEEP_WEEKLY_WEEKS,
        }
    }
}

/// What the report is of, and when and by what it was made.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportHeader {
    /// `day` or `week`.
    pub period: &'static str,
    /// `YYYY-MM-DD` or `YYYY-Www`.
    pub label: String,
    /// The period is not over: its values are so far.
    pub partial: bool,
    pub generated_at: String,
    /// The build identifier of the binary that made it.
    pub build: String,
}

/// One open finding as a report lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FindingLine {
    pub id: i64,
    pub kind: String,
    pub target: String,
    pub subject: String,
    pub summary: String,
    pub impact: &'static str,
    pub status: &'static str,
    pub occurrences: i64,
    pub last_seen_at: String,
}

/// The report: `dagq kpi --period <period> --at <the period>` with the
/// header and the top open findings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub report: ReportHeader,
    #[serde(flatten)]
    pub kpi: Kpi,
    /// The findings still `open` or `proposed`.
    pub findings_open: usize,
    /// The first [`FINDINGS_LISTED`] of them, larger impact first.
    pub findings: Vec<FindingLine>,
}

impl Report {
    /// The report of the latest period `kpi` lists; `findings` are the
    /// unsettled ones in the order `findings` lists them.
    pub fn new(
        kpi: Kpi,
        period: Period,
        generated_at_ms: i64,
        build: &str,
        findings: &[Finding],
    ) -> Self {
        let latest = kpi.periods.last();
        Self {
            report: ReportHeader {
                period: period.as_str(),
                label: latest.map(|p| p.label.clone()).unwrap_or_default(),
                partial: latest.is_some_and(|p| p.partial),
                generated_at: marks::utc_text(generated_at_ms),
                build: build.to_owned(),
            },
            kpi,
            findings_open: findings.len(),
            findings: findings
                .iter()
                .take(FINDINGS_LISTED)
                .map(|finding| FindingLine {
                    id: finding.id.as_i64(),
                    kind: finding.kind.clone(),
                    target: finding.target.clone(),
                    subject: finding.subject.clone(),
                    summary: finding.summary.clone(),
                    impact: finding.impact.as_str(),
                    status: finding.status.as_str(),
                    occurrences: finding.occurrences,
                    last_seen_at: marks::utc_text(finding.last_seen_at * 1000),
                })
                .collect(),
        }
    }

    /// The directory under the reports' root and the file stem.
    pub fn file_name(&self, period: Period) -> ReportFile {
        ReportFile {
            period,
            label: self.report.label.clone(),
            partial: self.report.partial,
        }
    }
}

/// A report's files: `<daily|weekly>/<label>[.partial].{json,html}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReportFile {
    pub period: Period,
    pub label: String,
    pub partial: bool,
}

impl ReportFile {
    pub const fn dir(&self) -> &'static str {
        dir(self.period)
    }

    pub fn stem(&self) -> String {
        if self.partial {
            format!("{}{PARTIAL}", self.label)
        } else {
            self.label.clone()
        }
    }

    /// `daily/2026-09-26.html` from the reports' root.
    pub fn path(&self, extension: &str) -> String {
        format!("{}/{}.{extension}", self.dir(), self.stem())
    }

    /// The report a file of `period`'s directory is, if it is one:
    /// `<label>[.partial].<json|html>` with a well-formed label.
    pub fn parse(period: Period, name: &str) -> Option<(Self, &str)> {
        let (stem, extension) = name.rsplit_once('.')?;
        if !matches!(extension, "json" | "html") {
            return None;
        }
        let (label, partial) = match stem.strip_suffix(PARTIAL) {
            Some(label) => (label, true),
            None => (stem, false),
        };
        label_days(period, label)?;
        Some((
            Self {
                period,
                label: label.to_owned(),
                partial,
            },
            extension,
        ))
    }
}

pub const fn dir(period: Period) -> &'static str {
    match period {
        Period::Day => "daily",
        Period::Week => "weekly",
    }
}

/// The local day (days since 1970-01-01) a label's period starts on: the
/// day itself, or the Monday of the ISO week. `None` for a malformed one.
pub fn label_days(period: Period, label: &str) -> Option<i64> {
    match period {
        Period::Day => {
            let days = timestamp_millis(&format!("{label}T00:00:00Z"))?.div_euclid(DAY_MS);
            (date(days) == label).then_some(days)
        }
        Period::Week => {
            let (year, week) = label.split_once("-W")?;
            if year.len() != 4 || week.len() != 2 {
                return None;
            }
            let week: i64 = week.parse().ok()?;
            // 4 January is always in week 1.
            let fourth = timestamp_millis(&format!("{year}-01-04T00:00:00Z"))?.div_euclid(DAY_MS);
            let monday = fourth - (fourth + 3).rem_euclid(7) + (week - 1) * 7;
            (week >= 1 && Period::Week.label(monday * DAY_MS, 0) == label).then_some(monday)
        }
    }
}

/// The reports the supervisor owes at `now` (unix ms, `offset_ms` east of
/// UTC), oldest first: each of the [`BACKFILL_DAYS`] days before today and
/// the ISO week before this one, unless `written` (period, label) has it.
/// Each comes with a time inside its period, for `kpi --at`.
pub fn due(
    now_ms: i64,
    offset_ms: i64,
    written: &HashSet<(String, String)>,
) -> Vec<(Period, String, i64)> {
    let today = Period::Day.start(now_ms, offset_ms);
    let this_week = Period::Week.start(now_ms, offset_ms);
    (1..=BACKFILL_DAYS)
        .rev()
        .map(|back| (Period::Day, today - back * DAY_MS))
        .chain([(Period::Week, this_week - 7 * DAY_MS)])
        .map(|(period, start)| (period, period.label(start, offset_ms), start))
        .filter(|(period, label, _)| {
            !written.contains(&(period.as_str().to_owned(), label.clone()))
        })
        .collect()
}

/// Whether the retention removes `file` at `now`: a day more than
/// `keep.daily_days` days before today (the complete days kept are
/// the last `keep.daily_days`), a week more than `keep.weekly_weeks`
/// weeks before this one, and the partial report of a
/// period whose complete one `complete` names.
pub fn expired(
    file: &ReportFile,
    now_ms: i64,
    offset_ms: i64,
    keep: Keep,
    complete: &HashSet<(Period, String)>,
) -> bool {
    if file.partial && complete.contains(&(file.period, file.label.clone())) {
        return true;
    }
    let Some(days) = label_days(file.period, &file.label) else {
        return false;
    };
    let today = (now_ms + offset_ms).div_euclid(DAY_MS);
    let age = |keep: usize| i64::try_from(keep).unwrap_or(i64::MAX);
    match file.period {
        Period::Day => today - days > age(keep.daily_days),
        Period::Week => {
            let this_monday = today - (today + 3).rem_euclid(7);
            (this_monday - days) / 7 > age(keep.weekly_weeks)
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
