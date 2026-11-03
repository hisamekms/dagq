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
/// their own records, each window's host load of the node rows, and the
/// reverts and CI failures after the landings the window's common read
/// matches to the runs ([`LedgerWindow::post_landing_book`]), and the
/// Executions its rows keep ([`LedgerWindow::executions`]).
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
    let ledger = window.run_times();
    let read = |from: i64, until: i64| window.host_between(from * 1000 - 1000, until * 1000);
    let rows: Vec<&crate::domain::ledger::LedgerRow> =
        window.of(crate::domain::ledger::LedgerKind::Run).collect();
    let post_landing = window.post_landing_book();
    let executions = window.executions();
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
            ledger: Some(&ledger),
            rows: &rows,
            post_landing: Some(&post_landing),
            executions: Some(&executions),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::ledger::{
        build,
        testkit::{Log, T0, ms, run_id},
        window::select,
    };
    use crate::domain::stats::Cursor;

    /// From the ledger's window to the KPIs: a landed run's row gives its
    /// time, and the window's common read the revert of its commit.
    #[test]
    fn the_window_gives_the_time_and_the_quality_after_landing() {
        let mut log = Log::default();
        log.landed(1, &run_id(1), 0);
        for event in &mut log.events {
            if event.kind.as_str() == "run_integrated" {
                event.payload["result_commit"] = json!("c1");
            }
        }
        log.add(
            None,
            None,
            "main_commits_recorded",
            json!({"after": null, "commits": [
                {"sha": "c1", "at": T0 + 900, "reverts": null},
                {"sha": "r1", "at": T0 + 1500, "reverts": "c1"},
            ]}),
            1600,
        )
        .add(
            None,
            None,
            "main_observed",
            json!({"head": "r1", "at": T0 + 1600, "since": T0 - 1, "reverts": true}),
            1600,
        );
        let now_ms = ms(2000);
        let window = select(
            build(&log.events, now_ms, "2030-01-01T00:00:00.000Z"),
            &WindowQuery {
                until: Some(Cursor::Time(now_ms)),
                now_ms,
                ..WindowQuery::default()
            },
        );
        let kpi = of_window(
            &window,
            now_ms / 1000,
            Host {
                utc_offset_secs: 0,
                cores: None,
                rust_toolchain: false,
            },
            &KpiConfig::default(),
            None,
            &KpiQuery {
                since: Some(Cursor::Time(ms(-1))),
                until: Some(Cursor::Time(now_ms)),
                ..KpiQuery::default()
            },
        )
        .unwrap();
        let kpis = &kpi.periods[0].window.kpis;
        assert_eq!(kpis["post_landing.revert_rate"]["all"].value, Some(1.0));
        assert_eq!(kpis["post_landing.reverts"]["all"].value, Some(1.0));
        assert_eq!(kpis["run_duration"]["all"].n, 1);
        assert_eq!(kpis["run_duration_without_human"]["all"].n, 1);
    }

    /// From the ledger's window to the provider pair's set: a treatment
    /// run claimed on Codex whose review ran on Claude as its group planned
    /// is in its group as assigned and as it ran, with its worker's tokens
    /// from its Codex turn's records and its review's cost as recorded.
    #[test]
    fn the_window_gives_the_pair_groups_tokens_and_cost() {
        let mut log = Log::default();
        let run = run_id(1);
        log.landed(1, &run, 0);
        for event in &mut log.events {
            if event.kind.as_str() == "run_claimed" {
                let pair = json!({"pair_group": "treatment", "pair_trial": "provider-pair",
                    "pair_worker": "codex", "pair_review": "claude"});
                event.payload = json!({"provider": "codex", "requested_provider": "codex",
                    "attributes": pair});
            }
        }
        let tokens = |output: i64, usd: Option<f64>| {
            json!({"input": 0, "output": output, "cache_read": 0, "cache_creation": 0,
                   "cost_usd": usd})
        };
        log.add(Some(1), Some(&run), "turn_started", json!({"turn": 1}), 20)
            .add(
                Some(1),
                Some(&run),
                "turn_finished",
                json!({"turn": 1, "provider": "codex", "tokens": tokens(1_000_000, None),
                       "tokens_source": "token_usage_record", "tokens_by_model": []}),
                300,
            )
            .add(
                Some(1),
                Some(&run),
                "review_job_started",
                json!({"attempt": 1, "agent": null, "launch": {"provider": "claude"}}),
                610,
            )
            .add(
                Some(1),
                Some(&run),
                "review_job_finished",
                json!({"attempt": 1, "agent": null, "provider": "claude",
                       "tokens": tokens(100, Some(0.25)), "tokens_source": "model_usage",
                       "tokens_by_model": []}),
                650,
            );
        log.events.sort_by_key(|event| event.created_at.clone());
        for (index, event) in log.events.iter_mut().enumerate() {
            event.id = crate::domain::EventId::new(i64::try_from(index).unwrap() + 1);
        }
        let now_ms = ms(2000);
        let window = select(
            build(&log.events, now_ms, "2030-01-01T00:00:00.000Z"),
            &WindowQuery {
                until: Some(Cursor::Time(now_ms)),
                now_ms,
                ..WindowQuery::default()
            },
        );
        let config = KpiConfig {
            codex_prices: Some(crate::domain::agent_eval::round::ProviderCost {
                input_usd_per_mtok: Some(2.0),
                output_usd_per_mtok: Some(10.0),
                ..Default::default()
            }),
            ..KpiConfig::default()
        };
        let kpi = of_window(
            &window,
            now_ms / 1000,
            Host {
                utc_offset_secs: 0,
                cores: None,
                rust_toolchain: false,
            },
            &config,
            None,
            &KpiQuery {
                since: Some(Cursor::Time(ms(-1))),
                until: Some(Cursor::Time(now_ms)),
                by: vec!["pair_group".parse().unwrap()],
                ..KpiQuery::default()
            },
        )
        .unwrap();
        let kpis = &kpi.periods[0].window.kpis;
        use crate::domain::kpi::pair;
        for stratum in ["pair_group=treatment", "ran:pair_group=treatment"] {
            assert_eq!(kpis["landings"][stratum].value, Some(1.0), "{stratum}");
            assert_eq!(kpis[pair::WORKER_TOKENS][stratum].value, Some(1_000_000.0));
            assert_eq!(kpis[pair::WORKER_COST][stratum].value, Some(10.0));
            assert_eq!(kpis[pair::REVIEW_COST][stratum].value, Some(0.25));
            assert_eq!(kpis[pair::LIMIT_WAITS][stratum].value, Some(0.0));
        }
        assert_eq!(
            kpis[pair::SWITCHED_REVIEW]["pair_group=treatment"].value,
            Some(0.0)
        );
    }
}
