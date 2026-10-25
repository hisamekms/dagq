//! `report` (ADR-0051 decisions 20 and 21): the KPI report of a day or an
//! ISO week, made from [`super::kpi::kpi`] and the open findings, written
//! as JSON and self-contained HTML under the reports' root
//! (`<queue dir>/reports/` unless `report --out`), each file through a
//! temporary file in the same directory and a rename. Each write lists the
//! reports again in `index.html` and removes what the retention no longer
//! keeps. The page carries the near-term dependency diagram (ADR-0077
//! decision 7), drawn by [`ReportSetup::diagram`] (the host's d2 and TALA)
//! and shown as an image of its own bytes, or why it is not drawn; the rest is written either way. The supervisor writes the reports it owes once a day
//! ([`write_due`]); a person writes any with `dagq report`. No LLM, no run
//! slot, nothing sent outside the queue's directory.
use crate::domain::LeaseToken;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    PlanningRecords, QueueRecords, RunFiles, RunLog, SupervisorReads, TaskReads, kpi::Host,
};
use crate::domain::{
    FindingQuery,
    host_metrics::HostSummary,
    kpi::{
        DAY_MS, HostReader, KpiConfig, KpiQuery, Period,
        report::{self, DiagramSection, INDEX_FILE, Keep, Report, ReportFile},
    },
    marks,
    stats::Cursor,
};

/// Draws a d2 source as SVG, or says why it cannot (ADR-0077 decisions 4
/// and 5).
pub type RenderSvg = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// What draws the report's dependency diagram.
#[derive(Clone)]
pub struct DiagramRenderer(pub RenderSvg);

impl std::fmt::Debug for DiagramRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiagramRenderer")
    }
}

/// What a report is made with: where it goes, the host's time zone and
/// cores, the `[kpi]` settings, the retention, the build that makes it and
/// what draws its dependency diagram.
#[derive(Debug, Clone)]
pub struct ReportSetup {
    /// The reports' root: `daily/`, `weekly/` and `index.html` go under it.
    pub root: PathBuf,
    pub host: Host,
    pub config: KpiConfig,
    pub keep: Keep,
    pub build: String,
    pub diagram: DiagramRenderer,
    /// The `[areas]` map and the landed commits' changes (ADR-t980-1).
    pub areas: super::areas::AreaReader,
    /// What summarizes the host's load next to each period (task 872);
    /// `None` reads none.
    pub host_metrics: Option<HostMetrics>,
}

impl ReportSetup {
    /// [`Self::host_metrics`] as the reader [`super::kpi::kpi`] takes.
    pub fn host_reader(&self) -> Option<HostReader<'_>> {
        self.host_metrics
            .as_ref()
            .map(|read| HostReader(read.0.as_ref()))
    }
}

/// Summarizes the host's load between two unix seconds (task 872).
#[derive(Clone)]
pub struct HostMetrics(pub Arc<dyn Fn(i64, i64) -> HostSummary + Send + Sync>);

impl std::fmt::Debug for HostMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostMetrics")
    }
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
/// second `now`, with the dependency diagram of the queue now.
pub fn make(
    queue: &(impl PlanningRecords + QueueRecords + SupervisorReads + RunLog + TaskReads + ?Sized),
    setup: &ReportSetup,
    now: i64,
    period: Period,
    at: Option<Cursor>,
) -> Result<Report> {
    let diagram = diagram(queue, &setup.diagram);
    make_with(queue, setup, now, period, at, diagram)
}

/// The near-term dependency diagram of the queue now, drawn by `render`:
/// its tasks and the SVG the page shows as an image, or why there is none.
/// A queue with no near-term task is not drawn.
pub fn diagram(queue: &(impl TaskReads + ?Sized), render: &DiagramRenderer) -> DiagramSection {
    diagram_draft(queue).draw(render)
}

/// The near-term dependency diagram before it is drawn: its tasks and d2
/// source, or why there is none. The queue service answers it and its
/// caller draws it ([`Self::draw`]), as no program starts in the service
/// (ADR-t3102-3 decision 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "draft", rename_all = "snake_case")]
pub enum DiagramDraft {
    Source { tasks: Vec<i64>, source: String },
    NotDrawn { tasks: Vec<i64>, reason: String },
}

impl DiagramDraft {
    /// The section the report's page carries: the source drawn by
    /// `render`, or why there is none.
    pub fn draw(self, render: &DiagramRenderer) -> DiagramSection {
        self.render(render).into()
    }

    /// The source drawn by `render` as d2 wrote it, or why there is none:
    /// what the queue service's client sends with `report`, which the
    /// service checks itself ([`DiagramSection::drawn`]).
    pub fn render(self, render: &DiagramRenderer) -> DrawnDiagram {
        match self {
            Self::Source { tasks, source } => match (render.0)(&source) {
                Ok(svg) => DrawnDiagram {
                    tasks,
                    d2_source: true,
                    svg: Some(svg),
                    reason: None,
                },
                Err(error) => DrawnDiagram {
                    tasks,
                    d2_source: true,
                    svg: None,
                    reason: Some(format!("{error:#}")),
                },
            },
            Self::NotDrawn { tasks, reason } => DrawnDiagram {
                tasks,
                d2_source: false,
                svg: None,
                reason: Some(reason),
            },
        }
    }
}

/// The near-term diagram as d2 drew it, or why it is not drawn: the tasks
/// it selects, whether its d2 source was made, and the SVG as d2 wrote it.
/// The SVG goes in the page only as [`DiagramSection::drawn`] keeps it,
/// whoever drew it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrawnDiagram {
    pub tasks: Vec<i64>,
    pub d2_source: bool,
    #[serde(default)]
    pub svg: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl From<DrawnDiagram> for DiagramSection {
    fn from(drawn: DrawnDiagram) -> Self {
        match drawn.svg {
            Some(svg) => Self::drawn(drawn.tasks, &svg),
            None => Self::not_drawn(
                drawn.tasks,
                drawn.d2_source,
                drawn
                    .reason
                    .unwrap_or_else(|| "no diagram was drawn".to_owned()),
            ),
        }
    }
}

/// What the queue service's client reads before it sends `report`, as no
/// Git or program runs in the service (ADR-t3102-3 decision 5): the
/// near-term diagram's draft, the Git common directory of the repository
/// the queue is bound to (none for a queue bound to none) and the commits
/// landed in the periods the report reads
/// ([`crate::domain::kpi::landed_in_periods`]; none until the client names
/// its checkout), whose areas the client reads with Git in its main
/// checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportInputs {
    pub diagram: DiagramDraft,
    pub repository: Option<String>,
    pub landed: Vec<String>,
}

/// What the queue service reads `report_inputs`' landed commits with: the
/// ports [`report_inputs`] reads through.
pub struct ReportInputsSources<'a> {
    pub files: &'a dyn RunFiles,
    /// The host's UTC offset (seconds) and the `[kpi]` settings of a
    /// checkout's `dagq.toml` merged with the host's.
    pub settings: &'a dyn Fn(&Path) -> Result<(i64, KpiConfig)>,
    /// The queue's events.
    pub events: &'a dyn Fn() -> Result<Vec<crate::domain::RunEvent>>,
}

/// `report_inputs` (ADR-t3102-3 decision 5): the near-term diagram's draft
/// `diagram` reads (after the rest), the bound repository `repository`
/// and, when `params` names a checkout (accepted as [`accept_checkout`]
/// accepts it against `served`, the main checkout the service found from
/// the binding), the commits landed in the periods the report of its
/// period at its `at` reads at `now`, by the served checkout's and the
/// host's `[kpi]` settings ([`crate::domain::kpi::landed_in_periods`]);
/// none for no checkout, whose settings and events are not read. No Git
/// or program runs.
pub fn report_inputs(
    params: &super::queue_commands::ReportInputsParams,
    repository: Option<String>,
    served: Option<&Path>,
    diagram: impl FnOnce() -> DiagramDraft,
    sources: &ReportInputsSources<'_>,
    now: i64,
) -> Result<ReportInputs> {
    let landed = match params.checkout.as_deref() {
        None => Vec::new(),
        Some(checkout) => {
            let checkout = accept_checkout(sources.files, checkout, served)?;
            let query = KpiQuery {
                period: params.period.parse().map_err(anyhow::Error::msg)?,
                at: params
                    .at
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .map_err(anyhow::Error::msg)?,
                ..KpiQuery::default()
            };
            let (utc_offset_secs, config) = (sources.settings)(checkout)?;
            crate::domain::kpi::landed_in_periods(
                &(sources.events)()?,
                &query,
                now,
                utc_offset_secs,
                &config,
            )
        }
    };
    Ok(ReportInputs {
        diagram: diagram(),
        repository,
        landed,
    })
}

/// `report`'s params fit to a request of at most `max` bytes of params
/// ([`serde_json::to_vec`]): with their SVG when that fits, else with the
/// diagram sent as not drawn, and why; params larger even then are
/// refused with their size, and nothing is sent.
pub fn fit_request(params: &mut super::queue_commands::ReportParams, max: usize) -> Result<()> {
    let size = |params: &super::queue_commands::ReportParams| {
        serde_json::to_vec(params).map(|bytes| bytes.len())
    };
    if size(params)? <= max {
        return Ok(());
    }
    if let Some(diagram) = params
        .diagram
        .as_mut()
        .filter(|diagram| diagram.svg.is_some())
    {
        diagram.svg = None;
        diagram.reason =
            Some("the SVG d2 wrote is larger than a request to the queue service takes".to_owned());
    }
    let size = size(params)?;
    anyhow::ensure!(
        size <= max,
        "the report's request is {size} bytes, larger than the {max} a request to the queue \
         service takes"
    );
    Ok(())
}

/// What the queue service's client reads and draws `report`'s request
/// with, outside the service (ADR-t3102-3 decision 5).
pub struct ReportClient<'a> {
    /// The main checkout of the repository whose Git common directory it
    /// is given.
    pub main_checkout: &'a dyn Fn(&Path) -> Result<PathBuf>,
    /// The `[areas]` of a checkout's `dagq.toml` and its Git.
    pub areas: &'a dyn Fn(&Path) -> Result<super::areas::AreaReader>,
    pub diagram: &'a DiagramRenderer,
}

/// `params` as the queue service's client sends `report`, from the
/// [`ReportInputs`] `call` answers ([`super::queue_commands::QueueCommand::ReportInputs`]
/// of the report's period and `--at`): the main checkout of the repository
/// the queue is bound to (none when it is bound to none or none is found),
/// the areas of the commits landed in the report's periods (asked again
/// with that checkout only when its `dagq.toml` maps areas; none without a
/// map), the near-term diagram drawn by `client`'s renderer, all fit to a
/// request ([`fit_request`]).
pub fn prepare_request(
    params: &mut super::queue_commands::ReportParams,
    client: &ReportClient<'_>,
    mut call: impl FnMut(&super::queue_commands::QueueCommand) -> Result<serde_json::Value>,
) -> Result<()> {
    use super::queue_commands::{QueueCommand, ReportInputsParams};
    let mut inputs_of = |checkout: Option<&Path>| -> Result<ReportInputs> {
        serde_json::from_value(call(&QueueCommand::ReportInputs(ReportInputsParams {
            period: params.period.clone(),
            at: params.at.clone(),
            checkout: checkout.map(Path::to_path_buf),
        }))?)
        .context("the report's inputs do not read")
    };
    let inputs = inputs_of(None)?;
    let checkout = inputs
        .repository
        .as_deref()
        .and_then(|common_dir| (client.main_checkout)(Path::new(common_dir)).ok());
    let areas = match &checkout {
        Some(checkout) => {
            let reader = (client.areas)(checkout)?;
            match reader.map {
                Some(_) => reader.commit_areas(&inputs_of(Some(checkout))?.landed),
                None => None,
            }
        }
        None => None,
    };
    params.diagram = Some(inputs.diagram.render(client.diagram));
    params.checkout = checkout;
    params.areas = areas.map(|areas| areas.into_iter().collect());
    fit_request(
        params,
        crate::domain::queue_service::MAX_REQUEST_BYTES
            - crate::domain::telemetry::REQUEST_ENVELOPE_BYTES,
    )
}

/// The checkout the queue service reads a report's `dagq.toml` in, for
/// the `checkout` its caller sent: `served`, the main checkout the service
/// found from the queue's binding with no Git run
/// ([`crate::infrastructure::adapters::main_checkout_from_files`]), and
/// only when `checkout` has the same real path. Nothing in `checkout`
/// (its `.git` included) is read to judge it, so a directory whose `.git`
/// file names the bound common directory is still refused; so is any
/// checkout of a queue bound to none or whose main checkout is not found.
pub fn accept_checkout<'a>(
    files: &dyn RunFiles,
    checkout: &Path,
    served: Option<&'a Path>,
) -> Result<&'a Path> {
    let real = |path: &Path| files.canonicalize(path).ok();
    served
        .filter(|served| real(served).is_some_and(|served| real(checkout) == Some(served)))
        .with_context(|| {
            format!(
                "{} is not the main checkout of the repository the queue is bound to",
                checkout.display()
            )
        })
}

/// [`DiagramDraft`] of the queue now. A queue with no near-term task has
/// no source.
pub fn diagram_draft(queue: &(impl TaskReads + ?Sized)) -> DiagramDraft {
    let not_drawn = |reason: String| DiagramDraft::NotDrawn {
        tasks: Vec::new(),
        reason,
    };
    let graph = match queue.graph_input() {
        Ok(input) => super::dependency_graph(input, None),
        Err(error) => {
            return not_drawn(format!("could not read the dependency graph: {error:#}"));
        }
    };
    let titles: BTreeMap<_, _> = match queue.list_goals() {
        Ok(goals) => goals
            .into_iter()
            .map(|goal| (goal.id, goal.title))
            .collect(),
        Err(error) => return not_drawn(format!("could not read the goals: {error:#}")),
    };
    let near_term = super::diagram::near_term(&graph, &titles);
    let tasks: Vec<i64> = near_term.task_ids().iter().map(|id| id.as_i64()).collect();
    if tasks.is_empty() {
        return DiagramDraft::NotDrawn {
            tasks,
            reason: "no near-term task to draw".to_owned(),
        };
    }
    DiagramDraft::Source {
        tasks,
        source: near_term.to_d2(),
    }
}

/// [`make`] with the diagram drawn already.
pub fn make_with(
    queue: &(impl PlanningRecords + QueueRecords + SupervisorReads + RunLog + TaskReads + ?Sized),
    setup: &ReportSetup,
    now: i64,
    period: Period,
    at: Option<Cursor>,
    diagram: DiagramSection,
) -> Result<Report> {
    let kpi = super::kpi::kpi(
        queue,
        now,
        setup.host,
        &setup.config,
        &setup.areas,
        &KpiQuery {
            period,
            at,
            ..KpiQuery::default()
        },
        setup.host_reader(),
    )?;
    let findings: Vec<_> = queue
        .findings(&FindingQuery::default())?
        .into_iter()
        .map(|view| view.finding.attributes())
        .collect();
    Ok(Report::new(
        kpi,
        period,
        now * 1000,
        &setup.build,
        &findings,
        diagram,
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
/// recording each as `report_written` by `supervisor`, with the report
/// made. The dependency diagram is drawn once, for the first report owed,
/// and shared by the others. A report another supervisor recorded
/// meanwhile is not recorded twice nor returned.
pub fn write_due(
    queue: &(impl PlanningRecords + QueueRecords + SupervisorReads + RunLog + TaskReads + ?Sized),
    files: &dyn RunFiles,
    setup: &ReportSetup,
    now: i64,
    supervisor: &LeaseToken,
) -> Result<Vec<(Written, Report)>> {
    let written = queue.reports_written()?;
    let offset_ms = setup.host.utc_offset_secs * 1000;
    let mut done = Vec::new();
    let mut drawn: Option<DiagramSection> = None;
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
        let diagram = drawn
            .get_or_insert_with(|| diagram(queue, &setup.diagram))
            .clone();
        let report = make_with(queue, setup, now, period, Some(Cursor::Time(at)), diagram)
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
            done.push((output, report));
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
/// Only `daily/` and `weekly/` are read, and a file there not named as a
/// report is left alone but for a stale temporary file; the throughput
/// reviews' `reviews/` beside them is never touched.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params(svg: Option<String>, areas: usize) -> super::super::queue_commands::ReportParams {
        super::super::queue_commands::ReportParams {
            period: "day".to_owned(),
            at: None,
            out: None,
            print: true,
            diagram: Some(DrawnDiagram {
                tasks: vec![1],
                d2_source: true,
                svg,
                reason: None,
            }),
            checkout: None,
            areas: Some(
                (0..areas)
                    .map(|commit| (format!("{commit:040}"), vec!["runtime".to_owned()]))
                    .collect(),
            ),
        }
    }

    /// Params at the limit are sent as they are; one byte past it, the SVG
    /// is sent as not drawn, and why; params past it without the SVG are
    /// refused.
    #[test]
    fn a_report_s_request_is_fit_to_the_limit_at_its_bound() {
        let svg = format!("<svg>{}</svg>", "<rect/>".repeat(100));
        let at = serde_json::to_vec(&params(Some(svg.clone()), 3))
            .unwrap()
            .len();
        let mut fits = params(Some(svg.clone()), 3);
        fit_request(&mut fits, at).unwrap();
        assert_eq!(fits, params(Some(svg.clone()), 3));

        let mut over = params(Some(svg), 3);
        fit_request(&mut over, at - 1).unwrap();
        let diagram = over.diagram.as_ref().unwrap();
        assert_eq!(diagram.svg, None);
        assert!(
            diagram
                .reason
                .as_deref()
                .unwrap()
                .contains("larger than a request"),
            "{diagram:?}"
        );
        assert!(serde_json::to_vec(&over).unwrap().len() < at);

        let bare = serde_json::to_vec(&params(None, 3)).unwrap().len();
        let mut refused = params(None, 3);
        let error = fit_request(&mut refused, bare - 1).unwrap_err().to_string();
        assert!(error.contains(&format!("is {bare} bytes")), "{error}");
    }

    /// The client's request: for a bound queue whose main checkout maps
    /// areas, the inputs are asked again with that checkout and each landed
    /// commit's areas are read with its Git, which leaves every commit out
    /// when it cannot be read; with no map they are asked once and no areas
    /// are sent; a queue bound to none, or whose main checkout is not found,
    /// is sent with neither. The diagram is drawn by the client's renderer.
    #[test]
    fn a_report_s_request_reads_the_areas_only_with_a_mapped_checkout() {
        use super::super::areas::AreaReader;
        use super::super::queue_commands::{QueueCommand, ReportInputsParams};
        use crate::domain::areas::AreaMap;
        use std::collections::HashMap;
        let commit = format!("{:040}", 1);
        let answer = |repository: Option<&str>, landed: &[&str]| {
            serde_json::to_value(ReportInputs {
                diagram: DiagramDraft::Source {
                    tasks: vec![1],
                    source: "a -> b".to_owned(),
                },
                repository: repository.map(str::to_owned),
                landed: landed.iter().map(|commit| (*commit).to_owned()).collect(),
            })
            .unwrap()
        };
        let map = AreaMap::new(vec![("docs".into(), vec!["docs/**".into()])]).unwrap();
        let reader = |map: Option<AreaMap>, git: bool| {
            let commit = commit.clone();
            AreaReader {
                map,
                changed: Arc::new(move |commits: &[String]| {
                    anyhow::ensure!(git, "no git");
                    assert_eq!(commits, std::slice::from_ref(&commit));
                    Ok(HashMap::from([(
                        commit.clone(),
                        vec!["docs/a.md".to_owned()],
                    )]))
                }),
                judged: None,
            }
        };
        let drawn = DiagramRenderer(Arc::new(|source: &str| Ok(format!("<svg>{source}</svg>"))));
        let prepared =
            |repository: Option<&str>, found: bool, areas: Option<AreaMap>, git: bool| {
                let mut asked = Vec::new();
                let mut sent = params(None, 0);
                sent.areas = None;
                let main_checkout = move |common_dir: &Path| {
                    assert_eq!(common_dir, Path::new("/r/.git"));
                    anyhow::ensure!(found, "no main checkout");
                    Ok(PathBuf::from("/r"))
                };
                let areas_of = |checkout: &Path| {
                    assert_eq!(checkout, Path::new("/r"));
                    Ok(reader(areas.clone(), git))
                };
                prepare_request(
                    &mut sent,
                    &ReportClient {
                        main_checkout: &main_checkout,
                        areas: &areas_of,
                        diagram: &drawn,
                    },
                    |command: &QueueCommand| {
                        let QueueCommand::ReportInputs(inputs) = command else {
                            panic!("{command:?}");
                        };
                        asked.push(inputs.clone());
                        let landed = [commit.as_str()];
                        let landed = if inputs.checkout.is_some() {
                            &landed[..]
                        } else {
                            &[]
                        };
                        Ok(answer(repository, landed))
                    },
                )
                .unwrap();
                (sent, asked)
            };
        let first = ReportInputsParams {
            period: "day".to_owned(),
            at: None,
            checkout: None,
        };
        let again = ReportInputsParams {
            checkout: Some(PathBuf::from("/r")),
            ..first.clone()
        };

        let (sent, asked) = prepared(Some("/r/.git"), true, Some(map.clone()), true);
        assert_eq!(asked, [first.clone(), again.clone()]);
        assert_eq!(sent.checkout.as_deref(), Some(Path::new("/r")));
        assert_eq!(
            sent.areas,
            Some(BTreeMap::from([(commit.clone(), vec!["docs".to_owned()])]))
        );
        assert_eq!(
            sent.diagram.unwrap().svg.as_deref(),
            Some("<svg>a -> b</svg>")
        );

        let (sent, asked) = prepared(Some("/r/.git"), true, Some(map), false);
        assert_eq!(asked, [first.clone(), again]);
        assert_eq!(sent.areas, Some(BTreeMap::new()));

        let (sent, asked) = prepared(Some("/r/.git"), true, None, true);
        assert_eq!(asked, std::slice::from_ref(&first));
        assert_eq!(sent.checkout.as_deref(), Some(Path::new("/r")));
        assert_eq!(sent.areas, None);

        for (repository, found) in [(None, true), (Some("/r/.git"), false)] {
            let (sent, asked) = prepared(repository, found, None, true);
            assert_eq!(
                asked,
                std::slice::from_ref(&first),
                "{repository:?} {found}"
            );
            assert_eq!((sent.checkout, sent.areas), (None, None));
        }
    }

    /// `report_inputs` answers the landed commits of the report's periods
    /// only for a checkout of the bound repository, read by that
    /// checkout's settings; with no checkout it reads neither the settings
    /// nor the events, and another repository's checkout is refused.
    #[test]
    fn report_inputs_lands_commits_only_for_the_bound_checkout() {
        use super::super::queue_commands::ReportInputsParams;
        use crate::domain::{EventId, RunEvent, RunId, TaskId};
        let files = super::super::memory_files::MemoryFiles::default();
        files.put_dir(Path::new("/r/main"));
        files.put_dir(Path::new("/r/main/.git"));
        files.put_dir(Path::new("/r/other"));
        files.put_dir(Path::new("/r/other/.git"));
        let served = Some(Path::new("/r/main"));
        // 2026-09-29T01:00:00Z, an hour after the run landed.
        let now = 1_790_640_000 + 3600;
        let events = vec![RunEvent {
            id: EventId::new(1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("r").unwrap()),
            kind: "run_integrated".into(),
            payload: json!({"commit": "c"}),
            created_at: "2026-09-29T00:00:00.000Z".into(),
            actor: None,
        }];
        let config = KpiConfig::merge(None, None);
        let read = std::cell::Cell::new(0);
        let settings = |checkout: &Path| {
            assert_eq!(checkout, Path::new("/r/main"));
            read.set(read.get() + 1);
            Ok((0, config.clone()))
        };
        let events_of = || {
            read.set(read.get() + 1);
            Ok(events.clone())
        };
        let sources = ReportInputsSources {
            files: &files,
            settings: &settings,
            events: &events_of,
        };
        let draft = DiagramDraft::NotDrawn {
            tasks: Vec::new(),
            reason: "no near-term task to draw".into(),
        };
        let params = |checkout: Option<&str>| ReportInputsParams {
            period: "day".into(),
            at: None,
            checkout: checkout.map(PathBuf::from),
        };
        let bound = Some("/r/main/.git".to_owned());

        let first = report_inputs(
            &params(None),
            bound.clone(),
            served,
            || draft.clone(),
            &sources,
            now,
        )
        .unwrap();
        assert_eq!(
            first,
            ReportInputs {
                diagram: draft.clone(),
                repository: bound.clone(),
                landed: Vec::new(),
            }
        );
        assert_eq!(read.get(), 0);

        let again = report_inputs(
            &params(Some("/r/main")),
            bound.clone(),
            served,
            || draft.clone(),
            &sources,
            now,
        )
        .unwrap();
        let query = KpiQuery {
            period: Period::Day,
            ..KpiQuery::default()
        };
        let expected = crate::domain::kpi::landed_in_periods(&events, &query, now, 0, &config);
        assert_eq!(expected, ["c"]);
        assert_eq!(again.landed, expected);
        assert_eq!(read.get(), 2);

        let error = report_inputs(
            &params(Some("/r/other")),
            bound,
            served,
            || draft.clone(),
            &sources,
            now,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("is not the main checkout"), "{error}");
        assert_eq!(read.get(), 2);
    }

    /// A checkout is accepted only as the main checkout the service found
    /// from the binding, by its real path: a worktree whose `.git` file
    /// names a directory under the bound common directory, another
    /// repository's checkout, a missing directory and any checkout with no
    /// served main checkout are refused.
    #[test]
    fn only_the_served_main_checkout_is_accepted() {
        let files = super::super::memory_files::MemoryFiles::default();
        let main = Path::new("/r/main");
        let common = main.join(".git");
        files.put_dir(main);
        files.put_dir(&common);
        files.put_dir(&common.join("worktrees/w"));
        let worktree = Path::new("/r/w");
        files.put_dir(worktree);
        files.put(
            &worktree.join(".git"),
            std::time::UNIX_EPOCH,
            &format!("gitdir: {}\n", common.join("worktrees/w").display()),
        );
        let other = Path::new("/r/other");
        files.put_dir(other);
        files.put_dir(&other.join(".git"));
        assert_eq!(accept_checkout(&files, main, Some(main)).unwrap(), main);
        for refused in [worktree, other, Path::new("/r/bare")] {
            let error = accept_checkout(&files, refused, Some(main))
                .unwrap_err()
                .to_string();
            assert!(error.contains("is not the main checkout"), "{error}");
        }
        assert!(accept_checkout(&files, main, None).is_err());
    }

    /// The SVG a client sends with `report` is kept only as
    /// [`DiagramSection::drawn`] keeps d2's: as written when it is
    /// well-formed, for the page to show as an image, or not drawn, and
    /// why; one not drawn keeps its reason.
    #[test]
    fn a_client_s_svg_is_kept_as_d2_s_is() {
        for (svg, kept) in [
            (
                "<svg><style>@import url(https://example.com/a.css);</style><rect/></svg>",
                true,
            ),
            ("<svg><rect>", false),
        ] {
            let section = DiagramSection::from(DrawnDiagram {
                tasks: vec![1],
                d2_source: true,
                svg: Some(svg.to_owned()),
                reason: None,
            });
            assert_eq!(section, DiagramSection::drawn(vec![1], svg));
            assert_eq!(section.svg.as_deref(), kept.then_some(svg), "{svg}");
        }
        let none = DiagramSection::from(DrawnDiagram {
            tasks: vec![2],
            d2_source: false,
            svg: None,
            reason: Some("no near-term task to draw".to_owned()),
        });
        assert_eq!(
            none,
            DiagramSection::not_drawn(vec![2], false, "no near-term task to draw")
        );
    }
}
