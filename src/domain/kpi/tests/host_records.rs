//! The KPIs read from the host's records: load, CPU per landing, health and disk.

use super::*;

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

/// Each period carries its health (task 1371): the workers' turns, nudges,
/// stalls and Claude's cost per route from that period's events only, and
/// the least and the median free space of the runs' filesystem when the
/// host's records are read; nothing is judged on it.
#[test]
fn periods_carry_the_health_per_route_and_the_disk_free() {
    use crate::domain::host_metrics::{DiskSpace, HostSample, summarize};
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    queue.run(&Run {
        provider: "codex",
        ..Run::new(2, None, MONDAY + DAY + HOUR, 300)
    });
    let run = |task: i64, claimed: i64| format!("{task:08x}-0000-4000-8000-{claimed:012x}");
    let (first, second) = (run(1, MONDAY + HOUR), run(2, MONDAY + DAY + HOUR));
    queue.push(
        Some(1),
        Some(&first),
        "stall_nudged",
        json!({"phase": "session"}),
        MONDAY + HOUR + 60,
    );
    queue.push(
        Some(2),
        Some(&second),
        "turn_finished",
        json!({"outcome": "failed", "failure": "usage_limit", "provider": "claude", "cost_usd": 0.5}),
        MONDAY + DAY + HOUR + 60,
    );
    queue.push(
        Some(2),
        Some(&second),
        "recovery_requested",
        json!({"alert": "stalled", "reason": "turn_without_receipt"}),
        MONDAY + DAY + HOUR + 120,
    );
    let gib = 1 << 30;
    let samples: Vec<HostSample> = [(MONDAY + 10, 50), (MONDAY + 20, 5), (MONDAY + 30, 20)]
        .into_iter()
        .map(|(unix, free)| {
            HostSample::new(unix).with_disk(Some(DiskSpace {
                free_bytes: free * gib,
                total_bytes: 100 * gib,
            }))
        })
        .collect();
    let read = |from, until| summarize(&samples, from, until);
    let config = KpiConfig::merge(None, None);
    let now = MONDAY + DAY + 2 * HOUR;
    let query = KpiQuery {
        period: Period::Day,
        last: 2,
        ..KpiQuery::default()
    };
    let kpi = queue.kpi_with_host(now, &config, &query, Some(HostReader(&read)));
    let [monday, tuesday] = [&kpi.periods[0], &kpi.periods[1]];
    assert_eq!(monday.health.routes["interactive"].stall_nudged, 1);
    assert!(!monday.health.routes.contains_key("headless"));
    let disk = monday.health.disk.unwrap();
    assert_eq!(
        (disk.samples, disk.min_free_bytes, disk.median_free_bytes),
        (3, 5_368_709_120.0, 21_474_836_480.0)
    );
    assert_eq!(
        (disk.min_free_pct, disk.median_free_pct),
        (Some(5.0), Some(20.0))
    );
    let headless = &tuesday.health.routes["headless"];
    assert_eq!(
        (headless.turns, headless.turn_failures["usage_limit"]),
        (1, 1)
    );
    assert_eq!(headless.stalled["turn_without_receipt"], 1);
    assert_eq!(headless.claude_cost_usd.total, 0.5);
    // A period without the host's records has no disk.
    assert_eq!(tuesday.health.disk, None);
    let json = serde_json::to_value(&kpi).unwrap();
    assert_eq!(
        json["periods"][0]["health"]["disk"]["min_free_bytes"],
        5_368_709_120.0
    );
    assert_eq!(
        json["periods"][1]["health"]["routes"]["headless"]["turn_outcomes"]["failed"],
        1
    );
    // Without the host's load, the routes stay and the disk is null.
    let without = queue.kpi(now, &config, &query);
    assert_eq!(without.periods[1].health.routes, tuesday.health.routes);
    assert_eq!(without.periods[0].health.disk, None);
    assert_eq!(kpi.targets, without.targets);
}
