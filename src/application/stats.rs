//! `stats` (ADR-0023 decision 5, ADR-t1662-1 decision 12, ADR-t1662-2
//! D1): what [`crate::domain::stats::stats`] derives the per-run and
//! per-goal times and the thresholds crossed from, all read from the
//! ledger's window ([`crate::domain::ledger::window`]): the rows' records,
//! the tasks' attributes, main's history the supervisor recorded, the
//! judgments of now its passes recorded, and the host's load of the node
//! rows. No EventStore read of every event, no StateStore, run directory,
//! process, cmux or Git: what is not recorded reads as missing, with why.
//! The settings (`[stall]`, `[conflicts]`, `[areas]`) are read beside the
//! window.
//!
//! The queue service's client sends `stats` and `kpi` as they are: no Git
//! runs on either side. A client older than this reads Git and sends what
//! it read ([`StatsGitReads`]), which is taken and not read.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::LedgerStore;
use crate::domain::{
    EventId,
    areas::{AreaMap, run_areas},
    ledger::{
        attributes::Folded,
        window::{
            LedgerWindow, WindowQuery,
            bounds::{Bound, DeclareBounds, Declared},
        },
    },
    stall::StallConfig,
    stats::{
        ConflictConfig, ConflictConfigReport, LiveSnapshot, SlotSnapshot, StallConfigReport, Stats,
        StatsQuery, stats_answering as aggregate, with_areas, with_changes, without_cargo_measures,
        without_rustc,
    },
};

/// What `stats` reads beside the ledger: settings, not observations.
pub struct StatsSources<'a> {
    /// The `[stall]` of `dagq.toml`; `None` when there is no file.
    pub config_file: &'a dyn Fn() -> Result<Option<StallConfig>>,
    /// The `[conflicts]` of `dagq.toml`; `None` when there is no file.
    pub conflicts_file: &'a dyn Fn() -> Result<Option<ConflictConfig>>,
    /// The `[areas]` of `dagq.toml`, which the landed runs' areas are
    /// judged by from main's history the ledger holds (ADR-t980-1); `None`
    /// without it, when no run has areas.
    pub areas: Option<&'a AreaMap>,
    /// The host's offset from UTC in seconds, east positive: when an ask
    /// counts as opened at night.
    pub utc_offset_secs: i64,
    /// Whether the rules that work for the repository's `dagq.toml` count
    /// `integrate`'s checks
    /// ([`crate::domain::command_rules::EffectiveRules::counts_checks`]):
    /// only then are the counts of what `integrate` repeats shown.
    pub counts_checks: bool,
    /// Whether the repository's profiles record the host's Rust toolchain
    /// at a claim
    /// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]):
    /// only then are the runs' `rustc` and the runs per `rustc` shown.
    pub rust_toolchain: bool,
}

/// The `[conflicts]` thresholds and where they came from.
pub fn conflict_config(file: Option<ConflictConfig>) -> ConflictConfigReport {
    match file {
        Some(config) => ConflictConfigReport {
            config,
            source: "file",
        },
        None => ConflictConfigReport::default(),
    }
}

/// The thresholds of the running alerts: the latest supervisor's
/// (`stall_config_loaded`, a queue row's record), else the file's, else
/// the defaults.
pub fn stall_config(
    events: &[crate::domain::RunEvent],
    config_file: &dyn Fn() -> Result<Option<StallConfig>>,
) -> Result<StallConfigReport> {
    Ok(match StallConfig::loaded(events) {
        Some(config) => StallConfigReport {
            config,
            source: "supervisor",
        },
        None => match config_file()? {
            Some(config) => StallConfigReport {
                config,
                source: "file",
            },
            None => StallConfigReport {
                config: StallConfig::default(),
                source: "default",
            },
        },
    })
}

/// The window of the ledger `stats` of `query` reads at `now` (unix
/// seconds): its cursors only, as `stats` filters by `--goal` itself, and
/// the range the store reads ([`read_from_ms`]).
pub fn window_query(query: &StatsQuery, now: i64) -> WindowQuery {
    WindowQuery {
        since: query.since,
        until: query.until,
        now_ms: now_ms(now),
        read_from_ms: read_from_ms(query, now),
        bounds: if query.full || query.since.is_some() {
            Declared::default()
        } else {
            Declared::of(PageBounds(query.clone()))
        },
        ..WindowQuery::default()
    }
}

/// Where the store's read of the ledger for `stats` of `query` at `now`
/// (unix seconds) may start (unix ms, [`WindowQuery::read_from_ms`]): the
/// time of a time `--since`, so that the store reads the rows that can bear
/// on the window from it and no more of the settled history before it than
/// the window names (`--until` bounds no read:
/// [`crate::application::LedgerStore::ledger_window`] says why). `None`
/// reads every row: without `--since`, with an event id as `--since` (only
/// a time once the events are read), with `--goal`, which counts by the
/// goal of every task an event names whatever its time, as `kpi`'s read
/// does ([`crate::domain::kpi::read_from_ms`]), and when the window is cut
/// down before `--since`: a `--since` past a time `--until` or past now, or
/// an event id as `--until`, whose time the read cannot compare before the
/// events are read. The window then starts before `--since`, and a read
/// from it would leave out what that start reads.
pub fn read_from_ms(query: &StatsQuery, now: i64) -> Option<i64> {
    use crate::domain::stats::Cursor;
    let Some(Cursor::Time(since)) = query.since else {
        return None;
    };
    let end = match query.until {
        None => now_ms(now),
        Some(Cursor::Time(until)) => until.min(now_ms(now)),
        Some(Cursor::Event(_)) => return None,
    };
    (query.goal_id.is_none() && since <= end).then_some(since)
}

/// The bound `stats` without `--full` and `--since` takes inside its
/// window: the first time of its page's ids
/// ([`crate::domain::stats::page_range`]), which starts at the first event
/// of the page's first run, a position the window's query cannot know
/// before the runs are read. So it is declared once the rows are read, of
/// the window's events and goals, and the window answers it of the same
/// snapshot ([`crate::domain::ledger::window::bounds`]): one first time of
/// a range, read as a window's first time is, whatever the history
/// around it, where declaring every position a page could start at would
/// read one for each run.
struct PageBounds(StatsQuery);

impl DeclareBounds for PageBounds {
    fn bounds(&self, window: &LedgerWindow) -> Vec<Bound> {
        let goals = match window.attributes() {
            Folded::Attributes(known) => known.task_goals(),
            Folded::BeforeBaseline => std::collections::HashMap::new(),
        };
        crate::domain::stats::page_range(window.events(), &goals, &self.0)
            .map(|(after, upto)| Bound::FirstIn(after, upto))
            .into_iter()
            .collect()
    }
}

/// The end of the window of a read at the unix second `now`, in unix
/// milliseconds: the last of that second, so that an event recorded
/// within it, after the second began, is in the window as `stats` of the
/// events read it.
pub const fn now_ms(now: i64) -> i64 {
    now * 1000 + 999
}

/// Per-run and per-goal times and the thresholds crossed at `now` (unix
/// seconds), from the ledger's window of `query` alone ([`of_window`]).
pub fn stats(
    queue: &(impl LedgerStore + ?Sized),
    now: i64,
    query: &StatsQuery,
    sources: &StatsSources<'_>,
) -> Result<Stats> {
    let window = queue.ledger_window(&window_query(query, now))?;
    of_window(&window, now, query, sources)
}

/// `stats` of `window`: the events its rows own up to its end, the tasks'
/// goals, titles, changes and drafts' origins its attribute records give
/// at its end (none before their baseline), the judgments of now and
/// main's history folded at its end, the areas `[areas]` gives main's
/// recorded paths, and the host's load of its node rows.
pub fn of_window(
    window: &LedgerWindow,
    now: i64,
    query: &StatsQuery,
    sources: &StatsSources<'_>,
) -> Result<Stats> {
    let events = window.events();
    let attributes = match window.attributes() {
        Folded::Attributes(attributes) => Some(attributes),
        Folded::BeforeBaseline => None,
    };
    let goals = attributes
        .as_ref()
        .map(|known| known.task_goals())
        .unwrap_or_default();
    let live = LiveSnapshot {
        runs: Vec::new(),
        config: stall_config(events, sources.config_file)?,
        history: window.main_history(),
        conflicts: conflict_config((sources.conflicts_file)()?),
        draft_origins: attributes
            .as_ref()
            .map(|known| known.draft_origins())
            .unwrap_or_default(),
        utc_offset_secs: sources.utc_offset_secs,
        judged: Some(window.judgments()),
        ledger: Some(window.run_times()),
        earlier_executions: window.earlier().executions.clone(),
        earlier_landings: window.earlier().landings.clone(),
    };
    let mut stats = aggregate(
        window.answers(),
        events,
        &goals,
        now,
        SlotSnapshot::default(),
        query,
        &live,
    );
    if let Some(known) = &attributes {
        let titles = known.task_titles();
        for run in &mut stats.runs {
            run.title = titles.get(&run.task_id).cloned();
        }
        with_changes(&mut stats, &known.task_changes());
    } else {
        with_changes(&mut stats, &std::collections::HashMap::new());
    }
    let areas = sources
        .areas
        .map(|map| run_areas(map, events, &window.main_log().changed));
    with_areas(&mut stats, areas.as_ref());
    if !sources.counts_checks {
        without_cargo_measures(&mut stats);
    }
    if !sources.rust_toolchain {
        without_rustc(&mut stats);
    }
    stats.host = Some(window.host());
    stats.breakdown = Some(breakdown(
        window,
        &stats,
        attributes.as_ref().map(|known| known.task_changes()),
        areas.as_ref(),
    ));
    Ok(stats)
}

/// The breakdown of the runs `stats` lists (its page) by their tags in
/// both measures, overall and per change and area of their task
/// ([`crate::domain::ledger::breakdown::Strata`]), from their ledger rows.
fn breakdown(
    window: &LedgerWindow,
    stats: &Stats,
    changes: Option<
        std::collections::HashMap<crate::domain::TaskId, Option<crate::domain::TaskChange>>,
    >,
    areas: Option<&crate::domain::areas::RunAreas>,
) -> crate::domain::stats::StatsBreakdown {
    use crate::domain::ledger::{
        LedgerKind, LedgerRow,
        breakdown::{AreasOf, Strata},
    };
    let listed: std::collections::HashSet<&str> =
        stats.runs.iter().map(|run| run.run_id.as_str()).collect();
    let rows: Vec<&LedgerRow> = window
        .of(LedgerKind::Run)
        .filter(|row| listed.contains(row.key.as_str()))
        .collect();
    let change_of = |row: &LedgerRow| {
        row.task_id
            .and_then(|task| changes.as_ref()?.get(&task).cloned().flatten())
            .map_or_else(|| "unknown".to_owned(), |change| change.as_str().to_owned())
    };
    let areas_of = |row: &LedgerRow| {
        crate::domain::RunId::new(row.key.clone())
            .ok()
            .and_then(|run| areas?.get(&run).cloned())
            .filter(|areas| !areas.is_empty())
            .unwrap_or_else(|| vec!["unknown".to_owned()])
    };
    let strata = Strata::of(
        &rows,
        change_of,
        areas.is_some().then_some(&areas_of as &AreasOf<'_>),
    );
    crate::domain::stats::StatsBreakdown {
        overall: strata.overall,
        by_change: strata.by_change,
        by_area: strata.by_area,
    }
}

/// What the queue service's client of `stats` and `kpi` was told to read
/// with Git before them: the answer of `stats_inputs`, which a client of
/// this version no longer asks. It names nothing to read, so an older
/// client reads no Git either; its `version` is how far the queue's
/// records reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsInputs {
    /// The main checkout of the repository the queue is bound to, as the
    /// service finds it from the binding with no Git run.
    pub checkout: Option<PathBuf>,
    /// Always empty: no landed commit is judged by a client.
    pub landed: Vec<String>,
    /// Always none: no main history is read by a client.
    pub history_since: Option<i64>,
    /// Always empty.
    pub conflicted: Vec<String>,
    /// The latest event id of the queue, as text.
    pub version: String,
}

/// `stats_inputs` of a queue whose bound main checkout is `checkout`, its
/// records reaching `latest`. Reads nothing.
pub fn stats_inputs(checkout: Option<PathBuf>, latest: EventId) -> StatsInputs {
    StatsInputs {
        checkout,
        version: latest.as_i64().to_string(),
        ..StatsInputs::default()
    }
}

/// What a client older than the ledger's `stats` read with Git and sent
/// with `stats` and `kpi`: taken, so its request is not refused, and not
/// read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsGitReads {
    #[serde(default)]
    pub inputs: String,
    #[serde(default)]
    pub areas: Option<serde_json::Value>,
    #[serde(default)]
    pub history: Option<serde_json::Value>,
}
