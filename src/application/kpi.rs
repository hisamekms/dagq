//! `kpi` (ADR-0051 decision 9): the queue's reads that
//! [`crate::domain::kpi::kpi`] derives the KPIs from. Reads only.
use anyhow::{Result, anyhow};

use super::LedgerStore;
use crate::domain::areas::{AreaMap, run_areas};
use crate::domain::kpi::{HostReader, Kpi, KpiConfig, KpiInput, KpiQuery, kpi as derive};
use crate::domain::ledger::attributes::Folded;
use crate::domain::ledger::window::{LedgerWindow, WindowQuery};

/// Where the host is: the time zone at `now` (seconds east of UTC) and the
/// logical cores; and whether the profiles of the queue's repository
/// record the Rust toolchain
/// ([`crate::domain::toolchain::Toolchains::records_rust_toolchain`]), for
/// the `toolchain` axis.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    pub utc_offset_secs: i64,
    pub cores: Option<usize>,
    pub rust_toolchain: bool,
}

/// The KPIs `query` asks for, at the unix second `now`, judged by
/// `config`, from the ledger's rows alone (ADR-t1662-2 D1): the events the
/// rows own, the tasks' goals, changes and drafts' origins of the
/// attribute records, the landed runs' areas `areas` (`[areas]`) gives
/// main's history the supervisor recorded, the supervisors' lives of
/// their own records, and each window's host load of the node rows.
pub fn kpi(
    queue: &(impl LedgerStore + ?Sized),
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
) -> Result<Kpi> {
    let window = queue.ledger_window(&WindowQuery {
        now_ms: super::stats::now_ms(now),
        ..WindowQuery::default()
    })?;
    of_window(&window, now, host, config, areas, query)
}

/// [`kpi`] of `window`, the whole ledger up to `now`.
pub fn of_window(
    window: &LedgerWindow,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
) -> Result<Kpi> {
    let events = window.events();
    let run_areas = areas.map(|map| run_areas(map, &events, &window.main_log().changed));
    let (goals, changes, draft_origins) = match window.attributes() {
        Folded::Attributes(known) => (
            known.task_goals(),
            known.task_changes(),
            known.draft_origins(),
        ),
        Folded::BeforeBaseline => Default::default(),
    };
    let heartbeats = crate::domain::supervisor_life::heartbeats(&events, now);
    let read = |from: i64, until: i64| window.host_between(from * 1000 - 1000, until * 1000);
    derive(
        &KpiInput {
            events: &events,
            goals: &goals,
            changes: &changes,
            areas: run_areas.as_ref(),
            heartbeats: &heartbeats,
            draft_origins: &draft_origins,
            now,
            utc_offset_secs: host.utc_offset_secs,
            cores: host.cores,
            rust_toolchain: host.rust_toolchain,
            config,
            host: Some(HostReader(&read)),
        },
        query,
    )
    .map_err(|error| anyhow!(error))
}

/// [`improvements`] against `configured`, `[kpi]`'s
/// `max_improvement_proposals` of the bound checkout's `dagq.toml`, or its
/// default for a queue bound to none or a file without it (ADR-0051
/// decision 25).
pub fn improvements_of_setting(
    store: &dyn super::DraftPlannerReads,
    configured: Option<usize>,
) -> Result<serde_json::Value> {
    improvements(
        store,
        configured.unwrap_or(crate::domain::kpi::config::DEFAULT_MAX_IMPROVEMENT_PROPOSALS),
    )
}

/// The improvement proposals running against `limit`, `[kpi]`'s
/// `max_improvement_proposals` (ADR-0051 decision 25), and when they
/// reached it, the findings waiting for a planner because of it (none
/// that is no improvement, [`crate::domain::counts_as_improvement`]: a
/// `ci_failure` or `agent_eval_watch_due` one the supervisor recorded does
/// not wait for it, ADR-t2928-1 and ADR-t2971-1).
pub fn improvements(
    store: &dyn super::DraftPlannerReads,
    limit: usize,
) -> Result<serde_json::Value> {
    let improvements = store.improvements(limit)?;
    let waiting: Vec<serde_json::Value> = if improvements.reached() {
        store
            .planner_findings()?
            .iter()
            .filter(|finding| crate::domain::counts_as_improvement(&finding.kind, &finding.recorded_by))
            .map(|finding| serde_json::json!({"finding_id": finding.id, "reason": "improvement_limit"}))
            .collect()
    } else {
        Vec::new()
    };
    Ok(serde_json::json!({
        "running": improvements.running,
        "limit": improvements.limit,
        "reached": improvements.reached(),
        "waiting": waiting,
    }))
}
