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
        by: vec![Axis::Model, Axis::Claude, Axis::Group, Axis::Provider],
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
