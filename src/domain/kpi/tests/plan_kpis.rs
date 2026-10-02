//! The KPIs of the plans: plan quality, drafts, follow-ups and the forecast.

use super::*;

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
        Some("runtime".parse::<TaskChange>().unwrap()),
        tuesday + 3 * HOUR,
        600,
    );
    revised.revise = true;
    queue.run(&revised);
    queue.run(&Run::new(
        10,
        Some("runtime".parse::<TaskChange>().unwrap()),
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
/// split by target, change, band, method and whether a change mark came
/// between; a canceled task is only counted.
#[test]
fn scores_the_forecast_snapshots_in_the_period_they_finished() {
    let mut queue = Queue::default();
    let runtime = Some("runtime".parse::<TaskChange>().unwrap());
    let docs = Some("docs".parse::<TaskChange>().unwrap());
    queue.changes.insert(TaskId::new(1), runtime);
    queue.changes.insert(TaskId::new(2), docs);
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
    assert_eq!(measure(monday, "forecast.p50_error", "change=runtime").n, 2);
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
    let error = measure(tuesday, "forecast.p50_abs_error", "change=docs");
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
