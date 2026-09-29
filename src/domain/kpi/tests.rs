use std::collections::HashMap;

use serde_json::{Value, json};

use super::config::{JudgedPeriod, KpiSettings, judge};
use super::*;
use crate::domain::{EventId, RunId};

/// 2026-09-21T00:00:00+09:00, a Monday, in unix seconds.
const MONDAY: i64 = 1_789_916_400;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const JST: i64 = 9 * HOUR;

/// The events of a queue, built in time order.
#[derive(Default)]
struct Queue {
    events: Vec<RunEvent>,
    kinds: HashMap<TaskId, Option<TaskKind>>,
    changes: HashMap<TaskId, Option<TaskChange>>,
    goals: HashMap<TaskId, Option<GoalId>>,
    /// The registered supervisors' last heartbeats.
    heartbeats: HashMap<String, i64>,
    draft_origins: HashMap<TaskId, DraftOrigin>,
    /// The landed runs' areas; `None` without `[areas]`.
    areas: Option<crate::domain::areas::RunAreas>,
}

/// How one run went.
#[derive(Clone)]
struct Run {
    task: i64,
    kind: Option<TaskKind>,
    /// The change its task declares (ADR-t980-1).
    change: Option<TaskChange>,
    /// Unix seconds of the claim.
    claimed: i64,
    work: i64,
    parallel: i64,
    slots: i64,
    load: f64,
    build: &'static str,
    /// The worker's provider; a Codex run is headless, with Codex 0.46.0.
    provider: &'static str,
    /// The provider it moved to in the middle (`provider_switched`), if any.
    switched_to: Option<&'static str>,
    /// The worker session the claim recorded (model, effort, trial group);
    /// none for a claim before ADR-0079.
    session: Option<(&'static str, &'static str, Option<&'static str>)>,
    /// The `nature` of the task's weight prediction before the claim.
    nature: Option<&'static str>,
    revise: bool,
    failed: bool,
}

impl Run {
    fn new(task: i64, kind: Option<TaskKind>, claimed: i64, work: i64) -> Self {
        Self {
            task,
            kind,
            change: None,
            claimed,
            work,
            parallel: 3,
            slots: 1,
            load: 2.0,
            build: "b1",
            provider: "claude",
            switched_to: None,
            session: None,
            nature: None,
            revise: false,
            failed: false,
        }
    }
}

impl Queue {
    fn push(
        &mut self,
        task: Option<i64>,
        run: Option<&str>,
        kind: &str,
        payload: Value,
        secs: i64,
    ) {
        self.events.push(RunEvent {
            id: EventId::new(self.events.len() as i64 + 1),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: marks::utc_text(secs * 1000),
            actor: None,
        });
    }

    fn queue_event(&mut self, kind: &str, payload: Value, secs: i64) {
        self.push(None, None, kind, payload, secs);
    }

    /// A run that lands (or fails) `work` seconds after its claim, the task
    /// ready an hour before; returns the landing time.
    fn run(&mut self, run: &Run) -> i64 {
        let id = format!("{:08x}-0000-4000-8000-{:012x}", run.task, run.claimed);
        let (task, id) = (Some(run.task), Some(id.as_str()));
        self.kinds.insert(TaskId::new(run.task), run.kind.clone());
        self.changes
            .insert(TaskId::new(run.task), run.change.clone());
        self.goals.insert(TaskId::new(run.task), None);
        self.push(
            task,
            None,
            "task_status_changed",
            json!({"from": "submitted", "to": "ready"}),
            run.claimed - HOUR,
        );
        if let Some(nature) = run.nature {
            self.push(
                task,
                None,
                "task_weight_predicted",
                json!({"prediction": {"nature": nature, "expected_output_tokens": 1000}}),
                run.claimed - 2 * HOUR,
            );
        }
        self.push(
            task,
            id,
            "run_claimed",
            {
                let mut claimed = json!({"parallel": run.parallel, "slots": run.slots, "load_avg": run.load,
                                         "dagq_version": run.build, "provider": run.provider,
                                         "worker_mode": "interactive"});
                if run.provider == "codex" {
                    claimed["worker_mode"] = json!("headless");
                    claimed["codex_version"] = json!("0.46.0");
                }
                if let Some((model, effort, group)) = run.session {
                    claimed["model"] = json!(model);
                    claimed["effort"] = json!(effort);
                    claimed["group"] = json!(group);
                }
                claimed
            },
            run.claimed,
        );
        self.push(task, id, "agent_started", json!({}), run.claimed + 10);
        self.push(
            task,
            id,
            "first_commit_observed",
            json!({}),
            run.claimed + 60,
        );
        if let Some(to) = run.switched_to {
            self.push(
                task,
                id,
                "provider_switched",
                json!({"from": run.provider, "to": to, "reason": "usage_limit", "phase": "nudge"}),
                run.claimed + 70,
            );
        }
        let receipt = run.claimed + run.work;
        if run.failed {
            self.push(task, id, "run_failed", json!({"status": "failed"}), receipt);
            return receipt;
        }
        self.push(task, id, "receipt_observed", json!({}), receipt);
        self.push(
            task,
            id,
            "validation_finished",
            json!({"status": "awaiting_integration"}),
            receipt + 20,
        );
        if run.revise {
            self.push(task, id, "revise_requested", json!({}), receipt + 30);
        }
        self.push(task, id, "integration_started", json!({}), receipt + 40);
        self.push(task, id, "integration_rebased", json!({}), receipt + 50);
        self.push(
            task,
            id,
            "verification_command",
            json!({"phase": "integration", "attempt": 1, "index": 1, "exit_code": 0}),
            receipt + 60,
        );
        self.push(
            task,
            id,
            "run_integrated",
            json!({"status": "integrated"}),
            receipt + 100,
        );
        receipt + 100
    }

    /// The events in time order, renumbered, as a queue records them; a
    /// retraction keeps naming its mark.
    fn sort(&mut self) {
        self.events
            .sort_by_key(|event| timestamp_millis(&event.created_at));
        let renumbered: HashMap<i64, i64> = self
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.id.as_i64(), index as i64 + 1))
            .collect();
        for event in &mut self.events {
            event.id = EventId::new(renumbered[&event.id.as_i64()]);
            if let Some(mark) = event.payload.get("mark").and_then(Value::as_i64) {
                event.payload["mark"] = json!(renumbered[&mark]);
            }
        }
    }

    /// The id of the mark labeled `label`, once sorted.
    fn mark_id(&mut self, label: &str) -> EventId {
        self.sort();
        self.events
            .iter()
            .find(|event| event.payload["label"] == label)
            .unwrap()
            .id
    }

    fn kpi(&mut self, now: i64, config: &KpiConfig, query: &KpiQuery) -> Kpi {
        self.kpi_with_host(now, config, query, None)
    }

    fn kpi_with_host(
        &mut self,
        now: i64,
        config: &KpiConfig,
        query: &KpiQuery,
        host: Option<HostReader<'_>>,
    ) -> Kpi {
        self.sort();
        kpi(
            &KpiInput {
                events: &self.events,
                goals: &self.goals,
                kinds: &self.kinds,
                changes: &self.changes,
                areas: self.areas.as_ref(),
                heartbeats: &self.heartbeats,
                draft_origins: &self.draft_origins,
                now,
                utc_offset_secs: JST,
                cores: Some(4),
                config,
                host,
            },
            query,
        )
        .unwrap()
    }
}

fn measure<'a>(period: &'a PeriodKpis, name: &str, stratum: &str) -> &'a Measure {
    &period.window.kpis[name][stratum]
}

/// Days start at local midnight and weeks on Monday; the labels are the
/// local date and the ISO week, across a year's end too.
#[test]
fn periods_start_at_local_midnight_and_monday() {
    let offset = JST * 1000;
    let noon = (MONDAY + 2 * DAY + 12 * HOUR) * 1000;
    let day = Period::Day.start(noon, offset);
    assert_eq!(day, (MONDAY + 2 * DAY) * 1000);
    assert_eq!(Period::Day.label(day, offset), "2026-09-23");
    // 08:59 in Tokyo is still the day before in UTC, but the local day's.
    let early = (MONDAY + 2 * DAY + 30 * 60) * 1000;
    assert_eq!(Period::Day.start(early, offset), day);
    let week = Period::Week.start(noon, offset);
    assert_eq!(week, MONDAY * 1000);
    assert_eq!(Period::Week.label(week, offset), "2026-W39");
    // 2027-01-01 is a Friday: its week is 2026's 53rd.
    let new_year = marks_ms("2027-01-01T12:00:00Z");
    assert_eq!(
        Period::Week.label(Period::Week.start(new_year, 0), 0),
        "2026-W53"
    );
    assert_eq!("week".parse::<Period>(), Ok(Period::Week));
    assert!("month".parse::<Period>().is_err());
    assert_eq!("load".parse::<Axis>(), Ok(Axis::Load));
    assert_eq!("provider".parse::<Axis>(), Ok(Axis::Provider));
    assert_eq!("route".parse::<Axis>(), Ok(Axis::Route));
    assert_eq!("codex".parse::<Axis>(), Ok(Axis::Codex));
    assert_eq!("group".parse::<Axis>(), Ok(Axis::Group));
    assert_eq!("nature".parse::<Axis>(), Ok(Axis::Nature));
    assert!("host".parse::<Axis>().unwrap_err().contains("toolchain"));
}

fn marks_ms(text: &str) -> i64 {
    timestamp_millis(text).unwrap()
}

/// Spreads follow `stats`' rules, a rate over nothing is null, and a count
/// is judged whatever its size.
#[test]
fn measures_keep_null_apart_from_zero() {
    let secs = Measure::secs([40, 10, 30, 20]);
    assert_eq!(
        (secs.n, secs.median, secs.p90, secs.min, secs.max),
        (4, Some(25.0), Some(40.0), Some(10.0), Some(40.0))
    );
    assert_eq!(Measure::secs([]).median, None);
    let rate = Measure::ratio(1.0, 3);
    assert_eq!(rate.value, Some(0.333));
    assert_eq!(Measure::ratio(0.0, 0).value, None);
    assert_eq!(
        serde_json::to_value(Measure::ratio(0.0, 0)).unwrap(),
        json!({"n": 0, "value": null})
    );
    let spread = Measure::spread([1.5, 0.5, 2.25]);
    assert_eq!((spread.median, spread.p90), (Some(1.5), Some(2.25)));
    assert!(Measure::count(1).enough(5));
    assert!(!secs.enough(5));
    assert_eq!(secs.primary(), Some(25.0));
    assert_eq!(Measure::count(2).primary(), Some(2.0));
    assert_eq!(direction("landings"), Some(Direction::Higher));
    assert_eq!(direction("phase.work"), Some(Direction::Lower));
    assert_eq!(direction("session_open.inbox"), None);
    assert_eq!(direction("session_open.worker"), Some(Direction::Lower));
    assert_eq!(direction("auto_repairs"), None);
}

/// Each run falls in the day it finished; its KPIs are split by the task's
/// kind (a task without one is `unknown`) and the claim's attributes, and
/// each day sits next to the previous one.
#[test]
fn splits_the_runs_by_kind_and_attributes_per_day() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY;
    let mut runs = Vec::new();
    for (index, kind) in [
        Some("runtime".parse::<TaskKind>().unwrap()),
        Some("runtime".parse::<TaskKind>().unwrap()),
        Some("docs".parse::<TaskKind>().unwrap()),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let index = index as i64;
        let mut run = Run::new(
            10 + index,
            kind,
            tuesday + HOUR * (index + 1),
            600 * (index + 1),
        );
        run.parallel = 2 + index % 2;
        run.load = 1.0 + 4.0 * index as f64;
        run.revise = index == 1;
        if index == 3 {
            run.provider = "codex";
        }
        if index == 2 {
            // Claimed interactive on Claude, moved to Codex in the middle.
            run.switched_to = Some("codex");
        }
        runs.push(run);
    }
    for run in &runs {
        queue.run(run);
    }
    // Monday: one runtime run, and one that failed.
    queue.run(&Run::new(
        1,
        Some("runtime".parse::<TaskKind>().unwrap()),
        MONDAY + HOUR,
        300,
    ));
    let mut failed = Run::new(
        2,
        Some("runtime".parse::<TaskKind>().unwrap()),
        MONDAY + 2 * HOUR,
        300,
    );
    failed.failed = true;
    queue.run(&failed);
    let query = KpiQuery {
        last: 2,
        at: Some(Cursor::Time(tuesday * 1000)),
        by: vec![
            Axis::Parallel,
            Axis::Load,
            Axis::Provider,
            Axis::Route,
            Axis::Codex,
        ],
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let result = queue.kpi(tuesday + DAY + HOUR, &config, &query);
    assert_eq!(result.period, "day");
    let [monday, tuesday] = [&result.periods[0], &result.periods[1]];
    assert_eq!(
        (monday.label.as_str(), tuesday.label.as_str()),
        ("2026-09-21", "2026-09-22")
    );
    assert!(!tuesday.partial);
    assert_eq!(tuesday.window.runs, 4);
    assert_eq!(measure(tuesday, "landings", ALL).value, Some(4.0));
    assert_eq!(
        measure(tuesday, "landings", "kind=runtime").value,
        Some(2.0)
    );
    assert_eq!(measure(tuesday, "landings", "kind=docs").value, Some(1.0));
    assert_eq!(
        measure(tuesday, "landings", "kind=unknown").value,
        Some(1.0)
    );
    let work = measure(tuesday, "phase.work", "kind=runtime");
    assert_eq!(
        (work.n, work.median, work.max),
        (2, Some(900.0), Some(1200.0))
    );
    assert_eq!(measure(tuesday, "phase.startup", ALL).median, Some(50.0));
    // Ready an hour before the claim, landed 120 s after the receipt.
    assert_eq!(
        measure(tuesday, "lead_time", "kind=docs").median,
        Some(3600.0 + 1800.0 + 100.0)
    );
    assert_eq!(
        measure(tuesday, "revise_rate", "kind=runtime").value,
        Some(0.5)
    );
    assert_eq!(measure(tuesday, "first_pass_rate", ALL).value, Some(0.75));
    assert_eq!(
        measure(tuesday, "verification_failed_rate", ALL).value,
        Some(0.0)
    );
    assert_eq!(measure(tuesday, "verification_failed_rate", ALL).n, 4);
    // parallel alternates 2, 3; the load per core (4 cores) 0.25, 1.25, 2.25, 3.25.
    assert_eq!(measure(tuesday, "landings", "parallel=2").value, Some(2.0));
    assert_eq!(measure(tuesday, "landings", "load=low").value, Some(1.0));
    assert_eq!(measure(tuesday, "landings", "load=mid").value, Some(1.0));
    assert_eq!(measure(tuesday, "landings", "load=high").value, Some(2.0));
    assert_eq!(measure(tuesday, "max_load_avg", ALL).value, Some(13.0));
    // The worker's provider and route, and Codex's version (ADR-t813-2
    // decision 7): a run claimed without Codex has none. The provider is
    // the one that did the work in the end, so the run moved from Claude to
    // Codex counts for Codex; its route is still the claim's.
    assert_eq!(
        measure(tuesday, "landings", "provider=claude").value,
        Some(2.0)
    );
    assert_eq!(
        measure(tuesday, "landings", "provider=codex").value,
        Some(2.0)
    );
    assert_eq!(
        measure(tuesday, "landings", "route=interactive").value,
        Some(3.0)
    );
    assert_eq!(
        measure(tuesday, "landings", "route=headless").value,
        Some(1.0)
    );
    assert_eq!(
        measure(tuesday, "landings", "codex=0.46.0").value,
        Some(1.0)
    );
    assert_eq!(
        measure(tuesday, "landings", "codex=unknown").value,
        Some(3.0)
    );
    assert_eq!(measure(monday, "failed_rate", ALL).value, Some(0.5));
    assert_eq!(measure(monday, "landings", ALL).value, Some(1.0));
    // Tuesday against Monday: a count is judged, a small spread is not.
    let landings = &tuesday.comparison["landings"][ALL];
    assert_eq!(
        (landings.previous, landings.delta, landings.ratio),
        (Some(1.0), Some(3.0), Some(4.0))
    );
    assert_eq!(
        (landings.judged, landings.verdict),
        (true, Some("improved"))
    );
    let work = &tuesday.comparison["phase.work"][ALL];
    assert_eq!((work.judged, work.reason), (false, Some("small_sample")));
    assert_eq!(
        tuesday.comparison["landings"]["kind=docs"].reason,
        Some("no_value")
    );
    // The 7 days before Tuesday: six empty days and Monday.
    assert_eq!(landings.baseline_7d, Some(0.0));
    // Listing only the docs strata leaves the others and `all`.
    let docs = queue.kpi(
        tuesday.end_ms() / 1000 + HOUR,
        &config,
        &KpiQuery {
            kinds: vec!["docs".into()],
            ..query.clone()
        },
    );
    let strata: Vec<&String> = docs.periods[1].window.kpis["landings"].keys().collect();
    assert!(strata.contains(&&"kind=docs".to_owned()) && strata.contains(&&ALL.to_owned()));
    assert!(!strata.contains(&&"kind=runtime".to_owned()));
}

impl PeriodKpis {
    fn end_ms(&self) -> i64 {
        timestamp_millis(&self.end).unwrap()
    }
}

/// Today is listed as partial, and no target is judged on it.
#[test]
fn the_period_not_over_yet_is_partial() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            targets: vec![Target {
                kpi: "landings".into(),
                kind: None,
                change: None,
                area: None,
                stat: None,
                min: Some(1.0),
                max: None,
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let result = queue.kpi(MONDAY + 3 * HOUR, &config, &KpiQuery::default());
    let today = result.periods.last().unwrap();
    assert!(today.partial);
    assert_eq!(measure(today, "landings", ALL).value, Some(1.0));
    let target = &result.targets[0];
    assert_eq!(target.periods.last().unwrap().reason, Some("partial"));
    // Shown next to yesterday, but not judged.
    let landings = &today.comparison["landings"][ALL];
    assert_eq!(
        (landings.delta, landings.reason, landings.verdict),
        (Some(1.0), Some("partial"), None)
    );
    // The days before, with no landing, are judged and off target.
    assert_eq!(target.state, "breach");
    assert_eq!(target.periods[DEFAULT_LAST - 2].met, Some(false));
    assert_eq!(result.periods.len(), DEFAULT_LAST);
}

fn window_kpis(value: Option<f64>, n: usize) -> Kpis<Measure> {
    let mut measure = Measure::secs(std::iter::repeat_n(0, n));
    measure.median = value;
    let mut kpis = Kpis::new();
    kpis.entry("phase.work".into())
        .or_default()
        .insert("kind=runtime".into(), measure);
    kpis
}

/// A target breaks after `breach_periods` judged periods off target in a
/// row; a period with too few samples is skipped without breaking the
/// streak; one period off target is only `missed`, and one on target ends
/// the breach.
#[test]
fn a_breach_needs_consecutive_judged_periods_off_target() {
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(3),
            targets: vec![Target {
                kpi: "phase.work".into(),
                kind: Some("runtime".into()),
                change: None,
                area: None,
                stat: Some(Stat::Median),
                min: None,
                max: Some(100.0),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let days: Vec<(String, Kpis<Measure>)> = [
        (Some(50.0), 5),  // ok
        (Some(150.0), 5), // off
        (Some(150.0), 2), // too few: skipped
        (None, 0),        // nothing: skipped
        (Some(160.0), 4), // off
        (Some(170.0), 9), // off: the third in a row
    ]
    .into_iter()
    .enumerate()
    .map(|(day, (value, n))| (format!("d{day}"), window_kpis(value, n)))
    .collect();
    let periods = |upto: usize| -> Vec<JudgedPeriod<'_>> {
        days[..upto]
            .iter()
            .map(|(label, kpis)| JudgedPeriod {
                label,
                kpis,
                partial: false,
                listed: true,
            })
            .collect()
    };
    let breach = &judge(&config, &periods(6), 3)[0];
    assert_eq!(breach.state, "breach");
    assert_eq!(breach.streak, 3);
    assert_eq!(breach.breach_since.as_deref(), Some("d1"));
    assert_eq!(breach.stratum, "kind=runtime");
    assert_eq!(breach.source, "repository");
    let reasons: Vec<Option<&str>> = breach.periods.iter().map(|p| p.reason).collect();
    assert_eq!(
        reasons,
        [
            None,
            None,
            Some("small_sample"),
            Some("no_value"),
            None,
            None
        ]
    );
    assert_eq!(breach.periods[1].met, Some(false));
    assert_eq!(breach.periods[2].met, None);
    let missed = &judge(&config, &periods(2), 3)[0];
    assert_eq!((missed.state, missed.streak), ("missed", 1));
    assert_eq!(judge(&config, &periods(1), 3)[0].state, "ok");
    assert_eq!(judge(&config, &periods(0), 3)[0].state, "not_judged");
    // Two weeks off target are a breach at `breach_weeks` 2.
    assert_eq!(judge(&config, &periods(6)[4..], 2)[0].state, "breach");
    // A judged period on target after the streak ends the breach.
    let mut recovered = days.clone();
    recovered.push(("d6".into(), window_kpis(Some(90.0), 5)));
    let periods: Vec<JudgedPeriod<'_>> = recovered
        .iter()
        .map(|(label, kpis)| JudgedPeriod {
            label,
            kpis,
            partial: false,
            listed: false,
        })
        .collect();
    let report = &judge(&config, &periods, 3)[0];
    assert_eq!((report.state, report.streak), ("ok", 0));
    assert!(report.periods.is_empty());
}

/// The host's settings win over the repository's, but not its
/// `max_improvement_proposals`; a target of the same KPI and stratum is
/// the host's, and no target is built in.
#[test]
fn the_host_settings_win_over_the_repository() {
    let target = |kpi: &str, kind: Option<&str>, max: f64| Target {
        kpi: kpi.into(),
        kind: kind.map(Into::into),
        change: None,
        area: None,
        stat: None,
        min: None,
        max: Some(max),
    };
    let repository = KpiSettings {
        min_samples: Some(7),
        breach_periods: Some(4),
        max_improvement_proposals: Some(1),
        targets: vec![
            target("phase.work", Some("runtime"), 3600.0),
            target("revise_rate", None, 0.3),
        ],
        ..KpiSettings::default()
    };
    let host = KpiSettings {
        min_samples: Some(2),
        max_improvement_proposals: Some(9),
        targets: vec![target("phase.work", Some("runtime"), 1800.0)],
        ..KpiSettings::default()
    };
    let config = KpiConfig::merge(Some(&repository), Some(&host));
    assert_eq!(config.min_samples, 2);
    assert_eq!(config.breach_periods, 4);
    assert_eq!(config.breach_weeks, config::DEFAULT_BREACH_WEEKS);
    assert_eq!(config.max_improvement_proposals, 1);
    assert_eq!(config.sources["min_samples"], "host");
    assert_eq!(config.sources["breach_periods"], "repository");
    assert_eq!(config.sources["breach_weeks"], "default");
    assert_eq!(config.targets.len(), 2);
    assert_eq!(config.targets[0].target.max, Some(1800.0));
    assert_eq!(config.targets[0].source, "host");
    assert_eq!(config.targets[1].source, "repository");
    assert!(KpiConfig::default().targets.is_empty());
    let overlaid = repository.clone().overlay(host);
    assert_eq!(overlaid.min_samples, Some(2));
    assert_eq!(overlaid.targets.len(), 2);
}

fn start(queue: &mut Queue, parallel: i64, secs: i64) {
    queue.queue_event(
        marks::SUPERVISOR_STARTED,
        json!({"supervisor": "s", "parallel": parallel, "dagq_version": "b1"}),
        secs,
    );
}

/// A comparison at a mark: a window on each side, the other marks in and
/// between them listed, marks too close to split taken as one change,
/// and every side and stratum with its `n`, median, p90 and range.
#[test]
fn compares_across_a_mark_with_its_confounders_and_strata() {
    let mut queue = Queue::default();
    // Before: runtime runs of 1000 s at parallel 4.
    for index in 0..6 {
        let mut run = Run::new(
            100 + index,
            Some("runtime".parse::<TaskKind>().unwrap()),
            MONDAY + index * HOUR,
            1000,
        );
        run.parallel = 4;
        queue.run(&run);
    }
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "sccache", "by": "human"}),
        MONDAY + 8 * HOUR,
    );
    // No run between these: taken with the next as one change.
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "parallel 4→3", "by": "human"}),
        MONDAY + 9 * HOUR,
    );
    // After: faster runtime runs, the last on a new build at parallel 3,
    // and a docs run.
    for index in 0..6 {
        let mut run = Run::new(
            200 + index,
            Some("runtime".parse::<TaskKind>().unwrap()),
            MONDAY + (10 + index) * HOUR,
            600,
        );
        run.parallel = 4;
        if index == 5 {
            run.build = "b2";
            run.parallel = 3;
        }
        queue.run(&run);
    }
    let mut docs = Run::new(
        300,
        Some("docs".parse::<TaskKind>().unwrap()),
        MONDAY + 17 * HOUR,
        100,
    );
    docs.build = "b2";
    queue.run(&docs);
    // A later mark inside the window after, and a retracted one that is none.
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "host arm64", "by": "human"}),
        MONDAY + 30 * HOUR,
    );
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "mistake", "by": "human"}),
        MONDAY + 31 * HOUR,
    );
    let mistake = queue.events.last().unwrap().id.as_i64();
    queue.queue_event(
        marks::MARK_RETRACTED,
        json!({"mark": mistake, "by": "human"}),
        MONDAY + 32 * HOUR,
    );
    let query = KpiQuery {
        last: 1,
        compare: Some(CompareSpec::At(Cursor::Event(
            queue.mark_id("parallel 4→3"),
        ))),
        window_days: 2,
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let result = queue.kpi(MONDAY + 5 * DAY, &config, &query);
    let compare = result.compare.unwrap();
    let split = compare.split.as_ref().unwrap();
    assert!(!split.separable);
    let labels: Vec<&str> = split.marks.iter().map(|mark| mark.label.as_str()).collect();
    assert_eq!(labels, ["sccache", "parallel 4→3"]);
    assert_eq!(
        timestamp_millis(&split.start),
        Some((MONDAY + 8 * HOUR) * 1000)
    );
    assert_eq!(
        timestamp_millis(&compare.before.end),
        Some((MONDAY + 8 * HOUR) * 1000)
    );
    assert_eq!(
        timestamp_millis(&compare.after.start),
        Some((MONDAY + 9 * HOUR) * 1000)
    );
    assert_eq!((compare.before.runs, compare.after.runs), (6, 7));
    assert!(!compare.after.partial);
    // The derived marks of the new build and parallel sit in the window
    // after, with the later person's mark; the retracted one is left out.
    // The three are one overlapping change: two runs finished between them.
    let confounders: Vec<(&str, &str)> = compare
        .confounders
        .iter()
        .map(|c| (c.position, c.mark.kind.as_str()))
        .collect();
    assert_eq!(
        confounders,
        [
            ("after", "derived:dagq_version"),
            ("after", "derived:parallel"),
            ("after", "mark_recorded"),
        ]
    );
    assert_eq!(compare.overlapping.len(), 2);
    assert_eq!(compare.overlapping[0].len(), 2);
    assert_eq!(compare.overlapping[1].len(), 3);
    let work = &compare.strata["phase.work"];
    let all = &work[ALL];
    assert_eq!((all.before.n, all.after.n), (6, 7));
    assert_eq!(
        (all.before.median, all.after.median),
        (Some(1000.0), Some(600.0))
    );
    assert_eq!(
        (all.after.min, all.after.max, all.after.p90),
        (Some(100.0), Some(600.0), Some(600.0))
    );
    assert_eq!(
        (all.change.judged, all.change.verdict),
        (true, Some("improved"))
    );
    for stratum in [
        "kind=runtime",
        "parallel=4",
        "parallel=3",
        "load=low",
        "build=b1",
        "build=b2",
    ] {
        assert!(work.contains_key(stratum), "{stratum}");
    }
    assert_eq!(
        (work["parallel=4"].before.n, work["parallel=4"].after.n),
        (6, 5)
    );
    assert!(work["parallel=4"].change.judged);
    // The new build's runtime run and the docs run.
    assert_eq!(
        (work["parallel=3"].before.n, work["parallel=3"].after.n),
        (0, 2)
    );
    assert_eq!(work["parallel=3"].change.reason, Some("no_value"));
    assert_eq!(work["build=b2"].after.n, 2);
    let docs = &work["kind=docs"];
    assert_eq!((docs.before.n, docs.after.n), (0, 1));
    // A derived mark is named by its claim: the new build's change is one
    // with the later person's mark, and none of them is its own confounder.
    let claim = compare.confounders[0].mark.detail["claim_event"]
        .as_i64()
        .unwrap();
    let derived = queue.kpi(
        MONDAY + 5 * DAY,
        &config,
        &KpiQuery {
            compare: Some(CompareSpec::At(Cursor::Event(EventId::new(claim)))),
            ..query.clone()
        },
    );
    let derived = derived.compare.unwrap();
    let split = derived.split.unwrap();
    assert_eq!((split.marks.len(), split.separable), (3, false));
    assert!(derived.confounders.iter().all(|c| c.position == "before"));
    // The summary is the runtime runs' times only.
    let summary = &compare.summary["runtime"];
    assert_eq!(summary["phase.work"].after.median, Some(600.0));
    assert!(!summary.contains_key("landings"));
}

/// Marks join one change while fewer than `min_samples` runs finish
/// between them; three or more in a row are one.
#[test]
fn marks_too_close_are_one_overlapping_change() {
    let mark = |secs: i64, label: &str| Mark {
        id: None,
        kind: "mark_recorded".into(),
        at: marks::utc_text(secs * 1000),
        recorded_at: marks::utc_text(secs * 1000),
        label: label.into(),
        retracted_by: None,
        detail: Value::Null,
    };
    let marks = [
        mark(100, "a"),
        mark(200, "b"),
        mark(300, "c"),
        mark(1000, "d"),
    ];
    let finishes: Vec<i64> = [150, 250, 400, 500, 600].map(|s| s * 1000).to_vec();
    let groups = compare::overlapping_groups(&marks, &finishes, 2);
    let labels: Vec<Vec<&str>> = groups
        .iter()
        .map(|group| group.iter().map(|m| m.label.as_str()).collect())
        .collect();
    assert_eq!(labels, [vec!["a", "b", "c"], vec!["d"]]);
    assert_eq!(compare::overlapping_groups(&marks, &finishes, 1).len(), 4);
}

/// Two explicit windows compare with no split, and a malformed
/// `--compare` is refused.
#[test]
fn compares_two_explicit_windows() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY + DAY, 200));
    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY - HOUR,
        MONDAY + HOUR,
        MONDAY + DAY - HOUR,
        MONDAY + DAY + HOUR
    )
    .parse()
    .unwrap();
    let query = KpiQuery {
        compare: Some(spec),
        last: 1,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 2 * DAY, &KpiConfig::default(), &query);
    let compare = result.compare.unwrap();
    assert!(compare.split.is_none());
    assert_eq!(compare.strata["phase.work"][ALL].after.median, Some(200.0));
    assert!("1..2".parse::<CompareSpec>().is_err());
    assert!("x".parse::<CompareSpec>().is_err());
    assert_eq!(
        "12".parse::<CompareSpec>(),
        Ok(CompareSpec::At(Cursor::Event(EventId::new(12))))
    );
    let backwards: CompareSpec = format!("@{}..@{},@1..@2", MONDAY + HOUR, MONDAY)
        .parse()
        .unwrap();
    let error = kpi(
        &KpiInput {
            events: &queue.events,
            goals: &queue.goals,
            kinds: &queue.kinds,
            changes: &queue.changes,
            areas: None,
            heartbeats: &queue.heartbeats,
            draft_origins: &queue.draft_origins,
            now: MONDAY + 2 * DAY,
            utc_offset_secs: 0,
            cores: None,
            config: &KpiConfig::default(),
            host: None,
        },
        &KpiQuery {
            compare: Some(backwards),
            ..KpiQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("start before it ends"), "{error}");
}

/// The worker model trial's strata (ADR-0079 decision 2): the group,
/// model and effort of the run's first claim and the nature of its task's
/// prediction, `none` outside the trial and `unknown` without a record;
/// the periods compare them like any stratum and so does `--compare` with
/// `--by`.
#[test]
fn splits_the_runs_by_the_trial_group_model_effort_and_nature() {
    const OPUS: (&str, &str, Option<&str>) = ("claude-opus-5-5", "medium", Some("control"));
    const SONNET: (&str, &str, Option<&str>) = ("claude-sonnet-5", "medium", Some("treatment"));
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY;
    // Monday: one control run.
    let mut run = Run::new(1, None, MONDAY + HOUR, 100);
    run.session = Some(OPUS);
    run.nature = Some("mechanical");
    queue.run(&run);
    // Tuesday: control, two treatments (one sent back), one outside the
    // trial without a prediction, and one claimed before the record.
    for (index, (session, nature, revise)) in [
        (Some(OPUS), Some("mechanical"), false),
        (Some(SONNET), Some("mechanical"), false),
        (Some(SONNET), Some("mechanical"), true),
        (Some(("claude-opus-5-5", "high", None)), None, false),
        (None, Some("design"), false),
    ]
    .into_iter()
    .enumerate()
    {
        let index = index as i64;
        let mut run = Run::new(10 + index, None, tuesday + HOUR * (index + 1), 100);
        run.session = session;
        run.nature = nature;
        run.revise = revise;
        queue.run(&run);
    }
    // A later claim of the first task, in a resume, keeps the first one's.
    queue.push(
        Some(11),
        Some(&format!(
            "{:08x}-0000-4000-8000-{:012x}",
            11,
            tuesday + 2 * HOUR
        )),
        "run_claimed",
        json!({"model": "claude-opus-5-5", "effort": "xhigh", "group": null}),
        tuesday + 2 * HOUR + 30,
    );
    let query = KpiQuery {
        last: 2,
        at: Some(Cursor::Time(tuesday * 1000)),
        by: vec![Axis::Group, Axis::Model, Axis::Effort, Axis::Nature],
        ..KpiQuery::default()
    };
    let result = queue.kpi(tuesday + DAY + HOUR, &KpiConfig::default(), &query);
    let [monday, tuesday_kpis] = [&result.periods[0], &result.periods[1]];
    let landings = |stratum: &str| measure(tuesday_kpis, "landings", stratum).value;
    assert_eq!(landings("group=control"), Some(1.0));
    assert_eq!(landings("group=treatment"), Some(2.0));
    assert_eq!(landings("group=none"), Some(1.0));
    assert_eq!(landings("group=unknown"), Some(1.0));
    assert_eq!(landings("model=claude-opus-5-5"), Some(2.0));
    assert_eq!(landings("model=claude-sonnet-5"), Some(2.0));
    assert_eq!(landings("model=unknown"), Some(1.0));
    assert_eq!(landings("effort=medium"), Some(3.0));
    assert_eq!(landings("effort=high"), Some(1.0));
    assert!(!tuesday_kpis.window.kpis["landings"].contains_key("effort=xhigh"));
    assert_eq!(landings("effort=unknown"), Some(1.0));
    assert_eq!(landings("nature=mechanical"), Some(3.0));
    assert_eq!(landings("nature=design"), Some(1.0));
    assert_eq!(landings("nature=unknown"), Some(1.0));
    assert_eq!(
        measure(tuesday_kpis, "revise_rate", "group=treatment").value,
        Some(0.5)
    );
    assert_eq!(measure(tuesday_kpis, "phase.work", "group=control").n, 1);
    assert_eq!(
        measure(monday, "landings", "group=control").value,
        Some(1.0)
    );
    // Against Monday like any stratum: a count is judged.
    let control = &tuesday_kpis.comparison["landings"]["group=control"];
    assert_eq!((control.previous, control.delta), (Some(1.0), Some(0.0)));
    assert!(control.judged);
    // Without `--by`, only the kinds.
    let plain = queue.kpi(
        tuesday + DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            by: Vec::new(),
            ..query.clone()
        },
    );
    assert!(!plain.periods[1].window.kpis["landings"].contains_key("group=control"));
    // `--compare` splits by `--by` too, next to its own axes.
    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY,
        MONDAY + DAY,
        tuesday,
        tuesday + DAY
    )
    .parse()
    .unwrap();
    let compared = queue.kpi(
        tuesday + DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            compare: Some(spec),
            by: vec![Axis::Group],
            ..KpiQuery::default()
        },
    );
    let compare = compared.compare.unwrap();
    let control = &compare.strata["landings"]["group=control"];
    assert_eq!(
        (control.before.value, control.after.value),
        (Some(1.0), Some(1.0))
    );
    assert!(compare.strata["landings"].contains_key("parallel=3"));
    assert!(!compare.strata["landings"].contains_key("model=claude-sonnet-5"));
}

/// `--since` / `--until` give one window next to the one of the same
/// length before it.
#[test]
fn one_window_of_any_length() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY + 3 * HOUR, 100));
    let query = KpiQuery {
        since: Some(Cursor::Time((MONDAY + HOUR) * 1000)),
        until: Some(Cursor::Time((MONDAY + 5 * HOUR) * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    assert_eq!(result.period, "window");
    assert_eq!(result.periods.len(), 1);
    let window = &result.periods[0];
    assert!(!window.partial);
    assert_eq!(measure(window, "landings", ALL).value, Some(1.0));
    assert_eq!(window.comparison["landings"][ALL].previous, Some(1.0));
    let error = kpi(
        &KpiInput {
            events: &queue.events,
            goals: &queue.goals,
            kinds: &queue.kinds,
            changes: &queue.changes,
            areas: None,
            heartbeats: &queue.heartbeats,
            draft_origins: &queue.draft_origins,
            now: MONDAY + DAY,
            utc_offset_secs: 0,
            cores: None,
            config: &KpiConfig::default(),
            host: None,
        },
        &KpiQuery {
            since: query.until,
            until: query.since,
            ..KpiQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("before --until"), "{error}");
}

/// The KPIs no run carries: slot usage while a supervisor lived, the
/// candidates' samples, asks per landing and a person's wait, attentions,
/// findings, and the ones not recorded yet.
#[test]
fn derives_the_queue_kpis_of_a_window() {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    queue.queue_event(
        window::CANDIDATES_SAMPLED,
        json!({"candidates": 2, "free_slots": 1, "ready": 2}),
        MONDAY,
    );
    // Two hours of one run in four hours of two slots.
    queue.run(&Run::new(
        1,
        Some("runtime".parse::<TaskKind>().unwrap()),
        MONDAY + HOUR,
        2 * HOUR - 100,
    ));
    queue.queue_event(
        window::CANDIDATES_SAMPLED,
        json!({"candidates": 0, "free_slots": 1, "ready": 1}),
        MONDAY + 2 * HOUR,
    );
    queue.queue_event(
        marks::SUPERVISOR_STOPPED,
        json!({"supervisor": "s"}),
        MONDAY + 4 * HOUR,
    );
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 7, "kind": "worker_question", "reason_category": "scope"}),
        MONDAY + 90 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_answered",
        json!({"ask_id": 7, "answered_by": "human"}),
        MONDAY + 100 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 8, "kind": "stuck_exit"}),
        MONDAY + 100 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_answered",
        json!({"ask_id": 8, "runtime_closed": true}),
        MONDAY + 101 * 60,
    );
    queue.queue_event("finding_recorded", json!({"finding_id": 1}), MONDAY + HOUR);
    queue.queue_event("finding_recorded", json!({"finding_id": 2}), MONDAY + HOUR);
    queue.queue_event(
        "finding_status_changed",
        json!({"finding_id": 1, "from": "open", "to": "resolved"}),
        MONDAY + 3 * HOUR,
    );
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    assert!(!day.partial);
    assert_eq!(measure(day, "slot_usage", ALL).value, Some(0.25));
    assert_eq!(measure(day, "slot_usage", ALL).n, 1);
    // 2 candidates for two hours, then none until now (22 hours).
    let candidates = measure(day, "candidates", ALL);
    assert_eq!(candidates.value, Some(round3(4.0 / 24.0)));
    assert_eq!(candidates.max, Some(2.0));
    assert_eq!(
        day.window.details["candidates"]["starved_secs"],
        json!(22 * HOUR)
    );
    assert_eq!(measure(day, "asks_per_landing", ALL).value, Some(2.0));
    assert_eq!(
        measure(day, "asks_per_landing", "kind=runtime").value,
        Some(2.0)
    );
    // The runtime's own close is no person's wait.
    let wait = measure(day, "ask_wait", ALL);
    assert_eq!((wait.n, wait.median), (1, Some(600.0)));
    assert_eq!(measure(day, "findings_open", ALL).value, Some(1.0));
    assert_eq!(
        measure(day, "finding_resolve_time", ALL).median,
        Some(7200.0)
    );
    assert_eq!(day.window.details["findings"]["recorded"], json!(2));
    assert!(measure(day, "attentions_per_landing", ALL).value.is_some());
    assert_eq!(
        day.window.unavailable["improvement_proposals"],
        "not_recorded"
    );
    assert!(!day.window.unavailable.contains_key("candidates"));
    // A day before the queue: no supervisor, no sample.
    let before = &result.periods[0];
    assert_eq!(measure(before, "slot_usage", ALL).value, None);
    assert_eq!(before.window.unavailable["candidates"], "no_samples");
    assert_eq!(measure(before, "landings", ALL).value, Some(0.0));
}

/// The slot usage of Monday with one run holding a slot for two hours
/// under a supervisor of two slots started at midnight, once `alive`
/// wrote its end (or left it out).
fn monday_slot_usage(alive: impl FnOnce(&mut Queue)) -> Option<f64> {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    queue.run(&Run::new(
        1,
        Some("runtime".parse::<TaskKind>().unwrap()),
        MONDAY + HOUR,
        2 * HOUR - 100,
    ));
    alive(&mut queue);
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    measure(result.periods.last().unwrap(), "slot_usage", ALL).value
}

/// A supervisor that went stale with neither its stop nor a later start
/// ends at its last heartbeat (ADR-0051 decision 10), not now; a live one
/// is alive until now.
#[test]
fn a_stale_supervisor_ends_at_its_last_heartbeat() {
    // Two hours of one run in four hours of two slots.
    let stale = monday_slot_usage(|queue| {
        queue.heartbeats.insert("s".to_owned(), MONDAY + 4 * HOUR);
    });
    assert_eq!(stale, Some(0.25));
    // Alive (its heartbeat fresh at now): the whole day.
    let live = monday_slot_usage(|queue| {
        queue.heartbeats.insert("s".to_owned(), MONDAY + DAY - 1);
    });
    assert_eq!(live, Some(round3(2.0 / 48.0)));
    // `up` or `down` pruned its row: the stop they recorded ends it at the
    // row's last heartbeat, not at the prune.
    let pruned = monday_slot_usage(|queue| {
        queue.queue_event(
            marks::SUPERVISOR_STOPPED,
            json!({"supervisor": "s", "outcome": "pruned", "last_heartbeat_at": MONDAY + 4 * HOUR}),
            MONDAY + 10 * HOUR,
        );
    });
    assert_eq!(pruned, Some(0.25));
    // Its row went without a stop: it ends at the last event it recorded.
    let gone = monday_slot_usage(|queue| {
        queue.queue_event(
            window::CANDIDATES_SAMPLED,
            json!({"supervisor": "s", "candidates": 0, "free_slots": 2, "ready": 0}),
            MONDAY + 4 * HOUR,
        );
    });
    assert_eq!(gone, Some(0.25));
    // A later start still ends it first.
    let restarted = monday_slot_usage(|queue| {
        queue.heartbeats.insert("s".to_owned(), MONDAY + 6 * HOUR);
        queue.queue_event(
            marks::SUPERVISOR_STARTED,
            json!({"supervisor": "t", "parallel": 2}),
            MONDAY + 4 * HOUR,
        );
        queue.queue_event(
            marks::SUPERVISOR_STOPPED,
            json!({"supervisor": "t"}),
            MONDAY + 4 * HOUR,
        );
    });
    assert_eq!(restarted, Some(0.25));
}

/// Only the `integrate` attempts that ran a verification command count
/// towards `verification_failed_rate`: one stopped before its commands
/// (an empty rebase, a dirty worktree) does not.
#[test]
fn verification_failed_rate_counts_the_attempts_that_verified() {
    let mut queue = Queue::default();
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, HOUR));
    let receipt = landed - 100;
    let id = format!("{:08x}-0000-4000-8000-{:012x}", 1, MONDAY + HOUR);
    let (task, id) = (Some(1), Some(id.as_str()));
    let attempt = |queue: &mut Queue, at: i64, verified: Option<i64>, code: &str| {
        queue.push(task, id, "integration_started", json!({}), at);
        queue.push(task, id, "integration_rebased", json!({}), at + 1);
        if let Some(number) = verified {
            queue.push(
                task,
                id,
                "verification_command",
                json!({"phase": "integration", "attempt": number, "index": 1, "exit_code": 1}),
                at + 2,
            );
        }
        queue.push(
            task,
            id,
            "integration_deferred",
            json!({"code": code}),
            at + 3,
        );
    };
    attempt(&mut queue, receipt + 21, None, "rebase_empty");
    attempt(&mut queue, receipt + 25, None, "dirty_worktree");
    attempt(&mut queue, receipt + 30, Some(3), "verification_failed");
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let rate = measure(
        result.periods.last().unwrap(),
        "verification_failed_rate",
        ALL,
    );
    // The failed attempt 3 and the landing's attempt 1 of the four rebased.
    assert_eq!((rate.n, rate.value), (2, Some(0.5)));
}

/// With `--goal`, only that goal's runs count.
#[test]
fn a_goal_keeps_only_its_runs() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY, 100));
    queue.goals.insert(TaskId::new(1), Some(GoalId::new(5)));
    let query = KpiQuery {
        goal_id: Some(GoalId::new(5)),
        last: 1,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 12 * HOUR, &KpiConfig::default(), &query);
    assert_eq!(
        measure(&result.periods[0], "landings", ALL).value,
        Some(1.0)
    );
    assert_eq!(result.config.min_samples, config::DEFAULT_MIN_SAMPLES);
}

/// The quality of the plans per day (ADR-0079 decision 7): the revises of
/// plan review split by the model and effort of its session, and the
/// proposal's tasks' rework and duplicates by the review that judged it and
/// the proposal's features; the follow-ups adopted as `draft_flow` counts
/// them.
#[test]
fn plan_quality_is_split_by_the_judging_session_and_the_proposal() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY + 10 * HOUR;
    let features = |revise_count: i64| {
        json!({"origin": "person", "follow_up_depth": 0, "related_score": 4.0,
               "related": "mid", "revise_count": revise_count})
    };
    let review = |queue: &mut Queue, review: i64, effort: &str, decision: &str, at: i64| {
        queue.push(
            Some(10),
            None,
            "plan_review_started",
            json!({"proposal_id": 1, "plan_review_id": review,
                   "features": features(review - 1)}),
            at,
        );
        queue.push(
            Some(10),
            None,
            "session_opened",
            json!({"kind": "plan_review", "plan_review_id": review, "marker": review}),
            at,
        );
        queue.push(
            Some(10),
            None,
            "plan_review_finished",
            json!({"proposal_id": 1, "plan_review_id": review, "decision": decision}),
            at + 60,
        );
        queue.push(
            Some(10),
            None,
            "session_closed",
            json!({"kind": "plan_review", "opened_marker": review,
                   "model": "claude-opus-5-5", "effort": effort}),
            at + 60,
        );
    };
    for task in [10, 11] {
        queue.push(
            Some(task),
            None,
            "task_submitted",
            json!({"proposal_id": 1}),
            tuesday - 60,
        );
    }
    review(&mut queue, 1, "medium", "revise", tuesday);
    review(&mut queue, 2, "high", "pass", tuesday + HOUR);
    // Task 10 is a follow-up draft adopted into the proposal; task 11's run
    // is sent back to revise; 10 lands.
    queue.push(
        Some(9),
        None,
        "follow_up_registered",
        json!({"task_id": 10}),
        tuesday - 2 * HOUR,
    );
    queue.push(
        Some(10),
        None,
        "task_created",
        json!({}),
        tuesday - 2 * HOUR,
    );
    queue.push(
        Some(10),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "submitted"}),
        tuesday - HOUR,
    );
    let mut revised = Run::new(
        11,
        Some("runtime".parse::<TaskKind>().unwrap()),
        tuesday + 3 * HOUR,
        600,
    );
    revised.revise = true;
    queue.run(&revised);
    queue.run(&Run::new(
        10,
        Some("runtime".parse::<TaskKind>().unwrap()),
        tuesday + 3 * HOUR,
        300,
    ));
    queue.sort();
    // Link each session_closed to its session_opened by the ids sorting
    // gave them.
    let opened: HashMap<i64, i64> = queue
        .events
        .iter()
        .filter(|e| e.kind == "session_opened")
        .map(|e| (e.payload["marker"].as_i64().unwrap(), e.id.as_i64()))
        .collect();
    for event in &mut queue.events {
        if let Some(marker) = event.payload.get("opened_marker").and_then(Value::as_i64) {
            event.payload["opened_event_id"] = json!(opened[&marker]);
        }
    }
    let kpi = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            ..KpiQuery::default()
        },
    );
    let day = &kpi.periods[0];
    assert_eq!(day.label, "2026-09-22");
    let value = |name: &str, stratum: &str| {
        let measure = measure(day, name, stratum);
        (measure.n, measure.value)
    };
    assert_eq!(value("plan.revise_rate", "all"), (2, Some(0.5)));
    assert_eq!(value("plan.revise_rate", "effort=medium"), (1, Some(1.0)));
    assert_eq!(value("plan.revise_rate", "effort=high"), (1, Some(0.0)));
    assert_eq!(value("plan.revise_rate", "revise_count=0"), (1, Some(1.0)));
    // The proposal was judged by the high review of its second submission.
    assert_eq!(
        value("plan.task_rework_rate", "effort=high"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "model=claude-opus-5-5"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "related=mid"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "revise_count=1"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "origin=person"),
        (2, Some(0.5))
    );
    assert_eq!(value("plan.task_rework_rate", "effort=medium"), (0, None));
    assert_eq!(
        value("plan.duplicate_cancels_after_ready", "effort=high"),
        (1, Some(0.0))
    );
    assert_eq!(
        value("plan.follow_up_canceled_after_adoption", "effort=high"),
        (1, Some(0.0))
    );
    assert_eq!(value("plan.follow_up_adoption_rate", "all"), (1, Some(1.0)));
    // The next day has no review: its rate is null, next to the previous.
    let next = &kpi.periods[1];
    assert_eq!(measure(next, "plan.revise_rate", "all").value, None);
    assert_eq!(
        next.comparison["plan.revise_rate"]["all"].previous,
        Some(0.5)
    );
    assert_eq!(direction("plan.task_rework_rate"), Some(Direction::Lower));
    assert_eq!(direction("plan.follow_up_adoption_rate"), None);
}

/// The drafts the runtime and the jobs register (task 611): per landing
/// and still waiting at the period's end, the very values of `stats`'
/// `draft_flow` over the same window, next to the previous day and judged
/// against a target.
#[test]
fn the_drafts_per_landing_and_the_backlog_are_stats_draft_flow() {
    let mut queue = Queue::default();
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    for (task, secs) in [(10, landed + 10), (11, landed + 20), (20, landed + 30)] {
        queue.push(Some(task), None, "task_created", json!({}), secs);
    }
    for task in [10, 11] {
        queue.push(
            Some(1),
            None,
            "follow_up_registered",
            json!({"task_id": task}),
            landed + 5 + task,
        );
    }
    queue
        .draft_origins
        .insert(TaskId::new(20), DraftOrigin::GoalGap);
    queue.run(&Run::new(2, None, MONDAY + DAY + HOUR, 300));
    let next = MONDAY + DAY + 2 * HOUR;
    let changed = |to: &str| json!({"from": "draft", "to": to});
    queue.push(
        Some(10),
        None,
        "task_status_changed",
        changed("submitted"),
        next,
    );
    queue.push(
        Some(11),
        None,
        "task_status_changed",
        changed("canceled"),
        next + 1,
    );
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: ["draft_backlog", "drafts_per_landing"]
                .map(|kpi| Target {
                    kpi: kpi.into(),
                    kind: None,
                    change: None,
                    area: None,
                    stat: None,
                    min: None,
                    max: Some(0.5),
                })
                .into(),
            ..KpiSettings::default()
        }),
        None,
    );
    let now = MONDAY + 2 * DAY + HOUR;
    let query = KpiQuery {
        last: 3,
        ..KpiQuery::default()
    };
    let result = queue.kpi(now, &config, &query);
    let [first, second, today] = &result.periods[..] else {
        panic!("three days");
    };
    let per_landing = measure(first, "drafts_per_landing", ALL);
    assert_eq!((per_landing.n, per_landing.value), (1, Some(3.0)));
    let backlog = measure(first, "draft_backlog", ALL);
    assert_eq!((backlog.value, backlog.max.is_some()), (Some(3.0), true));
    let drafts = &first.window.details["drafts"];
    assert_eq!(drafts["registered"], 3);
    assert_eq!(drafts["by_origin"]["follow_up"]["drafts_per_landing"], 2.0);
    assert_eq!(drafts["by_origin"]["goal_gap"]["drafts_per_landing"], 1.0);
    assert_eq!(measure(second, "drafts_per_landing", ALL).value, Some(0.0));
    assert_eq!(measure(second, "draft_backlog", ALL).value, Some(1.0));
    assert_eq!(second.window.details["drafts"]["inflow_per_outflow"], 0.0);
    // No landing: null, not 0.
    assert_eq!(measure(today, "drafts_per_landing", ALL).value, None);
    // Only `all`: no run carries a draft.
    assert_eq!(first.window.kpis["draft_backlog"].len(), 1);

    // The same window's `draft_flow`.
    for period in [first, second, today] {
        let flow = crate::domain::stats::stats(
            &queue.events,
            &queue.goals,
            now,
            crate::domain::stats::SlotSnapshot::default(),
            &crate::domain::stats::StatsQuery {
                since: Some(Cursor::Time(timestamp_millis(&period.start).unwrap())),
                until: Some(Cursor::Time(period.end_ms())),
                goal_id: None,
                full: true,
            },
            &crate::domain::stats::LiveSnapshot {
                draft_origins: queue.draft_origins.clone(),
                ..crate::domain::stats::LiveSnapshot::default()
            },
        )
        .draft_flow;
        let per_landing = measure(period, "drafts_per_landing", ALL);
        assert_eq!(
            per_landing.value, flow.drafts_per_landing,
            "{}",
            period.label
        );
        assert_eq!(per_landing.n as i64, flow.landings);
        let backlog = measure(period, "draft_backlog", ALL);
        assert_eq!(backlog.value, Some(flow.all.backlog as f64));
        assert_eq!(backlog.max, flow.all.oldest_backlog_secs.map(|s| s as f64));
    }

    // Better when smaller, against the day before.
    let change = &second.comparison["drafts_per_landing"][ALL];
    assert_eq!(
        (change.previous, change.verdict),
        (Some(3.0), Some("improved"))
    );
    assert_eq!(direction("draft_backlog"), Some(Direction::Lower));
    // The backlog is off target two judged days in a row (short of a
    // breach), the drafts per landing back on target the second day.
    let state = |kpi: &str| {
        let target = result.targets.iter().find(|t| t.kpi == kpi).unwrap();
        (target.state, target.streak)
    };
    assert_eq!(state("draft_backlog"), ("missed", 2));
    assert_eq!(state("drafts_per_landing"), ("ok", 0));
}

/// The follow_up drafts by the category their worker gave them
/// (ADR-t947-3): the adoption and duplicate rates and the time they stayed
/// drafts per `category=` stratum, next to `all`, and `stats`'
/// `follow_up_categories` in the details.
#[test]
fn the_follow_up_rates_are_split_by_category() {
    let mut queue = Queue::default();
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    for (task, category) in [(10, Some("defect")), (11, Some("defect")), (12, None)] {
        queue.push(Some(task), None, "task_created", json!({}), landed + task);
        let mut payload = json!({"task_id": task, "index": task - 10});
        if let Some(category) = category {
            payload["category"] = json!(category);
        }
        queue.push(
            Some(1),
            None,
            "follow_up_registered",
            payload,
            landed + task,
        );
    }
    let next = MONDAY + DAY + 2 * HOUR;
    for (task, to, duplicate_of, at) in [
        (10, "submitted", None, next),
        (11, "canceled", Some(3), next + 100),
        (12, "canceled", None, next + 200),
    ] {
        queue.push(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "draft", "to": to, "duplicate_of": duplicate_of}),
            at,
        );
    }
    let now = MONDAY + 2 * DAY + HOUR;
    let query = KpiQuery {
        last: 3,
        ..KpiQuery::default()
    };
    let result = queue.kpi(now, &KpiConfig::default(), &query);
    let second = &result.periods[1];
    let value = |kpi: &str, stratum: &str| {
        let measure = measure(second, kpi, stratum);
        (measure.n, measure.value)
    };
    assert_eq!(
        value("plan.follow_up_adoption_rate", "category=defect"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.follow_up_duplicate_rate", "category=defect"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.follow_up_adoption_rate", "category=unlabeled"),
        (1, Some(0.0))
    );
    assert_eq!(
        value("plan.follow_up_duplicate_rate", ALL),
        (3, Some(0.333))
    );
    let secs = measure(second, "plan.follow_up_draft_secs", "category=defect");
    assert_eq!(secs.n, 2);
    assert!(secs.max > secs.min, "{secs:?}");
    assert_eq!(measure(second, "plan.follow_up_draft_secs", ALL).n, 3);
    let details = &second.window.details["follow_up_categories"];
    assert_eq!(details["defect"]["duplicate"], 1);
    assert_eq!(details["unlabeled"]["canceled"], 1);
    // The day they were registered: none left draft yet.
    let first = &result.periods[0];
    assert_eq!(
        first.window.details["follow_up_categories"]["defect"]["registered"],
        2
    );
    assert_eq!(
        measure(first, "plan.follow_up_adoption_rate", "category=defect").value,
        None
    );
    assert_eq!(
        direction("plan.follow_up_duplicate_rate"),
        Some(Direction::Lower)
    );
}

/// The forecast's errors (ADR-0070 decision 4): every snapshot of a target
/// that finished in the period is a sample in the period it finished,
/// split by target, kind, band, method and whether a change mark came
/// between; a canceled task is only counted.
#[test]
fn scores_the_forecast_snapshots_in_the_period_they_finished() {
    let mut queue = Queue::default();
    let runtime = Some("runtime".parse::<TaskKind>().unwrap());
    let docs = Some("docs".parse::<TaskKind>().unwrap());
    queue.kinds.insert(TaskId::new(1), runtime);
    queue.kinds.insert(TaskId::new(2), docs);
    let snapshot = |queue: &mut Queue, secs: i64, tasks: Value, goals: Value| {
        queue.queue_event(
            "forecast_recorded",
            json!({"at_secs": secs, "method": 1, "tasks": tasks, "goals": goals}),
            secs,
        );
    };
    let row = |id: i64, p50: i64, p90: i64| json!({"id": id, "p50_secs": p50, "p90_secs": p90});
    let status = |queue: &mut Queue, task: i64, to: &str, secs: i64| {
        queue.push(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": to}),
            secs,
        );
    };
    snapshot(
        &mut queue,
        MONDAY + HOUR,
        json!([
            row(1, 2 * HOUR, 4 * HOUR),
            row(2, HOUR, HOUR),
            row(3, HOUR, HOUR)
        ]),
        json!([row(7, 10 * HOUR, 11 * HOUR)]),
    );
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "parallel 3"}),
        MONDAY + 2 * HOUR,
    );
    snapshot(
        &mut queue,
        MONDAY + 3 * HOUR,
        json!([row(1, HOUR / 2, 2 * HOUR)]),
        json!([]),
    );
    status(&mut queue, 1, "completed", MONDAY + 4 * HOUR);
    status(&mut queue, 3, "canceled", MONDAY + 5 * HOUR);
    queue.queue_event(
        "goal_closed",
        json!({"verdict": "achieved"}),
        MONDAY + 13 * HOUR,
    );
    queue.events.last_mut().unwrap().goal_id = Some(GoalId::new(7));
    status(&mut queue, 2, "completed", MONDAY + DAY + 2 * HOUR);
    let query = KpiQuery {
        last: 2,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + 12 * HOUR, &KpiConfig::default(), &query);
    let monday = &result.periods[0];
    assert_eq!(monday.label, "2026-09-21");
    // Task 1 an hour late, then 30 minutes; the goal 2 hours late and past
    // its p90.
    let error = measure(monday, "forecast.p50_error", ALL);
    assert_eq!((error.n, error.median), (3, Some(3600.0)));
    let ratio = measure(monday, "forecast.p50_error_ratio", ALL);
    assert_eq!((ratio.median, ratio.max), (Some(0.5), Some(1.0)));
    assert_eq!(
        measure(monday, "forecast.p90_hit_rate", ALL).value,
        Some(0.667)
    );
    assert_eq!(measure(monday, "forecast.late_rate", ALL).value, Some(1.0));
    assert_eq!(measure(monday, "forecast.early_rate", ALL).value, Some(0.0));
    assert_eq!(measure(monday, "forecast.p50_error", "target=goal").n, 1);
    assert_eq!(measure(monday, "forecast.p50_error", "kind=runtime").n, 2);
    assert_eq!(measure(monday, "forecast.p50_error", "band=0-1h").n, 1);
    assert_eq!(measure(monday, "forecast.p50_error", "method=1").n, 3);
    // Only the second snapshot of task 1 had no mark before the finish.
    let unmarked = measure(monday, "forecast.p50_error", "marks=0");
    assert_eq!((unmarked.n, unmarked.median), (1, Some(1800.0)));
    assert_eq!(measure(monday, "forecast.p50_error", "marks=1+").n, 2);
    let details = &monday.window.details["forecast"];
    assert_eq!(details["samples"], 3);
    assert_eq!(details["with_marks"], 2);
    assert_eq!(details["excluded"]["canceled"], 1);
    assert_eq!(details["marks_between"]["max"], 1.0);
    // Task 2 finished the next day, a day late.
    let tuesday = &result.periods[1];
    let error = measure(tuesday, "forecast.p50_abs_error", "kind=docs");
    assert_eq!((error.n, error.median), (1, Some(24.0 * 3600.0)));
    assert_eq!(
        measure(tuesday, "forecast.p90_hit_rate", ALL).value,
        Some(0.0)
    );
    assert_eq!(direction("forecast.p50_abs_error"), Some(Direction::Lower));
    assert_eq!(direction("forecast.p90_hit_rate"), None);
    // A goal's filter keeps its own samples only.
    let goal = queue.kpi(
        MONDAY + DAY + 12 * HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            goal_id: Some(GoalId::new(7)),
            ..query
        },
    );
    assert_eq!(measure(&goal.periods[0], "forecast.p50_error", ALL).n, 1);
}

/// The details split the backend failures, as `stats` does, into the
/// attempts a retry took up and the calls that gave up, per `op` too;
/// `backend_failures_per_run` still counts them all.
#[test]
fn details_split_the_backend_failures_into_retried_and_exhausted() {
    let mut queue = Queue::default();
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, HOUR));
    let failed = |queue: &mut Queue, op: &str, retry: Option<Value>, at: i64| {
        let mut payload = json!({"op": op, "attempt": 1, "max_attempts": 3});
        if let Some(retry) = retry {
            payload["retry_after_ms"] = retry;
        }
        queue.push(Some(1), None, "backend_call_failed", payload, at);
    };
    failed(&mut queue, "capture", Some(json!(2000)), landed - 300);
    failed(&mut queue, "capture", Some(Value::Null), landed - 200);
    failed(&mut queue, "send_text", Some(json!(4000)), landed - 150);
    // Before task 326: no retry_after_ms at all.
    failed(&mut queue, "close", None, landed - 120);
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    assert_eq!(
        measure(day, "backend_failures_per_run", ALL).value,
        Some(4.0)
    );
    let details = &day.window.details;
    assert_eq!(
        details["backend_failures_by_op"],
        json!({"capture": 2, "send_text": 1, "close": 1})
    );
    assert_eq!(
        details["backend_failures_retried"],
        json!({"count": 2, "by_op": {"capture": 1, "send_text": 1}})
    );
    assert_eq!(
        details["backend_failures_exhausted"],
        json!({"count": 2, "by_op": {"capture": 1, "close": 1}})
    );
}

/// Each period, the `--since`/`--until` window and both sides of a
/// comparison carry the host's load of their span (task 872): a sample at
/// a boundary belongs to the period that ends there, a period without one
/// has none, a reader's error is carried, and nothing is judged on it.
#[test]
fn periods_windows_and_comparisons_carry_the_host_load_of_their_span() {
    use crate::domain::host_metrics::{HostSample, summarize};
    let samples: Vec<HostSample> = [
        (MONDAY, 1.0),
        (MONDAY + 1, 2.0),
        (MONDAY + DAY, 4.0),
        (MONDAY + DAY + HOUR, 8.0),
    ]
    .into_iter()
    .map(|(unix, load)| {
        let mut sample = HostSample::new(unix);
        sample.set("load1", Some(load));
        sample
    })
    .collect();
    let read = |from, until| summarize(&samples, from, until);
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            targets: vec![Target {
                kpi: "landings".into(),
                kind: None,
                change: None,
                area: None,
                stat: None,
                min: Some(5.0),
                max: None,
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let now = MONDAY + DAY + 2 * HOUR;
    let query = KpiQuery {
        period: Period::Day,
        last: 4,
        ..KpiQuery::default()
    };
    let with = queue.kpi_with_host(now, &config, &query, Some(HostReader(&read)));
    let without = queue.kpi(now, &config, &query);
    let counts: Vec<usize> = with
        .periods
        .iter()
        .map(|period| {
            let host = period.host.as_ref().unwrap();
            let start = timestamp_millis(&period.start).unwrap() / 1000;
            assert_eq!(host.from, start + 1, "{}", period.label);
            assert_eq!(host.until, period.end_ms() / 1000, "{}", period.label);
            host.samples
        })
        .collect();
    assert_eq!(counts.iter().sum::<usize>(), samples.len(), "{counts:?}");
    assert!(counts.contains(&0), "{counts:?}");
    // Every sample is in exactly one period: none is counted at both
    // sides of a boundary.
    for sample in &samples {
        let holding = with
            .periods
            .iter()
            .filter(|period| {
                let host = period.host.as_ref().unwrap();
                (host.from..=host.until).contains(&sample.unix)
            })
            .count();
        assert_eq!(holding, 1, "{}", sample.unix);
    }
    // The host is a reference only: the targets are judged the same.
    assert_eq!(with.targets, without.targets);
    assert!(without.periods.iter().all(|period| period.host.is_none()));
    let json = serde_json::to_value(&without).unwrap();
    assert!(json["periods"][0].get("host").is_none());

    // The `--since` / `--until` window: a boundary sample goes to the
    // window that ends at it, not the one that starts there.
    let window = queue.kpi_with_host(
        now,
        &config,
        &KpiQuery {
            since: Some(Cursor::Time((MONDAY + 1) * 1000)),
            until: Some(Cursor::Time((MONDAY + DAY) * 1000)),
            ..KpiQuery::default()
        },
        Some(HostReader(&read)),
    );
    let host = window.periods[0].host.as_ref().unwrap();
    assert_eq!(
        (host.from, host.until, host.samples),
        (MONDAY + 2, MONDAY + DAY, 1)
    );
    assert_eq!(host.metrics["load1"].unwrap().max, 4.0);

    // Both sides of a comparison.
    let compared = queue.kpi_with_host(
        now,
        &config,
        &KpiQuery {
            compare: Some(CompareSpec::Windows([
                (
                    Cursor::Time((MONDAY - 1) * 1000),
                    Cursor::Time((MONDAY + 1) * 1000),
                ),
                (
                    Cursor::Time((MONDAY + 1) * 1000),
                    Cursor::Time((MONDAY + DAY + HOUR) * 1000),
                ),
            ])),
            ..query.clone()
        },
        Some(HostReader(&read)),
    );
    let compare = compared.compare.as_ref().unwrap();
    assert_eq!(compare.before.host.as_ref().unwrap().samples, 2);
    assert_eq!(compare.after.host.as_ref().unwrap().samples, 2);

    // A reader that failed: the error is carried and the KPIs are made.
    let failing = |from, until| crate::domain::host_metrics::HostSummary {
        error: Some("unreadable".into()),
        ..summarize(&[], from, until)
    };
    let failed = queue.kpi_with_host(now, &config, &query, Some(HostReader(&failing)));
    let host = failed.periods[0].host.as_ref().unwrap();
    assert_eq!(
        (host.samples, host.error.as_deref()),
        (0, Some("unreadable"))
    );
    assert_eq!(failed.targets, without.targets);
}

/// The integration slot (goal 72): every `integrate` attempt holds it,
/// landed or not, one at a time, so the day's total never exceeds the
/// day; the peak is the busiest hour, and a day not over yet counts up
/// to now.
#[test]
fn landing_utilization_counts_every_attempt_within_the_day() {
    let mut queue = Queue::default();
    // Lands at 01:00 + 30m + 100s, its attempt from 40s before the end.
    let landed = queue.run(&Run::new(1, None, MONDAY + HOUR, 30 * 60));
    let id = format!("{:08x}-0000-4000-8000-{:012x}", 2, MONDAY + HOUR);
    let (task, id) = (Some(2), Some(id.as_str()));
    queue.kinds.insert(TaskId::new(2), None);
    queue.goals.insert(TaskId::new(2), None);
    queue.push(task, id, "run_claimed", json!({}), MONDAY + HOUR);
    queue.push(
        task,
        id,
        "landing_queued",
        json!({"via": "exit"}),
        landed - 200,
    );
    // Deferred after 10 minutes: it held the slot all the same.
    queue.push(task, id, "integration_started", json!({}), landed);
    queue.push(
        task,
        id,
        "integration_deferred",
        json!({"code": "verification_failed", "status": "needs_session"}),
        landed + 600,
    );
    // Attempts back to back for 50 hours' worth cannot fill more than the
    // day: a run whose attempts never recorded an end is cut by the next.
    for hour in 3..24 {
        queue.push(
            task,
            id,
            "integration_started",
            json!({}),
            MONDAY + hour * HOUR,
        );
    }
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + 12 * HOUR, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    let details = &day.window.details["landing_utilization"];
    let busy = details["busy_secs"].as_i64().unwrap();
    assert!(busy <= details["window_secs"].as_i64().unwrap());
    assert_eq!(details["window_secs"], json!(DAY));
    // 60s landed, 600s deferred, 21 hours of attempts from 03:00.
    assert_eq!(busy, 60 + 600 + 21 * HOUR);
    assert_eq!(details["attempts"], json!(23));
    assert_eq!(details["landed"], json!(1));
    let utilization = measure(day, "landing_utilization", ALL);
    assert_eq!(utilization.value, Some(round3(float(busy) / float(DAY))));
    assert_eq!(utilization.max, Some(1.0));
    assert_eq!(
        measure(day, "landing_utilization.peak", ALL).value,
        Some(1.0)
    );
    assert_eq!(
        details["peak_hour"]["start"],
        marks::utc_text((MONDAY + 3 * HOUR) * 1000)
    );
    // The last attempt is still open at the day's end: 22 ended in it.
    let attempt = measure(day, "landing_attempt", ALL);
    assert_eq!((attempt.n, attempt.median), (22, Some(float(HOUR))));
    assert_eq!(attempt.min, Some(60.0));
    let waiting = measure(day, "landing_queue_depth", ALL);
    assert_eq!((waiting.n, waiting.max), (1, Some(1.0)));
    assert_eq!(waiting.value, Some(round3(200.0 / float(DAY))));

    // Monday not over yet at 04:00: three hours of attempts in four.
    let result = queue.kpi(MONDAY + 4 * HOUR, &KpiConfig::default(), &query);
    let partial = result.periods.last().unwrap();
    assert!(partial.partial);
    let details = &partial.window.details["landing_utilization"];
    assert_eq!(details["window_secs"], json!(4 * HOUR));
    assert_eq!(details["busy_secs"], json!(60 + 600 + HOUR));
}

/// The run id [`Queue::run`] gives a run.
fn run_id(run: &Run) -> RunId {
    RunId::new(format!(
        "{:08x}-0000-4000-8000-{:012x}",
        run.task, run.claimed
    ))
    .unwrap()
}

/// The areas (ADR-t980-1): a run counts in every stratum of its areas and
/// in `area=unknown` without one; `--area` keeps only those areas' strata;
/// a comparison summarizes per area; a target can bound an area. Without
/// `[areas]` no `area=` stratum is listed.
#[test]
fn splits_the_landed_runs_by_their_areas() {
    let mut queue = Queue::default();
    let runtime: TaskKind = "runtime".parse().unwrap();
    let runs = [
        Run::new(1, Some(runtime.clone()), MONDAY + HOUR, 100),
        Run::new(2, Some(runtime), MONDAY + 2 * HOUR, 300),
        Run::new(3, None, MONDAY + 3 * HOUR, 500),
    ];
    for run in &runs {
        queue.run(run);
    }
    let now = MONDAY + DAY + HOUR;
    let without = queue.kpi(now, &KpiConfig::default(), &KpiQuery::default());
    let monday = &without.periods[DEFAULT_LAST - 2];
    assert!(
        monday.window.kpis["landings"]
            .keys()
            .all(|stratum| !stratum.starts_with("area="))
    );
    queue.areas = Some(HashMap::from([
        (run_id(&runs[0]), vec!["docs".to_owned(), "src".to_owned()]),
        (run_id(&runs[1]), vec!["src".to_owned()]),
    ]));
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: vec![Target {
                kpi: "phase.work".into(),
                kind: None,
                change: None,
                area: Some("src".into()),
                stat: None,
                min: None,
                max: Some(150.0),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let result = queue.kpi(now, &config, &KpiQuery::default());
    let monday = &result.periods[DEFAULT_LAST - 2];
    let landings = |stratum: &str| measure(monday, "landings", stratum).value;
    assert_eq!(landings("area=src"), Some(2.0));
    assert_eq!(landings("area=docs"), Some(1.0));
    assert_eq!(landings("area=unknown"), Some(1.0));
    assert_eq!(landings(ALL), Some(3.0));
    assert_eq!(
        measure(monday, "phase.work", "area=src").median,
        Some(200.0)
    );
    let target = &result.targets[0];
    assert_eq!(target.stratum, "area=src");
    assert_eq!(target.periods[DEFAULT_LAST - 2].met, Some(false));

    let only = queue.kpi(
        now,
        &config,
        &KpiQuery {
            areas: vec!["docs".into()],
            ..KpiQuery::default()
        },
    );
    let strata: Vec<&String> = only.periods[DEFAULT_LAST - 2].window.kpis["landings"]
        .keys()
        .collect();
    assert!(strata.contains(&&"area=docs".to_owned()));
    assert!(strata.contains(&&"kind=runtime".to_owned()));
    assert!(!strata.contains(&&"area=src".to_owned()));

    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY,
        MONDAY + 2 * HOUR,
        MONDAY + 2 * HOUR,
        MONDAY + DAY
    )
    .parse()
    .unwrap();
    let compare = queue
        .kpi(
            now,
            &config,
            &KpiQuery {
                compare: Some(spec),
                ..KpiQuery::default()
            },
        )
        .compare
        .unwrap();
    assert_eq!(
        compare.area_summary.keys().collect::<Vec<_>>(),
        ["docs", "src", "unknown"]
    );
    let src = &compare.area_summary["src"]["phase.work"];
    assert_eq!(
        (src.before.median, src.after.median),
        (Some(100.0), Some(300.0))
    );
    let asked = queue
        .kpi(
            now,
            &config,
            &KpiQuery {
                compare: Some(spec),
                areas: vec!["src".into()],
                ..KpiQuery::default()
            },
        )
        .compare
        .unwrap();
    assert_eq!(asked.area_summary.keys().collect::<Vec<_>>(), ["src"]);
    assert!(
        asked.strata["landings"]
            .keys()
            .all(|stratum| !stratum.starts_with("area=") || stratum == "area=src")
    );
    assert_eq!("area".parse::<Axis>(), Ok(Axis::Area));
}

/// The host's CPU per landing and its load over the cores (goal 72): the
/// CPU seconds each record stands for, over the period's landings, in
/// total and per kind of process; a stretch without records counts only
/// up to twice the usual gap, so a day the supervisor was stopped for is
/// not taken as spent at its next record's load; a period without
/// records, and a report without the host's records, have none.
#[test]
fn cpu_per_landing_and_load_per_core_read_the_host_records() {
    use crate::domain::host_metrics::{HostSample, summarize};
    // Every 30 s for five minutes, then nothing for almost two hours.
    let times: Vec<i64> = (1..=11)
        .map(|index| MONDAY + 30 * index)
        .chain([MONDAY + 2 * HOUR, MONDAY + 2 * HOUR + 30])
        .collect();
    let samples: Vec<HostSample> = times
        .iter()
        .enumerate()
        .map(|(index, unix)| {
            let mut sample = HostSample::new(*unix);
            sample.set("cpu_total", Some(200.0));
            sample.set("cpu_cargo", Some(150.0));
            sample.set("cpu_other", Some(50.0));
            sample.set("load1", Some(4.0 * (index as f64 + 1.0)));
            sample
        })
        .collect();
    let read = |from, until| summarize(&samples, from, until);
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    queue.run(&Run::new(2, None, MONDAY + 3 * HOUR, 300));
    let now = MONDAY + DAY + 2 * HOUR;
    let query = KpiQuery {
        period: Period::Day,
        last: 2,
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let kpi = queue.kpi_with_host(now, &config, &query, Some(HostReader(&read)));
    let monday = &kpi.periods[0].window;
    let all = |kpis: &Kpis<Measure>, name: &str| kpis[name][ALL].clone();
    // 11 records of 30 s, the one after the gap for 60 s (twice the usual
    // 30 s, not 1h 55m) and the last for 30 s: 420 s at two cores.
    let cpu = &monday.details["cpu_per_landing"];
    assert_eq!(cpu["cpu_secs"]["covered_secs"], 420, "{cpu}");
    assert_eq!(cpu["cpu_secs"]["max_gap_secs"], 60);
    assert_eq!(cpu["cpu_secs"]["total"], 840.0);
    assert_eq!(cpu["landings"], 2);
    assert_eq!(cpu["cores"], 4);
    let per_landing = all(&monday.kpis, "cpu_per_landing");
    assert_eq!((per_landing.n, per_landing.value), (2, Some(420.0)));
    assert_eq!(
        all(&monday.kpis, "cpu_per_landing.cargo").value,
        Some(315.0)
    );
    assert_eq!(
        all(&monday.kpis, "cpu_per_landing.other").value,
        Some(105.0)
    );
    assert_eq!(all(&monday.kpis, "cpu_per_landing.rustc").value, Some(0.0));
    // load1 is 4, 8, …, 52 over four cores: 1 … 13.
    let load = all(&monday.kpis, "load_per_core");
    assert_eq!(
        (load.n, load.median, load.p90, load.max),
        (13, Some(7.0), Some(12.0), Some(13.0))
    );
    assert!(!monday.unavailable.contains_key("cpu_per_landing"));
    // Lower is better for both; the direction judges the comparison.
    assert_eq!(direction("cpu_per_landing"), Some(Direction::Lower));
    assert_eq!(direction("load_per_core"), Some(Direction::Lower));

    // Tuesday has no record: null, and why.
    let tuesday = &kpi.periods[1].window;
    assert_eq!(all(&tuesday.kpis, "cpu_per_landing").value, None);
    assert_eq!(all(&tuesday.kpis, "cpu_per_landing.cargo").value, None);
    assert_eq!(all(&tuesday.kpis, "load_per_core").median, None);
    assert_eq!(tuesday.unavailable["cpu_per_landing"], "no_host_records");
    assert_eq!(tuesday.unavailable["load_per_core"], "no_host_records");
    let json = serde_json::to_value(&kpi).unwrap();
    assert_eq!(
        json["periods"][1]["kpis"]["cpu_per_landing"]["all"]["value"],
        Value::Null
    );
    assert_eq!(
        json["periods"][0]["kpis"]["load_per_core"]["all"]["p90"],
        12.0
    );

    // A day with records and no landing has none per landing.
    let mut idle = Queue::default();
    let idle = idle.kpi_with_host(now, &config, &query, Some(HostReader(&read)));
    assert_eq!(
        all(&idle.periods[0].window.kpis, "cpu_per_landing").value,
        None
    );
    assert_eq!(
        idle.periods[0].window.unavailable["cpu_per_landing"],
        "no_landings"
    );
    // Without the host's records (the breach check, the observer), no
    // such KPI at all.
    let without = queue.kpi(now, &config, &query);
    assert!(
        !without.periods[0]
            .window
            .kpis
            .contains_key("cpu_per_landing")
    );
    assert!(
        !without.periods[0]
            .window
            .details
            .contains_key("cpu_per_landing")
    );
}

/// The review verdicts that sent runs back per primary code (ADR-t947-1
/// decision 5): `review.sendback_rate` over the runs reviewed, all of
/// them, per `code=`, per `kind=` and per `change=` (`unknown` without
/// one).
#[test]
fn review_sendback_rate_is_split_by_code_and_kind() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY + 10 * HOUR;
    let runtime = Some("runtime".parse::<TaskKind>().unwrap());
    let docs = Some("docs".parse::<TaskKind>().unwrap());
    let fix = Some("fix".parse::<TaskChange>().unwrap());
    let runs = [
        (1, runtime.clone(), fix.clone(), Some("adr_conflict")),
        (2, runtime, fix, None),
        (3, docs, None, Some("test_gap")),
    ];
    for (task, kind, change, code) in runs {
        let claimed = tuesday + task * HOUR;
        let mut run = Run::new(task, kind, claimed, 600);
        run.change = change;
        queue.run(&run);
        let id = format!("{task:08x}-0000-4000-8000-{claimed:012x}");
        let payload = match code {
            Some(code) => json!({"verdict": "concern", "reasons": ["x"],
                                 "reason_codes": [[code]], "primary_code": code}),
            None => json!({"verdict": "pass", "reasons": []}),
        };
        queue.push(
            Some(task),
            Some(&id),
            "review_finished",
            payload,
            claimed + 620,
        );
    }
    let kpi = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            ..KpiQuery::default()
        },
    );
    let day = &kpi.periods[0];
    let value = |stratum: &str| {
        let measure = measure(day, "review.sendback_rate", stratum);
        (measure.n, measure.value)
    };
    assert_eq!(value(ALL), (3, Some(0.667)));
    assert_eq!(value("code=adr_conflict"), (3, Some(0.333)));
    assert_eq!(value("code=test_gap"), (3, Some(0.333)));
    assert_eq!(value("kind=runtime"), (2, Some(0.5)));
    assert_eq!(value("kind=docs"), (1, Some(1.0)));
    assert_eq!(value("change=fix"), (2, Some(0.5)));
    assert_eq!(value("change=unknown"), (1, Some(1.0)));
    assert_eq!(direction("review.sendback_rate"), Some(Direction::Lower));
    let review = &day.window.details["review_reasons"]["review"];
    assert_eq!(review["sent_back"], 2);
    assert_eq!(review["by_change"][0]["change"], "fix");
    assert_eq!(review["by_change"][0]["by_code"]["adr_conflict"], 1);
    assert_eq!(review["by_change"][1]["change"], Value::Null);
}

/// The runs split by their task's change (ADR-t980-1): `change=<change>`
/// always, `change=unknown` without one; `--change` keeps only those
/// changes' strata; a comparison summarizes per change; a target can bound
/// a change; the asks of a task count per change.
#[test]
fn splits_the_runs_by_their_change() {
    let mut queue = Queue::default();
    let fix: TaskChange = "fix".parse().unwrap();
    let feature: TaskChange = "feature".parse().unwrap();
    let mut runs = [
        Run::new(1, None, MONDAY + HOUR, 100),
        Run::new(2, None, MONDAY + 2 * HOUR, 300),
        Run::new(3, None, MONDAY + 3 * HOUR, 500),
    ];
    runs[0].change = Some(fix.clone());
    runs[1].change = Some(fix);
    runs[2].change = Some(feature);
    for run in &runs {
        queue.run(run);
    }
    queue.run(&Run::new(4, None, MONDAY + 4 * HOUR, 700));
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 1, "kind": "worker_question"}),
        MONDAY + HOUR + 10,
    );
    let now = MONDAY + DAY + HOUR;
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: vec![Target {
                kpi: "phase.work".into(),
                kind: None,
                change: Some("fix".into()),
                area: None,
                stat: None,
                min: None,
                max: Some(150.0),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let result = queue.kpi(now, &config, &KpiQuery::default());
    let monday = &result.periods[DEFAULT_LAST - 2];
    let landings = |stratum: &str| measure(monday, "landings", stratum).value;
    assert_eq!(landings("change=fix"), Some(2.0));
    assert_eq!(landings("change=feature"), Some(1.0));
    assert_eq!(landings("change=unknown"), Some(1.0));
    assert_eq!(landings(ALL), Some(4.0));
    assert_eq!(
        measure(monday, "phase.work", "change=fix").median,
        Some(200.0)
    );
    assert_eq!(
        measure(monday, "asks_per_landing", "change=fix").value,
        Some(0.5)
    );
    let target = &result.targets[0];
    assert_eq!(target.stratum, "change=fix");
    assert_eq!(target.periods[DEFAULT_LAST - 2].met, Some(false));

    let only = queue.kpi(
        now,
        &config,
        &KpiQuery {
            changes: vec!["feature".into()],
            ..KpiQuery::default()
        },
    );
    let strata: Vec<&String> = only.periods[DEFAULT_LAST - 2].window.kpis["landings"]
        .keys()
        .collect();
    assert!(strata.contains(&&"change=feature".to_owned()));
    assert!(strata.contains(&&"kind=unknown".to_owned()));
    assert!(!strata.contains(&&"change=fix".to_owned()));

    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY,
        MONDAY + 2 * HOUR,
        MONDAY + 2 * HOUR,
        MONDAY + DAY
    )
    .parse()
    .unwrap();
    let compare = queue
        .kpi(
            now,
            &config,
            &KpiQuery {
                compare: Some(spec),
                ..KpiQuery::default()
            },
        )
        .compare
        .unwrap();
    assert_eq!(
        compare.change_summary.keys().collect::<Vec<_>>(),
        ["feature", "fix", "unknown"]
    );
    let fixes = &compare.change_summary["fix"]["phase.work"];
    assert_eq!(
        (fixes.before.median, fixes.after.median),
        (Some(100.0), Some(300.0))
    );
    let asked = queue
        .kpi(
            now,
            &config,
            &KpiQuery {
                compare: Some(spec),
                changes: vec!["fix".into()],
                ..KpiQuery::default()
            },
        )
        .compare
        .unwrap();
    assert_eq!(asked.change_summary.keys().collect::<Vec<_>>(), ["fix"]);
    assert_eq!("change".parse::<Axis>(), Ok(Axis::Change));
}

/// A person's answers to the workers' questions per primary topic
/// (ADR-t947-2): `ask.worker_question_wait` by `topic=`, by when the ask
/// was opened (`at=night` from 22:00 to 07:00 of the host's day,
/// `at=day`), and `stats`' `worker_question_topics` in the details.
#[test]
fn the_worker_question_waits_are_split_by_topic_and_night() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY;
    for (ask, topics, opened, waited) in [
        (
            1,
            json!(["adr_conflict", "task_overlap"]),
            tuesday + 23 * HOUR,
            600,
        ),
        (2, json!(["adr_conflict"]), tuesday + 12 * HOUR, 120),
        (3, Value::Null, tuesday + 13 * HOUR, 60),
    ] {
        let mut payload = json!({"ask_id": ask, "kind": "worker_question",
            "reason_category": "scope"});
        if !topics.is_null() {
            payload["topics"] = topics;
        }
        queue.push(Some(1), Some("r1"), "ask_opened", payload, opened);
        queue.push(
            Some(1),
            Some("r1"),
            "ask_answered",
            json!({"ask_id": ask, "kind": "worker_question", "answered_by": "inbox"}),
            opened + waited,
        );
    }
    let query = KpiQuery {
        last: 3,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 2 * DAY + HOUR, &KpiConfig::default(), &query);
    let second = &result.periods[1];
    let wait = |stratum: &str| {
        let measure = measure(second, "ask.worker_question_wait", stratum);
        (measure.n, measure.max)
    };
    assert_eq!(wait(ALL), (3, Some(600.0)));
    assert_eq!(wait("topic=adr_conflict"), (2, Some(600.0)));
    assert_eq!(wait("topic=unlabeled"), (1, Some(60.0)));
    assert_eq!(wait("at=night"), (1, Some(600.0)));
    assert_eq!(wait("at=day"), (2, Some(120.0)));
    let details = &second.window.details["worker_question_topics"];
    assert_eq!(details["asks"], 3);
    assert_eq!(details["codes"]["task_overlap"], 1);
    assert_eq!(details["by_topic"]["adr_conflict"]["night"]["total"], 600);
    assert_eq!(
        measure(&result.periods[0], "ask.worker_question_wait", ALL).n,
        0
    );
}

/// The headless jobs per kind (goal 73), split by the provider they ran on
/// (`claude` for a start that named none) and the model their session
/// used: Claude's and Codex's goal reviews side by side, how many, the
/// share failed, how long they took and the share of each verdict.
#[test]
fn the_goal_reviews_of_claude_and_codex_compare_by_provider_and_model() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY + 10 * HOUR;
    let goal_event = |queue: &mut Queue, kind: &str, payload: Value, secs: i64| {
        queue.queue_event(kind, payload, secs);
        queue.events.last_mut().unwrap().goal_id = Some(GoalId::new(9));
    };
    let reviews = [
        // (id, provider, model, verdict or failed, seconds)
        (1, None, None, Some("achieved"), 100),
        (
            2,
            Some("claude"),
            Some("claude-opus-5-5"),
            Some("gaps"),
            300,
        ),
        (3, Some("codex"), Some("gpt-5.5"), Some("achieved"), 60),
        (4, Some("codex"), Some("gpt-5.5"), None, 20),
        (5, Some("codex"), None, Some("gaps"), 80),
    ];
    for (id, provider, model, verdict, secs) in reviews {
        let at = tuesday + id * HOUR;
        let session = format!("s-{id}");
        let mut started = json!({"goal_review_id": id, "session_id": session});
        if let Some(provider) = provider {
            started["launch"] = json!({"role": "goal_review", "provider": provider});
        }
        goal_event(&mut queue, "goal_review_started", started, at);
        match verdict {
            Some(verdict) => goal_event(
                &mut queue,
                "goal_review_finished",
                json!({"goal_review_id": id, "verdict": verdict, "duration_secs": secs}),
                at + secs,
            ),
            None => goal_event(
                &mut queue,
                "goal_review_failed",
                json!({"goal_review_id": id, "duration_secs": secs}),
                at + secs,
            ),
        }
        if let Some(model) = model {
            queue.queue_event(
                "session_closed",
                json!({"kind": "goal_review", "session_id": session, "model": model}),
                at + secs,
            );
        }
    }
    let kpi = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            ..KpiQuery::default()
        },
    );
    let day = &kpi.periods[0];
    let count = |stratum: &str| measure(day, "job.count.goal_review", stratum).value;
    assert_eq!(count(ALL), Some(5.0));
    assert_eq!(count("provider=claude"), Some(2.0));
    assert_eq!(count("provider=codex"), Some(3.0));
    assert_eq!(count("model=gpt-5.5"), Some(2.0));
    assert_eq!(count("model=unknown"), Some(2.0));
    let failed = |stratum: &str| {
        let measure = measure(day, "job.failed_rate.goal_review", stratum);
        (measure.n, measure.value)
    };
    assert_eq!(failed("provider=claude"), (2, Some(0.0)));
    assert_eq!(failed("provider=codex"), (3, Some(0.333)));
    let secs = |stratum: &str| {
        let measure = measure(day, "job.secs.goal_review", stratum);
        (measure.median, measure.max)
    };
    assert_eq!(secs("provider=claude"), (Some(200.0), Some(300.0)));
    assert_eq!(secs("provider=codex"), (Some(60.0), Some(80.0)));
    // The share of the jobs that gave a verdict.
    let verdict = |name: &str, stratum: &str| {
        let measure = measure(day, name, stratum);
        (measure.n, measure.value)
    };
    assert_eq!(
        verdict("job.verdict.goal_review.achieved", "provider=claude"),
        (2, Some(0.5))
    );
    assert_eq!(
        verdict("job.verdict.goal_review.achieved", "provider=codex"),
        (2, Some(0.5))
    );
    assert_eq!(
        verdict("job.verdict.goal_review.gaps", "model=claude-opus-5-5"),
        (1, Some(1.0))
    );
    // Every kind is listed, with no job too.
    assert_eq!(measure(day, "job.count.review", ALL).value, Some(0.0));
    assert_eq!(direction("job.count.goal_review"), None);
    assert_eq!(direction("job.verdict.goal_review.gaps"), None);
    assert_eq!(
        direction("job.failed_rate.goal_review"),
        Some(Direction::Lower)
    );
    let details = &day.window.details["jobs"]["goal_review"];
    assert_eq!(details["by_provider"]["codex"]["failed"], 1);
    assert_eq!(details["by_provider"]["claude"]["verdicts"]["gaps"], 1);
    // A goal counts its own goal reviews only.
    let other = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            goal_id: Some(GoalId::new(8)),
            ..KpiQuery::default()
        },
    );
    assert_eq!(
        measure(&other.periods[0], "job.count.goal_review", ALL).value,
        Some(0.0)
    );
}
