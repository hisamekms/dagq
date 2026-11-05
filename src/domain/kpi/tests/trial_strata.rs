//! The worker model trial's strata and the Codex runs' model.

use super::*;

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
        by: vec![Axis::GROUP, Axis::MODEL, Axis::EFFORT, Axis::NATURE],
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
    // Without `--by`, only the changes and the areas.
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
            by: vec![Axis::GROUP],
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

/// Task 892: a run that worked on Codex (claimed or moved there) is counted
/// under the model Codex used, in no trial group and under no Claude
/// version; the Claude runs' strata stay.
#[test]
fn a_codex_run_is_not_counted_as_a_claude_model() {
    let mut queue = Queue::default();
    let mut claude = Run::new(1, None, MONDAY + HOUR, 100);
    claude.session = Some(("claude-opus-5-5", "medium", None));
    queue.run(&claude);
    let mut codex = Run::new(2, None, MONDAY + 2 * HOUR, 100);
    codex.provider = "codex";
    codex.session = Some(("claude-opus-5-5", "medium", None));
    queue.run(&codex);
    // A treatment run claimed on Claude that the fallback moved to Codex
    // worked on Codex too: it leaves the trial and the Claude strata, and
    // no Codex turn named its model.
    let mut moved = Run::new(3, None, MONDAY + 3 * HOUR, 100);
    moved.session = Some(("claude-sonnet-5", "medium", Some("treatment")));
    moved.switched_to = Some("codex");
    queue.run(&moved);
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec![Axis::MODEL, Axis::CLAUDE, Axis::GROUP, Axis::PROVIDER],
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let period = &result.periods[0];
    let landings = |stratum: &str| measure(period, "landings", stratum).value;
    assert_eq!(landings("model=claude-opus-5-5"), Some(1.0));
    assert_eq!(landings("model=gpt-6-astra"), Some(1.0));
    assert_eq!(landings("claude=2.1.0"), Some(1.0));
    assert_eq!(landings("claude=none"), Some(2.0));
    assert_eq!(landings("group=none"), Some(3.0));
    assert_eq!(landings("model=unknown"), Some(1.0));
    let strata = &period.window.kpis["landings"];
    assert!(!strata.contains_key("group=treatment"), "{strata:?}");
    assert!(!strata.contains_key("model=claude-sonnet-5"), "{strata:?}");
    assert_eq!(landings("provider=claude"), Some(1.0));
    assert_eq!(landings("provider=codex"), Some(2.0));
}

/// The provider pair trial's groups (ADR-t4118-1): `pair_group=` holds the
/// runs that ran as assigned, whatever the group's review mixes as configured;
/// `switched:pair_group=` those the records show moved, on either side, in
/// their assigned group even when they ended as the other group's pair, and
/// when the other side is unknown; `assigned:pair_group=` every run, with the
/// runs that cannot be judged, which only it holds and counts.
#[test]
fn the_pair_groups_split_into_the_primary_the_switched_and_the_assigned() {
    use crate::domain::ledger::provider_use::{ProviderUse, Waits};
    use std::collections::BTreeMap;
    let mut queue = Queue::default();
    let uses = |switches: &[(&str, i64)], waits: Option<(i64, i64)>| ProviderUse {
        worker_waits: Some(
            waits
                .map(|(count, millis)| {
                    BTreeMap::from([(
                        "usage_limit".to_owned(),
                        Waits {
                            count,
                            millis,
                            open: 0,
                        },
                    )])
                })
                .unwrap_or_default(),
        ),
        review_switches: Some(
            switches
                .iter()
                .map(|(reason, count)| ((*reason).to_owned(), *count))
                .collect(),
        ),
    };
    let fell_back = [("usage_limit", 1)];
    // (group, assigned worker, worker's end, its use of providers, sent back,
    // whether the worker's end is known)
    let runs = [
        // A, its review mixed as configured: ran as assigned.
        (
            ("control", "claude", "claude"),
            Some(uses(&[], None)),
            false,
            true,
        ),
        // A, a review job fell back for the usage limit.
        (
            ("control", "claude", "claude"),
            Some(uses(&fell_back, None)),
            true,
            true,
        ),
        // B, waited twice for Codex's limit: ran as assigned.
        (
            ("treatment", "codex", "codex"),
            Some(uses(&[], Some((2, 300_000)))),
            false,
            true,
        ),
        // B, its worker moved to Claude.
        (
            ("treatment", "codex", "claude"),
            Some(uses(&[], None)),
            false,
            true,
        ),
        // B, both sides moved: A's pair as it ran, and still B's.
        (
            ("treatment", "codex", "claude"),
            Some(uses(&fell_back, None)),
            false,
            true,
        ),
        // B, from a row without the record: cannot be judged.
        (("treatment", "codex", "codex"), None, false, true),
        // B, the worker's end unknown: cannot be judged.
        (
            ("treatment", "codex", "codex"),
            Some(uses(&[], None)),
            false,
            false,
        ),
        // B, its worker moved, the review unknown: switched, the worker's.
        (("treatment", "codex", "claude"), None, false, true),
        // B, a review job moved, the worker's end unknown: switched, the
        // review's.
        (
            ("treatment", "codex", "codex"),
            Some(uses(&fell_back, None)),
            false,
            false,
        ),
    ];
    for (index, (pair, used, revise, end_known)) in runs.into_iter().enumerate() {
        let index = index as i64;
        let mut run = Run::new(20 + index, None, MONDAY + HOUR * (index + 1), 100);
        run.revise = revise;
        let landed = queue.run(&run);
        let mut row = pair_row(&run, landed, pair, used);
        if !end_known {
            row.after_claim.as_mut().unwrap().last_provider = None;
        }
        queue.rows.push(row);
    }
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec!["pair_group".parse().unwrap()],
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let period = &result.periods[0];
    let value = |name: &str, stratum: &str| measure(period, name, stratum).value;
    for (stratum, landings) in [
        ("pair_group=control", 1.0),
        ("switched:pair_group=control", 1.0),
        ("assigned:pair_group=control", 2.0),
        ("pair_group=treatment", 1.0),
        ("switched:pair_group=treatment", 4.0),
        ("assigned:pair_group=treatment", 7.0),
    ] {
        assert_eq!(value("landings", stratum), Some(landings), "{stratum}");
        // The same items in each.
        for name in [
            trial::REVISES_PER_LANDING,
            trial::RUN_DURATION,
            trial::RUN_DURATION_WITHOUT_HUMAN,
            trial::REVERTS,
            trial::CI_FAILURES,
            "first_pass_rate",
            pair::SWITCHED_WORKER,
            pair::SWITCHED_REVIEW,
            pair::WORKER_TOKENS,
            pair::REVIEW_TOKENS,
            pair::WORKER_COST,
            pair::REVIEW_COST,
            pair::LIMIT_WAITS,
            pair::LIMIT_WAIT,
            pair::LIMIT_SWITCHES,
        ] {
            assert!(
                period.window.kpis[name].contains_key(stratum),
                "{name} {stratum}"
            );
        }
    }
    // The run KPIs follow each stratum's runs.
    assert_eq!(value("first_pass_rate", "pair_group=control"), Some(1.0));
    assert_eq!(
        value("first_pass_rate", "switched:pair_group=control"),
        Some(0.0)
    );
    assert_eq!(
        value("first_pass_rate", "assigned:pair_group=control"),
        Some(0.5)
    );
    assert_eq!(
        value("revises_per_landing", "assigned:pair_group=control"),
        Some(0.5)
    );
    let ran = measure(period, "run_duration", "pair_group=treatment");
    assert_eq!((ran.n, ran.median), (1, Some(200.0)));
    // The switched runs per group, the worker's and the review's apart: the
    // run that moved on both sides in each, none that cannot be judged.
    for (name, stratum, count) in [
        (pair::SWITCHED_WORKER, "switched:pair_group=treatment", 3.0),
        (pair::SWITCHED_REVIEW, "switched:pair_group=treatment", 2.0),
        (pair::SWITCHED_WORKER, "assigned:pair_group=treatment", 3.0),
        (pair::SWITCHED_REVIEW, "assigned:pair_group=treatment", 2.0),
        (pair::SWITCHED_WORKER, "pair_group=treatment", 0.0),
        (pair::SWITCHED_REVIEW, "pair_group=treatment", 0.0),
        (pair::SWITCHED_WORKER, "switched:pair_group=control", 0.0),
        (pair::SWITCHED_REVIEW, "switched:pair_group=control", 1.0),
        (pair::SWITCHED_REVIEW, "pair_group=control", 0.0),
    ] {
        assert_eq!(value(name, stratum), Some(count), "{name} {stratum}");
    }
    // The usage limit: B's waits and their time, A's review job's switch;
    // the row without the record is no sample.
    let waits = measure(period, pair::LIMIT_WAITS, "pair_group=treatment");
    assert_eq!((waits.n, waits.value), (1, Some(2.0)));
    let waits = measure(period, pair::LIMIT_WAITS, "assigned:pair_group=treatment");
    assert_eq!((waits.n, waits.value), (5, Some(2.0)));
    let waited = measure(period, pair::LIMIT_WAIT, "pair_group=treatment");
    assert_eq!((waited.n, waited.max), (1, Some(300.0)));
    let switches = measure(period, pair::LIMIT_SWITCHES, "switched:pair_group=control");
    assert_eq!((switches.n, switches.value), (1, Some(1.0)));
    let switches = measure(period, pair::LIMIT_SWITCHES, "assigned:pair_group=control");
    assert_eq!((switches.n, switches.value), (2, Some(1.0)));
    // The runs behind each, and those that cannot be judged only in the
    // secondary: not the switched run without the record.
    let details = &period.window.details["provider_pair"];
    for (stratum, runs, unrecorded) in [
        ("pair_group=treatment", 1, 0),
        ("switched:pair_group=treatment", 4, 0),
        ("assigned:pair_group=treatment", 7, 2),
        ("pair_group=control", 1, 0),
        ("switched:pair_group=control", 1, 0),
        ("assigned:pair_group=control", 2, 0),
    ] {
        assert_eq!(details[stratum]["runs"], runs, "{stratum}");
        assert_eq!(details[stratum]["unrecorded"], unrecorded, "{stratum}");
    }
    // Without `--by pair_group`, neither the groups nor the set.
    let plain = queue.kpi(
        MONDAY + DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            by: Vec::new(),
            ..query
        },
    );
    let window = &plain.periods[0].window;
    assert!(window.kpis.keys().all(|name| !name.starts_with("pair.")));
    assert!(
        window.kpis["landings"]
            .keys()
            .all(|stratum| !stratum.contains("pair_group"))
    );
    assert!(!window.details.contains_key("provider_pair"));
}

/// `--cross` keeps the meaning of the provider pair's strata: crossed by the
/// change, the primary's name holds only the run that ran as assigned, next
/// to the switched and the assigned.
#[test]
fn the_pair_groups_cross_by_the_same_strata() {
    use crate::domain::ledger::provider_use::ProviderUse;
    let mut queue = Queue::default();
    let recorded = ProviderUse {
        worker_waits: Some(Default::default()),
        review_switches: Some(Default::default()),
    };
    let runs = [
        ("codex", Some(recorded.clone())),
        ("claude", Some(recorded)),
        ("codex", None),
    ];
    for (index, (ran_on, uses)) in runs.into_iter().enumerate() {
        let task = index as i64 + 1;
        let change = Some("fix".parse::<TaskChange>().unwrap());
        let run = Run::new(task, change, MONDAY + HOUR * task, 100);
        let landed = queue.run(&run);
        queue
            .rows
            .push(pair_row(&run, landed, ("treatment", "codex", ran_on), uses));
    }
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec![Axis::CHANGE, "pair_group".parse().unwrap()],
        cross: true,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let period = &result.periods[0];
    for (stratum, landings) in [
        ("cross:change=fix|pair_group=treatment", 1.0),
        ("cross:change=fix|switched:pair_group=treatment", 1.0),
        ("cross:change=fix|assigned:pair_group=treatment", 3.0),
    ] {
        assert_eq!(
            measure(period, "landings", stratum).value,
            Some(landings),
            "{stratum}"
        );
    }
}

/// A run that did not land is a cost of its group too: a treatment run
/// that used Codex tokens, waited for the usage limit, had a review job
/// fall back and its worker move to Claude, and then failed, counts its
/// tokens, cost, waits and switches in its group's switched and assigned
/// strata, the latter next to a landed run of the group; the landings' set
/// stays the landed run's.
#[test]
fn a_run_that_failed_counts_its_tokens_waits_and_switches_in_its_group() {
    use crate::domain::agent_eval::round::ProviderCost;
    use crate::domain::ledger::provider_use::{ProviderUse, Waits};
    use std::collections::BTreeMap;
    let mut queue = Queue::default();
    let landed_run = Run::new(30, None, MONDAY + HOUR, 100);
    let landed = queue.run(&landed_run);
    queue.rows.push(pair_row(
        &landed_run,
        landed,
        ("treatment", "codex", "codex"),
        Some(ProviderUse {
            worker_waits: Some(BTreeMap::new()),
            review_switches: Some(BTreeMap::new()),
        }),
    ));
    let mut failed = Run::new(31, None, MONDAY + 2 * HOUR, 100);
    failed.failed = true;
    let ended = queue.run(&failed);
    let id = run_id(&failed);
    queue.push(
        Some(31),
        Some(id.as_str()),
        "turn_finished",
        json!({"provider": "codex", "tokens": {"input": 0, "output": 1_000_000, "cache_read": 0,
               "cache_creation": 0}, "tokens_source": "token_usage_record", "tokens_by_model": []}),
        failed.claimed + 50,
    );
    queue.rows.push(pair_row(
        &failed,
        ended,
        ("treatment", "codex", "claude"),
        Some(ProviderUse {
            worker_waits: Some(BTreeMap::from([(
                "usage_limit".to_owned(),
                Waits {
                    count: 1,
                    millis: 60_000,
                    open: 0,
                },
            )])),
            review_switches: Some(BTreeMap::from([("usage_limit".to_owned(), 1)])),
        }),
    ));
    let config = KpiConfig {
        codex_prices: Some(ProviderCost {
            input_usd_per_mtok: Some(2.0),
            output_usd_per_mtok: Some(10.0),
            ..ProviderCost::default()
        }),
        ..KpiConfig::default()
    };
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec!["pair_group".parse().unwrap()],
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &config, &query);
    let period = &result.periods[0];
    let value = |name: &str| {
        let measure = measure(period, name, "assigned:pair_group=treatment");
        (measure.n, measure.value)
    };
    assert_eq!(value("landings"), (1, Some(1.0)));
    assert_eq!(value(pair::WORKER_TOKENS), (1, Some(1_000_000.0)));
    assert_eq!(value(pair::WORKER_COST), (1, Some(10.0)));
    assert_eq!(value(pair::LIMIT_WAITS), (2, Some(1.0)));
    assert_eq!(value(pair::LIMIT_SWITCHES), (2, Some(1.0)));
    assert_eq!(value(pair::SWITCHED_WORKER).1, Some(1.0));
    assert_eq!(value(pair::SWITCHED_REVIEW).1, Some(1.0));
    let waited = measure(period, pair::LIMIT_WAIT, "assigned:pair_group=treatment");
    assert_eq!((waited.n, waited.max), (2, Some(60.0)));
    let details = &period.window.details["provider_pair"]["assigned:pair_group=treatment"];
    assert_eq!(details["runs"], 2);
    assert_eq!(details["worker"]["without_tokens"], 1);
    // The landings' set is the landed run's.
    assert_eq!(
        measure(period, "run_duration", "assigned:pair_group=treatment").n,
        1
    );
    // The primary holds only the landed run, which ran as assigned; the
    // switched stratum only the failed run and its cost.
    let details = &period.window.details["provider_pair"];
    assert_eq!(details["pair_group=treatment"]["runs"], 1);
    assert_eq!(details["switched:pair_group=treatment"]["runs"], 1);
    let cost = measure(period, pair::WORKER_COST, "switched:pair_group=treatment");
    assert_eq!((cost.n, cost.value), (1, Some(10.0)));
    assert_eq!(
        measure(period, pair::WORKER_COST, "pair_group=treatment").value,
        None
    );
}
