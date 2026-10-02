//! The cross strata (`--cross`).

use super::*;

#[test]
fn cross_strata_intersect_filters_and_axes_without_changing_existing_output() {
    let mut queue = Queue::default();
    let mut areas = crate::domain::areas::RunAreas::new();
    // Both windows have Codex headless, Claude headless and Claude interactive,
    // plus Claude headless runs excluded independently by area and change.
    for day in 0..2 {
        for (index, provider, route, area, change, work) in [
            (1, "codex", "headless", "runtime", "fix", 900),
            (2, "claude", "headless", "runtime", "fix", 100),
            (3, "claude", "interactive", "runtime", "fix", 300),
            (4, "claude", "headless", "docs", "fix", 700),
            (5, "claude", "headless", "runtime", "test", 500),
        ] {
            let mut run = Run::new(
                index + day * 10,
                Some(change.parse().unwrap()),
                MONDAY + day * DAY + index * HOUR,
                work + day * 20,
            );
            run.provider = provider;
            queue.run(&run);
            let claim = queue
                .events
                .iter_mut()
                .find(|event| {
                    event.run_id.as_ref() == Some(&run_id(&run)) && event.kind == "run_claimed"
                })
                .unwrap();
            claim.payload["worker_mode"] = json!(route);
            // Multi-area runs belong once to each matching area.
            areas.insert(run_id(&run), vec![area.into(), "shared".into()]);
        }
    }
    queue.areas = Some(areas);
    let mut query = KpiQuery {
        since: Some(Cursor::Time((MONDAY + DAY) * 1000)),
        until: Some(Cursor::Time((MONDAY + 2 * DAY) * 1000)),
        changes: vec!["fix".into()],
        areas: vec!["runtime".into(), "shared".into()],
        by: vec![Axis::Route, Axis::Provider, Axis::Provider],
        compare: Some(CompareSpec::Windows([
            (
                Cursor::Time(MONDAY * 1000),
                Cursor::Time((MONDAY + DAY) * 1000),
            ),
            (
                Cursor::Time((MONDAY + DAY) * 1000),
                Cursor::Time((MONDAY + 2 * DAY) * 1000),
            ),
        ])),
        ..KpiQuery::default()
    };
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: vec![Target {
                kpi: "phase.work".into(),
                change: None,
                area: Some("runtime".into()),
                stat: None,
                min: None,
                max: Some(150.0),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let now = MONDAY + 3 * DAY;
    let plain = queue.kpi(now, &config, &query);
    query.cross = true;
    let crossed = queue.kpi(now, &config, &query);
    let key = "cross:area=runtime|change=fix|provider=claude|route=headless";
    let work = measure(&crossed.periods[0], "phase.work", key);
    assert_eq!((work.n, work.median), (1, Some(120.0)));
    assert_eq!(
        measure(&crossed.periods[0], "landings", key).value,
        Some(1.0)
    );
    let shared = "cross:area=shared|change=fix|provider=claude|route=headless";
    assert_eq!(measure(&crossed.periods[0], "phase.work", shared).n, 2);
    assert_eq!(
        measure(&crossed.periods[0], "phase.work", "route=headless").n,
        4
    );
    let comparison = &crossed.compare.as_ref().unwrap().strata["phase.work"][key];
    assert_eq!(comparison.before.median, Some(100.0));
    assert_eq!(comparison.after.median, Some(120.0));
    assert_eq!(
        crossed.periods[0].comparison["phase.work"][key].delta,
        Some(20.0)
    );
    // Removing only the new keys restores the entire previous JSON, including
    // targets, summaries, queue-level KPIs and comparison defaults.
    fn remove_cross(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.retain(|key, _| !key.starts_with("cross:"));
                for child in map.values_mut() {
                    remove_cross(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    remove_cross(child);
                }
            }
            _ => {}
        }
    }
    let mut actual = serde_json::to_value(&crossed).unwrap();
    remove_cross(&mut actual);
    assert_eq!(actual, serde_json::to_value(&plain).unwrap());
    // No area map means no intersections requiring area, never invented unknowns.
    queue.areas = None;
    let missing = queue.kpi(now, &config, &query);
    assert!(
        !missing.periods[0].window.kpis["phase.work"]
            .keys()
            .any(|key| key.starts_with("cross:"))
    );
    // Without filters, only explicitly requested axes are crossed, in canonical order.
    query.areas.clear();
    query.changes.clear();
    let unfiltered = queue.kpi(now, &config, &query);
    assert_eq!(
        measure(
            &unfiltered.periods[0],
            "phase.work",
            "cross:provider=claude|route=headless"
        )
        .n,
        3
    );
    query.by = vec![Axis::Provider];
    let single = queue.kpi(now, &config, &query);
    assert!(
        !single.periods[0].window.kpis["phase.work"]
            .keys()
            .any(|key| key.starts_with("cross:"))
    );
}

#[test]
fn cross_names_preserve_unknown_values_and_escape_separators() {
    let mut queue = Queue::default();
    let mut run = Run::new(1, None, MONDAY + HOUR, 100);
    run.build = "b|model=x%3D";
    queue.run(&run);
    let mut query = KpiQuery {
        since: Some(Cursor::Time(MONDAY * 1000)),
        until: Some(Cursor::Time((MONDAY + DAY) * 1000)),
        cross: true,
        by: vec![Axis::Model, Axis::Build, Axis::Change],
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let result = queue.kpi(MONDAY + 2 * DAY, &config, &query);
    let key = "cross:build=b%7Cmodel%3Dx%253D|change=unknown|model=unknown";
    assert_eq!(measure(&result.periods[0], "landings", key).n, 1);
    // Toolchain is unavailable outside this source repository, unlike a missing value.
    queue.not_source = true;
    query.by.push(Axis::Toolchain);
    let result = queue.kpi(MONDAY + 2 * DAY, &config, &query);
    assert!(
        !result.periods[0].window.kpis["landings"]
            .keys()
            .any(|key| key.starts_with("cross:"))
    );
}
