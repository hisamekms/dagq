//! `kpi` (ADR-0051 decision 9): the queue's reads that
//! [`crate::domain::kpi::kpi`] derives the KPIs from. Reads only.
use anyhow::{Result, anyhow};

use super::{EventStore, PlanningRecords, SupervisorReads, areas::AreaReader};
use crate::domain::kpi::{HostReader, Kpi, KpiConfig, KpiInput, KpiQuery, kpi as derive};

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

/// The KPIs `query` asks for, at the unix second `now`, judged by `config`,
/// the landed runs' areas read by `areas`, with the host's load of each
/// window when `host_metrics` reads it (`kpi`, the reports, the breach
/// check and the observer's input all pass the host's records). The KPIs come from the
/// [`EventStore`]; the [`PlanningRecords`] and [`SupervisorReads`] reads
/// are what they still join from the StateStore (the tasks' goals and
/// changes, the drafts' origins and the supervisors' heartbeats) until the
/// ledger replaces them (ADR-t1662-2 D1).
pub fn kpi(
    queue: &(impl EventStore + PlanningRecords + SupervisorReads + ?Sized),
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: &AreaReader,
    query: &KpiQuery,
    host_metrics: Option<HostReader<'_>>,
) -> Result<Kpi> {
    kpi_checked(queue, now, host, config, areas, query, host_metrics, None)
}

/// [`kpi`] that first has `fresh` refuse the events it reads when they are
/// not the ones `areas` were read for (the queue service's
/// [`super::stats::ServedReads::check`]); `None` checks none.
#[allow(clippy::too_many_arguments)]
pub fn kpi_checked(
    queue: &(impl EventStore + PlanningRecords + SupervisorReads + ?Sized),
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: &AreaReader,
    query: &KpiQuery,
    host_metrics: Option<HostReader<'_>>,
    fresh: Option<&super::stats::EventsCheck<'_>>,
) -> Result<Kpi> {
    let events = queue.all_events()?;
    if let Some(fresh) = fresh {
        fresh(&events)?;
    }
    let run_areas = areas.run_areas(&events);
    let goals = queue.task_goals()?;
    let changes = queue.task_changes()?;
    let draft_origins = queue.draft_origins()?;
    let heartbeats = queue
        .supervisors()?
        .into_iter()
        .map(|registration| (registration.token.into_string(), registration.heartbeat_at))
        .collect();
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
            host: host_metrics,
        },
        query,
    )
    .map_err(|error| anyhow!(error))
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
