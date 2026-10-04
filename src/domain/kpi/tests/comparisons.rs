//! Comparisons across marks and explicit windows (`--compare`).

use super::*;

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
            Some("runtime".parse::<TaskChange>().unwrap()),
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
            Some("runtime".parse::<TaskChange>().unwrap()),
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
        Some("docs".parse::<TaskChange>().unwrap()),
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
    // The derived parallel and the person's mark are one overlapping change
    // (two runs finished between them); the build is routine and in none.
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
    assert_eq!(compare.overlapping[1].len(), 2);
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
        "change=runtime",
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
    let docs = &work["change=docs"];
    assert_eq!((docs.before.n, docs.after.n), (0, 1));
    // A derived mark is named by its claim: the claim names its derived
    // parallel, one change with the later person's mark; the derived build
    // of the same claim is no part of it and sits after (ADR-t1381-1 (i)).
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
    let kinds: Vec<&str> = split.marks.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(kinds, ["derived:parallel", "mark_recorded"]);
    assert!(!split.separable);
    let confounders: Vec<(&str, &str)> = derived
        .confounders
        .iter()
        .map(|c| (c.position, c.mark.kind.as_str()))
        .collect();
    assert_eq!(
        confounders,
        [
            ("before", "mark_recorded"),
            ("before", "mark_recorded"),
            ("after", "derived:dagq_version"),
        ]
    );
    // The summary is the runtime runs' times only.
    let summary = &compare.change_summary["runtime"];
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

            changes: &queue.changes,
            areas: None,
            heartbeats: &queue.heartbeats,
            draft_origins: &queue.draft_origins,
            now: MONDAY + 2 * DAY,
            utc_offset_secs: 0,
            cores: None,
            dagq_source: true,
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

#[test]
fn a_comparison_s_text_reads_back_as_the_same_comparison() {
    for text in [
        "12",
        "2026-09-26T08:52:00.500Z",
        "1..5,6..@1790000000",
        "2026-09-01T00:00:00Z..2026-09-08T00:00:00Z,9..12",
    ] {
        let spec: CompareSpec = text.parse().unwrap();
        assert_eq!(spec.text().parse::<CompareSpec>(), Ok(spec), "{text}");
    }
}

/// A supervisor's start in the queue, handing off to `build` when `handoff`.
fn started(queue: &mut Queue, parallel: i64, build: &str, handoff: bool, secs: i64) {
    queue.queue_event(
        marks::SUPERVISOR_STARTED,
        json!({"supervisor": "s", "parallel": parallel, "dagq_version": build, "handoff": handoff}),
        secs,
    );
}

/// The id of the event of `kind` at `secs`, once sorted.
fn event_id(queue: &mut Queue, kind: &str, secs: i64) -> EventId {
    queue.sort();
    let at = marks::utc_text(secs * 1000);
    queue
        .events
        .iter()
        .find(|event| event.kind == kind && event.created_at == at)
        .unwrap()
        .id
}

fn positions(compare: &compare::Comparison) -> Vec<(&str, &str)> {
    compare
        .confounders
        .iter()
        .map(|c| (c.position, c.mark.kind.as_str()))
        .collect()
}

/// Routine marks (ADR-t1381-1) next to a person's mark, with no run
/// between: the person's mark is a change of its own, split at its time,
/// and the routine ones are confounders in no overlapping change. A claim
/// that derives only a new build names that mark alone.
#[test]
fn routine_marks_next_to_a_person_s_mark_are_confounders_only() {
    let mut queue = Queue::default();
    started(&mut queue, 3, "b1", false, MONDAY - HOUR);
    for index in 0..6 {
        queue.run(&Run::new(100 + index, None, MONDAY + index * HOUR, 1000));
    }
    let mark = MONDAY + 7 * HOUR + 600;
    started(&mut queue, 3, "b2", true, MONDAY + 7 * HOUR);
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "headless", "by": "human"}),
        mark,
    );
    queue.queue_event(
        marks::SUPERVISOR_STOPPED,
        json!({"supervisor": "s", "dagq_version": "b2"}),
        mark + 600,
    );
    started(&mut queue, 3, "b2", false, mark + 1200);
    // The first run after derives a build no start announced.
    let first_after = MONDAY + 8 * HOUR;
    for index in 0..6 {
        let mut run = Run::new(200 + index, None, first_after + index * HOUR, 600);
        run.build = "b3";
        queue.run(&run);
    }
    let query = KpiQuery {
        last: 1,
        compare: Some(CompareSpec::At(Cursor::Event(queue.mark_id("headless")))),
        window_days: 2,
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let compare = queue
        .kpi(MONDAY + 5 * DAY, &config, &query)
        .compare
        .unwrap();
    let split = compare.split.as_ref().unwrap();
    let labels: Vec<&str> = split.marks.iter().map(|m| m.label.as_str()).collect();
    assert_eq!((labels, split.separable), (vec!["headless"], true));
    assert_eq!(timestamp_millis(&compare.before.end), Some(mark * 1000));
    assert_eq!(timestamp_millis(&compare.after.start), Some(mark * 1000));
    assert_eq!((compare.before.runs, compare.after.runs), (6, 6));
    assert!(compare.overlapping.is_empty());
    assert_eq!(
        positions(&compare),
        [
            ("before", "supervisor_started"),
            ("before", "supervisor_started"),
            ("after", "supervisor_stopped"),
            ("after", "supervisor_started"),
            ("after", "derived:dagq_version"),
        ]
    );
    // The claim that derived only the build names that mark alone.
    let claim = event_id(&mut queue, "run_claimed", first_after);
    let derived = queue
        .kpi(
            MONDAY + 5 * DAY,
            &config,
            &KpiQuery {
                compare: Some(CompareSpec::At(Cursor::Event(claim))),
                ..query.clone()
            },
        )
        .compare
        .unwrap();
    let split = derived.split.unwrap();
    let kinds: Vec<&str> = split.marks.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(
        (kinds, split.separable),
        (vec!["derived:dagq_version"], true)
    );
    assert_eq!(
        timestamp_millis(&derived.after.start),
        Some(first_after * 1000)
    );
    assert!(
        derived
            .confounders
            .iter()
            .any(|c| c.position == "before" && c.mark.label == "headless")
    );
}

/// Marks that are not routine still join one change when fewer than
/// `min_samples` runs finish between them, counted across the routine
/// marks between; a start that changes `parallel` is not routine. Naming
/// a routine mark splits at it alone, the others its confounders.
#[test]
fn routine_marks_between_close_marks_leave_them_one_change() {
    let mut queue = Queue::default();
    started(&mut queue, 3, "b1", false, MONDAY - HOUR);
    for index in 0..6 {
        queue.run(&Run::new(100 + index, None, MONDAY + index * HOUR, 1000));
    }
    let handoff = MONDAY + 7 * HOUR;
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "a", "by": "human"}),
        handoff - 600,
    );
    started(&mut queue, 3, "b1", true, handoff);
    // Two runs between the person's mark and the change of `[run.env]`.
    for index in 0..2 {
        queue.run(&Run::new(
            200 + index,
            None,
            handoff + 600 + index * HOUR,
            600,
        ));
    }
    queue.queue_event(
        marks::RUN_ENV_CHANGED,
        json!({"changed": ["RUST_TEST_THREADS"]}),
        MONDAY + 10 * HOUR,
    );
    for index in 0..6 {
        queue.run(&Run::new(
            300 + index,
            None,
            MONDAY + (11 + index) * HOUR,
            600,
        ));
    }
    // A start at a new `parallel` and a person's mark, no run between.
    started(&mut queue, 4, "b1", true, MONDAY + 20 * HOUR);
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "c", "by": "human"}),
        MONDAY + 20 * HOUR + 600,
    );
    let query = KpiQuery {
        last: 1,
        compare: Some(CompareSpec::At(Cursor::Event(queue.mark_id("a")))),
        window_days: 2,
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let compare = queue
        .kpi(MONDAY + 5 * DAY, &config, &query)
        .compare
        .unwrap();
    let split = compare.split.as_ref().unwrap();
    let kinds: Vec<&str> = split.marks.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(kinds, ["mark_recorded", "run_env_changed"]);
    assert!(!split.separable);
    let groups: Vec<Vec<&str>> = compare
        .overlapping
        .iter()
        .map(|group| group.iter().map(|m| m.kind.as_str()).collect())
        .collect();
    assert_eq!(
        groups,
        [
            vec!["mark_recorded", "run_env_changed"],
            vec!["supervisor_started", "mark_recorded"],
        ]
    );
    assert!(
        compare
            .confounders
            .iter()
            .any(|c| c.position == "between" && c.mark.kind == "supervisor_started")
    );
    // Naming the routine handoff splits at it alone.
    let start = event_id(&mut queue, marks::SUPERVISOR_STARTED, handoff);
    let routine = queue
        .kpi(
            MONDAY + 5 * DAY,
            &config,
            &KpiQuery {
                compare: Some(CompareSpec::At(Cursor::Event(start))),
                ..query.clone()
            },
        )
        .compare
        .unwrap();
    let split = routine.split.as_ref().unwrap();
    assert_eq!(split.marks.len(), 1);
    assert_eq!(split.marks[0].id, Some(start));
    assert!(split.separable);
    assert_eq!(timestamp_millis(&routine.before.end), Some(handoff * 1000));
    assert_eq!(timestamp_millis(&routine.after.start), Some(handoff * 1000));
    let confounders = positions(&routine);
    assert!(confounders.contains(&("before", "mark_recorded")));
    assert!(confounders.contains(&("before", "supervisor_started")));
    assert!(confounders.contains(&("after", "run_env_changed")));
}

/// A routine mark neither chains the marks around it nor resets the count
/// of runs between them: three runs on each side of a routine start leave
/// the two person's marks apart at `min_samples` 5. The first start is a
/// change of its own.
#[test]
fn a_routine_mark_does_not_chain_the_marks_around_it() {
    let mark = |secs: i64, kind: &str, detail: Value| Mark {
        id: None,
        kind: kind.into(),
        at: marks::utc_text(secs * 1000),
        recorded_at: marks::utc_text(secs * 1000),
        label: format!("{kind} {secs}"),
        retracted_by: None,
        detail,
    };
    let start = json!({"parallel": 3, "dagq_version": "b1"});
    let marks = [
        mark(0, marks::SUPERVISOR_STARTED, start.clone()),
        mark(100, marks::MARK_RECORDED, Value::Null),
        mark(400, marks::SUPERVISOR_STARTED, start),
        mark(700, marks::MARK_RECORDED, Value::Null),
    ];
    let finishes: Vec<i64> = [10, 20, 30, 40, 50, 200, 250, 300, 500, 550, 600]
        .map(|s| s * 1000)
        .to_vec();
    let groups = compare::overlapping_groups(&marks, &finishes, 5);
    let at: Vec<Vec<&str>> = groups
        .iter()
        .map(|group| group.iter().map(|m| m.at.as_str()).collect())
        .collect();
    assert_eq!(
        at,
        [
            vec![marks[0].at.as_str()],
            vec![&marks[1].at],
            vec![&marks[3].at]
        ]
    );
    // At seven the marks that are not routine are one change, without the
    // routine start between them.
    let groups = compare::overlapping_groups(&marks, &finishes, 7);
    assert_eq!((groups.len(), groups[0].len()), (1, 3));
    assert!(groups[0].iter().all(|m| m.at != marks[2].at));
}
