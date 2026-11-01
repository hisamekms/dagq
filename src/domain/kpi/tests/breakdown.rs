//! The finished runs' time and slot time by tags from their ledger rows,
//! per stratum, and the unattributed share's KPI judged by a target.

use super::*;
use crate::domain::ledger::fold::{Interval, UNATTRIBUTED as UNATTRIBUTED_PHASE};
use crate::domain::ledger::testkit::run_row;
use crate::domain::run_phase::{Attempt, AttemptKind, Blocker, Holds};

fn interval(
    tags: Option<(Blocker, Holds, Option<Attempt>)>,
    start_secs: i64,
    end_secs: i64,
) -> Interval {
    Interval {
        phase: if tags.is_some() {
            "work".into()
        } else {
            UNATTRIBUTED_PHASE.into()
        },
        blocker: tags.as_ref().map(|tags| tags.0),
        holds: tags.as_ref().map(|tags| tags.1),
        attempt: tags.and_then(|tags| tags.2),
        start: start_secs * 1000,
        end: end_secs * 1000,
        cause: None,
        blocked_by: None,
        run_id: None,
    }
}

const fn attempt(kind: AttemptKind) -> Option<Attempt> {
    Some(Attempt { kind, n: 1 })
}

/// Each `all`, `change=` and `area=` stratum of a period has its runs'
/// time and slot time by blocker, holds and attempt kind, each axis adding
/// up to 100% with `unattributed`; a run without a row is in none.
/// `unattributed_ratio` is a KPI of every stratum and a target
/// judges it; `--change` keeps only those changes' breakdowns.
#[test]
fn each_stratum_splits_its_runs_time_and_slot_time_by_tags() {
    let mut queue = Queue::default();
    let runtime: TaskChange = "runtime".parse().unwrap();
    let docs: TaskChange = "docs".parse().unwrap();
    let runs = [
        Run::new(1, Some(runtime), MONDAY + HOUR, 100),
        Run::new(2, Some(docs), MONDAY + 2 * HOUR, 100),
        Run::new(3, None, MONDAY + 3 * HOUR, 100),
    ];
    for run in &runs {
        queue.run(run);
    }
    queue.areas = Some(HashMap::from([
        (run_id(&runs[0]), vec!["src".to_owned()]),
        (run_id(&runs[1]), vec!["docs".to_owned(), "src".to_owned()]),
    ]));
    // Run 1: 600 s of AI work in the worker slot, 100 s nobody recorded
    // and 300 s waiting its turn to land in the landing slot.
    queue.rows.push(run_row(
        run_id(&runs[0]).as_str(),
        Some(TaskId::new(1)),
        vec![
            interval(
                Some((Blocker::Ai, Holds::WorkerSlot, attempt(AttemptKind::First))),
                0,
                600,
            ),
            interval(None, 600, 700),
            interval(
                Some((
                    Blocker::Queue,
                    Holds::LandingSlot,
                    attempt(AttemptKind::First),
                )),
                700,
                1000,
            ),
        ],
    ));
    // Run 2: 700 s waiting for a person out of the slots, then 300 s of
    // the revise in the worker slot. Run 3 has no row.
    queue.rows.push(run_row(
        run_id(&runs[1]).as_str(),
        Some(TaskId::new(2)),
        vec![
            interval(Some((Blocker::Human, Holds::None, None)), 0, 700),
            interval(
                Some((Blocker::Ai, Holds::WorkerSlot, attempt(AttemptKind::Revise))),
                700,
                1000,
            ),
        ],
    ));
    let at_most_2 = || Target {
        kpi: UNATTRIBUTED_RATIO.into(),
        change: None,
        area: None,
        stat: None,
        min: None,
        max: Some(0.02),
    };
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: vec![at_most_2()],
            ..KpiSettings::default()
        }),
        None,
    );
    let now = MONDAY + DAY + HOUR;
    let result = queue.kpi(now, &config, &KpiQuery::default());
    let monday = &result.periods[DEFAULT_LAST - 2];
    let breakdown = &monday.window.breakdown;
    let all = &breakdown[ALL];
    assert_eq!((all.runs, all.legacy), (2, 0));
    assert_eq!(all.duration.secs, 2000.0);
    assert_eq!(all.duration.unattributed.secs, 100.0);
    assert_eq!(all.duration.by_blocker["ai"].secs, 900.0);
    assert_eq!(all.duration.by_blocker["human"].ratio, Some(0.35));
    assert_eq!(all.duration.by_attempt["revise"].secs, 300.0);
    assert_eq!(all.duration.by_attempt["none"].secs, 700.0);
    for split in [&all.duration, &all.slot] {
        for axis in [&split.by_blocker, &split.by_holds, &split.by_attempt] {
            let total = axis.values().filter_map(|share| share.ratio).sum::<f64>()
                + split.unattributed.ratio.unwrap();
            assert!((total - 1.0).abs() < 1e-9, "{axis:?}");
        }
    }
    // The slot time: the worker slot's 900 s and the landing slot's 300 s.
    assert_eq!(all.slot.secs, 1200.0);
    assert_eq!(all.slot.by_holds["worker_slot"].secs, 900.0);
    assert_eq!(all.slot.by_holds["landing_slot"].ratio, Some(0.25));
    // By change and area: a run in each of its areas, run 3 in none.
    let runs_of = |stratum: &str| breakdown[stratum].runs;
    assert_eq!(runs_of("change=runtime"), 1);
    assert_eq!(runs_of("change=docs"), 1);
    assert_eq!(runs_of("change=unknown"), 0);
    assert_eq!(runs_of("area=src"), 2);
    assert_eq!(runs_of("area=docs"), 1);
    assert_eq!(breakdown["change=runtime"].unattributed_ratio(), Some(0.1));
    assert_eq!(breakdown["area=docs"].unattributed_ratio(), Some(0.0));
    // Not split by the claim's attributes.
    assert!(breakdown.keys().all(|stratum| stratum == ALL
        || stratum.starts_with("change=")
        || stratum.starts_with("area=")));
    // The KPI in every stratum, null where no run has a row.
    let unattributed = |stratum: &str| measure(monday, UNATTRIBUTED_RATIO, stratum);
    assert_eq!(unattributed(ALL).value, Some(0.05));
    assert_eq!(unattributed(ALL).n, 2);
    assert_eq!(unattributed("change=runtime").value, Some(0.1));
    assert_eq!(unattributed("change=unknown").value, None);
    assert!(!monday.window.unavailable.contains_key(UNATTRIBUTED_RATIO));
    // The repository's target: 5% is over 2%.
    let target = result
        .targets
        .iter()
        .find(|target| target.kpi == UNATTRIBUTED_RATIO)
        .unwrap();
    assert_eq!(
        (target.stratum.as_str(), target.source, target.state),
        (ALL, "repository", "missed")
    );
    // Two runs are too few under the default `min_samples`: not judged.
    let few_config = KpiConfig::merge(
        Some(&KpiSettings {
            targets: vec![at_most_2()],
            ..KpiSettings::default()
        }),
        None,
    );
    let few = queue.kpi(now, &few_config, &KpiQuery::default());
    let target = few
        .targets
        .iter()
        .find(|target| target.kpi == UNATTRIBUTED_RATIO)
        .unwrap();
    assert_eq!(target.state, "not_judged");
    // `--change` keeps that change's breakdown, `all` and the areas'.
    let only = queue.kpi(
        now,
        &config,
        &KpiQuery {
            changes: vec!["runtime".into()],
            ..KpiQuery::default()
        },
    );
    let kept: Vec<&String> = only.periods[DEFAULT_LAST - 2]
        .window
        .breakdown
        .keys()
        .collect();
    assert_eq!(
        kept,
        [
            "all",
            "area=docs",
            "area=src",
            "area=unknown",
            "change=runtime"
        ]
    );
    // `--area` likewise.
    let only = queue.kpi(
        now,
        &config,
        &KpiQuery {
            areas: vec!["docs".into()],
            ..KpiQuery::default()
        },
    );
    let kept = &only.periods[DEFAULT_LAST - 2].window.breakdown;
    assert!(kept.contains_key("area=docs") && !kept.contains_key("area=src"));
}

/// Without a row with intervals the KPI is null, says why, and the target
/// is not judged.
#[test]
fn no_rows_leave_the_unattributed_share_unrecorded() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 100));
    let now = MONDAY + DAY + HOUR;
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(1),
            targets: vec![Target {
                kpi: UNATTRIBUTED_RATIO.into(),
                change: None,
                area: None,
                stat: None,
                min: None,
                max: Some(0.02),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let result = queue.kpi(now, &config, &KpiQuery::default());
    let monday = &result.periods[DEFAULT_LAST - 2];
    assert_eq!(measure(monday, UNATTRIBUTED_RATIO, ALL).value, None);
    assert_eq!(
        monday.window.unavailable[UNATTRIBUTED_RATIO],
        "no_intervals"
    );
    assert_eq!(monday.window.breakdown[ALL].runs, 0);
    let target = result
        .targets
        .iter()
        .find(|target| target.kpi == UNATTRIBUTED_RATIO)
        .unwrap();
    assert_eq!(target.state, "not_judged");
}
