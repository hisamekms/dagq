//! The strata of the runs: change, claim attributes, area and toolchain.

use super::*;

/// Each run falls in the day it finished; its KPIs are split by the task's
/// change (a task without one is `unknown`) and the claim's attributes, and
/// each day sits next to the previous one.
#[test]
fn splits_the_runs_by_change_and_attributes_per_day() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY;
    let mut runs = Vec::new();
    for (index, change) in [
        Some("runtime".parse::<TaskChange>().unwrap()),
        Some("runtime".parse::<TaskChange>().unwrap()),
        Some("docs".parse::<TaskChange>().unwrap()),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let index = index as i64;
        let mut run = Run::new(
            10 + index,
            change,
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
        Some("runtime".parse::<TaskChange>().unwrap()),
        MONDAY + HOUR,
        300,
    ));
    let mut failed = Run::new(
        2,
        Some("runtime".parse::<TaskChange>().unwrap()),
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
        measure(tuesday, "landings", "change=runtime").value,
        Some(2.0)
    );
    assert_eq!(measure(tuesday, "landings", "change=docs").value, Some(1.0));
    assert_eq!(
        measure(tuesday, "landings", "change=unknown").value,
        Some(1.0)
    );
    let work = measure(tuesday, "phase.work", "change=runtime");
    assert_eq!(
        (work.n, work.median, work.max),
        (2, Some(900.0), Some(1200.0))
    );
    assert_eq!(measure(tuesday, "phase.startup", ALL).median, Some(50.0));
    // Ready an hour before the claim, landed 120 s after the receipt.
    assert_eq!(
        measure(tuesday, "lead_time", "change=docs").median,
        Some(3600.0 + 1800.0 + 100.0)
    );
    assert_eq!(
        measure(tuesday, "revise_rate", "change=runtime").value,
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
        tuesday.comparison["landings"]["change=docs"].reason,
        Some("no_value")
    );
    // The 7 days before Tuesday: six empty days and Monday.
    assert_eq!(landings.baseline_7d, Some(0.0));
    // Listing only the docs strata leaves the others and `all`.
    let docs = queue.kpi(
        tuesday.end_ms() / 1000 + HOUR,
        &config,
        &KpiQuery {
            changes: vec!["docs".into()],
            ..query.clone()
        },
    );
    let strata: Vec<&String> = docs.periods[1].window.kpis["landings"].keys().collect();
    assert!(strata.contains(&&"change=docs".to_owned()) && strata.contains(&&ALL.to_owned()));
    assert!(!strata.contains(&&"change=runtime".to_owned()));
}

/// The runs split by `rustc` (`--by toolchain`) in dagq's source; outside
/// it the cargo-only axis has no stratum (ADR-t614-1).
#[test]
fn the_toolchain_axis_splits_only_dagqs_source() {
    let mut queue = Queue::default();
    let mut old = Run::new(1, None, MONDAY + HOUR, 100);
    old.rustc = Some("1.90.0");
    queue.run(&old);
    let mut new = Run::new(2, None, MONDAY + 2 * HOUR, 100);
    new.rustc = Some("1.91.0");
    queue.run(&new);
    queue.run(&Run::new(3, None, MONDAY + 3 * HOUR, 100));
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec![Axis::Toolchain],
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let landings = |result: &Kpi, stratum: &str| {
        result.periods[0].window.kpis["landings"]
            .get(stratum)
            .and_then(|measure| measure.value)
    };
    assert_eq!(
        landings(&result, "toolchain=1.90.0 aarch64-apple-darwin"),
        Some(1.0)
    );
    assert_eq!(
        landings(&result, "toolchain=1.91.0 aarch64-apple-darwin"),
        Some(1.0)
    );
    assert_eq!(landings(&result, "toolchain=unknown"), Some(1.0));
    queue.not_source = true;
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let strata = &result.periods[0].window.kpis["landings"];
    assert!(
        strata
            .keys()
            .all(|stratum| !stratum.starts_with("toolchain=")),
        "{strata:?}"
    );
    assert_eq!(landings(&result, "all"), Some(3.0));
}

/// The areas (ADR-t980-1): a run counts in every stratum of its areas and
/// in `area=unknown` without one; `--area` keeps only those areas' strata;
/// a comparison summarizes per area; a target can bound an area. Without
/// `[areas]` no `area=` stratum is listed.
#[test]
fn splits_the_landed_runs_by_their_areas() {
    let mut queue = Queue::default();
    let runtime: TaskChange = "runtime".parse().unwrap();
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
    assert!(strata.contains(&&"change=runtime".to_owned()));
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
    assert!(strata.contains(&&ALL.to_owned()));
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
