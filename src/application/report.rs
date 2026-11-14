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
//!
//! Through the queue service, `report_inputs` is read once for the d2
//! source and bound repository; `landed` is empty and no client Git is read
//! for areas. The client draws the diagram and fits the request with
//! [`fit_request`]: omit an oversized SVG with a reason, then refuse if the
//! request still does not fit. The service builds the report with its own
//! `TZ` and `host.toml`. Without `--out` it writes only under the queue's
//! `reports/`; with `--out` it returns JSON, HTML and retention settings for
//! the client to write, prune and index with its own rights. Params naming
//! `out` are refused before writing anything on the service side.
//!
//! Only the bound main checkout accepted by [`accept_checkout`] supplies
//! `dagq.toml`, as for stats and KPI. Its text is read by
//! `crate::infrastructure::run_env`'s `read_config`: bounded in size and only
//! from a regular file, refusing devices and FIFOs even through symlinks.
//! Malformed SVG is omitted; well-formed XML rooted at `svg` is embedded as
//! an image data URL, whose scripts, external reads and styles cannot act
//! on the page (see `crate::domain::kpi::report::svg`).

use crate::domain::LeaseToken;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{LedgerStore, LedgerWindowRead, QueueRecords, RunFiles, kpi::Host, ledger_window_of};
use crate::domain::{
    EventId, RunEvent, TaskId,
    areas::AreaMap,
    kpi::{
        DAY_MS, KpiConfig, KpiQuery, Period,
        range::{self, RangeReads, Undecided},
        report::{self, DiagramSection, INDEX_FILE, Keep, Report, ReportFile},
    },
    ledger::{
        attributes::Folded,
        findings,
        window::bounds::Answers,
        window::{LedgerWindow, WindowQuery},
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
    /// The `[areas]` map the landed runs' areas are judged by from main's
    /// history the ledger holds (ADR-t980-1); `None` without it.
    pub areas: Option<AreaMap>,
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

/// The ledger's window the KPIs of `queries` are made of at the unix
/// second `now` with `setup` (each report [`make_with`] makes, the ones
/// [`write_due`] owes, the observer's days and weeks, the check of the
/// targets of `super::push`), and the findings, the goals and the
/// near-term diagram of its end: the bounds the queries take inside it
/// declared with its query ([`super::kpi::declared`]) and answered by its
/// read, all of one snapshot ([`LedgerStore::ledger_read_snapshot`]).
///
/// # The range it reads
///
/// Each query's range is decided by [`crate::domain::kpi::range::plan`]
/// of the query, now and `setup`'s `[kpi]` settings, an event id `--at`
/// taking the time the snapshot gives it ([`super::cursor_time_in`]); the
/// read is from the earliest of their starts
/// ([`WindowQuery::read_from_ms`]), the union of the ranges, with no upper
/// bound, so a report with an old `--at` (a backfill) reads as `kpi
/// --at` does and one without reads up to now. Through that time the
/// store's read ([`LedgerStore::ledger_window`], "The range a read from a
/// time reads") gives what the periods need of the settled history before
/// it by its indexes and seeks, whatever the clocks did: the marks by
/// when they took effect, their retractions, each claim's previous one,
/// the queue rows by their kinds' ranges and what the range names of them
/// (the waits, the supervisors' lives, the snapshots, main's and CI's
/// record, the goals' and the findings'). The forecast's snapshots of
/// before it are those a task's or a goal's finish in it scores: those
/// naming that target since its end before the finish, not those of the
/// periods it ended before. A snapshot older than that settled history is
/// no reason to read every row: the marks from it to the range are read as
/// the second step of the forecast's read, as many as that target's open
/// period holds (a goal's snapshots too). The findings unsettled and
/// the near-term tasks are folds of the window's end, which such a read
/// answers as every row.
///
/// Every row is read, as before the range, when any query has no range
/// ([`crate::domain::kpi::range::Undecided`]: one asking for the whole
/// ledger, `--last 0`, an event id cursor that names no event or whose time
/// the snapshot cannot give within its limit of candidates or off the
/// record's form, a comparison), when there is no query, and when the
/// window read from the time does not answer an event id cursor's time as
/// the range was decided by ([`read_holds`]; read again whole in the same
/// snapshot). The cursor's time is that of the events at or before it that
/// the window holds, of a time up to its end: an event a clock ahead of the
/// DB's wrote past now is none of them.
/// The store reads the queue rows of a kind whole again itself when it
/// cannot use its views of them (none, a mark of them missing or not of
/// the rows' position, or a visibility it cannot judge). The answers are
/// those of every row either way.
pub fn window(
    ledger: &(impl LedgerStore + ?Sized),
    now: i64,
    setup: &ReportSetup,
    queries: Vec<KpiQuery>,
) -> Result<LedgerWindow> {
    if !queries.iter().any(names_an_event) {
        // No range of these needs a read: the store's own snapshot.
        let mut query = window_query(now, setup, &queries);
        query.read_from_ms = read_from(
            &queries,
            &setup.config,
            now,
            setup.host.utc_offset_secs,
            &SnapshotCursors::of(None),
        );
        return ledger.ledger_window(&query);
    }
    let expected = ledger.ledger_events_expected()?;
    let mut taken = None;
    ledger.ledger_read_snapshot(&mut |snapshot| {
        taken = Some(window_in(snapshot, now, setup, &queries, expected)?);
        Ok(())
    })?;
    taken.context("the ledger's snapshot read nothing")
}

/// Whether `query` names an event id, whose time only the snapshot gives.
fn names_an_event(query: &KpiQuery) -> bool {
    let event = |cursor: Option<Cursor>| matches!(cursor, Some(Cursor::Event(_)));
    event(query.at)
        || event(query.since)
        || event(query.until)
        || match query.compare {
            Some(crate::domain::kpi::CompareSpec::At(cursor)) => event(Some(cursor)),
            Some(crate::domain::kpi::CompareSpec::Windows(windows)) => windows
                .iter()
                .any(|(a, b)| event(Some(*a)) || event(Some(*b))),
            None => false,
        }
}

/// The window's query of `queries` at `now`, with no range yet.
fn window_query(now: i64, setup: &ReportSetup, queries: &[KpiQuery]) -> WindowQuery {
    WindowQuery {
        now_ms: super::stats::now_ms(now),
        bounds: super::kpi::declared(
            queries.to_vec(),
            now,
            setup.host,
            &setup.config,
            setup.areas.as_ref(),
        ),
        ..WindowQuery::default()
    }
}

/// [`window`] of `snapshot`, with room for `expected` events if it reads
/// every row.
pub fn window_in(
    snapshot: &dyn LedgerWindowRead,
    now: i64,
    setup: &ReportSetup,
    queries: &[KpiQuery],
    expected: usize,
) -> Result<LedgerWindow> {
    let mut query = window_query(now, setup, queries);
    let cursors = SnapshotCursors::of(Some((snapshot, query.now_ms)));
    query.read_from_ms = read_from(
        queries,
        &setup.config,
        now,
        setup.host.utc_offset_secs,
        &cursors,
    );
    if let Some(error) = cursors.failed.into_inner() {
        return Err(error);
    }
    if query.read_from_ms.is_none() {
        return ledger_window_of(snapshot, &query, expected);
    }
    let window = ledger_window_of(snapshot, &query, 0)?;
    let only_at = queries.iter().all(|query| {
        !names_an_event(&KpiQuery {
            at: None,
            ..query.clone()
        })
    });
    if read_holds(&cursors.taken.borrow(), window.answers(), only_at) {
        return Ok(window);
    }
    drop(window);
    query.read_from_ms = None;
    ledger_window_of(snapshot, &query, expected)
}

/// Whether a window read from the range decided by the event id cursors'
/// times `taken` holds what its queries need, by the window's `answers` of
/// those cursors: each one answered as taken, or later when every event
/// cursor is an `--at` (`only_at`; a later `--at` starts its periods later,
/// in what was read, and any other cursor must stand where it was taken).
/// An earlier time, no time, or no answer does not: the window is read
/// again of every row.
fn read_holds(taken: &[(EventId, i64)], answers: Option<&Answers>, only_at: bool) -> bool {
    taken.iter().all(
        |(id, ms)| match answers.and_then(|answers| answers.time_through(*id)) {
            Some(Some(answer)) => answer == *ms || (only_at && answer > *ms),
            _ => false,
        },
    )
}

/// What [`range::plan`] takes as an event id cursor's time of what the
/// snapshot found ([`super::cursor_time_in`]): its time, `None` when no
/// event is at or before it ([`Undecided::NoEvent`] then), and
/// [`Undecided::OutOfTimeOrder`] when the snapshot could not say.
fn cursor_of(cursor: EventId, found: Option<Option<i64>>) -> range::Read<Option<i64>> {
    found.ok_or(Undecided::OutOfTimeOrder(cursor))
}

/// The start of the union of the ranges of `queries` at `now` with
/// `config` ([`crate::domain::kpi::range::plan`]), the event id cursors'
/// times read by `reads`: none when there is no query or any has no
/// range, which reads every row.
pub fn read_from(
    queries: &[KpiQuery],
    config: &KpiConfig,
    now: i64,
    utc_offset_secs: i64,
    reads: &impl RangeReads,
) -> Option<i64> {
    // A comparison's cohort is cut by the goals of the window's tasks; no
    // query of a report compares, and one that did reads every row (its
    // cohort's reads are none of `reads`).
    let goals = HashMap::new();
    let mut from: Option<i64> = None;
    for query in queries {
        let plan = range::plan(query, config, now, utc_offset_secs, reads, &goals).ok()?;
        from = Some(from.map_or(plan.range.from_ms, |from| from.min(plan.range.from_ms)));
    }
    from
}

/// The reads [`range::plan`] takes of a snapshot for a report's queries:
/// the time of an event id cursor ([`super::cursor_time_in`]) of a window
/// ending at the time with it, each one taken kept to be checked against
/// the window's answer; no other read, so a query that would need one (a
/// comparison's cohort) has no range. Of no snapshot, no cursor has a time.
struct SnapshotCursors<'a> {
    snapshot: Option<(&'a dyn LedgerWindowRead, i64)>,
    taken: RefCell<Vec<(EventId, i64)>>,
    failed: RefCell<Option<anyhow::Error>>,
}

impl<'a> SnapshotCursors<'a> {
    const fn of(snapshot: Option<(&'a dyn LedgerWindowRead, i64)>) -> Self {
        Self {
            snapshot,
            taken: RefCell::new(Vec::new()),
            failed: RefCell::new(None),
        }
    }
}

impl RangeReads for SnapshotCursors<'_> {
    fn recorded_effective(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn retractions_of(&self, _: EventId) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn claims_written(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn previous_claim_with(&self, _: &str, _: EventId) -> range::Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn start_announcing(
        &self,
        _: &str,
        _: &str,
        _: EventId,
        _: EventId,
    ) -> range::Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn start_before_in_time(&self, _: i64) -> range::Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn cursor_time(&self, cursor: EventId) -> range::Read<Option<i64>> {
        let Some((snapshot, end_ms)) = self.snapshot else {
            return Err(Undecided::Fallback);
        };
        match super::cursor_time_in(snapshot, cursor, end_ms) {
            Ok(found) => {
                if let Some(Some(ms)) = found {
                    self.taken.borrow_mut().push((cursor, ms));
                }
                cursor_of(cursor, found)
            }
            Err(error) => {
                self.failed.borrow_mut().get_or_insert(error);
                Err(Undecided::Fallback)
            }
        }
    }
    fn finishes_written(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn previous_finish(&self, _: &RunEvent) -> range::Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn first_of_target(&self, _: &RunEvent) -> range::Read<Option<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn snapshots_between(&self, _: EventId, _: EventId) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn plan_changes_written(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn marks_after(&self, _: EventId) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
    fn run_finishes(&self) -> range::Read<Vec<(TaskId, i64)>> {
        Err(Undecided::Fallback)
    }
    fn comparison_events(&self) -> range::Read<Vec<RunEvent>> {
        Err(Undecided::Fallback)
    }
}

/// The window [`report_inputs`] draws its near-term diagram of at the unix
/// second `now`: a fold of the window's end, which a read from any time up
/// to now answers as every row, so it is [`window`]'s of
/// [`diagram_queries`] with [`diagram_setup`] (the service has the report's
/// own settings only with the report).
pub fn diagram_window(ledger: &(impl LedgerStore + ?Sized), now: i64) -> Result<LedgerWindow> {
    window(ledger, now, &diagram_setup(), diagram_queries())
}

/// What [`diagram_window`] decides its range with: the default `[kpi]`
/// settings in UTC, and nothing drawn or written.
pub fn diagram_setup() -> ReportSetup {
    ReportSetup {
        root: PathBuf::new(),
        host: Host {
            utc_offset_secs: 0,
            cores: None,
            rust_toolchain: false,
        },
        config: KpiConfig::default(),
        keep: Keep::default(),
        build: String::new(),
        diagram: DiagramRenderer(Arc::new(|_| anyhow::bail!("no diagram is drawn here"))),
        areas: None,
    }
}

/// The query [`diagram_window`]'s range is of: today's report.
pub fn diagram_queries() -> Vec<KpiQuery> {
    vec![report_query(Period::Day, None)]
}

/// The query of the KPIs of the report of `period` that holds `at`
/// ([`make_with`]).
pub fn report_query(period: Period, at: Option<Cursor>) -> KpiQuery {
    KpiQuery {
        period,
        at,
        ..KpiQuery::default()
    }
}

/// The near-term dependency diagram of the ledger's `window`, drawn by
/// `render`: its tasks and the SVG the page shows as an image, or why
/// there is none. A window with no near-term task is not drawn.
pub fn diagram(window: &LedgerWindow, render: &DiagramRenderer) -> DiagramSection {
    diagram_draft(window).draw(render)
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
/// near-term diagram's draft and the Git common directory of the
/// repository the queue is bound to (none for a queue bound to none).
/// `landed` is always empty: the landed runs' areas come from main's
/// history the ledger holds, so the client reads no Git for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportInputs {
    pub diagram: DiagramDraft,
    pub repository: Option<String>,
    pub landed: Vec<String>,
}

/// What the queue service checks a checkout `report_inputs` names with.
pub struct ReportInputsSources<'a> {
    pub files: &'a dyn RunFiles,
}

/// `report_inputs` (ADR-t3102-3 decision 5): the near-term diagram's draft
/// `diagram` reads (after the rest) and the bound repository
/// `repository`; a checkout `params` names is accepted only as
/// [`accept_checkout`] accepts it against `served`, the main checkout the
/// service found from the binding. No Git or program runs, and no event
/// is read.
pub fn report_inputs(
    params: &super::queue_commands::ReportInputsParams,
    repository: Option<String>,
    served: Option<&Path>,
    diagram: impl FnOnce() -> DiagramDraft,
    sources: &ReportInputsSources<'_>,
) -> Result<ReportInputs> {
    if let Some(checkout) = params.checkout.as_deref() {
        accept_checkout(sources.files, checkout, served)?;
    }
    Ok(ReportInputs {
        diagram: diagram(),
        repository,
        landed: Vec::new(),
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
    pub diagram: &'a DiagramRenderer,
}

/// `params` as the queue service's client sends `report`, from the
/// [`ReportInputs`] `call` answers ([`super::queue_commands::QueueCommand::ReportInputs`]
/// of the report's period and `--at`): the main checkout of the repository
/// the queue is bound to (none when it is bound to none or none is found)
/// and the near-term diagram drawn by `client`'s renderer, fit to a
/// request ([`fit_request`]). No areas are sent: the service judges them
/// from the ledger.
pub fn prepare_request(
    params: &mut super::queue_commands::ReportParams,
    client: &ReportClient<'_>,
    mut call: impl FnMut(&super::queue_commands::QueueCommand) -> Result<serde_json::Value>,
) -> Result<()> {
    use super::queue_commands::{QueueCommand, ReportInputsParams};
    let inputs: ReportInputs =
        serde_json::from_value(call(&QueueCommand::ReportInputs(ReportInputsParams {
            period: params.period.clone(),
            at: params.at.clone(),
            checkout: None,
        }))?)
        .context("the report's inputs do not read")?;
    let checkout = inputs
        .repository
        .as_deref()
        .and_then(|common_dir| (client.main_checkout)(Path::new(common_dir)).ok());
    params.diagram = Some(inputs.diagram.render(client.diagram));
    params.checkout = checkout;
    params.areas = None;
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

/// [`DiagramDraft`] of the ledger's `window`: the tasks' and goals'
/// attributes folded at its end (task rows' records and the queue event
/// rows of the goals', [`LedgerWindow::attributes`]), not the state
/// tables. A window before the attributes' baseline is not drawn, and
/// neither is one with no near-term task.
pub fn diagram_draft(window: &LedgerWindow) -> DiagramDraft {
    let attributes = match window.attributes() {
        Folded::Attributes(attributes) => attributes,
        Folded::BeforeBaseline => {
            return DiagramDraft::NotDrawn {
                tasks: Vec::new(),
                reason: "the tasks' attributes are not recorded before their baseline".to_owned(),
            };
        }
    };
    let graph = super::dependency_graph(super::GraphInput::folded(&attributes), None);
    let titles: BTreeMap<_, _> = attributes
        .goal_summaries()
        .into_iter()
        .map(|goal| (goal.id, goal.title))
        .collect();
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

/// The report of the `period` that holds `at` (now without), at the unix
/// second `now`, of the ledger's `window` ([`window`]), with `diagram`
/// drawn already: its KPIs ([`super::kpi::of_window`]) and the findings
/// unsettled at its end, none before the findings' baseline.
pub fn make_with(
    window: &LedgerWindow,
    setup: &ReportSetup,
    now: i64,
    period: Period,
    at: Option<Cursor>,
    diagram: DiagramSection,
) -> Result<Report> {
    let kpi = super::kpi::of_window(
        window,
        now,
        setup.host,
        &setup.config,
        setup.areas.as_ref(),
        &report_query(period, at),
    )?;
    let findings = match window.findings() {
        findings::Folded::Findings(findings) => findings.unsettled(),
        findings::Folded::BeforeBaseline => Vec::new(),
    };
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
/// retention and the index ([`write_files`] of [`render`]).
pub fn write(
    files: &dyn RunFiles,
    setup: &ReportSetup,
    report: &Report,
    period: Period,
    now: i64,
) -> Result<Written> {
    write_files(files, &setup.root, &render(setup, report, period, now)?)
}

/// A report as the files its caller writes under a root of its own
/// (`report --out` through the queue service, ADR-t3102-3 decision 5): the
/// JSON and the HTML [`write`] writes, and what the retention and the
/// index under that root are judged with (the host's UTC offset, the
/// `[report]` retention of `host.toml` and the unix second the report was
/// made at).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportFiles {
    pub period: String,
    pub label: String,
    pub partial: bool,
    pub json: String,
    pub html: String,
    pub utc_offset_secs: i64,
    pub keep: Keep,
    pub now: i64,
}

/// `report` of `period` made at `now` with `setup`, as [`ReportFiles`].
pub fn render(
    setup: &ReportSetup,
    report: &Report,
    period: Period,
    now: i64,
) -> Result<ReportFiles> {
    let file = report.file_name(period);
    let mut json = serde_json::to_string_pretty(report)?;
    json.push('\n');
    Ok(ReportFiles {
        period: period.as_str().to_owned(),
        label: file.label,
        partial: file.partial,
        json,
        html: report::render_html(report),
        utc_offset_secs: setup.host.utc_offset_secs,
        keep: setup.keep,
        now,
    })
}

/// Write `rendered` under `root`: its JSON and HTML, then the retention of
/// the reports under `root` and its index.
pub fn write_files(files: &dyn RunFiles, root: &Path, rendered: &ReportFiles) -> Result<Written> {
    let period: Period = rendered.period.parse().map_err(anyhow::Error::msg)?;
    // A label not of the period's form (one naming another directory) is
    // not written anywhere.
    anyhow::ensure!(
        report::label_days(period, &rendered.label).is_some(),
        "the report's label {:?} is not a {} label",
        rendered.label,
        period.as_str()
    );
    let file = ReportFile {
        period,
        label: rendered.label.clone(),
        partial: rendered.partial,
    };
    let dir = root.join(file.dir());
    files
        .create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let json_path = root.join(file.path("json"));
    write_atomic(files, &json_path, rendered.json.as_bytes())?;
    let html_path = root.join(file.path("html"));
    write_atomic(files, &html_path, rendered.html.as_bytes())?;
    let offset_ms = rendered.utc_offset_secs * 1000;
    let removed = prune(files, root, rendered.now * 1000, offset_ms, rendered.keep)?;
    let index = write_index(files, root, rendered.now)?;
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
    queue: &(impl QueueRecords + ?Sized),
    ledger: &(impl LedgerStore + ?Sized),
    files: &dyn RunFiles,
    setup: &ReportSetup,
    now: i64,
    supervisor: &LeaseToken,
) -> Result<Vec<(Written, Report)>> {
    let written = queue.reports_written()?;
    let offset_ms = setup.host.utc_offset_secs * 1000;
    let mut done = Vec::new();
    let mut drawn: Option<DiagramSection> = None;
    let mut read: Option<LedgerWindow> = None;
    // A day the retention would remove at once (`keep_daily_days` under
    // the backfill) is not written.
    let owed: Vec<_> = report::due(now * 1000, offset_ms, &written)
        .into_iter()
        .filter(|(period, label, _)| {
            let file = ReportFile {
                period: *period,
                label: label.clone(),
                partial: false,
            };
            !report::expired(&file, now * 1000, offset_ms, setup.keep, &HashSet::new())
        })
        .collect();
    let queries: Vec<KpiQuery> = owed
        .iter()
        .map(|(period, _, at)| report_query(*period, Some(Cursor::Time(*at))))
        .collect();
    for (period, label, at) in owed {
        if read.is_none() {
            read = Some(window(ledger, now, setup, queries.clone())?);
        }
        let window = read.as_ref().context("the ledger's window was read")?;
        let diagram = drawn
            .get_or_insert_with(|| diagram(window, &setup.diagram))
            .clone();
        let report = make_with(window, setup, now, period, Some(Cursor::Time(at)), diagram)
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

    /// Reads that give an event id cursor's time by `cursor` and nothing
    /// else.
    struct Cursors(fn(EventId) -> range::Read<Option<i64>>);

    impl RangeReads for Cursors {
        fn recorded_effective(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn retractions_of(&self, _: EventId) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn claims_written(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn previous_claim_with(&self, _: &str, _: EventId) -> range::Read<Option<RunEvent>> {
            unreachable!()
        }
        fn start_announcing(
            &self,
            _: &str,
            _: &str,
            _: EventId,
            _: EventId,
        ) -> range::Read<Option<RunEvent>> {
            unreachable!()
        }
        fn start_before_in_time(&self, _: i64) -> range::Read<Option<RunEvent>> {
            unreachable!()
        }
        fn cursor_time(&self, cursor: EventId) -> range::Read<Option<i64>> {
            (self.0)(cursor)
        }
        fn finishes_written(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn previous_finish(&self, _: &RunEvent) -> range::Read<Option<RunEvent>> {
            unreachable!()
        }
        fn first_of_target(&self, _: &RunEvent) -> range::Read<Option<RunEvent>> {
            unreachable!()
        }
        fn snapshots_between(&self, _: EventId, _: EventId) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn plan_changes_written(&self, _: i64, _: i64) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn marks_after(&self, _: EventId) -> range::Read<Vec<RunEvent>> {
            unreachable!()
        }
        fn run_finishes(&self) -> range::Read<Vec<(TaskId, i64)>> {
            Err(Undecided::Fallback)
        }
        fn comparison_events(&self) -> range::Read<Vec<RunEvent>> {
            Err(Undecided::Fallback)
        }
    }

    /// A window read from the range of the cursors' times taken holds what
    /// its queries need only when it answers each as taken, or later for
    /// `--at`s alone; else it is read again of every row.
    #[test]
    fn a_window_holds_its_queries_only_when_it_answers_the_cursors_as_taken() {
        use crate::domain::ledger::window::bounds::Bound;
        let (a, b) = (EventId::new(7), EventId::new(9));
        let answered = |pairs: &[(EventId, Option<i64>)]| {
            let mut answers = Answers::default();
            for (id, ms) in pairs {
                answers.set(Bound::TimeThrough(*id), *id, *ms);
            }
            answers
        };
        let taken = [(a, 1_000), (b, 2_000)];
        let same = answered(&[(a, Some(1_000)), (b, Some(2_000))]);
        assert!(read_holds(&taken, Some(&same), true));
        assert!(read_holds(&taken, Some(&same), false));
        let later = answered(&[(a, Some(1_500)), (b, Some(2_000))]);
        assert!(read_holds(&taken, Some(&later), true));
        assert!(!read_holds(&taken, Some(&later), false));
        let earlier = answered(&[(a, Some(999)), (b, Some(2_000))]);
        assert!(!read_holds(&taken, Some(&earlier), true));
        let none = answered(&[(a, None), (b, Some(2_000))]);
        assert!(!read_holds(&taken, Some(&none), true));
        let missing = answered(&[(b, Some(2_000))]);
        assert!(!read_holds(&taken, Some(&missing), true));
        assert!(!read_holds(&taken, None, true));
        assert!(read_holds(&[], None, false));
    }

    /// An event id cursor's time found is the plan's; no event at or before
    /// it is none (the plan's `NoEvent`, every row); a snapshot that cannot
    /// say is out of time order (every row).
    #[test]
    fn a_cursor_s_time_the_snapshot_cannot_give_has_no_range() {
        use crate::application::{CursorRecordTime, CursorTimePath, WindowRead, record_time};
        let id = EventId::new(5);
        let found = |millis| {
            WindowRead::Complete(CursorRecordTime {
                millis,
                path: CursorTimePath::Aggregate,
            })
        };
        assert_eq!(cursor_of(id, record_time(found(42))), Ok(Some(42)));
        assert_eq!(cursor_of(id, record_time(found(i64::MIN))), Ok(None));
        for read in [
            WindowRead::Unavailable,
            WindowRead::Exhausted,
            WindowRead::OffTimeForm,
        ] {
            assert_eq!(
                cursor_of(id, record_time(read)),
                Err(Undecided::OutOfTimeOrder(id))
            );
        }
        let config = KpiConfig::default();
        let now = 400 * DAY_MS / 1000;
        let at = report_query(Period::Day, Some(Cursor::Event(id)));
        let none = Cursors(|_| Ok(None));
        assert_eq!(
            range::plan(&at, &config, now, 0, &none, &HashMap::new()),
            Err(Undecided::NoEvent(id))
        );
    }

    /// The read is from the earliest start of the queries' ranges, each as
    /// [`range::plan`] decides it (an event id `--at` by its time), and of
    /// every row when there is no query or any has no range.
    #[test]
    fn the_read_is_from_the_union_of_the_ranges_and_whole_when_one_has_none() {
        let config = KpiConfig::default();
        let now = 400 * DAY_MS / 1000;
        let found = Cursors(|id| Ok(Some(id.as_i64())));
        let start = |query: &KpiQuery| {
            range::plan(query, &config, now, 0, &found, &HashMap::new())
                .unwrap()
                .range
                .from_ms
        };
        let day = report_query(Period::Day, None);
        let week = report_query(Period::Week, None);
        let past = report_query(Period::Day, Some(Cursor::Time(100 * DAY_MS)));
        let at_event = report_query(Period::Day, Some(Cursor::Event(EventId::new(50 * DAY_MS))));
        assert_eq!(
            read_from(std::slice::from_ref(&day), &config, now, 0, &found),
            Some(start(&day))
        );
        assert_eq!(
            read_from(&[day.clone(), week.clone()], &config, now, 0, &found),
            Some(start(&day).min(start(&week)))
        );
        assert_eq!(
            read_from(
                &[day.clone(), past.clone(), at_event.clone()],
                &config,
                now,
                0,
                &found
            ),
            Some(start(&at_event))
        );
        assert!(start(&at_event) < start(&past) && start(&past) < start(&day));
        assert_eq!(read_from(&[], &config, now, 0, &found), None);
        let whole = KpiQuery {
            read_whole_ledger: true,
            ..day.clone()
        };
        let compared = KpiQuery {
            compare: Some(crate::domain::kpi::CompareSpec::At(Cursor::Time(
                300 * DAY_MS,
            ))),
            ..day.clone()
        };
        for undecided in [whole, compared] {
            assert_eq!(
                read_from(&[day.clone(), undecided], &config, now, 0, &found),
                None
            );
        }
        for cursors in [
            Cursors(|_| Ok(None)),
            Cursors(|id| Err(Undecided::OutOfTimeOrder(id))),
            Cursors(|_| Err(Undecided::Fallback)),
        ] {
            assert_eq!(
                read_from(&[day.clone(), at_event.clone()], &config, now, 0, &cursors),
                None
            );
        }
    }

    /// The window of the events `log` (kind, task, payload), each a minute
    /// apart, at their end.
    fn window_of(log: &[(&str, Option<i64>, serde_json::Value)]) -> LedgerWindow {
        use crate::domain::{EventId, RunEvent, TaskId, marks::utc_text};
        let t0 = 1_900_000_000_000;
        let events: Vec<RunEvent> = log
            .iter()
            .enumerate()
            .map(|(i, (kind, task, payload))| RunEvent {
                id: EventId::new(i as i64 + 1),
                task_id: task.map(TaskId::new),
                goal_id: None,
                run_id: None,
                kind: (*kind).into(),
                payload: payload.clone(),
                created_at: utc_text(t0 + i as i64 * 60_000),
                actor: None,
            })
            .collect();
        let now_ms = t0 + log.len() as i64 * 60_000;
        let rows = crate::domain::ledger::build(&events, now_ms, "built");
        crate::domain::ledger::window::select(
            rows,
            &WindowQuery {
                now_ms,
                ..WindowQuery::default()
            },
        )
    }

    /// The diagram is drawn from the tasks' attributes the window folds at
    /// its end, not from the state: a ready high task is near-term, a
    /// completed one is not drawn.
    #[test]
    fn the_diagram_is_drafted_from_the_windows_attributes() {
        let created = |status: &str| {
            json!({"goal_id": null, "status": status, "title": "t", "change": null,
                   "execution_class": "implementation", "priority": "high",
                   "priority_by": "person"})
        };
        let baseline = (
            crate::domain::event_kind::ATTRIBUTES_BASELINE,
            None,
            json!({"tasks": 0, "goals": 0}),
        );
        let ready = window_of(&[
            (
                crate::domain::event_kind::TASK_CREATED,
                Some(7),
                created("ready"),
            ),
            baseline.clone(),
        ]);
        match diagram_draft(&ready) {
            DiagramDraft::Source { tasks, source } => {
                assert_eq!(tasks, [7]);
                assert!(!source.is_empty());
            }
            other => panic!("{other:?}"),
        }
        let completed = window_of(&[
            (
                crate::domain::event_kind::TASK_CREATED,
                Some(7),
                created("completed"),
            ),
            baseline,
        ]);
        assert!(matches!(
            diagram_draft(&completed),
            DiagramDraft::NotDrawn { reason, .. } if reason == "no near-term task to draw"
        ));
    }

    /// Files answered with a label not of their period's form are refused
    /// before anything is written.
    #[test]
    fn report_files_with_a_label_naming_another_directory_are_not_written() {
        let rendered = ReportFiles {
            period: "day".to_owned(),
            label: "../../x".to_owned(),
            partial: false,
            json: "{}".to_owned(),
            html: String::new(),
            utc_offset_secs: 0,
            keep: Keep::default(),
            now: 0,
        };
        let files = crate::application::memory_files::MemoryFiles::default();
        let root = Path::new("/out");
        let error = write_files(&files, root, &rendered).unwrap_err();
        assert!(
            format!("{error:#}").contains("is not a day label"),
            "{error:#}"
        );
        for written in ["daily/../../x.json", "daily/../../x.html", "index.html"] {
            assert!(files.bytes(&root.join(written)).is_err(), "{written}");
        }
    }

    fn params(svg: Option<String>, areas: usize) -> super::super::queue_commands::ReportParams {
        super::super::queue_commands::ReportParams {
            period: "day".to_owned(),
            at: None,
            print: true,
            files: false,
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

    /// The client's request: the inputs are asked once, with no checkout,
    /// and no areas are sent (the service judges them from the ledger); a
    /// bound queue whose main checkout is found is sent with it, one bound
    /// to none or whose main checkout is not found with none. The diagram
    /// is drawn by the client's renderer.
    #[test]
    fn a_report_s_request_reads_no_areas_and_names_the_main_checkout() {
        use super::super::queue_commands::{QueueCommand, ReportInputsParams};
        let answer = |repository: Option<&str>| {
            serde_json::to_value(ReportInputs {
                diagram: DiagramDraft::Source {
                    tasks: vec![1],
                    source: "a -> b".to_owned(),
                },
                repository: repository.map(str::to_owned),
                landed: Vec::new(),
            })
            .unwrap()
        };
        let drawn = DiagramRenderer(Arc::new(|source: &str| Ok(format!("<svg>{source}</svg>"))));
        let prepared = |repository: Option<&str>, found: bool| {
            let mut asked = Vec::new();
            let mut sent = params(None, 1);
            let main_checkout = move |common_dir: &Path| {
                assert_eq!(common_dir, Path::new("/r/.git"));
                anyhow::ensure!(found, "no main checkout");
                Ok(PathBuf::from("/r"))
            };
            prepare_request(
                &mut sent,
                &ReportClient {
                    main_checkout: &main_checkout,
                    diagram: &drawn,
                },
                |command: &QueueCommand| {
                    let QueueCommand::ReportInputs(inputs) = command else {
                        panic!("{command:?}");
                    };
                    asked.push(inputs.clone());
                    Ok(answer(repository))
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
        let (sent, asked) = prepared(Some("/r/.git"), true);
        assert_eq!(asked, std::slice::from_ref(&first));
        assert_eq!(sent.checkout.as_deref(), Some(Path::new("/r")));
        assert_eq!(sent.areas, None);
        assert_eq!(
            sent.diagram.unwrap().svg.as_deref(),
            Some("<svg>a -> b</svg>")
        );
        for (repository, found) in [(None, true), (Some("/r/.git"), false)] {
            let (sent, asked) = prepared(repository, found);
            assert_eq!(
                asked,
                std::slice::from_ref(&first),
                "{repository:?} {found}"
            );
            assert_eq!((sent.checkout, sent.areas), (None, None));
        }
    }

    /// `report_inputs` names no landed commit and reads no event: a
    /// checkout of the bound repository is accepted, another repository's
    /// is refused.
    #[test]
    fn report_inputs_reads_no_event_and_accepts_only_the_bound_checkout() {
        use super::super::queue_commands::ReportInputsParams;
        let files = super::super::memory_files::MemoryFiles::default();
        files.put_dir(Path::new("/r/main"));
        files.put_dir(Path::new("/r/main/.git"));
        files.put_dir(Path::new("/r/other"));
        files.put_dir(Path::new("/r/other/.git"));
        let served = Some(Path::new("/r/main"));
        let sources = ReportInputsSources { files: &files };
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
        for checkout in [None, Some("/r/main")] {
            let inputs = report_inputs(
                &params(checkout),
                bound.clone(),
                served,
                || draft.clone(),
                &sources,
            )
            .unwrap();
            assert_eq!(
                inputs,
                ReportInputs {
                    diagram: draft.clone(),
                    repository: bound.clone(),
                    landed: Vec::new(),
                }
            );
        }
        let error = report_inputs(
            &params(Some("/r/other")),
            bound,
            served,
            || draft.clone(),
            &sources,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("is not the main checkout"), "{error}");
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
