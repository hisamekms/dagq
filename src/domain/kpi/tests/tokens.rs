//! The KPIs of the tokens per Execution (ADR-t1486-1): per day by when
//! each Execution ended, per actor, provider and model, per landing, the
//! days before the records, and `--compare`.

use super::*;
use crate::domain::stats::executions::Coverage;

fn tokens(input: i64, output: i64) -> Value {
    json!({"input": input, "output": output, "cache_read": 0, "cache_creation": 0, "messages": 1})
}

/// A cut of the inbox's span (event 1 is not looked at), at `at` unix
/// seconds, recorded `late` seconds after.
fn cut(queue: &mut Queue, input: Option<i64>, at: i64, late: i64) {
    let tokens = input.map_or(Value::Null, |input| tokens(input, 1));
    queue.queue_event(
        "session_tokens",
        json!({"opened_event_id": 1, "kind": "inbox", "at": marks::utc_text(at * 1000),
               "final": input.is_none(), "tokens": tokens,
               "tokens_source": input.map(|_| "transcript"), "tokens_by_model": []}),
        at + late,
    );
}

/// A queue whose records begin early on Sunday, with an observation; on
/// Sunday also a run with a turn recorded before the form and one after,
/// on Monday a worker's Claude turn and a Codex review of
/// a landed `fix` run in the area `runtime`, and an inbox whose cuts run
/// across midnight into Tuesday and whose close records all its tokens.
fn queue() -> Queue {
    let mut queue = Queue::default();
    let fix = Some("fix".parse::<TaskChange>().unwrap());
    // The first record: an observation early on Sunday.
    queue.queue_event(
        "observe_finished",
        json!({"outcome": "succeeded", "tokens": tokens(1, 0), "tokens_source": "model_usage",
               "tokens_by_model": []}),
        MONDAY - DAY + HOUR,
    );
    let sunday = Run::new(1, None, MONDAY - DAY + 10 * HOUR, 600);
    queue.run(&sunday);
    queue.push(
        Some(1),
        Some(run_id(&sunday).as_str()),
        "turn_finished",
        json!({"provider": "claude", "tokens": tokens(500, 50)}),
        sunday.claimed + 300,
    );
    queue.push(
        Some(1),
        Some(run_id(&sunday).as_str()),
        "turn_finished",
        json!({"provider": "claude", "tokens": tokens(2, 0), "tokens_source": "model_usage",
               "tokens_by_model": []}),
        sunday.claimed + 400,
    );
    let monday = Run::new(2, fix, MONDAY + 10 * HOUR, 600);
    queue.run(&monday);
    let run = run_id(&monday);
    queue.push(
        Some(2),
        Some(run.as_str()),
        "turn_finished",
        json!({"provider": "claude", "tokens": tokens(100, 10), "tokens_source": "model_usage",
               "tokens_by_model": [{"model": "opus", "input": 100, "output": 10}]}),
        monday.claimed + 300,
    );
    queue.push(
        Some(2),
        Some(run.as_str()),
        "review_finished",
        json!({"verdict": "pass", "tokens": tokens(7, 3), "tokens_source": "token_usage_record",
               "model": "gpt-5", "tokens_by_model": []}),
        monday.claimed + 650,
    );
    queue.areas = Some(HashMap::from([(run, vec!["runtime".to_owned()])]));
    queue.queue_event(
        "session_opened",
        json!({"kind": "inbox"}),
        MONDAY + 20 * HOUR,
    );
    cut(&mut queue, Some(10), MONDAY + 23 * HOUR + 1800, 5);
    cut(&mut queue, Some(20), MONDAY + DAY + 2700, 5);
    queue.queue_event(
        "session_closed",
        json!({"opened_event_id": 1, "reason": "exited", "tokens": tokens(5000, 0)}),
        MONDAY + DAY + HOUR,
    );
    // The close's cut, written at the supervisor's next pass; unmeasured.
    cut(&mut queue, None, MONDAY + DAY + HOUR, 600);
    queue
}

/// Each day counts what ended on it, a cut on the day it cuts at, in
/// `all`, per actor, provider and model and the three together, and per
/// the change and area of a run's Executions; per landing over the same
/// landings. Saturday, before the records, and Sunday, where they begin,
/// have null (not 0, nor a part to judge against) and say so; the tokens
/// the inbox recorded at its close are on no day.
#[test]
fn the_tokens_are_counted_per_day_actor_provider_and_model() {
    let kpi = queue().kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 5,
            ..KpiQuery::default()
        },
    );
    let [saturday, sunday, monday, tuesday, _] = &kpi.periods[..] else {
        panic!("five days");
    };
    assert_eq!(saturday.window.unavailable["tokens"], "not_recorded");
    assert_eq!(saturday.window.details["tokens"]["coverage"], "none");
    let value = |day: &PeriodKpis, name: &str, stratum: &str| {
        let measure = measure(day, name, stratum);
        (measure.n, measure.value)
    };
    assert_eq!(value(sunday, "tokens", ALL), (2, None));
    assert_eq!(value(sunday, "tokens_per_landing", ALL), (1, None));
    assert_eq!(value(sunday, "tokens", "actor=observer"), (1, None));
    assert_eq!(sunday.window.unavailable["tokens"], "partly_recorded");
    assert_eq!(sunday.window.details["tokens"]["total"], 3);
    // Not judged against the part of Sunday.
    assert!(!monday.window.unavailable.contains_key("tokens"));

    assert_eq!(value(monday, "tokens", ALL), (3, Some(131.0)));
    assert_eq!(value(monday, "tokens_per_landing", ALL), (1, Some(131.0)));
    assert_eq!(value(monday, "tokens", "actor=worker"), (1, Some(110.0)));
    assert_eq!(value(monday, "tokens", "actor=review"), (1, None));
    assert_eq!(value(monday, "tokens", "actor=inbox"), (1, None));
    assert_eq!(value(monday, "tokens", "provider=codex"), (1, Some(10.0)));
    assert_eq!(value(monday, "tokens", "provider=claude"), (2, Some(121.0)));
    assert_eq!(value(monday, "tokens", "model=gpt-5"), (1, Some(10.0)));
    assert_eq!(
        value(
            monday,
            "tokens",
            "cross:actor=worker|model=opus|provider=claude"
        ),
        (1, Some(110.0))
    );
    assert_eq!(value(monday, "tokens", "change=fix"), (2, Some(120.0)));
    assert_eq!(
        value(monday, "tokens_per_landing", "change=fix"),
        (1, Some(120.0))
    );
    assert_eq!(value(monday, "tokens", "area=runtime"), (2, Some(120.0)));
    // The review's and the inbox's records begin on Monday: their own
    // strata have no value on the day they begin.
    let details = &monday.window.details["tokens"];
    assert_eq!(
        details["by_actor"]["review"]["by_provider"]["codex"]["total"],
        10
    );
    assert_eq!(
        details["by_actor"]["inbox"]["recorded_from"],
        marks::utc_text((MONDAY + 23 * HOUR + 1800) * 1000)
    );

    // Tuesday: the inbox's cut after midnight and its close's unmeasured
    // one, not the 5000 of its close; no landing to divide by.
    assert_eq!(value(tuesday, "tokens", ALL), (1, Some(21.0)));
    assert_eq!(measure(tuesday, "tokens", "actor=inbox").n, 1);
    assert_eq!(tuesday.window.details["tokens"]["unmeasured"], 1);
    assert_eq!(value(tuesday, "tokens_per_landing", ALL), (0, None));
    assert!(!tuesday.window.unavailable.contains_key("tokens"));
    assert_eq!(direction("tokens"), None);
    assert_eq!(direction("tokens_per_landing"), Some(Direction::Lower));
}

/// `--compare` puts the tokens of both sides side by side per stratum,
/// and says how much of each side the records cover.
#[test]
fn compare_puts_the_tokens_of_both_sides_side_by_side() {
    let kpi = queue().kpi(
        MONDAY + 3 * DAY,
        &KpiConfig::default(),
        &KpiQuery {
            last: 1,
            compare: Some(CompareSpec::At(Cursor::Time((MONDAY + DAY) * 1000))),
            window_days: 1,
            ..KpiQuery::default()
        },
    );
    let compare = kpi.compare.unwrap();
    // The worker's tokens before, and none after; the inbox's records
    // begin on the side before, which has no value to compare.
    let worker = &compare.strata["tokens"]["actor=worker"];
    assert_eq!(
        (worker.before.value, worker.before.n, worker.after.value),
        (Some(110.0), 1, None)
    );
    let inbox = &compare.strata["tokens"]["actor=inbox"];
    assert_eq!((inbox.before.value, inbox.after.value), (None, Some(21.0)));
    assert!(!inbox.change.judged);
    assert_eq!(compare.before.token_coverage, Coverage::Full);
    assert_eq!(compare.after.token_coverage, Coverage::Full);
    let json = serde_json::to_value(&compare.before).unwrap();
    assert_eq!(json["token_coverage"], "full");
}
