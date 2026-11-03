//! The set a model and effort trial's groups are compared by
//! (ADR-t3957-1 decision 5): revises, the runs' time with and without a
//! person's answers, and the reverts and CI failures after the landings,
//! per `--by` stratum.

use super::*;
use crate::domain::ledger::fold::Interval;
use crate::domain::ledger::testkit::run_row;
use crate::domain::run_phase::{Blocker, Holds};

const OPUS: (&str, &str, Option<&str>) = ("claude-opus-5-5", "medium", Some("control"));
const SONNET: (&str, &str, Option<&str>) = ("claude-sonnet-5", "medium", Some("treatment"));

/// Runs `run`, landing `commit` on `main`; returns the landing time.
fn land(queue: &mut Queue, run: &Run, commit: &str) -> i64 {
    let landed = queue.run(run);
    let id = run_id(run);
    for event in queue.events.iter_mut().rev() {
        if event.kind.as_str() == "run_integrated" && event.run_id.as_ref() == Some(&id) {
            event.payload["result_commit"] = json!(commit);
            break;
        }
    }
    queue.push(
        Some(run.task),
        Some(id.as_str()),
        "push_finished",
        json!({"branch": "main"}),
        landed + 1,
    );
    landed
}

fn interval(blocker: Blocker, start_secs: i64, end_secs: i64) -> Interval {
    Interval {
        phase: "work".into(),
        blocker: Some(blocker),
        holds: Some(Holds::WorkerSlot),
        attempt: None,
        start: start_secs * 1000,
        end: end_secs * 1000,
        cause: None,
        blocked_by: None,
        run_id: None,
    }
}

/// main's history from `since` (unix seconds), with the evidence of the
/// reverts, recorded and reached at `at`.
fn main_recorded(queue: &mut Queue, commits: Value, since: i64, at: i64) {
    queue.queue_event(
        "main_commits_recorded",
        json!({"after": null, "commits": commits}),
        at,
    );
    queue.queue_event(
        "main_observed",
        json!({"head": "r", "at": at, "since": since, "reverts": true}),
        at,
    );
}

fn query() -> KpiQuery {
    KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec![Axis::GROUP, Axis::MODEL, Axis::EFFORT],
        ..KpiQuery::default()
    }
}

/// Per group, model and effort: the revises per landing, the runs' time
/// and the time without the person's answers from their ledger rows, and
/// the reverts and CI failures matched to the landed commits by the common
/// read, as counts and rates of the runs the record covers, with how long
/// after the period's end they were read and how far the record reached.
#[test]
fn the_groups_compare_revises_time_and_the_quality_after_landing() {
    let mut queue = Queue::default();
    let mut control = Run::new(1, None, MONDAY + HOUR, 100);
    control.session = Some(OPUS);
    let mut sent_back = Run::new(2, None, MONDAY + 2 * HOUR, 100);
    sent_back.session = Some(SONNET);
    sent_back.revise = true;
    let mut treatment = Run::new(3, None, MONDAY + 3 * HOUR, 100);
    treatment.session = Some(SONNET);
    land(&mut queue, &control, "c1");
    land(&mut queue, &sent_back, "c2");
    let last = land(&mut queue, &treatment, "c3");
    // Run 1 waited 400 s of its 1000 s for a person; run 2 had no wait.
    queue.rows.push(run_row(
        run_id(&control).as_str(),
        Some(TaskId::new(1)),
        vec![
            interval(Blocker::Ai, 0, 600),
            interval(Blocker::Human, 600, 1000),
        ],
    ));
    queue.rows.push(run_row(
        run_id(&sent_back).as_str(),
        Some(TaskId::new(2)),
        vec![interval(Blocker::Ai, 0, 300)],
    ));
    // The record ran from before the landings: run 2's commit was
    // reverted, and CI on main turned red on run 3's.
    let reached = last + 600;
    main_recorded(
        &mut queue,
        json!([
            {"sha": "c1", "at": MONDAY + HOUR + 200, "reverts": null},
            {"sha": "c2", "at": MONDAY + 2 * HOUR + 200, "reverts": null},
            {"sha": "c3", "at": last, "reverts": null},
            {"sha": "r", "at": last + 300, "reverts": "c2"},
        ]),
        MONDAY,
        reached,
    );
    queue.queue_event("ci_checked", json!({"branch": "main"}), MONDAY);
    queue.queue_event(
        "ci_turned_red",
        json!({"sha": "c3", "branch": "main"}),
        last + 400,
    );
    queue.queue_event("ci_checked", json!({"branch": "main"}), reached);
    let now = MONDAY + DAY + HOUR;
    let result = queue.kpi(now, &KpiConfig::default(), &query());
    let period = &result.periods[0];
    let value = |name: &str, stratum: &str| measure(period, name, stratum).value;
    assert_eq!(value("revises_per_landing", "group=treatment"), Some(0.5));
    assert_eq!(value("revises_per_landing", "group=control"), Some(0.0));
    assert_eq!(value("first_pass_rate", "group=treatment"), Some(0.5));
    // The time with and without the person's answers; run 3 has no row.
    let control_time = measure(period, "run_duration", "group=control");
    assert_eq!((control_time.n, control_time.median), (1, Some(1000.0)));
    assert_eq!(
        measure(period, "run_duration_without_human", "group=control").median,
        Some(600.0)
    );
    assert_eq!(
        measure(
            period,
            "run_duration_without_human",
            "model=claude-sonnet-5"
        )
        .median,
        Some(300.0)
    );
    // The late quality: counts and rates over the covered runs.
    assert_eq!(value("post_landing.reverts", "group=treatment"), Some(1.0));
    assert_eq!(
        value("post_landing.revert_rate", "group=treatment"),
        Some(0.5)
    );
    assert_eq!(value("post_landing.reverts", "group=control"), Some(0.0));
    assert_eq!(
        value("post_landing.revert_rate", "group=control"),
        Some(0.0)
    );
    assert_eq!(
        value("post_landing.ci_failures", "group=treatment"),
        Some(1.0)
    );
    assert_eq!(
        value("post_landing.ci_failure_rate", "effort=medium"),
        Some(0.333)
    );
    assert_eq!(measure(period, "post_landing.revert_rate", ALL).n, 3);
    assert!(
        !period
            .window
            .unavailable
            .contains_key("post_landing.reverts")
    );
    // How the record stands: read at now, a day after the period's start.
    let arrival = &period.window.details["post_landing"]["group=treatment"]["reverts"];
    assert_eq!(arrival["landed"], 2);
    assert_eq!(arrival["recorded"], 2);
    assert_eq!(arrival["read_at"], now * 1000);
    assert_eq!(arrival["after_period_ms"], HOUR * 1000);
    assert_eq!(arrival["reach_at"], reached * 1000);
    assert_eq!(arrival["reach_before_end_ms"], (now - reached) * 1000);
    assert!(arrival.get("reason").is_none());
}

/// A period whose runs landed before the record began has no value but
/// the reason, in `unavailable` and next to the stratum's arrival; one
/// with no record says so too.
#[test]
fn a_window_before_the_record_gives_the_reason_not_a_value() {
    let mut queue = Queue::default();
    let mut run = Run::new(1, None, MONDAY + HOUR, 100);
    run.session = Some(SONNET);
    let landed = land(&mut queue, &run, "c1");
    let now = MONDAY + DAY + HOUR;
    // No record of main or CI at all.
    let unrecorded = queue.kpi(now, &KpiConfig::default(), &query());
    let period = &unrecorded.periods[0];
    assert_eq!(measure(period, "post_landing.reverts", ALL).value, None);
    assert_eq!(
        period.window.unavailable["post_landing.revert_rate"],
        "not_recorded"
    );
    // The records begin after the landing.
    main_recorded(
        &mut queue,
        json!([{"sha": "r", "at": landed + 100, "reverts": null}]),
        landed + 50,
        landed + 100,
    );
    queue.queue_event("ci_checked", json!({"branch": "main"}), landed + 50);
    let result = queue.kpi(now, &KpiConfig::default(), &query());
    let period = &result.periods[0];
    for name in [
        "post_landing.reverts",
        "post_landing.revert_rate",
        "post_landing.ci_failures",
        "post_landing.ci_failure_rate",
    ] {
        let late = measure(period, name, "group=treatment");
        assert_eq!((late.n, late.value), (0, None), "{name}");
        assert_eq!(period.window.unavailable[name], "before_record", "{name}");
    }
    let arrival = &period.window.details["post_landing"]["group=treatment"]["ci_failures"];
    assert_eq!(arrival["before_record"], 1);
    assert_eq!(arrival["reason"], "before_record");
    // The run KPIs of the same runs keep their values.
    assert_eq!(
        measure(period, "landings", "group=treatment").value,
        Some(1.0)
    );
    assert_eq!(
        measure(period, "revises_per_landing", "group=treatment").value,
        Some(0.0)
    );
}

/// The tokens of a run's Executions in its claim's group and effort, next
/// to the tokens' own strata (the Execution's model, not the claim's),
/// and per landing over the group's own landings.
#[test]
fn the_groups_compare_the_tokens_of_their_runs() {
    let mut queue = Queue::default();
    let usage = |input: i64, output: i64, model: &str| {
        json!({"provider": "claude", "tokens_source": "model_usage",
               "tokens": {"input": input, "output": output, "cache_read": 0,
                          "cache_creation": 0, "messages": 1},
               "tokens_by_model": [{"model": model, "input": input, "output": output}]})
    };
    // The records begin before the period.
    queue.queue_event(
        "observe_finished",
        json!({"outcome": "succeeded", "tokens_source": "model_usage", "tokens_by_model": [],
               "tokens": {"input": 1, "output": 0, "cache_read": 0, "cache_creation": 0,
                          "messages": 1}}),
        MONDAY - HOUR,
    );
    let mut control = Run::new(1, None, MONDAY + HOUR, 600);
    control.session = Some(OPUS);
    let mut treatment = Run::new(2, None, MONDAY + 2 * HOUR, 600);
    treatment.session = Some(SONNET);
    for (run, input, model) in [(&control, 100, "opus"), (&treatment, 20, "sonnet")] {
        queue.run(run);
        queue.push(
            Some(run.task),
            Some(run_id(run).as_str()),
            "turn_finished",
            usage(input, input / 10, model),
            run.claimed + 300,
        );
    }
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query());
    let period = &result.periods[0];
    let value = |name: &str, stratum: &str| measure(period, name, stratum).value;
    assert_eq!(value("tokens", "group=control"), Some(110.0));
    assert_eq!(value("tokens", "group=treatment"), Some(22.0));
    assert_eq!(value("tokens", "effort=medium"), Some(132.0));
    assert_eq!(value("tokens", ALL), Some(132.0));
    assert_eq!(value("tokens_per_landing", "group=treatment"), Some(22.0));
    assert_eq!(value("tokens_per_landing", "effort=medium"), Some(66.0));
    // The tokens' `model=` stays the Execution's.
    assert_eq!(value("tokens", "model=opus"), Some(110.0));
    assert!(
        !period.window.kpis["tokens"].contains_key("model=claude-opus-5-5"),
        "{:?}",
        period.window.kpis["tokens"].keys()
    );
}

/// The tokens of a run still in flight (no end recorded) fall in the group
/// and effort its claim recorded, and stay there once it ends.
#[test]
fn the_tokens_of_a_run_in_flight_fall_in_its_claims_group() {
    let mut queue = Queue::default();
    queue.queue_event(
        "observe_finished",
        json!({"outcome": "succeeded", "tokens_source": "model_usage", "tokens_by_model": [],
               "tokens": {"input": 1, "output": 0, "cache_read": 0, "cache_creation": 0,
                          "messages": 1}}),
        MONDAY - HOUR,
    );
    let claimed = MONDAY + HOUR;
    let run = format!("{:08x}-0000-4000-8000-{:012x}", 7, claimed);
    queue.changes.insert(TaskId::new(7), None);
    queue.goals.insert(TaskId::new(7), None);
    queue.push(
        Some(7),
        None,
        "task_weight_predicted",
        json!({"prediction": {"nature": "mechanical", "expected_output_tokens": 1000}}),
        claimed - HOUR,
    );
    // A claim as the runtime records it: its fields and their attributes.
    let mut claim = json!({"provider": "claude", "worker_mode": "headless",
                           "model": SONNET.0, "effort": SONNET.1, "group": SONNET.2});
    let attributes = crate::domain::claim_facts::attributes(claim.as_object().unwrap());
    claim[crate::domain::claim_facts::ATTRIBUTES] = json!(attributes);
    queue.push(Some(7), Some(run.as_str()), "run_claimed", claim, claimed);
    queue.push(
        Some(7),
        Some(run.as_str()),
        "turn_finished",
        json!({"provider": "claude", "tokens_source": "model_usage",
               "tokens": {"input": 20, "output": 2, "cache_read": 0, "cache_creation": 0,
                          "messages": 1},
               "tokens_by_model": [{"model": "sonnet", "input": 20, "output": 2}]}),
        claimed + 300,
    );
    let query = KpiQuery {
        by: vec![Axis::GROUP, Axis::EFFORT, Axis::NATURE],
        ..query()
    };
    let now = MONDAY + DAY + HOUR;
    let tokens = |queue: &mut Queue, stratum: &str| {
        let result = queue.kpi(now, &KpiConfig::default(), &query);
        result.periods[0].window.kpis["tokens"]
            .get(stratum)
            .and_then(|measure| measure.value)
    };
    assert_eq!(tokens(&mut queue, "group=treatment"), Some(22.0));
    assert_eq!(tokens(&mut queue, "effort=medium"), Some(22.0));
    assert_eq!(tokens(&mut queue, "nature=mechanical"), Some(22.0));
    assert_eq!(tokens(&mut queue, "group=unknown"), None);
    // Once it ends, the same period reads it in the same strata.
    queue.push(
        Some(7),
        Some(run.as_str()),
        "run_failed",
        json!({"status": "failed"}),
        claimed + 600,
    );
    assert_eq!(tokens(&mut queue, "group=treatment"), Some(22.0));
    assert_eq!(tokens(&mut queue, "effort=medium"), Some(22.0));
    assert_eq!(tokens(&mut queue, "nature=mechanical"), Some(22.0));
}

/// `--compare` keeps, on each side, why a late quality has no value and
/// how its record stands, per stratum: a window before the record reads
/// `before_record` next to its null, and the one after it reads its value
/// with how long after its end it was read and how far the record reached.
#[test]
fn a_comparison_keeps_each_sides_reason_and_record_of_the_quality_after_landing() {
    let mut queue = Queue::default();
    let mut early = Run::new(1, None, MONDAY + HOUR, 100);
    early.session = Some(SONNET);
    let mut late = Run::new(2, None, MONDAY + DAY + HOUR, 100);
    late.session = Some(SONNET);
    land(&mut queue, &early, "c1");
    let landed = land(&mut queue, &late, "c2");
    // The records begin between the two windows; run 2's commit is
    // reverted.
    let begins = MONDAY + DAY;
    let reached = landed + 600;
    main_recorded(
        &mut queue,
        json!([
            {"sha": "c2", "at": landed, "reverts": null},
            {"sha": "r", "at": landed + 300, "reverts": "c2"},
        ]),
        begins,
        reached,
    );
    queue.queue_event("ci_checked", json!({"branch": "main"}), begins);
    queue.queue_event("ci_checked", json!({"branch": "main"}), reached);
    let now = MONDAY + 2 * DAY + HOUR;
    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY,
        MONDAY + DAY,
        MONDAY + DAY,
        MONDAY + 2 * DAY
    )
    .parse()
    .unwrap();
    let result = queue.kpi(
        now,
        &KpiConfig::default(),
        &KpiQuery {
            compare: Some(spec),
            by: vec![Axis::GROUP],
            ..KpiQuery::default()
        },
    );
    let compare = result.compare.unwrap();
    for stratum in ["group=treatment", ALL] {
        let side = &compare.strata["post_landing.revert_rate"][stratum];
        assert_eq!(
            (side.before.value, side.after.value),
            (None, Some(1.0)),
            "{stratum}"
        );
        assert_eq!(side.before_missing, Some("before_record"), "{stratum}");
        assert_eq!(side.after_missing, None, "{stratum}");
        assert_eq!(side.change.reason, Some("no_value"), "{stratum}");
        let ci = &compare.strata["post_landing.ci_failure_rate"][stratum];
        assert_eq!(ci.before_missing, Some("before_record"), "{stratum}");
        assert_eq!(ci.after.value, Some(0.0), "{stratum}");
    }
    assert_eq!(
        compare.before.unavailable["post_landing.revert_rate"],
        "before_record"
    );
    assert!(
        !compare
            .after
            .unavailable
            .contains_key("post_landing.revert_rate")
    );
    let before = &compare.before.post_landing["group=treatment"].reverts;
    assert_eq!(
        (before.landed, before.before_record, before.reason),
        (1, 1, Some("before_record"))
    );
    let after = &compare.after.post_landing["group=treatment"].reverts;
    assert_eq!((after.recorded, after.reason), (1, None));
    assert_eq!(after.read_at, now * 1000);
    assert_eq!(after.after_period_ms, (now - (MONDAY + 2 * DAY)) * 1000);
    assert_eq!(after.reach_at, Some(reached * 1000));
    // The output carries both sides' records.
    let json = serde_json::to_value(&compare).unwrap();
    assert_eq!(
        json["before"]["post_landing"]["group=treatment"]["reverts"]["reason"],
        "before_record"
    );
    assert_eq!(
        json["strata"]["post_landing.revert_rate"]["group=treatment"]["before_missing"],
        "before_record"
    );
}
