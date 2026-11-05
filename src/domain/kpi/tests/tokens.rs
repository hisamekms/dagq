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

/// The provider pair's tokens and cost per run (ADR-t4005-1 decision 8):
/// the worker's turns and the review's jobs of each landed run summed
/// apart, from the Executions the rows keep (a Codex one's from its
/// `token_usage_record`s); a Claude Execution's dollars as recorded, a
/// Codex one's at the repository's Codex prices; a run without tokens is
/// out of the sum and counted. Without the prices, a side with a Codex
/// Execution has no cost and its reason. A Codex session's close, which
/// records no tokens, is not read.
#[test]
fn the_pair_groups_sum_each_runs_worker_and_review_tokens_and_cost() {
    use crate::domain::agent_eval::round::ProviderCost;
    use crate::domain::ledger::provider_use::ProviderUse;
    let mut queue = Queue::default();
    let counts = |input: i64, cached: i64, output: i64, usd: Option<f64>| {
        let mut counts = json!({"input": input, "output": output, "cache_read": cached,
                                "cache_creation": 0});
        if let Some(usd) = usd {
            counts["cost_usd"] = json!(usd);
        }
        counts
    };
    let recorded = ProviderUse {
        worker_waits: Some(Default::default()),
        review_switches: Some(Default::default()),
    };
    // A: Claude writes ($0.5), Codex reviews (2M input, 1M cached, 100k
    // output: $4 + $0.1 + $1 at the prices below).
    let control = Run::new(1, None, MONDAY + HOUR, 100);
    let landed = queue.run(&control);
    let id = run_id(&control);
    queue.push(
        Some(1),
        Some(id.as_str()),
        "turn_finished",
        json!({"provider": "claude", "tokens": counts(1000, 0, 100, Some(0.5)),
               "tokens_source": "model_usage", "tokens_by_model": []}),
        control.claimed + 50,
    );
    queue.push(
        Some(1),
        Some(id.as_str()),
        "review_job_finished",
        json!({"provider": "codex", "tokens": counts(2_000_000, 1_000_000, 100_000, None),
               "tokens_source": "token_usage_record", "tokens_by_model": []}),
        control.claimed + 150,
    );
    queue.rows.push(pair_row(
        &control,
        landed,
        ("control", "claude", "claude"),
        Some(recorded.clone()),
    ));
    // B: Codex writes (1M output: $10), Claude reviews ($0.25); its
    // session's close names no tokens and is not read.
    let treatment = Run::new(2, None, MONDAY + 2 * HOUR, 100);
    let landed = queue.run(&treatment);
    let id = run_id(&treatment);
    queue.push(
        Some(2),
        Some(id.as_str()),
        "turn_finished",
        json!({"provider": "codex", "tokens": counts(0, 0, 1_000_000, None),
               "tokens_source": "token_usage_record", "tokens_by_model": []}),
        treatment.claimed + 50,
    );
    queue.push(
        Some(2),
        Some(id.as_str()),
        "session_closed",
        json!({"kind": "worker", "reason": "transcript_not_claude", "tokens": null,
               "effort": null}),
        treatment.claimed + 60,
    );
    queue.push(
        Some(2),
        Some(id.as_str()),
        "review_job_finished",
        json!({"provider": "claude", "tokens": counts(400, 0, 40, Some(0.25)),
               "tokens_source": "model_usage", "tokens_by_model": []}),
        treatment.claimed + 150,
    );
    queue.rows.push(pair_row(
        &treatment,
        landed,
        ("treatment", "codex", "codex"),
        Some(recorded.clone()),
    ));
    // A run of A with no Execution recorded.
    let bare = Run::new(3, None, MONDAY + 3 * HOUR, 100);
    let landed = queue.run(&bare);
    queue.rows.push(pair_row(
        &bare,
        landed,
        ("control", "claude", "claude"),
        Some(recorded),
    ));
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec!["pair_group".parse().unwrap()],
        ..KpiQuery::default()
    };
    let priced = KpiConfig {
        codex_prices: Some(ProviderCost {
            input_usd_per_mtok: Some(2.0),
            cached_input_usd_per_mtok: Some(0.1),
            output_usd_per_mtok: Some(10.0),
            default_run_usd: None,
        }),
        ..KpiConfig::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &priced, &query);
    let period = &result.periods[0];
    let value = |name: &str, stratum: &str| {
        let measure = measure(period, name, stratum);
        (measure.n, measure.value)
    };
    for stratum in ["pair_group=control", "assigned:pair_group=control"] {
        assert_eq!(value(pair::WORKER_TOKENS, stratum), (1, Some(1100.0)));
        assert_eq!(value(pair::REVIEW_TOKENS, stratum), (1, Some(3_100_000.0)));
        assert_eq!(value(pair::WORKER_COST, stratum), (1, Some(0.5)));
        assert_eq!(value(pair::REVIEW_COST, stratum), (1, Some(5.1)));
    }
    assert_eq!(
        value(pair::WORKER_TOKENS, "pair_group=treatment"),
        (1, Some(1_000_000.0))
    );
    assert_eq!(
        value(pair::WORKER_COST, "pair_group=treatment"),
        (1, Some(10.0))
    );
    assert_eq!(
        value(pair::REVIEW_COST, "pair_group=treatment"),
        (1, Some(0.25))
    );
    let details = &period.window.details["provider_pair"]["pair_group=control"];
    assert_eq!(details["worker"]["with_tokens"], 1);
    assert_eq!(details["worker"]["without_tokens"], 1);
    assert!(details["review"].get("cost_reason").is_none());

    // Without the prices: Codex's side has no cost and says why; Claude's
    // keeps its own.
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let period = &result.periods[0];
    let cost = |name: &str, stratum: &str| measure(period, name, stratum).value;
    assert_eq!(cost(pair::REVIEW_COST, "pair_group=control"), None);
    assert_eq!(cost(pair::WORKER_COST, "pair_group=control"), Some(0.5));
    assert_eq!(cost(pair::WORKER_COST, "pair_group=treatment"), None);
    assert_eq!(cost(pair::REVIEW_COST, "pair_group=treatment"), Some(0.25));
    let details = &period.window.details["provider_pair"];
    assert_eq!(
        details["pair_group=control"]["review"]["cost_reason"],
        pair::NO_PRICES
    );
    assert_eq!(
        details["pair_group=treatment"]["worker"]["cost_reason"],
        pair::NO_PRICES
    );
    // The tokens stay.
    assert_eq!(
        measure(period, pair::REVIEW_TOKENS, "pair_group=control").value,
        Some(3_100_000.0)
    );
}

/// The tokens of the Executions follow the provider pair's strata as the run
/// KPIs do, so each stratum's tokens per landing are over its own runs and
/// landings: a run that ran as assigned, one whose worker moved, one that
/// cannot be judged and one still in flight, of which nothing is judged yet.
#[test]
fn the_tokens_per_landing_of_the_pair_groups_are_over_each_strata_runs() {
    let mut queue = Queue::default();
    // The records begin before the window, which they cover whole.
    queue.queue_event(
        "observe_finished",
        json!({"outcome": "succeeded", "tokens": tokens(1, 0), "tokens_source": "model_usage",
               "tokens_by_model": []}),
        MONDAY - DAY,
    );
    let recorded = crate::domain::ledger::provider_use::ProviderUse {
        worker_waits: Some(Default::default()),
        review_switches: Some(Default::default()),
    };
    let runs = [
        ("codex", Some(recorded.clone()), 100),
        ("claude", Some(recorded.clone()), 20),
        ("codex", None, 3),
        // The environment caused this one's failure.
        ("codex", Some(recorded), 5000),
    ];
    for (index, (ran_on, uses, input)) in runs.into_iter().enumerate() {
        let task = index as i64 + 1;
        let run = Run::new(task, None, MONDAY + HOUR * task, 100);
        let landed = queue.run(&run);
        let id = run_id(&run);
        queue.push(
            Some(task),
            Some(id.as_str()),
            "turn_finished",
            json!({"provider": ran_on, "tokens": tokens(input, 0),
                   "tokens_source": "token_usage_record", "tokens_by_model": []}),
            run.claimed + 50,
        );
        let mut row = pair_row(&run, landed, ("treatment", "codex", ran_on), uses);
        if task == 4 {
            row.detail[crate::domain::ledger::failure_cause::DETAIL_KEY] = json!("environment");
        }
        queue.rows.push(row);
    }
    let claimed = MONDAY + 10 * HOUR;
    let flying = format!("{:08x}-0000-4000-8000-{:012x}", 9, claimed);
    queue.changes.insert(TaskId::new(9), None);
    queue.goals.insert(TaskId::new(9), None);
    let attributes = json!({
        crate::domain::provider_trial::GROUP: "treatment",
        crate::domain::provider_trial::WORKER: "codex",
    });
    queue.push(
        Some(9),
        Some(flying.as_str()),
        "run_claimed",
        json!({ crate::domain::claim_facts::ATTRIBUTES: attributes }),
        claimed,
    );
    queue.push(
        Some(9),
        Some(flying.as_str()),
        "turn_finished",
        json!({"provider": "codex", "tokens": tokens(900, 0),
               "tokens_source": "token_usage_record", "tokens_by_model": []}),
        claimed + 50,
    );
    let query = KpiQuery {
        last: 1,
        at: Some(Cursor::Time(MONDAY * 1000)),
        by: vec!["pair_group".parse().unwrap()],
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY + HOUR, &KpiConfig::default(), &query);
    let period = &result.periods[0];
    for (stratum, total, per_landing) in [
        ("pair_group=treatment", 100.0, 100.0),
        ("switched:pair_group=treatment", 20.0, 20.0),
        ("environment:pair_group=treatment", 5000.0, 5000.0),
        ("assigned:pair_group=treatment", 6023.0, 1505.75),
    ] {
        assert_eq!(
            measure(period, "tokens", stratum).value,
            Some(total),
            "{stratum}"
        );
        assert_eq!(
            measure(period, "tokens_per_landing", stratum).value,
            Some(per_landing),
            "{stratum}"
        );
    }
}
