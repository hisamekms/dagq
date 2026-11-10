//! `kpi` (ADR-0051 decision 9): the queue's reads that
//! [`crate::domain::kpi::kpi`] derives the KPIs from. Reads only.
use anyhow::{Result, anyhow};

use super::LedgerStore;
use crate::domain::areas::{AreaMap, run_areas};
use crate::domain::kpi::{
    HostReader, Kpi, KpiConfig, KpiInput, KpiQuery, bounds as kpi_bounds, kpi as derive,
    read_from_ms,
};
use crate::domain::ledger::attributes::Folded;
use crate::domain::ledger::window::{
    LedgerWindow, WindowQuery,
    bounds::{Bound, DeclareBounds, Declared},
};

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
/// Executions its rows keep ([`LedgerWindow::executions`]). The periods `query`
/// asks for say which rows the read may leave out
/// ([`crate::domain::kpi::read_from_ms`]); the answer is the same.
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
        read_from_ms: read_from_ms(query, config, now, host.utc_offset_secs),
        bounds: declared(vec![query.clone()], now, host, config, areas),
        ..WindowQuery::default()
    })?;
    of_window(&window, now, host, config, areas, query)
}

/// The bounds [`of_window`] of each of `queries` takes inside the window
/// ([`crate::domain::kpi::bounds`]), for the read of its window to answer
/// ([`WindowQuery::bounds`]): each reader of the KPIs (`kpi`, `report`,
/// the check of the targets, the observer's input) declares those of the
/// queries it makes of the window it reads.
pub fn declared(
    queries: Vec<KpiQuery>,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
) -> Declared {
    Declared::of(KpiBounds {
        queries,
        now,
        host,
        config: config.clone(),
        areas: areas.cloned(),
    })
}

/// [`declared`]'s declaration.
struct KpiBounds {
    queries: Vec<KpiQuery>,
    now: i64,
    host: Host,
    config: KpiConfig,
    areas: Option<AreaMap>,
}

impl DeclareBounds for KpiBounds {
    fn bounds(&self, window: &LedgerWindow) -> Vec<Bound> {
        let mut bounds = Vec::new();
        for query in &self.queries {
            // Only a comparison reads what the input holds past the events
            // (its marks and finishes); the periods read the events alone.
            if query.compare.is_some() {
                with_input(
                    window,
                    self.now,
                    self.host,
                    &self.config,
                    self.areas.as_ref(),
                    |input| bounds.extend(kpi_bounds(input, query)),
                );
            } else {
                bounds.extend(kpi_bounds(
                    &KpiInput {
                        events: window.events(),
                        goals: &std::collections::HashMap::new(),
                        changes: &std::collections::HashMap::new(),
                        areas: None,
                        heartbeats: &std::collections::HashMap::new(),
                        draft_origins: &std::collections::HashMap::new(),
                        now: self.now,
                        utc_offset_secs: self.host.utc_offset_secs,
                        cores: self.host.cores,
                        rust_toolchain: self.host.rust_toolchain,
                        config: &self.config,
                        host: None,
                        ledger: None,
                        rows: &[],
                        post_landing: None,
                        executions: None,
                        earlier: None,
                        answers: window.answers(),
                    },
                    query,
                ));
            }
        }
        bounds
    }
}

/// [`kpi`] of `window`, the ledger up to `now` (all of it, or read from the
/// time of [`read_from_ms`] for `query`). Its bounds inside the window are
/// the window's answers when its query declared them ([`declared`]).
pub fn of_window(
    window: &LedgerWindow,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    query: &KpiQuery,
) -> Result<Kpi> {
    with_input(window, now, host, config, areas, |input| {
        derive(input, query)
    })
    .map_err(|error| anyhow!(error))
}

/// `f` of the input of the KPIs of `window`.
fn with_input<T>(
    window: &LedgerWindow,
    now: i64,
    host: Host,
    config: &KpiConfig,
    areas: Option<&AreaMap>,
    f: impl FnOnce(&KpiInput<'_>) -> T,
) -> T {
    let events = window.events();
    let run_areas = areas.map(|map| run_areas(map, events, &window.main_log().changed));
    let (goals, changes, draft_origins) = match window.attributes() {
        Folded::Attributes(known) => (
            known.task_goals(),
            known.task_changes(),
            known.draft_origins(),
        ),
        Folded::BeforeBaseline => Default::default(),
    };
    let heartbeats = crate::domain::supervisor_life::heartbeats(events, now);
    let ledger = window.run_times();
    let read = |from: i64, until: i64| window.host_between(from * 1000 - 1000, until * 1000);
    let rows: Vec<&crate::domain::ledger::LedgerRow> =
        window.of(crate::domain::ledger::LedgerKind::Run).collect();
    let post_landing = window.post_landing_book();
    let executions = window.executions();
    f(&KpiInput {
        events,
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
        earlier: Some(window.earlier()),
        answers: window.answers(),
    })
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

    /// `kpi` of a window whose end drops events answers as of a queue that
    /// holds only the events up to that end: what is past it reaches no
    /// answer, whichever way the window lends what is within it.
    #[test]
    fn the_window_answers_of_the_events_up_to_its_end() {
        let mut upto = Log::default();
        upto.landed(1, &run_id(1), 0);
        let until = ms(1500);
        let mut more = upto.events.clone();
        let mut past = Log::default();
        past.landed(2, &run_id(2), 2000);
        for mut event in past.events {
            event.id = crate::domain::EventId::new(i64::try_from(more.len()).unwrap() + 1);
            more.push(event);
        }
        // What the store's rows hold at the time of the read, read `until`.
        let now_ms = ms(4000);
        let answer = |events: &[crate::domain::RunEvent]| {
            let window = select(
                build(events, now_ms, "2030-01-01T00:00:00.000Z"),
                &WindowQuery {
                    until: Some(Cursor::Time(until)),
                    now_ms,
                    ..WindowQuery::default()
                },
            );
            assert_eq!(window.events(), upto.events);
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
                    until: Some(Cursor::Time(until)),
                    ..KpiQuery::default()
                },
            )
            .unwrap();
            let landed = kpi.periods[0].window.kpis["run_duration"]["all"].n;
            (landed, serde_json::to_value(kpi).unwrap())
        };
        let within = answer(&upto.events);
        assert_eq!(answer(&more), within);
        // The one landing within the end is what it answers of.
        assert_eq!(within.0, 1);
    }

    /// `kpi` of a window folds its events more than once and holds no copy
    /// of their payloads for that.
    #[test]
    fn the_window_gives_the_kpis_with_no_copy_of_the_payloads() {
        const PAD: usize = 16 * 1024;
        let mut log = Log::default();
        log.landed(1, &run_id(1), 0);
        for at in 0..150 {
            log.add(
                None,
                None,
                "main_observed",
                json!({"head": "c", "at": T0 + at, "since": T0 - 1, "pad": "x".repeat(PAD)}),
                at,
            );
        }
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
        let query = KpiQuery {
            since: Some(Cursor::Time(ms(-1))),
            until: Some(Cursor::Time(now_ms)),
            ..KpiQuery::default()
        };
        let peak = crate::domain::ledger::allocation_count::peak_of(|| {
            of_window(
                &window,
                now_ms / 1000,
                Host {
                    utc_offset_secs: 0,
                    cores: None,
                    rust_toolchain: false,
                },
                &KpiConfig::default(),
                None,
                &query,
            )
            .unwrap()
        });
        let payloads = 150 * PAD;
        assert!(peak < payloads, "{peak} of {payloads}");
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
        for stratum in ["pair_group=treatment", "assigned:pair_group=treatment"] {
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

    /// A read from a time answers as the read of every row, a day's and a
    /// week's, with strata and without, of what looks back past the time:
    /// the rows left out alone hold the first Execution, the claim before a
    /// change of build, a retried task's first run and what a plan review
    /// predicted.
    #[test]
    fn a_read_from_a_time_answers_as_a_read_of_every_row() {
        use crate::domain::kpi::{Period, read_from_ms};
        use crate::domain::ledger::LedgerRow;
        use crate::domain::ledger::testkit::{
            DAY, READ_AT_DAY, rows_read_from_a_clock_turned_back, rows_read_from_a_time,
        };
        use crate::domain::ledger::window::select_from_time;
        let now = T0 + READ_AT_DAY * DAY;
        let host = Host {
            utc_offset_secs: 0,
            cores: Some(4),
            rust_toolchain: false,
        };
        let config = KpiConfig::default();
        let answer_of = |rows: &[LedgerRow], query: &KpiQuery| {
            let window = select_from_time(
                rows.to_vec(),
                &WindowQuery {
                    now_ms: crate::application::stats::now_ms(now),
                    read_from_ms: read_from_ms(query, &config, now, host.utc_offset_secs),
                    ..WindowQuery::default()
                },
            );
            serde_json::to_value(of_window(&window, now, host, &config, None, query).unwrap())
                .unwrap()
        };
        let rows = rows_read_from_a_time();
        let turned_back = rows_read_from_a_clock_turned_back();
        let answer = |query: &KpiQuery| answer_of(&rows, query);
        let day_two = KpiQuery {
            period: Period::Day,
            last: 2,
            ..KpiQuery::default()
        };
        let questions = [
            day_two.clone(),
            KpiQuery {
                period: Period::Week,
                last: 2,
                ..KpiQuery::default()
            },
            KpiQuery {
                by: vec!["nature".parse().unwrap(), "provider".parse().unwrap()],
                changes: vec!["unknown".to_owned()],
                ..day_two.clone()
            },
            KpiQuery {
                period: Period::Week,
                last: 2,
                by: vec!["nature".parse().unwrap()],
                changes: vec!["unknown".to_owned()],
                ..KpiQuery::default()
            },
            KpiQuery {
                at: Some(Cursor::Time((now - 2 * DAY) * 1000)),
                ..day_two.clone()
            },
        ];
        for query in questions {
            assert!(read_from_ms(&query, &config, now, 0).is_some(), "{query:?}");
            let whole = answer(&KpiQuery {
                read_whole_ledger: true,
                ..query.clone()
            });
            assert_eq!(answer(&query), whole, "{query:?}");
            let whole = answer_of(
                &turned_back,
                &KpiQuery {
                    read_whole_ledger: true,
                    ..query.clone()
                },
            );
            assert_eq!(answer_of(&turned_back, &query), whole, "{query:?}");
        }
        // What only the times of the rows of long ago put in the periods is
        // counted, and what only the ids put out of them is not: the landing
        // recorded before the first event id of its period, and the landing
        // whose attempt starts at the end of one of long ago.
        let yesterday = answer_of(&turned_back, &day_two)["periods"][0].clone();
        let by_actor = &yesterday["details"]["tokens"]["by_actor"];
        assert_eq!(by_actor["worker"]["input"], 11, "{by_actor}");
        assert_eq!(by_actor["inbox"]["input"], 11, "{by_actor}");
        let landings = |answer: &serde_json::Value| answer["kpis"]["landings"]["all"]["n"].as_u64();
        assert_eq!(
            landings(&yesterday),
            landings(&answer(&day_two)["periods"][0])
        );
        let answered = answer(&day_two);
        let tokens = &answered["periods"][1]["details"]["tokens"];
        assert_eq!(tokens["coverage"], json!("full"), "{tokens}");
        assert_eq!(
            tokens["recorded_from"],
            json!(crate::domain::marks::utc_text(ms(2 * DAY + 20)))
        );
        // Three completed, one of them retried across the time.
        let first_pass = &answered["periods"][0]["kpis"]["first_pass_rate"]["all"];
        assert_eq!(first_pass["n"], 3, "{first_pass}");
        assert!(first_pass["value"].as_f64().unwrap() < 1.0, "{first_pass}");
        let marks: Vec<&str> = answered["periods"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|period| period["marks"].as_array().unwrap())
            .filter_map(|mark| mark["label"].as_str())
            .collect();
        assert!(
            marks.iter().any(|label| label.contains("v1 → v2")),
            "{marks:?}"
        );
        let by_nature = answer(&KpiQuery {
            by: vec!["nature".parse().unwrap()],
            ..day_two
        });
        let landings = &by_nature["periods"][0]["kpis"]["landings"];
        assert!(landings.get("nature=mechanical").is_some(), "{landings}");
    }
}
