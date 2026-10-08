//! `kpi` (ADR-0051 decision 9): the queue's reads that
//! [`crate::domain::kpi::kpi`] derives the KPIs from. Reads only.
use anyhow::{Result, anyhow};

use super::{Queue, areas::AreaReader};
use crate::domain::kpi::{HostReader, Kpi, KpiConfig, KpiInput, KpiQuery, kpi as derive};

/// Where the host is: the time zone at `now` (seconds east of UTC) and the
/// logical cores; and whether the queue's repository is dagq's source
/// (ADR-t614-1), for the cargo-only `toolchain` axis.
#[derive(Debug, Clone, Copy)]
pub struct Host {
    pub utc_offset_secs: i64,
    pub cores: Option<usize>,
    pub dagq_source: bool,
}

/// The KPIs `query` asks for, at the unix second `now`, judged by `config`,
/// the landed runs' areas read by `areas`, with the host's load of each
/// window when `host_metrics` reads it (task 872; `kpi` and the reports do,
/// the breach check and the observer do not).
pub fn kpi(
    queue: &dyn Queue,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: &AreaReader,
    query: &KpiQuery,
    host_metrics: Option<HostReader<'_>>,
) -> Result<Kpi> {
    let events = queue.all_events()?;
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
            dagq_source: host.dagq_source,
            config,
            host: host_metrics,
        },
        query,
    )
    .map_err(|error| anyhow!(error))
}

/// The improvement proposals running against `limit`, `[kpi]`'s
/// `max_improvement_proposals` (ADR-0051 decision 25), and when they
/// reached it, the findings waiting for a planner because of it.
pub fn improvements(
    store: &dyn super::DraftPlannerStore,
    limit: usize,
) -> Result<serde_json::Value> {
    let improvements = store.improvements(limit)?;
    let waiting: Vec<serde_json::Value> = if improvements.reached() {
        store
            .planner_findings()?
            .iter()
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
