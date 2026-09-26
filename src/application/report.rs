//! `report` (ADR-0051 decisions 20 and 21): the KPI report of a day or an
//! ISO week, made from [`super::kpi::kpi`] and the open findings, written
//! as JSON and self-contained HTML under the reports' root
//! (`<queue dir>/reports/` unless `report --out`), each file through a
//! temporary file in the same directory and a rename. Each write lists the
//! reports again in `index.html` and removes what the retention no longer
//! keeps. The supervisor writes the reports it owes once a day
//! ([`write_due`]); a person writes any with `dagq report`. No LLM, no run
//! slot, nothing sent outside the queue's directory.
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;

use super::{Queue, RunFiles, kpi::Host};
use crate::domain::{
    FindingQuery,
    kpi::{
        DAY_MS, KpiConfig, KpiQuery, Period,
        report::{self, INDEX_FILE, Keep, Report, ReportFile},
    },
    marks,
    stats::Cursor,
};

/// What a report is made with: where it goes, the host's time zone and
/// cores, the `[kpi]` settings, the retention and the build that makes it.
#[derive(Debug, Clone)]
pub struct ReportSetup {
    /// The reports' root: `daily/`, `weekly/` and `index.html` go under it.
    pub root: PathBuf,
    pub host: Host,
    pub config: KpiConfig,
    pub keep: Keep,
    pub build: String,
}

/// One report written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Written {
    pub period: &'static str,
    pub label: String,
    pub partial: bool,
    pub html: PathBuf,
    pub json: PathBuf,
    pub index: PathBuf,
    /// The reports the retention removed.
    pub removed: Vec<PathBuf>,
}

/// The report of the `period` that holds `at` (now without), at the unix
/// second `now`.
pub fn make(
    queue: &dyn Queue,
    setup: &ReportSetup,
    now: i64,
    period: Period,
    at: Option<Cursor>,
) -> Result<Report> {
    let kpi = super::kpi::kpi(
        queue,
        now,
        setup.host,
        &setup.config,
        &KpiQuery {
            period,
            at,
            ..KpiQuery::default()
        },
    )?;
    let findings: Vec<_> = queue
        .findings(&FindingQuery::default())?
        .into_iter()
        .map(|view| view.finding)
        .collect();
    Ok(Report::new(
        kpi,
        period,
        now * 1000,
        &setup.build,
        &findings,
    ))
}

/// Write `report` of `period` under the root: its JSON and HTML, then the
/// retention and the index.
pub fn write(
    files: &dyn RunFiles,
    setup: &ReportSetup,
    report: &Report,
    period: Period,
    now: i64,
) -> Result<Written> {
    let file = report.file_name(period);
    let dir = setup.root.join(file.dir());
    files
        .create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let json_path = setup.root.join(file.path("json"));
    let mut json = serde_json::to_vec_pretty(report)?;
    json.push(b'\n');
    write_atomic(files, &json_path, &json)?;
    let html_path = setup.root.join(file.path("html"));
    write_atomic(files, &html_path, report::render_html(report).as_bytes())?;
    let offset_ms = setup.host.utc_offset_secs * 1000;
    let removed = prune(files, &setup.root, now * 1000, offset_ms, setup.keep)?;
    let index = write_index(files, &setup.root, now)?;
    Ok(Written {
        period: period.as_str(),
        label: file.label,
        partial: file.partial,
        html: html_path,
        json: json_path,
        index,
        removed,
    })
}

/// Write the reports the supervisor owes at `now` (the days and the week
/// [`report::due`] names that no `report_written` records), oldest first,
/// recording each as `report_written` by `supervisor`. A report another
/// supervisor recorded meanwhile is not recorded twice.
pub fn write_due(
    queue: &dyn Queue,
    files: &dyn RunFiles,
    setup: &ReportSetup,
    now: i64,
    supervisor: &str,
) -> Result<Vec<Written>> {
    let written = queue.reports_written()?;
    let offset_ms = setup.host.utc_offset_secs * 1000;
    let mut done = Vec::new();
    for (period, label, at) in report::due(now * 1000, offset_ms, &written) {
        // A day the retention would remove at once (`keep_daily_days` under
        // the backfill) is not written.
        let file = ReportFile {
            period,
            label: label.clone(),
            partial: false,
        };
        if report::expired(&file, now * 1000, offset_ms, setup.keep, &HashSet::new()) {
            continue;
        }
        let report = make(queue, setup, now, period, Some(Cursor::Time(at)))
            .with_context(|| format!("the report of {label}"))?;
        let output = write(files, setup, &report, period, now)
            .with_context(|| format!("write the report of {label}"))?;
        let recorded = queue.record_report_written(json!({
            "period": period.as_str(),
            "label": label,
            "html": output.html,
            "json": output.json,
            "removed": output.removed.len(),
            "build": setup.build,
            "supervisor": supervisor,
        }))?;
        if recorded {
            done.push(output);
        }
    }
    Ok(done)
}

/// The host's local day of the unix second `now` (days since the epoch):
/// the supervisor looks for the reports it owes once each time it changes.
pub fn local_day(now: i64, utc_offset_secs: i64) -> i64 {
    (now * 1000 + utc_offset_secs * 1000).div_euclid(DAY_MS)
}

/// `path`'s bytes through a temporary file in its directory and a rename.
fn write_atomic(files: &dyn RunFiles, path: &Path, contents: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("a report path has a file name")?;
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    files
        .write(&temporary, contents)
        .with_context(|| format!("write {}", temporary.display()))?;
    files
        .rename(&temporary, path)
        .with_context(|| format!("rename {} to {}", temporary.display(), path.display()))
}

/// The reports in `root`'s `daily/` and `weekly/`, each once (its JSON or
/// its HTML), with the paths of its files.
fn listed(files: &dyn RunFiles, root: &Path) -> Result<Vec<(ReportFile, Vec<PathBuf>)>> {
    let mut reports: Vec<(ReportFile, Vec<PathBuf>)> = Vec::new();
    for period in [Period::Day, Period::Week] {
        let dir = root.join(report::dir(period));
        let entries = match files.read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("list {}", dir.display())),
        };
        for path in entries {
            let Some((file, _)) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| ReportFile::parse(period, name))
            else {
                continue;
            };
            match reports.iter_mut().find(|(kept, _)| *kept == file) {
                Some((_, paths)) => paths.push(path),
                None => reports.push((file, vec![path])),
            }
        }
    }
    reports.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(reports)
}

/// Remove the reports the retention does not keep; returns their files.
fn prune(
    files: &dyn RunFiles,
    root: &Path,
    now_ms: i64,
    offset_ms: i64,
    keep: Keep,
) -> Result<Vec<PathBuf>> {
    let reports = listed(files, root)?;
    let complete: HashSet<(Period, String)> = reports
        .iter()
        .filter(|(file, _)| !file.partial)
        .map(|(file, _)| (file.period, file.label.clone()))
        .collect();
    let mut removed = Vec::new();
    for (file, paths) in reports {
        if !report::expired(&file, now_ms, offset_ms, keep, &complete) {
            continue;
        }
        for path in paths {
            match files.remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove {}", path.display()));
                }
            }
        }
    }
    // A temporary file an ended writer left (an exec mid-write), once it is
    // older than any write in progress.
    for period in [Period::Day, Period::Week] {
        let Ok(entries) = files.read_dir(&root.join(report::dir(period))) else {
            continue;
        };
        for path in entries {
            let temporary = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"));
            let stale = files.modified(&path).is_ok_and(|modified| {
                files
                    .now()
                    .duration_since(modified)
                    .is_ok_and(|age| age >= STALE_TEMPORARY)
            });
            if temporary && stale && files.remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    Ok(removed)
}

/// How old a temporary report file is before the retention removes it.
const STALE_TEMPORARY: std::time::Duration = std::time::Duration::from_secs(3600);

/// Write `index.html` from the reports under `root`.
fn write_index(files: &dyn RunFiles, root: &Path, now: i64) -> Result<PathBuf> {
    let reports = listed(files, root)?;
    let of = |period: Period| -> Vec<ReportFile> {
        reports
            .iter()
            .filter(|(file, _)| file.period == period)
            .map(|(file, _)| file.clone())
            .collect()
    };
    let html = report::index_html(
        &of(Period::Day),
        &of(Period::Week),
        &marks::utc_text(now * 1000),
    );
    let path = root.join(INDEX_FILE);
    write_atomic(files, &path, html.as_bytes())?;
    Ok(path)
}
